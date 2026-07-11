//! Foreign (non-Curie) project detection and default tool commands.
//!
//! A workspace member without a `Curie.toml` — or with one but listed under
//! `[workspace.foreign]` — is built by an external tool. This module
//! auto-detects the tool from marker files and supplies default
//! build/test/clean argv.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::Path;

/// Out-of-source directory used by default CMake build/test/clean commands.
pub const CMAKE_DEFAULT_BUILD_DIR: &str = "build";

/// External build tool that owns a foreign workspace member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForeignTool {
    Curie,
    Maven,
    Gradle,
    Cargo,
    Make,
    CMake,
    Npm,
    Bun,
    Yarn,
}

impl ForeignTool {
    /// Short label for `curie list` and error messages.
    pub fn label(self) -> &'static str {
        match self {
            ForeignTool::Curie => "curie",
            ForeignTool::Maven => "maven",
            ForeignTool::Gradle => "gradle",
            ForeignTool::Cargo => "cargo",
            ForeignTool::Make => "make",
            ForeignTool::CMake => "cmake",
            ForeignTool::Npm => "npm",
            ForeignTool::Bun => "bun",
            ForeignTool::Yarn => "yarn",
        }
    }

    /// Default build argv (no shell).  `dir` is used for wrapper detection
    /// (`mvnw`/`gradlew`) and for resolving the curie binary.
    ///
    /// For CMake this is only the *build* step (`cmake --build build`); the
    /// runner runs `cmake -S . -B build` first when this default is used.
    pub fn default_build_command(self, dir: &Path) -> Vec<String> {
        match self {
            ForeignTool::Maven => vec![maven_bin(dir), "-B".into(), "package".into()],
            ForeignTool::Gradle => vec![gradle_bin(dir), "build".into()],
            ForeignTool::Cargo => vec!["cargo".into(), "build".into()],
            ForeignTool::Make => vec!["make".into()],
            ForeignTool::CMake => vec![
                "cmake".into(),
                "--build".into(),
                CMAKE_DEFAULT_BUILD_DIR.into(),
            ],
            ForeignTool::Npm => vec!["npm".into(), "run".into(), "build".into()],
            ForeignTool::Bun => vec!["bun".into(), "run".into(), "build".into()],
            ForeignTool::Yarn => vec!["yarn".into(), "run".into(), "build".into()],
            ForeignTool::Curie => vec![curie_bin(), "build".into()],
        }
    }

    /// Configure argv for the default out-of-source CMake tree.
    ///
    /// Invoked by the build runner before [`Self::default_build_command`] when
    /// the member uses the stock CMake build command (two steps, no shell).
    pub fn cmake_configure_command() -> Vec<String> {
        vec![
            "cmake".into(),
            "-S".into(),
            ".".into(),
            "-B".into(),
            CMAKE_DEFAULT_BUILD_DIR.into(),
        ]
    }

    /// Default test argv.
    pub fn default_test_command(self, dir: &Path) -> Vec<String> {
        match self {
            ForeignTool::Maven => vec![maven_bin(dir), "-B".into(), "test".into()],
            ForeignTool::Gradle => vec![gradle_bin(dir), "test".into()],
            ForeignTool::Cargo => vec!["cargo".into(), "test".into()],
            ForeignTool::Make => vec!["make".into(), "test".into()],
            ForeignTool::CMake => vec![
                "ctest".into(),
                "--test-dir".into(),
                CMAKE_DEFAULT_BUILD_DIR.into(),
                "--output-on-failure".into(),
            ],
            ForeignTool::Npm => vec!["npm".into(), "test".into()],
            ForeignTool::Bun => vec!["bun".into(), "test".into()],
            ForeignTool::Yarn => vec!["yarn".into(), "test".into()],
            ForeignTool::Curie => vec![curie_bin(), "test".into()],
        }
    }

    /// Default clean argv.
    pub fn default_clean_command(self, dir: &Path) -> Vec<String> {
        match self {
            ForeignTool::Maven => vec![maven_bin(dir), "-B".into(), "clean".into()],
            ForeignTool::Gradle => vec![gradle_bin(dir), "clean".into()],
            ForeignTool::Cargo => vec!["cargo".into(), "clean".into()],
            ForeignTool::Make => vec!["make".into(), "clean".into()],
            ForeignTool::CMake => vec![
                "cmake".into(),
                "--build".into(),
                CMAKE_DEFAULT_BUILD_DIR.into(),
                "--target".into(),
                "clean".into(),
            ],
            ForeignTool::Npm => vec!["npm".into(), "run".into(), "clean".into()],
            ForeignTool::Bun => vec!["bun".into(), "run".into(), "clean".into()],
            ForeignTool::Yarn => vec!["yarn".into(), "run".into(), "clean".into()],
            ForeignTool::Curie => vec![curie_bin(), "clean".into()],
        }
    }
}

