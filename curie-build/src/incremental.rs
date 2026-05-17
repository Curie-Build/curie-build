//! Incremental-build primitives shared by `compile`, `test`, `docker`,
//! and the JAR packaging step.
//!
//! Two flavours of check:
//!   - **mtime comparisons** for "is X newer than Y?" — the building block
//!     for all skip decisions.
//!   - **JDK fingerprint** via a stamp file, so that a `javac` upgrade
//!     triggers a full recompile regardless of source mtimes.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;

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

/// Return the newest `modified` time among all files under `dir`, or
/// `SystemTime::UNIX_EPOCH` when the directory is empty or doesn't exist.
pub(crate) fn newest_mtime_in_dir(dir: &Path) -> SystemTime {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .max()
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

    /// Set the mtime of `path` to `time`.
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
