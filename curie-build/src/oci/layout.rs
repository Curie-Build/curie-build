//! Write an OCI image layout directory and a loadable tar archive.

use super::cache;
use super::image::AssembledImage;
use anyhow::{Context, Result};
use std::path::Path;

/// Write `target/image/` as an OCI image layout and pack it into `target/image.tar`.
///
/// Layout:
/// ```text
/// oci-layout
/// index.json
/// blobs/sha256/<hex>   # config, layers (base + new), manifest
/// ```
pub fn write_oci_layout(
    image_dir: &Path,
    assembled: &AssembledImage,
    image_ref: &str,
) -> Result<()> {
    // Clean previous layout.
    if image_dir.exists() {
        std::fs::remove_dir_all(image_dir)
            .with_context(|| format!("failed to remove {}", image_dir.display()))?;
    }
    let blobs = image_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)
        .with_context(|| format!("failed to create {}", blobs.display()))?;

    // oci-layout
    std::fs::write(
        image_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    // Config blob
    write_blob_file(&blobs, &assembled.config_digest, &assembled.config_bytes)?;

    // Layer blobs: new ones from assembled; base ones from cache.
    let new_digests: std::collections::HashSet<&str> = assembled
        .new_layer_blobs
        .iter()
        .map(|(d, _)| d.as_str())
        .collect();
    for (digest, bytes) in &assembled.new_layer_blobs {
        write_blob_file(&blobs, digest, bytes)?;
        // Also keep in global cache.
        let _ = cache::put_blob(digest, bytes);
    }
    for layer in &assembled.layers {
        if new_digests.contains(layer.digest.as_str()) {
            continue;
        }
        // Materialize from cache.
        let hex = layer
            .digest
            .strip_prefix("sha256:")
            .context("layer digest")?;
        let dest = blobs.join(hex);
        cache::materialize_blob(&layer.digest, &dest)?;
    }

    // Manifest blob
    write_blob_file(
        &blobs,
        &assembled.manifest_digest,
        &assembled.manifest_bytes,
    )?;

    // index.json pointing at the manifest, with optional org.opencontainers.image.ref.name
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": assembled.manifest_media_type,
            "digest": assembled.manifest_digest,
            "size": assembled.manifest_bytes.len(),
            "annotations": {
                "org.opencontainers.image.ref.name": image_ref,
            }
        }]
    });
    std::fs::write(
        image_dir.join("index.json"),
        serde_json::to_vec_pretty(&index)?,
    )?;

    // Docker-compatible manifest.json + repositories so `docker load` works
    // (plain OCI layout alone is rejected by Docker with "does not contain a
    // manifest.json"). LayerSources carries the original media types so gzip
    // layers load correctly.
    write_docker_manifest_sidecar(image_dir, assembled, image_ref)?;

    Ok(())
}

/// Write `manifest.json` + `repositories` next to the OCI layout for `docker load`.
fn write_docker_manifest_sidecar(
    image_dir: &Path,
    assembled: &AssembledImage,
    image_ref: &str,
) -> Result<()> {
    let config_rel = format!(
        "blobs/sha256/{}",
        assembled
            .config_digest
            .strip_prefix("sha256:")
            .unwrap_or(&assembled.config_digest)
    );
    let layers: Vec<String> = assembled
        .layers
        .iter()
        .map(|l| {
            format!(
                "blobs/sha256/{}",
                l.digest.strip_prefix("sha256:").unwrap_or(&l.digest)
            )
        })
        .collect();

    let mut layer_sources = serde_json::Map::new();
    for l in &assembled.layers {
        layer_sources.insert(
            l.digest.clone(),
            serde_json::json!({
                "mediaType": l.media_type,
                "size": l.size,
                "digest": l.digest,
            }),
        );
    }

    let docker_manifest = serde_json::json!([{
        "Config": config_rel,
        "RepoTags": [image_ref],
        "Layers": layers,
        "LayerSources": layer_sources,
    }]);
    std::fs::write(
        image_dir.join("manifest.json"),
        serde_json::to_vec(&docker_manifest)?,
    )?;

    // repositories: { "name": { "tag": "<last-layer-hex>" } }
    if let Some((name, tag)) = image_ref.rsplit_once(':') {
        let last_hex = assembled
            .layers
            .last()
            .map(|l| {
                l.digest
                    .strip_prefix("sha256:")
                    .unwrap_or(&l.digest)
                    .to_string()
            })
            .unwrap_or_default();
        let repos = serde_json::json!({ name: { tag: last_hex } });
        std::fs::write(image_dir.join("repositories"), serde_json::to_vec(&repos)?)?;
    }

    Ok(())
}

