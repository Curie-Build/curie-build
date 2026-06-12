//! Top-level build orchestrator: ties `compile`, `test`, `jar`, `main_class`,
//! and `docker` together for the `curie build` and `curie clean` commands.

use crate::compile::compile;
use crate::config;
use crate::descriptor;
use crate::docker;
use crate::git;
use crate::incremental::needs_repackage;
use crate::jar::{populate_libs_dir, write_deterministic_jar};
use crate::main_class::{detect_main_class, validate_main_class};
use crate::maven;
use crate::native;
use crate::test;
use anyhow::{Context, Result};
use curie_deps::repo::Repository;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone)]
pub struct BuildOptions {
    pub no_docker: bool,
    pub no_native: bool,
    pub offline: bool,
    pub coverage: bool,
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
    /// Fat/uber JAR path when `[fat-jar]` is enabled.  This is the single
    /// self-contained JAR used by downstream stages (run, docker, native)
    /// instead of the regular JAR + libs/.
    pub fat_jar: Option<PathBuf>,
}

/// Single-module entry point used by `curie build` outside a workspace.
/// Loads the descriptor, then defers to [`build_with_desc`] with an empty
/// extra-classpath.
pub fn build(project_root: &Path, opts: BuildOptions) -> Result<()> {
    let desc = descriptor::load(project_root)?;
    maven::sync_for_build(project_root, &desc, opts.offline)?;
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
    crate::parallel::emit(&crate::style::headline(
        "Building", desc.buildable_name(), desc.buildable_version(),
    ));

    if desc.is_bom() {
        return build_bom(project_root, desc);
    }

    // Library projects must not have a Dockerfile at the project root.
    if desc.is_library() && project_root.join("Dockerfile").exists() {
        anyhow::bail!(
            "library projects do not support Docker: remove the Dockerfile from the project root"
        );
    }

    let output = do_build(project_root, desc, opts, extra_cp)?;

    // When a fat JAR was produced, show that as the primary output.
    let display_jar = output.fat_jar.as_ref().unwrap_or(&output.jar);
    crate::parallel::emit(&crate::style::done(
        &display_jar
            .strip_prefix(project_root)
            .unwrap_or(display_jar)
            .display()
            .to_string(),
    ));

    // When a fat JAR exists, downstream stages (docker, native) use it
    // instead of the regular JAR + libs/, since it is self-contained.
    let effective_jar = output.fat_jar.as_ref().unwrap_or(&output.jar);
    let effective_deps: &[PathBuf] = if output.fat_jar.is_some() { &[] } else { &output.dep_jars };

    if !desc.is_library() && !opts.no_docker && descriptor::docker_enabled(project_root, desc) {
        docker::docker_build(project_root, desc, effective_jar, effective_deps)?;
    }

    if !desc.is_library() && !opts.no_native && descriptor::native_image_enabled(desc) {
        native::build_native(project_root, desc, effective_jar, effective_deps)?;
    }

    Ok(output)
}

/// The effective "default" repos (normally Maven Central) with any user
/// mirrors from `~/.curie/config.toml` applied.
pub fn central_repos() -> Vec<Repository> {
    let cfg = config::load_config().unwrap_or_default();
    config::apply_mirrors(curie_deps::repo::default_repositories(), &cfg.mirrors)
}

/// Named repositories from the descriptor with user mirrors applied.
/// All `[[repositories]]` entries are passed; the resolver activates only those
/// referenced by a dep's `repository = "id"` field.
pub fn extra_repos(desc: &descriptor::Descriptor) -> Vec<Repository> {
    let cfg = config::load_config().unwrap_or_default();
    let repos = desc
        .repositories
        .iter()
        .map(|r| Repository {
            id: r.id.clone(),
            name: r.display_name().to_string(),
            url: r.url.clone(),
        })
        .collect();
    config::apply_mirrors(repos, &cfg.mirrors)
}

