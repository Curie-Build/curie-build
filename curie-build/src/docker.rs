use crate::descriptor::Descriptor;
use crate::incremental::{mtime, Inputs, Stamp};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Determines which Dockerfile strategy to use.
enum DockerfileSource {
    /// User provided a Dockerfile at the project root. Build context = project root.
    UserProvided,
    /// Curie generates a Dockerfile in target/. Build context = target/.
    Generated,
}

fn dockerfile_source(project_root: &Path) -> DockerfileSource {
    if project_root.join("Dockerfile").exists() {
        DockerfileSource::UserProvided
    } else {
        DockerfileSource::Generated
    }
}

/// Build a Docker image. Returns the full image reference used (name:tag).
pub fn docker_build(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<String> {
    let image_ref = desc.image_ref();

    match dockerfile_source(project_root) {
        DockerfileSource::UserProvided => {
            build_with_user_dockerfile(project_root, desc, jar, &image_ref)?;
        }
        DockerfileSource::Generated => {
            build_with_generated_dockerfile(project_root, desc, jar, dep_jars, &image_ref)?;
        }
    }

    Ok(image_ref)
}

/// Run a Docker container from the built image, forwarding extra_args to the
/// container entrypoint. The container is removed after it exits (--rm).
pub fn docker_run(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    extra_args: &[String],
) -> Result<()> {
    let image_ref = docker_build(project_root, desc, jar, dep_jars)?;

    println!("Running container {}  (--rm)", image_ref);
    println!();

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("--rm").arg(&image_ref);

    for arg in extra_args {
        cmd.arg(arg);
    }

    let status = cmd
        .status()
        .context("failed to invoke docker run — is Docker installed?")?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Path of the stamp file written after every successful `docker build`.
/// Its mtime is the authoritative "last built" time for skip checks.
///
/// # Why a stamp file — and not the alternatives
///
/// ## `docker inspect --format '{{.Created}}'` (image creation timestamp)
/// The `Created` field reflects when the image was *first* assembled, not
/// when `docker build` last ran.  When all layers are cache-hits Docker
/// reuses the existing image object and never updates `Created`.  So after
/// the first build the timestamp is permanently frozen, and inputs written
/// later (e.g. a recompiled JAR) are always "newer" — the skip never fires.
///
/// ## Parsing `{{.Created}}` with `humantime`
/// Even if the timestamp were reliable, `humantime::parse_rfc3339` only
/// accepts a `Z` (UTC) suffix.  Docker emits the daemon's local timezone
/// offset (`+02:00`, `-05:00`, …), so the parse returns `None` and the
/// skip is again never reached.  We could normalise the offset to UTC
/// before parsing, but this is moot given the frozen-timestamp problem.
///
/// ## `docker inspect --format '{{.Metadata.LastTagTime.Unix}}'`
/// This is the time the tag (`name:version`) was last applied, not the
/// time the image content was built.  Re-tagging an old image would make
/// it appear fresh even though its layers are stale.
///
/// ## Stamp file (chosen approach)
/// We write `target/.docker-stamp` (empty file) immediately after every
/// successful `docker build`.  Its filesystem mtime is updated on every
/// real build, including cache-hit runs, so it accurately represents "the
/// last time we ran docker build for this project".  Skip iff:
///
///   newest_input_mtime(target/) <= mtime(target/.docker-stamp)
fn stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".docker-stamp")
}

/// Touch (create/update) the stamp file to record that a build just succeeded.
fn touch_stamp(target_dir: &Path) -> Result<()> {
    let path = stamp_path(target_dir);
    // Write empty content — we only care about the mtime.
    std::fs::write(&path, b"").with_context(|| format!("failed to write {}", path.display()))
}

/// Inputs that invalidate the generated-Dockerfile build's stamp:
/// the generated Dockerfile, the generated .dockerignore, the app JAR,
/// and every file copied into `target/libs/`.
fn generated_dockerfile_inputs(target_dir: &Path, jar_filename: &str, has_libs: bool) -> Inputs {
    let mut inputs = Inputs::new();
    inputs
        .add_file(&target_dir.join("Dockerfile"))
        .add_file(&target_dir.join(".dockerignore"))
        .add_file(&target_dir.join(jar_filename));
    if has_libs {
        inputs.add_dir(&target_dir.join("libs"));
    }
    inputs
}

