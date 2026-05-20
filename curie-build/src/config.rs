//! User-level Curie configuration loaded from `~/.curie/config.toml`.
//!
//! Unlike `Curie.toml` (project config checked into source control), this
//! file holds environment-specific settings such as repository mirrors that
//! redirect artifact downloads through a corporate proxy.

use anyhow::{Context, Result};
use curie_deps::repo::Repository;
use serde::Deserialize;
use std::path::PathBuf;

/// Top-level structure of `~/.curie/config.toml`.
#[derive(Debug, Deserialize, Default)]
pub struct CurieConfig {
    #[serde(default)]
    pub mirrors: Vec<MirrorEntry>,
    #[serde(default)]
    pub credentials: Vec<CredentialEntry>,
}

/// One entry in the `[[credentials]]` array.  Values support `${VAR}`
/// env-var substitution applied lazily at lookup time.
#[derive(Debug, Deserialize, Clone)]
pub struct CredentialEntry {
    /// Matches a `[[repositories]]` id, e.g. `"internal-nexus"`.
    pub repo_id: String,
    /// Username (literal or `${ENV_VAR}`).
    pub username: String,
    /// Password (literal or `${ENV_VAR}`).
    pub password: String,
}

impl CredentialEntry {
    /// Resolve `${ENV_VAR}` indirections at lookup time, returning
    /// `(username, password)` as concrete strings.
    pub fn resolve(&self) -> Result<(String, String)> {
        let u = resolve_env_indirection(&self.username)
            .with_context(|| format!("invalid username for repo '{}'", self.repo_id))?;
        let p = resolve_env_indirection(&self.password)
            .with_context(|| format!("invalid password for repo '{}'", self.repo_id))?;
        Ok((u, p))
    }
}

/// Look up the credentials for a given repo id, if any.
pub fn credentials_for<'a>(cfg: &'a CurieConfig, repo_id: &str) -> Option<&'a CredentialEntry> {
    cfg.credentials.iter().find(|c| c.repo_id == repo_id)
}

/// If `s` is exactly `${VAR}`, read `VAR` from the environment.  Otherwise
/// return `s` unchanged.  An unset `${VAR}` is a hard error so callers don't
/// silently use empty passwords.
fn resolve_env_indirection(s: &str) -> Result<String> {
    if let Some(var) = s.strip_prefix("${").and_then(|rest| rest.strip_suffix('}')) {
        std::env::var(var)
            .with_context(|| format!("environment variable '{}' is not set", var))
    } else {
        Ok(s.to_string())
    }
}

/// One entry in the `[[mirrors]]` array.
#[derive(Debug, Deserialize, Clone)]
pub struct MirrorEntry {
    /// Unique identifier for this mirror entry (required by the config
    /// format for clarity, not used by the resolver directly).
    #[allow(dead_code)]
    pub id: String,
    /// Which repository this mirror replaces.  Use `"central"` for Maven
    /// Central, a named-repo id (e.g. `"shibboleth"`) for a project-declared
    /// repo, or `"*"` to mirror every repository.
    pub mirror_of: String,
    /// Base URL of the mirror without trailing slash.
    pub url: String,
}

/// Load `~/.curie/config.toml`.  Returns an empty config when the file does
/// not exist — absent config is not an error.
pub fn load_config() -> Result<CurieConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(CurieConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {}", path.display(), e))
}

fn config_path() -> Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))
        .map(|h| h.join(".curie").join("config.toml"))
}

/// Replace each repo's URL with its mirror's URL when a matching mirror entry
/// exists.
///
/// Each repo is tested independently: `"central"` matches a mirror whose
/// `mirror_of` is `"central"` or `"*"`, while `"shibboleth"` matches a
/// mirror with `mirror_of = "shibboleth"` or `"*"`.  Multiple repos therefore
/// each get their own independent substitution.  When two mirrors both match
/// the same repo, the first declared one wins.
pub fn apply_mirrors(repos: Vec<Repository>, mirrors: &[MirrorEntry]) -> Vec<Repository> {
    if mirrors.is_empty() {
        return repos;
    }
    repos
        .into_iter()
        .map(|repo| match find_mirror(&repo.id, mirrors) {
            Some(m) => Repository {
                id: repo.id,
                name: repo.name,
                url: m.url.trim_end_matches('/').to_string(),
            },
            None => repo,
        })
        .collect()
}