fn write_blob_file(blobs_dir: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    let hex = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("bad digest {digest}"))?;
    let path = blobs_dir.join(hex);
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write blob {}", path.display()))?;
    }
    Ok(())
}

/// Tar the OCI layout directory into `image.tar` (for `docker load`).
pub fn write_layout_tar(image_dir: &Path, tar_path: &Path) -> Result<()> {
    if let Some(parent) = tar_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = tar_path.with_extension("tar.part");
    {
        let file = std::fs::File::create(&staging)
            .with_context(|| format!("failed to create {}", staging.display()))?;
        let mut archive = tar::Builder::new(file);
        archive.follow_symlinks(false);
        append_dir(&mut archive, image_dir, Path::new(""))?;
        archive.finish()?;
    }
    std::fs::rename(&staging, tar_path).with_context(|| {
        format!(
            "failed to rename {} → {}",
            staging.display(),
            tar_path.display()
        )
    })?;
    Ok(())
}

fn append_dir<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &Path,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = prefix.join(entry.file_name());
        let name_str = name.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            archive.append_dir(&name_str, &path)?;
            append_dir(archive, &path, &name)?;
        } else {
            let mut file = std::fs::File::open(&path)?;
            archive.append_file(&name_str, &mut file)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::image::{assemble_image, ImageOptions, LayerDescriptor, MEDIA_DOCKER_LAYER};
    use crate::oci::layer::{build_layer, LayerFile};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn write_layout_creates_oci_files() {
        let layer = build_layer(&[LayerFile {
            path: "app/app.jar".into(),
            data: b"jar".to_vec(),
            mode: 0o644,
        }])
        .unwrap();
        // Put "base" layer in cache.
        cache::put_blob(&layer.digest, &layer.compressed).unwrap();

        let base_config = json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": [layer.diff_id]},
            "history": []
        });
        // Second layer (app)
        let app_layer = build_layer(&[LayerFile {
            path: "app/x.txt".into(),
            data: b"x".to_vec(),
            mode: 0o644,
        }])
        .unwrap();

        let opts = ImageOptions {
            entrypoint: vec!["java".into(), "-jar".into(), "app.jar".into()],
            working_dir: Some("/app".into()),
            ..Default::default()
        };
        let base_layers = vec![LayerDescriptor {
            media_type: MEDIA_DOCKER_LAYER.into(),
            digest: layer.digest.clone(),
            size: layer.size,
            diff_id: layer.diff_id.clone(),
        }];
        let assembled =
            assemble_image(&base_config, &base_layers, &[app_layer], &opts, false).unwrap();

        let dir = tempdir().unwrap();
        let image_dir = dir.path().join("image");
        write_oci_layout(&image_dir, &assembled, "demo:0.1.0").unwrap();
        assert!(image_dir.join("oci-layout").exists());
        assert!(image_dir.join("index.json").exists());
        assert!(image_dir.join("manifest.json").exists());
        assert!(image_dir.join("blobs/sha256").exists());

        let tar_path = dir.path().join("image.tar");
        write_layout_tar(&image_dir, &tar_path).unwrap();
        assert!(tar_path.exists());
        assert!(tar_path.metadata().unwrap().len() > 0);
    }
}
