use crate::jar::classpath_string;
use crate::{build, descriptor, jlink, native, oci};
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Resolve the main class to launch, or produce an actionable error when it
/// couldn't be determined.  `main_class` is `None` when the packaged JAR is
/// up to date but corrupt, externally built, or has no `Main-Class`, and the
/// user also didn't declare `mainClass` under `[application]`.  Previously this
/// hit an `.expect(...)` and panicked with an internal-invariant message.
pub(crate) fn resolve_main_class<'a>(
    main_class: Option<&'a str>,
    name: &str,
    jar: &Path,
) -> Result<&'a str> {
    main_class.ok_or_else(|| {
        anyhow!(
            "could not determine the main class for `{name}`.\n\
             The packaged JAR ({}) has no readable Main-Class — it may be corrupt \
             or externally built. Set `mainClass` under [application] in Curie.toml, \
             or run `curie clean` and build again.",
            jar.display(),
        )
    })
}

pub struct RunOptions {
    pub no_docker: bool,
    pub offline: bool,
}

pub fn run(project_root: &Path, opts: RunOptions, extra_args: &[String]) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    if desc.is_library() {
        bail!("`curie run` is not supported for library projects");
    }

    let app = desc.application().expect("non-library has application");

    let build_opts = build::BuildOptions {
        no_docker: opts.no_docker,
        no_native: false,
        no_jlink: false,
        offline: opts.offline,
        coverage: false,
    };
    let output = build::do_build(project_root, &desc, build_opts, &[])?;

    let main_class = resolve_main_class(output.main_class.as_deref(), &app.name, &output.jar)?;

    println!("{}", crate::style::run_step(&app.name, &app.version));
    println!();

    let use_docker = !opts.no_docker && descriptor::docker_enabled(project_root, &desc);

    // When a fat JAR is available, use it as the single self-contained JAR.
    let effective_jar = output.fat_jar.as_ref().unwrap_or(&output.jar);
    let effective_deps: Vec<std::path::PathBuf> = if output.fat_jar.is_some() {
        vec![]
    } else {
        output.dep_jars.clone()
    };

    if use_docker {
        // The generated Dockerfile packages the native/jlink artifact instead
        // of the JAR when configured (see docker::select_docker_artifact),
        // but `do_build` above never builds those — only `build_with_desc`
        // does. Build whichever one Docker will need before handing off.
        if descriptor::native_image_enabled(&desc) {
            native::build_native(project_root, &desc, effective_jar, &effective_deps)?;
        } else if descriptor::jlink_enabled(&desc) {
            jlink::build_jlink(project_root, &desc, effective_jar, &effective_deps)?;
        }
        oci::run_image(
            project_root,
            &desc,
            effective_jar,
            &effective_deps,
            extra_args,
            opts.offline,
        )?;
    } else {
        let mut java = Command::new("java");
        if desc.java.preview_enabled() {
            java.arg("--enable-preview");
        }

        let agent_coords = desc.dep_java_agent_coords();
        let all_jars: Vec<_> = std::iter::once(effective_jar)
            .chain(effective_deps.iter())
            .cloned()
            .collect();
        let agent_jars = crate::java_agent::find_agent_jars(&agent_coords, &all_jars);
        for agent in &agent_jars {
            java.arg(format!("-javaagent:{}", agent.display()));
        }

        // Modular launch: use --module-path + --module instead of -cp/-jar.
        if output.is_modular && output.fat_jar.is_none() {
            let module_name = output.module_name.as_deref().expect(
                "is_modular implies module_name is Some after a successful build"
            );
            let resources_dir = output.resources_dir.as_deref();

            // Build the module path: module-path JARs + the project's own JAR.
            let mut mp_entries: Vec<std::path::PathBuf> = Vec::new();
            mp_entries.extend_from_slice(&output.module_path_jars);
            mp_entries.push(effective_jar.clone());
            java.arg("--module-path").arg(classpath_string(&mp_entries));

            // Remaining deps (not on module-path) go on -cp.
            let mut cp_entries: Vec<std::path::PathBuf> = Vec::new();
            for dep in &effective_deps {
                if !output.module_path_jars.contains(dep) {
                    cp_entries.push(dep.clone());
                }
            }
            if let Some(rd) = resources_dir {
                if rd.exists() {
                    cp_entries.push(rd.to_path_buf());
                }
            }
            if !cp_entries.is_empty() {
                java.arg("-cp").arg(classpath_string(&cp_entries));
            }

            java.arg("--module").arg(format!("{}/{}", module_name, main_class));
        } else {
            // When running with deps (can't use -jar), build a full classpath.
            // Also include src/main/resources so resource loading via getResourceAsStream works.
            // Fat JARs are self-contained — run directly with -jar.
            let resources_dir = output.resources_dir.as_deref();
            let has_deps = !effective_deps.is_empty();
            let has_resources = resources_dir.map(|p| p.exists()).unwrap_or(false);

            if has_deps || (has_resources && output.fat_jar.is_none()) {
                let mut cp_entries = Vec::new();
                cp_entries.push(effective_jar.clone());
                if let Some(rd) = resources_dir {
                    if rd.exists() {
                        cp_entries.push(rd.to_path_buf());
                    }
                }
                cp_entries.extend_from_slice(&effective_deps);
                java.arg("-cp").arg(classpath_string(&cp_entries));
                java.arg(main_class);
            } else {
                java.arg("-jar").arg(effective_jar);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn resolve_main_class_returns_value_when_present() {
        let jar = PathBuf::from("target/app.jar");
        let mc = resolve_main_class(Some("com.example.Main"), "app", &jar).unwrap();
        assert_eq!(mc, "com.example.Main");
    }

    #[test]
    fn resolve_main_class_errors_with_guidance_when_absent() {
        let jar = PathBuf::from("target/app.jar");
        let err = resolve_main_class(None, "app", &jar).unwrap_err().to_string();
        // The message must name the project, the JAR, and the two remedies, so
        // the user-facing guidance can't silently regress.
        assert!(err.contains("app"), "should name the project: {err}");
        assert!(err.contains("target/app.jar"), "should show the jar path: {err}");
        assert!(err.contains("mainClass"), "should suggest declaring mainClass: {err}");
        assert!(err.contains("curie clean"), "should suggest curie clean: {err}");
    }
}
