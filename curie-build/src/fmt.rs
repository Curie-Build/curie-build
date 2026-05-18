//! `curie fmt` — format Java source files with palantir-java-format.
//!
//! # How it works
//!
//! palantir-java-format (PJF) does not publish a standalone executable fat
//! JAR.  Instead, we resolve `com.palantir.javaformat:palantir-java-format`
//! and its transitive closure from Maven Central — exactly the same Maven
//! resolver already used for user project dependencies (`curie_deps`).  The
//! JARs land in `~/.m2` on the first run and are reused on subsequent calls
//! with zero network traffic.
//!
//! The formatter's `Main` class is then invoked via
//! `java -cp <all-jars> com.palantir.javaformat.java.Main`.
//!
//! # JVM flags
//!
//! PJF's `palantir-java-format.jar` manifest declares the `Add-Exports`
//! entries it needs.  When we invoke the JAR via `-cp` (not `-jar`) we must
//! supply those `--add-exports` flags ourselves.
//!
//! # File discovery
//!
//! All `*.java` files under the project's `src/main/java/`,
//! `src/test/java/`, and flat-package source roots are collected and passed
//! to the formatter in a single invocation.

use crate::compile::flat_package_src_dirs;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use indicatif::ProgressBar;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

/// PJF coordinate pinned to the latest version on Maven Central.
const PJF_COORD: &str = "com.palantir.javaformat:palantir-java-format";
const PJF_VERSION: &str = "2.90.0";

/// Fully-qualified name of PJF's CLI entry point.
const PJF_MAIN: &str = "com.palantir.javaformat.java.Main";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run palantir-java-format on all Java sources in `project_root`.
///
/// * `check_only` — use `--dry-run --set-exit-if-changed`; prints only the
///   paths of files that would change and exits non-zero if any would (CI).
///   No files are modified.
/// * `offline` — refuse to download JARs from Maven Central; fail if the
///   PJF JARs are not already in the local `~/.m2` cache.
/// * `log` — when `Some`, all diagnostic lines are emitted via
///   `ProgressBar::println` so they appear above indicatif's live bars
///   without corrupting them.  When `None`, plain `println!` is used.
pub fn run_fmt(
    project_root: &Path,
    check_only: bool,
    offline: bool,
    log: Option<&ProgressBar>,
) -> Result<()> {
    // Helper: print through the progress bar if present, else to stdout.
    let say = |msg: String| {
        if let Some(pb) = log {
            pb.println(msg);
        } else {
            println!("{}", msg);
        }
    };

    // --- discover source files ----------------------------------------------
    let java_files = collect_java_files(project_root);

    if java_files.is_empty() {
        say("fmt: no Java source files found — nothing to do.".into());
        return Ok(());
    }

    say(format!(
        "fmt: {} {} file(s) with palantir-java-format {}",
        if check_only { "checking" } else { "formatting" },
        java_files.len(),
        PJF_VERSION,
    ));

    // --- resolve PJF from Maven Central (or ~/.m2 cache) --------------------
    let pjf_jars = resolve(
        &[(PJF_COORD, PJF_VERSION)],
        &ResolveOptions {
            extra_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline,
        },
    )
    .context("failed to resolve palantir-java-format from Maven Central")?;

    // --- build classpath ----------------------------------------------------
    let cp = pjf_jars
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(":");

    // --- invoke java --------------------------------------------------------
    let mut cmd = Command::new("java");

    for flag in jvm_add_exports() {
        cmd.arg(flag);
    }

    cmd.arg("-cp").arg(&cp).arg(PJF_MAIN);

    // AOSP style = 4-space indentation (Google style uses 2 spaces).
    cmd.arg("--aosp");

    if check_only {
        // --dry-run prints only the paths of files that would change.
        // --set-exit-if-changed makes the process exit 1 when any would.
        cmd.args(["--dry-run", "--set-exit-if-changed"]);
    } else {
        // --replace writes formatted output back to each file in place.
        cmd.arg("--replace");
    }

    for f in &java_files {
        cmd.arg(f);
    }

    let status = cmd
        .status()
        .context("failed to launch `java` — is a JDK installed and on PATH?")?;

    if !status.success() {
        if check_only {
            bail!(
                "fmt: one or more files are not correctly formatted. \
                 Run `curie fmt` (without --check) to fix them."
            );
        } else {
            bail!("palantir-java-format exited with status {}", status);
        }
    }

    if !check_only {
        say("fmt: done.".into());
    }

    Ok(())
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
        // Must cover all seven packages PJF needs.
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
        // Non-java file — must be excluded.
        fs::write(main_java.join("README.txt"), "docs").unwrap();

        let files = collect_java_files(tmp.path());
        assert_eq!(files.len(), 3, "expected 3 .java files, got {:?}", files);
        // All returned paths end with .java.
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
}
