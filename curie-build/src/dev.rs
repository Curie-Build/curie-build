//! `curie dev` — run the application in exploded form with source watching.
//!
//! Instead of packaging a JAR, dev mode compiles to `target/classes/` and
//! launches the JVM with a `-cp` that points directly at that directory plus
//! the resolved dependency JARs — exactly how IDEs launch Java apps.
//!
//! After the initial launch, the process watches `src/` and `Curie.toml` for
//! changes.  On Linux, inotify provides event-driven, near-zero-latency
//! notifications.  On other platforms (and as a fallback when inotify fails
//! to initialise), the watcher polls mtime every 300 ms.
//!
//! When any change is detected, the running process is killed, sources are
//! recompiled, and the app restarts.  On a compile failure the process stays
//! down and curie keeps watching so the user can fix the error and save again.

use crate::{compile, descriptor, incremental, jar, main_class, resources, style};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_millis(300);

pub struct DevOptions {
    pub offline: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_dev(project_root: &Path, opts: DevOptions, extra_args: &[String]) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    let app = desc.application().ok_or_else(|| {
        anyhow::anyhow!("`curie dev` is only supported for application projects")
    })?;

    let enable_preview = desc.java.preview_enabled();

    println!("{}", style::headline("Dev", &app.name, &app.version));

    let compiled = compile::compile(project_root, &desc, opts.offline, &[])?;
    let main = resolve_main_class(app, &compiled)?;
    let resources_dir = dev_resources_dir(project_root, &desc, &compiled);
    let classpath = exploded_classpath(&compiled, resources_dir.as_deref());

    let agent_coords = desc.dep_java_agent_coords();
    let agent_jars = crate::java_agent::find_agent_jars(&agent_coords, &classpath);

    let mut proc = Some(spawn_app(&main, &classpath, enable_preview, &agent_jars, extra_args)?);
    println!("{}", style::dev_step("Watching", &main));
    println!();

    let mut watcher = ChangeWatcher::create(project_root);

    loop {
        reap_if_exited(&mut proc);

        if !watcher.wait_for_change(POLL_INTERVAL) {
            continue;
        }
        watcher.acknowledge();

        kill_and_wait(&mut proc);

        println!("{}", style::dev_step("Restarting", "source changed"));

        match compile::compile(project_root, &desc, opts.offline, &[]) {
            Ok(new_compiled) => {
                let new_rd = dev_resources_dir(project_root, &desc, &new_compiled);
                let new_cp = exploded_classpath(&new_compiled, new_rd.as_deref());
                match spawn_app(&main, &new_cp, enable_preview, &agent_jars, extra_args) {
                    Ok(child) => {
                        proc = Some(child);
                        println!("{}", style::dev_step("Watching", &main));
                        println!();
                    }
                    Err(e) => eprintln!("error: {e:#}"),
                }
            }
            Err(e) => eprintln!("error: {e:#}"),
        }
    }
}

// ── Classpath ─────────────────────────────────────────────────────────────────

/// Build the exploded classpath: classes dir first, then resources (if
/// present), then dep JARs, then language runtime JARs (Kotlin, Groovy).
///
/// `resources_dir` is the *effective* directory — the filtered output when a
/// `[resources]` scope is active, else the raw source dir — so `dev` and `run`
/// read identical resource values.
fn exploded_classpath(compiled: &compile::CompileOutput, resources_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut cp = vec![compiled.classes_dir.clone()];

    if let Some(rd) = resources_dir {
        if rd.exists() {
            cp.push(rd.to_path_buf());
        }
    }

    cp.extend_from_slice(&compiled.dep_jars);
    cp.extend_from_slice(&compiled.kotlin_stdlib_jars);
    cp.extend_from_slice(&compiled.groovy_jars);
    cp
}

/// Run resource filtering for the dev loop and return the effective main
/// resources dir (filtered output when active, else the raw source dir).  A
/// filtering error is reported but does not crash the watch loop — dev falls
/// back to the raw dir so the user can keep iterating.
fn dev_resources_dir(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    compiled: &compile::CompileOutput,
) -> Option<PathBuf> {
    let target_dir = compiled.classes_dir.parent().unwrap_or(project_root);
    let git_commit = crate::git::detect(project_root).map(|i| i.commit_id);
    match resources::process_resources(
        project_root,
        desc,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        git_commit.as_deref(),
        target_dir,
    ) {
        Ok(out) => out.main_dir.or_else(|| compiled.resources_dir.clone()),
        Err(e) => {
            eprintln!("warning: resource filtering failed: {e:#}");
            compiled.resources_dir.clone()
        }
    }
}

