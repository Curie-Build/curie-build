//! `curie fetch` — pre-download dependency artifacts into `~/.m2/repository`.
//!
//! With no arguments, fetches everything declared in `Curie.toml`:
//! `[dependencies]`, `[test-dependencies]`, `[annotation-processors]`, and
//! `[test-annotation-processors]`.  With a `"group:artifact:version"`
//! coordinate, fetches that artifact and (unless `--no-transitive`) its
//! transitive closure.

use crate::build::{central_repos, extra_repos};
use crate::{descriptor, workspace};
use anyhow::{bail, Context, Result};
use curie_deps::repo::Repository;
use curie_deps::{DepEntry, Gav, ResolveOptions};
use std::path::Path;

/// Entry point when called from a workspace member context.
pub fn run_fetch_workspace_member(
    workspace_root: &Path,
    member_index: usize,
    coord: Option<&str>,
    no_transitive: bool,
    offline: bool,
) -> Result<()> {
    let ws = workspace::load(workspace_root)?;
    let member = &ws.members[member_index];
    run_fetch_with_desc(&member.descriptor, coord, no_transitive, offline)
}

/// Entry point for standalone (non-workspace) projects.
pub fn run_fetch(project_root: &Path, coord: Option<&str>, no_transitive: bool, offline: bool) -> Result<()> {
    let desc = descriptor::load(project_root)?;
    if desc.is_workspace() {
        bail!("`curie fetch` cannot run on a workspace root; target a member with --project");
    }
    run_fetch_with_desc(&desc, coord, no_transitive, offline)
}

fn run_fetch_with_desc(
    desc: &descriptor::Descriptor,
    coord: Option<&str>,
    no_transitive: bool,
    offline: bool,
) -> Result<()> {
    match coord {
        Some(c) => fetch_coordinate(desc, c, no_transitive, offline),
        None if no_transitive => bail!("--no-transitive requires a coordinate argument"),
        None => fetch_project_dependencies(desc, offline),
    }
}

// ---------------------------------------------------------------------------
// Coordinate parsing
// ---------------------------------------------------------------------------

/// Whether to download a JAR (+ POM) or only the POM for an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ArtifactType {
    /// Normal artifact: download POM + JAR.
    Jar,
    /// POM-only artifact (BOM, parent POM): download the `.pom` file only.
    Pom,
}

/// Parse a coordinate with an optional type and classifier suffix:
/// - `"group:artifact:version"` → [`ArtifactType::Jar`], no classifier
/// - `"group:artifact:version:type"` → type is `jar` or `pom`
/// - `"group:artifact:version:type:classifier"` → JAR with classifier (e.g. `jar:runtime`)
fn parse_artifact_coord(arg: &str) -> Result<(Gav, ArtifactType)> {
    let parts: Vec<&str> = arg.split(':').collect();
    match parts.len() {
        3 => {
            let gav = Gav::from_key_version(&format!("{}:{}", parts[0], parts[1]), parts[2])?;
            Ok((gav, ArtifactType::Jar))
        }
        4 => {
            let artifact_type = match parts[3] {
                "jar" => ArtifactType::Jar,
                "pom" => ArtifactType::Pom,
                other => bail!(
                    "unsupported artifact type {:?} in {:?}; expected \"jar\" or \"pom\"",
                    other,
                    arg
                ),
            };
            let gav = Gav::from_key_version(&format!("{}:{}", parts[0], parts[1]), parts[2])?;
            Ok((gav, artifact_type))
        }
        5 => {
            let artifact_type = match parts[3] {
                "jar" => ArtifactType::Jar,
                other => bail!(
                    "unsupported artifact type {:?} in {:?}; classifiers are only supported for \"jar\"",
                    other,
                    arg
                ),
            };
            let mut gav = Gav::from_key_version(&format!("{}:{}", parts[0], parts[1]), parts[2])?;
            gav.classifier = Some(parts[4].to_string());
            Ok((gav, artifact_type))
        }
        _ => bail!(
            "invalid coordinate {:?}: expected \"group:artifact:version\", \
             \"group:artifact:version:type\", or \"group:artifact:version:type:classifier\"",
            arg
        ),
    }
}

/// Parse a `"group:artifact:version"` coordinate (3-part shorthand, tests only).
#[cfg(test)]
fn parse_gav_arg(arg: &str) -> Result<Gav> {
    let (gav, _) = parse_artifact_coord(arg)?;
    Ok(gav)
}

// ---------------------------------------------------------------------------
// `curie fetch <group:artifact:version> [--no-transitive]`
// ---------------------------------------------------------------------------

