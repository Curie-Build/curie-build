//! Maven repository configuration.

/// A Maven-compatible artifact repository.
#[derive(Debug, Clone)]
pub struct Repository {
    /// Unique identifier used when deps select this repo, e.g. `"shibboleth"`.
    pub id: String,
    /// Human-readable display label, e.g. "Maven Central".
    pub name: String,
    /// Base URL without trailing slash, e.g. `https://repo1.maven.org/maven2`.
    pub url: String,
}

impl Repository {
    /// Build the full URL for an artifact's relative path.
    pub fn artifact_url(&self, relative_path: &str) -> String {
        format!("{}/{}", self.url.trim_end_matches('/'), relative_path)
    }
}

/// The default set of repositories: Maven Central only.
pub fn default_repositories() -> Vec<Repository> {
    vec![Repository {
        id: "central".to_string(),
        name: "Maven Central".to_string(),
        url: "https://repo1.maven.org/maven2".to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_url_basic() {
        let repo = Repository {
            id: "central".to_string(),
            name: "Central".to_string(),
            url: "https://repo1.maven.org/maven2".to_string(),
        };
        assert_eq!(
            repo.artifact_url("com/example/foo/1.0/foo-1.0.jar"),
            "https://repo1.maven.org/maven2/com/example/foo/1.0/foo-1.0.jar"
        );
    }

    #[test]
    fn artifact_url_trailing_slash_normalised() {
        let repo = Repository {
            id: "central".to_string(),
            name: "Central".to_string(),
            url: "https://repo1.maven.org/maven2/".to_string(),
        };
        let url = repo.artifact_url("com/example/foo/1.0/foo-1.0.jar");
        assert!(
            !url.contains("//com"),
            "double slash in URL: {url}"
        );
        assert_eq!(
            url,
            "https://repo1.maven.org/maven2/com/example/foo/1.0/foo-1.0.jar"
        );
    }

    #[test]
    fn default_repositories_is_maven_central() {
        let repos = default_repositories();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "Maven Central");
        assert!(
            repos[0].url.contains("repo1.maven.org/maven2"),
            "unexpected URL: {}",
            repos[0].url
        );
    }
}