fn maven_bin(dir: &Path) -> String {
    if dir.join("mvnw").exists() {
        "./mvnw".into()
    } else {
        "mvn".into()
    }
}

fn gradle_bin(dir: &Path) -> String {
    if dir.join("gradlew").exists() {
        "./gradlew".into()
    } else {
        "gradle".into()
    }
}

/// Resolve argv[0] for foreign-curie members: the currently running binary,
/// falling back to `curie` on PATH.
pub fn curie_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "curie".into())
}

/// Marker groups used for auto-detection.  Exactly one group must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerGroup {
    Curie,
    Maven,
    Gradle,
    Cargo,
    Make,
    CMake,
    Node,
}

impl MarkerGroup {
    fn name(self) -> &'static str {
        match self {
            MarkerGroup::Curie => "Curie.toml",
            MarkerGroup::Maven => "pom.xml",
            MarkerGroup::Gradle => "build.gradle / settings.gradle",
            MarkerGroup::Cargo => "Cargo.toml",
            MarkerGroup::Make => "Makefile",
            MarkerGroup::CMake => "CMakeLists.txt",
            MarkerGroup::Node => "package.json",
        }
    }

    fn present(self, dir: &Path) -> bool {
        match self {
            MarkerGroup::Curie => dir.join("Curie.toml").exists(),
            MarkerGroup::Maven => dir.join("pom.xml").exists(),
            MarkerGroup::Gradle => {
                dir.join("build.gradle").exists()
                    || dir.join("build.gradle.kts").exists()
                    || dir.join("settings.gradle").exists()
                    || dir.join("settings.gradle.kts").exists()
            }
            MarkerGroup::Cargo => dir.join("Cargo.toml").exists(),
            MarkerGroup::Make => {
                dir.join("Makefile").exists()
                    || dir.join("GNUmakefile").exists()
                    || dir.join("makefile").exists()
            }
            MarkerGroup::CMake => dir.join("CMakeLists.txt").exists(),
            MarkerGroup::Node => dir.join("package.json").exists(),
        }
    }
}

const ALL_GROUPS: &[MarkerGroup] = &[
    MarkerGroup::Curie,
    MarkerGroup::Maven,
    MarkerGroup::Gradle,
    MarkerGroup::Cargo,
    MarkerGroup::Make,
    MarkerGroup::CMake,
    MarkerGroup::Node,
];

/// True when `dir` has any foreign-tool marker (including `Curie.toml`).
pub fn has_markers(dir: &Path) -> bool {
    ALL_GROUPS.iter().any(|g| g.present(dir))
}

/// Auto-detect the foreign tool for `dir` from marker files.
///
/// Exactly one marker group must match; otherwise a hard error names the
/// found markers and suggests an explicit `type = "..."`.
pub fn detect_tool(dir: &Path) -> Result<ForeignTool> {
    let found: Vec<MarkerGroup> = ALL_GROUPS
        .iter()
        .copied()
        .filter(|g| g.present(dir))
        .collect();

    match found.as_slice() {
        [] => bail!(
            "cannot detect foreign project type in {}: no marker files found \
             (looked for Curie.toml, pom.xml, build.gradle[.kts]/settings.gradle[.kts], \
             Cargo.toml, Makefile/GNUmakefile/makefile, CMakeLists.txt, package.json). \
             Set type = \"...\" under [workspace.foreign] to override.",
            dir.display(),
        ),
        [MarkerGroup::Curie] => Ok(ForeignTool::Curie),
        [MarkerGroup::Maven] => Ok(ForeignTool::Maven),
        [MarkerGroup::Gradle] => Ok(ForeignTool::Gradle),
        [MarkerGroup::Cargo] => Ok(ForeignTool::Cargo),
        [MarkerGroup::Make] => Ok(ForeignTool::Make),
        [MarkerGroup::CMake] => Ok(ForeignTool::CMake),
        [MarkerGroup::Node] => Ok(detect_node_tool(dir)),
        many => {
            let names: Vec<&str> = many.iter().map(|g| g.name()).collect();
            bail!(
                "ambiguous foreign project type in {}: found markers for {} — \
                 set type = \"...\" under [workspace.foreign] to disambiguate",
                dir.display(),
                names.join(" and "),
            );
        }
    }
}

