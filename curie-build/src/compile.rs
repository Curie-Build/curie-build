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

use crate::build::{central_repos, extra_repos, extra_repos_for};
use crate::descriptor;
use crate::incremental::{
    self, javac_version, needs_recompile, walk_files, write_javac_version_stamp, CompileStatus,
};
use crate::jar::classpath_string;
use crate::jpms;
use crate::kt_stale;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, DepEntry, ResolveOptions};
use crate::lock;
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

/// Stamp file (under `target/`) recording the canonical production source set
/// from the last successful compile, used to detect added/removed sources that
/// mtime comparison can't see.
const SOURCE_SET_STAMP: &str = ".sources";

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
    /// Whether the project has a `module-info.java` (JPMS explicit module).
    pub is_modular: bool,
    /// The JPMS module name from `module-info.java`, if the project is modular.
    pub module_name: Option<String>,
    /// Dependency JARs placed on `--module-path` (modular projects only).
    pub module_path_jars: Vec<PathBuf>,
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

/// Module name passed to `kotlinc -module-name` for production
/// compilation, matching kotlin-maven-plugin's default `moduleName`
/// (`${project.artifactId}`) so the emitted `META-INF/<name>.kotlin_module`
/// matches `mvn`'s output (`maven.rs::build_project` sets `artifactId` to
/// the same value).
fn kotlin_module_name(desc: &descriptor::Descriptor) -> &str {
    desc.buildable_name()
}

