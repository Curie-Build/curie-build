use crate::{descriptor, docker, test};
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use curie_deps::repo::Repository;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC.
/// Matches SOURCE_DATE_EPOCH convention used by Debian, Nix, etc.
fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0)
        .expect("epoch constant is valid")
}

pub struct BuildOptions {
    pub no_docker: bool,
}

/// Output paths produced by a successful build.
pub struct BuildOutput {
    pub jar: PathBuf,
    /// Resolved dependency JARs (empty when no [dependencies] declared).
    pub dep_jars: Vec<PathBuf>,
    /// Resolved (declared or auto-detected) main class; `None` for library projects.
    pub main_class: Option<String>,
    /// `src/main/resources` if the directory exists, otherwise `None`.
    pub resources_dir: Option<PathBuf>,
}

pub fn build(project_root: &Path, opts: BuildOptions) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    println!(
        "Building {} v{}",
        desc.project_name(), desc.project_version()
    );

    // Library projects must not have a Dockerfile at the project root.
    if desc.is_library() && project_root.join("Dockerfile").exists() {
        anyhow::bail!(
            "library projects do not support Docker: remove the Dockerfile from the project root"
        );
    }

    let output = do_build(project_root, &desc)?;

    println!(
        "  Done            {}",
        output
            .jar
            .strip_prefix(project_root)
            .unwrap_or(&output.jar)
            .display()
    );

    if !desc.is_library() && !opts.no_docker && descriptor::docker_enabled(project_root, &desc) {
        docker::docker_build(project_root, &desc, &output.jar, &output.dep_jars)?;
    }

    Ok(())
}

/// Build the list of extra Maven repositories from the descriptor.
pub fn extra_repos(desc: &descriptor::Descriptor) -> Vec<Repository> {
    desc.repositories
        .iter()
        .map(|r| Repository {
            name: r.name.clone(),
            url: r.url.clone(),
        })
        .collect()
}

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
    let src = project_root.join("src");
    if !src.exists() {
        return vec![];
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&src)
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

