use crate::compile::{
    flat_package_src_dirs, flat_package_test_dirs, KOTLIN_COMPILER_COORD, KOTLIN_STDLIB_COORD,
};
use crate::incremental::{
    javac_version, needs_recompile, walk_files, write_javac_version_stamp, Inputs, Stamp,
};
use crate::jar::classpath_string;
use crate::{build, descriptor};
use crate::build::central_repos;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, DepEntry, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
const JUNIT_STANDALONE_COORD: &str =
    "org.junit.platform:junit-platform-console-standalone";

/// Compile test sources and run all tests via the JUnit Platform Console
/// Standalone launcher.
///
/// `classes_dir`        — directory containing already-compiled production classes.
/// `dep_jars`           — resolved production dependency JARs.
/// `kotlin_stdlib_jars` — Kotlin stdlib JARs (empty for Java-only projects).
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
    kotlin_stdlib_jars: &[PathBuf],
    groovy_jars: &[PathBuf],
    resources_dir: Option<&Path>,
    test_resources_dir: Option<&Path>,
    filter: Option<&str>,
    offline: bool,
    extra_cp: &[PathBuf],
) -> Result<()> {
    // --- discover test sources -----------------------------------------------
    let (java_test_sources, kotlin_test_sources) = discover_test_sources(project_root);
    let groovy_test_sources = discover_groovy_test_sources(project_root);

    let all_test_sources: Vec<PathBuf> = {
        let mut v = java_test_sources.clone();
        v.extend(kotlin_test_sources.iter().cloned());
        v.extend(groovy_test_sources.iter().cloned());
        v.sort();
        v.dedup();
        v
    };

    if all_test_sources.is_empty() {
        println!("  Tests           no test sources found");
        return Ok(());
    }

    let has_kotlin_tests = !kotlin_test_sources.is_empty();
    let has_java_tests   = !java_test_sources.is_empty();
    let has_groovy_tests = !groovy_test_sources.is_empty();

    // --- resolve JUnit standalone launcher -----------------------------------
    let extra_repos = build::extra_repos(desc);

    // spock-core 2.x is built against JUnit Platform 1.x.  The default
    // standalone runner is JUnit 6 (Platform 2.x) which has an incompatible
    // `--scan-class-path` discovery protocol.  When Spock is enabled and the
    // user hasn't overridden the version, use the latest 1.x standalone.
    let junit_version = if desc.spock.enabled && !desc.test.junit_platform_version_is_user_set() {
        "1.14.4"
    } else {
        desc.test.junit_platform_version()
    };
    let standalone_jar = resolve_standalone(&extra_repos, offline, junit_version)
        .context("failed to resolve JUnit Platform Console Standalone")?;

    // --- resolve test-scoped dependencies ------------------------------------
    // Build effective BOM list for test resolution:
    //   [bom-imports] (lower priority) + [test-bom-imports] (higher priority).
    // Later entries in the list win, so prod BOMs come first.
    let test_bom_gavs = desc.test_bom_gavs()?;

    let test_dep_jars = if desc.test_dependencies.is_empty() {
        vec![]
    } else {
        let pairs: Vec<DepEntry> = desc
            .test_dependencies
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository() })
            .collect();

        resolve(
            &pairs,
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos.clone(),
                progress: true,
                bom_imports: test_bom_gavs.clone(),
                offline,
            },
        )
        .context("test dependency resolution failed")?
    };

    // --- resolve spock-core (when [spock] is configured) ---------------------
    // spock-core's transitive deps (junit-platform-engine, opentest4j, …)
    // carry no explicit version — they are managed by the embedded junit-bom
    // which spock-bom imports.  We pass spock-bom as an extra bom_import so
    // those managed versions land in global_managed and the resolver can
    // resolve the full transitive closure.
    let spock_jars: Vec<PathBuf> = if desc.spock.enabled {
        let spock_version = desc.spock.version();
        let spock_bom = curie_deps::Gav::from_key_version(
            "org.spockframework:spock-bom",
            spock_version,
        )?;
        let mut spock_bom_imports = test_bom_gavs.clone();
        spock_bom_imports.push(spock_bom);

        let jars = resolve(
            &[DepEntry {
                key: "org.spockframework:spock-core",
                version: spock_version,
                repo_id: None,
            }],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos.clone(),
                progress: true,
                bom_imports: spock_bom_imports,
                offline,
            },
        )
        .context("Spock resolution failed")?;
        println!("  Resolve Spock   {} JAR(s)", jars.len());
        jars
    } else {
        vec![]
    };

    // --- resolve Kotlin compiler for test compilation (when needed) ----------
    let test_kotlin_stdlib_jars: Vec<PathBuf>;
    let test_kotlin_compiler_jars: Vec<PathBuf>; // all resolved JARs for -cp invocation

    if has_kotlin_tests && kotlin_stdlib_jars.is_empty() {
        // Production had no Kotlin sources but tests do — resolve now.
        let kver = desc.kotlin.version();
        let kotlin_jars = resolve(
            &[
                DepEntry { key: KOTLIN_COMPILER_COORD, version: kver, repo_id: None },
                DepEntry { key: KOTLIN_STDLIB_COORD, version: kver, repo_id: None },
            ],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos.clone(),
                progress: true,
                bom_imports: test_bom_gavs.clone(),
                offline,
            },
        )
        .context("Kotlin compiler/stdlib resolution failed (test phase)")?;

        let stdlib: Vec<PathBuf> = kotlin_jars
            .iter()
            .filter(|p| {
                p.file_name()
                    .map(|f| !f.to_string_lossy().starts_with("kotlin-compiler-embeddable"))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        test_kotlin_compiler_jars = kotlin_jars;
        test_kotlin_stdlib_jars = stdlib;
    } else if has_kotlin_tests {
        // Re-resolve compiler (all transitive deps) — stdlib already in kotlin_stdlib_jars.
        let kver = desc.kotlin.version();
        let kotlin_jars = resolve(
            &[
                DepEntry { key: KOTLIN_COMPILER_COORD, version: kver, repo_id: None },
                DepEntry { key: KOTLIN_STDLIB_COORD, version: kver, repo_id: None },
            ],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos.clone(),
                progress: false,
                bom_imports: test_bom_gavs.clone(),
                offline,
            },
        )
        .context("Kotlin compiler resolution failed (test phase)")?;

        test_kotlin_compiler_jars = kotlin_jars;
        // Stdlib was already resolved in the prod compile phase.
        test_kotlin_stdlib_jars = kotlin_stdlib_jars.to_vec();
    } else {
        test_kotlin_compiler_jars = Vec::new();
        test_kotlin_stdlib_jars = kotlin_stdlib_jars.to_vec();
    }

    // --- resolve test-annotation-processor jars ----------------------------
    // Test compile sees BOTH production processors (so Lombok applied to
    // production code is also applied to test code referencing the same
    // annotations) AND test-only processors.
    let mut test_ap_coords: Vec<(&str, &str)> = desc.ap_pairs();
    test_ap_coords.extend(desc.test_ap_pairs());
    let (test_ap_jars, test_ap_on_cp_jars) = if test_ap_coords.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let ap_entries: Vec<DepEntry> = test_ap_coords
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v, repo_id: None })
            .collect();
        let jars = resolve(
            &ap_entries,
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos.clone(),
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
                &[DepEntry { key: coord, version, repo_id: None }],
                &ResolveOptions {
                    default_repos: central_repos(),
                    named_repos: extra_repos.clone(),
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

    // Pre-compile prune (same scheme as production compile).
    let old_test_manifest = crate::class_manifest::load(&test_manifest_path)?;
    let current_test_sources_set: std::collections::HashSet<String> = all_test_sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
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
        || needs_recompile(&all_test_sources, &test_classes_dir, &toml_path, &project_root.join("target")).needs_recompile();

    if needs_recompile_tests {
        let reason = if pre_pruned_tests > 0 { "  [stale classes removed]" } else { "" };
        println!(
            "  Compile tests   {} source file(s){}",
            all_test_sources.len(),
            reason,
        );

        // Shared classpath for both phases:
        //   production classes + src/main/resources + prod deps + test deps
        //   + standalone launcher + kotlin stdlib + extras
        let mut shared_cp: Vec<PathBuf> = Vec::new();
        shared_cp.push(classes_dir.to_path_buf());
        if let Some(rd) = resources_dir {
            shared_cp.push(rd.to_path_buf());
        }
        shared_cp.extend_from_slice(dep_jars);
        shared_cp.extend_from_slice(&test_dep_jars);
        shared_cp.extend_from_slice(extra_cp);
        shared_cp.extend_from_slice(&test_ap_on_cp_jars);
        shared_cp.extend_from_slice(&test_kotlin_stdlib_jars);
        shared_cp.extend_from_slice(groovy_jars);
        shared_cp.extend_from_slice(&spock_jars);
        shared_cp.push(standalone_jar.clone());

        if has_kotlin_tests {
            // Phase 1: kotlinc — compile all .kt + .java test sources together.
            let mut kotlinc = Command::new("java");
            kotlinc.arg("--enable-native-access=ALL-UNNAMED");
            kotlinc.arg("-cp").arg(classpath_string(&test_kotlin_compiler_jars));
            kotlinc.arg("org.jetbrains.kotlin.cli.jvm.K2JVMCompiler");
            kotlinc.arg("-no-stdlib").arg("-no-reflect");
            kotlinc.arg("-d").arg(&test_classes_dir);

            if !shared_cp.is_empty() {
                kotlinc.arg("-cp").arg(classpath_string(&shared_cp));
            }

            for src in &kotlin_test_sources {
                kotlinc.arg(src);
            }
            for src in &java_test_sources {
                kotlinc.arg(src);
            }

            let status = kotlinc
                .status()
                .context("failed to invoke kotlinc for test compilation")?;

            if !status.success() {
                bail!("Kotlin test compilation failed");
            }
        }

        if has_java_tests {
            // Phase 2: javac — re-compile Java test sources.
            let wrapper_jar = crate::wrapper::ensure()?;
            let mut javac = Command::new("java");
            javac.arg("-jar").arg(&wrapper_jar);
            javac.arg("--curie-manifest-out").arg(&test_manifest_path);
            javac
                .arg("--release")
                .arg(desc.java.effective())
                .arg("-g")
                .arg("-d")
                .arg(&test_classes_dir);

            // Classpath for compiling Java tests: test-classes (kotlin bytecode
            // from phase 1) + shared_cp.
            let mut compile_cp: Vec<PathBuf> = Vec::new();
            if has_kotlin_tests {
                compile_cp.push(test_classes_dir.clone());
            }
            compile_cp.extend_from_slice(&shared_cp);
            javac.arg("-cp").arg(classpath_string(&compile_cp));

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

            for src in &java_test_sources {
                javac.arg(src);
            }

            let status = javac
                .status()
                .context("failed to invoke java — is a JRE installed?")?;

            if !status.success() {
                bail!("test compilation failed");
            }

            // Post-compile prune for Java test classes.
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
        }

        if has_groovy_tests {
            // groovyc — compile all .groovy test sources.
            // When Java test sources are also present, use --jointCompilation.
            let groovy_cp_jars: Vec<PathBuf> = {
                if groovy_jars.is_empty() {
                    // Groovy tests exist but production had no groovy sources
                    // → resolve the Groovy runtime now.
                    use crate::compile::GROOVY_COORD;
                    resolve(
                        &[DepEntry { key: GROOVY_COORD, version: desc.groovy.version(), repo_id: None }],
                        &ResolveOptions {
                            default_repos: central_repos(),
                            named_repos: extra_repos.clone(),
                            progress: true,
                            bom_imports: test_bom_gavs.clone(),
                            offline,
                        },
                    )
                    .context("Groovy test compiler resolution failed")?
                } else {
                    groovy_jars.to_vec()
                }
            };
            // Spock AST transforms are loaded by the groovyc JVM process
            // itself, so spock-core and ALL its transitive deps (incl. opentest4j,
            // junit-platform-*) must be on the process -cp alongside groovy.jar.
            // shared_cp already contains all of these — use it as the process cp
            // (excluding the standalone launcher jar which is not a compile dep).
            let groovyc_process_cp: Vec<PathBuf> = {
                let mut cp = groovy_cp_jars.clone();
                cp.extend(shared_cp.iter().filter(|p| {
                    !p.file_name()
                        .map(|f| f.to_string_lossy().starts_with("junit-platform-console-standalone"))
                        .unwrap_or(false)
                }).cloned());
                cp
            };

            let mut groovyc = Command::new("java");
            groovyc.arg("-cp").arg(classpath_string(&groovyc_process_cp));
            groovyc.arg("org.codehaus.groovy.tools.FileSystemCompiler");
            groovyc.arg("-d").arg(&test_classes_dir);
            let mut gcp = shared_cp.clone();
            if !groovy_cp_jars.is_empty() {
                gcp.extend_from_slice(&groovy_cp_jars);
            }
            if !gcp.is_empty() {
                groovyc.arg("--classpath").arg(classpath_string(&gcp));
            }
            if has_java_tests {
                groovyc.arg("--jointCompilation");
                for src in &java_test_sources { groovyc.arg(src); }
            }
            for src in &groovy_test_sources { groovyc.arg(src); }
            let status = groovyc
                .status()
                .context("failed to invoke groovyc for test compilation")?;
            if !status.success() {
                bail!("Groovy test compilation failed");
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
    let stamp_path = project_root.join("target").join(".test-stamp");

    if filter.is_none() && !needs_test_run(&all_test_sources, classes_dir, &toml_path, &stamp_path, resources_dir, test_resources_dir) {
        println!("  Tests           up to date");
        return Ok(());
    }

    // --- run tests -----------------------------------------------------------
    // Classpath for running tests:
    //   test classes + production classes + src/main/resources + src/test/resources
    //   + prod deps + test deps + kotlin stdlib
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
    run_cp.extend_from_slice(&test_kotlin_stdlib_jars);
    run_cp.extend_from_slice(groovy_jars);
    run_cp.extend_from_slice(&spock_jars);

    println!();

    let mut java = Command::new("java");
    java.arg("-jar")
        .arg(&standalone_jar)
        .arg("execute")
        .arg("-cp")
        .arg(classpath_string(&run_cp))
        .arg("--scan-class-path");

    // JUnit Platform's default class-name filter matches *Test/*Tests and
    // similar Java/Kotlin conventions but skips Groovy *Spec classes.  When
    // Spock is enabled, broaden the filter to include `.*Spec` names.
    if let Some(f) = filter {
        java.arg(format!("--include-classname={}", f));
    } else if !spock_jars.is_empty() {
        // Match the Spock *Spec convention alongside JUnit's default patterns.
        // The default JUnit pattern already covers *Tests / *Test / Test* etc.;
        // we add .*Spec so Groovy specification classes are included.
        java.arg("--include-classname=.*Tests?$|^Test.*|.*TestCase$|.*Spec$");
    }

    let status = java
        .status()
        .context("failed to invoke java — is a JRE installed?")?;

    println!();

    if !status.success() {
        bail!("tests failed");
    }

    // --- write stamp on success ----------------------------------------------
    std::fs::write(&stamp_path, b"")
        .with_context(|| format!("failed to write test stamp {}", stamp_path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------

/// Collect test source files from all supported layout roots.
///
/// Returns `(java_sources, kotlin_sources)` — each sorted and deduplicated.
///
/// **Existing Maven-style layouts (unchanged):**
/// - Co-located: `src/main/java` — files ending in `Test.java`, `Tests.java`,
///   or `Spec.java`.
/// - Separate tree: `src/test/java` — all `*.java` files.
///
/// **Kotlin Maven-style layouts:**
/// - Co-located: `src/main/kotlin` — files ending in `Test.kt`, `Tests.kt`,
///   or `Spec.kt`.
/// - Separate tree: `src/test/kotlin` — all `*.kt` files.
///
/// **New flat-package layouts:**
/// - Co-located unit tests: each dot-named directory under `src/` — files ending
///   in `Test.java/kt`, `Tests.java/kt`, or `Spec.java/kt`.
/// - Integration tests: each dot-named directory under `tests/` — all `*.java`
///   and `*.kt` files.
fn discover_test_sources(project_root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut java_sources: Vec<PathBuf> = Vec::new();
    let mut kotlin_sources: Vec<PathBuf> = Vec::new();

    // --- Maven-style Java: co-located tests in src/main/java ----------------
    let main_java_src = project_root.join("src").join("main").join("java");
    if main_java_src.exists() {
        let colocated: Vec<PathBuf> = walk_files(&main_java_src)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.java")
                    || name.ends_with("Tests.java")
                    || name.ends_with("Spec.java")
            })
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(colocated);
    }

    // --- Maven-style Java: separate test tree src/test/java -----------------
    let test_java_src = project_root.join("src").join("test").join("java");
    if test_java_src.exists() {
        let separate: Vec<PathBuf> = walk_files(&test_java_src)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".java"))
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(separate);
    }

    // --- Maven-style Kotlin: co-located tests in src/main/kotlin ------------
    let main_kotlin_src = project_root.join("src").join("main").join("kotlin");
    if main_kotlin_src.exists() {
        let colocated: Vec<PathBuf> = walk_files(&main_kotlin_src)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.kt")
                    || name.ends_with("Tests.kt")
                    || name.ends_with("Spec.kt")
            })
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(colocated);
    }

    // --- Maven-style Kotlin: separate test tree src/test/kotlin -------------
    let test_kotlin_src = project_root.join("src").join("test").join("kotlin");
    if test_kotlin_src.exists() {
        let separate: Vec<PathBuf> = walk_files(&test_kotlin_src)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".kt"))
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(separate);
    }

    // --- Flat-package: co-located unit tests in src/<dot.pkg>/ --------------
    for pkg_dir in flat_package_src_dirs(project_root) {
        let colocated_java: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.java")
                    || name.ends_with("Tests.java")
                    || name.ends_with("Spec.java")
            })
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(colocated_java);

        let colocated_kotlin: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.kt")
                    || name.ends_with("Tests.kt")
                    || name.ends_with("Spec.kt")
            })
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(colocated_kotlin);
    }

    // --- Flat-package: integration tests in tests/<dot.pkg>/ ----------------
    for pkg_dir in flat_package_test_dirs(project_root) {
        let java_int: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".java"))
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(java_int);

        let kotlin_int: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".kt"))
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(kotlin_int);
    }

    // Deduplicate by canonical path and sort for determinism.
    java_sources.sort();
    java_sources.dedup();
    kotlin_sources.sort();
    kotlin_sources.dedup();

    (java_sources, kotlin_sources)
}