/// Extracts the JDK major version from the raw output of `javac -version`.
///
/// Scans for the line that starts with "javac " to skip any preamble lines
/// emitted when JAVA_TOOL_OPTIONS is set ("Picked up JAVA_TOOL_OPTIONS: ...").
fn parse_jdk_major_version(version_output: &str) -> Option<String> {
    for line in version_output.lines() {
        if let Some(rest) = line.trim().strip_prefix("javac ") {
            if let Some(major) = rest.trim().split('.').next() {
                if !major.is_empty() && major.chars().all(|c| c.is_ascii_digit()) {
                    return Some(major.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn running_jdk_major_version() -> Result<String> {
    let out = Command::new("javac")
        .arg("-version")
        .output()
        .context("failed to invoke javac — is a JDK installed?")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Try stderr first (the usual location), then stdout. When JAVA_TOOL_OPTIONS
    // is set some JDKs write the preamble to stderr and the actual version line
    // to stdout, so we must check both.
    parse_jdk_major_version(&stderr)
        .or_else(|| parse_jdk_major_version(&stdout))
        .with_context(|| {
            format!("cannot parse JDK major version from javac -version output\nstderr: {stderr}\nstdout: {stdout}")
        })
}

/// Returns the `--release` value to pass to javac, if any.
///
/// When `releaseVersion` is set, uses it.  When it is absent but
/// `enablePreview = true`, falls back to the running JDK's major version
/// (javac requires `--release` whenever `--enable-preview` is used).
/// When neither applies, returns `None` (javac targets the running JDK).
pub(crate) fn javac_release_arg(desc: &descriptor::Descriptor) -> Result<Option<String>> {
    if let Some(release) = desc.java.effective() {
        return Ok(Some(release.to_string()));
    }
    if desc.java.preview_enabled() {
        return Ok(Some(running_jdk_major_version()?));
    }
    Ok(None)
}

/// JVM system-property argument that pins the bytecode level emitted by
/// `groovyc` (`FileSystemCompiler`) to the project's effective Java release.
///
/// `FileSystemCompiler` has no `-target`/`--release` CLI flag; left alone it
/// targets the running JVM (e.g. class major 69 on JDK 25), which diverges
/// from `mvn`'s `gmavenplus-plugin` output where `<targetBytecode>` is pinned
/// to `${maven.compiler.release}`.  Groovy's `CompilerConfiguration` reads the
/// `groovy.target.bytecode` system property, so setting it makes Curie's
/// Groovy classes match `mvn`'s for the same `releaseVersion`.  Must be
/// placed before the main-class name (it is a JVM option, not a program arg).
/// Returns `None` when `releaseVersion` is unset (groovyc will target
/// the running JDK's own version, matching the behaviour of omitting `--release`
/// from javac in the same build).
pub(crate) fn groovy_target_bytecode_arg(desc: &descriptor::Descriptor) -> Option<String> {
    desc.java.effective().map(|v| format!("-Dgroovy.target.bytecode={v}"))
}

/// Build the `--classpath` argument list for a groovyc invocation.
/// When `java_classes_dir` is `Some`, it is appended last so Groovy code can
/// resolve Java types compiled in the preceding javac phase.
pub(crate) fn groovyc_compiler_classpath(
    shared_cp: &[PathBuf],
    groovy_jars: &[PathBuf],
    java_classes_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut gcp: Vec<PathBuf> = shared_cp.to_vec();
    gcp.extend_from_slice(groovy_jars);
    if let Some(dir) = java_classes_dir {
        gcp.push(dir.to_path_buf());
    }
    gcp
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
    compile_with_options(project_root, desc, offline, false, extra_cp)
}

/// Like [`compile`], with explicit control over SNAPSHOT refresh (`-U`).
pub fn compile_with_options(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    offline: bool,
    update_snapshots: bool,
    extra_cp: &[PathBuf],
) -> Result<CompileOutput> {
    // --- source roots --------------------------------------------------------
    // Supported layouts (may coexist):
    //   A) Maven-style Java:   src/main/java/
    //   B) Maven-style Kotlin: src/main/kotlin/
    //   C) Maven-style Groovy: src/main/groovy/
    //   D) Flat-package:       src/com.example.myapp/  (dot-named sibling under src/)
    //      .java, .kt, and .groovy files are all collected from flat-package dirs.
    //   E) Bare src/:          src/Hello.java  (unnamed classes — no package dir)
    let maven_java_src   = project_root.join("src").join("main").join("java");
    let maven_kotlin_src = project_root.join("src").join("main").join("kotlin");
    let maven_groovy_src = project_root.join("src").join("main").join("groovy");
    let flat_src_dirs = flat_package_src_dirs(project_root);
    // Track which roots use the Curie co-located-test convention (flat-package only).
    let flat_src_set: std::collections::HashSet<PathBuf> = flat_src_dirs.iter().cloned().collect();

    let mut src_roots: Vec<PathBuf> = Vec::new();
    if maven_java_src.exists()   { src_roots.push(maven_java_src.clone()); }
    if maven_kotlin_src.exists() { src_roots.push(maven_kotlin_src.clone()); }
    if maven_groovy_src.exists() { src_roots.push(maven_groovy_src.clone()); }
    src_roots.extend(flat_src_dirs);

    // Layout E: source files placed directly under src/ (no package subdirectory).
    // This is the natural location for unnamed-class files that have no package
    // declaration.  Only add src/ itself if it contains at least one direct
    // source file — this prevents accidentally treating src/ as a root in
    // projects that already use one of the layouts above.
    let bare_src = project_root.join("src");
    if bare_src.exists() && !src_roots.contains(&bare_src) {
        let has_direct_sources = std::fs::read_dir(&bare_src)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| {
                        e.file_type().map(|t| t.is_file()).unwrap_or(false)
                            && matches!(
                                e.path().extension().and_then(|s| s.to_str()),
                                Some("java") | Some("kt") | Some("groovy")
                            )
                    })
            })
            .unwrap_or(false);
        if has_direct_sources {
            src_roots.push(bare_src);
        }
    }

    // --- plugins: source generators -----------------------------------------
    // Each [plugin.<name>] activates curie-<name> to produce extra source dirs.
    // curie-build owns staleness tracking; the plugin only runs when inputs changed
    // or when a previously-generated output file is missing from disk.
    if !desc.plugins.is_empty() {
        for (plugin_name, plugin_config) in &desc.plugins {
            let envelope = build_plugin_envelope(plugin_config)?;
            let manifest = crate::plugin::fetch_manifest(plugin_name, &envelope, project_root)?;
            // Hash the config envelope + plugin version so a config edit that
            // doesn't touch any input file still re-runs generation (bug #5).
            let config_hash = crate::plugin::config_hash(&envelope, &manifest.version);
            let plugins_dir = project_root.join("target").join(".curie-plugins");
            let stamp_path = plugins_dir.join(format!("{plugin_name}.stamp"));
            let output_set_stamp =
                crate::plugin::plugin_output_set_stamp_path(&plugins_dir, plugin_name);

            // Load what the last successful run generated and what's on disk now.
            let prev_output_set = incremental::load_source_set(&output_set_stamp);
            let pre_run_output_set =
                crate::plugin::current_plugin_output_set(&manifest, project_root);

            // A previously-generated file that no longer exists on disk (e.g. manually
            // deleted) forces a re-run so it gets regenerated.
            let outputs_intact = prev_output_set
                .as_ref()
                .map(|prev| pre_run_output_set.is_superset(prev))
                .unwrap_or(true); // no stamp yet → first build, rely on input check only

            if !crate::plugin::is_up_to_date(&manifest, &stamp_path, project_root, &config_hash)
                || !outputs_intact
            {
                let input_summary = summarise_plugin_inputs(&manifest, project_root);
                crate::parallel::emit(&crate::style::active(
                    &format!("Plugin {plugin_name}"),
                    &input_summary,
                ));
                // Use both default (mirrored) repos and any project [[repositories]]
                // so that plugin artifacts (e.g. generators, protoc) respect the same
                // configuration surface as ordinary dependencies.
                let plugin_repos: Vec<_> = {
                    let mut r = crate::build::central_repos();
                    r.extend(crate::build::extra_repos(desc));
                    r
                };
                let resolved = crate::plugin::download_artifacts(
                    &manifest.artifacts,
                    &plugin_repos,
                    offline,
                )?;
                crate::plugin::generate_sources(
                    plugin_name,
                    &envelope,
                    &resolved,
                    project_root,
                    offline,
                )?;
                crate::plugin::write_stamp(&manifest, &stamp_path, project_root, &config_hash)?;

                // Orphan wipe: delete files from the previous run that the plugin no
                // longer emits (e.g. a .proto source was removed).
                let post_run_output_set =
                    crate::plugin::current_plugin_output_set(&manifest, project_root);
                if let Some(prev) = &prev_output_set {
                    let wiped =
                        crate::plugin::wipe_orphaned_plugin_outputs(prev, &post_run_output_set);
                    if !wiped.is_empty() {
                        crate::parallel::emit(&crate::style::info(
                            &format!("Plugin {plugin_name}"),
                            &format!("removed {} orphaned generated file(s)", wiped.len()),
                        ));
                    }
                }
                incremental::write_source_set(&output_set_stamp, &post_run_output_set)?;
            } else {
                crate::parallel::emit(&crate::style::up_to_date(&format!("Plugin {plugin_name}")));
            }
            for dir in &manifest.outputs.source_dirs {
                src_roots.push(project_root.join(dir));
            }
        }
    }

    if src_roots.is_empty() {
        bail!(
            "no source directory found: expected src/main/java/, src/main/kotlin/, \
             src/main/groovy/, src/<dot-named>/ (flat-package), or source files \
             directly in src/ (unnamed classes)"
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
            .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository(), exclusions: v.exclusions(), classifier: None, allow_version_conflict: v.allow_version_conflict() })
            .collect();

        // User-declared deps: honour Curie.lock for SNAPSHOT pins and rewrite
        // it after resolve. Tool classpaths (kotlinc, etc.) keep plain resolve.
        let jars = lock::resolve_with_lock(
            project_root,
            &pairs,
            ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos_for(desc, Some(project_root)),
                progress: crate::parallel::try_get_sink().is_none(),
                bom_imports: bom_gavs.clone(),
                offline,
                skip_version_ranges: false,
                // User-declared [dependencies]: fail on a major-version conflict
                // unless the coordinate sets allowVersionConflict (bug #13).
                error_on_version_conflict: true,
                snapshot_pins: Default::default(),
                update_snapshots: false,
                snapshot_update_policy: Default::default(),
            },
            update_snapshots,
        )
        .context("dependency resolution failed")?;

        crate::parallel::emit(&crate::style::resolve("Resolve deps", &format!("{} JAR(s)", jars.len())));
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
            .map(|(k, v)| DepEntry { key: k, version: v, repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false })
            .collect();
        let jars = resolve(
            &ap_entries,
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: crate::parallel::try_get_sink().is_none(),
                bom_imports: bom_gavs.clone(),
                offline,
                skip_version_ranges: false, error_on_version_conflict: false,
                snapshot_pins: Default::default(),
                update_snapshots: false,
                snapshot_update_policy: Default::default(),
            },
        )
        .context("annotation-processor resolution failed")?;
        crate::parallel::emit(&crate::style::resolve("Resolve APs", &format!("{} JAR(s)", jars.len())));

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
                &[DepEntry { key: coord, version, repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false }],
                &ResolveOptions {
                    default_repos: central_repos(),
                    named_repos: extra_repos(desc),
                    progress: false,
                    bom_imports: bom_gavs.clone(),
                    offline,
                    skip_version_ranges: false, error_on_version_conflict: false,
                    snapshot_pins: Default::default(),
                    update_snapshots: false,
                    snapshot_update_policy: Default::default(),
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

    // --- discover production sources -----------------------------------------
    // Co-located test convention (*Test.java, *Spec.java, etc.) applies only to
    // Curie flat-package roots.  Maven layout roots (src/main/java, etc.) include
    // every file — a class named LoadTest or ABTest there is production code.
    let mut java_sources: Vec<PathBuf>   = Vec::new();
    let mut kotlin_sources: Vec<PathBuf> = Vec::new();
    let mut groovy_sources: Vec<PathBuf> = Vec::new();

    for src_root in &src_roots {
        let colocated_layout = flat_src_set.contains(src_root);

        let root_java: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                if !name.ends_with(".java") { return false; }
                if colocated_layout {
                    !name.ends_with("Test.java")
                        && !name.ends_with("Tests.java")
                        && !name.ends_with("Spec.java")
                } else {
                    true
                }
            })
            .map(|e| e.into_path())
            .collect();
        java_sources.extend(root_java);

        let root_kotlin: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                if !name.ends_with(".kt") { return false; }
                if colocated_layout {
                    !name.ends_with("Test.kt")
                        && !name.ends_with("Tests.kt")
                        && !name.ends_with("Spec.kt")
                } else {
                    true
                }
            })
            .map(|e| e.into_path())
            .collect();
        kotlin_sources.extend(root_kotlin);

        let root_groovy: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                if !name.ends_with(".groovy") { return false; }
                if colocated_layout {
                    !name.ends_with("Test.groovy")
                        && !name.ends_with("Tests.groovy")
                        && !name.ends_with("Spec.groovy")
                } else {
                    true
                }
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
                DepEntry { key: KOTLIN_COMPILER_COORD, version: kver, repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false },
                DepEntry { key: KOTLIN_STDLIB_COORD, version: kver, repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false },
            ],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: crate::parallel::try_get_sink().is_none(),
                bom_imports: bom_gavs.clone(),
                offline,
                skip_version_ranges: false, error_on_version_conflict: false,
                snapshot_pins: Default::default(),
                update_snapshots: false,
                snapshot_update_policy: Default::default(),
            },
        )
        .context("Kotlin compiler/stdlib resolution failed")?;
        crate::parallel::emit(&crate::style::resolve("Resolve Kotlin", &format!("{} JAR(s)", kotlin_jars.len())));

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
            &[DepEntry { key: GROOVY_COORD, version: gver, repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false }],
            &ResolveOptions {
                default_repos: central_repos(),
                named_repos: extra_repos(desc),
                progress: crate::parallel::try_get_sink().is_none(),
                bom_imports: bom_gavs.clone(),
                offline,
                skip_version_ranges: false, error_on_version_conflict: false,
                snapshot_pins: Default::default(),
                update_snapshots: false,
                snapshot_update_policy: Default::default(),
            },
        )
        .context("Groovy compiler/runtime resolution failed")?;
        crate::parallel::emit(&crate::style::resolve("Resolve Groovy", &format!("{} JAR(s)", jars.len())));
        groovy_jars = jars;
    } else {
        groovy_jars = Vec::new();
    }

    // --- JPMS detection and validation ----------------------------------------
    let module_info_path = jpms::find_module_info_java(&src_roots);
    let is_modular = module_info_path.is_some();

    if is_modular && has_groovy {
        bail!(
            "JPMS modules (module-info.java) are not supported with Groovy sources; \
             use separate modules for each language"
        );
    }

    if is_modular {
        if let Some(release) = desc.java.effective() {
            let release_num: u32 = release.parse().unwrap_or(0);
            if release_num < 9 {
                bail!(
                    "JPMS modules (module-info.java) require Java 9 or later; \
                     project specifies releaseVersion = \"{release}\""
                );
            }
        }
    }

    if is_modular {
        if let crate::descriptor::DescriptorKind::Library(lib) = &desc.kind {
            if lib.automatic_module_name.is_some() {
                bail!(
                    "a project with module-info.java must not also declare \
                     automaticModuleName; remove one or the other"
                );
            }
        }
    }

    let module_split: Option<(jpms::ParsedModuleInfo, jpms::ModuleSplit)> =
        if let Some(ref mi_path) = module_info_path {
            let content = std::fs::read_to_string(mi_path)
                .with_context(|| format!("failed to read {}", mi_path.display()))?;
            let parsed = jpms::parse_module_info_java(&content)
                .with_context(|| format!("failed to parse {}", mi_path.display()))?;
            let target_dir = project_root.join("target");
            // Include kotlin stdlib jars in the split so they can appear on
            // --module-path when module-info.java declares `requires kotlin.stdlib`.
            let split_jars: Vec<PathBuf> = {
                let mut v = dep_jars.clone();
                v.extend_from_slice(&kotlin_stdlib_jars);
                v
            };
            let split = jpms::compute_module_path_split(&parsed, &split_jars, &target_dir)
                .context("failed to compute module-path split")?;
            Some((parsed, split))
        } else {
            None
        };

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

    // Source-set tracking (all languages): compares this build's canonical
    // source paths against the set stamped on the last successful compile.  A
    // source added with a preserved-old mtime (mv/cp -p/rsync/untar) or a pure
    // deletion doesn't bump any surviving mtime, so without this check
    // `needs_recompile` would wrongly return UpToDate.  An empty current set
    // with a non-empty previous one (e.g. transitioned away from Kotlin) is
    // also a change, which keeps the Kotlin orphan-wipe firing.
    let source_set = incremental::canonical_source_set(&sources);
    let source_set_prev =
        incremental::load_source_set(&incremental::source_set_stamp_path(&output_dir, SOURCE_SET_STAMP));
    let source_set_changed =
        incremental::source_set_changed(source_set_prev.as_ref(), &source_set);

    let compile_status = if pre_pruned > 0 {
        CompileStatus::StaleClasses
    } else if source_set_changed {
        CompileStatus::SourceSetChanged
    } else if old_manifest
        .as_ref()
        .is_some_and(|m| crate::class_manifest::has_missing_classes(m, &classes_dir))
    {
        // A class recorded in the manifest vanished from target/classes (partial
        // build, interrupted compile, externally deleted, or a failed parallel
        // build).  The mtime check would otherwise report "up to date" and never
        // regenerate it — leaving e.g. the main class missing.
        CompileStatus::MissingClasses
    } else {
        needs_recompile(&sources, &classes_dir, &toml_path, &output_dir, &[])
    };

    if compile_status.needs_recompile() {
        crate::parallel::emit(&crate::style::active(
            "Compile",
            &format!("{} source file(s)  [{}]", sources.len(), compile_status.reason()),
        ));

        // Wipe Kotlin-derived classes ahead of kotlinc.  kotlinc re-emits
        // every class the current Kotlin source set still produces, so any
        // .class file whose JVM `SourceFile` attribute names a `.kt` source
        // is either about to be rewritten (still produced) or is orphaned
        // (deleted source, or removed declaration inside an edited source).
        // Wiping unconditionally before the compiler runs makes the second
        // case impossible — anything not re-emitted stays gone.  Also fires
        // when the project just transitioned away from Kotlin (the source set
        // changed and the previous build had .kt sources), in which case
        // kotlinc won't run and the wipe is the only cleanup.
        let wiped_kotlin_classes: Vec<PathBuf> = if has_kotlin || source_set_changed {
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

            kotlinc.arg("-module-name").arg(kotlin_module_name(desc));

            // Output directory.
            kotlinc.arg("-d").arg(&classes_dir);

            // Classpath: deps + stdlib + extras.
            if !shared_cp.is_empty() {
                kotlinc.arg("-cp").arg(classpath_string(&shared_cp));
            }

            // Source files: all .kt and all .java together.
            // In modular projects, exclude module-info.java from kotlinc — it is
            // compiled by javac in phase 2 (with --patch-module) so the module
            // descriptor covers both Java and Kotlin classes.  Passing it to
            // kotlinc would require kotlin.stdlib on the module path at compile
            // time, which is an unnecessary complexity for kotlinc's classpath phase.
            for src in &kotlin_sources {
                kotlinc.arg(src);
            }
            for src in &java_sources {
                if is_modular && src.file_name() == Some(std::ffi::OsStr::new("module-info.java")) {
                    continue;
                }
                kotlinc.arg(src);
            }

            let status = crate::proc::spawn_cmd(&mut kotlinc)
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
                crate::parallel::emit(&crate::style::stale(
                    "Stale (Kotlin)",
                    &format!("removed {} orphan class file{}", kotlin_orphans, if kotlin_orphans == 1 { "" } else { "s" }),
                ));
            }
        } else if !wiped_kotlin_classes.is_empty() {
            crate::parallel::emit(&crate::style::stale(
                "Stale (Kotlin)",
                &format!("removed {} orphan class file{}", wiped_kotlin_classes.len(), if wiped_kotlin_classes.len() == 1 { "" } else { "s" }),
            ));
        }

        if has_java {
            // ------------------------------------------------------------------
            // Java phase: javac with our manifest wrapper.
            // target/classes/ is on the classpath so Java can see Kotlin
            // bytecode from the kotlinc phase above.  When Groovy sources are
            // also present javac still runs first (see Groovy phase below), so
            // Groovy code can reference the resulting Java bytecode.
            // ------------------------------------------------------------------
            let wrapper_jar = crate::wrapper::ensure()?;
            let mut javac = Command::new("java");
            javac.arg("-jar").arg(&wrapper_jar);
            javac.arg("--curie-manifest-out").arg(&manifest_path);
            if let Some(release) = javac_release_arg(desc)? {
                javac.arg("--release").arg(release);
            }
            if desc.java.preview_enabled() {
                javac.arg("--enable-preview");
            }
            javac
                .arg("-g")
                .arg("-d")
                .arg(&classes_dir);

            if let Some((parsed_mi, split)) = &module_split {
                // Modular javac invocation: use --module-path + --patch-module.
                if !split.module_path.is_empty() {
                    javac.arg("--module-path").arg(classpath_string(&split.module_path));
                }

                // When Kotlin sources were compiled in phase 1, patch the module
                // so javac can see the Kotlin .class files alongside our sources.
                if has_kotlin {
                    let patch_arg = format!(
                        "{}={}",
                        parsed_mi.module_name,
                        classes_dir.display()
                    );
                    javac.arg("--patch-module").arg(patch_arg);
                }

                // Remaining deps that are not on module-path go on -cp.
                let mut cp_entries: Vec<PathBuf> = Vec::new();
                cp_entries.extend_from_slice(&split.classpath);
                // shared_cp entries not already in split.module_path.
                for entry in &shared_cp {
                    if !split.module_path.contains(entry) && !cp_entries.contains(entry) {
                        cp_entries.push(entry.clone());
                    }
                }
                if !cp_entries.is_empty() {
                    javac.arg("-cp").arg(classpath_string(&cp_entries));
                }
            } else {
                // Non-modular: original -cp behaviour.
                let mut cp_entries: Vec<PathBuf> = Vec::new();
                if has_kotlin {
                    // Java must see the Kotlin .class files from Phase 1.
                    cp_entries.push(classes_dir.clone());
                }
                cp_entries.extend_from_slice(&shared_cp);
                if !cp_entries.is_empty() {
                    javac.arg("-cp").arg(classpath_string(&cp_entries));
                }
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

            let status = crate::proc::spawn_cmd(&mut javac)
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
                        crate::parallel::emit(&crate::style::stale(
                            "Stale",
                            &format!("removed {} orphaned class file{}", n, if n == 1 { "" } else { "s" }),
                        ));
                    }
                }
            }
        }

        if has_groovy {
            // ------------------------------------------------------------------
            // Groovy phase: FileSystemCompiler compiles all .groovy sources.
            // Java sources (if any) are compiled by the Java phase above so
            // they produce a .classes.toml manifest and get full AP support.
            // classes_dir is added to the compiler classpath so Groovy code
            // can reference Java types from the same module.
            // ------------------------------------------------------------------
            let mut groovyc = Command::new("java");
            if let Some(arg) = groovy_target_bytecode_arg(desc) {
                groovyc.arg(arg);
            }
            groovyc.arg("-cp").arg(classpath_string(&groovy_jars));
            groovyc.arg("org.codehaus.groovy.tools.FileSystemCompiler");
            groovyc.arg("-d").arg(&classes_dir);
            let gcp = groovyc_compiler_classpath(
                &shared_cp,
                &groovy_jars,
                if has_java { Some(&classes_dir) } else { None },
            );
            if !gcp.is_empty() {
                groovyc.arg("--classpath").arg(classpath_string(&gcp));
            }
            for src in &groovy_sources {
                groovyc.arg(src);
            }
            let status = crate::proc::spawn_cmd(&mut groovyc)
                .context("failed to invoke groovyc — is a JRE installed?")?;
            if !status.success() {
                bail!("Groovy compilation failed");
            }
        }

        // Record the JDK version used so that a future upgrade triggers a rebuild.
        if let Ok(version) = javac_version() {
            write_javac_version_stamp(&output_dir, &version)?;
        }

        // Stamp the canonical source set (all languages) so the next build can
        // detect additions/deletions that leave no surviving mtime to compare.
        incremental::write_source_set(
            &incremental::source_set_stamp_path(&output_dir, SOURCE_SET_STAMP),
            &source_set,
        )?;
    } else {
        crate::parallel::emit(&crate::style::up_to_date("Compile"));
    }

    let jar_name = format!(
        "{}-{}.jar",
        desc.buildable_name().replace(':', "-"), desc.buildable_version()
    );
    let jar_path = output_dir.join(&jar_name);

    let (module_name, module_path_jars) = match module_split {
        Some((parsed_mi, split)) => (Some(parsed_mi.module_name), split.module_path),
        None => (None, Vec::new()),
    };

    Ok(CompileOutput {
        jar_path, jar_name, classes_dir, src_roots, sources, dep_jars,
        kotlin_stdlib_jars, groovy_jars,
        resources_dir, test_resources_dir,
        is_modular,
        module_name,
        module_path_jars,
    })
}
fn summarise_plugin_inputs(manifest: &crate::plugin::PluginManifest, project_root: &Path) -> String {
    let from_dirs = manifest.inputs.dirs.iter().flat_map(|d| {
        let full = project_root.join(d);
        walkdir::WalkDir::new(&full)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    });

    let from_files = manifest.inputs.files.iter().map(|f| {
        Path::new(f)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.to_string_lossy().into_owned())
    });

    let all: Vec<_> = from_dirs.chain(from_files).collect();
    if all.is_empty() {
        "(no inputs)".to_string()
    } else {
        all.join(", ")
    }
}