/// Returns all immediate subdirectories of `<project_root>/tests/` whose names
/// contain a dot — these are flat-package integration-test roots.
pub fn flat_package_test_dirs(project_root: &Path) -> Vec<PathBuf> {
    let tests = project_root.join("tests");
    if !tests.exists() {
        return vec![];
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&tests)
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

/// Phase 1: resolve production deps and compile production sources.
/// Does NOT run tests or package a JAR.
pub fn compile(
    project_root: &Path,
    desc: &descriptor::Descriptor,
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
    let bom_gavs: Vec<curie_deps::Gav> = desc
        .bom_imports
        .iter()
        .map(|(k, v)| curie_deps::Gav::from_key_version(k, v))
        .collect::<anyhow::Result<_>>()
        .context("invalid coordinate in [bom-imports]")?;

    let dep_jars = if desc.dependencies.is_empty() && desc.bom_imports.is_empty() {
        vec![]
    } else if desc.dependencies.is_empty() {
        // BOMs declared but no deps — nothing to resolve yet.
        vec![]
    } else {
        let pairs: Vec<(&str, &str)> = desc
            .dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let extra_repos = extra_repos(desc);

        let jars = resolve(
            &pairs,
            &ResolveOptions {
                extra_repos,
                verbose: false,
                bom_imports: bom_gavs.clone(),
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
    let toml_path = project_root.join("curie.toml");

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

/// Phase 2: compile production sources, run tests, then package JAR.
pub fn do_build(
    project_root: &Path,
    desc: &descriptor::Descriptor,
) -> Result<BuildOutput> {
    let compiled = compile(project_root, desc)?;

    // --- run tests before packaging ------------------------------------------
    test::run_tests(
        project_root,
        desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        None,
    )?;

    // --- package (deterministic JAR, incremental) ----------------------------
    // mainClass detection/validation is deferred to here: it is only needed to
    // write the JAR manifest, so we skip it entirely when packaging is up to date.
    let resources_dir = compiled.resources_dir.as_deref();
    let resolved_main_class: Option<String> = if needs_repackage(&compiled.jar_path, &compiled.classes_dir, resources_dir) {
        let main_class = if let Some(app) = &desc.application {
            let mc = match &app.main_class {
                Some(declared) => {
                    validate_main_class(declared, &compiled.classes_dir, &compiled.dep_jars)?;
                    declared.clone()
                }
                None => {
                                    let detected = detect_main_class(
                                        &compiled.src_roots,
                                        &compiled.sources,
                                        &compiled.classes_dir,
                                        &compiled.dep_jars,
                                    )?;
                    println!("  Detected        mainClass = {}", detected);
                    detected
                }
            };
            Some(mc)
        } else {
            None // library
        };

        println!("  Package         {}", compiled.jar_name);
        write_deterministic_jar(
            &compiled.jar_path,
            &compiled.classes_dir,
            resources_dir,
            main_class.as_deref(),
            &compiled.dep_jars,
        )
        .context("failed to write JAR")?;

        main_class
    } else {
        println!("  Package         up to date");
        // mainClass not needed — JAR already has the correct manifest.
        desc.application.as_ref().and_then(|a| a.main_class.clone())
    };

    // --- populate target/libs/ with dep JARs (hardlink preferred) ------------
    // Always done for application projects so that `java -jar` works.
    // target/libs/ is wiped and repopulated on every build to stay in sync
    // with the current dep set (handles version bumps cleanly).
    if !compiled.dep_jars.is_empty() && desc.application.is_some() {
        let libs_dir = project_root.join("target").join("libs");
        populate_libs_dir(&libs_dir, &compiled.dep_jars)
            .context("failed to populate target/libs")?;
    }

    Ok(BuildOutput {
        jar: compiled.jar_path,
        dep_jars: compiled.dep_jars,
        main_class: resolved_main_class,
        resources_dir: compiled.resources_dir,
    })
}

// ---------------------------------------------------------------------------
// Incremental build helpers
// ---------------------------------------------------------------------------

/// Return the `modified` time of `path`, or `SystemTime::UNIX_EPOCH` on any
/// error (missing file, unsupported platform). Treating errors as epoch means
/// the missing output is always considered stale.
pub(crate) fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Return the oldest `modified` time among all files under `dir`, or
/// `SystemTime::UNIX_EPOCH` when the directory is empty or doesn't exist.
pub(crate) fn oldest_mtime_in_dir(dir: &Path) -> SystemTime {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .min()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Return the newest `modified` time among `paths`, or `SystemTime::UNIX_EPOCH`
/// when the slice is empty.
pub(crate) fn newest_mtime(paths: &[PathBuf]) -> SystemTime {
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Reason a recompile is required, or confirmation that it is not.
#[derive(Debug, PartialEq)]
pub(crate) enum CompileStatus {
    /// No `.class` files exist yet.
    NoClassFiles,
    /// At least one source file is newer than the oldest `.class` file.
    SourceChanged,
    /// `curie.toml` is newer than the oldest `.class` file.
    TomlChanged,
    /// Stale `.class` files were found (sources deleted since last compile).
    StaleClasses,
    /// The JDK version used to compile has changed since the last build.
    JdkChanged,
    /// All outputs are up to date — no recompile needed.
    UpToDate,
}

impl CompileStatus {
    pub(crate) fn needs_recompile(&self) -> bool {
        !matches!(self, CompileStatus::UpToDate)
    }

    /// Short human-readable reason appended to the "Compile" log line.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            CompileStatus::NoClassFiles => "no class files",
            CompileStatus::SourceChanged => "source changed",
            CompileStatus::TomlChanged => "curie.toml changed",
            CompileStatus::StaleClasses => "stale classes removed",
            CompileStatus::JdkChanged => "JDK version changed",
            CompileStatus::UpToDate => "up to date",
        }
    }
}

/// Returns the version string reported by `javac -version` (e.g. `"javac 21.0.3"`).
///
/// `javac` writes its version to **stderr** (not stdout).
pub(crate) fn javac_version() -> Result<String> {
    let out = Command::new("javac")
        .arg("-version")
        .output()
        .context("failed to invoke javac — is a JDK installed?")?;
    // javac writes its version to stderr.
    let raw = String::from_utf8_lossy(&out.stderr);
    let version = raw.trim().to_string();
    if version.is_empty() {
        // Fall back to stdout in case a non-standard JDK writes there.
        let raw_out = String::from_utf8_lossy(&out.stdout);
        let version_out = raw_out.trim().to_string();
        if version_out.is_empty() {
            bail!("javac -version produced no output");
        }
        return Ok(version_out);
    }
    Ok(version)
}

/// Path of the file that records the `javac` version used for the last
/// successful compilation.  Lives next to `.test-stamp` and `.docker-stamp`.
pub(crate) fn javac_version_stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".javac-version")
}

/// Write the current `javac` version to the stamp file in `target_dir`.
pub(crate) fn write_javac_version_stamp(target_dir: &Path, version: &str) -> Result<()> {
    let path = javac_version_stamp_path(target_dir);
    std::fs::write(&path, version)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Returns the reason a recompile is (or is not) required.
pub(crate) fn needs_recompile(
    sources: &[PathBuf],
    classes_dir: &Path,
    toml_path: &Path,
    target_dir: &Path,
) -> CompileStatus {
    let oldest_class = oldest_mtime_in_dir(classes_dir);
    if oldest_class == SystemTime::UNIX_EPOCH {
        return CompileStatus::NoClassFiles;
    }
    // Check JDK fingerprint before mtime comparisons — a JDK upgrade should
    // always trigger a full recompile regardless of source timestamps.
    if let Ok(current) = javac_version() {
        let stamp = javac_version_stamp_path(target_dir);
        let stored = std::fs::read_to_string(&stamp).unwrap_or_default();
        if stored.trim() != current.trim() {
            return CompileStatus::JdkChanged;
        }
    }
    if newest_mtime(sources) > oldest_class {
        return CompileStatus::SourceChanged;
    }
    if mtime(toml_path) > oldest_class {
        return CompileStatus::TomlChanged;
    }
    CompileStatus::UpToDate
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

/// Returns `true` when the output JAR needs to be written: either it doesn't
/// exist yet, a class file is newer than the JAR, or a resource file is newer
/// than the JAR.
pub(crate) fn needs_repackage(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
) -> bool {
    let jar_mtime = mtime(jar_path);
    if jar_mtime == SystemTime::UNIX_EPOCH {
        return true;
    }
    // Check class files.
    let newest_class = WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if newest_class > jar_mtime {
        return true;
    }
    // Check resource files (only when directory exists).
    if let Some(rd) = resources_dir {
        let newest_resource = WalkDir::new(rd)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if newest_resource > jar_mtime {
            return true;
        }
    }
    false
}

/// Build an OS-appropriate classpath string from a list of JAR paths.
pub fn classpath_string(jars: &[PathBuf]) -> String {
    join_classpath(jars)
}

/// Build a properly folded `Class-Path` manifest header per the JAR spec.
///
/// The JAR/manifest spec (JVMS §5.3.4, java.util.jar.Manifest) requires:
///   - No line in MANIFEST.MF may exceed 72 bytes (including the `\r\n`).
///   - Continuation lines begin with a single space (which does not count
///     as content); each continuation line is also limited to 72 bytes total.
///
/// So the first line holds 72 - len("Class-Path: ") - 2 = 58 bytes of value,
/// and each subsequent line holds 72 - 1 (space) - 2 (\r\n) = 69 bytes.
fn manifest_class_path(dep_jars: &[PathBuf]) -> String {
    let value = dep_jars
        .iter()
        .filter_map(|p| p.file_name())
        .map(|f| format!("libs/{}", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");

    fold_manifest_header("Class-Path", &value)
}

/// Fold a manifest header value to lines of at most 72 bytes (including \r\n).
/// Returns the complete header block including the trailing \r\n.
fn fold_manifest_header(name: &str, value: &str) -> String {
    // Bytes available on the first line: 72 total - name - ": " - "\r\n"
    let first_capacity = 72usize.saturating_sub(name.len() + 2 + 2);
    // Bytes available on continuation lines: 72 total - " " prefix - "\r\n"
    let cont_capacity = 69usize;

    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut pos = 0usize;
    let mut first = true;

    while pos < bytes.len() {
        let capacity = if first { first_capacity } else { cont_capacity };
        // Advance by at most `capacity` bytes, but don't split a multi-byte
        // UTF-8 sequence (walk back to a char boundary).
        let mut end = (pos + capacity).min(bytes.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &value[pos..end];
        if first {
            out.push_str(name);
            out.push_str(": ");
            first = false;
        } else {
            out.push(' '); // continuation line prefix
        }
        out.push_str(chunk);
        out.push_str("\r\n");
        pos = end;
    }

    // Edge case: empty value.
    if first {
        out.push_str(name);
        out.push_str(": \r\n");
    }

    out
}

/// Populate `libs_dir` with all `dep_jars`, using hardlinks where possible
/// and falling back to a full copy otherwise (e.g. cross-device).
///
/// The directory is wiped before population so that stale JARs from previous
/// builds (version bumps, removed dependencies) are removed.
fn populate_libs_dir(libs_dir: &Path, dep_jars: &[PathBuf]) -> Result<()> {
    // Wipe and recreate for a clean slate.
    if libs_dir.exists() {
        std::fs::remove_dir_all(libs_dir)
            .with_context(|| format!("failed to remove {}", libs_dir.display()))?;
    }
    std::fs::create_dir_all(libs_dir)
        .with_context(|| format!("failed to create {}", libs_dir.display()))?;

    for src in dep_jars {
        let file_name = src
            .file_name()
            .with_context(|| format!("dep JAR has no filename: {}", src.display()))?;
        let dst = libs_dir.join(file_name);

        // Try hardlink first; fall back to copy on any error (cross-device,
        // unsupported filesystem, permissions, etc.).
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst)
                .with_context(|| format!("failed to copy {} to {}", src.display(), dst.display()))?;
        }
    }

    println!("  Libs            {} JAR(s) → target/libs/", dep_jars.len());
    Ok(())
}

/// Write a reproducible JAR:
///   • entries sorted lexicographically
///   • all timestamps set to EPOCH (2024-01-01 00:00:00 UTC)
///   • MANIFEST.MF written first (JAR spec requirement)
///   • Class-Path header added when dep_jars is non-empty
///   • files from `resources_dir` (src/main/resources) embedded at their
///     path relative to that directory, alongside the class files
///   • no extra tool-specific metadata that embeds build time
fn write_deterministic_jar(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    main_class: Option<&str>,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("cannot create {}", jar_path.display()))?;

    let mut zip = ZipWriter::new(file);

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(epoch())
        .unix_permissions(0o644);

    let dir_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(epoch())
        .unix_permissions(0o755);

    // --- MANIFEST.MF (must be first entry per JAR spec) ---------------------
    zip.start_file("META-INF/", dir_options)
        .context("failed to write META-INF/ directory entry")?;

    let mut manifest = "Manifest-Version: 1.0\r\n".to_string();
    if let Some(mc) = main_class {
        manifest.push_str(&format!("Main-Class: {}\r\n", mc));
    }
    if !dep_jars.is_empty() {
        // manifest_class_path() returns the fully-folded header block
        // (including the trailing \r\n) per the JAR spec 72-byte line limit.
        manifest.push_str(&manifest_class_path(dep_jars));
    }
    manifest.push_str("\r\n");

    zip.start_file("META-INF/MANIFEST.MF", options)
        .context("failed to start MANIFEST.MF entry")?;
    zip.write_all(manifest.as_bytes())
        .context("failed to write MANIFEST.MF")?;

    // --- collect entries from classes_dir and resources_dir -----------------
    // We gather (zip_path, fs_path) pairs from both roots, deduplicate by
    // zip_path (class files win over resources for the same path, matching
    // Maven's behaviour), then sort and write.
    let mut entries: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();

    for (root, label) in [
        (Some(classes_dir), "classes"),
        (resources_dir, "resources"),
    ] {
        let Some(root) = root else { continue };
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() || e.file_type().is_dir())
        {
            let rel = entry
                .path()
                .strip_prefix(root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if rel.is_empty() {
                continue; // skip the root dir itself
            }
            let zip_path = if entry.file_type().is_dir() {
                format!("{}/", rel)
            } else {
                rel
            };
            // Skip META-INF from resources — we already wrote the manifest.
            if zip_path.starts_with("META-INF") {
                continue;
            }
            // Class files take precedence; skip if already inserted from classes.
            if label == "resources" && entries.contains_key(&zip_path) {
                continue;
            }
            entries.insert(zip_path, entry.into_path());
        }
    }

    // BTreeMap is already sorted lexicographically.
    for (zip_path, fs_path) in &entries {
        if zip_path.ends_with('/') {
            zip.start_file(zip_path, dir_options)
                .with_context(|| format!("failed to write directory entry {}", zip_path))?;
        } else {
            zip.start_file(zip_path, options)
                .with_context(|| format!("failed to start entry {}", zip_path))?;
            let data = std::fs::read(fs_path)
                .with_context(|| format!("failed to read {}", fs_path.display()))?;
            zip.write_all(&data)
                .with_context(|| format!("failed to write entry {}", zip_path))?;
        }
    }

    zip.finish().context("failed to finalise JAR")?;
    Ok(())
}

/// Join JAR paths with the OS classpath separator (":" on Unix, ";" on Windows).
fn join_classpath(jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    jars.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Write `content` to `path`, creating parent directories as needed.
    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Set the mtime of `path` to `base + offset`.
    fn set_mtime(path: &Path, time: SystemTime) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_system_time(time),
        )
        .unwrap_or_else(|e| panic!("set_mtime({}) failed: {e}", path.display()));
    }

    // -- mtime ----------------------------------------------------------------

    #[test]
    fn mtime_missing_file_returns_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("ghost.txt");
        assert_eq!(mtime(&absent), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn mtime_existing_file_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        write_file(&f, b"hi");
        assert!(mtime(&f) > SystemTime::UNIX_EPOCH);
    }

    // -- oldest_mtime_in_dir --------------------------------------------------

    #[test]
    fn oldest_mtime_empty_dir_returns_epoch() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(oldest_mtime_in_dir(dir.path()), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn oldest_mtime_missing_dir_returns_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no_such_dir");
        assert_eq!(oldest_mtime_in_dir(&absent), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn oldest_mtime_returns_minimum() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        let old = dir.path().join("old.class");
        let new = dir.path().join("new.class");
        write_file(&old, b"old");
        write_file(&new, b"new");
        set_mtime(&old, base);
        set_mtime(&new, base + Duration::from_secs(60));

        assert_eq!(oldest_mtime_in_dir(dir.path()), base);
    }

    // -- newest_mtime ---------------------------------------------------------

    #[test]
    fn newest_mtime_empty_slice_returns_epoch() {
        assert_eq!(newest_mtime(&[]), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn newest_mtime_returns_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);

        let a = dir.path().join("a.java");
        let b = dir.path().join("b.java");
        write_file(&a, b"A");
        write_file(&b, b"B");
        set_mtime(&a, base);
        set_mtime(&b, base + Duration::from_secs(30));

        assert_eq!(newest_mtime(&[a, b]), base + Duration::from_secs(30));
    }

    // -- needs_recompile ------------------------------------------------------

    #[test]
    fn needs_recompile_no_class_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        let classes_dir = dir.path().join("classes"); // does not exist
        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::NoClassFiles);
    }

    #[test]
    fn needs_recompile_empty_classes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        let classes_dir = dir.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::NoClassFiles);
    }

    #[test]
    fn needs_recompile_false_when_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        set_mtime(&src, base);

        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        // class is newer than both src and toml
        set_mtime(&class_file, base + Duration::from_secs(10));

        // Write the current javac version stamp so the JDK check passes.
        if let Ok(v) = javac_version() {
            write_javac_version_stamp(dir.path(), &v).unwrap();
        }

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::UpToDate);
    }

    #[test]
    fn needs_recompile_true_when_source_newer_than_class() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        // source is newer than the class
        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        set_mtime(&src, base + Duration::from_secs(5));

        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base - Duration::from_secs(10));

        // Write the current javac version stamp so the JDK check passes.
        if let Ok(v) = javac_version() {
            write_javac_version_stamp(dir.path(), &v).unwrap();
        }

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::SourceChanged);
    }

    #[test]
    fn needs_recompile_true_when_toml_newer_than_class() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        set_mtime(&src, base - Duration::from_secs(10));

        // curie.toml changed after last compile
        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base + Duration::from_secs(5));

        // Write the current javac version stamp so the JDK check passes.
        if let Ok(v) = javac_version() {
            write_javac_version_stamp(dir.path(), &v).unwrap();
        }

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::TomlChanged);
    }

    #[test]
    fn needs_recompile_true_when_jdk_changed() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base + Duration::from_secs(10));

        let src = dir.path().join("Foo.java");
        write_file(&src, b"class Foo {}");
        set_mtime(&src, base);

        let toml = dir.path().join("curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base);

        // Write a *different* javac version to simulate a JDK upgrade.
        write_javac_version_stamp(dir.path(), "javac 99.0.0").unwrap();

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::JdkChanged);
    }

    // -- needs_repackage ------------------------------------------------------

    #[test]
    fn needs_repackage_no_jar() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("app.jar"); // does not exist
        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");

        assert!(needs_repackage(&jar, &classes_dir, None));
    }

    #[test]
    fn needs_repackage_false_when_jar_newer_than_classes() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        let jar = dir.path().join("app.jar");
        write_file(&jar, b"jar");
        set_mtime(&jar, base + Duration::from_secs(5));

        assert!(!needs_repackage(&jar, &classes_dir, None));
    }

    #[test]
    fn needs_repackage_true_when_class_newer_than_jar() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let jar = dir.path().join("app.jar");
        write_file(&jar, b"jar");
        set_mtime(&jar, base);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base + Duration::from_secs(5));

        assert!(needs_repackage(&jar, &classes_dir, None));
    }

    #[test]
    fn needs_repackage_true_when_resource_newer_than_jar() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let jar = dir.path().join("app.jar");
        write_file(&jar, b"jar");
        set_mtime(&jar, base);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base - Duration::from_secs(10));

        let resources_dir = dir.path().join("resources");
        let res_file = resources_dir.join("data.txt");
        write_file(&res_file, b"resource");
        // resource is newer than the jar
        set_mtime(&res_file, base + Duration::from_secs(5));

        assert!(needs_repackage(&jar, &classes_dir, Some(&resources_dir)));
    }

    #[test]
    fn needs_repackage_false_when_jar_newer_than_classes_and_resources() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        let resources_dir = dir.path().join("resources");
        let res_file = resources_dir.join("data.txt");
        write_file(&res_file, b"resource");
        set_mtime(&res_file, base);

        let jar = dir.path().join("app.jar");
        write_file(&jar, b"jar");
        set_mtime(&jar, base + Duration::from_secs(5));

        assert!(!needs_repackage(&jar, &classes_dir, Some(&resources_dir)));
    }
}

