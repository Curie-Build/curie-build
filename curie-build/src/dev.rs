//! `curie dev` — run the application in exploded form with source watching.
//!
//! Instead of packaging a JAR, dev mode compiles to `target/classes/` and
//! launches the JVM with a `-cp` that points directly at that directory plus
//! the resolved dependency JARs — exactly how IDEs launch Java apps.
//!
//! After the initial launch, the process polls `src/` and `Curie.toml` every
//! 300 ms.  When any file changes, the running process is killed, sources are
//! recompiled, and the app restarts.  On a compile failure the process stays
//! down and curie keeps watching so the user can fix the error and save again.

use crate::{compile, descriptor, incremental, jar, main_class, style};
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
    let classpath = exploded_classpath(&compiled);

    let mut proc = Some(spawn_app(&main, &classpath, enable_preview, extra_args)?);
    println!("{}", style::dev_step("Watching", &main));
    println!();

    let mut last_check = SystemTime::now();

    loop {
        std::thread::sleep(POLL_INTERVAL);

        reap_if_exited(&mut proc);

        if !inputs_changed_since(project_root, last_check) {
            continue;
        }

        last_check = SystemTime::now();
        kill_and_wait(&mut proc);

        println!("{}", style::dev_step("Restarting", "source changed"));

        match compile::compile(project_root, &desc, opts.offline, &[]) {
            Ok(new_compiled) => {
                let new_cp = exploded_classpath(&new_compiled);
                match spawn_app(&main, &new_cp, enable_preview, extra_args) {
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
fn exploded_classpath(compiled: &compile::CompileOutput) -> Vec<PathBuf> {
    let mut cp = vec![compiled.classes_dir.clone()];

    if let Some(ref rd) = compiled.resources_dir {
        if rd.exists() {
            cp.push(rd.clone());
        }
    }

    cp.extend_from_slice(&compiled.dep_jars);
    cp.extend_from_slice(&compiled.kotlin_stdlib_jars);
    cp.extend_from_slice(&compiled.groovy_jars);
    cp
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
    extra_args: &[String],
) -> Result<Child> {
    let mut cmd = Command::new("java");
    if enable_preview {
        cmd.arg("--enable-preview");
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

/// Returns `true` when any file under `<project_root>/src/` or
/// `<project_root>/Curie.toml` is strictly newer than `since`.
fn inputs_changed_since(project_root: &Path, since: SystemTime) -> bool {
    let src_dir = project_root.join("src");
    let toml_path = project_root.join("Curie.toml");
    incremental::newest_mtime_in_dir(&src_dir) > since
        || incremental::mtime(&toml_path) > since
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
        }
    }

    fn write_with_mtime(path: &Path, mtime: SystemTime) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    // -- exploded_classpath ---------------------------------------------------

    #[test]
    fn classpath_starts_with_classes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let compiled = fake_compile_output(classes.clone(), vec![], vec![], vec![], None);
        let cp = exploded_classpath(&compiled);
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
        let cp = exploded_classpath(&compiled);
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
        let cp = exploded_classpath(&compiled);
        assert!(cp.contains(&resources));
    }

    #[test]
    fn classpath_omits_nonexistent_resources_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let resources = dir.path().join("does-not-exist");
        let compiled =
            fake_compile_output(classes.clone(), vec![], vec![], vec![], Some(resources.clone()));
        let cp = exploded_classpath(&compiled);
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
}
