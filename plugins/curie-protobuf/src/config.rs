use anyhow::Result;
use serde::Deserialize;

pub type Envelope = curie_plugin::Envelope<ProtobufConfig>;

pub fn read_envelope() -> Result<Envelope> {
    curie_plugin::read_envelope()
}

#[derive(Deserialize)]
pub struct ProtobufConfig {
    pub version: String,
    #[serde(default)]
    pub grpc: bool,
    #[serde(rename = "grpcVersion")]
    pub grpc_version: Option<String>,
    #[serde(rename = "sourceDir", default = "default_source_dir")]
    pub source_dir: String,
}

fn default_source_dir() -> String {
    "proto".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {"version": "3.25.0"}
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.curie_version, "0.6.0");
        assert_eq!(env.config.version, "3.25.0");
        assert!(!env.config.grpc);
        assert_eq!(env.config.source_dir, "proto");
        assert!(env.artifacts.is_empty());
    }

    #[test]
    fn config_deserializes_grpc() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {
                "version": "3.25.0",
                "grpc": true,
                "grpcVersion": "1.60.0",
                "sourceDir": "src/main/proto"
            }
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert!(env.config.grpc);
        assert_eq!(env.config.grpc_version.as_deref(), Some("1.60.0"));
        assert_eq!(env.config.source_dir, "src/main/proto");
    }

    #[test]
    fn config_accepts_unknown_top_level_fields() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {"version": "3.25.0"},
            "future_field": "ignored"
        }"#;
        let result: Result<Envelope, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "unknown fields should be ignored");
    }

    #[test]
    fn generate_sources_envelope_includes_artifacts() {
        let json = r#"{
            "curie_version": "0.6.0",
            "config": {"version": "3.25.0"},
            "artifacts": {
                "protoc": "/home/user/.m2/repository/protoc-3.25.0-linux-x86_64.exe"
            }
        }"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert!(env.artifacts.contains_key("protoc"));
    }
}
