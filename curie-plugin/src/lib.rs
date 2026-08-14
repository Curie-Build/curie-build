//! Shared manifest types and helpers for Curie plugins.
//!
//! Both sides of the plugin protocol use these types:
//! - **Plugin binary**: constructs and serializes a [`Manifest`] on stdout.
//! - **curie-build**: deserializes the [`Manifest`] from the plugin's stdout.
//!
//! Plugins also receive a typed [`Envelope`] on stdin; use [`read_envelope`]
//! to parse it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// ── Manifest ─────────────────────────────────────────────────────────────────

/// Top-level manifest returned by `curie-<name> manifest` on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub inputs: Inputs,
    #[serde(default)]
    pub outputs: Outputs,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// Source files that trigger re-generation when they change.
///
/// At least one of `dirs` or `files` should be non-empty.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Inputs {
    /// Directories to watch for added/removed files.
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    /// Optional regex filter applied to filenames inside `dirs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_regex: Option<String>,
    /// Individual files to watch (used when the input is a single spec file).
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

/// Directories written by the plugin that curie-build adds to the source path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Outputs {
    #[serde(default)]
    pub source_dirs: Vec<PathBuf>,
}

/// A Maven artifact the plugin needs curie-build to download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Logical name used as the key in the `generate-sources` envelope.
    pub id: String,
    pub group: String,
    pub artifact: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    pub extension: String,
    #[serde(default)]
    pub executable: bool,
}

// ── Envelope ─────────────────────────────────────────────────────────────────

/// Envelope sent by curie-build to the plugin on stdin.
///
/// `C` is the plugin-specific config type (e.g. `ProtobufConfig`).
/// The `artifacts` map is only populated for `generate-sources` calls.
#[derive(Debug, Deserialize)]
pub struct Envelope<C> {
    pub curie_version: String,
    pub config: C,
    #[serde(default)]
    pub artifacts: BTreeMap<String, PathBuf>,
}

/// Read and parse the [`Envelope`] from stdin.
pub fn read_envelope<C: serde::de::DeserializeOwned>() -> Result<Envelope<C>> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(serde_json::from_str(&s)?)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_manifest() -> Manifest {
        Manifest {
            name: "test".to_string(),
            description: "A test plugin".to_string(),
            version: "0.1.0".to_string(),
            types: vec!["source-generator".to_string()],
            inputs: Inputs {
                dirs: vec![PathBuf::from("proto")],
                file_regex: Some(r"\.proto$".to_string()),
                files: vec![],
            },
            outputs: Outputs {
                source_dirs: vec![PathBuf::from("target/generated-sources/test")],
            },
            artifacts: vec![Artifact {
                id: "tool".to_string(),
                group: "com.example".to_string(),
                artifact: "tool".to_string(),
                version: "1.0.0".to_string(),
                classifier: Some("linux-x86_64".to_string()),
                extension: "exe".to_string(),
                executable: true,
            }],
        }
    }

    #[test]
    fn manifest_roundtrips_through_json() {
        let m = minimal_manifest();
        let json = serde_json::to_string(&m).unwrap();
        let m2: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m2.name, "test");
        assert_eq!(m2.inputs.dirs, vec![PathBuf::from("proto")]);
        assert_eq!(m2.inputs.file_regex.as_deref(), Some(r"\.proto$"));
        assert_eq!(
            m2.outputs.source_dirs,
            vec![PathBuf::from("target/generated-sources/test")]
        );
        assert_eq!(m2.artifacts[0].classifier, Some("linux-x86_64".to_string()));
    }

    #[test]
    fn missing_optional_fields_deserialize_as_defaults() {
        let json = r#"{
            "name": "mini",
            "description": "desc",
            "version": "0.1.0"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.types.is_empty());
        assert!(m.inputs.dirs.is_empty());
        assert!(m.inputs.file_regex.is_none());
        assert!(m.inputs.files.is_empty());
        assert!(m.outputs.source_dirs.is_empty());
        assert!(m.artifacts.is_empty());
    }

    #[test]
    fn artifact_without_classifier_omits_field_in_json() {
        let art = Artifact {
            id: "cli".to_string(),
            group: "org.example".to_string(),
            artifact: "cli".to_string(),
            version: "1.0".to_string(),
            classifier: None,
            extension: "jar".to_string(),
            executable: false,
        };
        let json = serde_json::to_string(&art).unwrap();
        assert!(
            !json.contains("classifier"),
            "classifier key must be absent: {json}"
        );
    }

    #[test]
    fn file_regex_none_omits_field_in_json() {
        let inputs = Inputs {
            dirs: vec![],
            file_regex: None,
            files: vec![PathBuf::from("spec.yaml")],
        };
        let json = serde_json::to_string(&inputs).unwrap();
        assert!(
            !json.contains("file_regex"),
            "file_regex key must be absent: {json}"
        );
    }

    #[test]
    fn envelope_deserializes_config_and_artifacts() {
        #[derive(Deserialize)]
        struct MyConfig {
            value: String,
        }

        let json = r#"{
            "curie_version": "0.6.0",
            "config": {"value": "hello"},
            "artifacts": {"tool": "/path/to/tool"}
        }"#;
        let env: Envelope<MyConfig> = serde_json::from_str(json).unwrap();
        assert_eq!(env.curie_version, "0.6.0");
        assert_eq!(env.config.value, "hello");
        assert_eq!(env.artifacts["tool"], PathBuf::from("/path/to/tool"));
    }

    #[test]
    fn envelope_artifacts_defaults_to_empty() {
        #[derive(Deserialize)]
        struct MyConfig {
            #[allow(dead_code)]
            v: u32,
        }

        let json = r#"{"curie_version": "0.6.0", "config": {"v": 1}}"#;
        let env: Envelope<MyConfig> = serde_json::from_str(json).unwrap();
        assert!(env.artifacts.is_empty());
    }
}
