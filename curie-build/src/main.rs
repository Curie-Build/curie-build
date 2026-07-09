mod add_remove;
mod api_search;
mod api_search_ui;
mod version_ui;
mod audit;
mod dev;
mod setup;
mod inspect_ui;
mod build;
mod class_manifest;
mod compile;
mod config;
mod coverage;
mod deps;
mod descriptor;
mod docker;
mod oci;
mod fat_jar;
mod fetch;
mod fmt;
mod git;
mod incremental;
mod jar;
mod jpms;
mod java_agent;
mod jlink;
mod kt_stale;
mod main_class;
mod maven;
mod native;
mod new;
mod parallel;
mod plugin;
mod pom_writer;
mod proc;
mod publish;
mod resources;
mod run;
mod sources_jar;
mod style;
mod term;
mod test;
mod test_runner;
#[cfg(test)]
mod testenv;
mod tui;
mod update;
mod workspace;
mod wrapper;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "curie",
    about = "The Curie build tool",
    version,
    long_version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT_HASH"), ")")
)]
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

        /// Skip jlink runtime-image assembly even when [jlink] is configured
        #[arg(long)]
        no_jlink: bool,

        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,

        /// Maximum number of workspace members to build in parallel (default: CPU count)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
    /// Compile the project and run its tests (no JAR or Docker build)
    Test {
        /// Only run tests whose fully-qualified class name matches this pattern
        #[arg(long)]
        filter: Option<String>,

        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,

        /// Collect code coverage via JaCoCo and produce a report under target/coverage/
        #[arg(long)]
        coverage: bool,

        /// Maximum number of workspace members to test in parallel (default: CPU count)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
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
    /// Compile and run in development mode: launches from class files (no JAR),
    /// watches sources for changes, and restarts automatically on every edit
    Dev {
        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,

        /// Arguments to pass to the application (after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Remove the target/ build directory
    Clean {
        /// Maximum number of workspace members to clean in parallel (default: CPU count)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
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
    /// Compile the project and assemble a self-contained JDK runtime image
    ///
    /// Runs the full build pipeline (compile, test, package JAR) and then
    /// invokes the plain JDK's `jlink` — no GraalVM, no AOT compilation.
    /// Works with any JDK 21+ already on `PATH`.
    Jlink {
        /// Do not access the network; use only locally cached artifacts
        #[arg(long)]
        offline: bool,
    },
    /// Show the workspace tree (focused on the current project by default)
    List {
        /// Show the full workspace tree including unrelated siblings
        #[arg(long)]
        all: bool,
    },
    /// Format Java source files with palantir-java-format
    Fmt {
        /// Check formatting without modifying files; exit non-zero if any
        /// file would be reformatted (useful in CI)
        #[arg(long)]
        check: bool,

        /// Do not download formatter JARs; fail if not already cached
        #[arg(long)]
        offline: bool,

        /// Maximum number of members to format in parallel
        #[arg(short, long)]
        jobs: Option<usize>,
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
    /// Download dependency artifacts into the local Maven cache (~/.m2/repository)
    Fetch {
        /// Coordinates "group:artifact:version" to fetch (one or more). Omit to
        /// fetch every dependency declared in Curie.toml, including
        /// [test-dependencies] and annotation processors. Supplying a
        /// coordinate that a transitive range needs pins that range.
        #[arg(num_args = 0..)]
        coords: Vec<String>,

        /// Read coordinates from a file (one "group:artifact:version" per line;
        /// blank lines and lines starting with '#' are ignored). Mutually
        /// exclusive with positional coordinates.
        #[arg(long, conflicts_with = "coords")]
        file: Option<std::path::PathBuf>,

        /// With coordinates, download only those artifacts — skip their
        /// transitive dependencies. Requires at least one coordinate.
        #[arg(long)]
        no_transitive: bool,

        /// Do not access the network; fail if any artifact is not already cached
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
    /// Check for newer stable versions of all versioned dependencies and optionally update Curie.toml
    Update {
        /// Report available updates but do not rewrite Curie.toml; exit 1 when any updates exist
        #[arg(long)]
        check: bool,

        /// Do not access the network; skip the update check
        #[arg(long)]
        offline: bool,

        /// Skip [test-dependencies] and [test-bom-imports]
        #[arg(long = "no-test")]
        no_test: bool,
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
    /// Add a dependency to Curie.toml
    Add {
        /// Coordinate: "group:artifact" or "group:artifact@version".
        /// Omit to open the interactive artifact search UI.
        coord: Option<String>,
        /// Add to [test-dependencies] instead of [dependencies]
        #[arg(long)]
        test: bool,
        /// Add to [annotation-processors] (combine with --test for [test-annotation-processors])
        #[arg(long = "annotation-processor")]
        annotation_processor: bool,
        /// Add to [bom-imports] (combine with --test for [test-bom-imports]); requires @version
        #[arg(long)]
        bom: bool,
        /// Do not access the network; fail if a version must be resolved
        #[arg(long)]
        offline: bool,
    },
    /// Remove a dependency from Curie.toml
    Remove {
        /// Coordinate: "group:artifact" (a trailing @version is accepted but ignored)
        coord: String,
        /// Remove from [test-dependencies]
        #[arg(long)]
        test: bool,
        /// Remove from [annotation-processors] (combine with --test for [test-annotation-processors])
        #[arg(long = "annotation-processor")]
        annotation_processor: bool,
        /// Remove from [bom-imports] (combine with --test for [test-bom-imports])
        #[arg(long)]
        bom: bool,
    },
    /// Inspect the merged logs of the last build in an interactive TUI
    Inspect {},

    /// Download and install shell completions for the detected shell
    ///
    /// Detects your shell from $SHELL, then downloads the completion script
    /// that matches this exact binary version from GitHub and installs it to
    /// the conventional per-user completions directory.
    Setup {
        /// Override shell detection: fish, bash, or zsh
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,
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
    /// Maven interop: generate pom.xml from Curie.toml
    Maven {
        #[command(subcommand)]
        cmd: MavenCmd,
    },
}

#[derive(Subcommand)]
enum MavenCmd {
    /// Generate or refresh pom.xml (and, in a workspace, the aggregator pom.xml) from Curie.toml
    Sync {
        /// Write nothing; exit 1 if any generated pom.xml is missing or stale
        #[arg(long)]
        check: bool,

        /// Overwrite a pom.xml that does not carry the generated-by-Curie marker
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // `curie new`, `curie init`, `curie setup`, and `curie fetch --file`
    // don't require an existing Curie.toml project.  Skip workspace
    // discovery entirely for them.
    let early_result = match &cli.command {
        Cmd::New { kind, name, package } => {
            Some(new::run_new(*kind, name.clone(), package.clone()))
        }
        Cmd::Init { kind, package } => {
            Some(new::run_init(*kind, package.clone()))
        }
        Cmd::Setup { shell } => {
            Some(setup::run_setup(shell.clone()))
        }
        Cmd::Fetch { file: Some(path), no_transitive, offline, .. } => {
            Some(fetch::run_fetch_file(&cli.project, path, *no_transitive, *offline))
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
        Cmd::Build { no_docker, no_native, no_jlink, offline, jobs } => {
            let opts = build::BuildOptions { no_docker, no_native, no_jlink, offline, coverage: false };
            let jobs = resolve_jobs(jobs);
            match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    workspace::build_all(root, opts, jobs)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    workspace::build_one(workspace_root, *member_index, opts, jobs)
                }
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    workspace::build_subtree(workspace_root, member_indices, opts, jobs)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    build::build(project, opts)
                }
            }
        }
        Cmd::Test { filter, offline, coverage, jobs } => {
            let jobs = resolve_jobs(jobs);
            match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    workspace::test_all(root, filter.as_deref(), offline, coverage, jobs)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    workspace::test_one(workspace_root, *member_index, filter.as_deref(), offline, coverage, jobs)
                }
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    workspace::test_subtree(workspace_root, member_indices, filter.as_deref(), offline, coverage, jobs)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    test_single_module(project, filter.as_deref(), offline, coverage)
                }
            }
        }
        Cmd::Run { no_docker, offline, args } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
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
        Cmd::Dev { offline, args } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                "`curie dev` is ambiguous in a workspace.  Re-run with \
                 --project <member> to choose one, e.g.\n  \
                 curie --project examples/hello-world dev"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                dev::run_dev(&cli.project, dev::DevOptions { offline }, &args)
            }
        },
        Cmd::Clean { jobs } => {
            let jobs = resolve_jobs(jobs);
            match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => workspace::clean_all(root, jobs),
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    workspace::clean_subtree(workspace_root, member_indices, jobs)
                }
                workspace::WorkspaceContext::WorkspaceMember { .. } => {
                    build::clean(&cli.project)
                }
                workspace::WorkspaceContext::Standalone(project) => build::clean(project),
            }
        }
        Cmd::Native { offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                "`curie native` is ambiguous in a workspace — native binaries are \
                 per-application.  Re-run with --project <member>, e.g.\n  \
                 curie --project examples/graalvm-hello native"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                native_single_module(&cli.project, offline)
            }
        },
        Cmd::Jlink { offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                "`curie jlink` is ambiguous in a workspace — runtime images are \
                 per-application.  Re-run with --project <member>, e.g.\n  \
                 curie --project examples/jlink-hello jlink"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                jlink_single_module(&cli.project, offline)
            }
        },
        Cmd::List { all } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root)
            | workspace::WorkspaceContext::WorkspaceMember { workspace_root: root, .. }
            | workspace::WorkspaceContext::WorkspaceSubtree { workspace_root: root, .. } => {
                workspace::list(root, &cli.project, all, crate::term::use_color())
            }
            workspace::WorkspaceContext::Standalone(project) => {
                workspace::list(project, project, all, crate::term::use_color())
            }
        },
        Cmd::Fmt { check, offline, jobs } => {
            let jobs = resolve_jobs(jobs);
            match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(root) => {
                workspace::fmt_all(root, check, offline, jobs)
            }
            workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                workspace::fmt_subtree(workspace_root, member_indices, check, offline, jobs)
            }
            workspace::WorkspaceContext::WorkspaceMember { .. } => {
                fmt::run_fmt(&cli.project, check, offline)
            }
            workspace::WorkspaceContext::Standalone(project) => {
                fmt::run_fmt(project, check, offline)
            }
        }},
        Cmd::Deps { why, tests, offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
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
        Cmd::Fetch { coords, file, no_transitive, offline } => {
            if let Some(path) = file {
                fetch::run_fetch_file(&cli.project, &path, no_transitive, offline)
            } else {
                match &ctx {
                    workspace::WorkspaceContext::WorkspaceRoot(_)
                    | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                        "`curie fetch` cannot run on a workspace root; \
                         target a member with --project"
                    )),
                    workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                        fetch::run_fetch_workspace_member(
                            workspace_root, *member_index, &coords, no_transitive, offline,
                        )
                    }
                    workspace::WorkspaceContext::Standalone(project) => {
                        fetch::run_fetch(project, &coords, no_transitive, offline)
                    }
                }
            }
        },
        Cmd::Publish { repo, no_sign, no_javadoc, dry_run } => {
            let target = match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(_)
                | workspace::WorkspaceContext::WorkspaceSubtree { .. } => {
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
        Cmd::Update { check, offline, no_test } => {
            let opts = update::UpdateOptions {
                check,
                offline,
                include_test: !no_test,
            };
            let any_updates = match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    workspace::update_all(root, &opts)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    workspace::update_one(workspace_root, *member_index, &opts)
                }
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    workspace::update_subtree(workspace_root, member_indices, &opts)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    match update::run_update(project, &opts) {
                        Ok(report) => Ok(report.has_updates()),
                        Err(e) => Err(e),
                    }
                }
            };
            match any_updates {
                Ok(true) if check => {
                    std::process::exit(1);
                }
                Ok(_) => return,
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
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    workspace::audit_subtree(workspace_root, member_indices, &opts)
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
        Cmd::Maven { cmd: MavenCmd::Sync { check, force } } => {
            // Phase 1 has no `--offline` flag for `maven sync`; pinTransitive
            // and BOM-managed annotation-processor resolution (when needed)
            // are allowed to hit the network.
            let any_written = match &ctx {
                workspace::WorkspaceContext::WorkspaceRoot(root) => {
                    maven::run_maven_sync_workspace_root(root, force, check, false)
                }
                workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                    maven::run_maven_sync_workspace_member(workspace_root, *member_index, force, check, false)
                }
                workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                    maven::run_maven_sync_workspace_subtree(workspace_root, member_indices, force, check, false)
                }
                workspace::WorkspaceContext::Standalone(project) => {
                    maven::run_maven_sync_standalone(project, force, check, false)
                }
            };
            match any_written {
                Ok(true) if check => {
                    std::process::exit(1);
                }
                Ok(_) => return,
                Err(e) => Err(e),
            }
        }
        Cmd::Inspect {} => run_inspect(&ctx),

        // Handled above in the early-exit block; unreachable at runtime.
        Cmd::New { .. } | Cmd::Init { .. } => unreachable!(),
        Cmd::Add { coord, test, annotation_processor, bom, offline } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                "`curie add` cannot run on a workspace root; \
                 target a member with --project"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                add_remove::run_add(
                    &cli.project,
                    coord.as_deref(),
                    add_remove::AddOptions { test, annotation_processor, bom, offline },
                )
            }
        },
        Cmd::Remove { coord, test, annotation_processor, bom } => match &ctx {
            workspace::WorkspaceContext::WorkspaceRoot(_)
            | workspace::WorkspaceContext::WorkspaceSubtree { .. } => Err(anyhow::anyhow!(
                "`curie remove` cannot run on a workspace root; \
                 target a member with --project"
            )),
            workspace::WorkspaceContext::WorkspaceMember { .. }
            | workspace::WorkspaceContext::Standalone(_) => {
                add_remove::run_remove(
                    &cli.project,
                    &coord,
                    add_remove::RemoveOptions { test, annotation_processor, bom },
                )
            }
        },
        // Handled before workspace discovery in the early_result block above.
        Cmd::Setup { .. } => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

