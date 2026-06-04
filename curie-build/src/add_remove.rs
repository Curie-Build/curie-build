//! `curie add` and `curie remove` — manage dependencies in `Curie.toml`.
//!
//! # `curie add`
//!
//! ```text
//! curie add <group:artifact>[@version] [--test] [--annotation-processor] [--bom]
//! ```
//!
//! Adds one entry to the appropriate section of `Curie.toml`:
//!
//! | flags                            | section                       |
//! |----------------------------------|-------------------------------|
//! | *(none)*                         | `[dependencies]`              |
//! | `--test`                         | `[test-dependencies]`         |
//! | `--annotation-processor`         | `[annotation-processors]`     |
//! | `--test --annotation-processor`  | `[test-annotation-processors]`|
//! | `--bom`                          | `[bom-imports]`               |
//! | `--test --bom`                   | `[test-bom-imports]`          |
//!
//! **Version selection** (in order of priority):
//!
//! 1. Explicit `@version` in the coordinate string — used as-is.
//! 2. Coordinate is managed by an already-declared BOM (checked by resolving
//!    the project's `[bom-imports]`) → writes `""` (BOM-managed).
//! 3. Otherwise: fetch `maven-metadata.xml` and pick the latest stable
//!    release using the same algorithm as `curie update`.
//!
//! BOM imports (`--bom`) always require an explicit `@version`.
//!
//! # `curie remove`
//!
//! ```text
//! curie remove <group:artifact> [--test] [--annotation-processor] [--bom]
//! ```
//!
//! Removes the entry from the appropriate section.  Errors when the entry
//! is not present.

use crate::build::{central_repos, extra_repos};
use crate::descriptor::{self, Descriptor};
use crate::update::{fetch_latest_stable, resolve_repo_url};
use anyhow::{bail, Context, Result};
use curie_deps::repo::Repository;
use std::path::Path;
use toml_edit::DocumentMut;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for `curie add`.
#[derive(Debug, Clone)]
pub struct AddOptions {
    /// Add to the test-scoped section instead of the production section.
    pub test: bool,
    /// Target `[annotation-processors]` / `[test-annotation-processors]`
    /// instead of `[dependencies]` / `[test-dependencies]`.
    pub annotation_processor: bool,
    /// Target `[bom-imports]` / `[test-bom-imports]`.
    pub bom: bool,
    /// Do not access the network; fail when version must be resolved.
    pub offline: bool,
    /// Force re-download of the local artifact index before opening the search UI.
    pub refresh_index: bool,
}

/// Options for `curie remove`.
#[derive(Debug, Clone)]
pub struct RemoveOptions {
    /// Remove from the test-scoped section.
    pub test: bool,
    /// Remove from `[annotation-processors]` / `[test-annotation-processors]`.
    pub annotation_processor: bool,
    /// Remove from `[bom-imports]` / `[test-bom-imports]`.
    pub bom: bool,
}

/// Add a dependency to `Curie.toml`.
///
/// `coord_arg` is `"group:artifact"` or `"group:artifact@version"`, or `None`
/// to open the interactive artifact search UI.
pub fn run_add(project_root: &Path, coord_arg: Option<&str>, opts: AddOptions) -> Result<()> {
    // -- resolve coordinate (interactive or explicit) ------------------------
    let coord_str: String = match coord_arg {
        Some(c) => c.to_string(),
        None => {
            let db = crate::search_index::ensure_index(opts.refresh_index, opts.offline)?;
            match crate::search_ui::run_search_ui(&db)? {
                Some(coord) => coord,
                None => return Ok(()), // user cancelled
            }
        }
    };
    let coord_arg = coord_str.as_str();

    // -- parse coordinate ----------------------------------------------------
    let (coord, explicit_version) = parse_coord_arg(coord_arg)?;

    // BOMs always need an explicit version.
    if opts.bom && explicit_version.is_none() {
        bail!(
            "BOM imports require an explicit version.\n\
             Use:  curie add --bom {}@<version>",
            coord
        );
    }

    let section = section_name(&opts);
    let desc = descriptor::load(project_root)?;

    // -- guard: already present ----------------------------------------------
    if coord_exists_in_section(&desc, section, &coord) {
        bail!(
            "\"{}\" is already present in [{}].\n\
             Run `curie update` to upgrade it.",
            coord, section
        );
    }

    // -- resolve version -----------------------------------------------------
    let version = match explicit_version {
        Some(v) => v,
        None => resolve_version(&coord, &desc, &opts)?,
    };

    // -- rewrite Curie.toml --------------------------------------------------
    rewrite_toml(project_root, |doc| {
        insert_entry(doc, section, &coord, &version);
        Ok(())
    })?;

    // -- report --------------------------------------------------------------
    if version.is_empty() {
        println!("  Added \"{}\" = \"\" (BOM-managed) to [{}]", coord, section);
    } else {
        println!("  Added \"{}\" = \"{}\" to [{}]", coord, version, section);
    }

    Ok(())
}

