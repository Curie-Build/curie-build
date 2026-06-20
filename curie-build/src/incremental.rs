//! Incremental-build primitives shared by `compile`, `test`, `docker`,
//! and the JAR packaging step.
//!
//! Three flavours of check:
//!   - **[`Stamp`] / [`Inputs`]** — the high-level "is this output covered
//!     by every input it depends on?" predicate.  All binary skip checks
//!     (test stamp, Docker stamp, JAR repackage) should go through this.
//!   - **Per-input mtime comparisons** ([`mtime`], [`newest_mtime`],
//!     [`oldest_class_mtime_in_dir`]) — building blocks used by `needs_recompile`,
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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use walkdir::{DirEntry, WalkDir};

/// Write (create or overwrite) a zero-byte stamp file at `path`.
///
/// The file's filesystem mtime is the "as-of" timestamp for incremental skip
/// checks.  We only care about mtime; the content is always empty.
pub(crate) fn touch_stamp(path: &Path) -> Result<()> {
    std::fs::write(path, b"")
        .with_context(|| format!("failed to write stamp file {}", path.display()))
}

/// Walk `dir` recursively and yield every regular file as a [`DirEntry`].
///
/// Errors from [`WalkDir`] (e.g. permission denied on a sub-entry) are
/// silently skipped — callers that need error visibility should use
/// [`WalkDir`] directly.  Missing or empty directories yield no entries.
pub(crate) fn walk_files(dir: &Path) -> impl Iterator<Item = DirEntry> + '_ {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
}

/// Return the `modified` time of `path`, or `SystemTime::UNIX_EPOCH` on any
/// error (missing file, unsupported platform). Treating errors as epoch means
/// the missing output is always considered stale.
pub(crate) fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Return the oldest `modified` time among `.class` files under `dir`, or
/// `SystemTime::UNIX_EPOCH` when no `.class` files exist or the directory
/// doesn't exist.
///
/// Restricting to `.class` files avoids false positives when annotation
/// processors (e.g. JMH) write non-class resources (BenchmarkList,
/// CompilerHints, …) into the classes directory: those resource files may
/// have an older mtime than the source files, causing the incremental check
/// to always conclude that recompilation is needed.
pub(crate) fn oldest_class_mtime_in_dir(dir: &Path) -> SystemTime {
    walk_files(dir)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".class"))
        .filter_map(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok())
        .min()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Return the newest `modified` time among all files under `dir`, or
