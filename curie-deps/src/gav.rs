//! Group-Artifact-Version coordinate parsing and path/URL derivation.

use anyhow::{bail, Result};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// A fully-specified Maven coordinate.
///
/// Classifiers are supported for special artifacts (e.g. JaCoCo agent
/// `runtime` classifier, or `sources` / `javadoc`).
///
/// Extensions are supported for plugin artifacts and other non-JAR files
/// (e.g. `protoc` with extension "exe", custom generators). When absent,
/// "jar" is assumed for the primary artifact path.
///
/// For Maven unique snapshots (`1.0-SNAPSHOT` published as
/// `1.0-20260610.123456-3`), [`Self::version`] is always the base version
/// (directory segment / identity) while [`Self::snapshot_version`] holds the
/// timestamped filename version. Identity (`Eq`/`Hash`) ignores
/// `snapshot_version` so two references to the same base snapshot collapse
/// correctly under nearest-wins.
#[derive(Debug, Clone, Default)]
pub struct Gav {
    pub group: String,
    pub artifact: String,
    pub version: String,
    /// Classifier, if any (e.g. Some("runtime"), Some("sources")).
    /// Empty/None means the main artifact (no classifier in filename).
    pub classifier: Option<String>,
    /// File extension without the leading dot (e.g. Some("jar"), Some("exe"),
    /// Some("zip")). If None or empty, "jar" is used for `relative_path`.
    /// This enables downloading non-JAR plugin artifacts via the hardened
    /// resolver path.
    pub extension: Option<String>,
    /// Resolved unique snapshot version used only in the *filename*, e.g.
    /// `"1.0-20260610.123456-3"`. `None` means the filename uses `version`
    /// verbatim (releases, and non-unique / locally-installed snapshots).
    pub snapshot_version: Option<String>,
}

impl PartialEq for Gav {
    fn eq(&self, other: &Self) -> bool {
        self.group == other.group
            && self.artifact == other.artifact
            && self.version == other.version
            && self.classifier == other.classifier
            && self.extension == other.extension
    }
}

impl Eq for Gav {}

impl Hash for Gav {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.group.hash(state);
        self.artifact.hash(state);
        self.version.hash(state);
        self.classifier.hash(state);
        self.extension.hash(state);
    }
}

