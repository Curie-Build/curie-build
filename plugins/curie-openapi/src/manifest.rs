use crate::config::OpenApiConfig;
use std::path::PathBuf;

pub fn build(cfg: &OpenApiConfig) -> curie_plugin::Manifest {
    let source_dir = format!("{}/{}", cfg.output_dir, cfg.source_folder);

    curie_plugin::Manifest {
        name: "openapi".to_string(),
        description: "Generate Java sources from an OpenAPI spec".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        types: vec!["source-generator".to_string()],
        inputs: curie_plugin::Inputs {
            files: vec![PathBuf::from(&cfg.spec_file)],
            dirs: vec![],
            file_regex: None,
        },
        outputs: curie_plugin::Outputs {
            source_dirs: vec![PathBuf::from(source_dir)],
        },
        artifacts: vec![curie_plugin::Artifact {
            id: "openapi-generator-cli".to_string(),
            group: "org.openapitools".to_string(),
            artifact: "openapi-generator-cli".to_string(),
            version: cfg.version.clone(),
            classifier: None,
            extension: "jar".to_string(),
            executable: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn minimal_config() -> OpenApiConfig {
        serde_json::from_str(r#"{
            "version": "7.2.0",
            "specFile": "api/greeter.yaml",
            "generatorName": "java"
        }"#)
        .unwrap()
    }

    #[test]
    fn manifest_artifact_has_no_classifier() {
        let m = build(&minimal_config());
        let json = serde_json::to_string(&m.artifacts[0]).unwrap();
        assert!(!json.contains("classifier"), "classifier should be absent from JSON: {json}");
    }

    #[test]
    fn manifest_source_dir_combines_output_and_source_folder() {
        let m = build(&minimal_config());
        assert_eq!(
            m.outputs.source_dirs[0],
            PathBuf::from("target/generated-sources/openapi/src/main/java")
        );
    }

    #[test]
    fn manifest_input_is_spec_file() {
        let m = build(&minimal_config());
        assert_eq!(m.inputs.files, vec![PathBuf::from("api/greeter.yaml")]);
    }

    #[test]
    fn manifest_types_includes_source_generator() {
        let m = build(&minimal_config());
        assert!(m.types.iter().any(|t| t == "source-generator"));
    }

    #[test]
    fn manifest_artifact_group_and_version() {
        let m = build(&minimal_config());
        let art = &m.artifacts[0];
        assert_eq!(art.group, "org.openapitools");
        assert_eq!(art.artifact, "openapi-generator-cli");
        assert_eq!(art.version, "7.2.0");
        assert_eq!(art.extension, "jar");
        assert!(!art.executable);
    }

    #[test]
    fn manifest_custom_output_and_source_folder() {
        let cfg: OpenApiConfig = serde_json::from_str(r#"{
            "version": "7.2.0",
            "specFile": "spec/api.yaml",
            "generatorName": "spring",
            "outputDir": "target/gen",
            "sourceFolder": "src/main/java"
        }"#)
        .unwrap();
        let m = build(&cfg);
        assert_eq!(m.outputs.source_dirs[0], PathBuf::from("target/gen/src/main/java"));
    }
}
