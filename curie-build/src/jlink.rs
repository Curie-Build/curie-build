//! `jlink` runtime-image assembly step (plain JDK, no GraalVM).
//!
//! Called after JAR packaging when `[jlink]` is present in `Curie.toml`.
//! Produces a self-contained JDK runtime image under `target/runtime/`:
//!
//! - `target/runtime/jdk/`  — a custom runtime built by `jlink`, containing
//!   only the modules the app needs.
//! - `target/runtime/lib/`  — the app JAR plus a `libs/` subdir of dependency
//!   JARs (same layout `jar::populate_libs_dir` already produces for
//!   `docker.rs`), so the JAR's own `Class-Path` manifest header resolves
//!   dependencies with no extra classpath handling needed.
//! - `target/runtime/bin/<name>` / `bin/<name>.bat` — launcher scripts that
//!   invoke `jdk/bin/java -jar lib/<jarfile>`.
//!
//! # Module detection
//!
//! When `[jlink].modules` is omitted, the required JDK modules are detected
//! by running `jdeps --print-module-deps --ignore-missing-deps` against the
//! built JAR (and its dependency JARs, if any, via `--class-path`).
//!
//! # `--module-path`
//!
//! `jlink` is not passed an explicit `--module-path`: on JDK 21+ it defaults
//! to the running JDK's own `jmods`, which is exactly what we want since
//! `jlink`/`jdeps` are only ever invoked with the JDK already used to build
//! the project.
//!
//! # Incremental skip
//!
//! Like `native.rs`, a stamp file `target/.jlink-stamp` is written after every
//! successful run; the step is skipped when the stamp is newer than every
//! input (the app JAR and all dependency JARs).
//!
//! # Directory staging
//!
//! `jlink --output` refuses to write into an already-existing directory, so
//! the runtime is built into a sibling staging directory
//! (`resources::staging_dir`) and then moved into place — the same
//! remove-then-rename idiom `resources.rs::filter_roots` already uses for its
//! own directory outputs.

use crate::descriptor::Descriptor;
use crate::incremental::{Inputs, Stamp};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Assemble a `jlink` runtime image for an application project.
///
/// * `project_root` — the directory containing `Curie.toml`.
/// * `desc`         — the fully-loaded descriptor.
/// * `jar`          — the application JAR produced by [`crate::jar`].
/// * `dep_jars`     — transitive dependency JARs (may be empty).
pub fn build_jlink(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let cfg = &desc.jlink;
    let app_name = desc.buildable_name();
    let launcher_name = cfg.resolved_output_name(app_name);

    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let runtime_dir = target_dir.join("runtime");
    let jdk_dir = runtime_dir.join("jdk");
    let lib_dir = runtime_dir.join("lib");
    let bin_dir = runtime_dir.join("bin");

    let stamp = stamp_path(&target_dir);

    // --- incremental skip ---------------------------------------------------
    let inputs = jlink_inputs(jar, dep_jars);
    if Stamp::of(&stamp).covers(&inputs) {
        crate::parallel::emit(&crate::style::up_to_date("jlink"));
        return Ok(());
    }

    // --- resolve modules -----------------------------------------------------
    let modules = if cfg.modules.is_empty() {
        detect_modules_via_jdeps(jar, dep_jars, desc)?
    } else {
        cfg.modules.clone()
    };
    if modules.is_empty() {
        bail!("jlink: no modules to link — jdeps detected none and [jlink].modules is empty");
    }

    // --- build the runtime image into a staging dir, then swap in -----------
    let staging = crate::resources::staging_dir(&jdk_dir);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to clear {}", staging.display()))?;
    }

    let mut cmd = Command::new("jlink");
    cmd.arg("--add-modules").arg(modules.join(","));
    cmd.arg("--output").arg(&staging);
    if cfg.strip_debug {
        cmd.arg("--strip-debug");
    }
    cmd.arg("--compress").arg(if cfg.compress { "zip-6" } else { "zip-0" });

    crate::parallel::emit(&crate::style::active(
        "jlink",
        &format!("{} -> target/runtime/jdk", modules.join(",")),
    ));

    let status = crate::proc::spawn_cmd(&mut cmd)
        .context("failed to invoke jlink — is a JDK 21+ installed and on PATH?")?;
    if !status.success() {
        bail!("jlink failed");
    }

    if jdk_dir.exists() {
        std::fs::remove_dir_all(&jdk_dir)
            .with_context(|| format!("failed to clear {}", jdk_dir.display()))?;
    }
    std::fs::rename(&staging, &jdk_dir).with_context(|| {
        format!("failed to move {} into {}", staging.display(), jdk_dir.display())
    })?;

    // --- lib/: app jar + dependency libs/ ------------------------------------
    std::fs::create_dir_all(&lib_dir)
        .with_context(|| format!("failed to create {}", lib_dir.display()))?;
    let jar_file_name = jar
        .file_name()
        .context("application JAR path has no file name")?;
    let lib_jar_path = lib_dir.join(jar_file_name);
    std::fs::copy(jar, &lib_jar_path)
        .with_context(|| format!("failed to copy {} into {}", jar.display(), lib_dir.display()))?;
    crate::jar::populate_libs_dir(&lib_dir.join("libs"), dep_jars)?;

    // --- bin/: launcher scripts -----------------------------------------------
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    write_launchers(&bin_dir, launcher_name, jar_file_name.to_string_lossy().as_ref())?;

    // Write stamp so the next build can skip this step.
    touch_stamp(&target_dir)?;
    crate::parallel::emit(&crate::style::done(&format!("target/runtime/bin/{launcher_name}")));

    Ok(())
}

