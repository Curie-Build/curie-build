//! GraalVM native-image compilation step.
//!
//! Called after JAR packaging when `[native-image]` is present in
//! `Curie.toml`.  Produces a standalone native binary in `target/`.
//!
//! # Locating `native-image`
//!
//! Curie checks, in order:
//!   1. `$GRAALVM_HOME/bin/native-image`
//!   2. `native-image` on `$PATH` (resolved by the OS)
//!
//! If neither is found the build fails with an actionable error message.
//!
//! # Incremental skip
//!
//! `native-image` compilation is slow (tens of seconds to minutes), so Curie
//! writes a stamp file `target/.native-stamp` after every successful run.
//! On the next invocation, if the stamp is newer than every input (the app
//! JAR, all dependency JARs, and the optional config directory) the step is
//! skipped entirely.

use crate::descriptor::{Descriptor, NativeImage};
use crate::incremental::{Inputs, Stamp};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run `native-image` for an application project.
///
/// * `project_root` — the directory containing `Curie.toml`.
/// * `desc`         — the fully-loaded descriptor.
/// * `jar`          — the application JAR produced by [`crate::jar`].
/// * `dep_jars`     — transitive dependency JARs (may be empty).
pub fn build_native(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let cfg = &desc.native_image;
    let app_name = desc.buildable_name();
    let output_name = cfg.resolved_output_name(app_name);

    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let output_path = target_dir.join(output_name);
    let stamp = stamp_path(&target_dir);

    // --- incremental skip ---------------------------------------------------
    let inputs = native_inputs(jar, dep_jars, cfg, project_root);
    if Stamp::of(&stamp).covers(&inputs) {
        println!("  Native image    up to date");
        return Ok(());
    }

    // --- locate native-image executable ------------------------------------
    let exe = find_native_image_exe().context(
        "native-image not found.\n\
         Install GraalVM and either:\n  \
         • set GRAALVM_HOME to the GraalVM installation directory, or\n  \
         • add $GRAALVM_HOME/bin to PATH.\n\
         Download: https://www.graalvm.org/downloads/",
    )?;

    // --- determine main class -----------------------------------------------
    let main_class = desc
        .application()
        .and_then(|a| a.main_class.as_deref())
        .context(
            "native-image requires mainClass to be declared in [application]; \
             auto-detection is not supported for native compilation",
        )?;

    // --- build classpath ---------------------------------------------------
    let cp = build_classpath(jar, dep_jars);

    // --- assemble command --------------------------------------------------
    let mut cmd = Command::new(&exe);
    cmd.arg("-cp").arg(&cp);
    cmd.arg(main_class);
    cmd.arg("-o").arg(&output_path);

    if let Some(config_dir) = &cfg.config_dir {
        let abs_config = project_root.join(config_dir);
        cmd.arg(format!(
            "-H:ConfigurationFileDirectories={}",
            abs_config.display()
        ));
    }

    for extra in &cfg.extra_args {
        cmd.arg(extra);
    }

    println!(
        "  Native image    {} -> target/{}",
        exe.display(),
        output_name
    );

    let status = cmd
        .status()
        .with_context(|| format!("failed to invoke {}", exe.display()))?;

    if !status.success() {
        bail!("native-image failed");
    }

    // Write stamp so the next build can skip this step.
    touch_stamp(&target_dir)?;
    println!("  Native image    target/{}", output_name);

    Ok(())
}

// ---------------------------------------------------------------------------
// Executable discovery
// ---------------------------------------------------------------------------

/// Find the `native-image` binary.
///
/// Search order:
///   1. `$GRAALVM_HOME/bin/native-image`
///   2. `native-image` via `$PATH` (checked with `which`-style existence test)
fn find_native_image_exe() -> Option<PathBuf> {
    // 1. $GRAALVM_HOME
    if let Ok(graalvm_home) = std::env::var("GRAALVM_HOME") {
        let candidate = PathBuf::from(graalvm_home).join("bin").join("native-image");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // 2. PATH — delegate to the OS; `which` is not available everywhere, but
    //    we can probe by resolving the name via a no-op Command invocation.
    //    Use `--version` so it exits immediately on success.
    #[cfg(unix)]
    {
        if let Ok(output) = Command::new("which").arg("native-image").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                let path = PathBuf::from(path_str.trim());
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }

    // Fallback: just try the bare name and let the OS error if it's missing.
    // We return it here; the caller will get a useful OS error on `cmd.status()`.
    // But we also need to report "not found" before even trying, so check with
    // a quick `--version` probe.
    let probe = Command::new("native-image")
        .arg("--version")
        .output();
    if probe.is_ok() {
        return Some(PathBuf::from("native-image"));
    }

    None
}

// ---------------------------------------------------------------------------
// Classpath helpers
// ---------------------------------------------------------------------------

fn build_classpath(jar: &Path, dep_jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut parts: Vec<String> = vec![jar.to_string_lossy().into_owned()];
    for dep in dep_jars {
        parts.push(dep.to_string_lossy().into_owned());
    }
    parts.join(sep)
}

// ---------------------------------------------------------------------------
// Stamp helpers
// ---------------------------------------------------------------------------

fn stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".native-stamp")
}

fn touch_stamp(target_dir: &Path) -> Result<()> {
    let path = stamp_path(target_dir);
    std::fs::write(&path, b"")
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Collect all inputs that can invalidate the native binary.
fn native_inputs(
    jar: &Path,
    dep_jars: &[PathBuf],
    cfg: &NativeImage,
    project_root: &Path,
) -> Inputs {
    let mut inputs = Inputs::new();
    inputs.add_file(jar);
    inputs.add_paths(dep_jars);
    if let Some(config_dir) = &cfg.config_dir {
        inputs.add_dir(&project_root.join(config_dir));
    }
    inputs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classpath_single_jar() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let jar = PathBuf::from("/target/app.jar");
        let cp = build_classpath(&jar, &[]);
        assert_eq!(cp, "/target/app.jar");
        assert!(!cp.contains(sep) || cp == "/target/app.jar");
    }

    #[test]
    fn classpath_with_deps() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let jar = PathBuf::from("/target/app.jar");
        let deps = vec![
            PathBuf::from("/m2/foo.jar"),
            PathBuf::from("/m2/bar.jar"),
        ];
        let cp = build_classpath(&jar, &deps);
        let parts: Vec<&str> = cp.split(sep).collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "/target/app.jar");
        assert_eq!(parts[1], "/m2/foo.jar");
        assert_eq!(parts[2], "/m2/bar.jar");
    }

    #[test]
    fn stamp_path_is_in_target_dir() {
        let target = PathBuf::from("/some/target");
        assert_eq!(stamp_path(&target), PathBuf::from("/some/target/.native-stamp"));
    }

    #[test]
    fn native_inputs_no_config_dir() {
        let jar = PathBuf::from("/target/app.jar");
        let cfg = NativeImage::default();
        let project_root = PathBuf::from("/project");
        let inputs = native_inputs(&jar, &[], &cfg, &project_root);
        // inputs.newest() returns UNIX_EPOCH for non-existent paths — just
        // ensure the call doesn't panic.
        let _ = inputs;
    }

    #[test]
    fn resolved_output_name_uses_app_name_as_default() {
        let cfg = NativeImage::default();
        assert_eq!(cfg.resolved_output_name("my-app"), "my-app");
    }

    #[test]
    fn resolved_output_name_uses_override_when_set() {
        let cfg = NativeImage {
            output_name: Some("custom-binary".to_string()),
            ..NativeImage::default()
        };
        assert_eq!(cfg.resolved_output_name("my-app"), "custom-binary");
    }
}