// ── Main class ────────────────────────────────────────────────────────────────

/// Resolve the main class: validate the declared value if present, or
/// auto-detect it from compiled bytecode via `main_class::detect_main_class`.
fn resolve_main_class(
    app: &descriptor::Application,
    compiled: &compile::CompileOutput,
) -> Result<String> {
    if let Some(declared) = app.main_class.as_deref() {
        main_class::validate_main_class(declared, &compiled.classes_dir, &compiled.dep_jars)?;
        return Ok(declared.to_string());
    }
    main_class::detect_main_class(
        &compiled.src_roots,
        &compiled.sources,
        &compiled.classes_dir,
        &compiled.dep_jars,
    )
}

// ── Process management ────────────────────────────────────────────────────────

fn spawn_app(
    main: &str,
    classpath: &[PathBuf],
    enable_preview: bool,
    agent_jars: &[PathBuf],
    extra_args: &[String],
) -> Result<Child> {
    let mut cmd = Command::new("java");
    if enable_preview {
        cmd.arg("--enable-preview");
    }
    for agent in agent_jars {
        cmd.arg(format!("-javaagent:{}", agent.display()));
    }
    cmd.arg("-cp").arg(jar::classpath_string(classpath));
    cmd.arg(main);
    cmd.args(extra_args);
    cmd.spawn().context("failed to invoke java — is a JRE installed?")
}

/// If the child has already exited on its own, print its exit status and
/// clear the slot so subsequent polls don't try to kill a zombie.
fn reap_if_exited(proc: &mut Option<Child>) {
    if let Some(ref mut child) = proc {
        if let Ok(Some(status)) = child.try_wait() {
            let code = status.code().unwrap_or(1);
            if code != 0 {
                eprintln!("  app exited (code {code}); watching for changes...");
            } else {
                println!("  app exited; watching for changes...");
            }
            *proc = None;
        }
    }
}

/// Kill the running child (if any) and wait for it to fully exit.
fn kill_and_wait(proc: &mut Option<Child>) {
    if let Some(mut child) = proc.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ── Change detection ──────────────────────────────────────────────────────────

/// Unified watcher: inotify on Linux, mtime polling everywhere else.
struct ChangeWatcher {
    inner: WatcherInner,
}

enum WatcherInner {
    #[cfg(target_os = "linux")]
    Inotify(std::sync::mpsc::Receiver<()>),
    Poll {
        project_root: PathBuf,
        last_check: SystemTime,
    },
}

impl ChangeWatcher {
    fn create(project_root: &Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            match start_inotify_thread(project_root) {
                Ok(rx) => {
                    return ChangeWatcher {
                        inner: WatcherInner::Inotify(rx),
                    };
                }
                Err(_) => {
                    // inotify unavailable (container without user-namespaces, etc.) — fall through
                }
            }
        }
        ChangeWatcher {
            inner: WatcherInner::Poll {
                project_root: project_root.to_path_buf(),
                last_check: SystemTime::now(),
            },
        }
    }

    /// Block for up to `timeout`.  Returns `true` the moment a source change
    /// is detected; `false` when the timeout expires with no change.
    fn wait_for_change(&mut self, timeout: Duration) -> bool {
        match &mut self.inner {
            #[cfg(target_os = "linux")]
            WatcherInner::Inotify(rx) => {
                rx.recv_timeout(timeout).is_ok()
            }
            WatcherInner::Poll { project_root, last_check } => {
                std::thread::sleep(timeout);
                inputs_changed_since(project_root, *last_check)
            }
        }
    }

    /// Reset the watcher state after a change has been handled.
    ///
    /// For the poll watcher this advances `last_check` to the current time so
    /// the same mtime change is not reported a second time.  For inotify the
    /// kernel event queue is already consumed, so this is a no-op.
    fn acknowledge(&mut self) {
        if let WatcherInner::Poll { last_check, .. } = &mut self.inner {
            *last_check = SystemTime::now();
        }
    }
}