/// Remove a dependency from `Curie.toml`.
///
/// `coord_arg` is `"group:artifact"` (a trailing `@version` is tolerated but
/// ignored — only the coordinate is needed to identify the entry).
pub fn run_remove(project_root: &Path, coord_arg: &str, opts: RemoveOptions) -> Result<()> {
    // strip optional @version so callers can pass the same string to both commands
    let (coord, _) = parse_coord_arg(coord_arg)?;
    let section = section_name_remove(&opts);

    // -- rewrite Curie.toml --------------------------------------------------
    rewrite_toml(project_root, |doc| {
        // Verify entry exists before mutating.
        let entry_present = doc
            .get(section)
            .and_then(|v| v.as_table())
            .map(|t| t.contains_key(&coord))
            .unwrap_or(false);

        if !entry_present {
            anyhow::bail!("\"{}\" not found in [{}]", coord, section);
        }

        doc.get_mut(section)
            .and_then(|v| v.as_table_mut())
            .expect("table existence checked above")
            .remove(&coord);
        Ok(())
    })?;

    println!("  Removed \"{}\" from [{}]", coord, section);
    Ok(())
}

// ---------------------------------------------------------------------------
// TOML rewrite helper
// ---------------------------------------------------------------------------

/// Read `Curie.toml`, parse it, call `f` to mutate the document, then write
/// it back.  All three I/O operations carry the file path in their error
/// context.  `f` may return an error to abort the write.
fn rewrite_toml(project_root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let toml_path = project_root.join("Curie.toml");
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("failed to read {}", toml_path.display()))?;
    let mut doc: DocumentMut = content
        .parse()
        .with_context(|| format!("failed to parse {} as TOML", toml_path.display()))?;
    f(&mut doc)?;
    std::fs::write(&toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", toml_path.display()))
}

// ---------------------------------------------------------------------------
// Section routing
// ---------------------------------------------------------------------------

/// Map the three section-routing flags to the Curie.toml table name.
fn section_for(bom: bool, annotation_processor: bool, test: bool) -> &'static str {
    match (bom, annotation_processor, test) {
        (true,  _,     false) => "bom-imports",
        (true,  _,     true)  => "test-bom-imports",
        (false, true,  false) => "annotation-processors",
        (false, true,  true)  => "test-annotation-processors",
        (false, false, false) => "dependencies",
        (false, false, true)  => "test-dependencies",
    }
}

fn section_name(opts: &AddOptions) -> &'static str {
    section_for(opts.bom, opts.annotation_processor, opts.test)
}

fn section_name_remove(opts: &RemoveOptions) -> &'static str {
    section_for(opts.bom, opts.annotation_processor, opts.test)
}

// ---------------------------------------------------------------------------
// Coordinate parsing
// ---------------------------------------------------------------------------

/// Parse `"group:artifact"` or `"group:artifact@version"`.
///
/// Returns `(coord, Option<version>)`.  Errors when the coordinate does not
/// contain exactly one `:` (before the `@`).
fn parse_coord_arg(arg: &str) -> Result<(String, Option<String>)> {
    let (coord_part, version_part) = match arg.split_once('@') {
        Some((c, v)) => (c, Some(v.to_string())),
        None => (arg, None),
    };

    if coord_part.split(':').count() != 2 {
        bail!(
            "invalid coordinate \"{}\": expected \"group:artifact\" or \
             \"group:artifact@version\"",
            arg
        );
    }
    // Validate neither part is empty
    let mut parts = coord_part.splitn(2, ':');
    let group = parts.next().unwrap_or("").trim();
    let artifact = parts.next().unwrap_or("").trim();
    if group.is_empty() || artifact.is_empty() {
        bail!("invalid coordinate \"{}\": group and artifact must be non-empty", arg);
    }

    Ok((coord_part.to_string(), version_part))
}

