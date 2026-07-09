//! Registry authentication: Bearer challenge + Basic, with credential sources.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Process-lifetime token cache keyed by (registry, scope).
static TOKEN_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn token_cache() -> std::sync::MutexGuard<'static, Option<HashMap<String, String>>> {
    TOKEN_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Username/password pair for registry auth.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Parse a `WWW-Authenticate: Bearer …` challenge header into key/value params.
pub fn parse_bearer_challenge(header: &str) -> Result<HashMap<String, String>> {
    let header = header.trim();
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .with_context(|| format!("not a Bearer challenge: {header}"))?;

    let mut params = HashMap::new();
    for part in split_auth_params(rest) {
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            params.insert(k.trim().to_string(), v);
        }
    }
    if !params.contains_key("realm") {
        bail!("Bearer challenge missing realm: {header}");
    }
    Ok(params)
}

/// Split comma-separated auth params, respecting double quotes.
fn split_auth_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

/// Resolve credentials for a registry host, in precedence order:
/// 1. `CURIE_REGISTRY_USERNAME` / `CURIE_REGISTRY_PASSWORD`
/// 2. `[[credentials]]` matched by `registry_id` (when provided)
/// 3. `~/.docker/config.json` auths / credHelpers
pub fn resolve_credentials(registry_host: &str, registry_id: Option<&str>) -> Result<Option<Credentials>> {
    if let (Ok(u), Ok(p)) = (
        std::env::var("CURIE_REGISTRY_USERNAME"),
        std::env::var("CURIE_REGISTRY_PASSWORD"),
    ) {
        if !u.is_empty() {
            return Ok(Some(Credentials {
                username: u,
                password: p,
            }));
        }
    }

    if let Some(id) = registry_id {
        let cfg = crate::config::load_config().unwrap_or_default();
        if let Some(entry) = crate::config::credentials_for(&cfg, id) {
            let (u, p) = entry.resolve()?;
            return Ok(Some(Credentials {
                username: u,
                password: p,
            }));
        }
    }

    if let Some(creds) = docker_config_credentials(registry_host)? {
        return Ok(Some(creds));
    }

    Ok(None)
}

/// Fetch a Bearer token for the given challenge params, using optional Basic creds.
pub fn fetch_bearer_token(
    client: &reqwest::blocking::Client,
    params: &HashMap<String, String>,
    creds: Option<&Credentials>,
) -> Result<String> {
    let realm = params.get("realm").context("missing realm")?;
    let mut url = reqwest::Url::parse(realm).with_context(|| format!("invalid realm URL: {realm}"))?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(service) = params.get("service") {
            q.append_pair("service", service);
        }
        if let Some(scope) = params.get("scope") {
            // Scope may contain multiple space-separated values; pass as-is.
            q.append_pair("scope", scope);
        }
    }

    let cache_key = format!("{url}");
    {
        let guard = token_cache();
        if let Some(map) = guard.as_ref() {
            if let Some(t) = map.get(&cache_key) {
                return Ok(t.clone());
            }
        }
    }

    let mut req = client.get(url);
    if let Some(c) = creds {
        req = req.basic_auth(&c.username, Some(&c.password));
    }
    let resp = req.send().context("failed to request registry token")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("registry token request failed ({status}): {body}");
    }
    let body: serde_json::Value = resp.json().context("invalid token response JSON")?;
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .context("token response missing token/access_token")?
        .to_string();

    {
        let mut guard = token_cache();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(cache_key, token.clone());
    }
    Ok(token)
}

fn docker_config_credentials(registry_host: &str) -> Result<Option<Credentials>> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(None),
    };
    let path = home.join(".docker").join("config.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let cfg: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    // Try exact host and common aliases.
    let candidates = [
        registry_host.to_string(),
        format!("https://{registry_host}"),
        format!("http://{registry_host}"),
        format!("https://{registry_host}/v1/"),
    ];

    if let Some(auths) = cfg.get("auths").and_then(|v| v.as_object()) {
        for key in &candidates {
            if let Some(entry) = auths.get(key) {
                if let Some(auth) = entry.get("auth").and_then(|v| v.as_str()) {
                    if let Some(creds) = decode_docker_auth(auth)? {
                        return Ok(Some(creds));
                    }
                }
            }
        }
        // Also try bare keys that end with the host.
        for (key, entry) in auths {
            if key.contains(registry_host) {
                if let Some(auth) = entry.get("auth").and_then(|v| v.as_str()) {
                    if let Some(creds) = decode_docker_auth(auth)? {
                        return Ok(Some(creds));
                    }
                }
            }
        }
    }

    // credHelpers / credsStore — exec docker-credential-*
    if let Some(helpers) = cfg.get("credHelpers").and_then(|v| v.as_object()) {
        for key in &candidates {
            if let Some(helper) = helpers.get(key).and_then(|v| v.as_str()) {
                if let Some(creds) = run_credential_helper(helper, registry_host)? {
                    return Ok(Some(creds));
                }
            }
        }
    }
    if let Some(store) = cfg.get("credsStore").and_then(|v| v.as_str()) {
        if let Some(creds) = run_credential_helper(store, registry_host)? {
            return Ok(Some(creds));
        }
    }

    Ok(None)
}

fn decode_docker_auth(auth: &str) -> Result<Option<Credentials>> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(auth.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(auth.trim()))
        .context("invalid base64 in docker config auth")?;
    let s = String::from_utf8(raw).context("docker config auth is not UTF-8")?;
    let Some((user, pass)) = s.split_once(':') else {
        return Ok(None);
    };
    if user.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credentials {
        username: user.to_string(),
        password: pass.to_string(),
    }))
}

fn run_credential_helper(helper: &str, registry_host: &str) -> Result<Option<Credentials>> {
    let bin = format!("docker-credential-{helper}");
    let mut child = match std::process::Command::new(&bin)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Ok(None), // helper not installed
    };
    use std::io::Write;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = writeln!(stdin, "{registry_host}");
    }
    let output = child.wait_with_output().context("credential helper failed")?;
    if !output.status.success() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let user = v.get("Username").and_then(|u| u.as_str()).unwrap_or("");
    let pass = v.get("Secret").and_then(|u| u.as_str()).unwrap_or("");
    if user.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credentials {
        username: user.to_string(),
        password: pass.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bearer_challenge_quoted_params() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/ubuntu:pull""#;
        let p = parse_bearer_challenge(h).unwrap();
        assert_eq!(p.get("realm").unwrap(), "https://auth.docker.io/token");
        assert_eq!(p.get("service").unwrap(), "registry.docker.io");
        assert_eq!(
            p.get("scope").unwrap(),
            "repository:library/ubuntu:pull"
        );
    }

    #[test]
    fn parse_bearer_rejects_basic() {
        assert!(parse_bearer_challenge(r#"Basic realm="x""#).is_err());
    }

    #[test]
    fn decode_docker_auth_roundtrip() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
        let c = decode_docker_auth(&encoded).unwrap().unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.password, "s3cret");
    }
}
