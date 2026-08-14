//! Daemonless OCI image build (Jib-style).
//!
//! Pulls a base image from an OCI Distribution registry, appends deterministic
//! application layers, and writes an OCI image layout under `target/image/`
//! plus a loadable `target/image.tar` — no Docker daemon required.

mod auth;
mod cache;
mod image;
mod layer;
mod layout;
mod reference;
mod registry;

use crate::descriptor::{self, Descriptor, DockerBuilder};
use crate::incremental::{Inputs, Stamp};
use crate::{jlink, native};
use anyhow::{bail, Context, Result};
use image::{assemble_image, ImageOptions};
use layer::{build_layer, build_layer_from_paths, collect_dir_files};
use reference::ImageReference;
use registry::RegistryClient;
use std::path::{Path, PathBuf};

/// Build a container image. Dispatches to the daemonless OCI assembler or the
/// classic `docker build` path based on `[docker] builder` and whether a
/// project-root Dockerfile exists.
pub fn build_image(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    offline: bool,
) -> Result<String> {
    match descriptor::effective_docker_builder(project_root, desc) {
        DockerBuilder::Docker => crate::docker::docker_build(project_root, desc, jar, dep_jars),
        DockerBuilder::Daemonless => oci_build(project_root, desc, jar, dep_jars, offline),
    }
}

/// Run the built image. Daemonless path loads `target/image.tar` into Docker
/// first; docker path builds via the daemon as before.
pub fn run_image(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    extra_args: &[String],
    offline: bool,
) -> Result<()> {
    match descriptor::effective_docker_builder(project_root, desc) {
        DockerBuilder::Docker => {
            crate::docker::docker_run(project_root, desc, jar, dep_jars, extra_args)
        }
        DockerBuilder::Daemonless => {
            let image_ref = oci_build(project_root, desc, jar, dep_jars, offline)?;
            docker_load_and_run(project_root, &image_ref, extra_args)
        }
    }
}

fn docker_load_and_run(project_root: &Path, image_ref: &str, extra_args: &[String]) -> Result<()> {
    let tar_path = project_root.join("target").join("image.tar");
    if !tar_path.exists() {
        bail!(
            "daemonless image tar not found at {} — run `curie build` first",
            tar_path.display()
        );
    }

    crate::parallel::emit(&crate::style::active(
        "Docker load",
        &tar_path
            .strip_prefix(project_root)
            .unwrap_or(&tar_path)
            .display()
            .to_string(),
    ));

    let mut load = std::process::Command::new("docker");
    load.arg("load").arg("-i").arg(&tar_path);
    let status = crate::proc::spawn_cmd(&mut load)
        .context("failed to invoke docker load — is Docker installed?")?;
    if !status.success() {
        bail!("docker load failed for {}", tar_path.display());
    }

    crate::parallel::emit(&crate::style::run_step(image_ref, ""));
    let mut run = std::process::Command::new("docker");
    run.arg("run").arg("--rm").arg(image_ref);
    for arg in extra_args {
        run.arg(arg);
    }
    let status = crate::proc::spawn_cmd(&mut run)
        .context("failed to invoke docker run — is Docker installed?")?;
    if !status.success() {
        std::process::exit(1);
    }
    Ok(())
}

