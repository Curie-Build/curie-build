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

/// Lifecycle phases a plugin may bind to.
///
/// `generate-sources` is the original pre-compile source-generator phase.
/// The rest are invoked by `curie build` / `curie publish` at the matching
/// cut-point. Plugins declare the phases they implement in [`Manifest::phases`].
pub const PHASE_GENERATE_SOURCES: &str = "generate-sources";
pub const PHASE_POST_COMPILE: &str = "post-compile";
pub const PHASE_PRE_TEST: &str = "pre-test";
pub const PHASE_POST_TEST: &str = "post-test";
pub const PHASE_PRE_PACKAGE: &str = "pre-package";
pub const PHASE_POST_PACKAGE: &str = "post-package";
pub const PHASE_PRE_PUBLISH: &str = "pre-publish";
pub const PHASE_PUBLISH: &str = "publish";
pub const PHASE_POST_PUBLISH: &str = "post-publish";

/// Every phase Curie currently dispatches.
pub const ALL_PHASES: &[&str] = &[
    PHASE_GENERATE_SOURCES,
    PHASE_POST_COMPILE,
    PHASE_PRE_TEST,
    PHASE_POST_TEST,
    PHASE_PRE_PACKAGE,
    PHASE_POST_PACKAGE,
    PHASE_PRE_PUBLISH,
    PHASE_PUBLISH,
    PHASE_POST_PUBLISH,
];

/// Top-level manifest returned by `curie-<name> manifest` on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub types: Vec<String>,
    /// Lifecycle phases this plugin implements (e.g. `"post-package"`, `"publish"`).
    /// Source-generator plugins may omit this and set `types` to `["source-generator"]`.
    #[serde(default)]
    pub phases: Vec<String>,
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

/// Directories and files written by the plugin.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Outputs {
    /// Directories added to the compiler source path after `generate-sources`.
    #[serde(default)]
    pub source_dirs: Vec<PathBuf>,
    /// Extra files the plugin produces (relative to the project root). Used
    /// for incremental stamps and orphan tracking on non-generator phases.
    #[serde(default)]
    pub files: Vec<PathBuf>,
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
/// The `artifacts` map is populated for `generate-sources` and `run` calls.
/// `phase` and `context` are set for `run` and (when known) `manifest`.
#[derive(Debug, Deserialize)]
pub struct Envelope<C> {
    pub curie_version: String,
    pub config: C,
    #[serde(default)]
    pub artifacts: BTreeMap<String, PathBuf>,
    /// Lifecycle phase for `run` invocations. Absent on `manifest`.
    #[serde(default)]
    pub phase: Option<String>,
    /// Project/build context provided by Curie. Absent on older hosts.
    #[serde(default)]
    pub context: Option<PluginContext>,
}

/// Build/publish context Curie attaches to plugin envelopes.
///
/// All fields are optional or defaulted so older plugins and partial
/// envelopes (e.g. `manifest` before the JAR exists) deserialize cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default)]
    pub artifact_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jar: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classes_dir: Option<PathBuf>,
    #[serde(default)]
    pub target_dir: PathBuf,
    #[serde(default)]
    pub project_root: PathBuf,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Target URL Curie itself is publishing to (Maven repository).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
    /// Repositories declared by the project, with credentials when available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<PluginRepository>,
}

/// A project `[[repositories]]` entry, optionally carrying resolved credentials.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginRepository {
    pub id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Structured stdout of `curie-<name> run --phase <phase>`.
///
/// Empty / omitted stdout is treated as a default (empty) result. Extra
/// artifacts listed here are picked up by `curie publish` and uploaded
/// alongside the project's Maven artifacts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ProducedArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PathBuf>,
}

/// An extra file a plugin asks Curie to treat as a publishable Maven artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProducedArtifact {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

/// Read and parse the [`Envelope`] from stdin.
pub fn read_envelope<C: serde::de::DeserializeOwned>() -> Result<Envelope<C>> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(serde_json::from_str(&s)?)
}

/// True when `manifest` should run at `phase`.
///
/// A plugin with empty `types` and empty `phases` is treated as a
/// source-generator for backward compatibility with the original protocol.
pub fn participates_in(manifest: &Manifest, phase: &str) -> bool {
    if phase == PHASE_GENERATE_SOURCES {
        return is_source_generator(manifest);
    }
    manifest.phases.iter().any(|p| p == phase)
}

