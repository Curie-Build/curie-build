use crate::config::ProtobufConfig;
use crate::platform;
use anyhow::Result;
use serde::Serialize;

#[derive(Serialize)]
pub struct Manifest {
    pub name: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub types: Vec<&'static str>,
    pub inputs: Inputs,
    pub outputs: Outputs,
    pub artifacts: Vec<Artifact>,
}

#[derive(Serialize)]
pub struct Inputs {
    pub dirs: Vec<String>,
    pub file_regex: &'static str,
}

#[derive(Serialize)]
pub struct Outputs {
    pub source_dirs: Vec<&'static str>,
}

#[derive(Serialize)]
pub struct Artifact {
    pub id: &'static str,
    pub group: &'static str,
    pub artifact: &'static str,
    pub version: String,
    pub classifier: String,
    pub extension: &'static str,
    pub executable: bool,
}

pub fn build(cfg: &ProtobufConfig) -> Result<Manifest> {
    let classifier = platform::maven_classifier()?.to_string();

    let mut artifacts = vec![Artifact {
        id: "protoc",
        group: "com.google.protobuf",
        artifact: "protoc",
        version: cfg.version.clone(),
        classifier: classifier.clone(),
        extension: "exe",
        executable: true,
    }];

    if cfg.grpc {
        let grpc_version = cfg
            .grpc_version
            .as_deref()
            .unwrap_or("1.60.0")
            .to_string();
        artifacts.push(Artifact {
            id: "grpc-plugin",
            group: "io.grpc",
            artifact: "protoc-gen-grpc-java",
            version: grpc_version,
            classifier,
            extension: "exe",
            executable: true,
        });
    }

    Ok(Manifest {
        name: "protobuf",
        description: "Generate Java + gRPC stubs from .proto files",
        version: env!("CARGO_PKG_VERSION"),
        types: vec!["source-generator"],
        inputs: Inputs {
            dirs: vec![cfg.source_dir.clone()],
            file_regex: r"\.proto$",
        },
        outputs: Outputs {
            source_dirs: vec!["target/generated-sources/protobuf"],
        },
        artifacts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(m.types.contains(&"source-generator"));
    }

    #[test]
    fn manifest_input_dir_matches_config_source_dir() {
        let cfg: ProtobufConfig =
            serde_json::from_str(r#"{"version":"3.25.0","sourceDir":"src/main/proto"}"#).unwrap();
        let m = build(&cfg).unwrap();
        assert_eq!(m.inputs.dirs, vec!["src/main/proto"]);
    }

    #[test]
    fn manifest_output_dir_is_fixed() {
        let m = build(&config_no_grpc()).unwrap();
        assert_eq!(m.outputs.source_dirs, vec!["target/generated-sources/protobuf"]);
    }
}