// ---------------------------------------------------------------------------
// Main-class detection and validation
// ---------------------------------------------------------------------------

/// Derive the fully-qualified class name from a `.java` source path, trying
/// each `src_root` in order and using the first successful strip.
///
/// For Maven-style roots (`src/main/java`), the FQCN is the path under the
/// root with separators replaced by dots.
///
/// For flat-package roots (`src/com.example.foo`), the directory name IS the
/// package, so it is prepended to the FQCN.  Example: source
/// `src/com.example.foo/Bar.java` under root `src/com.example.foo` yields
/// FQCN `com.example.foo.Bar`.
///
/// For unnamed/compact source files (no top-level type declaration) the class
/// name equals the file stem.
fn fqcn_from_source(src_roots: &[PathBuf], source: &Path) -> Option<String> {
    for src_root in src_roots {
        if let Ok(rel) = source.strip_prefix(src_root) {
            let without_ext = rel.with_extension("");
            let rel_fqcn = without_ext
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(".");
            if rel_fqcn.is_empty() {
                continue;
            }
            let pkg_prefix = pkg_prefix_for_src_root(src_root);
            let fqcn = if pkg_prefix.is_empty() {
                rel_fqcn
            } else {
                format!("{}.{}", pkg_prefix, rel_fqcn)
            };
            return Some(fqcn);
        }
    }
    None
}

