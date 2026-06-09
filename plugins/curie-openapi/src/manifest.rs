use crate::config::OpenApiConfig;
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
    pub files: Vec<String>,
}

#[derive(Serialize)]
pub struct Outputs {
    pub source_dirs: Vec<String>,
}

#[derive(Serialize)]
pub struct Artifact {
    pub id: &'static str,
    pub group: &'static str,
    pub artifact: &'static str,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    pub extension: &'static str,
    pub executable: bool,
}

pub fn build(cfg: &OpenApiConfig) -> Manifest {
    let source_dir = format!("{}/{}", cfg.output_dir, cfg.source_folder);

    Manifest {
        name: "openapi",
        description: "Generate Java sources from an OpenAPI spec",
        version: env!("CARGO_PKG_VERSION"),
        types: vec!["source-generator"],
        inputs: Inputs {
            files: vec![cfg.spec_file.clone()],
        },
        outputs: Outputs {
            source_dirs: vec![source_dir],
        },
        artifacts: vec![Artifact {
            id: "openapi-generator-cli",
            group: "org.openapitools",
            artifact: "openapi-generator-cli",
            version: cfg.version.clone(),
            classifier: None,
            extension: "jar",
            executable: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!json.contains("classifier"), "classifier should be absent from JSON");
    }

    #[test]
    fn manifest_source_dir_combines_output_and_source_folder() {
        let m = build(&minimal_config());
        assert_eq!(
            m.outputs.source_dirs[0],
            "target/generated-sources/openapi/src/main/java"
        );
    }

    #[test]
    fn manifest_input_is_spec_file() {
        let m = build(&minimal_config());
        assert_eq!(m.inputs.files, vec!["api/greeter.yaml"]);
    }

    #[test]
    fn manifest_types_includes_source_generator() {
        let m = build(&minimal_config());
        assert!(m.types.contains(&"source-generator"));
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
        assert_eq!(m.outputs.source_dirs[0], "target/gen/src/main/java");
    }
}