/// Build a BOM project: generate the POM file into `target/` and return.
/// No compilation or test phases run; the output JAR path holds the POM path.
fn build_bom(project_root: &Path, desc: &descriptor::Descriptor) -> Result<BuildOutput> {
    let target = project_root.join("target");
    std::fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;

    let name = desc.buildable_name();
    let version = desc.buildable_version();
    let pom_path = target.join(format!("{}-{}.pom", name, version));

    crate::pom_writer::write_bom_pom(desc, &pom_path)?;

    crate::parallel::emit(&crate::style::done(
        &pom_path
            .strip_prefix(project_root)
            .unwrap_or(&pom_path)
            .display()
            .to_string(),
    ));

    Ok(BuildOutput {
        jar: pom_path,
        dep_jars: vec![],
        main_class: None,
        resources_dir: None,
        fat_jar: None,
    })
}

/// Phase 2: compile production sources, run tests, then package JAR.
pub fn do_build(
    project_root: &Path,
    desc: &descriptor::Descriptor,
    opts: BuildOptions,
    extra_cp: &[PathBuf],
) -> Result<BuildOutput> {
    let offline = opts.offline;
    let compiled = compile(project_root, desc, offline, extra_cp)?;

    // --- run tests before packaging ------------------------------------------
    test::run_tests(
        project_root,
        desc,
        &compiled.classes_dir,
        &compiled.dep_jars,
        &compiled.kotlin_stdlib_jars,
        &compiled.groovy_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        None,
        offline,
        opts.coverage || desc.test.coverage_enabled(),
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
                    crate::parallel::emit(&crate::style::info("Detected", &format!("mainClass = {}", detected)));
                    detected
                }
            };
            Some(mc)
        } else {
            None // library
        };

        crate::parallel::emit(&crate::style::active("Package", &compiled.jar_name));
        let manifest_dep_jars = manifest_dep_jars(desc, &compiled.dep_jars, &compiled.groovy_jars);
        write_deterministic_jar(
            &compiled.jar_path,
            &compiled.classes_dir,
            resources_dir,
            main_class.as_deref(),
            &manifest_dep_jars,
            build_info_content.as_deref(),
        )
        .context("failed to write JAR")?;

        main_class
    } else {
        crate::parallel::emit(&crate::style::up_to_date("Package"));
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
    // Merge Groovy stdlib into effective_dep_jars for libs/ and run-time classpath.
    let effective_dep_jars: Vec<std::path::PathBuf> = {
        let mut v = compiled.dep_jars;
        v.extend(compiled.groovy_jars);
        v
    };
    if !effective_dep_jars.is_empty() && desc.application().is_some()
        && !descriptor::fat_jar_enabled(desc)
    {
        let libs_dir = project_root.join("target").join("libs");
        populate_libs_dir(&libs_dir, &effective_dep_jars)
            .context("failed to populate target/libs/")?;
    }

    // --- fat/uber JAR (when [fat-jar] is enabled) ----------------------------
    let fat_jar_path = if descriptor::fat_jar_enabled(desc) {
        let fat_name = format!(
            "{}-{}-fat.jar",
            desc.buildable_name().replace(':', "-"),
            desc.buildable_version()
        );
        let fat_path = project_root.join("target").join(&fat_name);
        let toml_path = project_root.join("Curie.toml");

        // Filter deps according to [fat-jar].shadeAll + per-dep shade/relocations.
        let fat_dep_jars = crate::fat_jar::filter_fat_jar_deps(&effective_dep_jars, desc);

        // Compute active relocation rules: global + those declared on any
        // direct dep that will be shaded (per should_shade).
        let mut active_relocs: Vec<crate::descriptor::Relocation> =
            desc.fat_jar.relocations.clone();
        for (_coord, v) in &desc.dependencies {
            if v.should_shade(desc.fat_jar.shade_all) {
                active_relocs.extend(v.relocations().iter().cloned());
            }
        }

        // Overlap safety check for per-dep relocations (required by the feature).
        // For every relocation declared on a direct dep that is being shaded,
        // verify that its "from" package prefix does not appear in any *other*
        // JAR that will actually be bundled.
        crate::fat_jar::check_per_dep_relocation_overlap(desc, &fat_dep_jars)
            .context("fat-jar relocation overlap check failed")?;

        if crate::fat_jar::needs_rebuild(
            &fat_path,
            &compiled.classes_dir,
            compiled.resources_dir.as_deref(),
            &fat_dep_jars,
            &toml_path,
        ) {
            crate::parallel::emit(&crate::style::active("Fat JAR", &fat_name));
            crate::fat_jar::write_fat_jar(
                &fat_path,
                &compiled.classes_dir,
                compiled.resources_dir.as_deref(),
                resolved_main_class.as_deref(),
                &fat_dep_jars,
                build_info_content.as_deref(),
                &active_relocs,
            )
            .context("failed to write fat JAR")?;
        } else {
            crate::parallel::emit(&crate::style::up_to_date("Fat JAR"));
        }

        Some(fat_path)
    } else {
        None
    };

    Ok(BuildOutput {
        jar: compiled.jar_path,
        dep_jars: effective_dep_jars,
        main_class: resolved_main_class,
        resources_dir: compiled.resources_dir,
        fat_jar: fat_jar_path,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Dependency JARs to list in the main JAR's `Class-Path` manifest header.
///
/// Effective runtime deps = user deps + Groovy stdlib (when Groovy sources
/// present). Kotlin stdlib is NOT included because simple Kotlin programs
/// compile to bytecode that doesn't reference stdlib classes — Groovy always
/// does.
///
/// When `[fat-jar]` is enabled, the main JAR gets no Class-Path: deps are
/// bundled into the fat JAR instead, `target/libs/` is not populated, and the
/// generated pom.xml's maven-jar-plugin correspondingly omits
/// `<addClasspath>`/`<classpathPrefix>` (`maven.rs::build_jar_plugin`).
fn manifest_dep_jars(
    desc: &descriptor::Descriptor,
    dep_jars: &[PathBuf],
    groovy_jars: &[PathBuf],
) -> Vec<PathBuf> {
    if descriptor::fat_jar_enabled(desc) {
        Vec::new()
    } else {
        let mut deps = dep_jars.to_vec();
        deps.extend_from_slice(groovy_jars);
        deps
    }
}

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
    let target_dir = project_root.join("target");

    match std::fs::remove_dir_all(&target_dir) {
        Ok(()) => {
            crate::parallel::emit(&crate::style::clean_step("Target dir", "removed"));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::parallel::emit(&crate::style::neutral("Target dir", "nothing to clean"));
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to remove {}", target_dir.display()));
        }
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

#[cfg(test)]
mod manifest_dep_jars_tests {
    use super::*;

    fn load_desc(dir: &Path, toml: &str) -> descriptor::Descriptor {
        std::fs::write(dir.join("Curie.toml"), toml).unwrap();
        descriptor::load(dir).unwrap()
    }

    #[test]
    fn includes_deps_and_groovy_jars_when_no_fat_jar() {
        let dir = tempfile::tempdir().unwrap();
        let desc = load_desc(
            dir.path(),
            "[application]\nname = \"test\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nsourceCompatibility = \"21\"\n",
        );

        let dep_jars = vec![PathBuf::from("/m2/dep-1.0.jar")];
        let groovy_jars = vec![PathBuf::from("/m2/groovy-5.0.6.jar")];

        let result = manifest_dep_jars(&desc, &dep_jars, &groovy_jars);

        assert_eq!(result, vec![
            PathBuf::from("/m2/dep-1.0.jar"),
            PathBuf::from("/m2/groovy-5.0.6.jar"),
        ]);
    }

    #[test]
    fn empty_when_fat_jar_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let desc = load_desc(
            dir.path(),
            "[application]\nname = \"test\"\nversion = \"0.1.0\"\nmainClass = \"Main\"\n\
             [java]\nsourceCompatibility = \"21\"\n\
             [fat-jar]\nenabled = true\n",
        );

        let dep_jars = vec![PathBuf::from("/m2/dep-1.0.jar")];
        let groovy_jars = vec![PathBuf::from("/m2/groovy-5.0.6.jar")];

        let result = manifest_dep_jars(&desc, &dep_jars, &groovy_jars);

        assert!(result.is_empty(), "fat JAR's main JAR must have no Class-Path deps");
    }
}
