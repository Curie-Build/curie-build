//! Image config JSON and manifest assembly on top of a base image.

use super::layer::sha256_digest;
use super::layer::BuiltLayer;
use super::layer::REPRODUCIBLE_EPOCH;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Media types we emit / accept.
pub const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const MEDIA_DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const MEDIA_DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
pub const MEDIA_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MEDIA_OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MEDIA_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// A layer descriptor already present in a base image (or freshly built).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LayerDescriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    /// Uncompressed diff_id from the base config (or our BuiltLayer).
    pub diff_id: String,
}

/// Options applied on top of the base image config.
#[derive(Debug, Clone, Default)]
pub struct ImageOptions {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
}

/// Fully assembled image (config + manifest + new layer blobs).
#[derive(Debug)]
#[allow(dead_code)]
pub struct AssembledImage {
    pub config_bytes: Vec<u8>,
    pub config_digest: String,
    pub config_media_type: String,
    pub manifest_bytes: Vec<u8>,
    pub manifest_digest: String,
    pub manifest_media_type: String,
    /// All layers in order (base + new), with media types for the manifest.
    pub layers: Vec<LayerDescriptor>,
    /// Newly built layers whose compressed bytes must be stored/pushed.
    pub new_layer_blobs: Vec<(String, Vec<u8>)>,
}

/// Build a new image config + manifest by appending `new_layers` to `base_config`.
pub fn assemble_image(
    base_config: &Value,
    base_layers: &[LayerDescriptor],
    new_layers: &[BuiltLayer],
    opts: &ImageOptions,
    // When true, emit OCI media types; otherwise Docker schema 2.
    use_oci: bool,
) -> Result<AssembledImage> {
    let mut config = base_config.clone();
    if !config.is_object() {
        bail!("base image config is not a JSON object");
    }

    // Fixed creation timestamps for reproducibility (Jib does the same).
    let created = format_epoch_rfc3339(REPRODUCIBLE_EPOCH);
    config["created"] = json!(created);

    // Append diff_ids.
    let rootfs = config
        .get_mut("rootfs")
        .context("base config missing rootfs")?;
    let diff_ids = rootfs
        .get_mut("diff_ids")
        .context("base config missing rootfs.diff_ids")?
        .as_array_mut()
        .context("rootfs.diff_ids is not an array")?;
    for layer in new_layers {
        diff_ids.push(json!(&layer.diff_id));
    }

    // history entries for the new layers.
    let history = config
        .as_object_mut()
        .unwrap()
        .entry("history")
        .or_insert_with(|| json!([]));
    let history = history
        .as_array_mut()
        .context("config.history is not an array")?;
    for _ in new_layers {
        history.push(json!({
            "created": created,
            "created_by": "curie",
            "comment": "curie layer",
        }));
    }

    // config.config (runtime settings)
    let runtime = config
        .as_object_mut()
        .unwrap()
        .entry("config")
        .or_insert_with(|| json!({}));
    let runtime = runtime
        .as_object_mut()
        .context("config.config is not an object")?;

    if !opts.entrypoint.is_empty() {
        runtime.insert("Entrypoint".into(), json!(opts.entrypoint));
    }
    if !opts.cmd.is_empty() {
        runtime.insert("Cmd".into(), json!(opts.cmd));
    }
    if let Some(wd) = &opts.working_dir {
        runtime.insert("WorkingDir".into(), json!(wd));
    }
    if let Some(user) = &opts.user {
        runtime.insert("User".into(), json!(user));
    }

    // Env: merge base + ours (ours win on key).
    let mut env_map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(arr) = runtime.get("Env").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                if let Some((k, v)) = s.split_once('=') {
                    env_map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    for (k, v) in &opts.env {
        env_map.insert(k.clone(), v.clone());
    }
    if !env_map.is_empty() {
        let env_list: Vec<String> = env_map.iter().map(|(k, v)| format!("{k}={v}")).collect();
        runtime.insert("Env".into(), json!(env_list));
    }

    // Labels
    if !opts.labels.is_empty() {
        let labels_val = runtime
            .entry("Labels".to_string())
            .or_insert_with(|| json!({}));
        let labels_obj = labels_val
            .as_object_mut()
            .context("config.Labels is not an object")?;
        for (k, v) in &opts.labels {
            labels_obj.insert(k.clone(), json!(v));
        }
    }

    let config_bytes = serde_json::to_vec(&config).context("serialize image config")?;
    let config_digest = sha256_digest(&config_bytes);
    let config_media_type = if use_oci {
        MEDIA_OCI_CONFIG
    } else {
        MEDIA_DOCKER_CONFIG
    }
    .to_string();

    let layer_media = if use_oci {
        MEDIA_OCI_LAYER
    } else {
        MEDIA_DOCKER_LAYER
    };

    let mut all_layers: Vec<LayerDescriptor> = base_layers.to_vec();
    let mut new_layer_blobs = Vec::new();
    for layer in new_layers {
        all_layers.push(LayerDescriptor {
            media_type: layer_media.to_string(),
            digest: layer.digest.clone(),
            size: layer.size,
            diff_id: layer.diff_id.clone(),
        });
        new_layer_blobs.push((layer.digest.clone(), layer.compressed.clone()));
    }

    // Manifest layers use original media types for base layers.
    let manifest_layers: Vec<Value> = all_layers
        .iter()
        .map(|l| {
            json!({
                "mediaType": l.media_type,
                "digest": l.digest,
                "size": l.size,
            })
        })
        .collect();

    let manifest_media_type = if use_oci {
        MEDIA_OCI_MANIFEST
    } else {
        MEDIA_DOCKER_MANIFEST
    }
    .to_string();

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": manifest_media_type,
        "config": {
            "mediaType": config_media_type,
            "digest": config_digest,
            "size": config_bytes.len(),
        },
        "layers": manifest_layers,
    });

    let manifest_bytes = serde_json::to_vec(&manifest).context("serialize manifest")?;
    let manifest_digest = sha256_digest(&manifest_bytes);

    Ok(AssembledImage {
        config_bytes,
        config_digest,
        config_media_type,
        manifest_bytes,
        manifest_digest,
        manifest_media_type,
        layers: all_layers,
        new_layer_blobs,
    })
}

