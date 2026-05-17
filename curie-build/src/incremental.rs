//! Incremental-build primitives shared by `compile`, `test`, `docker`,
//! and the JAR packaging step.
//!
//! Three flavours of check:
//!   - **[`Stamp`] / [`Inputs`]** — the high-level "is this output covered
//!     by every input it depends on?" predicate.  All binary skip checks
//!     (test stamp, Docker stamp, JAR repackage) should go through this.
//!   - **Per-input mtime comparisons** ([`mtime`], [`newest_mtime`],
//!     [`oldest_mtime_in_dir`]) — building blocks used by `needs_recompile`,
//!     where the return value distinguishes *which* input forced a rebuild.
//!   - **JDK fingerprint** via a stamp file, so that a `javac` upgrade
//!     triggers a full recompile regardless of source mtimes.
//!
//! # Tie-breaking
//!
//! Every comparison in this module treats `input_mtime == stamp_mtime` as
//! *out-of-date* (i.e. rebuild).  Filesystem mtime resolution varies — ext4
//! with nanoseconds on a developer laptop, second-resolution on FAT, on
//! cache-restored CI workspaces, on some NFS mounts, and inside Docker
//! bind-mounts — and a build that writes its stamp in the same second the
//! user edited a source must not silently mask the edit.  False positives
//! (a no-op rebuild on a fast machine) are tolerable; false negatives are
//! not.

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

/// A successful build's "as-of" mtime.  Holds either the mtime of a stamp
/// file (or output JAR / output directory) or `None` when the stamp doesn't
/// exist — which always reports as out-of-date.
///
/// Use [`Stamp::of`] for a single-file stamp (`.test-stamp`, `.docker-stamp`,
/// the output JAR) and [`Stamp::oldest_in_dir`] when the "stamp" is an
/// output directory (the oldest class file in `target/classes`).
#[derive(Copy, Clone, Debug)]
pub(crate) struct Stamp(Option<SystemTime>);

impl Stamp {
    /// Read the stamp from a single file's mtime.  Missing/unreadable
    /// files report `None` so [`covers`](Self::covers) returns false.
    pub(crate) fn of(path: &Path) -> Self {
        Self(std::fs::metadata(path).and_then(|m| m.modified()).ok())
    }

    /// True iff the stamp exists AND every observed input is **strictly
    /// older** than the stamp.  See the module-level note on tie-breaking.
    pub(crate) fn covers(&self, inputs: &Inputs) -> bool {
        match (self.0, inputs.newest()) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(s), Some(i)) => i < s,
        }
    }
}

/// Accumulator for input mtimes.  Tracks only the running maximum — call
/// sites don't care which input was newest, only whether it beat the stamp.
///
/// Builder methods return `&mut Self` so calls chain.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Inputs(SystemTime);

impl Inputs {
    pub(crate) fn new() -> Self {
        Self(SystemTime::UNIX_EPOCH)
    }

    /// Observe a single file.  Missing files contribute nothing.
    pub(crate) fn add_file(&mut self, path: &Path) -> &mut Self {
        self.bump(mtime(path))
    }

    /// Observe the newest file under `dir` (recursively).  Missing or
    /// empty directories contribute nothing.
    pub(crate) fn add_dir(&mut self, dir: &Path) -> &mut Self {
        self.bump(newest_mtime_in_dir(dir))
    }

    /// Observe `add_dir(dir)` only when the option is `Some`.
    pub(crate) fn add_dir_opt(&mut self, dir: Option<&Path>) -> &mut Self {
        if let Some(d) = dir {
            self.add_dir(d);
        }
        self
    }

    /// Observe the newest mtime among an explicit list of paths.
    pub(crate) fn add_paths(&mut self, paths: &[PathBuf]) -> &mut Self {
        self.bump(newest_mtime(paths))
    }

    fn bump(&mut self, t: SystemTime) -> &mut Self {
        if t > self.0 {
            self.0 = t;
        }
        self
    }

    /// Newest observed mtime, or `None` if no input contributed a real
    /// timestamp (everything was missing/empty).
    pub(crate) fn newest(&self) -> Option<SystemTime> {
        (self.0 != SystemTime::UNIX_EPOCH).then_some(self.0)
    }
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
    /// `Curie.toml` is newer than the oldest `.class` file.
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
            CompileStatus::TomlChanged => "Curie.toml changed",
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
///
/// Uses `>=` against `oldest_mtime_in_dir(classes_dir)` so a source edited
/// in the same filesystem-second as the oldest class file is still treated
/// as "changed".  See the module-level tie-breaking note.
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
    if newest_mtime(sources) >= oldest_class {
        return CompileStatus::SourceChanged;
    }
    if mtime(toml_path) >= oldest_class {
        return CompileStatus::TomlChanged;
    }
    CompileStatus::UpToDate
}

