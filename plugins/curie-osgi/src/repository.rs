//! Publish an OSGi bundle to an OBR (OSGi Bundle Repository).
//!
//! Destinations:
//! - `file:` / bare path — write `bundles/<bsn>-<version>.jar` and `repository.xml`
//! - `http(s):` — HTTP PUT the same two files
//!
//! A `[[repositories]]` id in `[plugin.osgi] repository` is resolved from
//! the envelope context (URL + optional credentials).

use crate::config::{self, Envelope, OsgiConfig};
use crate::headers;
use anyhow::{bail, Context, Result};
use curie_plugin::PluginContext;
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(project_root: &Path, env: &Envelope) -> Result<()> {
    let ctx = env
        .context
        .as_ref()
        .context("curie-build did not provide a plugin context")?;
    let repo_spec = env.config.repository.as_deref().context(
        "[plugin.osgi] repository is required for publish \
         (file: path, http(s) URL, or a [[repositories]] id)",
    )?;

    let jar = ctx.jar.as_ref().context("plugin context has no jar path")?;
    let jar = if jar.is_absolute() {
        jar.clone()
    } else {
        project_root.join(jar)
    };
    if !jar.exists() {
        bail!("project JAR does not exist: {}", jar.display());
    }

    let resolved = resolve_repository(repo_spec, ctx, project_root)?;
    let packages = crate::bundle::scan_packages(&jar)?;
    let spec = PublishSpec {
        bundle_path: jar,
        repo_url: resolved.url,
        symbolic_name: config::symbolic_name(&env.config, ctx),
        version: config::bundle_version(&env.config, ctx),
        presentation_name: env
            .config
            .bundle_name
            .clone()
            .unwrap_or_else(|| ctx.artifact_id.clone()),
        export_packages: export_list(&env.config, &packages),
        dry_run: ctx.dry_run,
        username: resolved.username,
        password: resolved.password,
    };
    publish(&spec)
}

struct ResolvedRepo {
    url: String,
    username: Option<String>,
    password: Option<String>,
}

fn resolve_repository(
    spec: &str,
    ctx: &PluginContext,
    project_root: &Path,
) -> Result<ResolvedRepo> {
    if let Some(r) = ctx.repositories.iter().find(|r| r.id == spec) {
        return Ok(ResolvedRepo {
            url: absolutize_file_url(&r.url, project_root),
            username: r.username.clone(),
            password: r.password.clone(),
        });
    }
    Ok(ResolvedRepo {
        url: absolutize_file_url(spec, project_root),
        username: None,
        password: None,
    })
}

fn absolutize_file_url(url: &str, project_root: &Path) -> String {
    let Some(rest) = url.strip_prefix("file:") else {
        return url.to_string();
    };
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let path = Path::new(rest);
    if path.is_absolute() {
        return format!("file:{}", path.display());
    }
    format!("file:{}", project_root.join(path).display())
}

fn export_list(cfg: &OsgiConfig, packages: &std::collections::BTreeSet<String>) -> Vec<String> {
    headers::compute_exports(cfg, packages)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .collect()
}

pub struct PublishSpec {
    pub bundle_path: PathBuf,
    pub repo_url: String,
    pub symbolic_name: String,
    pub version: String,
    pub presentation_name: String,
    pub export_packages: Vec<String>,
    pub dry_run: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub fn publish(spec: &PublishSpec) -> Result<()> {
    let jar_name = format!("{}-{}.jar", spec.symbolic_name, spec.version);
    let jar_rel = format!("bundles/{jar_name}");
    let dest = Destination::parse(&spec.repo_url)?;

    let existing_xml = if spec.dry_run {
        None
    } else {
        dest.fetch_text(
            "repository.xml",
            spec.username.as_deref(),
            spec.password.as_deref(),
        )?
    };
    let xml = merge_repository_xml(existing_xml.as_deref(), spec, &jar_rel);

    if spec.dry_run {
        eprintln!("dry-run: would PUT {}/{}", spec.repo_url, jar_rel);
        eprintln!("dry-run: would PUT {}/repository.xml", spec.repo_url);
        return Ok(());
    }

    let jar_bytes = fs::read(&spec.bundle_path)
        .with_context(|| format!("failed to read {}", spec.bundle_path.display()))?;
    dest.put(
        &jar_rel,
        &jar_bytes,
        "application/java-archive",
        spec.username.as_deref(),
        spec.password.as_deref(),
    )?;
    dest.put(
        "repository.xml",
        xml.as_bytes(),
        "application/xml",
        spec.username.as_deref(),
        spec.password.as_deref(),
    )?;
    eprintln!("OSGi repository {}", spec.repo_url);
    Ok(())
}

enum Destination {
    File(PathBuf),
    Http(String),
}

impl Destination {
    fn parse(url: &str) -> Result<Self> {
        if let Some(rest) = url.strip_prefix("file:") {
            let rest = rest.strip_prefix("//").unwrap_or(rest);
            return Ok(Destination::File(PathBuf::from(rest)));
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(Destination::Http(url.trim_end_matches('/').to_string()));
        }
        Ok(Destination::File(PathBuf::from(url)))
    }

