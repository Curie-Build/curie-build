use crate::config::ProtobufConfig;
use crate::platform;
use anyhow::Result;
use std::path::PathBuf;

pub fn build(cfg: &ProtobufConfig) -> Result<curie_plugin::Manifest> {
    let classifier = platform::maven_classifier()?.to_string();

    let mut artifacts = vec![curie_plugin::Artifact {
        id: "protoc".to_string(),
        group: "com.google.protobuf".to_string(),
        artifact: "protoc".to_string(),
        version: cfg.version.clone(),
        classifier: Some(classifier.clone()),
        extension: "exe".to_string(),
        executable: true,
    }];

    if cfg.grpc {
        let grpc_version = cfg.grpc_version.as_deref().unwrap_or("1.60.0").to_string();
        artifacts.push(curie_plugin::Artifact {
            id: "grpc-plugin".to_string(),
            group: "io.grpc".to_string(),
            artifact: "protoc-gen-grpc-java".to_string(),
            version: grpc_version,
            classifier: Some(classifier),
            extension: "exe".to_string(),
            executable: true,
        });
    }

    Ok(curie_plugin::Manifest {
        name: "protobuf".to_string(),
        description: "Generate Java + gRPC stubs from .proto files".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        types: vec!["source-generator".to_string()],
        phases: vec![],
        inputs: curie_plugin::Inputs {
            dirs: vec![PathBuf::from(&cfg.source_dir)],
            file_regex: Some(r"\.proto$".to_string()),
            files: vec![],
        },
        outputs: curie_plugin::Outputs {
            source_dirs: vec![PathBuf::from("target/generated-sources/protobuf")],
            files: vec![],
        },
        artifacts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config_no_grpc() -> ProtobufConfig {
        serde_json::from_str(r#"{"version":"3.25.0"}"#).unwrap()
    }

    fn config_with_grpc() -> ProtobufConfig {
        serde_json::from_str(r#"{"version":"3.25.0","grpc":true,"grpcVersion":"1.60.0"}"#).unwrap()
    }

    #[test]
    fn manifest_has_one_artifact_without_grpc() {
        let m = build(&config_no_grpc()).unwrap();
        assert_eq!(m.artifacts.len(), 1);
        assert_eq!(m.artifacts[0].id, "protoc");
    }

    #[test]
    fn manifest_has_two_artifacts_with_grpc() {
        let m = build(&config_with_grpc()).unwrap();
        assert_eq!(m.artifacts.len(), 2);
        assert_eq!(m.artifacts[1].id, "grpc-plugin");
    }

    #[test]
    fn manifest_types_includes_source_generator() {
        let m = build(&config_no_grpc()).unwrap();
        assert!(m.types.iter().any(|t| t == "source-generator"));
    }

    #[test]
    fn manifest_input_dir_matches_config_source_dir() {
        let cfg: ProtobufConfig =
            serde_json::from_str(r#"{"version":"3.25.0","sourceDir":"src/main/proto"}"#).unwrap();
        let m = build(&cfg).unwrap();
        assert_eq!(m.inputs.dirs, vec![PathBuf::from("src/main/proto")]);
    }

    #[test]
    fn manifest_output_dir_is_fixed() {
        let m = build(&config_no_grpc()).unwrap();
        assert_eq!(
            m.outputs.source_dirs,
            vec![PathBuf::from("target/generated-sources/protobuf")]
        );
    }
}
