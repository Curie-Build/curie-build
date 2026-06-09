use crate::config::ProtobufConfig;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run protoc to generate Java (and optionally gRPC) stubs.
/// `artifacts` maps artifact id → local path (provided by curie-build).
pub fn run(
    project_root: &Path,
    cfg: &ProtobufConfig,
    artifacts: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let protoc = artifacts
        .get("protoc")
        .with_context(|| "curie-build did not provide 'protoc' artifact path")?;

    let source_dir = project_root.join(&cfg.source_dir);
    if !source_dir.exists() {
        bail!(
            "proto source directory does not exist: {}",
            source_dir.display()
        );
    }

    let proto_files = collect_proto_files(&source_dir)?;
    if proto_files.is_empty() {
        bail!(
            "no .proto files found in {}",
            source_dir.display()
        );
    }

    let out_dir = project_root.join("target").join("generated-sources").join("protobuf");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;

    let mut cmd = Command::new(protoc);
    cmd.arg(format!("--proto_path={}", source_dir.display()));
    cmd.arg(format!("--java_out={}", out_dir.display()));

    if cfg.grpc {
        let grpc_plugin = artifacts
            .get("grpc-plugin")
            .with_context(|| "curie-build did not provide 'grpc-plugin' artifact path")?;
        cmd.arg(format!("--plugin=protoc-gen-grpc-java={}", grpc_plugin.display()));
        cmd.arg(format!("--grpc-java_out={}", out_dir.display()));
    }

    for proto in &proto_files {
        cmd.arg(proto);
    }

    let status = cmd
        .status()
        .context("failed to run protoc")?;

    if !status.success() {
        bail!("protoc exited with status {:?}", status.code());
    }

    Ok(())
}

fn collect_proto_files(source_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(source_dir).min_depth(1) {
        let entry = entry.with_context(|| format!("error walking {}", source_dir.display()))?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("proto")
        {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}