/// Inputs that invalidate the user-Dockerfile build's stamp.
///
/// We track the user's `Dockerfile`, their `.dockerignore` if present, the
/// app JAR, and `target/libs/` (in case the user's Dockerfile COPYs from
/// it).  We do NOT scan arbitrary project-root files referenced by other
/// `COPY` instructions — that's an open-ended set and tracking it correctly
/// would require parsing the Dockerfile.  Users with custom COPY sources
/// outside this set may need `curie clean` to force a rebuild.
fn user_dockerfile_inputs(project_root: &Path, jar: &Path) -> Inputs {
    let mut inputs = Inputs::new();
    inputs
        .add_file(&project_root.join("Dockerfile"))
        .add_file(&project_root.join(".dockerignore"))
        .add_file(jar);
    let libs_dir = project_root.join("target").join("libs");
    if libs_dir.exists() {
        inputs.add_dir(&libs_dir);
    }
    inputs
}

fn build_with_user_dockerfile(
    project_root: &Path,
    _desc: &Descriptor,
    jar: &Path,
    image_ref: &str,
) -> Result<()> {
    println!("  Dockerfile      using project root Dockerfile");

    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let inputs = user_dockerfile_inputs(project_root, jar);
    if Stamp::of(&stamp_path(&target_dir)).covers(&inputs) {
        println!("  Docker image    up to date");
        return Ok(());
    }

    println!("  Docker image    building {}", image_ref);
    // Make JAR path relative to project root for the build arg.
    let jar_rel = jar
        .strip_prefix(project_root)
        .unwrap_or(jar)
        .to_string_lossy()
        .to_string();

    let status = Command::new("docker")
        .arg("build")
        .arg("--build-arg")
        .arg(format!("JAR_FILE={}", jar_rel))
        .arg("-t")
        .arg(image_ref)
        .arg(project_root)
        .status()
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    touch_stamp(&target_dir)?;
    println!("  Docker image    {}", image_ref);
    Ok(())
}

fn build_with_generated_dockerfile(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    image_ref: &str,
) -> Result<()> {
    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let jar_filename = jar
        .file_name()
        .context("JAR path has no filename")?
        .to_string_lossy()
        .to_string();

    // Copy dependency JARs into target/libs/ (skip up-to-date files).
    if !dep_jars.is_empty() {
        let libs_dir = target_dir.join("libs");
        std::fs::create_dir_all(&libs_dir).context("failed to create target/libs")?;

        let mut copied = 0usize;
        let mut skipped = 0usize;
        for dep in dep_jars {
            let fname = dep
                .file_name()
                .context("dep JAR path has no filename")?
                .to_string_lossy()
                .to_string();
            let dest = libs_dir.join(&fname);
            if mtime(dep) > mtime(&dest) {
                std::fs::copy(dep, &dest).with_context(|| {
                    format!(
                        "failed to copy dep JAR {} to {}",
                        dep.display(),
                        dest.display()
                    )
                })?;
                copied += 1;
            } else {
                skipped += 1;
            }
        }

        match (copied, skipped) {
            (0, _) => println!("  Docker dep JARs up to date"),
            (c, 0) => println!("  Docker dep JARs {} copied", c),
            (c, s) => println!("  Docker dep JARs {} copied, {} up to date", c, s),
        }
    }

    // Generate Dockerfile in target/ — skip write if content is unchanged.
    let dockerfile_content =
        generate_dockerfile(&desc.docker.base_image, &jar_filename, !dep_jars.is_empty());
    let dockerfile_path = target_dir.join("Dockerfile");
    let existing = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
    if existing == dockerfile_content {
        println!("  Dockerfile      up to date");
    } else {
        std::fs::write(&dockerfile_path, &dockerfile_content)
            .context("failed to write generated Dockerfile")?;
        println!("  Dockerfile      generated  (target/Dockerfile)");
    }

    // Generate .dockerignore in target/ — skip write if content is unchanged.
    let dockerignore_content = generate_dockerignore(&jar_filename, !dep_jars.is_empty());
    let dockerignore_path = target_dir.join(".dockerignore");
    let existing_ignore = std::fs::read_to_string(&dockerignore_path).unwrap_or_default();
    if existing_ignore == dockerignore_content {
        println!("  .dockerignore   up to date");
    } else {
        std::fs::write(&dockerignore_path, &dockerignore_content)
            .context("failed to write .dockerignore")?;
        println!("  .dockerignore   generated  (target/.dockerignore)");
    }

    let has_libs = !dep_jars.is_empty();

    // Skip docker build if the stamp is newer than all inputs.
    // We use a stamp file (target/.docker-stamp) rather than the Docker image's
    // Created timestamp, because Docker does not update Created when all layers
    // are cached — making the image appear older than it really is.
    let stamp = stamp_path(&target_dir);
    let inputs = generated_dockerfile_inputs(&target_dir, &jar_filename, has_libs);
    if Stamp::of(&stamp).covers(&inputs) {
        println!("  Docker image    up to date");
        return Ok(());
    }

    println!("  Docker image    building {}", image_ref);
    let status = Command::new("docker")
        .arg("build")
        .arg("-t")
        .arg(image_ref)
        .arg(&target_dir)
        .status()
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    touch_stamp(&target_dir)?;
    println!("  Docker image    {}", image_ref);
    Ok(())
}

