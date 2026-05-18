//! Production source discovery and compilation.
//!
//! Supports two source layouts side-by-side:
//!   - Maven-style: `src/main/java/com/foo/Bar.java`
//!   - Flat-package: any dot-named sibling under `src/`, e.g.
//!     `src/com.example.myapp/Bar.java` (the directory name IS the package).
//!
//! Both layouts produce the same compiled output under `target/classes/`
//! and may coexist in a single project.

use crate::build::extra_repos;
use crate::descriptor;
use crate::incremental::{
    javac_version, needs_recompile, walk_files, write_javac_version_stamp, CompileStatus,
};
use crate::jar::classpath_string;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Resolved production annotation-processor JARs (empty when no
    /// `[annotation-processors]` declared / inherited).  These are on
    /// javac's `-processorpath` only; downstream code paths
    /// (test/run/Docker) generally don't need them.
    pub ap_jars: Vec<PathBuf>,
    /// Subset of `ap_jars` whose entry was declared with
    /// `on-compile-classpath = true`.  Added to javac's `-cp` so user code
    /// can reference annotation types that live in the same jar as the
    /// processor (Lombok).
    pub ap_on_compile_classpath_jars: Vec<PathBuf>,
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
    // Support two layouts simultaneously:
    //   A) Maven-style:   src/main/java/
    //   B) Flat-package:  src/com.example.myapp/  (any dot-named sibling under src/)
    let maven_src = project_root.join("src").join("main").join("java");
    let flat_src_dirs = flat_package_src_dirs(project_root);

    let mut src_roots: Vec<PathBuf> = Vec::new();
    if maven_src.exists() {
        src_roots.push(maven_src.clone());
    }
    src_roots.extend(flat_src_dirs);

    if src_roots.is_empty() {
        bail!(
            "no source directory found: expected src/main/java/ \
             or at least one dot-named directory under src/ \
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
        let pairs: Vec<(&str, &str)> = desc
            .dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let jars = resolve(
            &pairs,
            &ResolveOptions {
                extra_repos: extra_repos(desc),
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
        let jars = resolve(
            &ap_pairs,
            &ResolveOptions {
                extra_repos: extra_repos(desc),
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
                &[(coord, version)],
                &ResolveOptions {
                    extra_repos: extra_repos(desc),
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
    let mut sources: Vec<PathBuf> = Vec::new();
    for src_root in &src_roots {
        let root_sources: Vec<_> = walk_files(src_root)
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                name.ends_with(".java")
                    && !name.ends_with("Test.java")
                    && !name.ends_with("Tests.java")
                    && !name.ends_with("Spec.java")
            })
            .map(|e| e.into_path())
            .collect();
        sources.extend(root_sources);
    }

    if sources.is_empty() {
        bail!(
            "no Java source files found under {}",
            src_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    sources.sort();
    sources.dedup();

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

    // --- compile (incremental) -----------------------------------------------
    let toml_path = project_root.join("Curie.toml");
    let manifest_path = output_dir.join(".classes.toml");

    // Pre-compile prune: any source in the previous manifest that is no
    // longer in the current source set takes its old classes with it.
    // This must run BEFORE javac because the classes dir is implicitly
    // searched during compile — a stale class could otherwise still
    // satisfy an unrelated import.
    let old_manifest = crate::class_manifest::load(&manifest_path)?;
    let current_sources_set: std::collections::HashSet<String> = sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let pre_pruned: usize = match &old_manifest {
        Some(old) => {
            let stale = crate::class_manifest::stale_classes(old, None, &current_sources_set);
            crate::class_manifest::delete_classes(&classes_dir, &stale)?
        }
        None => 0,
    };

    let compile_status = if pre_pruned > 0 {
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

        // Invoke the embedded javac wrapper instead of javac directly so
        // we capture the source → class mapping in `.classes.toml`.
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

        // Build compile classpath: src/main/resources + production deps
        // + caller-supplied entries (workspace-dep JARs and their
        // transitive contributions) + processor jars marked on-compile-classpath.
        let mut cp_entries: Vec<PathBuf> = Vec::new();
        if let Some(ref rd) = resources_dir {
            cp_entries.push(rd.clone());
        }
        cp_entries.extend_from_slice(&dep_jars);
        cp_entries.extend_from_slice(extra_cp);
        cp_entries.extend_from_slice(&ap_on_compile_classpath_jars);
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

        for src in &sources {
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

        // Record the JDK version used so that a future upgrade triggers a rebuild.
        if let Ok(version) = javac_version() {
            write_javac_version_stamp(&output_dir, &version)?;
        }
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
        ap_jars, ap_on_compile_classpath_jars,
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
}