/// Returns `true` when the output JAR needs to be written: either it doesn't
/// exist yet, or any input (class file, resource file, or `Curie.toml` —
/// which influences the JAR manifest via mainClass) is newer than the JAR.
pub(crate) fn needs_repackage(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    toml_path: &Path,
) -> bool {
    let mut inputs = Inputs::new();
    inputs
        .add_dir(classes_dir)
        .add_dir_opt(resources_dir)
        .add_file(toml_path);
    !Stamp::of(jar_path).covers(&inputs)
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
        let toml = dir.path().join("Curie.toml");
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
        let toml = dir.path().join("Curie.toml");
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

        let toml = dir.path().join("Curie.toml");
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

        let toml = dir.path().join("Curie.toml");
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

        // Curie.toml changed after last compile
        let toml = dir.path().join("Curie.toml");
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

        let toml = dir.path().join("Curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base);

        // Write a *different* javac version to simulate a JDK upgrade.
        write_javac_version_stamp(dir.path(), "javac 99.0.0").unwrap();

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path()), CompileStatus::JdkChanged);
    }

    // -- needs_repackage ------------------------------------------------------

    /// `needs_repackage` requires a Curie.toml path. Most tests only exercise
    /// the class/resource paths; a non-existent placeholder contributes
    /// nothing to `Inputs` (mtime returns UNIX_EPOCH).
    fn placeholder_toml(dir: &Path) -> PathBuf {
        dir.join("does-not-exist.toml")
    }

    #[test]
    fn needs_repackage_no_jar() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("app.jar"); // does not exist
        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        let missing_toml = placeholder_toml(dir.path());

        assert!(needs_repackage(&jar, &classes_dir, None, &missing_toml));
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
        let missing_toml = placeholder_toml(dir.path());

        assert!(!needs_repackage(&jar, &classes_dir, None, &missing_toml));
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
        let missing_toml = placeholder_toml(dir.path());

        assert!(needs_repackage(&jar, &classes_dir, None, &missing_toml));
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
        let missing_toml = placeholder_toml(dir.path());

        assert!(needs_repackage(&jar, &classes_dir, Some(&resources_dir), &missing_toml));
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
        let missing_toml = placeholder_toml(dir.path());

        assert!(!needs_repackage(&jar, &classes_dir, Some(&resources_dir), &missing_toml));
    }

    /// B4: a change to Curie.toml (e.g. `[application] mainClass`) must
    /// invalidate the JAR even when no class file changed.
    #[test]
    fn needs_repackage_true_when_toml_newer_than_jar() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let classes_dir = dir.path().join("classes");
        let class_file = classes_dir.join("Foo.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base - Duration::from_secs(10));

        let jar = dir.path().join("app.jar");
        write_file(&jar, b"jar");
        set_mtime(&jar, base);

        let toml = dir.path().join("Curie.toml");
        write_file(&toml, b"[application]");
        // toml edited after the JAR was packaged
        set_mtime(&toml, base + Duration::from_secs(5));

        assert!(needs_repackage(&jar, &classes_dir, None, &toml));
    }

    // -- Stamp / Inputs ------------------------------------------------------

    #[test]
    fn stamp_missing_never_covers() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = Stamp::of(&dir.path().join("ghost"));
        let mut inputs = Inputs::new();
        inputs.add_file(&dir.path().join("also-missing"));
        assert!(!stamp.covers(&inputs));
    }

    #[test]
    fn stamp_with_no_inputs_covers() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("stamp");
        write_file(&s, b"");
        assert!(Stamp::of(&s).covers(&Inputs::new()));
    }

    #[test]
    fn stamp_strictly_newer_covers() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src");
        write_file(&src, b"");
        set_mtime(&src, base);

        let stamp = dir.path().join("stamp");
        write_file(&stamp, b"");
        set_mtime(&stamp, base + Duration::from_secs(1));

        let mut inputs = Inputs::new();
        inputs.add_file(&src);
        assert!(Stamp::of(&stamp).covers(&inputs));
    }

    /// The Layer-1 fix: a tied mtime (same filesystem-second) must NOT
    /// count as covered.  On second-resolution filesystems a fast TDD loop
    /// can edit-test-edit-test all within one second; the old `>` check
    /// silently masked the second edit.
    #[test]
    fn stamp_tied_mtime_does_not_cover() {
        let dir = tempfile::tempdir().unwrap();
        let same = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src");
        write_file(&src, b"");
        set_mtime(&src, same);

        let stamp = dir.path().join("stamp");
        write_file(&stamp, b"");
        set_mtime(&stamp, same); // exact tie

        let mut inputs = Inputs::new();
        inputs.add_file(&src);
        assert!(
            !Stamp::of(&stamp).covers(&inputs),
            "tied input mtime must NOT count as covered (would mask edits on second-resolution fs)",
        );
    }

    #[test]
    fn inputs_add_dir_picks_newest_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);
        let sub = dir.path().join("d");
        write_file(&sub.join("a"), b"");
        set_mtime(&sub.join("a"), base);
        write_file(&sub.join("b"), b"");
        set_mtime(&sub.join("b"), base + Duration::from_secs(7));

        let mut inputs = Inputs::new();
        inputs.add_dir(&sub);
        assert_eq!(inputs.newest(), Some(base + Duration::from_secs(7)));
    }

    #[test]
    fn inputs_add_dir_opt_none_is_noop() {
        let mut inputs = Inputs::new();
        inputs.add_dir_opt(None);
        assert_eq!(inputs.newest(), None);
    }
}