/// Returns `true` when the source text looks like a compact (unnamed-class)
/// source file — heuristically: no top-level `class`, `interface`, `enum`, or
/// `record` keyword outside comments.
fn is_compact_source(text: &str) -> bool {
    // Strip line comments then block comments.
    let no_line: String = text
        .lines()
        .map(|l| if let Some(i) = l.find("//") { &l[..i] } else { l })
        .collect::<Vec<_>>()
        .join("\n");

    let mut stripped = String::with_capacity(no_line.len());
    let mut chars = no_line.chars().peekable();
    let mut in_block = false;
    while let Some(ch) = chars.next() {
        if in_block {
            if ch == '*' && chars.peek() == Some(&'/') { chars.next(); in_block = false; }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next(); in_block = true;
        } else {
            stripped.push(ch);
        }
    }

    for kw in ["class", "interface", "enum", "record"] {
        let mut s = stripped.as_str();
        while let Some(idx) = s.find(kw) {
            let before = if idx == 0 { ' ' } else { s.as_bytes()[idx - 1] as char };
            let after_idx = idx + kw.len();
            let after = if after_idx >= s.len() { ' ' } else { s.as_bytes()[after_idx] as char };
            if !before.is_alphanumeric() && before != '_' && !after.is_alphanumeric() && after != '_' {
                return false;
            }
            s = &s[idx + kw.len()..];
        }
    }
    true
}