/// True when the plugin is a pre-compile source generator.
pub fn is_source_generator(manifest: &Manifest) -> bool {
    if manifest.types.iter().any(|t| t == "source-generator") {
        return true;
    }
    if manifest.phases.iter().any(|p| p == PHASE_GENERATE_SOURCES) {
        return true;
    }
    // Original protocol: no types, no phases → source generator.
    manifest.types.is_empty() && manifest.phases.is_empty()
}

/// True when the plugin binds to at least one phase Curie knows about.
pub fn participates_in_any_known_phase(manifest: &Manifest) -> bool {
    ALL_PHASES.iter().any(|p| participates_in(manifest, p))
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
            phases: vec![],
            inputs: Inputs {
                dirs: vec![PathBuf::from("proto")],
                file_regex: Some(r"\.proto$".to_string()),
                files: vec![],
            },
            outputs: Outputs {
                source_dirs: vec![PathBuf::from("target/generated-sources/test")],
                files: vec![],
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
        assert!(m.outputs.files.is_empty());
        assert!(m.phases.is_empty());
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
        assert!(env.phase.is_none());
        assert!(env.context.is_none());
    }

    #[test]
    fn envelope_deserializes_phase_and_context() {
        #[derive(Deserialize)]
        struct MyConfig {
            #[allow(dead_code)]
            v: u32,
        }

        let json = r#"{
            "curie_version": "0.7.0",
            "config": {"v": 1},
            "phase": "post-package",
            "context": {
                "artifact_id": "greeter",
                "version": "1.0.0",
                "jar": "/tmp/greeter-1.0.0.jar",
                "dry_run": true
            }
        }"#;
        let env: Envelope<MyConfig> = serde_json::from_str(json).unwrap();
        assert_eq!(env.phase.as_deref(), Some("post-package"));
        let ctx = env.context.expect("context present");
        assert_eq!(ctx.artifact_id, "greeter");
        assert_eq!(ctx.version, "1.0.0");
        assert_eq!(ctx.jar, Some(PathBuf::from("/tmp/greeter-1.0.0.jar")));
        assert!(ctx.dry_run);
    }

    #[test]
    fn empty_manifest_is_source_generator_for_compat() {
        let m: Manifest = serde_json::from_str(
            r#"{
            "name": "legacy", "description": "d", "version": "0.1.0"
        }"#,
        )
        .unwrap();
        assert!(is_source_generator(&m));
        assert!(participates_in(&m, PHASE_GENERATE_SOURCES));
        assert!(!participates_in(&m, PHASE_POST_PACKAGE));
    }

    #[test]
    fn lifecycle_plugin_matches_declared_phases_only() {
        let m: Manifest = serde_json::from_str(
            r#"{
            "name": "pack",
            "description": "d",
            "version": "0.1.0",
            "types": ["lifecycle"],
            "phases": ["post-package", "publish"]
        }"#,
        )
        .unwrap();
        assert!(!is_source_generator(&m));
        assert!(!participates_in(&m, PHASE_GENERATE_SOURCES));
        assert!(participates_in(&m, PHASE_POST_PACKAGE));
        assert!(participates_in(&m, PHASE_PUBLISH));
        assert!(!participates_in(&m, PHASE_PRE_TEST));
        assert!(participates_in_any_known_phase(&m));
    }

    #[test]
    fn phase_result_empty_stdout_defaults() {
        let r: PhaseResult = serde_json::from_str("{}").unwrap();
        assert!(r.artifacts.is_empty());
        assert!(r.files.is_empty());
    }

    #[test]
    fn phase_result_roundtrips_produced_artifact() {
        let r = PhaseResult {
            artifacts: vec![ProducedArtifact {
                path: PathBuf::from("target/extra.jar"),
                classifier: Some("extra".into()),
                extension: Some("jar".into()),
            }],
            files: vec![],
        };
        let json = serde_json::to_string(&r).unwrap();
        let r2: PhaseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r2.artifacts[0].path, PathBuf::from("target/extra.jar"));
        assert_eq!(r2.artifacts[0].classifier.as_deref(), Some("extra"));
    }
}
