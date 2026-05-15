use crate::{build, descriptor};
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;

/// Version of JUnit Platform Console Standalone resolved from Maven Central.
const JUNIT_STANDALONE_VERSION: &str = "6.0.3";
const JUNIT_STANDALONE_COORD: &str =
    "org.junit.platform:junit-platform-console-standalone";

/// Compile test sources and run all tests via the JUnit Platform Console
/// Standalone launcher.
///
/// `classes_dir`  — directory containing already-compiled production classes.
/// `dep_jars`     — resolved production dependency JARs.
/// `filter`       — optional class-name pattern passed to `--include-classname`.
///
/// Returns `Ok(())` when all tests pass (or when no test sources exist).
/// Returns `Err` when compilation fails or any test fails.
pub fn run_tests(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    classes_dir: &Path,
    dep_jars: &[PathBuf],
    filter: Option<&str>,
) -> Result<()> {
    // --- discover test sources -----------------------------------------------
    let test_sources = discover_test_sources(project_root);

    if test_sources.is_empty() {
        println!("  Tests           no test sources found");
        return Ok(());
    }

    println!("  Tests           {} test source file(s)", test_sources.len());

    // --- resolve JUnit standalone launcher -----------------------------------
    let extra_repos = build::extra_repos(desc);

    let standalone_jar = resolve_standalone(&extra_repos)
        .context("failed to resolve JUnit Platform Console Standalone")?;

    // --- resolve test-scoped dependencies ------------------------------------
    let test_dep_jars = if desc.test_dependencies.is_empty() {
        vec![]
    } else {
        let pairs: Vec<(&str, &str)> = desc
            .test_dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        resolve(
            &pairs,
            &ResolveOptions {
                extra_repos: extra_repos.clone(),
                verbose: false,
            },
        )
        .context("test dependency resolution failed")?
    };

    // --- compile test sources (incremental) ----------------------------------
    let test_classes_dir = project_root.join("target").join("test-classes");
    std::fs::create_dir_all(&test_classes_dir)
        .context("failed to create target/test-classes")?;

    let toml_path = project_root.join("curie.toml");

    if needs_test_recompile(&test_sources, &test_classes_dir, &toml_path) {
        println!(
            "  Compile tests   {} source file(s)",
            test_sources.len()
        );

        // Classpath for compiling tests:
        //   production classes + prod deps + test deps + standalone launcher
        let mut compile_cp: Vec<PathBuf> = Vec::new();
        compile_cp.push(classes_dir.to_path_buf());
        compile_cp.extend_from_slice(dep_jars);
        compile_cp.extend_from_slice(&test_dep_jars);
        compile_cp.push(standalone_jar.clone());

        let mut javac = Command::new("javac");
        javac
            .arg("--release")
            .arg(&desc.java.source_compatibility)
            .arg("-g")
            .arg("-d")
            .arg(&test_classes_dir)
            .arg("-cp")
            .arg(build::classpath_string(&compile_cp));

        for src in &test_sources {
            javac.arg(src);
        }

        let status = javac
            .status()
            .context("failed to invoke javac — is a JDK installed?")?;

        if !status.success() {
            bail!("test compilation failed");
        }
    } else {
        println!("  Compile tests   up to date");
    }

    // --- skip if stamp is newer than all inputs ------------------------------
    // When no filter is active, check whether the test-stamp is newer than
    // every input that could invalidate results.  A filter run always executes
    // (it is a partial run and must not mark the full suite as passing).
    let stamp_path = project_root.join("target").join(".test-stamp");

    if filter.is_none() && !needs_test_run(&test_sources, classes_dir, &toml_path, &stamp_path) {
        println!("  Tests           up to date");
        return Ok(());
    }

    // --- run tests -----------------------------------------------------------
    // Classpath for running tests:
    //   test classes + production classes + prod deps + test deps
    // (standalone is provided as -jar, not on -cp)
    let mut run_cp: Vec<PathBuf> = Vec::new();
    run_cp.push(test_classes_dir.clone());
    run_cp.push(classes_dir.to_path_buf());
    run_cp.extend_from_slice(dep_jars);
    run_cp.extend_from_slice(&test_dep_jars);

    println!();

    let mut java = Command::new("java");
    java.arg("-jar")
        .arg(&standalone_jar)
        .arg("execute")
        .arg("-cp")
        .arg(build::classpath_string(&run_cp))
        .arg("--scan-class-path");

    if let Some(f) = filter {
        java.arg(format!("--include-classname={}", f));
    }

    let status = java
        .status()
        .context("failed to invoke java — is a JRE installed?")?;

    println!();

    if !status.success() {
        bail!("tests failed");
    }

    // --- write stamp on success ----------------------------------------------
    // Only written when no filter was active (a partial run must not mark the
    // full suite as passing).
    std::fs::write(&stamp_path, b"")
        .with_context(|| format!("failed to write test stamp {}", stamp_path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------

/// Collect test source files from both co-located and separate-tree locations.
///
/// Co-located: `src/main/java` — files ending in `Test.java`, `Tests.java`,
/// or `Spec.java`.
///
/// Separate tree: `src/test/java` — all `*.java` files.
///
/// Results are merged, deduplicated by canonical path, and sorted
/// lexicographically for a deterministic `javac` invocation.
fn discover_test_sources(project_root: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();

    // Co-located tests in src/main/java
    let main_src = project_root.join("src").join("main").join("java");
    if main_src.exists() {
        let colocated: Vec<PathBuf> = WalkDir::new(&main_src)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.java")
                    || name.ends_with("Tests.java")
                    || name.ends_with("Spec.java")
            })
            .map(|e| e.into_path())
            .collect();
        sources.extend(colocated);
    }

    // Separate test tree: src/test/java
    let test_src = project_root.join("src").join("test").join("java");
    if test_src.exists() {
        let separate: Vec<PathBuf> = WalkDir::new(&test_src)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".java"))
            .map(|e| e.into_path())
            .collect();
        sources.extend(separate);
    }

    // Deduplicate by canonical path (in case a path appears via both roots,
    // which shouldn't happen in practice but is cheap to guard against).
    sources.sort();
    sources.dedup();
    sources
}

