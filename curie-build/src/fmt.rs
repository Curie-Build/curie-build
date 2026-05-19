//! `curie fmt` — format Java and Kotlin source files.
//!
//! # How it works
//!
//! Two formatters are used, each resolved from Maven Central on first run and
//! cached in `~/.m2` for all subsequent invocations:
//!
//!   * **Java** — [`palantir-java-format`] (`com.palantir.javaformat:palantir-java-format`)
//!     invoked via `java -cp <jars> com.palantir.javaformat.java.Main --aosp`.
//!   * **Kotlin** — [`ktfmt`] (`com.facebook:ktfmt`)
//!     invoked via `java -cp <jars> com.facebook.ktfmt.cli.Main --kotlinlang-style`.
//!
//! The Kotlin step is skipped entirely (including resolution) when the project
//! has no `.kt` sources.
//!
//! # JVM flags
//!
//! PJF's JAR manifest declares `Add-Exports` entries it needs on JDK 17+.
//! When the JAR is invoked via `-cp` (not `-jar`) the JVM does not process
//! manifest attributes, so we supply the flags explicitly.  ktfmt uses the
//! Kotlin compiler's PSI library which accesses `sun.misc.Unsafe`; we pass
//! `--enable-native-access=ALL-UNNAMED` to silence the warning (the same flag
//! used when invoking kotlinc for compilation).
//!
//! # File discovery
//!
//! **Java** — all `*.java` files under `src/main/java/`, `src/test/java/`,
//! and flat-package source roots.
//!
//! **Kotlin** — all `*.kt` files under `src/main/kotlin/`, `src/test/kotlin/`,
//! and flat-package source/test roots.  Test files (`*Test.kt` etc.) are
//! included — formatting tests is desirable.

use crate::compile::{flat_package_src_dirs, flat_package_test_dirs};
use anyhow::{bail, Context, Result};
use crate::build::central_repos;
use curie_deps::resolver::{resolve, DepEntry, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

/// PJF coordinate pinned to the latest version on Maven Central.
const PJF_COORD: &str = "com.palantir.javaformat:palantir-java-format";
const PJF_VERSION: &str = "2.90.0";
/// Fully-qualified name of PJF's CLI entry point.
const PJF_MAIN: &str = "com.palantir.javaformat.java.Main";

/// ktfmt coordinate pinned to the latest stable version on Maven Central.
const KTFMT_COORD: &str = "com.facebook:ktfmt";
const KTFMT_VERSION: &str = "0.51";
/// Fully-qualified name of ktfmt's CLI entry point (stable since 0.42).
const KTFMT_MAIN: &str = "com.facebook.ktfmt.cli.Main";

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Resolve palantir-java-format and its transitive dependencies once.
///
/// Callers that format more than one project in a single invocation
/// (e.g. `fmt_all` over a workspace) MUST call this once and reuse the
/// resulting classpath via [`run_fmt_with_jars`] — concurrent identical
/// `resolve()` calls would otherwise race on the same `~/.m2/.part` files.
pub fn resolve_pjf(offline: bool) -> Result<Vec<PathBuf>> {
    resolve(
        &[DepEntry { key: PJF_COORD, version: PJF_VERSION, repo_id: None }],
        &ResolveOptions {
            default_repos: central_repos(),
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline,
        },
    )
    .context("failed to resolve palantir-java-format from Maven Central")
}

/// Resolve ktfmt and its transitive dependencies once.
///
/// Same sharing contract as [`resolve_pjf`]: call once at the workspace level
/// and pass the result to [`run_fmt_with_jars`] to avoid concurrent races.
pub fn resolve_ktfmt(offline: bool) -> Result<Vec<PathBuf>> {
    resolve(
        &[(KTFMT_COORD, KTFMT_VERSION)],
        &ResolveOptions {
            extra_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline,
        },
    )
    .context("failed to resolve ktfmt from Maven Central")
}

/// Return `true` if `project_root` contains at least one `.kt` source file.
///
/// Used by `fmt_all` to decide whether to bother resolving ktfmt for the
/// whole workspace.  Short-circuits on the first match.
pub fn has_kotlin_sources(project_root: &Path) -> bool {
    kotlin_source_roots(project_root).into_iter().any(|root| {
        WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_type().is_file()
                    && e.path().extension().map_or(false, |x| x == "kt")
            })
    })
}

/// Run both formatters on all sources in `project_root`.
///
/// * `check_only` — dry-run + exit-non-zero-if-changed (CI mode).
/// * `offline` — resolve from `~/.m2` cache only.
pub fn run_fmt(project_root: &Path, check_only: bool, offline: bool) -> Result<()> {
    let pjf_jars = resolve_pjf(offline)?;
    let ktfmt_jars = if has_kotlin_sources(project_root) {
        resolve_ktfmt(offline)?
    } else {
        Vec::new()
    };
    run_fmt_with_jars(project_root, check_only, &pjf_jars, &ktfmt_jars)
}

