mod config;
mod generate;
mod manifest;
mod platform;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("manifest") => cmd_manifest(&args[2..]),
        Some("generate-sources") => cmd_generate_sources(&args[2..]),
        Some(other) => bail!("unknown subcommand: {other}"),
        None => {
            bail!("usage: curie-protobuf <manifest|generate-sources> [--project <dir>] [--offline]")
        }
    }
}

fn cmd_manifest(args: &[String]) -> Result<()> {
    let _project_root = parse_project(args)?;
    let envelope = config::read_envelope()?;
    let m = manifest::build(&envelope.config)?;
    println!("{}", serde_json::to_string(&m)?);
    Ok(())
}

fn cmd_generate_sources(args: &[String]) -> Result<()> {
    let project_root = parse_project(args)?;
    let envelope = config::read_envelope()?;
    generate::run(&project_root, &envelope.config, &envelope.artifacts)?;
    Ok(())
}

fn parse_project(args: &[String]) -> Result<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--project" {
            return args
                .get(i + 1)
                .map(PathBuf::from)
                .context("--project requires an argument");
        }
        i += 1;
    }
    std::env::current_dir().context("failed to get current directory")
}
