mod bundle;
mod classfile;
mod config;
mod dummy_repo;
mod headers;
mod manifest;
mod repository;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("manifest") => cmd_manifest(&args[2..]),
        Some("run") => cmd_run(&args[2..]),
        Some("dummy-repo") => cmd_dummy_repo(&args[2..]),
        Some(other) => bail!("unknown subcommand: {other}"),
        None => bail!(
            "usage: curie-osgi <manifest|run|dummy-repo> [--project <dir>] [--phase <phase>] [--dir <path>] [--port <n>]"
        ),
    }
}

fn cmd_manifest(args: &[String]) -> Result<()> {
    let _project_root = parse_project(args)?;
    let envelope = config::read_envelope()?;
    let m = manifest::build(&envelope.config, envelope.context.as_ref());
    println!("{}", serde_json::to_string(&m)?);
    Ok(())
}

fn cmd_run(args: &[String]) -> Result<()> {
    let project_root = parse_project(args)?;
    let phase = parse_opt(args, "--phase").context("run requires --phase")?;
    let envelope = config::read_envelope()?;
    match phase.as_str() {
        p if p == curie_plugin::PHASE_POST_PACKAGE => bundle::run(&project_root, &envelope),
        p if p == curie_plugin::PHASE_PUBLISH => repository::run(&project_root, &envelope),
        other => bail!("curie-osgi does not implement phase '{other}'"),
    }
}

fn cmd_dummy_repo(args: &[String]) -> Result<()> {
    let dir = parse_opt(args, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/osgi-repo"));
    let port = parse_opt(args, "--port")
        .map(|s| s.parse::<u16>())
        .transpose()
        .context("--port must be a number")?
        .unwrap_or(0);
    let listener = dummy_repo::bind(port)?;
    dummy_repo::serve_forever(listener, dir)
}

fn parse_project(args: &[String]) -> Result<PathBuf> {
    Ok(parse_opt(args, "--project")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("cwd")))
}

fn parse_opt(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_opt_reads_flag() {
        let args = vec![
            "--phase".into(),
            "publish".into(),
            "--project".into(),
            "/tmp".into(),
        ];
        assert_eq!(parse_opt(&args, "--phase").as_deref(), Some("publish"));
        assert_eq!(parse_opt(&args, "--missing"), None);
    }
}
