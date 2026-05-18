//! Top-level build orchestrator: ties `compile`, `test`, `jar`, `main_class`,
//! and `docker` together for the `curie build` and `curie clean` commands.

use crate::compile::compile;
use crate::descriptor;
use crate::docker;
use crate::git;
use crate::incremental::needs_repackage;
use crate::jar::{populate_libs_dir, write_deterministic_jar};
use crate::main_class::{detect_main_class, validate_main_class};
use crate::test;
use anyhow::{Context, Result};
use curie_deps::repo::Repository;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone)]
pub struct BuildOptions {
    pub no_docker: bool,
    pub offline: bool,
}

/// Output paths produced by a successful build.
pub struct BuildOutput {
    pub jar: PathBuf,
    /// Resolved dependency JARs (empty when no [dependencies] declared).
    pub dep_jars: Vec<PathBuf>,
    /// Resolved (declared or auto-detected) main class; `None` for library projects.
    pub main_class: Option<String>,
    /// `src/main/resources` if the directory exists, otherwise `None`.
    pub resources_dir: Option<PathBuf>,
}

/// Single-module entry point used by `curie build` outside a workspace.
/// Loads the descriptor, then defers to [`build_with_desc`] with an empty
/// extra-classpath.
pub fn build(project_root: &Path, opts: BuildOptions) -> Result<()> {
    let desc = descriptor::load(project_root)?;
    build_with_desc(project_root, &desc, opts, &[]).map(|_| ())
}

/// Run the full single-module pipeline for a project whose descriptor has
/// already been loaded, with extra classpath entries appended to compile
/// and test.  Used by [`build`] (with `&[]`) and by `workspace::build_all`
/// (which threads each member's workspace-dep classpath here).
pub fn build_with_desc(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    opts: BuildOptions,
    extra_cp: &[PathBuf],
) -> Result<BuildOutput> {
    println!(
        "Building {} v{}",
        desc.buildable_name(), desc.buildable_version()
    );

    // Library projects must not have a Dockerfile at the project root.
    if desc.is_library() && project_root.join("Dockerfile").exists() {
        anyhow::bail!(
            "library projects do not support Docker: remove the Dockerfile from the project root"
        );
    }

    let output = do_build(project_root, desc, opts.offline, extra_cp)?;

    println!(
        "  Done            {}",
        output
            .jar
            .strip_prefix(project_root)
            .unwrap_or(&output.jar)
            .display()
    );

    if !desc.is_library() && !opts.no_docker && descriptor::docker_enabled(project_root, desc) {
        docker::docker_build(project_root, desc, &output.jar, &output.dep_jars)?;
    }

    Ok(output)
}

/// Build the list of extra Maven repositories from the descriptor.
/// Shared by production and test dependency resolution.
pub fn extra_repos(desc: &descriptor::Descriptor) -> Vec<Repository> {
    desc.repositories
        .iter()
        .map(|r| Repository {
            name: r.name.clone(),
            url: r.url.clone(),
        })
        .collect()
}