/// Format against already-resolved formatter classpaths.
///
/// Splitting resolution out of the runner lets `fmt_all` resolve both
/// formatters exactly once and reuse them across every workspace member.
///
/// `ktfmt_jars` may be empty — pass `&[]` for projects / workspaces with
/// no Kotlin sources.  Both formatters run independently; if one fails the
/// other still runs so `--check` in CI reports all unformatted files in a
/// single pass.
pub fn run_fmt_with_jars(
    project_root: &Path,
    check_only: bool,
    pjf_jars: &[PathBuf],
    ktfmt_jars: &[PathBuf],
) -> Result<()> {
    let java_files = collect_java_files(project_root);
    let kotlin_files = if ktfmt_jars.is_empty() {
        vec![]
    } else {
        collect_kotlin_files(project_root)
    };

    if java_files.is_empty() && kotlin_files.is_empty() {
        return Ok(());
    }

    // Run both formatters independently so that both errors surface in one
    // --check pass rather than short-circuiting on the first failure.
    let java_err = if !java_files.is_empty() {
        fmt_java(&java_files, pjf_jars, check_only).err()
    } else {
        None
    };
    let kotlin_err = if !kotlin_files.is_empty() {
        fmt_kotlin(&kotlin_files, ktfmt_jars, check_only).err()
    } else {
        None
    };

    match (java_err, kotlin_err) {
        (None, None) => Ok(()),
        (Some(e), None) | (None, Some(e)) => Err(e),
        (Some(je), Some(ke)) => bail!("{:#}\n{:#}", je, ke),
    }
}

// ---------------------------------------------------------------------------
// Private formatter invocations
// ---------------------------------------------------------------------------

fn fmt_java(java_files: &[PathBuf], pjf_jars: &[PathBuf], check_only: bool) -> Result<()> {
    let cp = classpath(pjf_jars);
    let mut cmd = Command::new("java");
    for flag in jvm_add_exports() {
        cmd.arg(flag);
    }
    cmd.arg("-cp").arg(&cp).arg(PJF_MAIN).arg("--aosp");
    if check_only {
        cmd.args(["--dry-run", "--set-exit-if-changed"]);
    } else {
        cmd.arg("--replace");
    }
    for f in java_files {
        cmd.arg(f);
    }
    let status = cmd
        .status()
        .context("failed to launch `java` — is a JDK installed and on PATH?")?;
    if !status.success() {
        if check_only {
            bail!(
                "fmt: one or more Java files are not correctly formatted. \
                 Run `curie fmt` (without --check) to fix them."
            );
        } else {
            bail!("palantir-java-format exited with status {}", status);
        }
    }
    Ok(())
}

fn fmt_kotlin(kotlin_files: &[PathBuf], ktfmt_jars: &[PathBuf], check_only: bool) -> Result<()> {
    let cp = classpath(ktfmt_jars);
    let mut cmd = Command::new("java");
    // ktfmt bundles the Kotlin compiler PSI which accesses sun.misc.Unsafe;
    // suppress the warning with the same flag used when invoking kotlinc.
    cmd.arg("--enable-native-access=ALL-UNNAMED");
    cmd.arg("-cp").arg(&cp).arg(KTFMT_MAIN);
    // Kotlinlang style: 4-space indentation, matching Java's --aosp.
    cmd.arg("--kotlinlang-style");
    if check_only {
        cmd.args(["--dry-run", "--set-exit-if-changed"]);
    }
    // No --replace flag: ktfmt rewrites in-place by default.
    for f in kotlin_files {
        cmd.arg(f);
    }
    let status = cmd
        .status()
        .context("failed to launch `java` — is a JDK installed and on PATH?")?;
    if !status.success() {
        if check_only {
            bail!(
                "fmt: one or more Kotlin files are not correctly formatted. \
                 Run `curie fmt` (without --check) to fix them."
            );
        } else {
            bail!("ktfmt exited with status {}", status);
        }
    }
    Ok(())
}

fn classpath(jars: &[PathBuf]) -> String {
    jars.iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(":")
}

// ---------------------------------------------------------------------------
// Helpers (pub(crate) for unit-testability)
// ---------------------------------------------------------------------------