/// Single-module variant of the test pipeline.  Lifted out of the inline
/// match arm so the workspace fan-out can reuse the same conceptual flow
/// (see `workspace::run_member_tests`) without duplicating the printf.
fn test_single_module(project: &std::path::Path, filter: Option<&str>, offline: bool, cli_coverage: bool) -> anyhow::Result<()> {
    let desc = descriptor::load(project)?;
    if desc.is_bom() {
        println!("{}", style::neutral("Tests", "skipped for BOM"));
        return Ok(());
    }
    println!(
        "Testing {} v{}",
        desc.buildable_name(),
        desc.buildable_version()
    );
    let compiled = compile::compile(project, &desc, offline, &[])?;
    let enable_coverage = cli_coverage || desc.test.coverage_enabled();
    let target_dir = compiled.classes_dir.parent().unwrap_or(project);
    let (eff_main, eff_test) = resources::effective_test_dirs(
        project,
        &desc,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        target_dir,
    )?;
    test::run_tests(
        project,
        &desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        &compiled.kotlin_stdlib_jars,
        &compiled.groovy_jars,
        eff_main.as_deref(),
        eff_test.as_deref(),
        filter,
        offline,
        enable_coverage,
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
        no_jlink: true,
        offline,
        coverage: false,
    };
    let output = build::build_with_desc(project, &desc, opts, &[])?;

    let effective_jar = output.fat_jar.as_ref().unwrap_or(&output.jar);
    let effective_deps: &[std::path::PathBuf] = if output.fat_jar.is_some() { &[] } else { &output.dep_jars };
    native::build_native(project, &desc, effective_jar, effective_deps)?;

    Ok(())
}

