//! Production source discovery and compilation.
//!
//! Supports two source layouts side-by-side:
//!   - Maven-style: `src/main/java/com/foo/Bar.java`
//!   - Maven-style Kotlin: `src/main/kotlin/com/foo/Bar.kt`
//!   - Flat-package: any dot-named sibling under `src/`, e.g.
//!     `src/com.example.myapp/Bar.java` (the directory name IS the package).
//!     Kotlin files (`.kt`) in the same flat-package dirs are also collected.
//!
//! All layouts produce the same compiled output under `target/classes/`
//! and may coexist in a single project.
//!
//! ## Mixed Java + Kotlin compilation
//!
//! When `.kt` sources are present, compilation uses a two-phase approach:
//!
//!   1. **Phase 1 (`kotlinc`)**: compile all `.kt` + all `.java` sources
//!      together → `target/classes/`.  The Kotlin compiler resolves Java
//!      types from source so no pre-compiled stubs are needed.
//!
//!   2. **Phase 2 (`javac`)**: re-compile only the `.java` sources with
//!      `target/classes/` on the classpath so Java can see the Kotlin
//!      `.class` files.  This step is skipped when there are no Java sources.
//!
//! For Java-only projects the existing single-phase `javac` path is used
//! unchanged.

use crate::build::{central_repos, extra_repos};
use crate::descriptor;
use crate::incremental::{
    javac_version, needs_recompile, walk_files, write_javac_version_stamp, CompileStatus,
};
use crate::jar::classpath_string;
use crate::kt_stale;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, DepEntry, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Kotlin version used when resolving the compiler and stdlib from Maven Central.
/// This is the *default*; a project (or its enclosing workspace) may override
/// it via the `[kotlin] version` key in Curie.toml.  The value is the same
/// string that `descriptor::DEFAULT_KOTLIN_VERSION` holds.
///
/// The name is re-exported under `#[cfg(test)]` so the existing unit test
/// `kotlin_version_constant_is_set` continues to compile while normal builds
/// do not carry an unused-import warning.
#[cfg(test)]
pub use crate::descriptor::DEFAULT_KOTLIN_VERSION as KOTLIN_VERSION;
pub const KOTLIN_COMPILER_COORD: &str = "org.jetbrains.kotlin:kotlin-compiler-embeddable";
pub const KOTLIN_STDLIB_COORD: &str = "org.jetbrains.kotlin:kotlin-stdlib";
/// Apache Groovy compiler + runtime (transitive deps provide ASM, Antlr, etc.).
pub const GROOVY_COORD: &str = "org.apache.groovy:groovy";

/// Intermediate output from the compile phase.
pub struct CompileOutput {
    pub jar_path: PathBuf,
    pub jar_name: String,
    pub classes_dir: PathBuf,
    /// All active source roots (Maven-style and/or flat-package dirs).
    pub src_roots: Vec<PathBuf>,
    /// All non-test production source files (sorted).
    pub sources: Vec<PathBuf>,
    /// Resolved production dependency JARs (empty when no [dependencies] declared).
    pub dep_jars: Vec<PathBuf>,
    /// Resolved Kotlin stdlib JARs (empty when no `.kt` sources found).
    /// Must be added to every runtime classpath that runs Kotlin code.
    pub kotlin_stdlib_jars: Vec<PathBuf>,
    /// Resolved Groovy runtime JARs (empty when no `.groovy` sources found).
    /// Must be added to every runtime classpath that runs Groovy code.
    pub groovy_jars: Vec<PathBuf>,
    /// Production resources directory (`src/main/resources` or top-level `resources/`), if it exists.
    pub resources_dir: Option<PathBuf>,
    /// Test resources directory (`src/test/resources` or top-level `test-resources/`), if it exists.
    pub test_resources_dir: Option<PathBuf>,
}

/// Returns all immediate subdirectories of `<project_root>/src/` whose names
/// contain a dot — these are flat-package source roots (e.g. `com.example.myapp`).
///
/// Sub-packages are siblings under `src/`, not nested inside their parent package
/// directory.  For example, `src/com.example.myapp/` and
/// `src/com.example.myapp.service/` are both returned.
pub fn flat_package_src_dirs(project_root: &Path) -> Vec<PathBuf> {
    flat_package_dirs_under(&project_root.join("src"))
}

