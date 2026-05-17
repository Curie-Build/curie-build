use crate::compile::{flat_package_src_dirs, flat_package_test_dirs, remove_stale_classes};
use crate::incremental::{
    javac_version, javac_version_stamp_path, mtime, newest_mtime, oldest_mtime_in_dir,
    write_javac_version_stamp, Inputs, Stamp,
};
use crate::jar::classpath_string;
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
/// `classes_dir`        — directory containing already-compiled production classes.
/// `dep_jars`           — resolved production dependency JARs.
/// `resources_dir`      — production resources dir if it exists (`src/main/resources`
///                        or top-level `resources/`), otherwise `None`.
/// `test_resources_dir` — test resources dir if it exists (`src/test/resources` or
///                        top-level `test-resources/`), otherwise `None`.
/// `filter`             — optional class-name pattern passed to `--include-classname`.
///
/// Returns `Ok(())` when all tests pass (or when no test sources exist).
/// Returns `Err` when compilation fails or any test fails.
pub fn run_tests(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    classes_dir: &Path,
    dep_jars: &[PathBuf],
    resources_dir: Option<&Path>,
    test_resources_dir: Option<&Path>,
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
    // Build effective BOM list for test resolution:
    //   [bom-imports] (lower priority) + [test-bom-imports] (higher priority).
    // Later entries in the list win, so prod BOMs come first.
    let test_bom_gavs: Vec<curie_deps::Gav> = {
        let mut v: Vec<curie_deps::Gav> = desc
            .bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in [bom-imports]")?;
        let test_only: Vec<curie_deps::Gav> = desc
            .test_bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in [test-bom-imports]")?;
        v.extend(test_only);
        v
    };

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
                bom_imports: test_bom_gavs,
            },
        )
        .context("test dependency resolution failed")?
    };

    // --- compile test sources (incremental) ----------------------------------
    let test_classes_dir = project_root.join("target").join("test-classes");
    std::fs::create_dir_all(&test_classes_dir)
        .context("failed to create target/test-classes")?;

    let toml_path = project_root.join("curie.toml");

    // Remove stale test class files before checking whether recompilation is
    // needed.  Test sources may come from multiple roots; collect all
    // (root, sources_in_that_root) pairs for remove_stale_classes.
    let main_src = project_root.join("src").join("main").join("java");
    let test_src = project_root.join("src").join("test").join("java");
    let flat_src_dirs = flat_package_src_dirs(project_root);
    let flat_test_dirs = flat_package_test_dirs(project_root);

    // Collect sources belonging to each root.
    let mut root_source_pairs: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();

    for root in std::iter::once(&main_src)
        .chain(std::iter::once(&test_src))
        .chain(flat_src_dirs.iter())
        .chain(flat_test_dirs.iter())
    {
        let belonging: Vec<PathBuf> = test_sources
            .iter()
            .filter(|p| p.starts_with(root))
            .cloned()
            .collect();
        if !belonging.is_empty() || root.exists() {
            root_source_pairs.push((root.clone(), belonging));
        }
    }

    let pairs_ref: Vec<(&Path, &[PathBuf])> = root_source_pairs
        .iter()
        .map(|(r, s)| (r.as_path(), s.as_slice()))
        .collect();

    let stale_removed = remove_stale_classes(&pairs_ref, &test_classes_dir)?;

    let needs_recompile = stale_removed > 0
        || needs_test_recompile(&test_sources, &test_classes_dir, &toml_path, &project_root.join("target"));

    if needs_recompile {
        let reason = if stale_removed > 0 { "  [stale classes removed]" } else { "" };
        println!(
            "  Compile tests   {} source file(s){}",
            test_sources.len(),
            reason,
        );

        // Classpath for compiling tests:
        //   production classes + src/main/resources + prod deps + test deps + standalone launcher
        let mut compile_cp: Vec<PathBuf> = Vec::new();
        compile_cp.push(classes_dir.to_path_buf());
        if let Some(rd) = resources_dir {
            compile_cp.push(rd.to_path_buf());
        }
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
            .arg(classpath_string(&compile_cp));

        for src in &test_sources {
            javac.arg(src);
        }

        let status = javac
            .status()
            .context("failed to invoke javac — is a JDK installed?")?;

        if !status.success() {
            bail!("test compilation failed");
        }

        // Record the JDK version used so that a future upgrade triggers a rebuild.
        if let Ok(version) = javac_version() {
            write_javac_version_stamp(&project_root.join("target"), &version)?;
        }
    } else {
        println!("  Compile tests   up to date");
    }

    // --- skip if stamp is newer than all inputs ------------------------------
    // When no filter is active, check whether the test-stamp is newer than
    // every input that could invalidate results.  A filter run always executes
    // (it is a partial run and must not mark the full suite as passing).
    let stamp_path = project_root.join("target").join(".test-stamp");

    if filter.is_none() && !needs_test_run(&test_sources, classes_dir, &toml_path, &stamp_path, resources_dir, test_resources_dir) {
        println!("  Tests           up to date");
        return Ok(());
    }

    // --- run tests -----------------------------------------------------------
    // Classpath for running tests:
    //   test classes + production classes + src/main/resources + src/test/resources
    //   + prod deps + test deps
    // (standalone is provided as -jar, not on -cp)
    let mut run_cp: Vec<PathBuf> = Vec::new();
    run_cp.push(test_classes_dir.clone());
    run_cp.push(classes_dir.to_path_buf());
    if let Some(rd) = resources_dir {
        run_cp.push(rd.to_path_buf());
    }
    if let Some(trd) = test_resources_dir {
        run_cp.push(trd.to_path_buf());
    }
    run_cp.extend_from_slice(dep_jars);
    run_cp.extend_from_slice(&test_dep_jars);

    println!();

    let mut java = Command::new("java");
    java.arg("-jar")
        .arg(&standalone_jar)
        .arg("execute")
        .arg("-cp")
        .arg(classpath_string(&run_cp))
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

