mod build;
mod class_manifest;
mod compile;
mod descriptor;
mod docker;
mod fmt;
mod git;
mod incremental;
mod jar;
mod main_class;
mod run;
mod test;
mod workspace;
mod wrapper;

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
    /// Format Java source files with palantir-java-format
    Fmt {
        /// Check formatting without modifying files; exit non-zero if any
        /// file would be reformatted (useful in CI)
        #[arg(long)]
        check: bool,

        /// Do not download formatter JARs; fail if not already cached
        #[arg(long)]
        offline: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // Discovery is done once per invocation so every command sees a
    // consistent view of (project, surrounding workspace) — and so a
    // failure to discover surfaces before the command-specific logic
    // gets a chance to throw a less-useful error.
    let ctx = match workspace::discover(&cli.project) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Cmd::Build { no_docker, offline } => {
            let opts = build::BuildOptions { no_docker, offline };
            match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    workspace::build_all(root, opts)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    workspace::build_one(workspace_root, *member_index, opts)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    build::build(project, opts)
                }
            }
        }
        Cmd::Test { filter, offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root) => {
                workspace::test_all(root, filter.as_deref(), offline)
            }
            workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                workspace::test_one(workspace_root, *member_index, filter.as_deref(), offline)
            }
            workspace::WorkspaceContext::Standalone(project) => {
                test_single_module(project, filter.as_deref(), offline)
            }
        },
        Cmd::Run { no_docker, offline, args } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_) => Err(anyhow::anyhow!(
                "`curie run` is ambiguous in a workspace.  Re-run with \
                 --project <member> to choose one, e.g.\n  \
                 curie --project examples/hello-world run"
            )),
            workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                let opts = run::RunOptions { no_docker, offline };
                // Members without [workspace-dependencies] don't need
                // the workspace-aware runtime classpath; the standalone
                // path also keeps Docker working for them.  Members WITH
                // workspace-deps go through run_one so their upstream
                // members' JARs land on -cp.
                let has_ws_deps = match descriptor::load(&cli.project) {
                    Ok(d) => !d.workspace_dependencies.is_empty(),
                    Err(_) => false,
                };
                if has_ws_deps {
                    workspace::run_one(workspace_root, *member_index, opts, &args)
                } else {
                    run::run(&cli.project, opts, &args)
                }
            }
            workspace::WorkspaceContext::Standalone(project) => {
                run::run(project, run::RunOptions { no_docker, offline }, &args)
            }
        },
        Cmd::Clean {} => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root) => workspace::clean_all(root),
            workspace::WorkspaceContext::WorkspaceMember { .. } => {
                // Per-member `clean` matches Cargo's semantics: it wipes
                // just the targeted member's `target/`, not the whole
                // workspace's.
                build::clean(&cli.project)
            }
            workspace::WorkspaceContext::Standalone(project) => build::clean(project),
        },
        Cmd::List {} => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root)
            | workspace::WorkspaceContext::WorkspaceMember { workspace_root: root, .. } => {
                workspace::list(root)
            }
            workspace::WorkspaceContext::Standalone(_) => Err(anyhow::anyhow!(
                "`curie list` only makes sense inside a workspace.  Add a \
                 [workspace] Curie.toml at the project root, or invoke \
                 from a workspace member's directory."
            )),
        },
        Cmd::Fmt { check, offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root) => {
                workspace::fmt_all(root, check, offline)
            }
            workspace::WorkspaceContext::WorkspaceMember { .. } => {
                fmt::run_fmt(&cli.project, check, offline)
            }
            workspace::WorkspaceContext::Standalone(project) => {
                fmt::run_fmt(project, check, offline)
            }
        },
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

/// Single-module variant of the test pipeline.  Lifted out of the inline
/// match arm so the workspace fan-out can reuse the same conceptual flow
/// (see `workspace::run_member_tests`) without duplicating the printf.
fn test_single_module(project: &std::path::Path, filter: Option<&str>, offline: bool) -> anyhow::Result<()> {
    let desc = descriptor::load(project)?;
    println!(
        "Testing {} v{}",
        desc.buildable_name(),
        desc.buildable_version()
    );
    let compiled = compile::compile(project, &desc, offline, &[])?;
    test::run_tests(
        project,
        &desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        filter,
        offline,
        &[],
    )?;
    Ok(())
}
