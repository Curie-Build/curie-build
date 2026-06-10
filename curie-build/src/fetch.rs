//! `curie fetch` — pre-download dependency artifacts into `~/.m2/repository`.
//!
//! With no arguments, fetches everything declared in `Curie.toml`:
//! `[dependencies]`, `[test-dependencies]`, `[annotation-processors]`, and
//! `[test-annotation-processors]`.  With a `"group:artifact:version"`
//! coordinate, fetches that artifact and (unless `--no-transitive`) its
//! transitive closure.

use crate::build::{central_repos, extra_repos};
use crate::{descriptor, workspace};
use anyhow::{bail, Result};
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

/// Parse a `"group:artifact:version"` coordinate.  All three parts are
/// required since fetching needs a concrete version.  Group/artifact/version
/// validation is delegated to [`Gav::from_key_version`].
fn parse_gav_arg(arg: &str) -> Result<Gav> {
    let parts: Vec<&str> = arg.split(':').collect();
    if parts.len() != 3 {
        bail!("invalid coordinate {:?}: expected \"group:artifact:version\"", arg);
    }
    Gav::from_key_version(&format!("{}:{}", parts[0], parts[1]), parts[2])
}

// ---------------------------------------------------------------------------
// `curie fetch <group:artifact:version> [--no-transitive]`
// ---------------------------------------------------------------------------

fn fetch_coordinate(desc: &descriptor::Descriptor, coord: &str, no_transitive: bool, offline: bool) -> Result<()> {
    let gav = parse_gav_arg(coord)?;

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
    };
    let jars = curie_deps::resolve(&entries, &opts)?;
    let label = if tests { "Test deps" } else { "Dependencies" };
    crate::parallel::emit(&crate::style::resolve(label, &format!("{} JAR(s)", jars.len())));
    Ok(jars.len())
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
}
