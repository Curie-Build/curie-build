use crate::compile::{flat_package_src_dirs, flat_package_test_dirs};
use crate::incremental::{
    javac_version, needs_recompile, walk_files, write_javac_version_stamp, Inputs, Stamp,
};
use crate::jar::classpath_string;
use crate::{build, descriptor};
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
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
///
/// The argument count exceeds clippy's default cap (7) because the test
/// pipeline genuinely needs each piece independently — all of them flow
/// in from a `CompileOutput` plus the user's CLI flags, and bundling them
/// into an intermediate struct adds plumbing without making the API
/// clearer.  Revisit if the list grows further.
#[allow(clippy::too_many_arguments)]
pub fn run_tests(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    classes_dir: &Path,
    dep_jars: &[PathBuf],
    resources_dir: Option<&Path>,
    test_resources_dir: Option<&Path>,
    filter: Option<&str>,
    offline: bool,
    extra_cp: &[PathBuf],
) -> Result<()> {
    // --- discover test sources -----------------------------------------------
    let test_sources = discover_test_sources(project_root);

    if test_sources.is_empty() {
        println!("  Tests           no test sources found");
        return Ok(());
    }

    // --- resolve JUnit standalone launcher -----------------------------------
    let extra_repos = build::extra_repos(desc);

    let standalone_jar = resolve_standalone(&extra_repos, offline)
        .context("failed to resolve JUnit Platform Console Standalone")?;

    // --- resolve test-scoped dependencies ------------------------------------
    // Build effective BOM list for test resolution:
    //   [bom-imports] (lower priority) + [test-bom-imports] (higher priority).
    // Later entries in the list win, so prod BOMs come first.
    let test_bom_gavs = desc.test_bom_gavs()?;

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
                progress: true,
                bom_imports: test_bom_gavs.clone(),
                offline,
            },
        )
        .context("test dependency resolution failed")?
    };

    // --- resolve test-annotation-processor jars ----------------------------
    // Test compile sees BOTH production processors (so Lombok applied to
    // production code is also applied to test code referencing the same
    // annotations) AND test-only processors.
    let mut test_ap_coords: Vec<(&str, &str)> = desc.ap_pairs();
    test_ap_coords.extend(desc.test_ap_pairs());
    let (test_ap_jars, test_ap_on_cp_jars) = if test_ap_coords.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let jars = resolve(
            &test_ap_coords,
            &ResolveOptions {
                extra_repos: extra_repos.clone(),
                progress: true,
                bom_imports: test_bom_gavs.clone(),
                offline,
            },
        )
        .context("test annotation-processor resolution failed")?;

        // Resolve each on-compile-classpath coord individually for its
        // transitive closure (second resolve hits ~/.m2; cheap).
        let on_cp_coords = desc.test_ap_on_compile_classpath_coords();
        let mut on_cp_jars: Vec<PathBuf> = Vec::new();
        for coord in on_cp_coords {
            let version = test_ap_coords
                .iter()
                .find(|(k, _)| *k == coord)
                .map(|(_, v)| *v)
                .expect("on-cp coord must be in test_ap_coords");
            let single = resolve(
                &[(coord, version)],
                &ResolveOptions {
                    extra_repos: extra_repos.clone(),
                    progress: false,
                    bom_imports: test_bom_gavs.clone(),
                    offline,
                },
            )
            .with_context(|| {
                format!("test annotation-processor classpath resolution failed for {}", coord)
            })?;
            on_cp_jars.extend(single);
        }
        (jars, on_cp_jars)
    };

    // --- compile test sources (incremental) ----------------------------------
    let test_classes_dir = project_root.join("target").join("test-classes");
    std::fs::create_dir_all(&test_classes_dir)
        .context("failed to create target/test-classes")?;

    let toml_path = project_root.join("Curie.toml");
    let test_manifest_path = project_root.join("target").join(".test-classes.toml");

    // Pre-compile prune (same scheme as production compile).  The
    // manifest is root-agnostic so the source-roots gymnastics the old
    // heuristic needed are gone.
    let old_test_manifest = crate::class_manifest::load(&test_manifest_path)?;
    let current_test_sources_set: std::collections::HashSet<String> = test_sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    // AP-generated test sources live under `target/generated-test-sources/`;
    // the whole `target/` tree is a fine over-approximation to give the
    // pre-prune a path-prefix carve-out (post-compile catches the real
    // "generator stopped producing this" case).
    let canonical_test_target = project_root
        .join("target")
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    let pre_pruned_tests: usize = match &old_test_manifest {
        Some(old) => {
            let stale = crate::class_manifest::stale_classes(
                old,
                None,
                &current_test_sources_set,
                canonical_test_target.as_deref(),
            );
            crate::class_manifest::delete_classes(&test_classes_dir, &stale)?
        }
        None => 0,
    };

    let needs_recompile_tests = pre_pruned_tests > 0
        || needs_recompile(&test_sources, &test_classes_dir, &toml_path, &project_root.join("target")).needs_recompile();

    if needs_recompile_tests {
        let reason = if pre_pruned_tests > 0 { "  [stale classes removed]" } else { "" };
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
        compile_cp.extend_from_slice(extra_cp);
        compile_cp.extend_from_slice(&test_ap_on_cp_jars);
        compile_cp.push(standalone_jar.clone());

        // Invoke the embedded javac wrapper so we capture the source →
        // class mapping in `.test-classes.toml`.
        let wrapper_jar = crate::wrapper::ensure()?;
        let mut javac = Command::new("java");
        javac.arg("-jar").arg(&wrapper_jar);
        javac.arg("--curie-manifest-out").arg(&test_manifest_path);
        javac
            .arg("--release")
            .arg(desc.java.effective())
            .arg("-g")
            .arg("-d")
            .arg(&test_classes_dir)
            .arg("-cp")
            .arg(classpath_string(&compile_cp));

        // Annotation-processor path + generated-test-sources directory.
        if !test_ap_jars.is_empty() {
            let gen_dir = project_root
                .join("target")
                .join("generated-test-sources")
                .join("annotations");
            std::fs::create_dir_all(&gen_dir).with_context(|| {
                format!("failed to create {}", gen_dir.display())
            })?;
            javac.arg("-processorpath").arg(classpath_string(&test_ap_jars));
            javac.arg("-s").arg(&gen_dir);
        }
        for (key, value) in desc.flat_test_ap_options() {
            javac.arg(format!("-A{}={}", key, value));
        }

        for src in &test_sources {
            javac.arg(src);
        }

        let status = javac
            .status()
            .context("failed to invoke java — is a JRE installed?")?;

        if !status.success() {
            bail!("test compilation failed");
        }

        // Post-compile prune: companion test classes removed from a still-
        // existing source (e.g. dropping an `@Nested` inner class) leave
        // orphan .class files in target/test-classes/.  Diff old vs new.
        if let Some(old) = &old_test_manifest {
            if let Some(new) = crate::class_manifest::load(&test_manifest_path)? {
                let stale = crate::class_manifest::stale_classes(
                    old, Some(&new), &current_test_sources_set, None,
                );
                let n = crate::class_manifest::delete_classes(&test_classes_dir, &stale)?;
                if n > 0 {
                    println!(
                        "  Stale tests     removed {} orphaned class file{}",
                        n,
                        if n == 1 { "" } else { "s" },
                    );
                }
            }
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
    run_cp.extend_from_slice(extra_cp);

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
        let colocated: Vec<PathBuf> = walk_files(&main_src)
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
        let separate: Vec<PathBuf> = walk_files(&test_src)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".java"))
            .map(|e| e.into_path())
            .collect();
        sources.extend(separate);
    }

    // --- Flat-package: co-located unit tests in src/<dot.pkg>/ ---------------
    for pkg_dir in flat_package_src_dirs(project_root) {
        let colocated: Vec<PathBuf> = walk_files(&pkg_dir)
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
        let integration: Vec<PathBuf> = walk_files(&pkg_dir)
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

fn resolve_standalone(extra_repos: &[curie_deps::repo::Repository], offline: bool) -> Result<PathBuf> {
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
            progress: false,
            bom_imports: vec![],
            offline,
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
// Incremental run check
// ---------------------------------------------------------------------------

/// Returns true when tests need to be executed.
///
/// Inputs that invalidate the stamp:
///   - test sources
///   - Curie.toml
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