/// Sub-resolve npm/bun/yarn from lockfiles once `package.json` is present.
fn detect_node_tool(dir: &Path) -> ForeignTool {
    if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
        ForeignTool::Bun
    } else if dir.join("yarn.lock").exists() {
        ForeignTool::Yarn
    } else {
        ForeignTool::Npm
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn detect_each_marker() {
        let cases: &[(&str, ForeignTool)] = &[
            ("Curie.toml", ForeignTool::Curie),
            ("pom.xml", ForeignTool::Maven),
            ("build.gradle", ForeignTool::Gradle),
            ("build.gradle.kts", ForeignTool::Gradle),
            ("settings.gradle", ForeignTool::Gradle),
            ("Cargo.toml", ForeignTool::Cargo),
            ("Makefile", ForeignTool::Make),
            ("GNUmakefile", ForeignTool::Make),
            ("makefile", ForeignTool::Make),
            ("CMakeLists.txt", ForeignTool::CMake),
            ("package.json", ForeignTool::Npm),
        ];
        for (marker, expected) in cases {
            let dir = tmp();
            fs::write(dir.path().join(marker), "").unwrap();
            assert_eq!(
                detect_tool(dir.path()).unwrap(),
                *expected,
                "marker {marker}"
            );
        }
    }

    #[test]
    fn detect_bun_and_yarn_via_lockfile() {
        let dir = tmp();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("bun.lock"), "").unwrap();
        assert_eq!(detect_tool(dir.path()).unwrap(), ForeignTool::Bun);

        let dir = tmp();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        assert_eq!(detect_tool(dir.path()).unwrap(), ForeignTool::Yarn);
    }

    #[test]
    fn ambiguity_error_names_both_markers() {
        let dir = tmp();
        fs::write(dir.path().join("Curie.toml"), "").unwrap();
        fs::write(dir.path().join("pom.xml"), "").unwrap();
        let err = detect_tool(dir.path()).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains("Curie.toml"), "got: {err}");
        assert!(err.contains("pom.xml"), "got: {err}");
    }

    #[test]
    fn empty_dir_errors() {
        let dir = tmp();
        let err = detect_tool(dir.path()).unwrap_err().to_string();
        assert!(err.contains("no marker"), "got: {err}");
    }

    #[test]
    fn has_markers_true_and_false() {
        let dir = tmp();
        assert!(!has_markers(dir.path()));
        fs::write(dir.path().join("Makefile"), "").unwrap();
        assert!(has_markers(dir.path()));
    }

    #[test]
    fn default_commands_per_tool() {
        let dir = tmp();
        assert_eq!(
            ForeignTool::Cargo.default_build_command(dir.path()),
            vec!["cargo", "build"]
        );
        assert_eq!(
            ForeignTool::Cargo.default_test_command(dir.path()),
            vec!["cargo", "test"]
        );
        assert_eq!(
            ForeignTool::Cargo.default_clean_command(dir.path()),
            vec!["cargo", "clean"]
        );
        assert_eq!(
            ForeignTool::Make.default_build_command(dir.path()),
            vec!["make"]
        );
        assert_eq!(
            ForeignTool::Make.default_test_command(dir.path()),
            vec!["make", "test"]
        );
        assert_eq!(
            ForeignTool::Maven.default_build_command(dir.path()),
            vec!["mvn", "-B", "package"]
        );
        assert_eq!(
            ForeignTool::Npm.default_build_command(dir.path()),
            vec!["npm", "run", "build"]
        );
        assert_eq!(
            ForeignTool::CMake.default_build_command(dir.path()),
            vec!["cmake", "--build", "build"]
        );
        assert_eq!(
            ForeignTool::CMake.default_test_command(dir.path()),
            vec!["ctest", "--test-dir", "build", "--output-on-failure"]
        );
        assert_eq!(
            ForeignTool::CMake.default_clean_command(dir.path()),
            vec!["cmake", "--build", "build", "--target", "clean"]
        );
        assert_eq!(
            ForeignTool::cmake_configure_command(),
            vec!["cmake", "-S", ".", "-B", "build"]
        );
        let curie_build = ForeignTool::Curie.default_build_command(dir.path());
        assert_eq!(curie_build.len(), 2);
        assert_eq!(curie_build[1], "build");
    }

    #[test]
    fn maven_and_gradle_prefer_wrappers() {
        let dir = tmp();
        fs::write(dir.path().join("mvnw"), "").unwrap();
        assert_eq!(
            ForeignTool::Maven.default_build_command(dir.path())[0],
            "./mvnw"
        );
        let dir = tmp();
        fs::write(dir.path().join("gradlew"), "").unwrap();
        assert_eq!(
            ForeignTool::Gradle.default_test_command(dir.path())[0],
            "./gradlew"
        );
    }

    #[test]
    fn label_is_lowercase() {
        assert_eq!(ForeignTool::Make.label(), "make");
        assert_eq!(ForeignTool::CMake.label(), "cmake");
        assert_eq!(ForeignTool::Curie.label(), "curie");
    }
}
