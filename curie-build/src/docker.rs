use crate::descriptor::{self, Descriptor};
use crate::incremental::{mtime, Inputs, Stamp};
use crate::{jlink, native};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default base image when the user hasn't set `[docker].baseImage`
/// explicitly. A JAR needs a JRE; a native binary or jlink runtime image
/// bundles its own — so those two use a minimal glibc base instead.
const DEFAULT_BASE_IMAGE_JAR: &str = "eclipse-temurin:21-jre-alpine";
const DEFAULT_BASE_IMAGE_SELF_CONTAINED: &str = "debian:trixie-slim";

/// Which artifact the generated Dockerfile packages.
///
/// Selected by [`select_docker_artifact`]: when both `[native-image]` and
/// `[jlink]` are configured, native-image wins (it's the more specialized/
/// optimized of the two tiers) — this precedence is not user-configurable,
/// since combining both is a rare edge case.
#[derive(Debug)]
enum DockerArtifact {
    /// The plain (or fat) JAR, plus `libs/` when there are separate dep JARs.
    /// This is the only mode when neither `[native-image]` nor `[jlink]` is
    /// configured.
    Jar { jar_filename: String, has_libs: bool },
    /// A GraalVM native-image binary, already at `target/<binary_name>`.
    Native { binary_name: String },
    /// A jlink runtime image, already at `target/runtime/`.
    Jlink { launcher_name: String },
}

/// Decide which artifact the generated Dockerfile should package, based on
/// which of `[native-image]` / `[jlink]` is configured. For the native/jlink
/// cases, the artifact must already exist on disk (built by an earlier step
/// in `build.rs`/`run.rs`) — if it's missing, fail with an actionable error
/// rather than silently falling back to the JAR.
fn select_docker_artifact(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<DockerArtifact> {
    if descriptor::native_image_enabled(desc) {
        let binary_path = native::output_path(project_root, desc);
        if !binary_path.exists() {
            bail!(
                "docker: [native-image] is enabled but no binary found at {}\n\
                 Build it first — run `curie build` (without --no-native) or `curie native`.",
                binary_path.display()
            );
        }
        let binary_name = binary_path
            .file_name()
            .context("native binary path has no file name")?
            .to_string_lossy()
            .into_owned();
        return Ok(DockerArtifact::Native { binary_name });
    }

    if descriptor::jlink_enabled(desc) {
        let runtime_dir = jlink::runtime_dir(project_root);
        if !runtime_dir.exists() {
            bail!(
                "docker: [jlink] is enabled but no runtime image found at {}\n\
                 Build it first — run `curie build` (without --no-jlink) or `curie jlink`.",
                runtime_dir.display()
            );
        }
        return Ok(DockerArtifact::Jlink { launcher_name: jlink::launcher_name(desc).to_string() });
    }

    let jar_filename = jar
        .file_name()
        .context("JAR path has no filename")?
        .to_string_lossy()
        .into_owned();
    Ok(DockerArtifact::Jar { jar_filename, has_libs: !dep_jars.is_empty() })
}

/// Resolve the effective base image: the user's explicit `baseImage` if set,
/// else a default that depends on the artifact being packaged.
fn resolved_base_image<'a>(desc: &'a Descriptor, artifact: &DockerArtifact) -> &'a str {
    if let Some(explicit) = desc.docker.base_image.as_deref() {
        return explicit;
    }
    match artifact {
        DockerArtifact::Jar { .. } => DEFAULT_BASE_IMAGE_JAR,
        DockerArtifact::Native { .. } | DockerArtifact::Jlink { .. } => DEFAULT_BASE_IMAGE_SELF_CONTAINED,
    }
}

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

    crate::parallel::emit(&crate::style::run_step(&image_ref, ""));

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("--rm").arg(&image_ref);

    for arg in extra_args {
        cmd.arg(arg);
    }

    let status = crate::proc::spawn_cmd(&mut cmd)
        .context("failed to invoke docker run — is Docker installed?")?;

    if !status.success() {
        std::process::exit(1);
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
    crate::incremental::touch_stamp(&stamp_path(target_dir))
}