/// `SystemTime::UNIX_EPOCH` when the directory is empty or doesn't exist.
pub(crate) fn newest_mtime_in_dir(dir: &Path) -> SystemTime {
    walk_files(dir)
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

// ---------------------------------------------------------------------------
// Atomic staging for build outputs (protects against truncated files on crash)
// ---------------------------------------------------------------------------

/// Per-process counter to make staging names unique even within one PID
/// (e.g. parallel threads or two `curie` invocations on the same project).
static NEXT_STAGING_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Return a unique collocated staging path for `dest`.
///
/// The result lives next to `dest` and includes the PID + a monotonic seq so
/// that concurrent writers (different processes or threads) never collide on
/// the temporary file. The original filename is preserved in the part name so
/// that a `.jar` and a `.pom` for the same artifact do not collide on their
/// staging files.
pub(crate) fn staging_path(dest: &Path) -> PathBuf {
    let pid = std::process::id();
    let seq = NEXT_STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
    let orig = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    dest.with_file_name(format!("{}.part.{}.{}", orig, pid, seq))
}

/// Rename `part` to `dest`. If the rename fails because `dest` already exists
/// (another writer won the race), treat it as success and remove our part file.
/// This is the same tolerant pattern used by wrapper.rs and the resolver.
pub(crate) fn finalize_staged(part: &Path, dest: &Path) -> Result<()> {
    match std::fs::rename(part, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            if dest.exists() {
                let _ = std::fs::remove_file(part);
                Ok(())
            } else {
                Err(e).with_context(|| {
                    format!("failed to rename {} \u{2192} {}", part.display(), dest.display())
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Source-set tracking
// ---------------------------------------------------------------------------
//
// mtime comparison cannot see *set-membership* changes: a source added with a
// preserved old timestamp (`mv`, `cp -p`, `rsync -a`, tar unpack) is not newer
// than any existing class, so a pure-mtime check reports "up to date" and never
// compiles it.  Symmetrically, a deletion leaves no newer mtime to notice.
//
// We close that gap by stamping the canonical source set after every successful
// compile and comparing it on the next build.  Any difference (addition OR
// deletion) forces a recompile.  The helpers are language-agnostic — the caller
// chooses which sources to track and which stamp file to use, so the same
// mechanism serves production sources, test sources, and packaged resources.

/// Canonicalise a slice of paths into a comparison set, dropping any that fail
/// to canonicalise (which only happens if the file vanished between discovery
/// and this call).  Canonical form matches the paths recorded elsewhere (e.g.
/// the class manifest), so sets compare consistently.
pub(crate) fn canonical_source_set(sources: &[PathBuf]) -> BTreeSet<String> {
    sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Path of a source-set stamp named `file_name` under `target_dir`.
pub(crate) fn source_set_stamp_path(target_dir: &Path, file_name: &str) -> PathBuf {
    target_dir.join(file_name)
}

/// Load a previously stamped source set, or `None` when the stamp is missing
/// (first build after clean, or this stamp was never written).
pub(crate) fn load_source_set(path: &Path) -> Option<BTreeSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    )
}

/// Write `set` (one canonical path per line) to the stamp at `path`.
pub(crate) fn write_source_set(path: &Path, set: &BTreeSet<String>) -> Result<()> {
    let mut body = String::with_capacity(set.iter().map(|p| p.len() + 1).sum());
    for p in set {
        body.push_str(p);
        body.push('\n');
    }
    std::fs::write(path, body)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// True when the current source set differs from the previously stamped one
/// (a source was added or removed).  A missing previous stamp reports `false`:
/// the very first build is driven by the no-class-files check instead, and we
/// must not force a recompile just because the stamp doesn't exist yet.
pub(crate) fn source_set_changed(
    previous: Option<&BTreeSet<String>>,
    current: &BTreeSet<String>,
) -> bool {
    previous.map(|p| p != current).unwrap_or(false)
}

/// Reason a recompile is required, or confirmation that it is not.
#[derive(Debug, PartialEq)]
pub(crate) enum CompileStatus {
    /// No `.class` files exist yet.
    NoClassFiles,
    /// At least one source file is newer than the oldest `.class` file.
    SourceChanged,
    /// The set of source files changed (a source was added or removed) since
    /// the last compile — caught even when mtimes don't reflect it.
    SourceSetChanged,
    /// A secondary input directory (e.g. production `classes` when deciding
    /// whether to recompile tests) is newer than the output classes.
    DependencyChanged,
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
            CompileStatus::SourceSetChanged => "source set changed",
            CompileStatus::DependencyChanged => "dependency changed",
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
/// Uses `>=` against `oldest_class_mtime_in_dir(classes_dir)` so a source edited
/// in the same filesystem-second as the oldest class file is still treated
/// as "changed".  See the module-level tie-breaking note.
///
/// `extra_input_dirs` may be used to model additional inputs that should
/// invalidate the outputs (e.g. pass the production `classes` directory when
/// deciding whether test sources need recompilation).
pub(crate) fn needs_recompile(
    sources: &[PathBuf],
    classes_dir: &Path,
    toml_path: &Path,
    target_dir: &Path,
    extra_input_dirs: &[&Path],
) -> CompileStatus {
    // Use only .class files as the baseline — annotation processors (e.g. JMH)
    // may write non-class resources (BenchmarkList, CompilerHints, …) into the
    // classes directory with older mtimes, which would otherwise make the oldest
    // file appear older than the sources and force a recompile every time.
    let oldest_class = oldest_class_mtime_in_dir(classes_dir);
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
    for &d in extra_input_dirs {
        if newest_mtime_in_dir(d) >= oldest_class {
            return CompileStatus::DependencyChanged;
        }
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

    // -- oldest_class_mtime_in_dir --------------------------------------------

    #[test]
    fn oldest_class_mtime_ignores_non_class_files() {
        // Regression: annotation processors (e.g. JMH) place resource files
        // such as META-INF/BenchmarkList into the classes directory.  If those
        // files have an older mtime than the source files, the incremental
        // check would always trigger a recompile.  oldest_class_mtime_in_dir
        // must ignore non-.class files.
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        // A resource file written long before the source edit.
        let resource = dir.path().join("BenchmarkList");
        write_file(&resource, b"resource");
        set_mtime(&resource, base);

        // A .class file that is newer than the resource but whose mtime we
        // want returned as the baseline.
        let class = dir.path().join("Foo.class");
        write_file(&class, b"class");
        set_mtime(&class, base + Duration::from_secs(120));

        // oldest_class_mtime_in_dir must return the .class mtime, not the
        // resource mtime.
        assert_eq!(
            oldest_class_mtime_in_dir(dir.path()),
            base + Duration::from_secs(120),
        );
    }

    #[test]
    fn oldest_class_mtime_no_classes_returns_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let resource = dir.path().join("BenchmarkList");
        write_file(&resource, b"resource");
        // No .class files — should behave as if the directory is empty.
        assert_eq!(oldest_class_mtime_in_dir(dir.path()), SystemTime::UNIX_EPOCH);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::NoClassFiles);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::NoClassFiles);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::UpToDate);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::SourceChanged);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::TomlChanged);
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

        assert_eq!(needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[]), CompileStatus::JdkChanged);
    }

    #[test]
    fn needs_recompile_true_when_extra_input_dir_newer_than_output() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let classes_dir = dir.path().join("test-classes");
        let class_file = classes_dir.join("FooTest.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        let src = dir.path().join("FooTest.java");
        write_file(&src, b"class FooTest {}");
        set_mtime(&src, base - Duration::from_secs(10));

        let toml = dir.path().join("Curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base - Duration::from_secs(20));

        if let Ok(v) = javac_version() {
            write_javac_version_stamp(dir.path(), &v).unwrap();
        }

        // Production classes newer than test outputs.
        let prod_dir = dir.path().join("classes");
        let prod_class = prod_dir.join("Foo.class");
        write_file(&prod_class, b"bytecode");
        set_mtime(&prod_class, base + Duration::from_secs(5));

        assert_eq!(
            needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[&prod_dir]),
            CompileStatus::DependencyChanged
        );
    }

    #[test]
    fn needs_recompile_false_when_extra_input_dir_older_than_output() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);

        let classes_dir = dir.path().join("test-classes");
        let class_file = classes_dir.join("FooTest.class");
        write_file(&class_file, b"bytecode");
        set_mtime(&class_file, base);

        let src = dir.path().join("FooTest.java");
        write_file(&src, b"class FooTest {}");
        set_mtime(&src, base - Duration::from_secs(10));

        let toml = dir.path().join("Curie.toml");
        write_file(&toml, b"[application]");
        set_mtime(&toml, base - Duration::from_secs(20));

        if let Ok(v) = javac_version() {
            write_javac_version_stamp(dir.path(), &v).unwrap();
        }

        let prod_dir = dir.path().join("classes");
        let prod_class = prod_dir.join("Foo.class");
        write_file(&prod_class, b"bytecode");
        set_mtime(&prod_class, base - Duration::from_secs(5));

        assert_eq!(
            needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[&prod_dir]),
            CompileStatus::UpToDate
        );
    }

    #[test]
    fn needs_recompile_no_class_files_even_with_extra_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("FooTest.java");
        write_file(&src, b"class FooTest {}");
        let classes_dir = dir.path().join("test-classes"); // does not exist
        let toml = dir.path().join("Curie.toml");
        write_file(&toml, b"[application]");

        let prod_dir = dir.path().join("classes");
        write_file(&prod_dir.join("Foo.class"), b"bytecode");

        assert_eq!(
            needs_recompile(&[src], &classes_dir, &toml, dir.path(), &[&prod_dir]),
            CompileStatus::NoClassFiles
        );
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

    // -- source-set tracking -------------------------------------------------

    fn set_of(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn source_set_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = source_set_stamp_path(dir.path(), ".sources");
        let set = set_of(&["/a/Foo.java", "/a/Bar.kt"]);
        write_source_set(&path, &set).unwrap();
        assert_eq!(load_source_set(&path).unwrap(), set);
    }

    #[test]
    fn source_set_load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_source_set(&source_set_stamp_path(dir.path(), ".sources")).is_none());
    }

    #[test]
    fn source_set_load_ignores_blank_lines_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = source_set_stamp_path(dir.path(), ".sources");
        std::fs::write(&path, "\n  /a/Foo.java  \n\n/a/Bar.java\n").unwrap();
        assert_eq!(load_source_set(&path).unwrap(), set_of(&["/a/Foo.java", "/a/Bar.java"]));
    }

    #[test]
    fn source_set_changed_detects_addition() {
        let prev = set_of(&["/a/Foo.java"]);
        let now = set_of(&["/a/Foo.java", "/a/Bar.java"]); // Bar added
        assert!(source_set_changed(Some(&prev), &now));
    }

    #[test]
    fn source_set_changed_detects_deletion() {
        let prev = set_of(&["/a/Foo.java", "/a/Bar.java"]);
        let now = set_of(&["/a/Foo.java"]); // Bar removed
        assert!(source_set_changed(Some(&prev), &now));
    }

    #[test]
    fn source_set_changed_false_when_equal() {
        let set = set_of(&["/a/Foo.java", "/a/Bar.java"]);
        assert!(!source_set_changed(Some(&set), &set.clone()));
    }

    #[test]
    fn source_set_changed_false_when_no_previous_stamp() {
        // First build has no stamp — must NOT force a recompile on that basis;
        // the no-class-files path drives the initial compile instead.
        let now = set_of(&["/a/Foo.java"]);
        assert!(!source_set_changed(None, &now));
    }

    #[test]
    fn canonical_source_set_drops_uncanonicalizable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Real.java");
        write_file(&real, b"class Real {}");
        let ghost = dir.path().join("Ghost.java"); // never created
        let set = canonical_source_set(&[real.clone(), ghost]);
        assert_eq!(set.len(), 1, "only the existing file canonicalizes");
        assert!(set.iter().next().unwrap().ends_with("Real.java"));
    }

    #[test]
    fn compile_status_source_set_changed_reason_and_needs_recompile() {
        assert_eq!(CompileStatus::SourceSetChanged.reason(), "source set changed");
        assert!(CompileStatus::SourceSetChanged.needs_recompile());
    }

    // -- atomic staging helpers ----------------------------------------------

    #[test]
    fn staging_path_is_sibling_and_unique() {
        let dest = Path::new("/tmp/target/foo.jar");
        let p1 = staging_path(dest);
        let p2 = staging_path(dest);

        assert!(p1.starts_with("/tmp/target"));
        assert!(p1.file_name().unwrap().to_string_lossy().contains("foo.jar.part."));
        assert_ne!(p1, p2, "consecutive calls must produce distinct staging names");
    }

    #[test]
    fn finalize_staged_success_moves_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.jar");
        let part = dir.path().join("out.jar.part.test");
        write_file(&part, b"complete");

        finalize_staged(&part, &dest).unwrap();
        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete");
        assert!(!part.exists());
    }

    #[test]
    fn finalize_staged_tolerates_dest_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.jar");
        write_file(&dest, b"winner");

        let part = dir.path().join("out.jar.part.loser");
        write_file(&part, b"loser-content");

        // On Unix rename replaces; the tolerant path is exercised on platforms
        // or races where rename returns error but dest now exists. We at least
        // verify we never leave the part behind and dest ends up with complete bytes.
        finalize_staged(&part, &dest).unwrap();
        // Either content is acceptable — what matters is that a *complete* file
        // from one of the writers is present and our staging file is gone.
        let final_bytes = std::fs::read(&dest).unwrap();
        assert!(final_bytes == b"winner" || final_bytes == b"loser-content");
        assert!(!part.exists());
    }

    #[test]
    fn finalize_staged_errors_if_neither_exists_after_failure() {
        // Hard to simulate rename failure without the dest appearing, but we can at
        // least ensure the error path is reachable by attempting rename of a
        // non-existent part (the helper will propagate the underlying error).
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("ghost.part");
        let dest = dir.path().join("never-created");

        let res = finalize_staged(&part, &dest);
        assert!(res.is_err());
    }
}