fn fetch_coordinate(desc: &descriptor::Descriptor, coord: &str, no_transitive: bool, offline: bool) -> Result<()> {
    let (gav, artifact_type) = parse_artifact_coord(coord)?;

    if artifact_type == ArtifactType::Pom {
        let mut repos = central_repos();
        repos.extend(extra_repos(desc));
        let pom = curie_deps::fetch_pom_only(&gav, &repos, offline)?;
        crate::parallel::emit(&crate::style::done(&pom.display().to_string()));
        return Ok(());
    }

    if no_transitive {
        let mut repos = central_repos();
        repos.extend(extra_repos(desc));
        let jar = curie_deps::fetch_artifact(&gav, &repos, offline)?;
        crate::parallel::emit(&crate::style::done(&jar.display().to_string()));
        return Ok(());
    }

    let key = format!("{}:{}", gav.group, gav.artifact);
    let entries = [DepEntry { key: &key, version: &gav.version, repo_id: None, exclusions: vec![], classifier: None }];
    let opts = ResolveOptions {
        default_repos: central_repos(),
        named_repos: extra_repos(desc),
        progress: true,
        bom_imports: desc.prod_bom_gavs()?,
        offline,
        skip_version_ranges: false,
    };
    let jars = curie_deps::resolve(&entries, &opts)?;
    crate::parallel::emit(&crate::style::done(&format!(
        "{} JAR(s) cached for {}",
        jars.len(),
        gav.notation()
    )));
    Ok(())
}

// ---------------------------------------------------------------------------
// `curie fetch` (no arguments)
// ---------------------------------------------------------------------------

fn fetch_project_dependencies(desc: &descriptor::Descriptor, offline: bool) -> Result<()> {
    let prod = fetch_dep_section(desc, false, offline)?;
    let test = fetch_dep_section(desc, true, offline)?;
    let total = prod + test;
    if total == 0 {
        crate::parallel::emit(&crate::style::neutral("Fetch", "no dependencies declared"));
    } else {
        crate::parallel::emit(&crate::style::done(&format!("{total} JAR(s) cached")));
    }
    Ok(())
}

/// Resolve and download one scope's dependencies (`[dependencies]` +
/// `[annotation-processors]`, or `[test-dependencies]` +
/// `[test-annotation-processors]` plus the production annotation processors,
/// which test compilation also needs on its processor path).  Returns the
/// number of JARs in the resolved closure, or `0` without contacting the
/// resolver if nothing is declared.
fn fetch_dep_section(desc: &descriptor::Descriptor, tests: bool, offline: bool) -> Result<usize> {
    let dep_map = if tests { &desc.test_dependencies } else { &desc.dependencies };
    let mut entries: Vec<DepEntry> = dep_map
        .iter()
        .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository(), exclusions: v.exclusions(), classifier: None })
        .collect();

    let mut ap_pairs = desc.ap_pairs();
    if tests {
        ap_pairs.extend(desc.test_ap_pairs());
    }
    entries.extend(
        ap_pairs
            .into_iter()
            .map(|(k, v)| DepEntry { key: k, version: v, repo_id: None, exclusions: vec![], classifier: None }),
    );

    if entries.is_empty() {
        return Ok(0);
    }

    let bom_gavs = if tests { desc.test_bom_gavs()? } else { desc.prod_bom_gavs()? };
    let opts = ResolveOptions {
        default_repos: central_repos(),
        named_repos: extra_repos(desc),
        progress: true,
        bom_imports: bom_gavs,
        offline,
        skip_version_ranges: false,
    };
    let jars = curie_deps::resolve(&entries, &opts)?;
    let label = if tests { "Test deps" } else { "Dependencies" };
    crate::parallel::emit(&crate::style::resolve(label, &format!("{} JAR(s)", jars.len())));
    Ok(jars.len())
}

// ---------------------------------------------------------------------------
// `curie fetch --file <path>`
// ---------------------------------------------------------------------------

/// Read a coordinate file and return the non-empty, non-comment lines.
/// Blank lines and lines whose first non-whitespace character is `#` are
/// skipped.  The returned strings are trimmed.
fn read_coordinate_file(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read coordinate file {}", path.display()))?;
    let lines = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    Ok(lines)
}

/// Parse a slice of coordinate strings into `(Gav, ArtifactType)` pairs,
/// reporting the offending line on error.
fn parse_coordinate_lines(lines: &[String]) -> Result<Vec<(Gav, ArtifactType)>> {
    lines
        .iter()
        .map(|line| {
            parse_artifact_coord(line).with_context(|| format!("invalid coordinate {:?}", line))
        })
        .collect()
}

