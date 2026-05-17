mod build;
mod compile;
mod descriptor;
mod docker;
mod incremental;
mod jar;
mod main_class;
mod run;
mod test;
mod workspace;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "curie", about = "The Curie build tool", version)]
struct Cli {
    /// Path to the project root (defaults to current directory)
    #[arg(long, default_value = ".")]
    project: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compile the project, run tests, package a JAR, and (when applicable) build a Docker image
    Build {
        /// Skip Docker build even when Docker support is configured
        #[arg(long)]
        no_docker: bool,

        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,
    },
    /// Compile the project and run its tests (no JAR or Docker build)
    Test {
        /// Only run tests whose fully-qualified class name matches this pattern
        #[arg(long)]
        filter: Option<String>,

        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,
    },
    /// Build the project and run it (via Docker or java -jar)
    Run {
        /// Skip Docker; run directly with java -jar
        #[arg(long)]
        no_docker: bool,

        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,

        /// Arguments to pass to the application (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Remove the target/ build directory
    Clean {},
    /// List the members of a workspace (project must be a workspace root)
    List {},
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Cmd::Build { no_docker, offline } => {
            let opts = build::BuildOptions { no_docker, offline };
            if is_workspace(&cli.project) {
                workspace::build_all(&cli.project, opts)
            } else {
                build::build(&cli.project, opts)
            }
        }
        Cmd::Test { filter, offline } => {
            if is_workspace(&cli.project) {
                workspace::test_all(&cli.project, filter.as_deref(), offline)
            } else {
                test_single_module(&cli.project, filter.as_deref(), offline)
            }
        }
        Cmd::Run { no_docker, offline, args } => {
            if is_workspace(&cli.project) {
                Err(anyhow::anyhow!(
                    "`curie run` is ambiguous in a workspace.  Re-run with \
                     --project <member> to choose one, e.g.\n  curie --project examples/hello-world run"
                ))
            } else {
                run::run(&cli.project, run::RunOptions { no_docker, offline }, &args)
            }
        }
        Cmd::Clean {} => {
            if is_workspace(&cli.project) {
                workspace::clean_all(&cli.project)
            } else {
                build::clean(&cli.project)
            }
        }
        Cmd::List {} => workspace::list(&cli.project),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

/// True when `project` is a workspace root (its `Curie.toml` has
/// `[workspace]`).  Returns false when the descriptor is missing or
/// malformed — those errors are surfaced later by the command-specific
/// path, with a more useful context message.
fn is_workspace(project: &std::path::Path) -> bool {
    descriptor::load(project).is_ok_and(|d| d.is_workspace())
}

/// Single-module variant of the test pipeline.  Lifted out of the inline
/// match arm so the workspace fan-out can reuse the same conceptual flow
/// (see `workspace::run_member_tests`) without duplicating the printf.
fn test_single_module(project: &std::path::Path, filter: Option<&str>, offline: bool) -> anyhow::Result<()> {
    let desc = descriptor::load(project)?;
    println!(
        "Testing {} v{}",
        desc.project_name(),
        desc.project_version()
    );
    let compiled = compile::compile(project, &desc, offline)?;
    test::run_tests(
        project,
        &desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        filter,
        offline,
    )?;
    Ok(())
}