/// Inputs that invalidate the generated-Dockerfile build's stamp: the
/// generated Dockerfile, the generated .dockerignore, and whichever artifact
/// the Dockerfile packages (app JAR + `libs/`, the native binary, or the
/// jlink runtime image).
fn generated_dockerfile_inputs(target_dir: &Path, project_root: &Path, artifact: &DockerArtifact) -> Inputs {
    let mut inputs = Inputs::new();
    inputs
        .add_file(&target_dir.join("Dockerfile"))
        .add_file(&target_dir.join(".dockerignore"));
    match artifact {
        DockerArtifact::Jar { jar_filename, has_libs } => {
            inputs.add_file(&target_dir.join(jar_filename));
            if *has_libs {
                inputs.add_dir(&target_dir.join("libs"));
            }
        }
        DockerArtifact::Native { binary_name } => {
            inputs.add_file(&target_dir.join(binary_name));
        }
        DockerArtifact::Jlink { .. } => {
            inputs.add_dir(&jlink::runtime_dir(project_root));
        }
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
    crate::parallel::emit(&crate::style::info("Dockerfile", "using project root Dockerfile"));

    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let inputs = user_dockerfile_inputs(project_root, jar);
    if Stamp::of(&stamp_path(&target_dir)).covers(&inputs) {
        crate::parallel::emit(&crate::style::up_to_date("Docker image"));
        return Ok(());
    }

    crate::parallel::emit(&crate::style::active("Docker image", &format!("building {}", image_ref)));
    // Make JAR path relative to project root for the build arg.
    let jar_rel = jar
        .strip_prefix(project_root)
        .unwrap_or(jar)
        .to_string_lossy()
        .to_string();

    let mut build_cmd = Command::new("docker");
    build_cmd
        .arg("build")
        .arg("--progress=plain")
        .arg("--build-arg")
        .arg(format!("JAR_FILE={}", jar_rel))
        .arg("-t")
        .arg(image_ref)
        .arg(project_root);
    let status = crate::proc::spawn_cmd(&mut build_cmd)
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    touch_stamp(&target_dir)?;
    crate::parallel::emit(&crate::style::info("Docker image", image_ref));
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

    let artifact = select_docker_artifact(project_root, desc, jar, dep_jars)?;
    let base_image = resolved_base_image(desc, &artifact).to_string();

    // Copy dependency JARs into target/libs/ (skip up-to-date files) — only
    // for the Jar artifact. The native binary and jlink runtime image are
    // already self-contained under target/, so there's nothing extra to copy.
    // Use the same disambiguated names as jar::populate_libs_dir / MANIFEST
    // Class-Path so colliding bare filenames (same artifact+version, different
    // group) do not overwrite each other (bug #38).
    if let DockerArtifact::Jar { has_libs: true, .. } = &artifact {
        let libs_dir = target_dir.join("libs");
        std::fs::create_dir_all(&libs_dir).context("failed to create target/libs")?;

        let names = crate::jar::libs_entry_names(dep_jars);
        let mut copied = 0usize;
        let mut skipped = 0usize;
        for (dep, fname) in dep_jars.iter().zip(&names) {
            let dest = libs_dir.join(fname);
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
            (0, _) => crate::parallel::emit(&crate::style::up_to_date("Docker dep JARs")),
            (c, 0) => crate::parallel::emit(&crate::style::info("Docker dep JARs", &format!("{} copied", c))),
            (c, s) => crate::parallel::emit(&crate::style::info("Docker dep JARs", &format!("{} copied, {} up to date", c, s))),
        }
    }

    // Generate Dockerfile in target/ — skip write if content is unchanged.
    let dockerfile_content = match &artifact {
        DockerArtifact::Jar { jar_filename, has_libs } => {
            generate_dockerfile_jar(&base_image, jar_filename, *has_libs)
        }
        DockerArtifact::Native { binary_name } => generate_dockerfile_native(&base_image, binary_name),
        DockerArtifact::Jlink { launcher_name } => generate_dockerfile_jlink(&base_image, launcher_name),
    };
    let dockerfile_path = target_dir.join("Dockerfile");
    let existing = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
    if existing == dockerfile_content {
        crate::parallel::emit(&crate::style::up_to_date("Dockerfile"));
    } else {
        std::fs::write(&dockerfile_path, &dockerfile_content)
            .context("failed to write generated Dockerfile")?;
        crate::parallel::emit(&crate::style::info("Dockerfile", "generated  (target/Dockerfile)"));
    }

    // Generate .dockerignore in target/ — skip write if content is unchanged.
    let dockerignore_content = match &artifact {
        DockerArtifact::Jar { jar_filename, has_libs } => generate_dockerignore_jar(jar_filename, *has_libs),
        DockerArtifact::Native { binary_name } => generate_dockerignore_native(binary_name),
        DockerArtifact::Jlink { .. } => generate_dockerignore_jlink(),
    };
    let dockerignore_path = target_dir.join(".dockerignore");
    let existing_ignore = std::fs::read_to_string(&dockerignore_path).unwrap_or_default();
    if existing_ignore == dockerignore_content {
        crate::parallel::emit(&crate::style::up_to_date(".dockerignore"));
    } else {
        std::fs::write(&dockerignore_path, &dockerignore_content)
            .context("failed to write .dockerignore")?;
        crate::parallel::emit(&crate::style::info(".dockerignore", "generated  (target/.dockerignore)"));
    }

    // Skip docker build if the stamp is newer than all inputs.
    // We use a stamp file (target/.docker-stamp) rather than the Docker image's
    // Created timestamp, because Docker does not update Created when all layers
    // are cached — making the image appear older than it really is.
    let stamp = stamp_path(&target_dir);
    let inputs = generated_dockerfile_inputs(&target_dir, project_root, &artifact);
    if Stamp::of(&stamp).covers(&inputs) {
        crate::parallel::emit(&crate::style::up_to_date("Docker image"));
        return Ok(());
    }

    crate::parallel::emit(&crate::style::active("Docker image", &format!("building {}", image_ref)));
    let mut build_cmd2 = Command::new("docker");
    build_cmd2.arg("build").arg("--progress=plain").arg("-t").arg(image_ref).arg(&target_dir);
    let status = crate::proc::spawn_cmd(&mut build_cmd2)
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    touch_stamp(&target_dir)?;
    crate::parallel::emit(&crate::style::info("Docker image", image_ref));
    Ok(())
}

/// Generate the content of `target/.dockerignore` for the plain-JAR artifact.
///
/// Starts with `*` to exclude everything, then whitelists only the app JAR
/// and (when present) the `libs/` directory.
fn generate_dockerignore_jar(jar_filename: &str, has_libs: bool) -> String {
    let mut lines = vec!["*".to_string(), format!("!{}", jar_filename)];
    if has_libs {
        lines.push("!libs/".to_string());
    }
    lines.join("\n") + "\n"
}

fn generate_dockerfile_jar(base_image: &str, jar_filename: &str, has_libs: bool) -> String {
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

/// Generate the content of `target/.dockerignore` for the native-image
/// artifact: whitelist only the binary.
fn generate_dockerignore_native(binary_name: &str) -> String {
    format!("*\n!{}\n", binary_name)
}

/// The native binary is already self-contained (no JVM needed), so the
/// Dockerfile just copies it in and runs it directly.
fn generate_dockerfile_native(base_image: &str, binary_name: &str) -> String {
    format!(
        "FROM {base_image}\n\
         WORKDIR /app\n\
         COPY {binary_name} ./{binary_name}\n\
         ENTRYPOINT [\"./{binary_name}\"]\n"
    )
}

/// Generate the content of `target/.dockerignore` for the jlink artifact:
/// whitelist only `runtime/` (the whole `target/runtime/{jdk,lib,bin}` tree).
fn generate_dockerignore_jlink() -> String {
    "*\n!runtime/\n".to_string()
}

/// The jlink runtime image already bundles its own JDK (`runtime/jdk/`), so
/// the Dockerfile just copies the whole tree in and runs the launcher — no
/// JRE base image or extra classpath handling needed.
fn generate_dockerfile_jlink(base_image: &str, launcher_name: &str) -> String {
    format!(
        "FROM {base_image}\n\
         WORKDIR /app\n\
         COPY runtime/ runtime/\n\
         ENTRYPOINT [\"runtime/bin/{launcher_name}\"]\n"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockerignore_no_libs() {
        let content = generate_dockerignore_jar("myapp-1.0.jar", false);
        assert_eq!(content, "*\n!myapp-1.0.jar\n");
    }

    #[test]
    fn dockerignore_with_libs() {
        let content = generate_dockerignore_jar("myapp-1.0.jar", true);
        assert_eq!(content, "*\n!myapp-1.0.jar\n!libs/\n");
    }

    #[test]
    fn dockerfile_no_deps() {
        let content = generate_dockerfile_jar("eclipse-temurin:21-jre", "myapp-1.0.jar", false);
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
        let content = generate_dockerfile_jar("eclipse-temurin:21-jre", "myapp-1.0.jar", true);
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
    fn dockerignore_native() {
        assert_eq!(generate_dockerignore_native("my-cli"), "*\n!my-cli\n");
    }

    #[test]
    fn dockerfile_native() {
        let content = generate_dockerfile_native("debian:trixie-slim", "my-cli");
        assert_eq!(
            content,
            "FROM debian:trixie-slim\n\
             WORKDIR /app\n\
             COPY my-cli ./my-cli\n\
             ENTRYPOINT [\"./my-cli\"]\n"
        );
    }

    #[test]
    fn dockerignore_jlink() {
        assert_eq!(generate_dockerignore_jlink(), "*\n!runtime/\n");
    }

    #[test]
    fn dockerfile_jlink() {
        let content = generate_dockerfile_jlink("debian:trixie-slim", "my-cli");
        assert_eq!(
            content,
            "FROM debian:trixie-slim\n\
             WORKDIR /app\n\
             COPY runtime/ runtime/\n\
             ENTRYPOINT [\"runtime/bin/my-cli\"]\n"
        );
    }

    /// Regression for bug #38: docker must use the same destination names as
    /// `populate_libs_dir` / MANIFEST Class-Path when bare filenames collide.
    #[test]
    fn docker_libs_dest_names_disambiguate_collisions() {
        use std::path::PathBuf;
        let jars = vec![
            PathBuf::from("/home/u/.m2/repository/javax/inject/javax.inject/1/javax.inject-1.jar"),
            PathBuf::from("/home/u/.m2/repository/com/example/javax.inject/1/javax.inject-1.jar"),
        ];
        let names = crate::jar::libs_entry_names(&jars);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "colliding deps must not share a libs/ name");
        assert_eq!(names[0], "javax.inject-javax.inject-1.jar");
        assert_eq!(names[1], "com.example-javax.inject-1.jar");
        // Same mapping as jar packaging — docker and Class-Path stay aligned.
        assert_eq!(names, crate::jar::libs_entry_names(&jars));
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

        let artifact = DockerArtifact::Jar { jar_filename: "app-1.0.jar".to_string(), has_libs: false };

        // No stamp yet → must build.
        let inputs = generated_dockerfile_inputs(target, target, &artifact);
        assert!(!Stamp::of(&stamp).covers(&inputs));

        // Write stamp with mtime strictly after all inputs → skip.
        fs::write(&stamp, b"").unwrap();
        set_file_mtime(&stamp, t1).unwrap();
        assert!(Stamp::of(&stamp).covers(&inputs));

        // Update the jar to be newer than the stamp → build again.
        set_file_mtime(&jar, FileTime::from_unix_time(1_000_002, 0)).unwrap();
        let inputs = generated_dockerfile_inputs(target, target, &artifact);
        assert!(!Stamp::of(&stamp).covers(&inputs));

        // Layer-1 regression guard: a tied jar mtime (same second as the
        // stamp) must also force a rebuild.
        set_file_mtime(&jar, t1).unwrap(); // jar mtime == stamp mtime
        let inputs = generated_dockerfile_inputs(target, target, &artifact);
        assert!(
            !Stamp::of(&stamp).covers(&inputs),
            "tied input mtime must not be considered covered",
        );
    }

    #[test]
    fn resolved_base_image_defaults_by_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"0.1\"\nmainClass = \"X\"\n[docker]\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(root).unwrap();

        assert_eq!(
            resolved_base_image(&desc, &DockerArtifact::Jar { jar_filename: "x.jar".to_string(), has_libs: false }),
            "eclipse-temurin:21-jre-alpine"
        );
        assert_eq!(
            resolved_base_image(&desc, &DockerArtifact::Native { binary_name: "x".to_string() }),
            "debian:trixie-slim"
        );
        assert_eq!(
            resolved_base_image(&desc, &DockerArtifact::Jlink { launcher_name: "x".to_string() }),
            "debian:trixie-slim"
        );
    }

    #[test]
    fn resolved_base_image_honours_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"0.1\"\nmainClass = \"X\"\n\
             [docker]\nbaseImage = \"alpine:3.20\"\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(root).unwrap();

        assert_eq!(
            resolved_base_image(&desc, &DockerArtifact::Native { binary_name: "x".to_string() }),
            "alpine:3.20"
        );
    }

    #[test]
    fn select_docker_artifact_falls_back_to_jar_when_neither_configured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"0.1\"\nmainClass = \"X\"\n[docker]\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(root).unwrap();
        let jar = root.join("target").join("x-0.1.jar");

        let artifact = select_docker_artifact(root, &desc, &jar, &[]).unwrap();
        assert!(matches!(artifact, DockerArtifact::Jar { has_libs: false, .. }));
    }

    #[test]
    fn select_docker_artifact_native_wins_over_jlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"0.1\"\nmainClass = \"X\"\n\
             [docker]\n[native-image]\n[jlink]\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(root).unwrap();
        let jar = root.join("target").join("x-0.1.jar");

        // Both configured, but neither artifact has been built yet → error
        // must mention native-image specifically (it takes precedence).
        let err = select_docker_artifact(root, &desc, &jar, &[]).unwrap_err().to_string();
        assert!(err.contains("native-image"), "got: {err}");

        // Build only the native binary; native must still be selected.
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target").join("x"), b"").unwrap();
        let artifact = select_docker_artifact(root, &desc, &jar, &[]).unwrap();
        assert!(matches!(artifact, DockerArtifact::Native { .. }));
    }

    #[test]
    fn select_docker_artifact_uses_jlink_when_only_jlink_configured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"0.1\"\nmainClass = \"X\"\n\
             [docker]\n[jlink]\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(root).unwrap();
        let jar = root.join("target").join("x-0.1.jar");

        let err = select_docker_artifact(root, &desc, &jar, &[]).unwrap_err().to_string();
        assert!(err.contains("jlink"), "got: {err}");

        std::fs::create_dir_all(jlink::runtime_dir(root)).unwrap();
        let artifact = select_docker_artifact(root, &desc, &jar, &[]).unwrap();
        assert!(matches!(artifact, DockerArtifact::Jlink { .. }));
    }
}
