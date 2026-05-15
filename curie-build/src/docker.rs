use crate::build::mtime;
use crate::descriptor::Descriptor;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;

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

/// Ask Docker when the local image was last created.
/// Returns `None` when the image doesn't exist locally or Docker is unavailable.
fn docker_image_created(image_ref: &str) -> Option<SystemTime> {
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.Created}}", image_ref])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let ts = std::str::from_utf8(&out.stdout).ok()?.trim();
    // Parse RFC 3339 / ISO 8601.
    // humantime only accepts UTC ("Z") but Docker may emit a local-timezone offset
    // like "2026-05-15T10:23:45.123456789+02:00".  Normalise to UTC first.
    parse_rfc3339_to_system_time(ts)
}

/// Parse an RFC 3339 timestamp that may have a `±HH:MM` timezone offset.
/// Converts to UTC by subtracting the offset, then delegates to `humantime`.
fn parse_rfc3339_to_system_time(ts: &str) -> Option<SystemTime> {
    // humantime handles "Z" suffix natively — fast path.
    if ts.ends_with('Z') {
        return humantime::parse_rfc3339(ts).ok();
    }

    // Look for a `+HH:MM` or `-HH:MM` suffix (last 6 chars, e.g. "+02:00").
    if ts.len() < 6 {
        return None;
    }
    let (body, offset_str) = ts.split_at(ts.len() - 6);
    let sign = offset_str.chars().next()?;
    if sign != '+' && sign != '-' {
        return None;
    }
    let parts: Vec<&str> = offset_str[1..].splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let offset_hours: i64 = parts[0].parse().ok()?;
    let offset_mins: i64 = parts[1].parse().ok()?;
    let offset_secs: i64 = (offset_hours * 60 + offset_mins) * 60;
    let offset_secs = if sign == '+' { offset_secs } else { -offset_secs };

    // Build a UTC string that humantime can parse.
    let utc_ts = format!("{}Z", body);
    let t = humantime::parse_rfc3339(&utc_ts).ok()?;

    // Subtract the UTC offset to get actual UTC time.
    if offset_secs >= 0 {
        t.checked_sub(std::time::Duration::from_secs(offset_secs as u64))
    } else {
        t.checked_add(std::time::Duration::from_secs((-offset_secs) as u64))
    }
}

/// Return the newest mtime among all Docker build inputs in `target/`:
/// Dockerfile, .dockerignore, the app JAR, and every file under libs/.
fn newest_input_mtime(target_dir: &Path, jar_filename: &str, has_libs: bool) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;

    for name in &["Dockerfile", ".dockerignore"] {
        let t = mtime(&target_dir.join(name));
        if t > newest {
            newest = t;
        }
    }

    let t = mtime(&target_dir.join(jar_filename));
    if t > newest {
        newest = t;
    }

    if has_libs {
        let libs_dir = target_dir.join("libs");
        for entry in WalkDir::new(&libs_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let t = mtime(entry.path());
            if t > newest {
                newest = t;
            }
        }
    }

    newest
}

fn build_with_user_dockerfile(
    project_root: &Path,
    _desc: &Descriptor,
    jar: &Path,
    image_ref: &str,
) -> Result<()> {
    println!("  Dockerfile      using project root Dockerfile");

    // Skip if the image is newer than both the Dockerfile and the app JAR.
    let newest_input = {
        let mut t = mtime(&project_root.join("Dockerfile"));
        let jar_t = mtime(jar);
        if jar_t > t { t = jar_t; }
        t
    };
    if let Some(image_time) = docker_image_created(image_ref) {
        if newest_input <= image_time {
            println!("  Docker image    up to date");
            return Ok(());
        }
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

    // Skip docker build if the image is newer than all inputs.
    if let Some(image_time) = docker_image_created(image_ref) {
        if newest_input_mtime(&target_dir, &jar_filename, has_libs) <= image_time {
            println!("  Docker image    up to date");
            return Ok(());
        }
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
    fn parse_ts_utc_z() {
        // Plain UTC with Z suffix — humantime fast path.
        let t = parse_rfc3339_to_system_time("2026-05-15T08:44:03.776995931Z");
        assert!(t.is_some());
    }

    #[test]
    fn parse_ts_positive_offset() {
        // +02:00 means the wall clock was 2 h ahead of UTC.
        // Both strings should resolve to the same UTC instant.
        let utc = parse_rfc3339_to_system_time("2026-05-15T08:44:03Z").unwrap();
        let local = parse_rfc3339_to_system_time("2026-05-15T10:44:03+02:00").unwrap();
        assert_eq!(utc, local);
    }

    #[test]
    fn parse_ts_negative_offset() {
        // -05:00 means wall clock was 5 h behind UTC.
        let utc = parse_rfc3339_to_system_time("2026-05-15T13:00:00Z").unwrap();
        let local = parse_rfc3339_to_system_time("2026-05-15T08:00:00-05:00").unwrap();
        assert_eq!(utc, local);
    }

    #[test]
    fn parse_ts_invalid_returns_none() {
        assert!(parse_rfc3339_to_system_time("not-a-timestamp").is_none());
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
}