fn format_epoch_rfc3339(epoch: u64) -> String {
    // 2024-01-01T00:00:00Z
    let secs = epoch as i64;
    let datetime = time::OffsetDateTime::from_unix_timestamp(secs).expect("epoch is valid");
    datetime
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "2024-01-01T00:00:00Z".into())
}

/// Parse platform string `os/arch[/variant]` into components.
pub fn parse_platform(platform: &str) -> Result<(String, String, Option<String>)> {
    let mut parts = platform.split('/');
    let os = parts
        .next()
        .filter(|s| !s.is_empty())
        .context("platform missing os")?
        .to_string();
    let arch = parts
        .next()
        .filter(|s| !s.is_empty())
        .context("platform missing arch")?
        .to_string();
    let variant = parts.next().map(|s| s.to_string());
    if parts.next().is_some() {
        bail!("invalid platform \"{platform}\"; expected os/arch[/variant]");
    }
    Ok((os, arch, variant))
}

/// Select a manifest descriptor from an index/list matching `platform`.
pub fn select_platform_manifest(index: &Value, platform: &str) -> Result<(String, String)> {
    let (want_os, want_arch, want_variant) = parse_platform(platform)?;
    let manifests = index
        .get("manifests")
        .and_then(|v| v.as_array())
        .context("image index missing manifests array")?;

    let mut available = Vec::new();
    for m in manifests {
        let p = m.get("platform");
        let os = p
            .and_then(|p| p.get("os"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let arch = p
            .and_then(|p| p.get("architecture"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let variant = p.and_then(|p| p.get("variant")).and_then(|v| v.as_str());
        available.push(format!(
            "{os}/{arch}{}",
            variant.map(|v| format!("/{v}")).unwrap_or_default()
        ));

        if os == want_os && arch == want_arch {
            let variant_ok = match (&want_variant, variant) {
                (None, _) => true,
                (Some(w), Some(v)) => w == v,
                (Some(_), None) => false,
            };
            if variant_ok {
                let digest = m
                    .get("digest")
                    .and_then(|v| v.as_str())
                    .context("manifest entry missing digest")?
                    .to_string();
                let media_type = m
                    .get("mediaType")
                    .and_then(|v| v.as_str())
                    .unwrap_or(MEDIA_OCI_MANIFEST)
                    .to_string();
                return Ok((digest, media_type));
            }
        }
    }

    bail!(
        "base image has no manifest for platform \"{platform}\"; available: {}",
        available.join(", ")
    );
}

/// Extract layer descriptors + diff_ids from a base manifest + config.
pub fn base_layers_from(manifest: &Value, config: &Value) -> Result<Vec<LayerDescriptor>> {
    let layers = manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .context("manifest missing layers")?;
    let diff_ids = config
        .pointer("/rootfs/diff_ids")
        .and_then(|v| v.as_array())
        .context("config missing rootfs.diff_ids")?;
    if layers.len() != diff_ids.len() {
        bail!(
            "base image layer count ({}) does not match diff_ids ({})",
            layers.len(),
            diff_ids.len()
        );
    }
    let mut out = Vec::with_capacity(layers.len());
    for (layer, diff) in layers.iter().zip(diff_ids.iter()) {
        out.push(LayerDescriptor {
            media_type: layer
                .get("mediaType")
                .and_then(|v| v.as_str())
                .unwrap_or(MEDIA_OCI_LAYER)
                .to_string(),
            digest: layer
                .get("digest")
                .and_then(|v| v.as_str())
                .context("layer missing digest")?
                .to_string(),
            size: layer
                .get("size")
                .and_then(|v| v.as_u64())
                .context("layer missing size")?,
            diff_id: diff.as_str().context("diff_id not a string")?.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::layer::{build_layer, LayerFile};

    fn minimal_base_config() -> Value {
        json!({
            "created": "2020-01-01T00:00:00Z",
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Env": ["PATH=/usr/bin"],
                "WorkingDir": "/"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
            },
            "history": [{
                "created": "2020-01-01T00:00:00Z",
                "created_by": "base"
            }]
        })
    }

    #[test]
    fn assemble_appends_diff_ids_and_entrypoint() {
        let layer = build_layer(&[LayerFile {
            path: "app/app.jar".into(),
            data: b"jar-bytes".to_vec(),
            mode: 0o644,
        }])
        .unwrap();

        let base_layers = vec![LayerDescriptor {
            media_type: MEDIA_DOCKER_LAYER.into(),
            digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
            size: 12,
            diff_id: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
        }];

        let mut opts = ImageOptions::default();
        opts.entrypoint = vec!["java".into(), "-jar".into(), "app.jar".into()];
        opts.working_dir = Some("/app".into());
        opts.env.insert("FOO".into(), "bar".into());

        let assembled =
            assemble_image(&minimal_base_config(), &base_layers, &[layer], &opts, false).unwrap();

        assert_eq!(assembled.manifest_media_type, MEDIA_DOCKER_MANIFEST);
        assert_eq!(assembled.layers.len(), 2);

        let config: Value = serde_json::from_slice(&assembled.config_bytes).unwrap();
        let diffs = config["rootfs"]["diff_ids"].as_array().unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(
            config["config"]["Entrypoint"],
            json!(["java", "-jar", "app.jar"])
        );
        assert_eq!(config["config"]["WorkingDir"], json!("/app"));
        let env = config["config"]["Env"].as_array().unwrap();
        assert!(env.iter().any(|e| e.as_str() == Some("FOO=bar")));
        assert!(env.iter().any(|e| e.as_str() == Some("PATH=/usr/bin")));
    }

    #[test]
    fn select_platform_picks_matching_entry() {
        let index = json!({
            "manifests": [
                {
                    "digest": "sha256:arm",
                    "mediaType": MEDIA_OCI_MANIFEST,
                    "platform": {"os": "linux", "architecture": "arm64"}
                },
                {
                    "digest": "sha256:amd",
                    "mediaType": MEDIA_OCI_MANIFEST,
                    "platform": {"os": "linux", "architecture": "amd64"}
                }
            ]
        });
        let (digest, _) = select_platform_manifest(&index, "linux/amd64").unwrap();
        assert_eq!(digest, "sha256:amd");
    }

    #[test]
    fn select_platform_errors_with_available_list() {
        let index = json!({
            "manifests": [{
                "digest": "sha256:arm",
                "mediaType": MEDIA_OCI_MANIFEST,
                "platform": {"os": "linux", "architecture": "arm64"}
            }]
        });
        let err = select_platform_manifest(&index, "linux/amd64")
            .unwrap_err()
            .to_string();
        assert!(err.contains("linux/arm64"), "got: {err}");
    }

    #[test]
    fn assemble_is_deterministic() {
        let layer = build_layer(&[LayerFile {
            path: "app/app.jar".into(),
            data: b"jar".to_vec(),
            mode: 0o644,
        }])
        .unwrap();
        let base_layers = vec![LayerDescriptor {
            media_type: MEDIA_DOCKER_LAYER.into(),
            digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
            size: 1,
            diff_id: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
        }];
        let opts = ImageOptions {
            entrypoint: vec!["java".into(), "-jar".into(), "app.jar".into()],
            working_dir: Some("/app".into()),
            ..Default::default()
        };
        let a = assemble_image(
            &minimal_base_config(),
            &base_layers,
            &[layer.clone()],
            &opts,
            true,
        )
        .unwrap();
        let b =
            assemble_image(&minimal_base_config(), &base_layers, &[layer], &opts, true).unwrap();
        assert_eq!(a.manifest_digest, b.manifest_digest);
        assert_eq!(a.config_digest, b.config_digest);
        assert_eq!(a.config_bytes, b.config_bytes);
    }
}
