use crate::{build, descriptor, docker};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

pub struct RunOptions {
    pub no_docker: bool,
}

pub fn run(project_root: &Path, opts: RunOptions, extra_args: &[String]) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    if desc.is_library() {
        bail!("`curie run` is not supported for library projects");
    }

    let app = desc.application.as_ref().expect("non-library has application");

    let output = build::do_build(project_root, &desc)?;

    println!(
        "Running {} v{}",
        app.name, app.version
    );
    println!();

    let use_docker = !opts.no_docker && descriptor::docker_enabled(project_root, &desc);

    if use_docker {
        docker::docker_run(project_root, &desc, &output.jar, &output.dep_jars, extra_args)?;
    } else {
        let mut java = Command::new("java");

        // When deps are present the JAR has a Class-Path manifest header that
        // references libs/<name>.jar relative to the JAR location.  At dev-run
        // time those libs live in ~/.m2, so we supply an explicit -cp instead.
        if !output.dep_jars.is_empty() {
            let cp = build::classpath_string(&output.dep_jars);
            java.arg("-cp").arg(format!(
                "{}{}{}",
                output.jar.to_string_lossy(),
                if cfg!(windows) { ";" } else { ":" },
                cp
            ));
            // Main class must be specified explicitly when using -cp (not -jar).
            java.arg(&app.main_class);
        } else {
            java.arg("-jar").arg(&output.jar);
        }

        for arg in extra_args {
            java.arg(arg);
        }

        let status = java
            .status()
            .context("failed to invoke java — is a JRE installed?")?;

        if !status.success() {
            let code = status.code().unwrap_or(1);
            std::process::exit(code);
        }
    }

    Ok(())
}