/// Returns `true` when any file under `<project_root>/src/` or
/// `<project_root>/Curie.toml` is strictly newer than `since`.
fn inputs_changed_since(project_root: &Path, since: SystemTime) -> bool {
    let src_dir = project_root.join("src");
    let toml_path = project_root.join("Curie.toml");
    incremental::newest_mtime_in_dir(&src_dir) > since
        || incremental::mtime(&toml_path) > since
}

// ── inotify watcher (Linux only) ──────────────────────────────────────────────

/// Spawn a background thread that watches `src/` recursively and
/// `Curie.toml` via inotify.  Sends `()` on the returned receiver whenever
/// any source file changes.  Errors from the background thread are silent —
/// the thread simply exits, causing the channel to become disconnected and
/// `recv_timeout` to return an error, which `wait_for_change` treats as a
/// spurious timeout.
#[cfg(target_os = "linux")]
fn start_inotify_thread(project_root: &Path) -> Result<std::sync::mpsc::Receiver<()>> {
    use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
    use std::collections::HashMap;

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let project_root = project_root.to_path_buf();

    std::thread::spawn(move || -> anyhow::Result<()> {
        let mut inotify = Inotify::init()?;
        let mut wd_paths: HashMap<WatchDescriptor, PathBuf> = HashMap::new();

        let mask = WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO;

        // Watch the project root so that edits to Curie.toml are caught.
        let wd = inotify.watches().add(&project_root, mask)?;
        wd_paths.insert(wd, project_root.clone());

        // Recursively watch every existing subdirectory of src/.
        let src_dir = project_root.join("src");
        if src_dir.exists() {
            watch_dir_recursive(&mut inotify, &src_dir, mask, &mut wd_paths)?;
        }

        let mut buf = vec![0u8; 4096];

        loop {
            // Collect events into owned data so we can mutate `inotify`
            // afterwards to add watches for newly created directories.
            let owned: Vec<(WatchDescriptor, EventMask, Option<std::ffi::OsString>)> = inotify
                .read_events_blocking(&mut buf)?
                .map(|e| (e.wd, e.mask, e.name.map(|n| n.to_os_string())))
                .collect();

            let mut source_changed = false;

            for (wd, event_mask, name) in owned {
                let is_dir = event_mask.contains(EventMask::ISDIR);
                let is_new_dir = is_dir
                    && (event_mask.contains(EventMask::CREATE)
                        || event_mask.contains(EventMask::MOVED_TO));

                if is_new_dir {
                    // Add a watch for the new directory so files created
                    // inside it are also tracked.
                    if let (Some(parent), Some(name)) = (wd_paths.get(&wd), name) {
                        let new_dir = parent.join(&name);
                        if let Ok(new_wd) = inotify.watches().add(&new_dir, mask) {
                            wd_paths.insert(new_wd, new_dir);
                        }
                    }
                } else if !is_dir {
                    source_changed = true;
                }
            }

            if source_changed {
                // Non-blocking send; if the receiver is behind, older signals
                // are already sufficient — no need to queue duplicates.
                let _ = tx.send(());
            }
        }
    });

    Ok(rx)
}