/// Single-module variant of the jlink pipeline.
///
/// Runs the full build pipeline (compile, test, package JAR) → jlink.  The
/// `[jlink]` section must be present in `Curie.toml`; if it is absent this
/// function errors early.
fn jlink_single_module(project: &std::path::Path, offline: bool) -> anyhow::Result<()> {
    let desc = descriptor::load(project)?;

    if !descriptor::jlink_enabled(&desc) {
        anyhow::bail!(
            "jlink is not enabled for this project.\n\
             Add a [jlink] section to Curie.toml to enable it, e.g.:\n\n  \
             [jlink]\n  stripDebug = true"
        );
    }

    println!(
        "Jlink   {} v{}",
        desc.buildable_name(),
        desc.buildable_version()
    );

    // compile + package JAR, skipping Docker and native-image
    let opts = build::BuildOptions {
        no_docker: true,
        no_native: true,
        no_jlink: true, // we call jlink::build_jlink ourselves below
        offline,
        coverage: false,
    };
    let output = build::build_with_desc(project, &desc, opts, &[])?;

    let effective_jar = output.fat_jar.as_ref().unwrap_or(&output.jar);
    let effective_deps: &[std::path::PathBuf] = if output.fat_jar.is_some() { &[] } else { &output.dep_jars };
    jlink::build_jlink(project, &desc, effective_jar, effective_deps)?;

    Ok(())
}