/// Assemble an OCI image without a Docker daemon.
fn oci_build(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    offline: bool,
) -> Result<String> {
    let image_ref = desc.image_ref();
    let target_dir = project_root.join("target");
    std::fs::create_dir_all(&target_dir).context("failed to create target/")?;

    let plan = layer_plan(project_root, desc, jar, dep_jars)?;
    let base_image_str = plan.base_image().to_string();

    // Incremental skip: stamp covers JAR/libs/binary + Curie.toml docker fields.
    let stamp = oci_stamp_path(&target_dir);
    let inputs = oci_inputs(project_root, desc, &plan);
    if Stamp::of(&stamp).covers(&inputs)
        && target_dir.join("image").join("index.json").exists()
        && target_dir.join("image.tar").exists()
    {
        crate::parallel::emit(&crate::style::up_to_date("OCI image"));
        return Ok(image_ref);
    }

    crate::parallel::emit(&crate::style::active(
        "OCI image",
        &format!("pulling base {base_image_str}"),
    ));

    let reference = ImageReference::parse(&base_image_str)
        .with_context(|| format!("invalid [docker] baseImage \"{base_image_str}\""))?;
    let client = RegistryClient::new(offline)?;
    let base = client
        .pull_base(
            &reference,
            desc.docker.resolved_platform(),
            desc.docker.registry_id.as_deref(),
        )
        .with_context(|| format!("failed to pull base image {base_image_str}"))?;

    crate::parallel::emit(&crate::style::active(
        "OCI image",
        &format!("assembling {image_ref}"),
    ));

    let new_layers = plan.build_layers()?;
    let opts = image_options(desc, &plan);
    let assembled = assemble_image(&base.config, &base.layers, &new_layers, &opts, base.use_oci)?;

    let image_dir = target_dir.join("image");
    layout::write_oci_layout(&image_dir, &assembled, &image_ref)?;
    let tar_path = target_dir.join("image.tar");
    layout::write_layout_tar(&image_dir, &tar_path)?;

    crate::incremental::touch_stamp(&stamp)?;
    crate::parallel::emit(&crate::style::info(
        "OCI image",
        &format!(
            "{image_ref}  (target/image.tar, {})",
            short_digest(&assembled.manifest_digest)
        ),
    ));

    Ok(image_ref)
}

fn short_digest(d: &str) -> &str {
    d.get(..19).unwrap_or(d)
}

fn oci_stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".oci-stamp")
}

fn oci_inputs(project_root: &Path, desc: &Descriptor, plan: &LayerPlan) -> Inputs {
    let mut inputs = Inputs::new();
    inputs.add_file(&project_root.join("Curie.toml"));
    match plan {
        LayerPlan::Jar {
            jar_path, dep_jars, ..
        } => {
            inputs.add_file(jar_path);
            for d in dep_jars {
                inputs.add_file(d);
            }
        }
        LayerPlan::Native { binary_path, .. } => {
            inputs.add_file(binary_path);
        }
        LayerPlan::Jlink { runtime_dir, .. } => {
            inputs.add_dir(runtime_dir);
        }
    }
    // Include resolved builder-relevant config in the input set via a hash file
    // is overkill; Curie.toml covers config changes.
    let _ = desc;
    inputs
}

/// What we put into the image on top of the base.
enum LayerPlan {
    Jar {
        base_image: String,
        jar_path: PathBuf,
        dep_jars: Vec<PathBuf>,
    },
    Native {
        base_image: String,
        binary_path: PathBuf,
        binary_name: String,
    },
    Jlink {
        base_image: String,
        runtime_dir: PathBuf,
        launcher_name: String,
    },
}

impl LayerPlan {
    fn base_image(&self) -> &str {
        match self {
            Self::Jar { base_image, .. }
            | Self::Native { base_image, .. }
            | Self::Jlink { base_image, .. } => base_image,
        }
    }

    fn build_layers(&self) -> Result<Vec<layer::BuiltLayer>> {
        match self {
            Self::Jar {
                jar_path, dep_jars, ..
            } => build_jar_layers(jar_path, dep_jars),
            Self::Native {
                binary_path,
                binary_name,
                ..
            } => {
                let layer = build_layer_from_paths(&[(
                    format!("app/{binary_name}"),
                    binary_path.as_path(),
                    0o755,
                )])?;
                Ok(vec![layer])
            }
            Self::Jlink { runtime_dir, .. } => {
                let files = collect_dir_files(runtime_dir, "app/runtime")?;
                let layer = build_layer(&files)?;
                Ok(vec![layer])
            }
        }
    }
}