fn build_plugin_envelope(config: &toml::Value) -> Result<String> {
    let envelope = serde_json::json!({
        "curie_version": env!("CARGO_PKG_VERSION"),
        "config": serde_json::to_value(config).context("failed to convert plugin config to JSON")?,
    });
    serde_json::to_string(&envelope).context("failed to serialize plugin envelope")
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

    #[test]
    fn kotlin_module_name_matches_buildable_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"hello-kotlin\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nreleaseVersion = \"21\"\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();

        assert_eq!(kotlin_module_name(&desc), "hello-kotlin");
    }

    #[test]
    fn groovy_target_bytecode_uses_effective_release() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"g\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nreleaseVersion = \"21\"\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();

        assert_eq!(groovy_target_bytecode_arg(&desc), Some("-Dgroovy.target.bytecode=21".to_string()));
    }

    #[test]
    fn groovy_target_bytecode_absent_when_no_release_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"g\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();

        // No [java] section → effective() is None → no bytecode arg (groovyc targets running JDK).
        assert_eq!(groovy_target_bytecode_arg(&desc), None);
    }

    // --- parse_jdk_major_version --------------------------------------------

    #[test]
    fn parse_jdk_major_version_plain_output() {
        assert_eq!(parse_jdk_major_version("javac 21.0.3\n"), Some("21".to_string()));
        assert_eq!(parse_jdk_major_version("javac 25.0.1"), Some("25".to_string()));
        assert_eq!(parse_jdk_major_version("javac 11"), Some("11".to_string()));
    }

    #[test]
    fn parse_jdk_major_version_with_java_tool_options_preamble() {
        let output = "Picked up JAVA_TOOL_OPTIONS: -Dhttp.proxyHost=172.16.0.4 -Dhttp.proxyPort=2080\njavac 21.0.3\n";
        assert_eq!(parse_jdk_major_version(output), Some("21".to_string()));
    }

    #[test]
    fn parse_jdk_major_version_preamble_only_returns_none() {
        // Version line absent — only the JAVA_TOOL_OPTIONS preamble on stderr;
        // version line was written to stdout instead (caller should try stdout).
        let stderr = "Picked up JAVA_TOOL_OPTIONS: -Dhttp.proxyHost=172.16.0.4 -Dhttp.proxyPort=2080";
        assert_eq!(parse_jdk_major_version(stderr), None);
    }

    #[test]
    fn parse_jdk_major_version_unrecognised_output_returns_none() {
        assert_eq!(parse_jdk_major_version(""), None);
        assert_eq!(parse_jdk_major_version("Picked up JAVA_TOOL_OPTIONS: something"), None);
    }

    // --- javac_release_arg --------------------------------------------------

    #[test]
    fn javac_release_arg_uses_release_version_when_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nreleaseVersion = \"21\"\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();
        assert_eq!(javac_release_arg(&desc).unwrap(), Some("21".to_string()));
    }

    #[test]
    fn javac_release_arg_absent_without_preview_or_release_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();
        assert_eq!(javac_release_arg(&desc).unwrap(), None);
    }

    #[test]
    fn javac_release_arg_falls_back_to_running_jdk_when_preview_and_no_release_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nenablePreview = true\n",
        )
        .unwrap();
        let desc = descriptor::load(dir.path()).unwrap();
        let release = javac_release_arg(&desc).unwrap().expect("should return running JDK version");
        // Must be a non-empty numeric major version string.
        assert!(!release.is_empty(), "release should not be empty");
        assert!(release.parse::<u32>().is_ok(), "release should be a number, got: {release}");
    }

    // --- Groovy source detection helpers ------------------------------------

    #[test]
    fn groovy_sources_discovered_from_maven_layout_includes_test_named_files() {
        // Maven layout (src/main/groovy) is a production root — files named
        // *Test.groovy / *Spec.groovy there are production classes, not tests.
        let dir = tempfile::tempdir().unwrap();
        let groovy_src = dir.path().join("src").join("main").join("groovy")
            .join("com").join("example");
        std::fs::create_dir_all(&groovy_src).unwrap();
        std::fs::write(groovy_src.join("Greeter.groovy"), b"package com.example; class Greeter {}").unwrap();
        std::fs::write(groovy_src.join("GreeterSpec.groovy"), b"package com.example; class GreeterSpec {}").unwrap();

        use crate::incremental::walk_files;
        let root = dir.path().join("src").join("main").join("groovy");
        // No test-suffix exclusion for maven layout — all .groovy files are production.
        let found: Vec<_> = walk_files(&root)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".groovy"))
            .collect();
        assert_eq!(found.len(), 2, "both Greeter.groovy and GreeterSpec.groovy should be included; got: {:?}", found);
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

    // --- groovyc_compiler_classpath -----------------------------------------

    #[test]
    fn groovyc_classpath_omits_classes_dir_when_no_java() {
        let shared = vec![PathBuf::from("/deps/dep.jar")];
        let groovy = vec![PathBuf::from("/groovy/groovy.jar")];
        let classes = PathBuf::from("/target/classes");

        let gcp = groovyc_compiler_classpath(&shared, &groovy, None);
        assert_eq!(gcp, vec![
            PathBuf::from("/deps/dep.jar"),
            PathBuf::from("/groovy/groovy.jar"),
        ]);
        assert!(!gcp.contains(&classes), "classes_dir must not appear when Java is absent");
    }

    #[test]
    fn groovyc_classpath_appends_classes_dir_when_java_present() {
        let shared = vec![PathBuf::from("/deps/dep.jar")];
        let groovy = vec![PathBuf::from("/groovy/groovy.jar")];
        let classes = PathBuf::from("/target/classes");

        let gcp = groovyc_compiler_classpath(&shared, &groovy, Some(&classes));
        assert_eq!(gcp, vec![
            PathBuf::from("/deps/dep.jar"),
            PathBuf::from("/groovy/groovy.jar"),
            PathBuf::from("/target/classes"),
        ]);
    }
}
