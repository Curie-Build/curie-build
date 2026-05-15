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
    /// Resolved production dependency JARs (empty when no [dependencies] declared).
    pub dep_jars: Vec<PathBuf>,
}

/// Phase 1: resolve production deps and compile production sources.
/// Does NOT run tests or package a JAR.
pub fn compile(
    project_root: &Path,
    desc: &descriptor::Descriptor,
) -> Result<CompileOutput> {
    // --- directories ---------------------------------------------------------
    let src_root = project_root.join("src").join("main").join("java");
    if !src_root.exists() {
        bail!("source directory not found: {}", src_root.display());
    }

    let classes_dir = project_root.join("target").join("classes");
    let output_dir = project_root.join("target");

    std::fs::create_dir_all(&classes_dir)
        .context("failed to create target/classes")?;

    // --- resolve production dependencies -------------------------------------
    let dep_jars = if desc.dependencies.is_empty() {
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
            },
        )
        .context("dependency resolution failed")?;

        println!("  Resolve deps    {} JAR(s)", jars.len());
        jars
    };

    // --- discover production sources (exclude test files) --------------------
    let mut sources: Vec<_> = WalkDir::new(&src_root)
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

    if sources.is_empty() {
        bail!("no Java source files found under {}", src_root.display());
    }

    sources.sort();

    // --- compile (incremental) -----------------------------------------------
    let toml_path = project_root.join("curie.toml");
    let compile_status = needs_recompile(&sources, &classes_dir, &toml_path);
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

        if !dep_jars.is_empty() {
            let cp = classpath_string(&dep_jars);
            javac.arg("-cp").arg(&cp);
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
    } else {
        println!("  Compile         up to date");
    }

    let jar_name = format!(
        "{}-{}.jar",
        desc.project_name(), desc.project_version()
    );
    let jar_path = output_dir.join(&jar_name);

    Ok(CompileOutput { jar_path, jar_name, classes_dir, dep_jars })
}

/// Phase 2: compile production sources, run tests, then package JAR.
pub fn do_build(
    project_root: &Path,
    desc: &descriptor::Descriptor,
) -> Result<BuildOutput> {
    let compiled = compile(project_root, desc)?;

    // --- run tests before packaging ------------------------------------------
    test::run_tests(project_root, desc, &compiled.classes_dir, &compiled.dep_jars, None)?;

    // --- package (deterministic JAR, incremental) ----------------------------
    if needs_repackage(&compiled.jar_path, &compiled.classes_dir) {
        println!("  Package         {}", compiled.jar_name);
        let main_class = desc.application.as_ref().map(|a| a.main_class.as_str());
        write_deterministic_jar(&compiled.jar_path, &compiled.classes_dir, main_class, &compiled.dep_jars)
            .context("failed to write JAR")?;
    } else {
        println!("  Package         up to date");
    }

    Ok(BuildOutput { jar: compiled.jar_path, dep_jars: compiled.dep_jars })
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
            CompileStatus::UpToDate => "up to date",
        }
    }
}

/// Returns the reason a recompile is (or is not) required.
pub(crate) fn needs_recompile(
    sources: &[PathBuf],
    classes_dir: &Path,
    toml_path: &Path,
) -> CompileStatus {
    let oldest_class = oldest_mtime_in_dir(classes_dir);
    if oldest_class == SystemTime::UNIX_EPOCH {
        return CompileStatus::NoClassFiles;
    }
    if newest_mtime(sources) > oldest_class {
        return CompileStatus::SourceChanged;
    }
    if mtime(toml_path) > oldest_class {
        return CompileStatus::TomlChanged;
    }
    CompileStatus::UpToDate
}

/// Returns `true` when the output JAR needs to be written: either it doesn't
/// exist yet, or at least one class file is newer than the JAR.
pub(crate) fn needs_repackage(jar_path: &Path, classes_dir: &Path) -> bool {
    let jar_mtime = mtime(jar_path);
    if jar_mtime == SystemTime::UNIX_EPOCH {
        return true;
    }
    // The newest class must not be newer than the jar.
    let newest_class = WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    newest_class > jar_mtime
}

/// Build an OS-appropriate classpath string from a list of JAR paths.
pub fn classpath_string(jars: &[PathBuf]) -> String {
    join_classpath(jars)
}

/// Build the `Class-Path` manifest value: space-separated list of JAR
/// filenames relative to the application JAR (they will sit in `libs/`).
fn manifest_class_path(dep_jars: &[PathBuf]) -> String {
    dep_jars
        .iter()
        .filter_map(|p| p.file_name())
        .map(|f| format!("libs/{}", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write a reproducible JAR:
///   • entries sorted lexicographically
///   • all timestamps set to EPOCH (2024-01-01 00:00:00 UTC)
///   • MANIFEST.MF written first (JAR spec requirement)
///   • Class-Path header added when dep_jars is non-empty
///   • no extra tool-specific metadata that embeds build time
fn write_deterministic_jar(
    jar_path: &Path,
    classes_dir: &Path,
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
        // The JAR spec requires the Class-Path value to be folded at 72 bytes.
        // For simplicity we write the full value and rely on the JVM's leniency.
        manifest.push_str(&format!(
            "Class-Path: {}\r\n",
            manifest_class_path(dep_jars)
        ));
    }
    manifest.push_str("\r\n");

    zip.start_file("META-INF/MANIFEST.MF", options)
        .context("failed to start MANIFEST.MF entry")?;
    zip.write_all(manifest.as_bytes())
        .context("failed to write MANIFEST.MF")?;

    // --- collect all class-file entries, sort them --------------------------
    let mut entries: Vec<(String, PathBuf)> = WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() || e.file_type().is_dir())
        .filter_map(|e| {
            let rel = e
                .path()
                .strip_prefix(classes_dir)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/"); // normalise on Windows
            if rel.is_empty() {
                return None; // skip classes_dir root itself
            }
            let zip_path = if e.file_type().is_dir() {
                format!("{}/", rel)
            } else {
                rel
            };
            Some((zip_path, e.into_path()))
        })
        .collect();

    // Stable sort: directories before their contents, then lexicographic.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml), CompileStatus::NoClassFiles);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml), CompileStatus::NoClassFiles);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml), CompileStatus::UpToDate);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml), CompileStatus::SourceChanged);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml), CompileStatus::TomlChanged);
    }

    // -- needs_repackage ------------------------------------------------------

    #[test]
    fn needs_repackage_no_jar() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("app.jar"); // does not exist
        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");

        assert!(needs_repackage(&jar, &classes_dir));
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

        assert!(!needs_repackage(&jar, &classes_dir));
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

        assert!(needs_repackage(&jar, &classes_dir));
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
}
