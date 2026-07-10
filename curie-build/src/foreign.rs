//! Run foreign (non-Curie) workspace members via their external tools.
//!
//! Invoked from workspace orchestration for build/test/clean.  Output is
//! routed through [`crate::proc::spawn_cmd`] so parallel builds mux correctly
//! and `target/<action>.log` captures foreign tool output.

use crate::resources;
use anyhow::{bail, Context, Result};
use curie_meta::{ForeignCommand, ForeignProject, ForeignTool};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which foreign action to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignAction {
    Build,
    Test,
    Clean,
}

/// Recursion-depth guard for foreign-curie members that re-enter this binary.
const CURIE_FOREIGN_DEPTH_ENV: &str = "CURIE_FOREIGN_DEPTH";
const CURIE_FOREIGN_DEPTH_MAX: u32 = 8;

/// Run a foreign member's build/test/clean command.
///
/// On `Build`, expands `artifacts` globs and returns matching paths (for
/// dependents' classpaths).  Test and clean return an empty vec.
pub fn run_foreign(
    member_dir: &Path,
    f: &ForeignProject,
    action: ForeignAction,
) -> Result<Vec<PathBuf>> {
    let (label, cmd) = match action {
        ForeignAction::Build => ("Building", ForeignCommand::Explicit(f.build_command.clone())),
        ForeignAction::Test => ("Testing", f.test_command.clone()),
        ForeignAction::Clean => ("Cleaning", f.clean_command.clone()),
    };

    crate::parallel::emit(&crate::style::headline(
        label,
        &format!("{} ({})", f.name, f.tool.label()),
        "",
    ));

    match &cmd {
        ForeignCommand::Skip => {
            crate::parallel::emit(&crate::style::neutral("Foreign", "skipped (disabled)"));
            return Ok(Vec::new());
        }
        ForeignCommand::Default(argv) => {
            if should_skip_default(member_dir, f.tool, action, argv)? {
                return Ok(Vec::new());
            }
            run_command(member_dir, f, argv, action)?;
        }
        ForeignCommand::Explicit(argv) => {
            run_command(member_dir, f, argv, action)?;
        }
    }

    if action == ForeignAction::Build {
        expand_artifacts(member_dir, &f.artifacts)
    } else {
        Ok(Vec::new())
    }
}