/// Walk `dir` recursively and add an inotify watch for every subdirectory
/// (including `dir` itself).
#[cfg(target_os = "linux")]
fn watch_dir_recursive(
    inotify: &mut inotify::Inotify,
    dir: &Path,
    mask: inotify::WatchMask,
    wd_paths: &mut std::collections::HashMap<inotify::WatchDescriptor, PathBuf>,
) -> Result<()> {
    use walkdir::WalkDir;
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_dir() {
            if let Ok(wd) = inotify.watches().add(entry.path(), mask) {
                wd_paths.insert(wd, entry.path().to_path_buf());
            }
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fake_compile_output(
        classes_dir: PathBuf,
        dep_jars: Vec<PathBuf>,
        kotlin_stdlib_jars: Vec<PathBuf>,
        groovy_jars: Vec<PathBuf>,
        resources_dir: Option<PathBuf>,
    ) -> compile::CompileOutput {
        compile::CompileOutput {
            jar_path: classes_dir.join("app.jar"),
            jar_name: "app.jar".to_string(),
            classes_dir,
            src_roots: vec![],
            sources: vec![],
            dep_jars,
            kotlin_stdlib_jars,
            groovy_jars,
            resources_dir,
            test_resources_dir: None,
            is_modular: false,
            module_name: None,
            module_path_jars: vec![],
        }
    }

    fn write_with_mtime(path: &Path, mtime: SystemTime) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    fn poll_watcher(project_root: PathBuf, last_check: SystemTime) -> ChangeWatcher {
        ChangeWatcher {
            inner: WatcherInner::Poll { project_root, last_check },
        }
    }

    // -- exploded_classpath ---------------------------------------------------

    #[test]
    fn classpath_starts_with_classes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let compiled = fake_compile_output(classes.clone(), vec![], vec![], vec![], None);
        let cp = exploded_classpath(&compiled, compiled.resources_dir.as_deref());
        assert_eq!(cp[0], classes);
    }

    #[test]
    fn classpath_includes_dep_and_runtime_jars() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let dep = dir.path().join("dep.jar");
        let kotlin = dir.path().join("kotlin-stdlib.jar");
        let groovy = dir.path().join("groovy.jar");
        let compiled = fake_compile_output(
            classes.clone(),
            vec![dep.clone()],
            vec![kotlin.clone()],
            vec![groovy.clone()],
            None,
        );
        let cp = exploded_classpath(&compiled, compiled.resources_dir.as_deref());
        assert!(cp.contains(&dep));
        assert!(cp.contains(&kotlin));
        assert!(cp.contains(&groovy));
    }

    #[test]
    fn classpath_includes_existing_resources_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let resources = dir.path().join("resources");
        std::fs::create_dir_all(&resources).unwrap();
        let compiled =
            fake_compile_output(classes.clone(), vec![], vec![], vec![], Some(resources.clone()));
        let cp = exploded_classpath(&compiled, compiled.resources_dir.as_deref());
        assert!(cp.contains(&resources));
    }

    #[test]
    fn classpath_omits_nonexistent_resources_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let resources = dir.path().join("does-not-exist");
        let compiled =
            fake_compile_output(classes.clone(), vec![], vec![], vec![], Some(resources.clone()));
        let cp = exploded_classpath(&compiled, compiled.resources_dir.as_deref());
        assert!(!cp.contains(&resources));
    }

    // -- inputs_changed_since -------------------------------------------------

    #[test]
    fn detects_source_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base);

        let src = dir.path().join("src").join("Hello.java");
        write_with_mtime(&src, base + Duration::from_secs(10));

        assert!(inputs_changed_since(dir.path(), base));
        assert!(!inputs_changed_since(dir.path(), base + Duration::from_secs(10)));
    }

    #[test]
    fn detects_toml_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base + Duration::from_secs(5));

        assert!(inputs_changed_since(dir.path(), base));
    }

    #[test]
    fn no_change_when_all_inputs_older() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src").join("Hello.java");
        write_with_mtime(&src, base);
        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base);

        assert!(!inputs_changed_since(dir.path(), base + Duration::from_secs(10)));
    }

    // -- ChangeWatcher (poll path) -------------------------------------------

    #[test]
    fn poll_watcher_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src").join("Hello.java");
        write_with_mtime(&src, base + Duration::from_secs(1));
        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base);

        let mut w = poll_watcher(dir.path().to_path_buf(), base);
        assert!(w.wait_for_change(Duration::from_millis(1)));
    }

    #[test]
    fn poll_watcher_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src").join("Hello.java");
        write_with_mtime(&src, base);
        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base);

        // last_check is after both files → no change
        let mut w = poll_watcher(dir.path().to_path_buf(), base + Duration::from_secs(10));
        assert!(!w.wait_for_change(Duration::from_millis(1)));
    }

    #[test]
    fn poll_watcher_acknowledge_prevents_double_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);

        let src = dir.path().join("src").join("Hello.java");
        write_with_mtime(&src, base + Duration::from_secs(1));
        let toml = dir.path().join("Curie.toml");
        write_with_mtime(&toml, base);

        let mut w = poll_watcher(dir.path().to_path_buf(), base);

        // First call detects the change.
        assert!(w.wait_for_change(Duration::from_millis(1)));

        // After acknowledge, last_check advances to ~now; the file's mtime
        // (base + 1s ≈ year 1970) is far in the past → no second trigger.
        w.acknowledge();
        assert!(!w.wait_for_change(Duration::from_millis(1)));
    }
}