/// Discover Groovy test sources — mirrors [`discover_test_sources`] for `.groovy` files.
fn discover_groovy_test_sources(project_root: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();

    // Co-located tests in src/main/groovy/
    let main_groovy = project_root.join("src").join("main").join("groovy");
    if main_groovy.exists() {
        let colocated: Vec<PathBuf> = walk_files(&main_groovy)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.groovy")
                    || name.ends_with("Tests.groovy")
                    || name.ends_with("Spec.groovy")
            })
            .map(|e| e.into_path())
            .collect();
        sources.extend(colocated);
    }

    // Separate test tree src/test/groovy/
    let test_groovy = project_root.join("src").join("test").join("groovy");
    if test_groovy.exists() {
        let all: Vec<PathBuf> = walk_files(&test_groovy)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".groovy"))
            .map(|e| e.into_path())
            .collect();
        sources.extend(all);
    }

    // Flat-package: co-located tests in src/<dot.pkg>/
    for pkg_dir in flat_package_src_dirs(project_root) {
        let colocated: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with("Test.groovy")
                    || name.ends_with("Tests.groovy")
                    || name.ends_with("Spec.groovy")
            })
            .map(|e| e.into_path())
            .collect();
        sources.extend(colocated);
    }

    // Flat-package: integration tests in tests/<dot.pkg>/
    for pkg_dir in flat_package_test_dirs(project_root) {
        let all: Vec<PathBuf> = walk_files(&pkg_dir)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".groovy"))
            .map(|e| e.into_path())
            .collect();
        sources.extend(all);
    }

    sources.sort();
    sources.dedup();
    sources
}

// ---------------------------------------------------------------------------
// JUnit standalone resolution
// ---------------------------------------------------------------------------

fn resolve_standalone(
    extra_repos: &[curie_deps::repo::Repository],
    offline: bool,
    junit_version: &str,
) -> Result<PathBuf> {
    let coord = format!("{}:{}", JUNIT_STANDALONE_COORD, junit_version);
    // coord is "group:artifact:version" — split off the version for the resolver.
    // The resolver takes (key, version) pairs where key = "group:artifact".
    let jars = resolve(
        &[DepEntry { key: JUNIT_STANDALONE_COORD, version: junit_version, repo_id: None }],
        &ResolveOptions {
            default_repos: central_repos(),
            named_repos: extra_repos.to_vec(),
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
                junit_version
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