/// Returns true if `s` contains only characters allowed in Maven coordinates
/// per the fix for bug #21: [A-Za-z0-9._-]+
fn is_valid_coord_part(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Validates a coordinate part. Errors with a descriptive message if it
/// contains invalid characters, is empty, or is "." / ".." (to prevent
/// path traversal in local repository layout).
fn validate_coord(name: &str, s: &str) -> Result<()> {
    if !is_valid_coord_part(s) {
        bail!("invalid {} {:?}: must match [A-Za-z0-9._-]+", name, s);
    }
    if s == "." || s == ".." {
        bail!("invalid {} {:?}: must not be . or ..", name, s);
    }
    if name == "group" {
        for seg in s.split('.') {
            if seg.is_empty() || seg == "." || seg == ".." {
                bail!("invalid group {:?}: contains invalid segment", s);
            }
        }
    }
    Ok(())
}

impl Gav {
    /// Validates that all coordinate parts are safe for use in local
    /// `~/.m2/repository` paths (rejects path traversal and invalid chars).
    pub fn validate_for_path(&self) -> Result<()> {
        validate_coord("group", &self.group)?;
        validate_coord("artifact", &self.artifact)?;
        validate_coord("version", &self.version)?;
        if let Some(c) = &self.classifier {
            if !c.is_empty() {
                validate_coord("classifier", c)?;
            }
        }
        if let Some(e) = &self.extension {
            if !e.is_empty() {
                validate_coord("extension", e)?;
            }
        }
        if let Some(sv) = &self.snapshot_version {
            if !sv.is_empty() {
                validate_coord("snapshot_version", sv)?;
            }
        }
        Ok(())
    }

    /// Maven snapshot test: case-sensitive `-SNAPSHOT` suffix on the base
    /// version (`1.0-SNAPSHOT`). Unique timestamps (`1.0-2026…`) are not
    /// snapshots in this sense — they are the resolved form of one.
    pub fn is_snapshot(&self) -> bool {
        self.version.ends_with("-SNAPSHOT")
    }

    /// Version string used in the artifact *filename* (unique snapshot when
    /// resolved, otherwise the base `version`).
    pub fn filename_version(&self) -> &str {
        self.snapshot_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.version)
    }

    /// Lockfile / pin key: `group:artifact:baseVersion` (classifier omitted —
    /// pins apply to the GA+base version; classifier only affects the path).
    pub fn snapshot_pin_key(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }
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
            bail!(
                "dependency key {:?} has empty group, artifact, or version",
                key
            );
        }

        let g = Gav {
            group,
            artifact,
            version,
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        g.validate_for_path()?;
        Ok(g)
    }

    /// Parse `"group:artifact"` key + `"version"` value and an optional
    /// classifier.  Intended for internal tool resolutions that need
    /// classified artifacts (e.g. `org.jacoco:org.jacoco.agent:runtime`).
    pub fn from_key_version_classifier(
        key: &str,
        version: &str,
        classifier: Option<&str>,
    ) -> Result<Self> {
        let mut g = Self::from_key_version(key, version)?;
        g.classifier = classifier.map(|s| s.to_string());
        g.validate_for_path()?;
        Ok(g)
    }

    /// Parse `"group:artifact"` key + `"version"` value plus optional
    /// classifier and extension.  Used for plugin artifacts and other
    /// non-standard published files (e.g. `protoc` executables).
    pub fn from_key_version_classifier_extension(
        key: &str,
        version: &str,
        classifier: Option<&str>,
        extension: &str,
    ) -> Result<Self> {
        let mut g = Self::from_key_version_classifier(key, version, classifier)?;
        g.extension = if extension.is_empty() {
            None
        } else {
            Some(extension.to_string())
        };
        g.validate_for_path()?;
        Ok(g)
    }

    /// The group path segment used in Maven repository layout:
    /// `com.example` → `com/example`.
    pub fn group_path(&self) -> String {
        self.group.replace('.', "/")
    }

    /// Relative path within a Maven repository layout.
    ///
    /// The **directory** always uses the base `version` (`1.0-SNAPSHOT`);
    /// the **filename** uses [`Self::filename_version`] so unique snapshots
    /// land at `…/1.0-SNAPSHOT/foo-1.0-20260610.123456-3.jar`.
    ///
    /// Respects classifier and extension (defaults to ".jar" when extension
    /// is absent). Examples:
    ///   foo-1.0.jar
    ///   foo-1.0-runtime.jar
    ///   foo-1.0-20260610.123456-3.jar   (unique snapshot)
    ///   protoc-3.25.0-linux-x86_64.exe   (plugin artifact)
    pub fn relative_path(&self) -> String {
        self.validate_for_path()
            .expect("GAV must be valid for path construction (see bug #21)");
        let file_ver = self.filename_version();
        let base = format!(
            "{}/{}/{}/{}-{}",
            self.group_path(),
            self.artifact,
            self.version,
            self.artifact,
            file_ver,
        );
        let ext = self
            .extension
            .as_deref()
            .filter(|e| !e.is_empty())
            .unwrap_or("jar");
        if let Some(c) = &self.classifier {
            if !c.is_empty() {
                return format!("{}-{}.{}", base, c, ext);
            }
        }
        format!("{}.{}", base, ext)
    }

    /// Relative POM path within a Maven repository layout.
    ///
    /// Directory uses base `version`; filename uses [`Self::filename_version`].
    pub fn relative_pom_path(&self) -> String {
        self.validate_for_path()
            .expect("GAV must be valid for path construction (see bug #21)");
        format!(
            "{}/{}/{}/{}-{}.pom",
            self.group_path(),
            self.artifact,
            self.version,
            self.artifact,
            self.filename_version(),
        )
    }

    /// Relative path of the version-level `maven-metadata.xml` used to resolve
    /// unique snapshots (`…/artifact/1.0-SNAPSHOT/maven-metadata.xml`).
    pub fn relative_snapshot_metadata_path(&self) -> String {
        self.validate_for_path()
            .expect("GAV must be valid for path construction (see bug #21)");
        format!(
            "{}/{}/{}/maven-metadata.xml",
            self.group_path(),
            self.artifact,
            self.version,
        )
    }

    /// Absolute path in the local `~/.m2/repository` cache.
    pub fn local_repository_path(&self) -> Result<PathBuf> {
        self.validate_for_path()?;
        let home = home_dir()?;
        Ok(home
            .join(".m2")
            .join("repository")
            .join(self.relative_path()))
    }

    /// Absolute POM path in the local `~/.m2/repository` cache.
    pub fn pom_local_repository_path(&self) -> Result<PathBuf> {
        self.validate_for_path()?;
        let home = home_dir()?;
        Ok(home
            .join(".m2")
            .join("repository")
            .join(self.relative_pom_path()))
    }

    /// Canonical `group:artifact:version` (or with `:classifier` when present) notation.
    pub fn notation(&self) -> String {
        if let Some(c) = &self.classifier {
            if !c.is_empty() {
                return format!("{}:{}:{}:{}", self.group, self.artifact, self.version, c);
            }
        }
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
    fn relative_path_with_classifier() {
        let g = Gav::from_key_version_classifier(
            "org.jacoco:org.jacoco.agent",
            "0.8.13",
            Some("runtime"),
        )
        .unwrap();
        assert_eq!(
            g.relative_path(),
            "org/jacoco/org.jacoco.agent/0.8.13/org.jacoco.agent-0.8.13-runtime.jar"
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
    fn notation_with_classifier() {
        let mut g = Gav::from_key_version("org.jacoco:org.jacoco.agent", "0.8.13").unwrap();
        g.classifier = Some("runtime".to_string());
        assert_eq!(g.notation(), "org.jacoco:org.jacoco.agent:0.8.13:runtime");
    }

    #[test]
    fn display_equals_notation() {
        let g = Gav::from_key_version("com.example:foo", "2.0").unwrap();
        assert_eq!(format!("{}", g), g.notation());
    }

    #[test]
    fn from_key_version_classifier_sets_field_and_path() {
        let g = Gav::from_key_version_classifier(
            "org.jacoco:org.jacoco.agent",
            "0.8.13",
            Some("runtime"),
        )
        .unwrap();
        assert_eq!(g.classifier.as_deref(), Some("runtime"));
        assert!(g.extension.is_none());
        assert!(
            g.relative_path().contains("-runtime.jar"),
            "expected classifier in path, got {}",
            g.relative_path()
        );
        assert_eq!(g.notation(), "org.jacoco:org.jacoco.agent:0.8.13:runtime");
    }

    #[test]
    fn relative_path_with_extension() {
        let g = Gav::from_key_version_classifier_extension(
            "com.google.protobuf:protoc",
            "3.25.0",
            Some("linux-x86_64"),
            "exe",
        )
        .unwrap();
        assert_eq!(
            g.relative_path(),
            "com/google/protobuf/protoc/3.25.0/protoc-3.25.0-linux-x86_64.exe"
        );
        assert!(g.extension.as_deref() == Some("exe"));
    }

    #[test]
    fn from_key_version_classifier_extension_defaults_jar() {
        let g = Gav::from_key_version_classifier_extension(
            "org.jacoco:org.jacoco.agent",
            "0.8.13",
            Some("runtime"),
            "jar",
        )
        .unwrap();
        assert!(g.relative_path().ends_with("-runtime.jar"));
    }

    #[test]
    fn rejects_path_traversal_in_version() {
        assert!(Gav::from_key_version("g:a", "../evil").is_err());
        assert!(Gav::from_key_version("g:a", "..").is_err());
    }

    #[test]
    fn rejects_slash_or_backslash() {
        assert!(Gav::from_key_version("g:a", "1/0").is_err());
        assert!(Gav::from_key_version("g:a", "1\\0").is_err());
        assert!(Gav::from_key_version("com/evil:foo", "1.0").is_err());
    }

    #[test]
    fn rejects_other_invalid_chars() {
        assert!(Gav::from_key_version("g:a", "1.0$").is_err());
        assert!(Gav::from_key_version("g:a", "v with space").is_err());
    }

    #[test]
    fn rejects_dot_or_empty_segments_in_group() {
        assert!(Gav::from_key_version("com..evil:foo", "1.0").is_err());
        assert!(Gav::from_key_version(".com:foo", "1.0").is_err());
        assert!(Gav::from_key_version("com.:foo", "1.0").is_err());
    }

    #[test]
    fn accepts_normal_coordinates() {
        let g = Gav::from_key_version("com.example:foo-bar_baz", "1.2.3").unwrap();
        assert_eq!(g.group, "com.example");
        assert_eq!(g.artifact, "foo-bar_baz");
        assert_eq!(g.version, "1.2.3");
        let _ = g.relative_path();
        let _ = g.local_repository_path();
    }

    #[test]
    fn validate_for_path_on_direct_struct() {
        let mut g = Gav::from_key_version("g:a", "1.0").unwrap();
        g.version = "../bad".to_string();
        assert!(g.validate_for_path().is_err());
        // relative_path uses expect (panics on violation for invariant); test via Result fn
        let p = g.local_repository_path();
        assert!(p.is_err());
    }

    #[test]
    fn is_snapshot_strict_suffix() {
        assert!(Gav::from_key_version("g:a", "1.0-SNAPSHOT")
            .unwrap()
            .is_snapshot());
        assert!(!Gav::from_key_version("g:a", "1.0").unwrap().is_snapshot());
        assert!(!Gav::from_key_version("g:a", "1.0-snapshot")
            .unwrap()
            .is_snapshot());
        assert!(!Gav::from_key_version("g:a", "1.0-SNAPSHOTX")
            .unwrap()
            .is_snapshot());
        assert!(!Gav::from_key_version("g:a", "1.0-20260610.123456-3")
            .unwrap()
            .is_snapshot());
    }

    #[test]
    fn relative_path_unique_snapshot() {
        let mut g = Gav::from_key_version("com.example:foo", "1.0-SNAPSHOT").unwrap();
        g.snapshot_version = Some("1.0-20260610.123456-3".into());
        assert_eq!(
            g.relative_path(),
            "com/example/foo/1.0-SNAPSHOT/foo-1.0-20260610.123456-3.jar"
        );
        assert_eq!(
            g.relative_pom_path(),
            "com/example/foo/1.0-SNAPSHOT/foo-1.0-20260610.123456-3.pom"
        );
    }

    #[test]
    fn relative_path_unique_snapshot_with_classifier() {
        let mut g =
            Gav::from_key_version_classifier("com.example:foo", "1.0-SNAPSHOT", Some("sources"))
                .unwrap();
        g.snapshot_version = Some("1.0-20260610.123456-3".into());
        assert_eq!(
            g.relative_path(),
            "com/example/foo/1.0-SNAPSHOT/foo-1.0-20260610.123456-3-sources.jar"
        );
    }

    #[test]
    fn relative_path_non_unique_snapshot() {
        let g = Gav::from_key_version("com.example:foo", "1.0-SNAPSHOT").unwrap();
        assert!(g.snapshot_version.is_none());
        assert_eq!(
            g.relative_path(),
            "com/example/foo/1.0-SNAPSHOT/foo-1.0-SNAPSHOT.jar"
        );
    }

    #[test]
    fn snapshot_version_excluded_from_eq_and_hash() {
        use std::collections::HashSet;
        let mut a = Gav::from_key_version("com.example:foo", "1.0-SNAPSHOT").unwrap();
        let mut b = a.clone();
        a.snapshot_version = Some("1.0-20260610.123456-1".into());
        b.snapshot_version = Some("1.0-20260610.123456-2".into());
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn snapshot_pin_key_uses_base_version() {
        let mut g = Gav::from_key_version("com.example:foo", "1.0-SNAPSHOT").unwrap();
        g.snapshot_version = Some("1.0-20260610.123456-3".into());
        assert_eq!(g.snapshot_pin_key(), "com.example:foo:1.0-SNAPSHOT");
    }
}
