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
            build::build(&cli.project, build::BuildOptions { no_docker, offline })
        }
        Cmd::Test { filter, offline } => {
            let desc = match descriptor::load(&cli.project) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            };
            println!(
                "Testing {} v{}",
                desc.project_name(),
                desc.project_version()
            );
            let compiled = compile::compile(&cli.project, &desc, offline).and_then(|compiled| {
                test::run_tests(
                    &cli.project,
                    &desc,
                    &compiled.classes_dir,
                    &compiled.dep_jars,
                    compiled.resources_dir.as_deref(),
                    compiled.test_resources_dir.as_deref(),
                    filter.as_deref(),
                    offline,
                )?;
                Ok(())
            });
            compiled
        }
        Cmd::Run { no_docker, offline, args } => {
            run::run(&cli.project, run::RunOptions { no_docker, offline }, &args)
        }
        Cmd::Clean {} => build::clean(&cli.project),
        Cmd::List {} => workspace::list(&cli.project),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