fn run_command(
    member_dir: &Path,
    f: &ForeignProject,
    argv: &[String],
    action: ForeignAction,
) -> Result<()> {
    if argv.is_empty() {
        bail!("foreign {} command for \"{}\" is empty", action_name(action), f.name);
    }

    // Foreign-curie recursion guard.
    let depth = if f.tool == ForeignTool::Curie {
        let d = std::env::var(CURIE_FOREIGN_DEPTH_ENV)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if d >= CURIE_FOREIGN_DEPTH_MAX {
            bail!(
                "foreign curie member \"{}\" exceeded recursion depth limit ({}); \
                 check for a workspace that re-enters its parent via [workspace.foreign]",
                f.name,
                CURIE_FOREIGN_DEPTH_MAX,
            );
        }
        Some(d + 1)
    } else {
        None
    };

    let display = argv.join(" ");
    crate::parallel::emit(&crate::style::active("Foreign", &display));

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(member_dir);
    for (k, v) in &f.env {
        cmd.env(k, v);
    }
    if let Some(d) = depth {
        cmd.env(CURIE_FOREIGN_DEPTH_ENV, d.to_string());
    }
    // For Curie tool, resolve argv[0] through current_exe when the stored
    // command still says "curie" or is the path captured at load time.
    if f.tool == ForeignTool::Curie {
        let bin = curie_meta::foreign::curie_bin();
        cmd = Command::new(&bin);
        cmd.args(&argv[1..]);
        cmd.current_dir(member_dir);
        for (k, v) in &f.env {
            cmd.env(k, v);
        }
        if let Some(d) = depth {
            cmd.env(CURIE_FOREIGN_DEPTH_ENV, d.to_string());
        }
    }

    let status = crate::proc::spawn_cmd(&mut cmd).with_context(|| {
        format!(
            "failed to start foreign {} command for \"{}\": {}",
            action_name(action),
            f.name,
            display
        )
    })?;

    if !status.success() {
        bail!(
            "foreign {} of \"{}\" failed (exit {})",
            action_name(action),
            f.name,
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn action_name(action: ForeignAction) -> &'static str {
    match action {
        ForeignAction::Build => "build",
        ForeignAction::Test => "test",
        ForeignAction::Clean => "clean",
    }
}

/// Graceful skip for default test/clean commands when the target/script is
/// absent.  Explicit overrides never reach here.
fn should_skip_default(
    member_dir: &Path,
    tool: ForeignTool,
    action: ForeignAction,
    argv: &[String],
) -> Result<bool> {
    match (tool, action) {
        (ForeignTool::Make, ForeignAction::Test | ForeignAction::Clean) => {
            // argv is typically ["make", "test"] or ["make", "clean"]
            let target = argv.get(1).map(String::as_str).unwrap_or("all");
            if !make_target_exists(member_dir, target) {
                crate::parallel::emit(&crate::style::neutral(
                    "Foreign",
                    &format!("skipped: no `{target}` target"),
                ));
                return Ok(true);
            }
            Ok(false)
        }
        (
            ForeignTool::Npm | ForeignTool::Bun | ForeignTool::Yarn,
            ForeignAction::Test | ForeignAction::Clean | ForeignAction::Build,
        ) => {
            // Default npm/bun/yarn: build uses "build", test uses "test",
            // clean uses "clean" (via `run clean`).
            let script = match action {
                ForeignAction::Build => "build",
                ForeignAction::Test => "test",
                ForeignAction::Clean => "clean",
            };
            // `npm test` does not use `run`; package.json still needs scripts.test.
            if !package_json_has_script(member_dir, script)? {
                crate::parallel::emit(&crate::style::neutral(
                    "Foreign",
                    &format!("skipped: no `scripts.{script}` in package.json"),
                ));
                return Ok(true);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Probe with `make -n <target>`: non-zero exit → target missing (or other
/// make error — treated as skip for defaults only).
fn make_target_exists(member_dir: &Path, target: &str) -> bool {
    Command::new("make")
        .args(["-n", target])
        .current_dir(member_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn package_json_has_script(member_dir: &Path, script: &str) -> Result<bool> {
    let path = member_dir.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(v
        .get("scripts")
        .and_then(|s| s.get(script))
        .is_some())
}

/// Expand artifact globs relative to `member_dir`.  Each pattern must match
/// at least one file; results are sorted for stability.
pub fn expand_artifacts(member_dir: &Path, globs: &[String]) -> Result<Vec<PathBuf>> {
    if globs.is_empty() {
        return Ok(Vec::new());
    }

    let mut out: Vec<PathBuf> = Vec::new();
    for pattern in globs {
        let mut matched = Vec::new();
        for entry in walkdir::WalkDir::new(member_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let abs = entry.path();
            let rel = match abs.strip_prefix(member_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if resources::glob_match(pattern, &rel_str) {
                matched.push(abs.to_path_buf());
            }
        }
        if matched.is_empty() {
            bail!(
                "foreign artifact pattern \"{pattern}\" matched zero files under {}",
                member_dir.display()
            );
        }
        out.extend(matched);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn expand_artifacts_literal_and_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("out")).unwrap();
        fs::write(root.join("out/a.jar"), b"").unwrap();
        fs::write(root.join("out/b.jar"), b"").unwrap();
        fs::create_dir_all(root.join("deep/x")).unwrap();
        fs::write(root.join("deep/x/c.jar"), b"").unwrap();

        let lit = expand_artifacts(root, &["out/a.jar".into()]).unwrap();
        assert_eq!(lit.len(), 1);
        assert!(lit[0].ends_with("out/a.jar"));

        let star = expand_artifacts(root, &["out/*.jar".into()]).unwrap();
        assert_eq!(star.len(), 2);

        let dd = expand_artifacts(root, &["**/*.jar".into()]).unwrap();
        assert_eq!(dd.len(), 3);
        // Sorted.
        let names: Vec<_> = dd
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(names, vec!["deep/x/c.jar", "out/a.jar", "out/b.jar"]);
    }

    #[test]
    fn expand_artifacts_zero_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = expand_artifacts(dir.path(), &["missing.jar".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched zero files"), "got: {err}");
        assert!(err.contains("missing.jar"), "got: {err}");
    }

    #[test]
    fn run_foreign_build_applies_env() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Script writes $GREETING_SUFFIX to a file and creates a jar placeholder.
        write_executable(
            &root.join("build.sh"),
            "#!/bin/sh\necho -n \"$GREETING_SUFFIX\" > env.out\nmkdir -p out\ntouch out/lib.jar\n",
        );
        let f = ForeignProject {
            name: "legacy".into(),
            tool: ForeignTool::Make, // arbitrary; command is explicit
            build_command: vec![root.join("build.sh").to_string_lossy().into_owned()],
            test_command: ForeignCommand::Skip,
            clean_command: ForeignCommand::Skip,
            artifacts: vec!["out/lib.jar".into()],
            env: {
                let mut e = BTreeMap::new();
                e.insert("GREETING_SUFFIX".into(), "!".into());
                e
            },
        };
        let arts = run_foreign(root, &f, ForeignAction::Build).unwrap();
        assert_eq!(arts.len(), 1);
        let env_out = fs::read_to_string(root.join("env.out")).unwrap();
        assert_eq!(env_out, "!");
    }

    #[test]
    fn run_foreign_skip_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let f = ForeignProject {
            name: "x".into(),
            tool: ForeignTool::Make,
            build_command: vec!["make".into()],
            test_command: ForeignCommand::Skip,
            clean_command: ForeignCommand::Skip,
            artifacts: vec![],
            env: BTreeMap::new(),
        };
        let arts = run_foreign(dir.path(), &f, ForeignAction::Test).unwrap();
        assert!(arts.is_empty());
    }

    #[test]
    fn make_default_test_skips_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Makefile with only `all`, no `test`.
        fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
        let f = ForeignProject {
            name: "legacy".into(),
            tool: ForeignTool::Make,
            build_command: vec!["make".into()],
            test_command: ForeignCommand::Default(vec!["make".into(), "test".into()]),
            clean_command: ForeignCommand::Skip,
            artifacts: vec![],
            env: BTreeMap::new(),
        };
        // Should skip, not fail.
        run_foreign(root, &f, ForeignAction::Test).unwrap();
    }

    #[test]
    fn make_explicit_test_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
        let f = ForeignProject {
            name: "legacy".into(),
            tool: ForeignTool::Make,
            build_command: vec!["make".into()],
            test_command: ForeignCommand::Explicit(vec!["make".into(), "test".into()]),
            clean_command: ForeignCommand::Skip,
            artifacts: vec![],
            env: BTreeMap::new(),
        };
        let err = run_foreign(root, &f, ForeignAction::Test).unwrap_err().to_string();
        assert!(err.contains("failed"), "got: {err}");
    }

    #[test]
    fn npm_default_skips_missing_script() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("package.json"), r#"{"name":"x","scripts":{}}"#).unwrap();
        let f = ForeignProject {
            name: "fe".into(),
            tool: ForeignTool::Npm,
            build_command: vec!["npm".into(), "run".into(), "build".into()],
            test_command: ForeignCommand::Default(vec!["npm".into(), "test".into()]),
            clean_command: ForeignCommand::Skip,
            artifacts: vec![],
            env: BTreeMap::new(),
        };
        run_foreign(root, &f, ForeignAction::Test).unwrap();
    }
}