/// Collect test source files from all supported layout roots.
///
/// **Existing Maven-style layouts (unchanged):**
/// - Co-located: `src/main/java` — files ending in `Test.java`, `Tests.java`,
///   or `Spec.java`.
/// - Separate tree: `src/test/java` — all `*.java` files.
///
/// **New flat-package layouts:**
/// - Co-located unit tests: each dot-named directory under `src/` — files ending
///   in `Test.java`, `Tests.java`, or `Spec.java`.
/// - Integration tests: each dot-named directory under `tests/` — all `*.java`
///   files.
///
/// Results are merged, deduplicated by canonical path, and sorted
/// lexicographically for a deterministic `javac` invocation.
fn discover_test_sources(project_root: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();

    // --- Maven-style: co-located tests in src/main/java ----------------------
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

    // --- Maven-style: separate test tree src/test/java -----------------------
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

    // --- Flat-package: co-located unit tests in src/<dot.pkg>/ ---------------
    for pkg_dir in flat_package_src_dirs(project_root) {
        let colocated: Vec<PathBuf> = WalkDir::new(&pkg_dir)
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

    // --- Flat-package: integration tests in tests/<dot.pkg>/ -----------------
    for pkg_dir in flat_package_test_dirs(project_root) {
        let integration: Vec<PathBuf> = WalkDir::new(&pkg_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".java"))
            .map(|e| e.into_path())
            .collect();
        sources.extend(integration);
    }

    // Deduplicate by canonical path (in case a path appears via multiple roots)
    // and sort for determinism.
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
            bom_imports: vec![],
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

/// Returns true when tests need to be executed.
///
/// Inputs that invalidate the stamp:
///   - test sources
///   - curie.toml
///   - any file under `target/classes` (production recompile happened)
///   - any file under `src/main/resources` or `src/test/resources`
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
    resources_dir: Option<&Path>,
    test_resources_dir: Option<&Path>,
) -> bool {
    let mut inputs = Inputs::new();
    inputs
        .add_paths(test_sources)
        .add_file(toml_path)
        .add_dir(classes_dir)
        .add_dir_opt(resources_dir)
        .add_dir_opt(test_resources_dir);
    !Stamp::of(stamp_path).covers(&inputs)
}

/// Returns true when test sources need to be recompiled.
///
/// Uses `>=` so a same-second edit (on second-resolution filesystems)
/// counts as out-of-date.  See the tie-breaking note in `incremental.rs`.
fn needs_test_recompile(
    test_sources: &[PathBuf],
    test_classes_dir: &Path,
    toml_path: &Path,
    target_dir: &Path,
) -> bool {
    let oldest_class = oldest_mtime_in_dir(test_classes_dir);
    if oldest_class == SystemTime::UNIX_EPOCH {
        return true;
    }
    // Check JDK fingerprint — a JDK upgrade should always trigger a recompile.
    if let Ok(current) = javac_version() {
        let stamp = javac_version_stamp_path(target_dir);
        let stored = std::fs::read_to_string(&stamp).unwrap_or_default();
        if stored.trim() != current.trim() {
            return true;
        }
    }
    if newest_mtime(test_sources) >= oldest_class {
        return true;
    }
    if mtime(toml_path) >= oldest_class {
        return true;
    }
    false
}