/// Dispatch `curie inspect` for all four workspace contexts.
fn run_inspect(ctx: &workspace::WorkspaceContext) -> anyhow::Result<()> {
    use inspect_ui::{LogTarget, run_inspect_ui};
    match ctx {
        workspace::WorkspaceContext::WorkspaceRoot(root) => {
            let ws      = workspace::load(root)?;
            let targets = member_targets(&ws.members);
            run_inspect_ui(root, &targets, "build", None)
        }
        workspace::WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
            let ws      = workspace::load(workspace_root)?;
            let targets = member_targets(&ws.members);
            run_inspect_ui(workspace_root, &targets, "build", Some(*member_index))
        }
        workspace::WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
            let ws      = workspace::load(workspace_root)?;
            let targets: Vec<LogTarget> = member_indices.iter()
                .map(|&i| LogTarget {
                    declared: ws.members[i].declared.clone(),
                    path:     ws.members[i].path.clone(),
                })
                .collect();
            run_inspect_ui(workspace_root, &targets, "build", None)
        }
        workspace::WorkspaceContext::Standalone(path) => {
            let name = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_string();
            let targets = vec![LogTarget { declared: name, path: path.clone() }];
            run_inspect_ui(path, &targets, "build", None)
        }
    }
}

/// Build a `LogTarget` slice from workspace members for `curie inspect`.
fn member_targets(members: &[workspace::Member]) -> Vec<inspect_ui::LogTarget> {
    members.iter().map(|m| inspect_ui::LogTarget {
        declared: m.declared.clone(),
        path:     m.path.clone(),
    }).collect()
}

/// Resolve `--jobs` option: explicit value wins; default to available parallelism.
fn resolve_jobs(jobs: Option<usize>) -> usize {
    jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_module_skips_bom_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            r#"[bom]
name    = "test-bom"
version = "1.0.0"
groupId = "com.example"
"#,
        )
        .unwrap();
        let result = test_single_module(dir.path(), None, true, false);
        assert!(result.is_ok(), "expected Ok for BOM project, got: {result:?}");
    }
}
