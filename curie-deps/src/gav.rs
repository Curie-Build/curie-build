//! Group-Artifact-Version coordinate parsing and path/URL derivation.

use anyhow::{bail, Result};
use std::fmt;
use std::path::PathBuf;

/// A fully-specified Maven coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Gav {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Gav {
    /// Parse `"group:artifact"` key + `"version"` value (Curie TOML format).
    ///
    /// ```
    /// # use curie_deps::Gav;
    /// let g = Gav::from_key_version("com.google.guava:guava", "33.2.0-jre").unwrap();
    /// assert_eq!(g.group, "com.google.guava");
    /// assert_eq!(g.artifact, "guava");
    /// assert_eq!(g.version, "33.2.0-jre");
    /// ```
    pub fn from_key_version(key: &str, version: &str) -> Result<Self> {
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        if parts.len() != 2 {
            bail!(
                "invalid dependency key {:?}: expected \"group:artifact\"",
                key
            );
        }
        let group = parts[0].trim().to_string();
        let artifact = parts[1].trim().to_string();
        let version = version.trim().to_string();

        if group.is_empty() || artifact.is_empty() || version.is_empty() {
            bail!("dependency key {:?} has empty group, artifact, or version", key);
        }

        Ok(Gav { group, artifact, version })
    }

    /// The group path segment used in Maven repository layout:
    /// `com.example` → `com/example`.
    pub fn group_path(&self) -> String {
        self.group.replace('.', "/")
    }

    /// Relative path within a Maven repository layout:
    /// `com/example/foo/1.0/foo-1.0.jar`
    pub fn relative_path(&self) -> String {
        format!(
            "{}/{}/{}/{}-{}.jar",
            self.group_path(),
            self.artifact,
            self.version,
            self.artifact,
            self.version,
        )
    }

    /// Relative POM path within a Maven repository layout.
    pub fn relative_pom_path(&self) -> String {
        format!(
            "{}/{}/{}/{}-{}.pom",
            self.group_path(),
            self.artifact,
            self.version,
            self.artifact,
            self.version,
        )
    }

    /// Absolute path in the local `~/.m2/repository` cache.
    pub fn local_cache_path(&self) -> Result<PathBuf> {
        let home = home_dir()?;
        Ok(home.join(".m2").join("repository").join(self.relative_path()))
    }

    /// Absolute POM path in the local `~/.m2/repository` cache.
    pub fn local_pom_cache_path(&self) -> Result<PathBuf> {
        let home = home_dir()?;
        Ok(home.join(".m2").join("repository").join(self.relative_pom_path()))
    }

    /// Canonical `group:artifact:version` notation.
    pub fn notation(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }
}

impl fmt::Display for Gav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.notation())
    }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid() {
        let g = Gav::from_key_version("com.google.guava:guava", "33.2.0-jre").unwrap();
        assert_eq!(g.group, "com.google.guava");
        assert_eq!(g.artifact, "guava");
        assert_eq!(g.version, "33.2.0-jre");
    }

    #[test]
    fn parse_trims_whitespace() {
        let g = Gav::from_key_version("  com.example  :  foo  ", "  1.0  ").unwrap();
        assert_eq!(g.group, "com.example");
        assert_eq!(g.artifact, "foo");
        assert_eq!(g.version, "1.0");
    }

    #[test]
    fn parse_missing_colon() {
        assert!(Gav::from_key_version("nocohereseparator", "1.0").is_err());
    }

    #[test]
    fn parse_empty_group() {
        assert!(Gav::from_key_version(":artifact", "1.0").is_err());
    }

    #[test]
    fn parse_empty_artifact() {
        assert!(Gav::from_key_version("com.example:", "1.0").is_err());
    }

    #[test]
    fn parse_empty_version() {
        assert!(Gav::from_key_version("com.example:foo", "").is_err());
    }

    #[test]
    fn group_path_dots_to_slashes() {
        let g = Gav::from_key_version("com.example.foo:bar", "1.0").unwrap();
        assert_eq!(g.group_path(), "com/example/foo");
    }

    #[test]
    fn relative_path() {
        let g = Gav::from_key_version("com.google.guava:guava", "33.2.0-jre").unwrap();
        assert_eq!(
            g.relative_path(),
            "com/google/guava/guava/33.2.0-jre/guava-33.2.0-jre.jar"
        );
    }

    #[test]
    fn relative_pom_path() {
        let g = Gav::from_key_version("com.google.guava:guava", "33.2.0-jre").unwrap();
        assert_eq!(
            g.relative_pom_path(),
            "com/google/guava/guava/33.2.0-jre/guava-33.2.0-jre.pom"
        );
    }

    #[test]
    fn notation() {
        let g = Gav::from_key_version("com.example:foo", "2.0").unwrap();
        assert_eq!(g.notation(), "com.example:foo:2.0");
    }

    #[test]
    fn display_equals_notation() {
        let g = Gav::from_key_version("com.example:foo", "2.0").unwrap();
        assert_eq!(format!("{}", g), g.notation());
    }
}
