mod build;
mod descriptor;
mod docker;
mod run;
mod test;

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
    },
    /// Compile the project and run its tests (no JAR or Docker build)
    Test {
        /// Only run tests whose fully-qualified class name matches this pattern
        #[arg(long)]
        filter: Option<String>,
    },
    /// Build the project and run it (via Docker or java -jar)
    Run {
        /// Skip Docker; run directly with java -jar
        #[arg(long)]
        no_docker: bool,

        /// Arguments to pass to the application (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Remove the target/ build directory
    Clean {},
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Cmd::Build { no_docker } => {
            build::build(&cli.project, build::BuildOptions { no_docker })
        }
        Cmd::Test { filter } => {
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
            let compiled = build::compile(&cli.project, &desc).and_then(|compiled| {
                test::run_tests(
                    &cli.project,
                    &desc,
                    &compiled.classes_dir,
                    &compiled.dep_jars,
                    compiled.resources_dir.as_deref(),
                    compiled.test_resources_dir.as_deref(),
                    filter.as_deref(),
                )?;
                Ok(())
            });
            compiled
        }
        Cmd::Run { no_docker, args } => {
            run::run(&cli.project, run::RunOptions { no_docker }, &args)
        }
        Cmd::Clean {} => build::clean(&cli.project),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