// ---------------------------------------------------------------------------
// JUnit standalone resolution
// ---------------------------------------------------------------------------

fn resolve_standalone(extra_repos: &[curie_deps::repo::Repository]) -> Result<PathBuf> {
    let coord = format!(
        "{}:{}",
        JUNIT_STANDALONE_COORD, JUNIT_STANDALONE_VERSION
    );
    // coord is "group:artifact:version" — split off the version for the resolver.
    // The resolver takes (key, version) pairs where key = "group:artifact".
    let jars = resolve(
        &[(JUNIT_STANDALONE_COORD, JUNIT_STANDALONE_VERSION)],
        &ResolveOptions {
            extra_repos: extra_repos.to_vec(),
            verbose: false,
        },
    )
    .with_context(|| format!("failed to resolve {}", coord))?;

    // The standalone JAR is self-contained (fat JAR) — only one JAR is expected.
    // Filter to the standalone JAR itself (not transitive deps, which it
    // already bundles internally).
    jars.into_iter()
        .find(|p| {
            p.file_name()
                .map(|f| {
                    let s = f.to_string_lossy();
                    s.starts_with("junit-platform-console-standalone")
                })
                .unwrap_or(false)
        })
        .with_context(|| {
            format!(
                "junit-platform-console-standalone-{}.jar not found after resolution",
                JUNIT_STANDALONE_VERSION
            )
        })
}

// ---------------------------------------------------------------------------
// Incremental compilation check for test sources
// ---------------------------------------------------------------------------

/// Returns true when tests need to be executed:
/// - No stamp file exists (tests have never passed), OR
/// - Any test source file is newer than the stamp, OR
/// - Any production source file (compiled class) is newer than the stamp, OR
/// - curie.toml is newer than the stamp.
///
/// The stamp (`target/.test-stamp`) is written after every successful
/// full test run.  A filtered run (`curie test --filter`) never writes the
/// stamp and always bypasses this check so that a partial run cannot mask
/// failures in the untested portion.
fn needs_test_run(
    test_sources: &[PathBuf],
    classes_dir: &Path,
    toml_path: &Path,
    stamp_path: &Path,
) -> bool {
    let stamp = build::mtime(stamp_path);
    if stamp == SystemTime::UNIX_EPOCH {
        return true; // no stamp yet
    }
    if build::newest_mtime(test_sources) > stamp {
        return true;
    }
    if build::mtime(toml_path) > stamp {
        return true;
    }
    // Any change to production classes (recompile happened) invalidates the stamp.
    if build::oldest_mtime_in_dir(classes_dir) == SystemTime::UNIX_EPOCH {
        return true; // no classes — shouldn't happen here, but be safe
    }
    if newest_mtime_in_dir(classes_dir) > stamp {
        return true;
    }
    false
}

/// Return the newest `modified` time among all files under `dir`.
fn newest_mtime_in_dir(dir: &Path) -> SystemTime {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Returns true when test sources need to be recompiled:
/// - No test class files exist yet, OR
/// - Any test source file is newer than the oldest test class file, OR
/// - curie.toml is newer than the oldest test class file.
fn needs_test_recompile(
    test_sources: &[PathBuf],
    test_classes_dir: &Path,
    toml_path: &Path,
) -> bool {
    let oldest_class = oldest_mtime_in_dir(test_classes_dir);
    if oldest_class == SystemTime::UNIX_EPOCH {
        return true;
    }
    if build::newest_mtime(test_sources) > oldest_class {
        return true;
    }
    if build::mtime(toml_path) > oldest_class {
        return true;
    }
    false
}

/// Return the oldest `modified` time among all files under `dir`.
fn oldest_mtime_in_dir(dir: &Path) -> SystemTime {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .min()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}