/// Return the repository list for file mode: always Maven Central + global
/// mirrors; also appends the project's `[[repositories]]` when the current
/// directory (or `--project`) contains a valid non-workspace `Curie.toml`.
fn repos_for_file_mode(project_root: &Path) -> Vec<Repository> {
    let mut repos = central_repos();
    if let Ok(desc) = descriptor::load(project_root) {
        if !desc.is_workspace() {
            repos.extend(extra_repos(&desc));
        }
    }
    repos
}

/// Entry point for `curie fetch --file <path>`.
pub fn run_fetch_file(project_root: &Path, path: &Path, no_transitive: bool, offline: bool) -> Result<()> {
    let lines = read_coordinate_file(path)?;
    let coords = parse_coordinate_lines(&lines)?;

    if coords.is_empty() {
        crate::parallel::emit(&crate::style::neutral("Fetch", "no coordinates in file"));
        return Ok(());
    }

    let repos = repos_for_file_mode(project_root);

    // POM-only artifacts (BOMs, parent POMs) — always fetched individually,
    // regardless of --no-transitive (there is no JAR to resolve transitively).
    let pom_count = fetch_pom_coords(coords.iter().filter(|(_, t)| *t == ArtifactType::Pom).map(|(g, _)| g), &repos, offline)?;

    // JAR artifacts — either flat (--no-transitive) or a single shared resolve pass.
    let jar_gavs: Vec<&Gav> = coords.iter().filter(|(_, t)| *t == ArtifactType::Jar).map(|(g, _)| g).collect();
    let jar_count = if jar_gavs.is_empty() {
        0
    } else if no_transitive {
        fetch_jar_coords_flat(&jar_gavs, &repos, offline)?
    } else {
        fetch_jar_coords_transitive(&jar_gavs, &repos, offline)?
    };

    let total = pom_count + jar_count;
    crate::parallel::emit(&crate::style::done(&format!("{} artifact(s) cached", total)));
    Ok(())
}

fn fetch_pom_coords<'a>(
    gavs: impl Iterator<Item = &'a Gav>,
    repos: &[curie_deps::repo::Repository],
    offline: bool,
) -> Result<usize> {
    let mut count = 0;
    for gav in gavs {
        curie_deps::fetch_pom_only(gav, repos, offline)?;
        count += 1;
    }
    Ok(count)
}

fn fetch_jar_coords_flat(gavs: &[&Gav], repos: &[curie_deps::repo::Repository], offline: bool) -> Result<usize> {
    let mut count = 0;
    for gav in gavs {
        curie_deps::fetch_artifact(gav, repos, offline)?;
        count += 1;
    }
    Ok(count)
}

