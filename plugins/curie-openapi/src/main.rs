mod config;
mod generate;
mod manifest;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let project_root = parse_project(&args);

    match subcommand {
        "manifest" => cmd_manifest(&project_root),
        "generate-sources" => cmd_generate_sources(&project_root),
        other => bail!("unknown subcommand '{other}'; expected 'manifest' or 'generate-sources'"),
    }
}

fn cmd_manifest(project_root: &Path) -> Result<()> {
    let env = config::read_envelope()?;
    let manifest = manifest::build(&env.config);
    let _ = project_root; // received but not needed for manifest
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn cmd_generate_sources(project_root: &Path) -> Result<()> {
    let env = config::read_envelope()?;
    generate::run(project_root, &env.config, &env.artifacts)
}

/// Extract `--project <dir>` from argv; default to current directory.
fn parse_project(args: &[String]) -> PathBuf {
    args.windows(2)
        .find(|w| w[0] == "--project")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