/// Returns `true` when the source text contains any recognisable main-method
/// signature under Java 21's flexible launch protocol.
fn source_has_main(text: &str) -> bool {
    // Compact / unnamed class: any `void main` is enough.
    if is_compact_source(text) && text.contains("void main") {
        return true;
    }
    // Static main (patterns 1 & 2).
    let flat = text.replace(['\n', '\r'], " ");
    if flat.contains("static") && flat.contains("void main") {
        return true;
    }
    // Instance main (patterns 3 & 4): any remaining `void main`.
    if flat.contains("void main") {
        return true;
    }
    false
}

/// Returns `true` when `javap` output contains a recognisable main-method
/// signature under Java 21's launch protocol.
fn javap_output_has_main(javap_out: &str) -> bool {
    for line in javap_out.lines() {
        let l = line.trim();
        // static void main(...) — with or without `public`
        if l.contains("static") && l.contains("void main(") {
            return true;
        }
        // instance void main(...) — non-private
        if l.contains("void main(") && !l.contains("private") {
            return true;
        }
    }
    false
}

/// Validate a declared or detected class name against compiled bytecode via
/// `javap`.  Returns `Ok(())` if the class has a launchable main method.
pub fn validate_main_class(
    class_name: &str,
    classes_dir: &Path,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut cp = classes_dir.to_string_lossy().into_owned();
    for jar in dep_jars {
        cp.push_str(sep);
        cp.push_str(&jar.to_string_lossy());
    }

    let output = Command::new("javap")
        .arg("-p")
        .arg("-classpath")
        .arg(&cp)
        .arg(class_name)
        .output()
        .context("failed to invoke javap — is a JDK installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "mainClass `{}` was not found in compiled output\n  {}",
            class_name,
            stderr.trim()
        );
    }

    let javap_out = String::from_utf8_lossy(&output.stdout);
    if !javap_output_has_main(&javap_out) {
        anyhow::bail!(
            "mainClass `{}` does not declare a launchable main method\n\
             \n\
             Expected one of:\n\
               public static void main(String[] args)\n\
               static void main()\n\
               void main(String[] args)   (instance, non-private)\n\
               void main()                (instance, non-private)",
            class_name
        );
    }
    Ok(())
}