// ---------------------------------------------------------------------------
// Guard: duplicate detection against the parsed descriptor
// ---------------------------------------------------------------------------

fn coord_exists_in_section(desc: &Descriptor, section: &str, coord: &str) -> bool {
    match section {
        "dependencies"              => desc.dependencies.contains_key(coord),
        "test-dependencies"         => desc.test_dependencies.contains_key(coord),
        "bom-imports"               => desc.bom_imports.contains_key(coord),
        "test-bom-imports"          => desc.test_bom_imports.contains_key(coord),
        "annotation-processors"     => desc.annotation_processors.contains_key(coord),
        "test-annotation-processors" => desc.test_annotation_processors.contains_key(coord),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Version resolution
// ---------------------------------------------------------------------------

/// Determine the version string to write when none was given on the CLI.
///
/// 1. Check whether the coord is managed by an already-declared BOM.
///    If so, return `""` (BOM-managed shorthand).
/// 2. Otherwise fetch the latest stable release from Maven metadata.
fn resolve_version(
    coord: &str,
    desc: &Descriptor,
    opts: &AddOptions,
) -> Result<String> {
    // Step 1: BOM-managed check (cheap path — no network needed when BOM
    // itself is already cached locally; we accept the network cost of BOM
    // resolution because it matches what the build itself would do).
    if let Some(managed) = bom_managed_version(coord, desc, opts) {
        println!(
            "  {} is managed by a BOM ({}); writing empty version",
            coord, managed
        );
        return Ok(String::new());
    }

    // Step 2: network fetch of maven-metadata.xml
    if opts.offline {
        bail!(
            "\"{}\" is not managed by any declared BOM and no version was \
             specified.\nRe-run without --offline or pass an explicit version:\n  \
             curie add {}@<version>",
            coord, coord
        );
    }

    let default_repos = central_repos();
    let named_repos = extra_repos(desc);
    let repo_url = resolve_repo_url(&None, &named_repos, &default_repos);

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-add/0.1")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    match fetch_latest_stable(&client, &repo_url, coord) {
        Some(v) => Ok(v),
        None => bail!(
            "could not find any stable release of \"{}\" in {}.\n\
             Specify a version explicitly:  curie add {}@<version>",
            coord, repo_url, coord
        ),
    }
}

/// Look up `coord` in the BOM-managed version map for the project.
///
/// Uses the *same* BOM resolution the build itself would use:
/// - For production deps/annotation-processors/bom-imports: `prod_bom_gavs()`.
/// - For test-scoped: `test_bom_gavs()` (superset of prod).
///
/// Returns `Some(managed_version)` when the coord is found, `None` otherwise.
///
/// This is intentionally a *best-effort* check: if BOM resolution fails
/// (network error, malformed POM), we fall through to the Maven-metadata
/// fetch rather than aborting the `add` command.
fn bom_managed_version(coord: &str, desc: &Descriptor, opts: &AddOptions) -> Option<String> {
    use curie_deps::resolver::resolve_boms;

    let bom_gavs = if opts.test {
        desc.test_bom_gavs().ok()?
    } else {
        desc.prod_bom_gavs().ok()?
    };

    if bom_gavs.is_empty() {
        return None;
    }

    let default_repos = central_repos();
    let named_repos = extra_repos(desc);
    let all_repos: Vec<Repository> = default_repos
        .into_iter()
        .chain(named_repos)
        .collect();

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-add/0.1")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    let managed = resolve_boms(&bom_gavs, &all_repos, &client, false).ok()?;
    managed.get(coord).cloned()
}

// ---------------------------------------------------------------------------
// TOML insert
// ---------------------------------------------------------------------------

/// Insert `"coord" = "version"` into `section`, creating the table when absent.
///
/// The new entry is always appended at the end of the section so it doesn't
/// disrupt existing comment/whitespace layout.
fn insert_entry(doc: &mut DocumentMut, section: &str, coord: &str, version: &str) {
    // Ensure the section table exists.
    if doc.get(section).is_none() {
        let mut tbl = toml_edit::Table::new();
        tbl.set_implicit(false);
        doc.insert(section, toml_edit::Item::Table(tbl));
    }

    let table = doc
        .get_mut(section)
        .and_then(|v| v.as_table_mut())
        .expect("section was just created");

    table.insert(coord, toml_edit::value(version));
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Write a minimal `Curie.toml` in a temp dir and return the dir handle.
    fn make_project(content: &str) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Curie.toml"), content).unwrap();
        dir
    }

    fn read_toml(dir: &TempDir) -> String {
        fs::read_to_string(dir.path().join("Curie.toml")).unwrap()
    }

    // -----------------------------------------------------------------------
    // parse_coord_arg
    // -----------------------------------------------------------------------

    #[test]
    fn parse_coord_without_version() {
        let (coord, ver) = parse_coord_arg("com.example:foo").unwrap();
        assert_eq!(coord, "com.example:foo");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_coord_with_version() {
        let (coord, ver) = parse_coord_arg("com.example:foo@1.2.3").unwrap();
        assert_eq!(coord, "com.example:foo");
        assert_eq!(ver.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn parse_coord_missing_colon_errors() {
        assert!(parse_coord_arg("invalid").is_err());
    }

    #[test]
    fn parse_coord_empty_group_errors() {
        assert!(parse_coord_arg(":artifact").is_err());
    }

    #[test]
    fn parse_coord_empty_artifact_errors() {
        assert!(parse_coord_arg("group:").is_err());
    }

    // -----------------------------------------------------------------------
    // section routing
    // -----------------------------------------------------------------------

    #[test]
    fn section_defaults_to_dependencies() {
        let opts = AddOptions { test: false, annotation_processor: false, bom: false, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "dependencies");
    }

    #[test]
    fn section_test_flag() {
        let opts = AddOptions { test: true, annotation_processor: false, bom: false, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "test-dependencies");
    }

    #[test]
    fn section_annotation_processor() {
        let opts = AddOptions { test: false, annotation_processor: true, bom: false, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "annotation-processors");
    }

    #[test]
    fn section_test_annotation_processor() {
        let opts = AddOptions { test: true, annotation_processor: true, bom: false, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "test-annotation-processors");
    }

    #[test]
    fn section_bom() {
        let opts = AddOptions { test: false, annotation_processor: false, bom: true, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "bom-imports");
    }

    #[test]
    fn section_test_bom() {
        let opts = AddOptions { test: true, annotation_processor: false, bom: true, offline: false, refresh_index: false };
        assert_eq!(section_name(&opts), "test-bom-imports");
    }

    // -----------------------------------------------------------------------
    // insert_entry
    // -----------------------------------------------------------------------

    #[test]
    fn insert_entry_creates_section_if_absent() {
        let toml = "[application]\nname = \"demo\"\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        insert_entry(&mut doc, "dependencies", "com.example:foo", "1.0.0");
        let out = doc.to_string();
        assert!(out.contains("[dependencies]"), "got: {}", out);
        assert!(out.contains("\"com.example:foo\" = \"1.0.0\""), "got: {}", out);
    }

    #[test]
    fn insert_entry_appends_to_existing_section() {
        let toml = "[dependencies]\n\"org.existing:lib\" = \"2.0\"\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        insert_entry(&mut doc, "dependencies", "com.example:foo", "1.5.0");
        let out = doc.to_string();
        assert!(out.contains("\"org.existing:lib\" = \"2.0\""), "got: {}", out);
        assert!(out.contains("\"com.example:foo\" = \"1.5.0\""), "got: {}", out);
    }

    #[test]
    fn insert_entry_preserves_comments() {
        let toml = "# header\n[dependencies]\n# keep this\n\"x:y\" = \"1.0\"\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        insert_entry(&mut doc, "dependencies", "a:b", "2.0");
        let out = doc.to_string();
        assert!(out.contains("# header"), "got: {}", out);
        assert!(out.contains("# keep this"), "got: {}", out);
    }

    // -----------------------------------------------------------------------
    // run_add — file-level integration (no network)
    // -----------------------------------------------------------------------

    fn minimal_app_toml() -> &'static str {
        "[application]\nname = \"demo\"\nversion = \"0.1.0\"\n"
    }

    #[test]
    fn add_with_explicit_version_inserts_entry() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("com.google.guava:guava@33.0.0-jre"),
            AddOptions { test: false, annotation_processor: false, bom: false, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[dependencies]"), "got: {}", out);
        assert!(out.contains("\"com.google.guava:guava\" = \"33.0.0-jre\""), "got: {}", out);
    }

    #[test]
    fn add_to_test_section_with_explicit_version() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("org.junit.jupiter:junit-jupiter@5.10.0"),
            AddOptions { test: true, annotation_processor: false, bom: false, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[test-dependencies]"), "got: {}", out);
        assert!(out.contains("\"org.junit.jupiter:junit-jupiter\" = \"5.10.0\""), "got: {}", out);
        assert!(!out.contains("[dependencies]"), "should not have prod deps section, got: {}", out);
    }

    #[test]
    fn add_annotation_processor_with_explicit_version() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("org.projectlombok:lombok@1.18.30"),
            AddOptions { test: false, annotation_processor: true, bom: false, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[annotation-processors]"), "got: {}", out);
        assert!(out.contains("\"org.projectlombok:lombok\" = \"1.18.30\""), "got: {}", out);
    }

    #[test]
    fn add_test_annotation_processor_with_explicit_version() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("com.example:proc@2.0.0"),
            AddOptions { test: true, annotation_processor: true, bom: false, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[test-annotation-processors]"), "got: {}", out);
        assert!(out.contains("\"com.example:proc\" = \"2.0.0\""), "got: {}", out);
    }

    #[test]
    fn add_bom_with_explicit_version() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("org.springframework.boot:spring-boot-dependencies@3.2.0"),
            AddOptions { test: false, annotation_processor: false, bom: true, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[bom-imports]"), "got: {}", out);
        assert!(
            out.contains("\"org.springframework.boot:spring-boot-dependencies\" = \"3.2.0\""),
            "got: {}", out
        );
    }

    #[test]
    fn add_test_bom_with_explicit_version() {
        let dir = make_project(minimal_app_toml());
        run_add(
            dir.path(),
            Some("com.example:test-bom@1.0.0"),
            AddOptions { test: true, annotation_processor: false, bom: true, offline: false, refresh_index: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("[test-bom-imports]"), "got: {}", out);
    }

    #[test]
    fn add_bom_without_version_errors() {
        let dir = make_project(minimal_app_toml());
        let result = run_add(
            dir.path(),
            Some("org.springframework.boot:spring-boot-dependencies"),
            AddOptions { test: false, annotation_processor: false, bom: true, offline: false, refresh_index: false },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("require an explicit version"));
    }

    #[test]
    fn add_duplicate_coord_errors() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [dependencies]\n\"com.example:foo\" = \"1.0.0\"\n";
        let dir = make_project(toml);
        let result = run_add(
            dir.path(),
            Some("com.example:foo@2.0.0"),
            AddOptions { test: false, annotation_processor: false, bom: false, offline: false, refresh_index: false },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already present"));
    }

    #[test]
    fn add_without_version_offline_errors() {
        let dir = make_project(minimal_app_toml());
        let result = run_add(
            dir.path(),
            Some("com.example:foo"),
            AddOptions { test: false, annotation_processor: false, bom: false, offline: true, refresh_index: false },
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("offline") || msg.contains("--offline"), "got: {}", msg);
    }

    #[test]
    fn add_with_bom_managed_dep_writes_empty_version() {
        // We test the *TOML-rewrite* side only (no network): simulate the
        // BOM-managed path by calling insert_entry with an empty version.
        let dir = make_project(minimal_app_toml());
        let toml_path = dir.path().join("Curie.toml");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let mut doc: DocumentMut = content.parse().unwrap();
        insert_entry(&mut doc, "dependencies", "com.fasterxml.jackson.core:jackson-databind", "");
        std::fs::write(&toml_path, doc.to_string()).unwrap();

        let out = read_toml(&dir);
        assert!(
            out.contains("\"com.fasterxml.jackson.core:jackson-databind\" = \"\""),
            "got: {}", out
        );
    }

    // -----------------------------------------------------------------------
    // run_remove
    // -----------------------------------------------------------------------

    #[test]
    fn remove_existing_dep_succeeds() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [dependencies]\n\"com.example:foo\" = \"1.0.0\"\n\
                    \"com.example:bar\" = \"2.0.0\"\n";
        let dir = make_project(toml);
        run_remove(
            dir.path(),
            "com.example:foo",
            RemoveOptions { test: false, annotation_processor: false, bom: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(!out.contains("\"com.example:foo\""), "got: {}", out);
        assert!(out.contains("\"com.example:bar\""), "got: {}", out);
    }

    #[test]
    fn remove_absent_dep_errors() {
        let dir = make_project(minimal_app_toml());
        let result = run_remove(
            dir.path(),
            "com.example:nonexistent",
            RemoveOptions { test: false, annotation_processor: false, bom: false },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn remove_from_test_section() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [test-dependencies]\n\"org.junit.jupiter:junit-jupiter\" = \"5.10.0\"\n";
        let dir = make_project(toml);
        run_remove(
            dir.path(),
            "org.junit.jupiter:junit-jupiter",
            RemoveOptions { test: true, annotation_processor: false, bom: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(!out.contains("junit-jupiter"), "got: {}", out);
    }

    #[test]
    fn remove_from_bom_imports() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [bom-imports]\n\"org.springframework.boot:spring-boot-dependencies\" = \"3.2.0\"\n";
        let dir = make_project(toml);
        run_remove(
            dir.path(),
            "org.springframework.boot:spring-boot-dependencies",
            RemoveOptions { test: false, annotation_processor: false, bom: true },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(!out.contains("spring-boot-dependencies"), "got: {}", out);
    }

    #[test]
    fn remove_from_annotation_processors() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [annotation-processors]\n\"org.projectlombok:lombok\" = \"1.18.30\"\n";
        let dir = make_project(toml);
        run_remove(
            dir.path(),
            "org.projectlombok:lombok",
            RemoveOptions { test: false, annotation_processor: true, bom: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(!out.contains("lombok"), "got: {}", out);
    }

    #[test]
    fn remove_wrong_section_errors() {
        // coord present in [dependencies] but user passes --test
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [dependencies]\n\"com.example:foo\" = \"1.0.0\"\n";
        let dir = make_project(toml);
        let result = run_remove(
            dir.path(),
            "com.example:foo",
            RemoveOptions { test: true, annotation_processor: false, bom: false },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn remove_ignores_version_suffix_in_coord() {
        let toml = "[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [dependencies]\n\"com.example:foo\" = \"1.0.0\"\n";
        let dir = make_project(toml);
        // Passing com.example:foo@1.0.0 should strip the @1.0.0 part
        run_remove(
            dir.path(),
            "com.example:foo@1.0.0",
            RemoveOptions { test: false, annotation_processor: false, bom: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(!out.contains("\"com.example:foo\""), "got: {}", out);
    }

    #[test]
    fn remove_preserves_sibling_entries_and_comments() {
        let toml = "# top\n[application]\nname=\"demo\"\nversion=\"0.1.0\"\n\
                    [dependencies]\n# keep\n\"a:b\" = \"1.0\"\n\"c:d\" = \"2.0\"\n";
        let dir = make_project(toml);
        run_remove(
            dir.path(),
            "c:d",
            RemoveOptions { test: false, annotation_processor: false, bom: false },
        ).unwrap();
        let out = read_toml(&dir);
        assert!(out.contains("# top"), "got: {}", out);
        assert!(out.contains("# keep"), "got: {}", out);
        assert!(out.contains("\"a:b\""), "got: {}", out);
        assert!(!out.contains("\"c:d\""), "got: {}", out);
    }
}