    fn fetch_text(
        &self,
        rel: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Option<String>> {
        match self {
            Destination::File(dir) => {
                let path = dir.join(rel);
                if path.exists() {
                    Ok(Some(fs::read_to_string(path)?))
                } else {
                    Ok(None)
                }
            }
            Destination::Http(base) => http_get(&format!("{base}/{rel}"), username, password),
        }
    }

    fn put(
        &self,
        rel: &str,
        body: &[u8],
        content_type: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<()> {
        match self {
            Destination::File(dir) => {
                let path = dir.join(rel);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, body)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(())
            }
            Destination::Http(base) => http_put(
                &format!("{base}/{rel}"),
                body,
                content_type,
                username,
                password,
            ),
        }
    }
}

fn http_get(url: &str, username: Option<&str>, password: Option<&str>) -> Result<Option<String>> {
    let mut req = ureq::get(url);
    req = apply_auth(req, username, password);
    match req.call() {
        Ok(resp) => Ok(Some(resp.into_string().context("failed to read GET body")?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("GET {url} failed")),
    }
}

fn http_put(
    url: &str,
    body: &[u8],
    content_type: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    let mut req = ureq::put(url).set("Content-Type", content_type);
    req = apply_auth(req, username, password);
    let resp = req
        .send_bytes(body)
        .with_context(|| format!("PUT {url} failed"))?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        bail!("HTTP {status} from PUT {url}");
    }
    Ok(())
}

fn apply_auth(req: ureq::Request, username: Option<&str>, password: Option<&str>) -> ureq::Request {
    match (username, password) {
        (Some(u), Some(p)) => req.set("Authorization", &basic_auth(u, p)),
        _ => req,
    }
}

fn basic_auth(user: &str, pass: &str) -> String {
    use std::io::Write as _;
    let mut buf = Vec::new();
    // std has no base64; encode manually (RFC 4648).
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{pass}");
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        buf.push(T[(b0 >> 2) as usize]);
        buf.push(T[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize]);
        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                buf.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize]);
                buf.push(T[(b2 & 0x3f) as usize]);
            }
            (Some(b1), None) => {
                buf.push(T[((b1 & 0x0f) << 2) as usize]);
                buf.push(b'=');
            }
            (None, _) => {
                buf.push(b'=');
                buf.push(b'=');
            }
        }
        i += 3;
    }
    let mut header = b"Basic ".to_vec();
    let _ = header.write_all(&buf);
    String::from_utf8(header).expect("base64 is ascii")
}

pub fn render_resource_xml(spec: &PublishSpec, jar_rel: &str, size: u64) -> String {
    let id = format!("{}/{}", spec.symbolic_name, spec.version);
    let mut caps = format!(
        "    <capability name=\"bundle\">\n      <p n=\"symbolicname\" v=\"{}\"/>\n      <p n=\"manifestversion\" v=\"2\"/>\n      <p n=\"version\" t=\"version\" v=\"{}\"/>\n    </capability>\n",
        xml_escape(&spec.symbolic_name),
        xml_escape(&spec.version),
    );
    for pkg in &spec.export_packages {
        caps.push_str(&format!(
            "    <capability name=\"package\">\n      <p n=\"package\" v=\"{}\"/>\n    </capability>\n",
            xml_escape(pkg),
        ));
    }
    format!(
        "  <resource id=\"{id}\" symbolicname=\"{bsn}\" presentationname=\"{name}\" uri=\"{uri}\" version=\"{ver}\">\n    <size>{size}</size>\n{caps}  </resource>",
        id = xml_escape(&id),
        bsn = xml_escape(&spec.symbolic_name),
        name = xml_escape(&spec.presentation_name),
        uri = xml_escape(jar_rel),
        ver = xml_escape(&spec.version),
        size = size,
        caps = caps,
    )
}

pub fn render_repository_xml(resources: &[String]) -> String {
    let lastmodified = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".into());
    let mut out = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<repository name='curie-osgi' lastmodified='{lastmodified}'>\n"
    );
    for r in resources {
        out.push_str(r);
        out.push('\n');
    }
    out.push_str("</repository>\n");
    out
}

