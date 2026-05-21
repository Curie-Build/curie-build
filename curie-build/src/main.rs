mod audit;
mod build;
mod class_manifest;
mod compile;
mod config;
mod deps;
mod descriptor;
mod docker;
mod fmt;
mod git;
mod incremental;
mod jar;
mod kt_stale;
mod main_class;
mod native;
mod new;
mod pom_writer;
mod publish;
mod run;
mod sources_jar;
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

        /// Skip native-image compilation even when [native-image] is configured
        #[arg(long)]
        no_native: bool,

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
    /// Compile the project and produce a GraalVM native binary (skips tests)
    ///
    /// Runs the full build pipeline (compile, package JAR) and then invokes
    /// `native-image`.  Tests are intentionally skipped so the command is
    /// fast enough for the inner compile→native iteration loop.  Use
    /// `curie build` to also run tests before compiling the native binary.
    ///
    /// Requires GraalVM to be installed.  Curie looks for the `native-image`
    /// executable in $GRAALVM_HOME/bin first, then on $PATH.
    Native {
        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,
    },
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
    /// Print the dependency tree; optionally explain why a specific artifact was chosen
    Deps {
        /// Explain why this artifact was selected (e.g. "org.foo:bar" or "org.foo:bar:1.0")
        #[arg(long)]
        why: Option<String>,
        /// Show [test-dependencies] instead of [dependencies]
        #[arg(long)]
        tests: bool,
        /// Use only locally cached POMs; do not download
        #[arg(long)]
        offline: bool,
    },
    /// Build, sign, and upload artifacts to a Maven repository
    Publish {
        /// Override [publish] repository/url with an inline URL
        #[arg(long)]
        repo: Option<String>,

        /// Skip GPG signing (overrides [publish] sign = true)
        #[arg(long = "no-sign")]
        no_sign: bool,

        /// Skip building the javadoc jar (overrides [publish] javadoc = true)
        #[arg(long = "no-javadoc")]
        no_javadoc: bool,

        /// Build and prepare all artifacts but do not PUT them
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Emit a CycloneDX 1.6 SBOM and check dependencies against the OSV vulnerability database
    Audit {
        /// Include test-scope dependencies in the SBOM and scan
        #[arg(long = "include-test")]
        include_test: bool,

        /// Skip the OSV network call; only emit the SBOM
        #[arg(long)]
        offline: bool,

        /// Show vuln IDs only, skip fetching full detail; exit 1 on any finding
        #[arg(long)]
        short: bool,

        /// CVSS score threshold for a non-zero exit (default: 7.0)
        #[arg(long, default_value = "7.0")]
        severity: f32,

        /// Override the SBOM output path (default: target/sbom.cdx.json)
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },
    /// Scaffold a new Curie project in a new subdirectory
    New {
        /// Project kind: app, lib, or workspace
        kind: new::ProjectKind,

        /// Project name (defaults to current directory name for app/lib)
        name: Option<String>,

        /// Root Java package, e.g. com.example.myapp (derived from name when absent)
        #[arg(long)]
        package: Option<String>,
    },
    /// Initialise a Curie project in the current directory
    Init {
        /// Project kind: app, lib, or workspace
        kind: new::ProjectKind,

        /// Root Java package, e.g. com.example.myapp (derived from directory name when absent)
        #[arg(long)]
        package: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    // `curie new` and `curie init` don't operate on an existing project —
    // they create one.  Skip workspace discovery entirely for them.
    let early_result = match &cli.command {
        Cmd::New { kind, name, package } => {
            Some(new::run_new(*kind, name.clone(), package.clone()))
        }
        Cmd::Init { kind, package } => {
            Some(new::run_init(*kind, package.clone()))
        }
        _ => None,
    };
    if let Some(result) = early_result {
        if let Err(e) = result {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
        return;
    }

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
        Cmd::Build { no_docker, no_native, offline } => {
            let opts = build::BuildOptions { no_docker, no_native, offline };
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
                // just the targeted member's `target/`, not the whole
                // workspace's.
                build::clean(&cli.project)
            }
            workspace::WorkspaceContext::Standalone(project) => build::clean(project),
        },
        Cmd::Native { offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_) => Err(anyhow::anyhow!(
                "`curie native` is ambiguous in a workspace — native binaries are \
                 per-application.  Re-run with --project <member>, e.g.\n  \
                 curie --project examples/graalvm-hello native"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                native_single_module(&cli.project, offline)
            }
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
        Cmd::Deps { why, tests, offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_) => Err(anyhow::anyhow!(
                "`curie deps` cannot run on a workspace root; \
                 target a member with --project"
            )),
            workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                deps::run_deps_workspace_member(
                    workspace_root, *member_index, why.as_deref(), tests, offline,
                )
            }
            workspace::WorkspaceContext::Standalone(project) => {
                deps::run_deps(project, why.as_deref(), tests, offline)
            }
        },
        Cmd::Publish { repo, no_sign, no_javadoc, dry_run } => {
            let target = match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(_) => {
                    Err(anyhow::anyhow!(
                        "`curie publish` cannot run on a workspace root; target a member with --project"
                    ))
                }
                workspace::WorkspaceContext::WorkspaceMember { .. }
                | workspace::WorkspaceContext::Standalone(_) => Ok(cli.project.clone()),
            };
            match target {
                Ok(project) => publish::publish(
                    &project,
                    publish::PublishOptions {
                        repo_url: repo,
                        no_sign,
                        no_javadoc,
                        dry_run,
                        skip_tests: false,
                    },
                ),
                Err(e) => Err(e),
            }
        }
        Cmd::Audit { include_test, offline, short, severity, output } => {
            let opts = audit::AuditOptions {
                include_test,
                offline,
                full: !short,
                severity,
                output,
            };
            let exit_nonzero = match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    workspace::audit_all(root, &opts)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    workspace::audit_one(workspace_root, *member_index, &opts)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    match audit::run_audit(project, &opts) {
                        Ok(report) => Ok(audit::should_exit_nonzero(&report, &opts)),
                        Err(e) => Err(e),
                    }
                }
            };
            match exit_nonzero {
                Ok(true) => {
                    std::process::exit(1);
                }
                Ok(false) => return,
                Err(e) => Err(e),
            }
        }
        // Handled above in the early-exit block; unreachable at runtime.
        Cmd::New { .. } | Cmd::Init { .. } => unreachable!(),
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
        &compiled.kotlin_stdlib_jars,
        &compiled.groovy_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        filter,
        offline,
        &[],
    )?;
    Ok(())
}

/// Single-module variant of the native-image pipeline.
///
/// Runs compile → package JAR (no tests) → native-image.  Tests are
/// intentionally skipped so this command is fast enough for the inner
/// compile→native iteration loop.  The `[native-image]` section must be
/// present in `Curie.toml`; if it is absent this function errors early.
fn native_single_module(project: &std::path::Path, offline: bool) -> anyhow::Result<()> {
    let desc = descriptor::load(project)?;

    if !descriptor::native_image_enabled(&desc) {
        anyhow::bail!(
            "native-image is not enabled for this project.\n\
             Add a [native-image] section to Curie.toml to enable it, e.g.:\n\n  \
             [native-image]\n  extraArgs = [\"--no-fallback\"]"
        );
    }

    println!(
        "Native  {} v{}",
        desc.buildable_name(),
        desc.buildable_version()
    );

    // compile + package JAR, skipping tests and Docker
    let opts = build::BuildOptions {
        no_docker: true,
        no_native: true, // we call native::build_native ourselves below
        offline,
    };
    let output = build::build_with_desc(project, &desc, opts, &[])?;

    native::build_native(project, &desc, &output.jar, &output.dep_jars)?;

    Ok(())
}