/// Phase 2: compile production sources, run tests, then package JAR.
pub fn do_build(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    offline: bool,
    extra_cp: &[PathBuf],
) -> Result<BuildOutput> {
    let compiled = compile(project_root, desc, offline, extra_cp)?;

    // --- run tests before packaging ------------------------------------------
    test::run_tests(
        project_root,
        desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        &compiled.kotlin_stdlib_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        None,
        offline,
        extra_cp,
    )?;

    // --- package (deterministic JAR, incremental) ----------------------------
    // mainClass detection/validation is deferred to here: it is only needed to
    // write the JAR manifest, so we skip it entirely when packaging is up to date.
    let resources_dir = compiled.resources_dir.as_deref();
    let toml_path = project_root.join("Curie.toml");

    // Detect Git information once for the whole packaging step.
    // `None` when git is unavailable or the project is not in a repo.
    let build_info_content: Option<String> = if desc.build_info.enabled {
        git::detect(project_root).map(|info| {
            format!("git.commit.id={}\n", info.commit_id)
        })
    } else {
        None
    };

    let resolved_main_class: Option<String> = if needs_repackage(&compiled.jar_path, &compiled.classes_dir, resources_dir, &toml_path) {
        let main_class = if let Some(app) = desc.application() {
            let mc = match &app.main_class {
                Some(declared) => {
                    validate_main_class(declared, &compiled.classes_dir, &compiled.dep_jars)?;
                    declared.clone()
                }
                None => {
                    let detected = detect_main_class(
                        &compiled.src_roots,
                        &compiled.sources,
                        &compiled.classes_dir,
                        &compiled.dep_jars,
                    )?;
                    println!("  Detected        mainClass = {}", detected);
                    detected
                }
            };
            Some(mc)
        } else {
            None // library
        };

        println!("  Package         {}", compiled.jar_name);
        write_deterministic_jar(
            &compiled.jar_path,
            &compiled.classes_dir,
            resources_dir,
            main_class.as_deref(),
            &compiled.dep_jars,
            build_info_content.as_deref(),
        )
        .context("failed to write JAR")?;

        main_class
    } else {
        println!("  Package         up to date");
        // JAR is up to date. Prefer the declared mainClass from the descriptor;
        // if absent (auto-detected on a previous build), read it back from the
        // JAR manifest so `curie run` doesn't panic.
        if let Some(declared) = desc.application().and_then(|a| a.main_class.clone()) {
            Some(declared)
        } else if desc.application().is_some() {
            read_main_class_from_jar(&compiled.jar_path)
        } else {
            None
        }
    };

    // --- populate target/libs/ with dep JARs (hardlink preferred) ------------
    // Always done for application projects so that `java -jar` works.
    // target/libs/ is wiped and repopulated on every build to stay in sync
    // with the current dep set (handles version bumps cleanly).
    if !compiled.dep_jars.is_empty() && desc.application().is_some() {
        let libs_dir = project_root.join("target").join("libs");
        populate_libs_dir(&libs_dir, &compiled.dep_jars)
            .context("failed to populate target/libs")?;
    }

    Ok(BuildOutput {
        jar: compiled.jar_path,
        dep_jars: compiled.dep_jars,
        main_class: resolved_main_class,
        resources_dir: compiled.resources_dir,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the `Main-Class` attribute from an existing JAR's manifest.
/// Returns `None` if the JAR doesn't exist, has no manifest, or has no
/// `Main-Class` entry.
fn read_main_class_from_jar(jar_path: &Path) -> Option<String> {
    let file = std::fs::File::open(jar_path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("META-INF/MANIFEST.MF").ok()?;
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut entry, &mut contents).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Main-Class:") {
            let mc = rest.trim().to_string();
            if !mc.is_empty() {
                return Some(mc);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

pub fn clean(project_root: &Path) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    println!(
        "Cleaning {} v{}",
        desc.buildable_name(), desc.buildable_version()
    );

    let target_dir = project_root.join("target");

    if target_dir.exists() {
        std::fs::remove_dir_all(&target_dir).with_context(|| {
            format!("failed to remove {}", target_dir.display())
        })?;
        println!("  Target dir      removed");
    } else {
        println!("  Target dir      nothing to clean");
    }

    Ok(())
}

#[cfg(test)]
mod clean_tests {
    use super::*;

    /// Minimal valid `Curie.toml` content.  Used in multiple tests to satisfy
    /// `descriptor::load` without duplicating the literal in each test body.
    fn minimal_app_toml() -> &'static str {
        "[application]\nname = \"test\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
         [java]\nsourceCompatibility = \"21\"\n"
    }

    #[test]
    fn clean_removes_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("Curie.toml"), minimal_app_toml()).unwrap();

        let target = root.join("target");
        std::fs::create_dir_all(target.join("classes")).unwrap();
        std::fs::write(target.join("app.jar"), b"jar").unwrap();

        clean(root).unwrap();

        assert!(!root.join("target").exists());
    }

    #[test]
    fn clean_no_target_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("Curie.toml"), minimal_app_toml()).unwrap();

        // No target/ directory — should succeed without error.
        clean(root).unwrap();
    }
}