/// Return all `*.java` files under the project's source roots (sorted).
///
/// Source roots:
///   * `src/main/java/`  — Maven-style production sources
///   * `src/test/java/`  — Maven-style test sources
///   * flat-package dirs (`src/com.example.foo/` etc.)
pub(crate) fn collect_java_files(project_root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    let main_java = project_root.join("src").join("main").join("java");
    if main_java.exists() {
        roots.push(main_java);
    }

    let test_java = project_root.join("src").join("test").join("java");
    if test_java.exists() {
        roots.push(test_java);
    }

    roots.extend(flat_package_src_dirs(project_root));

    let mut files: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| {
            WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file()
                        && e.path().extension().map_or(false, |x| x == "java")
                })
                .map(|e| e.into_path())
        })
        .collect();

    files.sort();
    files
}

/// Return all `*.kt` files under the project's Kotlin source roots (sorted).
///
/// Source roots:
///   * `src/main/kotlin/` — Maven-style Kotlin production sources
///   * `src/test/kotlin/` — Maven-style Kotlin test sources
///   * flat-package `src/<dot-name>/` dirs — both `.java` and `.kt` files live here
///   * flat-package `tests/<dot-name>/` dirs — integration test sources
///
/// Unlike the compile-time Kotlin source discovery, test files (`*Test.kt`,
/// `*Tests.kt`, `*Spec.kt`) are NOT excluded — formatting test files is
/// desirable and mirrors the Java formatter behaviour.
pub(crate) fn collect_kotlin_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = kotlin_source_roots(project_root)
        .iter()
        .flat_map(|root| {
            WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file()
                        && e.path().extension().map_or(false, |x| x == "kt")
                })
                .map(|e| e.into_path())
        })
        .collect();

    files.sort();
    files.dedup();
    files
}

/// All source roots that may contain `.kt` files for this project.
fn kotlin_source_roots(project_root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    let main_kotlin = project_root.join("src").join("main").join("kotlin");
    if main_kotlin.exists() {
        roots.push(main_kotlin);
    }
    let test_kotlin = project_root.join("src").join("test").join("kotlin");
    if test_kotlin.exists() {
        roots.push(test_kotlin);
    }
    roots.extend(flat_package_src_dirs(project_root));
    roots.extend(flat_package_test_dirs(project_root));
    roots
}