/// Generate the content of `target/.dockerignore`.
///
/// Starts with `*` to exclude everything, then whitelists only the app JAR
/// and (when present) the `libs/` directory.
fn generate_dockerignore(jar_filename: &str, has_libs: bool) -> String {
    let mut lines = vec!["*".to_string(), format!("!{}", jar_filename)];
    if has_libs {
        lines.push("!libs/".to_string());
    }
    lines.join("\n") + "\n"
}

fn generate_dockerfile(base_image: &str, jar_filename: &str, has_libs: bool) -> String {
    let mut lines = vec![
        format!("FROM {}", base_image),
        "WORKDIR /app".to_string(),
    ];

    if has_libs {
        // Copy dep JARs before the app JAR so this layer is cached across app-code changes.
        // Class-Path in MANIFEST.MF points to libs/, so java -jar resolves them automatically.
        lines.push("COPY libs/ libs/".to_string());
    }

    lines.push(format!("COPY {} app.jar", jar_filename));
    lines.push("ENTRYPOINT [\"java\", \"-jar\", \"app.jar\"]".to_string());

    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockerignore_no_libs() {
        let content = generate_dockerignore("myapp-1.0.jar", false);
        assert_eq!(content, "*\n!myapp-1.0.jar\n");
    }

    #[test]
    fn dockerignore_with_libs() {
        let content = generate_dockerignore("myapp-1.0.jar", true);
        assert_eq!(content, "*\n!myapp-1.0.jar\n!libs/\n");
    }

    #[test]
    fn dockerfile_no_deps() {
        let content = generate_dockerfile("eclipse-temurin:21-jre", "myapp-1.0.jar", false);
        assert_eq!(
            content,
            "FROM eclipse-temurin:21-jre\n\
             WORKDIR /app\n\
             COPY myapp-1.0.jar app.jar\n\
             ENTRYPOINT [\"java\", \"-jar\", \"app.jar\"]\n"
        );
    }

    #[test]
    fn dockerfile_with_deps() {
        let content = generate_dockerfile("eclipse-temurin:21-jre", "myapp-1.0.jar", true);
        assert_eq!(
            content,
            "FROM eclipse-temurin:21-jre\n\
             WORKDIR /app\n\
             COPY libs/ libs/\n\
             COPY myapp-1.0.jar app.jar\n\
             ENTRYPOINT [\"java\", \"-jar\", \"app.jar\"]\n"
        );
    }

    #[test]
    fn stamp_skip_logic() {
        // Stamp::covers semantics (via generated_dockerfile_inputs):
        //   covers → skip; !covers → build
        use filetime::{set_file_mtime, FileTime};
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path();

        let jar = target.join("app-1.0.jar");
        let dockerfile = target.join("Dockerfile");
        let dockerignore = target.join(".dockerignore");
        let stamp = stamp_path(target);

        let t0 = FileTime::from_unix_time(1_000_000, 0);
        let t1 = FileTime::from_unix_time(1_000_001, 0);

        for path in &[&jar, &dockerfile, &dockerignore] {
            fs::write(path, b"").unwrap();
            set_file_mtime(path, t0).unwrap();
        }

        // No stamp yet → must build.
        let inputs = generated_dockerfile_inputs(target, "app-1.0.jar", false);
        assert!(!Stamp::of(&stamp).covers(&inputs));

        // Write stamp with mtime strictly after all inputs → skip.
        fs::write(&stamp, b"").unwrap();
        set_file_mtime(&stamp, t1).unwrap();
        assert!(Stamp::of(&stamp).covers(&inputs));

        // Update the jar to be newer than the stamp → build again.
        set_file_mtime(&jar, FileTime::from_unix_time(1_000_002, 0)).unwrap();
        let inputs = generated_dockerfile_inputs(target, "app-1.0.jar", false);
        assert!(!Stamp::of(&stamp).covers(&inputs));

        // Layer-1 regression guard: a tied jar mtime (same second as the
        // stamp) must also force a rebuild.
        set_file_mtime(&jar, t1).unwrap(); // jar mtime == stamp mtime
        let inputs = generated_dockerfile_inputs(target, "app-1.0.jar", false);
        assert!(
            !Stamp::of(&stamp).covers(&inputs),
            "tied input mtime must not be considered covered",
        );
    }
}