fn layer_plan(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<LayerPlan> {
    // Reuse the same artifact selection as the docker path.
    if descriptor::native_image_enabled(desc) {
        let binary_path = native::output_path(project_root, desc);
        if !binary_path.exists() {
            bail!(
                "oci: [native-image] is enabled but no binary found at {}\n\
                 Build it first — run `curie build` (without --no-native) or `curie native`.",
                binary_path.display()
            );
        }
        let binary_name = binary_path
            .file_name()
            .context("native binary path has no file name")?
            .to_string_lossy()
            .into_owned();
        let base = resolved_base_image(desc, ArtifactKind::Native);
        return Ok(LayerPlan::Native {
            base_image: base,
            binary_path,
            binary_name,
        });
    }

    if descriptor::jlink_enabled(desc) {
        let runtime_dir = jlink::runtime_dir(project_root);
        if !runtime_dir.exists() {
            bail!(
                "oci: [jlink] is enabled but no runtime image found at {}\n\
                 Build it first — run `curie build` (without --no-jlink) or `curie jlink`.",
                runtime_dir.display()
            );
        }
        let base = resolved_base_image(desc, ArtifactKind::Jlink);
        return Ok(LayerPlan::Jlink {
            base_image: base,
            runtime_dir,
            launcher_name: jlink::launcher_name(desc).to_string(),
        });
    }

    let base = resolved_base_image(desc, ArtifactKind::Jar);
    Ok(LayerPlan::Jar {
        base_image: base,
        jar_path: jar.to_path_buf(),
        dep_jars: dep_jars.to_vec(),
    })
}

enum ArtifactKind {
    Jar,
    Native,
    Jlink,
}

fn resolved_base_image(desc: &Descriptor, kind: ArtifactKind) -> String {
    if let Some(explicit) = desc.docker.base_image.as_deref() {
        return explicit.to_string();
    }
    match kind {
        ArtifactKind::Jar => "eclipse-temurin:21-jre-alpine".to_string(),
        ArtifactKind::Native | ArtifactKind::Jlink => "debian:trixie-slim".to_string(),
    }
}

/// Jib packaged-mode style: deps layer then app JAR layer.
fn build_jar_layers(jar: &Path, dep_jars: &[PathBuf]) -> Result<Vec<layer::BuiltLayer>> {
    let mut layers = Vec::new();

    if !dep_jars.is_empty() {
        let names = crate::jar::libs_entry_names(dep_jars);
        let mut entries: Vec<(String, &Path, u32)> = Vec::with_capacity(dep_jars.len());
        for (dep, name) in dep_jars.iter().zip(&names) {
            entries.push((format!("app/libs/{name}"), dep.as_path(), 0o644));
        }
        layers.push(build_layer_from_paths(&entries)?);
    }

    let app_layer = build_layer_from_paths(&[("app/app.jar".into(), jar, 0o644)])?;
    layers.push(app_layer);
    Ok(layers)
}

fn image_options(desc: &Descriptor, plan: &LayerPlan) -> ImageOptions {
    let mut opts = ImageOptions {
        working_dir: Some("/app".into()),
        env: desc.docker.env.clone(),
        labels: desc.docker.labels.clone(),
        user: desc.docker.user.clone(),
        ..Default::default()
    };

    match plan {
        LayerPlan::Jar { .. } => {
            let mut entry = vec!["java".to_string()];
            entry.extend(desc.docker.jvm_args.iter().cloned());
            entry.push("-jar".into());
            entry.push("app.jar".into());
            opts.entrypoint = entry;
        }
        LayerPlan::Native { binary_name, .. } => {
            opts.entrypoint = vec![format!("./{binary_name}")];
        }
        LayerPlan::Jlink { launcher_name, .. } => {
            opts.entrypoint = vec![format!("/app/runtime/bin/{launcher_name}")];
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_layers_use_libs_entry_names() {
        let dir = tempfile::tempdir().unwrap();
        // Two jars with the same bare filename → disambiguated names.
        let a_dir = dir.path().join("repository/com/example/foo/1.0");
        let b_dir = dir.path().join("repository/org/other/foo/1.0");
        std::fs::create_dir_all(&a_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();
        let a = a_dir.join("foo-1.0.jar");
        let b = b_dir.join("foo-1.0.jar");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bbb").unwrap();
        let app = dir.path().join("app.jar");
        std::fs::write(&app, b"app").unwrap();

        let layers = build_jar_layers(&app, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(layers.len(), 2, "deps layer + app layer");

        // Determinism
        let again = build_jar_layers(&app, &[a, b]).unwrap();
        assert_eq!(layers[0].digest, again[0].digest);
        assert_eq!(layers[1].digest, again[1].digest);
    }

    #[test]
    fn empty_deps_single_app_layer() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app.jar");
        std::fs::write(&app, b"app").unwrap();
        let layers = build_jar_layers(&app, &[]).unwrap();
        assert_eq!(layers.len(), 1);
    }

    #[test]
    fn image_reference_used_for_base() {
        let r = ImageReference::parse("eclipse-temurin:21-jre-alpine").unwrap();
        assert_eq!(r.repository, "library/eclipse-temurin");
    }
}
