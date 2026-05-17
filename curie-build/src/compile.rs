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
    javac_version, needs_recompile, write_javac_version_stamp, CompileStatus,
};
use crate::jar::classpath_string;
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

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
pub fn compile(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    offline: bool,
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

    // --- discover production sources (exclude test files) --------------------
    let mut sources: Vec<PathBuf> = Vec::new();
    for src_root in &src_roots {
        let root_sources: Vec<_> = WalkDir::new(src_root)
            .into_iter()
            .filter_map(|e| e.ok())
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

    // Remove stale class files before checking whether recompilation is needed.
    // If any are removed we must recompile even if no source is newer than the
    // oldest surviving class file.
    let root_source_pairs: Vec<(&Path, &[PathBuf])> = src_roots
        .iter()
        .map(|r| {
            let slice: &[PathBuf] = &sources;
            (r.as_path(), slice)
        })
        .collect();
    let stale_removed = remove_stale_classes(
        &root_source_pairs,
        &classes_dir,
    )?;

    let compile_status = if stale_removed > 0 {
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

        let mut javac = Command::new("javac");
        javac
            .arg("--release")
            .arg(&desc.java.source_compatibility)
            .arg("-g")
            .arg("-d")
            .arg(&classes_dir);

        // Build compile classpath: src/main/resources + production deps.
        let mut cp_entries: Vec<PathBuf> = Vec::new();
        if let Some(ref rd) = resources_dir {
            cp_entries.push(rd.clone());
        }
        cp_entries.extend_from_slice(&dep_jars);
        if !cp_entries.is_empty() {
            javac.arg("-cp").arg(classpath_string(&cp_entries));
        }

        for src in &sources {
            javac.arg(src);
        }

        let status = javac
            .status()
            .context("failed to invoke javac — is a JDK installed?")?;

        if !status.success() {
            bail!("compilation failed");
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
        desc.project_name().replace(':', "-"), desc.project_version()
    );
    let jar_path = output_dir.join(&jar_name);

    Ok(CompileOutput { jar_path, jar_name, classes_dir, src_roots, sources, dep_jars, resources_dir, test_resources_dir })
}

/// Remove `.class` files in `classes_dir` that have no corresponding source
/// file in `sources` (relative to `src_root`).
///
/// A `.class` file belongs to a source `Foo.java` when its top-level class
/// stem matches — i.e. the class file is `Foo.class` or `Foo$anything.class`.
/// Inner and anonymous classes (`Foo$Bar.class`, `Foo$1.class`) are therefore
/// covered automatically by their enclosing top-level class.
///
/// Multiple `(src_root, sources)` pairs can be supplied so that co-located
/// tests and separate-tree tests can both be accounted for when cleaning
/// `target/test-classes/`.
///
/// Returns the number of `.class` files deleted.
///
/// Suppress this check by passing an empty slice. Note that this also
/// implies that the absence of any sources will be considered as "all
/// classes are stale".
pub(crate) fn remove_stale_classes(
    src_roots_and_sources: &[(&Path, &[PathBuf])],
    classes_dir: &Path,
) -> Result<usize> {
    // Build a set of expected stems: relative path without extension,
    // prefixed by the src_root's package (empty for Maven-style roots,
    // the directory name for flat-package roots such as `src/com.example/`).
    //
    // e.g.
    //   src_root=src/main/java, source=src/main/java/com/foo/Bar.java
    //     -> stem = "com/foo/Bar"
    //   src_root=src/com.example, source=src/com.example/Foo.java
    //     -> stem = "com/example/Foo"  (the dir name IS the package)
    let mut expected: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (src_root, sources) in src_roots_and_sources {
        let pkg_prefix = pkg_prefix_for_src_root(src_root);
        let pkg_path = pkg_prefix.replace('.', "/");
        for src in *sources {
            if let Ok(rel) = src.strip_prefix(src_root) {
                let s = rel.to_string_lossy();
                let rel_stem = s.trim_end_matches(".java").replace('\\', "/");
                let stem = if pkg_path.is_empty() {
                    rel_stem
                } else {
                    format!("{}/{}", pkg_path, rel_stem)
                };
                expected.insert(stem);
            }
        }
    }

    if !classes_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0usize;

    for entry in WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name().to_string_lossy().ends_with(".class")
        })
    {
        let rel = entry
            .path()
            .strip_prefix(classes_dir)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();

        // Derive the top-level class stem: strip ".class" and everything
        // from the first "$" onward in the filename component.
        let stem = {
            // Remove ".class"
            let without_ext = &rel[..rel.len() - 6];
            // Split off any inner-class suffix in the final component.
            // e.g. "com/foo/Bar$Inner" -> "com/foo/Bar"
            if let Some(dollar) = without_ext.rfind('$') {
                // Only strip after '$' in the last path component.
                let slash = without_ext.rfind('/').map(|i| i + 1).unwrap_or(0);
                if dollar >= slash {
                    without_ext[..dollar].to_string()
                } else {
                    without_ext.to_string()
                }
            } else {
                without_ext.to_string()
            }
        };

        if !expected.contains(&stem) {
            std::fs::remove_file(entry.path()).with_context(|| {
                format!("failed to remove stale class {}", entry.path().display())
            })?;
            removed += 1;
        }
    }

    Ok(removed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    // -----------------------------------------------------------------------
    // Stale class detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn remove_stale_classes_removes_orphaned_class() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();

        // Only Foo.java exists as a source.
        let foo_src = src_root.join("com").join("Foo.java");
        std::fs::create_dir_all(foo_src.parent().unwrap()).unwrap();
        std::fs::write(&foo_src, b"").unwrap();

        // Both Foo.class and Bar.class exist — Bar is stale.
        write_file(&classes_dir.join("com").join("Foo.class"), b"");
        write_file(&classes_dir.join("com").join("Bar.class"), b"");

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 1);
        assert!(classes_dir.join("com").join("Foo.class").exists());
        assert!(!classes_dir.join("com").join("Bar.class").exists());
    }

    #[test]
    fn remove_stale_classes_keeps_inner_classes() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();

        // Foo.java is the only source.
        let foo_src = src_root.join("Foo.java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(&foo_src, b"").unwrap();

        // Foo.class, Foo$Inner.class, Foo$1.class — all should survive.
        write_file(&classes_dir.join("Foo.class"), b"");
        write_file(&classes_dir.join("Foo$Inner.class"), b"");
        write_file(&classes_dir.join("Foo$1.class"), b"");

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 0);
        assert!(classes_dir.join("Foo.class").exists());
        assert!(classes_dir.join("Foo$Inner.class").exists());
        assert!(classes_dir.join("Foo$1.class").exists());
    }

    #[test]
    fn remove_stale_classes_removes_inner_of_deleted_source() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();

        // Only Foo.java remains; Bar.java was deleted.
        let foo_src = src_root.join("Foo.java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(&foo_src, b"").unwrap();

        write_file(&classes_dir.join("Foo.class"), b"");
        // Bar and its inner classes are stale.
        write_file(&classes_dir.join("Bar.class"), b"");
        write_file(&classes_dir.join("Bar$Inner.class"), b"");
        write_file(&classes_dir.join("Bar$1.class"), b"");

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 3);
        assert!(classes_dir.join("Foo.class").exists());
        assert!(!classes_dir.join("Bar.class").exists());
        assert!(!classes_dir.join("Bar$Inner.class").exists());
        assert!(!classes_dir.join("Bar$1.class").exists());
    }

    #[test]
    fn remove_stale_classes_empty_classes_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src");
        let classes_dir = dir.path().join("classes");
        // classes_dir does not exist at all.

        let foo_src = src_root.join("Foo.java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(&foo_src, b"").unwrap();

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 0);
    }

    #[test]
    fn remove_stale_classes_multiple_src_roots() {
        let dir = tempfile::tempdir().unwrap();
        let main_src = dir.path().join("src").join("main");
        let test_src = dir.path().join("src").join("test");
        let classes_dir = dir.path().join("classes");

        let foo_src = main_src.join("Foo.java");
        let bar_test_src = test_src.join("BarTest.java");
        std::fs::create_dir_all(&main_src).unwrap();
        std::fs::create_dir_all(&test_src).unwrap();
        std::fs::write(&foo_src, b"").unwrap();
        std::fs::write(&bar_test_src, b"").unwrap();

        // Both Foo.class and BarTest.class are valid; Baz.class is stale.
        write_file(&classes_dir.join("Foo.class"), b"");
        write_file(&classes_dir.join("BarTest.class"), b"");
        write_file(&classes_dir.join("Baz.class"), b"");

        let main_sources = vec![foo_src];
        let test_sources = vec![bar_test_src];
        let removed = remove_stale_classes(
            &[
                (main_src.as_path(), &main_sources),
                (test_src.as_path(), &test_sources),
            ],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 1);
        assert!(classes_dir.join("Foo.class").exists());
        assert!(classes_dir.join("BarTest.class").exists());
        assert!(!classes_dir.join("Baz.class").exists());
    }

    // Regression: flat-package src roots (e.g. `src/com.example/`) encode the
    // package in the directory name.  javac emits class files under the
    // declared package (e.g. `com/example/Foo.class`), so the stale-class
    // computation must prepend the dir-name-as-package when matching.  Without
    // this, every build deletes the valid class file as "orphaned" and forces
    // a full recompile.
    #[test]
    fn remove_stale_classes_flat_package_root() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src").join("com.example");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();

        // src/com.example/Foo.java declares `package com.example;`
        // → javac emits classes/com/example/Foo.class
        let foo_src = src_root.join("Foo.java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(&foo_src, b"package com.example; class Foo {}").unwrap();
        write_file(&classes_dir.join("com").join("example").join("Foo.class"), b"");

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        // Must NOT delete the valid class file.
        assert_eq!(removed, 0);
        assert!(classes_dir.join("com").join("example").join("Foo.class").exists());
    }

    #[test]
    fn remove_stale_classes_flat_package_root_removes_truly_stale() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("src").join("com.example");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();

        let foo_src = src_root.join("Foo.java");
        std::fs::create_dir_all(&src_root).unwrap();
        std::fs::write(&foo_src, b"").unwrap();

        // Foo.class is valid; Bar.class is from a deleted source — must go.
        write_file(&classes_dir.join("com").join("example").join("Foo.class"), b"");
        write_file(&classes_dir.join("com").join("example").join("Bar.class"), b"");

        let sources = vec![foo_src];
        let removed = remove_stale_classes(
            &[(&src_root, &sources)],
            &classes_dir,
        ).unwrap();

        assert_eq!(removed, 1);
        assert!(classes_dir.join("com").join("example").join("Foo.class").exists());
        assert!(!classes_dir.join("com").join("example").join("Bar.class").exists());
    }

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
