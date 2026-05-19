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

    #[test]
    fn apply_mirrors_trailing_slash_stripped() {
        let repos = vec![make_repo("central", "https://repo1.maven.org/maven2")];
        let mirrors = vec![mirror("m", "central", "https://nexus.internal/maven2/")];
        let result = apply_mirrors(repos, &mirrors);
        assert_eq!(result[0].url, "https://nexus.internal/maven2");
    }
}