/// Scan production sources for candidates then validate each against compiled
/// bytecode.  Returns the single detected class name, or an error.
pub fn detect_main_class(
    src_roots: &[PathBuf],
    sources: &[PathBuf],
    classes_dir: &Path,
    dep_jars: &[PathBuf],
) -> Result<String> {
    // Phase 1: fast source heuristic.
    let mut source_candidates: Vec<(String, PathBuf)> = Vec::new();
    for source in sources {
        let text = match std::fs::read_to_string(source) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if source_has_main(&text) {
            if let Some(fqcn) = fqcn_from_source(src_roots, source) {
                source_candidates.push((fqcn, source.clone()));
            }
        }
    }

    if source_candidates.is_empty() {
        anyhow::bail!(
            "no main method found in any production source file\n\
             \n\
             Add a main method to one of your classes, or declare it explicitly:\n\
             \n\
               # curie.toml\n\
               [application]\n\
               mainClass = \"com.example.YourMainClass\""
        );
    }

    // Phase 2: bytecode validation.
    let mut valid: Vec<String> = Vec::new();
    for (fqcn, _) in &source_candidates {
        if validate_main_class(fqcn, classes_dir, dep_jars).is_ok() {
            valid.push(fqcn.clone());
        }
    }

    match valid.len() {
        0 => anyhow::bail!(
            "no launchable main method found after bytecode inspection\n\
             \n\
             Source candidates that did not pass bytecode validation:\n\
             {}\n\
             \n\
             Declare the main class explicitly in curie.toml:\n\
             \n\
               [application]\n\
               mainClass = \"com.example.YourMainClass\"",
            source_candidates
                .iter()
                .map(|(n, _)| format!("  {}", n))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        1 => Ok(valid.remove(0)),
        _ => anyhow::bail!(
            "multiple classes with a main method found — declare one explicitly in curie.toml:\n\
             \n\
             {}\n\
             \n\
               # curie.toml\n\
               [application]\n\
               mainClass = \"com.example.YourChosenMainClass\"",
            valid
                .iter()
                .map(|n| format!("  {}", n))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

pub fn clean(project_root: &Path) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    println!(
        "Cleaning {} v{}",
        desc.project_name(), desc.project_version()
    );

    let target_dir = project_root.join("target");

    if target_dir.exists() {
        std::fs::remove_dir_all(&target_dir).with_context(|| {
            format!("failed to remove {}", target_dir.display())
        })?;
        println!("  Target dir      removed");
    } else {
        println!("  Target dir      nothing to clean");
    }

    Ok(())
}

#[cfg(test)]
mod clean_tests {
    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn clean_removes_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Write a minimal curie.toml so descriptor::load succeeds.
        std::fs::write(
            root.join("curie.toml"),
            "[application]\nname = \"test\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nsourceCompatibility = \"21\"\n",
        )
        .unwrap();

        let target = root.join("target");
        std::fs::create_dir_all(target.join("classes")).unwrap();
        std::fs::write(target.join("app.jar"), b"jar").unwrap();

        clean(root).unwrap();

        assert!(!root.join("target").exists());
    }

    #[test]
    fn clean_no_target_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("curie.toml"),
            "[application]\nname = \"test\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nsourceCompatibility = \"21\"\n",
        )
        .unwrap();

        // No target/ directory — should succeed without error.
        clean(root).unwrap();
    }

    // -----------------------------------------------------------------------
    // Manifest folding tests
    // -----------------------------------------------------------------------

    #[test]
    fn fold_manifest_header_short_value_fits_on_one_line() {
        // "Class-Path: libs/foo.jar\r\n" is 26 bytes — well under 72.
        let result = fold_manifest_header("Class-Path", "libs/foo.jar");
        assert_eq!(result, "Class-Path: libs/foo.jar\r\n");
    }

    #[test]
    fn fold_manifest_header_long_value_is_folded() {
        // Build a value that definitely exceeds 72 bytes on the first line.
        let value = "libs/aaaa.jar libs/bbbb.jar libs/cccc.jar libs/dddd.jar libs/eeee.jar libs/ffff.jar";
        let result = fold_manifest_header("Class-Path", value);
        for line in result.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(
                line.len() <= 70, // 70 bytes of content + \r\n = 72
                "line exceeds 70 bytes: {:?} ({} bytes)",
                line,
                line.len()
            );
        }
        // The folded result must round-trip: strip header name, join
        // continuation lines, and get back the original value.
        let reconstructed = result
            .split("\r\n")
            .filter(|l| !l.is_empty())
            .enumerate()
            .map(|(i, l)| if i == 0 { l["Class-Path: ".len()..].to_string() } else { l[1..].to_string() })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(reconstructed, value);
    }

    #[test]
    fn fold_manifest_header_empty_value() {
        let result = fold_manifest_header("Class-Path", "");
        assert_eq!(result, "Class-Path: \r\n");
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
}