fn find_mirror<'a>(repo_id: &str, mirrors: &'a [MirrorEntry]) -> Option<&'a MirrorEntry> {
    mirrors
        .iter()
        .find(|m| m.mirror_of == "*" || m.mirror_of == repo_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn make_repo(id: &str, url: &str) -> Repository {
        Repository {
            id: id.to_string(),
            name: id.to_string(),
            url: url.to_string(),
        }
    }

    fn mirror(id: &str, mirror_of: &str, url: &str) -> MirrorEntry {
        MirrorEntry {
            id: id.to_string(),
            mirror_of: mirror_of.to_string(),
            url: url.to_string(),
        }
    }

    #[test]
    fn load_config_returns_default_when_file_absent() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        let cfg = load_config().unwrap();
        assert!(cfg.mirrors.is_empty());

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn load_config_parses_mirror_entries() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        let curie_dir = dir.path().join(".curie");
        std::fs::create_dir_all(&curie_dir).unwrap();
        std::fs::write(
            curie_dir.join("config.toml"),
            r#"
[[mirrors]]
id = "nexus"
mirror_of = "central"
url = "https://nexus.internal/maven2"

[[mirrors]]
id = "nexus-shibboleth"
mirror_of = "shibboleth"
url = "https://nexus.internal/shibboleth"
"#,
        )
        .unwrap();

        let cfg = load_config().unwrap();
        assert_eq!(cfg.mirrors.len(), 2);
        assert_eq!(cfg.mirrors[0].id, "nexus");
        assert_eq!(cfg.mirrors[0].mirror_of, "central");
        assert_eq!(cfg.mirrors[1].mirror_of, "shibboleth");

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn apply_mirrors_empty_mirrors_is_noop() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let result = apply_mirrors(repos.clone(), &[]);
        assert_eq!(result[0].url, repos[0].url);
    }

    #[test]
    fn apply_mirrors_no_match_leaves_repo_unchanged() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let mirrors = vec![mirror("m", "shibboleth", "https://nexus.internal/s")];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://repo1.maven.org/maven2");
    }

    #[test]
    fn apply_mirrors_replaces_central_url() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let mirrors = vec![mirror("m", "central", "https://nexus.internal/maven2")];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://nexus.internal/maven2");
        assert_eq!(result[0].id, "central", "id must be preserved");
    }

    #[test]
    fn apply_mirrors_each_repo_gets_its_own_mirror() {
        let repos = vec![
            make_repo("central", "https://repo1.maven.org/maven2"),
            make_repo("shibboleth", "https://build.shibboleth.net/nexus/releases"),
        ];
        let mirrors = vec![
            mirror("m-central", "central", "https://nexus.internal/central"),
            mirror("m-shib", "shibboleth", "https://nexus.internal/shibboleth"),
        ];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://nexus.internal/central");
        assert_eq!(result[1].url, "https://nexus.internal/shibboleth");
    }

    #[test]
    fn apply_mirrors_star_matches_any_repo() {
        let repos = vec![
            make_repo("central", "https://repo1.maven.org/maven2"),
            make_repo("shibboleth", "https://build.shibboleth.net/nexus/releases"),
        ];
        let mirrors = vec![mirror("all", "*", "https://nexus.internal/all")];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://nexus.internal/all");
        assert_eq!(result[1].url, "https://nexus.internal/all");
    }

    #[test]
    fn apply_mirrors_first_match_wins() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let mirrors = vec![
            mirror("first", "central", "https://first.mirror/maven2"),
            mirror("second", "central", "https://second.mirror/maven2"),
        ];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://first.mirror/maven2");
    }

    // -- credentials --------------------------------------------------------

    #[test]
    fn credentials_load_with_literal_values() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        std::fs::create_dir_all(dir.path().join(".curie")).unwrap();
        std::fs::write(
            dir.path().join(".curie").join("config.toml"),
            r#"
[[credentials]]
repo_id = "nexus"
username = "alice"
password = "literal-secret"
"#,
        )
        .unwrap();

        let cfg = load_config().unwrap();
        let cred = credentials_for(&cfg, "nexus").expect("found");
        let (u, p) = cred.resolve().unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "literal-secret");

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn credentials_load_with_env_var_substitution() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("TEST_NEXUS_USER", "bob");
        std::env::set_var("TEST_NEXUS_TOKEN", "tok-xyz");

        std::fs::create_dir_all(dir.path().join(".curie")).unwrap();
        std::fs::write(
            dir.path().join(".curie").join("config.toml"),
            r#"
[[credentials]]
repo_id = "nx"
username = "${TEST_NEXUS_USER}"
password = "${TEST_NEXUS_TOKEN}"
"#,
        )
        .unwrap();

        let cfg = load_config().unwrap();
        let (u, p) = credentials_for(&cfg, "nx").unwrap().resolve().unwrap();
        assert_eq!(u, "bob");
        assert_eq!(p, "tok-xyz");

        std::env::remove_var("TEST_NEXUS_USER");
        std::env::remove_var("TEST_NEXUS_TOKEN");
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn credentials_missing_env_var_errors() {
        let entry = CredentialEntry {
            repo_id: "x".into(),
            username: "${DEFINITELY_NOT_SET_42}".into(),
            password: "p".into(),
        };
        let err = entry.resolve().unwrap_err().to_string();
        // anyhow's chain shows the leaf cause — both Display and Debug forms work.
        let chain = format!("{:#}", entry.resolve().unwrap_err());
        assert!(
            chain.contains("DEFINITELY_NOT_SET_42") || err.contains("DEFINITELY_NOT_SET_42"),
            "expected env-var name in error, got: {err}, chain: {chain}",
        );
    }

    #[test]
    fn credentials_lookup_by_repo_id_picks_correct_entry() {
        let cfg = CurieConfig {
            mirrors: vec![],
            credentials: vec![
                CredentialEntry { repo_id: "a".into(), username: "ua".into(), password: "pa".into() },
                CredentialEntry { repo_id: "b".into(), username: "ub".into(), password: "pb".into() },
            ],
        };
        let b = credentials_for(&cfg, "b").unwrap();
        assert_eq!(b.username, "ub");
        assert!(credentials_for(&cfg, "missing").is_none());
    }

    #[test]
    fn apply_mirrors_trailing_slash_stripped() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let mirrors = vec![mirror("m", "central", "https://nexus.internal/maven2/")];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://nexus.internal/maven2");
    }
}
