use crate::config::OpenApiConfig;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run openapi-generator-cli to produce Java sources from the spec.
/// `artifacts` maps artifact id → local path (provided by curie-build).
pub fn run(
    project_root: &Path,
    cfg: &OpenApiConfig,
    artifacts: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let cli_jar = artifacts
        .get("openapi-generator-cli")
        .context("curie-build did not provide 'openapi-generator-cli' artifact path")?;

    let spec_path = project_root.join(&cfg.spec_file);
    if !spec_path.exists() {
        bail!("OpenAPI spec file does not exist: {}", spec_path.display());
    }

    let out_dir = project_root.join(&cfg.output_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;

    let mut cmd = Command::new("java");
    cmd.arg("-jar").arg(cli_jar);
    cmd.arg("generate");
    cmd.arg("-g").arg(&cfg.generator_name);
    cmd.arg("-i").arg(&spec_path);
    cmd.arg("-o").arg(&out_dir);

    if let Some(pkg) = &cfg.api_package {
        cmd.arg("--api-package").arg(pkg);
    }
    if let Some(pkg) = &cfg.model_package {
        cmd.arg("--model-package").arg(pkg);
    }
    if let Some(pkg) = &cfg.invoker_package {
        cmd.arg("--invoker-package").arg(pkg);
    }

    for (key, val) in &cfg.global_properties {
        cmd.arg("--global-property").arg(format!("{key}={val}"));
    }

    if !cfg.additional_properties.is_empty() {
        cmd.arg("--additional-properties")
            .arg(join_properties(&cfg.additional_properties));
    }

    let status = cmd.status().context("failed to run java -jar openapi-generator-cli")?;
    if !status.success() {
        bail!("openapi-generator-cli exited with status {:?}", status.code());
    }

    Ok(())
}

/// Join a BTreeMap as `key=val,key=val,...` for `--additional-properties`.
/// BTreeMap iteration is sorted, so the result is deterministic.
pub fn join_properties(props: &BTreeMap<String, String>) -> String {
    props
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additional_properties_joined_with_comma() {
        let mut props = BTreeMap::new();
        props.insert("hideGenerationTimestamp".to_string(), "true".to_string());
        props.insert("serializationLibrary".to_string(), "jackson".to_string());
        // BTreeMap is sorted alphabetically
        assert_eq!(
            join_properties(&props),
            "hideGenerationTimestamp=true,serializationLibrary=jackson"
        );
    }

    #[test]
    fn additional_properties_single_entry() {
        let mut props = BTreeMap::new();
        props.insert("key".to_string(), "val".to_string());
        assert_eq!(join_properties(&props), "key=val");
    }

    #[test]
    fn additional_properties_empty() {
        assert_eq!(join_properties(&BTreeMap::new()), "");
    }
}