/// Returns all immediate subdirectories of `<project_root>/tests/` whose names
/// contain a dot — these are flat-package integration-test roots.
pub fn flat_package_test_dirs(project_root: &Path) -> Vec<PathBuf> {
    flat_package_dirs_under(&project_root.join("tests"))
}

/// Helper: enumerate dot-named immediate subdirectories of `parent`.
/// Returns an empty Vec when `parent` does not exist.
fn flat_package_dirs_under(parent: &Path) -> Vec<PathBuf> {
    if !parent.exists() {
        return vec![];
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.contains('.') && e.path().is_dir()
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// For a flat-package source root such as `src/com.example.foo`, returns
/// the directory name (`"com.example.foo"`) — which is also the Java package
/// declared by all files inside it.  Returns `""` for Maven-style roots
/// (`src/main/java`, `src/test/java`) whose final component contains no dot.
///
/// This prefix is what `javac` will emit class files under (relative to
/// `target/classes`) and what callers prepend when deriving fully-qualified
/// class names from a source file's path.
pub fn pkg_prefix_for_src_root(src_root: &Path) -> String {
    src_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| n.contains('.'))
        .unwrap_or_default()
}

/// Phase 1: resolve production deps and compile production sources.
/// Does NOT run tests or package a JAR.
///
/// `extra_cp` carries additional classpath entries supplied by the caller
/// (typically workspace-dep JARs + their transitive classpath in
/// workspace builds).  Pass `&[]` for a self-contained single-module build.
pub fn compile(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    offline: bool,
    extra_cp: &[PathBuf],
) -> Result<CompileOutput> {
    // --- source roots --------------------------------------------------------
    // Supported layouts (may coexist):
    //   A) Maven-style Java:   src/main/java/
    //   B) Maven-style Kotlin: src/main/kotlin/
    //   C) Maven-style Groovy: src/main/groovy/
    //   D) Flat-package:       src/com.example.myapp/  (dot-named sibling under src/)
    //      .java, .kt, and .groovy files are all collected from flat-package dirs.
    let maven_java_src   = project_root.join("src").join("main").join("java");
    let maven_kotlin_src = project_root.join("src").join("main").join("kotlin");
    let maven_groovy_src = project_root.join("src").join("main").join("groovy");
    let flat_src_dirs = flat_package_src_dirs(project_root);

    let mut src_roots: Vec<PathBuf> = Vec::new();
    if maven_java_src.exists()   { src_roots.push(maven_java_src.clone()); }
    if maven_kotlin_src.exists() { src_roots.push(maven_kotlin_src.clone()); }
    if maven_groovy_src.exists() { src_roots.push(maven_groovy_src.clone()); }
    src_roots.extend(flat_src_dirs);

    if src_roots.is_empty() {
        bail!(
            "no source directory found: expected src/main/java/, src/main/kotlin/, \
             src/main/groovy/, or at least one dot-named directory under src/ \
             (e.g. src/com.example.myapp/)"
        );
    }

    let classes_dir = project_root.join("target").join("classes");
    let output_dir = project_root.join("target");

    std::fs::create_dir_all(&classes_dir)
        .context("failed to create target/classes")?;

    // --- resolve production dependencies -------------------------------------
    // Parse [bom-imports] into GAVs once — reused for both prod and test.
    let bom_gavs = desc.prod_bom_gavs()?;

    let dep_jars = if desc.dependencies.is_empty() {
        // No deps to resolve. (BOMs without deps is a no-op for this phase.)
        vec![]
    } else {
        let pairs: Vec<DepEntry> = desc
            .dependencies
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository() })
            .collect();

        let jars = resolve(
            &pairs,
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: true,
                bom_imports: bom_gavs.clone(),
                offline,
            },
        )
        .context("dependency resolution failed")?;

        println!("  Resolve deps    {} JAR(s)", jars.len());
        jars
    };

    // --- resolve annotation-processor jars ----------------------------------
    // Same resolver, separate result list — these go on `-processorpath`,
    // not the main `-cp`.  Honour [bom-imports] just like regular deps so
    // processor versions can be BOM-managed.
    let ap_pairs = desc.ap_pairs();
    let (ap_jars, ap_on_compile_classpath_jars) = if ap_pairs.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let ap_entries: Vec<DepEntry> = ap_pairs
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v, repo_id: None })
            .collect();
        let jars = resolve(
            &ap_entries,
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: true,
                bom_imports: bom_gavs.clone(),
                offline,
            },
        )
        .context("annotation-processor resolution failed")?;
        println!("  Resolve APs     {} JAR(s)", jars.len());

        // Each ap_pairs entry yields a transitive closure starting with
        // the entry's own jar (declared deps first, BFS).  Match the
        // on-compile-classpath flag against the entry coords; that flag
        // applies to the leaf coordinate the user declared.  The leaf
        // jar lives at the index of its declaration in `ap_pairs`'s
        // transitive expansion — i.e. the first jar emitted for each
        // declared coord.
        //
        // The resolver gives a flat list; recover declaration boundaries
        // by re-resolving each declared coord individually.  Cheap because
        // results are cached in ~/.m2 after the first call above.
        let on_cp_coords = desc.ap_on_compile_classpath_coords();
        let mut on_cp_jars: Vec<PathBuf> = Vec::new();
        for coord in on_cp_coords {
            // Find the version we just resolved for this coord.
            let version = ap_pairs
                .iter()
                .find(|(k, _)| *k == coord)
                .map(|(_, v)| *v)
                .expect("on-cp coord must be in ap_pairs");
            // Resolve the single coord again — second call hits ~/.m2.
            let single = resolve(
                &[DepEntry { key: coord, version, repo_id: None }],
                &ResolveOptions {
                    default_repos: central_repos(),
                    named_repos: extra_repos(desc),
                    progress: false,
                    bom_imports: bom_gavs.clone(),
                    offline,
                },
            )
            .with_context(|| format!("annotation-processor classpath resolution failed for {}", coord))?;
            // The leaf coord's own JAR is the first entry; the rest are
            // its transitive deps which the processor needs at compile
            // time too (it'd be incomplete without them).
            on_cp_jars.extend(single);
        }
        (jars, on_cp_jars)
    };

    // --- discover production sources (exclude test files) --------------------
    let mut java_sources: Vec<PathBuf>   = Vec::new();
    let mut kotlin_sources: Vec<PathBuf> = Vec::new();
    let mut groovy_sources: Vec<PathBuf> = Vec::new();

    for src_root in &src_roots {
        let root_java: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with(".java")
                    && !name.ends_with("Test.java")
                    && !name.ends_with("Tests.java")
                    && !name.ends_with("Spec.java")
            })
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(root_java);

        let root_kotlin: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with(".kt")
                    && !name.ends_with("Test.kt")
                    && !name.ends_with("Tests.kt")
                    && !name.ends_with("Spec.kt")
            })
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(root_kotlin);

        let root_groovy: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with(".groovy")
                    && !name.ends_with("Test.groovy")
                    && !name.ends_with("Tests.groovy")
                    && !name.ends_with("Spec.groovy")
            })
            .map(|e| e.into_path())
            .collect();
        groovy_sources.extend(root_groovy);
    }

    java_sources.sort();   java_sources.dedup();
    kotlin_sources.sort(); kotlin_sources.dedup();
    groovy_sources.sort(); groovy_sources.dedup();

    let has_kotlin = !kotlin_sources.is_empty();
    let has_java   = !java_sources.is_empty();
    let has_groovy = !groovy_sources.is_empty();

    if has_groovy && has_kotlin {
        bail!(
            "mixing Groovy and Kotlin sources in the same module is not supported; \
             use separate modules for each language"
        );
    }

    // Combined source list for incremental stamp / manifest purposes.
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.extend(java_sources.iter().cloned());
    sources.extend(kotlin_sources.iter().cloned());
    sources.extend(groovy_sources.iter().cloned());
    sources.sort();
    sources.dedup();

    if sources.is_empty() {
        bail!(
            "no Java, Kotlin, or Groovy source files found under {}",
            src_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    // --- resource directories ------------------------------------------------
    // Maven-style: src/main/resources  /  src/test/resources
    // Flat-package style: resources/   /  test-resources/   (top-level)
    // Whichever exists is used; Maven-style takes precedence when both present.
    let resources_dir = {
        let maven = project_root.join("src").join("main").join("resources");
        let flat  = project_root.join("resources");
        if maven.exists() { Some(maven) } else if flat.exists() { Some(flat) } else { None }
    };
    let test_resources_dir = {
        let maven = project_root.join("src").join("test").join("resources");
        let flat  = project_root.join("test-resources");
        if maven.exists() { Some(maven) } else if flat.exists() { Some(flat) } else { None }
    };

    // --- resolve Kotlin compiler + stdlib (when needed) ----------------------
    let kotlin_stdlib_jars: Vec<PathBuf>;
    let kotlin_compiler_jars: Vec<PathBuf>; // all resolved JARs (compiler + stdlib + transitive)

    if has_kotlin {
        let kver = desc.kotlin.version();
        let kotlin_jars = resolve(
            &[
                DepEntry { key: KOTLIN_COMPILER_COORD, version: kver, repo_id: None },
                DepEntry { key: KOTLIN_STDLIB_COORD, version: kver, repo_id: None },
            ],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: true,
                bom_imports: bom_gavs.clone(),
                offline,
            },
        )
        .context("Kotlin compiler/stdlib resolution failed")?;
        println!("  Resolve Kotlin  {} JAR(s)", kotlin_jars.len());

        // Stdlib jars: everything except the compiler embeddable itself.
        // These are threaded into the compile and test runtime classpaths.
        let stdlib: Vec<PathBuf> = kotlin_jars
            .iter()
            .filter(|p| {
                p.file_name()
                    .map(|f| !f.to_string_lossy().starts_with("kotlin-compiler-embeddable"))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        kotlin_compiler_jars = kotlin_jars;
        kotlin_stdlib_jars = stdlib;
    } else {
        kotlin_compiler_jars = Vec::new();
        kotlin_stdlib_jars = Vec::new();
    }

    // --- resolve Groovy compiler + runtime (when needed) ---------------------
    let groovy_jars: Vec<PathBuf>;
    if has_groovy {
        let gver = desc.groovy.version();
        let jars = resolve(
            &[DepEntry { key: GROOVY_COORD, version: gver, repo_id: None }],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: true,
                bom_imports: bom_gavs.clone(),
                offline,
            },
        )
        .context("Groovy compiler/runtime resolution failed")?;
        println!("  Resolve Groovy  {} JAR(s)", jars.len());
        groovy_jars = jars;
    } else {
        groovy_jars = Vec::new();
    }

    // --- compile (incremental) -----------------------------------------------
    let toml_path = project_root.join("Curie.toml");
    let manifest_path = output_dir.join(".classes.toml");

    // Pre-compile prune: any source in the previous manifest that is no
    // longer in the current source set takes its old classes with it.
    // This must run BEFORE compilation because the classes dir is implicitly
    // searched during compile — a stale class could otherwise still
    // satisfy an unrelated import.
    let old_manifest = crate::class_manifest::load(&manifest_path)?;
    let current_sources_set: std::collections::HashSet<String> = sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    // AP-generated sources sit under `target/` and won't appear in
    // `current_sources_set`.  Tell the pre-prune to skip them — the
    // post-compile diff handles "AP stopped producing this".
    let canonical_target = output_dir
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    let pre_pruned: usize = match &old_manifest {
        Some(old) => {
            let stale = crate::class_manifest::stale_classes(
                old,
                None,
                &current_sources_set,
                canonical_target.as_deref(),
            );
            crate::class_manifest::delete_classes(&classes_dir, &stale)?
        }
        None => 0,
    };

    // Kotlin source-set tracking: compares this build's canonical .kt paths
    // against the set we stamped on the last successful compile.  Pure
    // deletions of .kt files don't bump any surviving mtime, so without this
    // check `needs_recompile` would return UpToDate and orphan Kotlin classes
    // would never get cleaned.  Empty `kt_set` with a non-empty previous set
    // (last build had Kotlin, this build has none) is also a change.
    let kt_set = kt_stale::canonical_kt_set(&kotlin_sources);
    let kt_prev = kt_stale::load_kt_sources(&output_dir);
    let kt_set_changed = kt_prev.as_ref().map(|p| p != &kt_set).unwrap_or(false);

    let compile_status = if pre_pruned > 0 || kt_set_changed {
        CompileStatus::StaleClasses
    } else {
        needs_recompile(&sources, &classes_dir, &toml_path, &output_dir)
    };

    if compile_status.needs_recompile() {
        println!(
            "  Compile         {} source file(s)  [{}]",
            sources.len(),
            compile_status.reason()
        );

        // Wipe Kotlin-derived classes ahead of kotlinc.  kotlinc re-emits
        // every class the current Kotlin source set still produces, so any
        // .class file whose JVM `SourceFile` attribute names a `.kt` source
        // is either about to be rewritten (still produced) or is orphaned
        // (deleted source, or removed declaration inside an edited source).
        // Wiping unconditionally before the compiler runs makes the second
        // case impossible — anything not re-emitted stays gone.  Also fires
        // when the project just transitioned away from Kotlin (kt_set is
        // empty but the previous build had .kt sources), in which case
        // kotlinc won't run and the wipe is the only cleanup.
        let wiped_kotlin_classes: Vec<PathBuf> = if has_kotlin || kt_set_changed {
            kt_stale::wipe_kotlin_derived_classes(&classes_dir)?
        } else {
            Vec::new()
        };

        // Build shared classpath entries used by both phases.
        let mut shared_cp: Vec<PathBuf> = Vec::new();
        if let Some(ref rd) = resources_dir {
            shared_cp.push(rd.clone());
        }
        shared_cp.extend_from_slice(&dep_jars);
        shared_cp.extend_from_slice(extra_cp);
        shared_cp.extend_from_slice(&ap_on_compile_classpath_jars);
        shared_cp.extend_from_slice(&kotlin_stdlib_jars);

        if has_kotlin {
            // ------------------------------------------------------------------
            // Phase 1: kotlinc — compiles all .kt + .java sources together.
            // kotlin-compiler-embeddable has no Main-Class manifest entry so
            // we invoke it via -cp + explicit main class, passing ALL resolved
            // Kotlin JARs (compiler + stdlib + transitive deps) on the
            // classpath.  We also pass -no-stdlib and -no-reflect so kotlinc
            // does not try to locate them relative to its "kotlin home"
            // directory (which doesn't exist in this Maven-based setup).
            // ------------------------------------------------------------------
            let mut kotlinc = Command::new("java");
            // Suppress the jansi native-access warning on JDK 17+.
            kotlinc.arg("--enable-native-access=ALL-UNNAMED");
            kotlinc.arg("-cp").arg(classpath_string(&kotlin_compiler_jars));
            kotlinc.arg("org.jetbrains.kotlin.cli.jvm.K2JVMCompiler");

            // Tell kotlinc not to try to find stdlib/reflect relative to a
            // kotlin-home directory (we supply them on the -cp explicitly).
            kotlinc.arg("-no-stdlib");
            kotlinc.arg("-no-reflect");

            // Output directory.
            kotlinc.arg("-d").arg(&classes_dir);

            // Classpath: deps + stdlib + extras.
            if !shared_cp.is_empty() {
                kotlinc.arg("-cp").arg(classpath_string(&shared_cp));
            }

            // Source files: all .kt and all .java together.
            for src in &kotlin_sources {
                kotlinc.arg(src);
            }
            for src in &java_sources {
                kotlinc.arg(src);
            }

            let status = kotlinc
                .status()
                .context("failed to invoke kotlinc — is a JRE installed?")?;

            if !status.success() {
                bail!("Kotlin compilation failed");
            }

            // Of the Kotlin-derived classes we wiped pre-kotlinc, anything
            // not present on disk now is a true orphan (deleted source, or
            // a declaration removed from a still-present source).  Classes
            // kotlinc just re-emitted are back, so they're filtered out.
            let kotlin_orphans = wiped_kotlin_classes.iter().filter(|p| !p.exists()).count();
            if kotlin_orphans > 0 {
                println!(
                    "  Stale (Kotlin)  removed {} orphan class file{}",
                    kotlin_orphans,
                    if kotlin_orphans == 1 { "" } else { "s" },
                );
            }
        } else if !wiped_kotlin_classes.is_empty() {
            // Project transitioned away from Kotlin entirely (last build had
            // .kt sources, this one doesn't): kotlinc didn't run, so every
            // wiped class is an orphan by definition.
            println!(
                "  Stale (Kotlin)  removed {} orphan class file{}",
                wiped_kotlin_classes.len(),
                if wiped_kotlin_classes.len() == 1 { "" } else { "s" },
            );
        }

        if has_groovy {
            // ------------------------------------------------------------------
            // Groovy phase: FileSystemCompiler compiles all .groovy sources.
            // When Java sources are also present, --jointCompilation is passed
            // so Groovy and Java can reference each other.  No separate javac
            // step is needed in that case.
            // ------------------------------------------------------------------
            let mut groovyc = Command::new("java");
            groovyc.arg("-cp").arg(classpath_string(&groovy_jars));
            groovyc.arg("org.codehaus.groovy.tools.FileSystemCompiler");
            groovyc.arg("-d").arg(&classes_dir);
            // Provide the full compile classpath so Groovy can resolve types.
            let mut gcp = shared_cp.clone();
            gcp.extend_from_slice(&groovy_jars);
            if !gcp.is_empty() {
                groovyc.arg("--classpath").arg(classpath_string(&gcp));
            }
            if has_java {
                groovyc.arg("--jointCompilation");
                for src in &java_sources {
                    groovyc.arg(src);
                }
            }
            for src in &groovy_sources {
                groovyc.arg(src);
            }
            let status = groovyc
                .status()
                .context("failed to invoke groovyc — is a JRE installed?")?;
            if !status.success() {
                bail!("Groovy compilation failed");
            }
        } else if has_java {
            // ------------------------------------------------------------------
            // Phase 2: javac — re-compiles Java sources only.
            // target/classes/ is on the classpath so Java can see Kotlin bytecode.
            // When there are no Kotlin sources this is the only phase (original
            // behaviour, unchanged except for the manifest wrapper).
            // ------------------------------------------------------------------
            let wrapper_jar = crate::wrapper::ensure()?;
            let mut javac = Command::new("java");
            javac.arg("-jar").arg(&wrapper_jar);
            javac.arg("--curie-manifest-out").arg(&manifest_path);
            javac
                .arg("--release")
                .arg(desc.java.effective())
                .arg("-g")
                .arg("-d")
                .arg(&classes_dir);

            // Classpath: target/classes (Kotlin bytecode) + shared entries.
            let mut cp_entries: Vec<PathBuf> = Vec::new();
            if has_kotlin {
                // Java must see the Kotlin .class files from Phase 1.
                cp_entries.push(classes_dir.clone());
            }
            cp_entries.extend_from_slice(&shared_cp);
            if !cp_entries.is_empty() {
                javac.arg("-cp").arg(classpath_string(&cp_entries));
            }

            // Annotation-processor classpath + generated-sources directory.
            if !ap_jars.is_empty() {
                let gen_dir = output_dir.join("generated-sources").join("annotations");
                std::fs::create_dir_all(&gen_dir).with_context(|| {
                    format!("failed to create {}", gen_dir.display())
                })?;
                javac.arg("-processorpath").arg(classpath_string(&ap_jars));
                javac.arg("-s").arg(&gen_dir);
            }

            // -A options (nested table flattened to `<prefix>.<key>=<value>`).
            for (key, value) in desc.flat_ap_options() {
                javac.arg(format!("-A{}={}", key, value));
            }

            for src in &java_sources {
                javac.arg(src);
            }

            let status = javac
                .status()
                .context("failed to invoke java — is a JRE installed?")?;

            if !status.success() {
                bail!("compilation failed");
            }

            // Post-compile prune: a source that's still around but produces a
            // smaller class set this time (e.g. removed a companion `class
            // Bar {}` from inside Foo.java) leaves Bar.class orphaned in the
            // classes dir.  Diff the new manifest against the old one.
            if let Some(old) = &old_manifest {
                if let Some(new) = crate::class_manifest::load(&manifest_path)? {
                    let stale = crate::class_manifest::stale_classes(
                        old,
                        Some(&new),
                        &current_sources_set,
                        None, // post-compile uses the new manifest, not the prefix carve-out
                    );
                    let n = crate::class_manifest::delete_classes(&classes_dir, &stale)?;
                    if n > 0 {
                        println!(
                            "  Stale           removed {} orphaned class file{}",
                            n,
                            if n == 1 { "" } else { "s" },
                        );
                    }
                }
            }
        } else if has_kotlin || has_groovy {
            // Kotlin-only / Groovy-only: no manifest written by javac wrapper,
            // but we still
            // need to write a minimal stamp so incremental works next time.
            // We write an empty manifest that covers all .kt sources so that
            // a future unchanged build is detected as up-to-date.
            // (The manifest schema expects source→[class] entries; an empty
            // file is accepted by class_manifest::load as None which triggers
            // a full recompile — so we write a minimal placeholder instead.)
            // For now: leave manifest absent; the stamp written below is enough
            // because needs_recompile checks source mtimes against the stamp.
        }

        // Record the JDK version used so that a future upgrade triggers a rebuild.
        if let Ok(version) = javac_version() {
            write_javac_version_stamp(&output_dir, &version)?;
        }

        // Stamp the canonical Kotlin source set so the next build can detect
        // pure deletions (which leave no surviving mtime to compare against).
        kt_stale::write_kt_sources(&output_dir, &kt_set)?;
    } else {
        println!("  Compile         up to date");
    }

    let jar_name = format!(
        "{}-{}.jar",
        desc.buildable_name().replace(':', "-"), desc.buildable_version()
    );
    let jar_path = output_dir.join(&jar_name);

    Ok(CompileOutput {
        jar_path, jar_name, classes_dir, src_roots, sources, dep_jars,
        kotlin_stdlib_jars, groovy_jars,
        resources_dir, test_resources_dir,
    })
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_prefix_maven_style_is_empty() {
        let p = Path::new("/some/path/src/main/java");
        assert_eq!(pkg_prefix_for_src_root(p), "");
    }

    #[test]
    fn pkg_prefix_flat_package_is_dir_name() {
        let p = Path::new("/some/path/src/com.example.myapp");
        assert_eq!(pkg_prefix_for_src_root(p), "com.example.myapp");
    }

    #[test]
    fn pkg_prefix_kotlin_maven_style_is_empty() {
        // src/main/kotlin has no dot in the final component — should return "".
        let p = Path::new("/some/path/src/main/kotlin");
        assert_eq!(pkg_prefix_for_src_root(p), "");
    }

    // --- flat_package_src_dirs detects dot-named dirs -----------------------

    #[test]
    fn flat_package_src_dirs_finds_dot_named_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("com.example.foo")).unwrap();
        std::fs::create_dir_all(src.join("com.example.bar")).unwrap();
        // A non-dot dir should be ignored.
        std::fs::create_dir_all(src.join("main")).unwrap();

        let mut found = flat_package_src_dirs(dir.path());
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("com.example.bar"));
        assert!(found[1].ends_with("com.example.foo"));
    }

    #[test]
    fn flat_package_src_dirs_empty_when_no_src_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(flat_package_src_dirs(dir.path()).is_empty());
    }

    // --- Kotlin source detection helpers ------------------------------------

    #[test]
    fn kotlin_version_constant_is_set() {
        assert!(!KOTLIN_VERSION.is_empty());
        // Basic sanity: must look like a semver triple.
        let parts: Vec<&str> = KOTLIN_VERSION.split('.').collect();
        assert!(parts.len() >= 2, "KOTLIN_VERSION should be at least major.minor");
    }

    // --- Groovy source detection helpers ------------------------------------

    #[test]
    fn groovy_sources_discovered_from_maven_layout() {
        let dir = tempfile::tempdir().unwrap();
        let groovy_src = dir.path().join("src").join("main").join("groovy")
            .join("com").join("example");
        std::fs::create_dir_all(&groovy_src).unwrap();
        std::fs::write(groovy_src.join("Greeter.groovy"), b"package com.example; class Greeter {}").unwrap();

        // Walk the maven-layout Groovy root and collect *.groovy, excluding test suffixes.
        use crate::incremental::walk_files;
        let root = dir.path().join("src").join("main").join("groovy");
        let found: Vec<_> = walk_files(&root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with(".groovy")
                    && !name.ends_with("Test.groovy")
                    && !name.ends_with("Tests.groovy")
                    && !name.ends_with("Spec.groovy")
            })
            .collect();
        assert_eq!(found.len(), 1, "should find exactly Greeter.groovy; got: {:?}", found);
        assert!(found[0].file_name().to_string_lossy().ends_with("Greeter.groovy"));
    }

    #[test]
    fn groovy_sources_discovered_from_flat_package() {
        let dir = tempfile::tempdir().unwrap();
        let flat_dir = dir.path().join("src").join("com.example");
        std::fs::create_dir_all(&flat_dir).unwrap();
        std::fs::write(flat_dir.join("Hello.groovy"), b"package com.example; class Hello {}").unwrap();
        std::fs::write(flat_dir.join("Hello.java"), b"package com.example; class Hello {}").unwrap();

        let flat_dirs = flat_package_src_dirs(dir.path());
        assert!(!flat_dirs.is_empty(), "should find com.example dir");

        use crate::incremental::walk_files;
        let groovy_files: Vec<_> = flat_dirs.iter()
            .flat_map(|d| walk_files(d)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".groovy"))
                .collect::<Vec<_>>()
            )
            .collect();
        assert_eq!(groovy_files.len(), 1, "should find Hello.groovy; got: {:?}", groovy_files);
    }
}