/// Return the `--add-exports` JVM flags required by PJF on JDK 17+.
///
/// These mirror the `Add-Exports` attribute in PJF's JAR manifest.  When
/// the JAR is invoked via `-cp` instead of `-jar` the JVM does not process
/// manifest attributes, so we must supply the flags explicitly.
pub(crate) fn jvm_add_exports() -> Vec<String> {
    let packages = [
        "com.sun.tools.javac.api",
        "com.sun.tools.javac.code",
        "com.sun.tools.javac.file",
        "com.sun.tools.javac.main",
        "com.sun.tools.javac.parser",
        "com.sun.tools.javac.tree",
        "com.sun.tools.javac.util",
    ];
    packages
        .iter()
        .map(|p| format!("--add-exports=jdk.compiler/{}=ALL-UNNAMED", p))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- jvm_add_exports ----------------------------------------------------

    #[test]
    fn jvm_add_exports_covers_required_packages() {
        let flags = jvm_add_exports();
        let required = [
            "com.sun.tools.javac.api",
            "com.sun.tools.javac.code",
            "com.sun.tools.javac.file",
            "com.sun.tools.javac.main",
            "com.sun.tools.javac.parser",
            "com.sun.tools.javac.tree",
            "com.sun.tools.javac.util",
        ];
        for pkg in required {
            let needle = format!("jdk.compiler/{}=ALL-UNNAMED", pkg);
            assert!(
                flags.iter().any(|f| f.contains(&needle)),
                "missing --add-exports for {pkg}"
            );
        }
    }

    #[test]
    fn jvm_add_exports_all_start_with_flag() {
        for flag in jvm_add_exports() {
            assert!(
                flag.starts_with("--add-exports="),
                "unexpected flag format: {flag}"
            );
        }
    }

    // --- collect_java_files -------------------------------------------------

    #[test]
    fn collect_java_files_empty_project() {
        let tmp = TempDir::new().unwrap();
        let files = collect_java_files(tmp.path());
        assert!(files.is_empty(), "expected no files in empty project");
    }

    #[test]
    fn collect_java_files_maven_layout() {
        let tmp = TempDir::new().unwrap();
        let main_java = tmp.path().join("src").join("main").join("java");
        let test_java = tmp.path().join("src").join("test").join("java");
        fs::create_dir_all(&main_java).unwrap();
        fs::create_dir_all(&test_java).unwrap();

        fs::write(main_java.join("Foo.java"), "class Foo {}").unwrap();
        fs::write(main_java.join("Bar.java"), "class Bar {}").unwrap();
        fs::write(test_java.join("FooTest.java"), "class FooTest {}").unwrap();
        fs::write(main_java.join("README.txt"), "docs").unwrap();

        let files = collect_java_files(tmp.path());
        assert_eq!(files.len(), 3, "expected 3 .java files, got {:?}", files);
        for f in &files {
            assert_eq!(f.extension().unwrap(), "java");
        }
    }

    #[test]
    fn collect_java_files_returns_sorted() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src").join("main").join("java");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Zoo.java"), "class Zoo {}").unwrap();
        fs::write(src.join("Alpha.java"), "class Alpha {}").unwrap();
        fs::write(src.join("Mango.java"), "class Mango {}").unwrap();

        let files = collect_java_files(tmp.path());
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "files should be returned sorted");
    }

    #[test]
    fn collect_java_files_recursive() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp
            .path()
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("Deep.java"), "class Deep {}").unwrap();

        let files = collect_java_files(tmp.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("Deep.java"));
    }

    // --- collect_kotlin_files -----------------------------------------------

    #[test]
    fn collect_kotlin_files_empty_project() {
        let tmp = TempDir::new().unwrap();
        assert!(collect_kotlin_files(tmp.path()).is_empty());
    }

    #[test]
    fn collect_kotlin_files_maven_layout() {
        let tmp = TempDir::new().unwrap();
        let main_kt = tmp.path().join("src").join("main").join("kotlin");
        let test_kt = tmp.path().join("src").join("test").join("kotlin");
        fs::create_dir_all(&main_kt).unwrap();
        fs::create_dir_all(&test_kt).unwrap();

        fs::write(main_kt.join("Greeting.kt"), "class Greeting").unwrap();
        fs::write(test_kt.join("GreetingTest.kt"), "class GreetingTest").unwrap();
        // Non-.kt file — must be excluded.
        fs::write(main_kt.join("notes.txt"), "docs").unwrap();

        let files = collect_kotlin_files(tmp.path());
        assert_eq!(files.len(), 2, "expected 2 .kt files, got {:?}", files);
        for f in &files {
            assert_eq!(f.extension().unwrap(), "kt");
        }
    }

    #[test]
    fn collect_kotlin_files_flat_package() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("src").join("com.example.mixed");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("Greeting.kt"), "class Greeting").unwrap();
        fs::write(pkg.join("Main.java"), "class Main {}").unwrap(); // should be ignored

        let files = collect_kotlin_files(tmp.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("Greeting.kt"));
    }

    #[test]
    fn collect_kotlin_files_includes_test_files() {
        // Regression: compile excludes *Test.kt; fmt must include them.
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("src").join("com.example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("Foo.kt"), "class Foo").unwrap();
        fs::write(pkg.join("FooTest.kt"), "class FooTest").unwrap();
        fs::write(pkg.join("FooSpec.kt"), "class FooSpec").unwrap();

        let files = collect_kotlin_files(tmp.path());
        assert_eq!(files.len(), 3, "test/spec files must be included in fmt: {:?}", files);
    }

    #[test]
    fn collect_kotlin_files_includes_tests_dir() {
        // Integration tests in flat-package tests/ should also be formatted.
        let tmp = TempDir::new().unwrap();
        let tests_pkg = tmp.path().join("tests").join("com.example");
        fs::create_dir_all(&tests_pkg).unwrap();
        fs::write(tests_pkg.join("IntTest.kt"), "class IntTest").unwrap();

        let files = collect_kotlin_files(tmp.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("IntTest.kt"));
    }

    #[test]
    fn collect_kotlin_files_returns_sorted() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src").join("main").join("kotlin");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Zoo.kt"), "class Zoo").unwrap();
        fs::write(src.join("Alpha.kt"), "class Alpha").unwrap();

        let files = collect_kotlin_files(tmp.path());
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    // --- has_kotlin_sources -------------------------------------------------

    #[test]
    fn has_kotlin_sources_false_for_empty_project() {
        let tmp = TempDir::new().unwrap();
        assert!(!has_kotlin_sources(tmp.path()));
    }

    #[test]
    fn has_kotlin_sources_false_for_java_only() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src").join("main").join("java");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Foo.java"), "class Foo {}").unwrap();
        assert!(!has_kotlin_sources(tmp.path()));
    }

    #[test]
    fn has_kotlin_sources_true_when_kt_present() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src").join("main").join("kotlin");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Greeting.kt"), "class Greeting").unwrap();
        assert!(has_kotlin_sources(tmp.path()));
    }

    #[test]
    fn has_kotlin_sources_true_for_flat_package_kt() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("src").join("com.example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("App.kt"), "fun main() {}").unwrap();
        assert!(has_kotlin_sources(tmp.path()));
    }
}