fn fetch_jar_coords_transitive(gavs: &[&Gav], repos: &[curie_deps::repo::Repository], offline: bool) -> Result<usize> {
    // Resolve each coordinate individually so that multiple versions of the
    // same group:artifact (e.g. maven-compiler-plugin:3.13.0 and :3.15.0)
    // are both fetched.  The resolver deduplicates by group:artifact key within
    // a single batch, which would silently drop the second version.
    let mut total = 0;
    for gav in gavs {
        let key = format!("{}:{}", gav.group, gav.artifact);
        let classifier = gav.classifier.as_deref();
        let entry = DepEntry { key: &key, version: &gav.version, repo_id: None, exclusions: vec![], classifier };
        let opts = ResolveOptions {
            default_repos: repos.to_vec(),
            named_repos: vec![],
            progress: true,
            bom_imports: vec![],
            offline,
            skip_version_ranges: true,
        };
        let jars = curie_deps::resolve(&[entry], &opts)?;
        total += jars.len();
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write a minimal `Curie.toml` with no dependencies and load its descriptor.
    fn empty_app_descriptor() -> descriptor::Descriptor {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        descriptor::load(dir.path()).unwrap()
    }

    // -----------------------------------------------------------------------
    // parse_gav_arg
    // -----------------------------------------------------------------------

    #[test]
    fn parse_gav_arg_accepts_three_parts() {
        let gav = parse_gav_arg("com.google.guava:guava:33.2.0-jre").unwrap();
        assert_eq!(gav.group, "com.google.guava");
        assert_eq!(gav.artifact, "guava");
        assert_eq!(gav.version, "33.2.0-jre");
    }

    #[test]
    fn parse_gav_arg_rejects_two_parts() {
        assert!(parse_gav_arg("com.google.guava:guava").is_err());
    }

    #[test]
    fn parse_gav_arg_rejects_four_parts() {
        assert!(parse_gav_arg("a:b:c:d").is_err());
    }

    #[test]
    fn parse_gav_arg_rejects_empty_group() {
        assert!(parse_gav_arg(":artifact:1.0").is_err());
    }

    // -----------------------------------------------------------------------
    // run_fetch_with_desc — no network required
    // -----------------------------------------------------------------------

    #[test]
    fn no_transitive_without_coord_is_an_error() {
        let desc = empty_app_descriptor();
        let err = run_fetch_with_desc(&desc, None, true, true).unwrap_err();
        assert!(err.to_string().contains("--no-transitive"));
    }

    #[test]
    fn empty_project_fetch_succeeds_without_network() {
        let desc = empty_app_descriptor();
        run_fetch_with_desc(&desc, None, false, true).unwrap();
    }

    // -----------------------------------------------------------------------
    // read_coordinate_file
    // -----------------------------------------------------------------------

    fn write_coord_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deps.txt");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn read_coordinate_file_skips_blank_lines_and_comments() {
        let (_dir, path) = write_coord_file(
            "# comment\n\ncom.example:foo:1.0\n   \n# another\ncom.example:bar:2.0\n",
        );
        let lines = read_coordinate_file(&path).unwrap();
        assert_eq!(lines, vec!["com.example:foo:1.0", "com.example:bar:2.0"]);
    }

    #[test]
    fn read_coordinate_file_skips_indented_comment() {
        let (_dir, path) = write_coord_file("   # indented comment\ncom.example:foo:1.0\n");
        let lines = read_coordinate_file(&path).unwrap();
        assert_eq!(lines, vec!["com.example:foo:1.0"]);
    }

    #[test]
    fn read_coordinate_file_missing_path_errors_with_path() {
        let err = read_coordinate_file(std::path::Path::new("/no/such/file.txt")).unwrap_err();
        assert!(err.to_string().contains("/no/such/file.txt"));
    }

    // -----------------------------------------------------------------------
    // parse_coordinate_lines
    // -----------------------------------------------------------------------

    #[test]
    fn parse_coordinate_lines_accepts_jar_coord() {
        let lines = vec!["com.google.guava:guava:33.2.0-jre".to_owned()];
        let coords = parse_coordinate_lines(&lines).unwrap();
        let (gav, kind) = &coords[0];
        assert_eq!(gav.group, "com.google.guava");
        assert_eq!(gav.artifact, "guava");
        assert_eq!(gav.version, "33.2.0-jre");
        assert_eq!(*kind, ArtifactType::Jar);
    }

    #[test]
    fn parse_coordinate_lines_accepts_pom_type() {
        let lines = vec!["com.google.guava:guava-bom:33.4.8-jre:pom".to_owned()];
        let coords = parse_coordinate_lines(&lines).unwrap();
        let (gav, kind) = &coords[0];
        assert_eq!(gav.artifact, "guava-bom");
        assert_eq!(*kind, ArtifactType::Pom);
    }

    #[test]
    fn parse_coordinate_lines_accepts_explicit_jar_type() {
        let lines = vec!["com.example:foo:1.0:jar".to_owned()];
        let coords = parse_coordinate_lines(&lines).unwrap();
        assert_eq!(coords[0].1, ArtifactType::Jar);
    }

    #[test]
    fn parse_coordinate_lines_rejects_unknown_type() {
        let lines = vec!["com.example:foo:1.0:war".to_owned()];
        assert!(parse_coordinate_lines(&lines).is_err());
    }

    #[test]
    fn parse_coordinate_lines_rejects_malformed_and_names_it() {
        let lines = vec!["not-a-coordinate".to_owned()];
        let err = parse_coordinate_lines(&lines).unwrap_err();
        assert!(err.to_string().contains("not-a-coordinate"));
    }

    // -----------------------------------------------------------------------
    // run_fetch_file — no network required
    // -----------------------------------------------------------------------

    #[test]
    fn run_fetch_file_with_only_comments_succeeds() {
        let (_dir, path) = write_coord_file("# nothing here\n\n# also nothing\n");
        let project = tempfile::tempdir().unwrap();
        run_fetch_file(project.path(), &path, false, true).unwrap();
    }

    #[test]
    fn run_fetch_file_offline_cache_miss_errors() {
        let (_dir, path) = write_coord_file("com.example:definitely-not-cached:9.9.9\n");
        let project = tempfile::tempdir().unwrap();
        let err = run_fetch_file(project.path(), &path, false, true).unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn run_fetch_file_pom_offline_cache_miss_errors() {
        let (_dir, path) = write_coord_file("com.example:definitely-not-cached:9.9.9:pom\n");
        let project = tempfile::tempdir().unwrap();
        let err = run_fetch_file(project.path(), &path, false, true).unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