fn merge_repository_xml(existing: Option<&str>, spec: &PublishSpec, jar_rel: &str) -> String {
    let size = spec.bundle_path.metadata().map(|m| m.len()).unwrap_or(0);
    let new_resource = render_resource_xml(spec, jar_rel, size);
    let new_id = format!("{}/{}", spec.symbolic_name, spec.version);
    let mut resources = match existing {
        Some(xml) => extract_resources(xml),
        None => Vec::new(),
    };
    resources.retain(|r| resource_id(r).as_deref() != Some(new_id.as_str()));
    resources.push(new_resource);
    resources.sort();
    render_repository_xml(&resources)
}

fn extract_resources(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<resource ") {
        let after = &rest[start..];
        if let Some(end_rel) = after.find("</resource>") {
            let block = after[..end_rel + "</resource>".len()].to_string();
            out.push(block);
            rest = &after[end_rel + "</resource>".len()..];
        } else {
            break;
        }
    }
    out
}

fn resource_id(block: &str) -> Option<String> {
    let after = block.split("id=\"").nth(1)?;
    Some(after.split('"').next()?.to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(dir: &Path, repo: &str) -> PublishSpec {
        let jar = dir.join("lib.jar");
        if !jar.exists() {
            fs::write(&jar, b"jar-bytes").unwrap();
        }
        PublishSpec {
            bundle_path: jar,
            repo_url: repo.to_string(),
            symbolic_name: "com.example.greeter".into(),
            version: "1.0.0".into(),
            presentation_name: "Greeter".into(),
            export_packages: vec!["com.example".into()],
            dry_run: false,
            username: None,
            password: None,
        }
    }

    #[test]
    fn publish_to_file_repo_writes_bundle_and_index() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("obr");
        let s = spec(tmp.path(), &format!("file:{}", repo.display()));
        publish(&s).unwrap();

        let jar = repo.join("bundles/com.example.greeter-1.0.0.jar");
        assert!(jar.exists(), "bundle should be copied");
        assert_eq!(fs::read(&jar).unwrap(), b"jar-bytes");
        let xml = fs::read_to_string(repo.join("repository.xml")).unwrap();
        assert!(xml.contains("com.example.greeter"));
        assert!(xml.contains("com.example"));
        assert!(xml.contains("uri=\"bundles/com.example.greeter-1.0.0.jar\""));
    }

    #[test]
    fn publish_merges_existing_resource() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("obr");
        let first = spec(tmp.path(), &format!("file:{}", repo.display()));
        publish(&first).unwrap();

        let mut second = spec(tmp.path(), &format!("file:{}", repo.display()));
        second.symbolic_name = "com.example.other".into();
        publish(&second).unwrap();

        let xml = fs::read_to_string(repo.join("repository.xml")).unwrap();
        assert!(xml.contains("com.example.greeter"));
        assert!(xml.contains("com.example.other"));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("obr");
        let mut s = spec(tmp.path(), &format!("file:{}", repo.display()));
        s.dry_run = true;
        publish(&s).unwrap();
        assert!(!repo.exists());
    }

    #[test]
    fn destination_parses_file_and_http() {
        match Destination::parse("file:/tmp/obr").unwrap() {
            Destination::File(p) => assert_eq!(p, PathBuf::from("/tmp/obr")),
            _ => panic!("expected file"),
        }
        match Destination::parse("http://localhost:9/obr/").unwrap() {
            Destination::Http(u) => assert_eq!(u, "http://localhost:9/obr"),
            _ => panic!("expected http"),
        }
    }

    #[test]
    fn basic_auth_header_is_rfc4648() {
        assert_eq!(
            basic_auth("aladdin", "opensesame"),
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l"
        );
    }

    #[test]
    fn publish_to_dummy_http_osgi_repository() {
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("obr-store");
        let listener = crate::dummy_repo::bind(0).unwrap();
        let addr = listener.local_addr().unwrap();
        let store_for_server = store.clone();
        thread::spawn(move || {
            let _ = crate::dummy_repo::serve_forever(listener, store_for_server);
        });

        let s = spec(tmp.path(), &format!("http://{addr}"));
        publish(&s).unwrap();

        let xml = fs::read_to_string(store.join("repository.xml")).unwrap();
        assert!(
            xml.contains("symbolicname=\"com.example.greeter\""),
            "got: {xml}"
        );
        assert_eq!(
            fs::read(store.join("bundles/com.example.greeter-1.0.0.jar")).unwrap(),
            b"jar-bytes"
        );

        let fetched = ureq::get(&format!("http://{addr}/repository.xml"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(fetched.contains("com.example.greeter"));
    }
}