// ---------------------------------------------------------------------------
// Module detection via jdeps
// ---------------------------------------------------------------------------

/// Run `jdeps --print-module-deps` against `jar` (with `dep_jars` on
/// `--class-path`, if any) and parse the resulting comma-separated module
/// list.
fn detect_modules_via_jdeps(
    jar: &Path,
    dep_jars: &[PathBuf],
    desc: &Descriptor,
) -> Result<Vec<String>> {
    let release = jdeps_multi_release_version(desc)?;

    let mut cmd = Command::new("jdeps");
    cmd.arg("--print-module-deps");
    cmd.arg("--ignore-missing-deps");
    cmd.arg("--multi-release").arg(&release);
    if !dep_jars.is_empty() {
        cmd.arg("--class-path").arg(build_classpath(dep_jars));
    }
    cmd.arg(jar);

    let output = cmd
        .output()
        .context("failed to invoke jdeps — is a JDK 21+ installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "jdeps failed to detect modules for {}:\n{}",
            jar.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(parse_module_deps(&String::from_utf8_lossy(&output.stdout)))
}

/// The `--multi-release` value to pass to `jdeps`: the project's declared
/// Java release if set, else the running JDK's major version — same
/// fallback `compile::javac_release_arg` uses for `javac --release`.
fn jdeps_multi_release_version(desc: &Descriptor) -> Result<String> {
    if let Some(release) = desc.java.effective() {
        return Ok(release.to_string());
    }
    crate::compile::running_jdk_major_version()
}

/// Parse `jdeps --print-module-deps` stdout (a single comma-separated line,
/// e.g. `"java.base,java.logging"`) into a list of module names.
fn parse_module_deps(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

fn build_classpath(dep_jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    dep_jars
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

// ---------------------------------------------------------------------------
// Launcher scripts
// ---------------------------------------------------------------------------

fn unix_launcher_script(jar_file_name: &str) -> String {
    format!(
        "#!/bin/sh\n\
         DIR=\"$(cd \"$(dirname \"$0\")/..\" && pwd)\"\n\
         exec \"$DIR/jdk/bin/java\" -jar \"$DIR/lib/{jar_file_name}\" \"$@\"\n"
    )
}

fn windows_launcher_script(jar_file_name: &str) -> String {
    format!(
        "@echo off\r\n\
         set DIR=%~dp0..\r\n\
         \"%DIR%\\jdk\\bin\\java\" -jar \"%DIR%\\lib\\{jar_file_name}\" %*\r\n"
    )
}

fn write_launchers(bin_dir: &Path, launcher_name: &str, jar_file_name: &str) -> Result<()> {
    let unix_path = bin_dir.join(launcher_name);
    std::fs::write(&unix_path, unix_launcher_script(jar_file_name))
        .with_context(|| format!("failed to write {}", unix_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&unix_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&unix_path, perms)?;
    }

    let bat_path = bin_dir.join(format!("{launcher_name}.bat"));
    std::fs::write(&bat_path, windows_launcher_script(jar_file_name))
        .with_context(|| format!("failed to write {}", bat_path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Stamp helpers
// ---------------------------------------------------------------------------

fn stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".jlink-stamp")
}

fn touch_stamp(target_dir: &Path) -> Result<()> {
    crate::incremental::touch_stamp(&stamp_path(target_dir))
}

/// Collect all inputs that can invalidate the runtime image.
fn jlink_inputs(jar: &Path, dep_jars: &[PathBuf]) -> Inputs {
    let mut inputs = Inputs::new();
    inputs.add_file(jar);
    inputs.add_paths(dep_jars);
    inputs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::Jlink;

    #[test]
    fn parse_module_deps_single_module() {
        assert_eq!(parse_module_deps("java.base\n"), vec!["java.base"]);
    }

    #[test]
    fn parse_module_deps_multiple_modules() {
        assert_eq!(
            parse_module_deps("java.base,java.logging,java.sql\n"),
            vec!["java.base", "java.logging", "java.sql"]
        );
    }

    #[test]
    fn parse_module_deps_trims_whitespace() {
        assert_eq!(
            parse_module_deps(" java.base , java.logging \n"),
            vec!["java.base", "java.logging"]
        );
    }

    #[test]
    fn parse_module_deps_empty_output() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(parse_module_deps(""), empty);
        assert_eq!(parse_module_deps("\n"), empty);
    }

    #[test]
    fn stamp_path_is_in_target_dir() {
        let target = PathBuf::from("/some/target");
        assert_eq!(stamp_path(&target), PathBuf::from("/some/target/.jlink-stamp"));
    }

    #[test]
    fn resolved_output_name_uses_app_name_as_default() {
        let cfg = Jlink::default();
        assert_eq!(cfg.resolved_output_name("my-app"), "my-app");
    }

    #[test]
    fn resolved_output_name_uses_override_when_set() {
        let cfg = Jlink {
            output_name: Some("my-runtime".to_string()),
            ..Jlink::default()
        };
        assert_eq!(cfg.resolved_output_name("my-app"), "my-runtime");
    }

    #[test]
    fn unix_launcher_invokes_bundled_jdk_and_jar() {
        let script = unix_launcher_script("app-1.0.0.jar");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("\"$DIR/jdk/bin/java\" -jar \"$DIR/lib/app-1.0.0.jar\" \"$@\""));
    }

    #[test]
    fn windows_launcher_invokes_bundled_jdk_and_jar() {
        let script = windows_launcher_script("app-1.0.0.jar");
        assert!(script.starts_with("@echo off\r\n"));
        assert!(script.contains("\"%DIR%\\jdk\\bin\\java\" -jar \"%DIR%\\lib\\app-1.0.0.jar\" %*"));
    }

    #[test]
    fn jlink_inputs_no_dep_jars() {
        let jar = PathBuf::from("/target/app.jar");
        let inputs = jlink_inputs(&jar, &[]);
        // inputs.newest() returns None for non-existent paths — just ensure
        // the call doesn't panic.
        let _ = inputs;
    }
}
