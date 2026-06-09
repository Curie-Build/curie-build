use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Top-level envelope sent by curie-build on stdin.
/// Unknown fields are ignored for forward-compatibility.
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct Envelope {
    pub curie_version: String,
    pub config: OpenApiConfig,
    /// Only present in `generate-sources` calls.
    #[serde(default)]
    pub artifacts: BTreeMap<String, PathBuf>,
}

#[derive(Deserialize)]
pub struct OpenApiConfig {
    pub version: String,
    #[serde(rename = "specFile")]
    pub spec_file: String,
    #[serde(rename = "generatorName")]
    pub generator_name: String,
    #[serde(rename = "apiPackage")]
    pub api_package: Option<String>,
    #[serde(rename = "modelPackage")]
    pub model_package: Option<String>,
    #[serde(rename = "invokerPackage")]
    pub invoker_package: Option<String>,
    #[serde(rename = "outputDir", default = "default_output_dir")]
    pub output_dir: String,
    #[serde(rename = "sourceFolder", default = "default_source_folder")]
    pub source_folder: String,
    #[serde(rename = "globalProperties", default)]
    pub global_properties: BTreeMap<String, String>,
    #[serde(rename = "additionalProperties", default)]
    pub additional_properties: BTreeMap<String, String>,
}

fn default_output_dir() -> String {
    "target/generated-sources/openapi".to_string()
}

fn default_source_folder() -> String {
    "src/main/java".to_string()
}

pub fn read_envelope() -> Result<Envelope> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(serde_json::from_str(&s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {
                "version": "7.2.0",
                "specFile": "api/greeter.yaml",
                "generatorName": "java"
            }
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.config.version, "7.2.0");
        assert_eq!(env.config.spec_file, "api/greeter.yaml");
        assert_eq!(env.config.generator_name, "java");
        assert_eq!(env.config.output_dir, "target/generated-sources/openapi");
        assert_eq!(env.config.source_folder, "src/main/java");
        assert!(env.config.api_package.is_none());
        assert!(env.config.model_package.is_none());
        assert!(env.config.global_properties.is_empty());
        assert!(env.config.additional_properties.is_empty());
        assert!(env.artifacts.is_empty());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {
                "version": "7.2.0",
                "specFile": "api/greeter.yaml",
                "generatorName": "java",
                "apiPackage": "com.example.api",
                "modelPackage": "com.example.model",
                "invokerPackage": "com.example",
                "outputDir": "target/generated-sources/openapi",
                "sourceFolder": "src/main/java",
                "globalProperties": {
                    "models": "",
                    "supportingFiles": ""
                },
                "additionalProperties": {
                    "serializationLibrary": "jackson",
                    "hideGenerationTimestamp": "true"
                }
            }
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        let cfg = &env.config;
        assert_eq!(cfg.api_package.as_deref(), Some("com.example.api"));
        assert_eq!(cfg.model_package.as_deref(), Some("com.example.model"));
        assert_eq!(cfg.global_properties["models"], "");
        assert_eq!(cfg.additional_properties["serializationLibrary"], "jackson");
    }

    #[test]
    fn generate_sources_envelope_includes_artifacts() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {
                "version": "7.2.0",
                "specFile": "api/greeter.yaml",
                "generatorName": "java"
            },
            "artifacts": {
                "openapi-generator-cli": "/home/user/.m2/repository/org/openapitools/openapi-generator-cli/7.2.0/openapi-generator-cli-7.2.0.jar"
            }
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert!(env.artifacts.contains_key("openapi-generator-cli"));
    }
}
