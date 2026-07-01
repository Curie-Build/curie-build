//! `curie deps` — print the dependency tree and explain version selection.

use crate::build::{central_repos, extra_repos};
use crate::{descriptor, maven, workspace};
use anyhow::{bail, Context, Result};
use curie_deps::{DepEntry, DepTree, ResolvedDep, ResolveOptions};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Entry point when called from a workspace member context.
pub fn run_deps_workspace_member(
    workspace_root: &Path,
    member_index: usize,
    why: Option<&str>,
    tests: bool,
    offline: bool,
) -> Result<()> {
    let ws = workspace::load(workspace_root)?;
    let member = &ws.members[member_index];
    run_deps_with_desc(&member.path, &member.descriptor, why, tests, offline)
}

/// Entry point for standalone (non-workspace) projects.
pub fn run_deps(project_root: &Path, why: Option<&str>, tests: bool, offline: bool) -> Result<()> {
    let desc = descriptor::load(project_root)?;
    if desc.is_workspace() {
        bail!(
            "`curie deps` cannot run on a workspace root; \
             target a member with --project"
        );
    }
    run_deps_with_desc(project_root, &desc, why, tests, offline)
}

/// Entry point when the descriptor has already been loaded with workspace
/// inheritance applied (for workspace member projects).
pub fn run_deps_with_desc(
    _project_root: &Path,
    desc: &descriptor::Descriptor,
    why: Option<&str>,
    tests: bool,
    offline: bool,
) -> Result<()> {
    // Choose between production deps and test deps.
    let dep_map  = if tests { &desc.test_dependencies } else { &desc.dependencies };
    let bom_gavs = if tests { desc.test_bom_gavs()? }  else { desc.prod_bom_gavs()? };
    let scope_label = if tests { "Test dependencies" } else { "Dependencies" };

    if dep_map.is_empty() {
        println!(
            "{} for {} v{}",
            scope_label, desc.buildable_name(), desc.buildable_version(),
        );
        println!("  (none)");
        return Ok(());
    }

    let entries: Vec<DepEntry> = dep_map
        .iter()
        .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository(), exclusions: v.exclusions(), classifier: None, allow_version_conflict: v.allow_version_conflict() })
        .collect();
    let opts = ResolveOptions {
        default_repos: central_repos(),
        named_repos: extra_repos(desc),
        progress: false,
        bom_imports: bom_gavs,
        offline,
        skip_version_ranges: false, error_on_version_conflict: false,
    };

    let tree = curie_deps::resolve_tree(&entries, &opts)?;

    match why {
        None => print_tree_with_label(scope_label, desc, &tree),
        Some(coord) => explain_why(coord, &tree),
    }
}

// ---------------------------------------------------------------------------
// `curie maven sync` resolution helpers
// ---------------------------------------------------------------------------
//
// `maven.rs` builds POM models from caller-supplied "resolved external
// inputs" so it stays free of network/resolver I/O (see
// `maven::build_project`'s doc comment). These two functions are how
// `curie maven sync` (maven.rs's `run_maven_sync_*` entry points) produces
// those inputs for the `pinTransitive` and BOM-managed-annotation-processor
// cases. Both return early without touching the resolver when the descriptor
// doesn't need them — the common case (`pinTransitive = false`, no blank AP
// versions) requires no dependency resolution at all.

/// Resolve the full transitive dependency closure (`[dependencies]` +
/// `[test-dependencies]`, combined) for `[maven] pinTransitive = true`,
/// returning the `group:artifact -> version` set to materialize into
/// `<dependencyManagement>` so Maven's version mediation can't diverge from
/// Curie's resolver. Returns `Ok(vec![])` without resolving anything when
/// pinTransitive is disabled (the default).
pub fn resolve_pinned_dependencies(desc: &descriptor::Descriptor, offline: bool) -> Result<Vec<maven::PinnedDependency>> {
    if !desc.maven.pin_transitive_enabled() {
        return Ok(Vec::new());
    }

    let mut seen = HashSet::new();
    let entries: Vec<DepEntry> = desc
        .dependencies
        .iter()
        .chain(desc.test_dependencies.iter())
        .filter(|(k, _)| seen.insert(k.as_str()))
        .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository(), exclusions: v.exclusions(), classifier: None, allow_version_conflict: v.allow_version_conflict() })
        .collect();

    let tree = curie_deps::resolve_tree(
        &entries,
        &ResolveOptions {
            default_repos: central_repos(),
            named_repos: extra_repos(desc),
            progress: false,
            bom_imports: desc.test_bom_gavs()?,
            offline,
            skip_version_ranges: false, error_on_version_conflict: false,
        },
    )
    .context("pinTransitive: failed to resolve the transitive dependency closure")?;

    let mut pins: Vec<maven::PinnedDependency> = tree
        .resolved
        .iter()
        .map(|d| maven::PinnedDependency {
            group_id: d.gav.group.clone(),
            artifact_id: d.gav.artifact.clone(),
            version: d.gav.version.clone(),
        })
        .collect();
    pins.sort();
    pins.dedup();
    Ok(pins)
}

/// Resolve concrete versions for `[annotation-processors]`/
/// `[test-annotation-processors]` entries declared with a blank version
/// (BOM-managed), for `curie maven sync`. Returns `Ok(BTreeMap::new())`
/// without resolving anything when every processor has an explicit version.
pub fn resolve_ap_versions_for_sync(desc: &descriptor::Descriptor, offline: bool) -> Result<BTreeMap<String, String>> {
    let mut seen = HashSet::new();
    let blank: Vec<&str> = desc
        .ap_pairs()
        .into_iter()
        .chain(desc.test_ap_pairs())
        .filter(|(coord, version)| version.is_empty() && seen.insert(*coord))
        .map(|(coord, _)| coord)
        .collect();

    if blank.is_empty() {
        return Ok(BTreeMap::new());
    }

    let entries: Vec<DepEntry> = blank
        .iter()
        .map(|coord| DepEntry { key: coord, version: "", repo_id: None, exclusions: vec![], classifier: None, allow_version_conflict: false })
        .collect();

    let tree = curie_deps::resolve_tree(
        &entries,
        &ResolveOptions {
            default_repos: central_repos(),
            named_repos: extra_repos(desc),
            progress: false,
            bom_imports: desc.test_bom_gavs()?,
            offline,
            skip_version_ranges: false, error_on_version_conflict: false,
        },
    )
    .context("failed to resolve BOM-managed annotation processor versions")?;

    let mut resolved = BTreeMap::new();
    for coord in blank {
        if let Some(dep) = tree.resolved.iter().find(|d| format!("{}:{}", d.gav.group, d.gav.artifact) == coord) {
            resolved.insert(coord.to_string(), dep.gav.version.clone());
        }
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Tree printing
// ---------------------------------------------------------------------------

fn print_tree_with_label(label: &str, desc: &descriptor::Descriptor, tree: &DepTree) -> Result<()> {
    println!(
        "{} for {} v{}",
        label, desc.buildable_name(),
        desc.buildable_version(),
    );

    if tree.resolved.is_empty() {
        println!("  (none)");
        return Ok(());
    }

    // Build a parent → children map keyed by "group:artifact:version".
    // Depth-0 roots have no parent (via = None).
    let mut children_of: std::collections::HashMap<String, Vec<&ResolvedDep>> =
        std::collections::HashMap::new();
    let roots: Vec<&ResolvedDep> = tree
        .resolved
        .iter()
        .filter(|d| d.via.is_none())
        .collect();

    for dep in &tree.resolved {
        if let Some(via) = &dep.via {
            children_of
                .entry(via.notation())
                .or_default()
                .push(dep);
        }
    }

    for (i, root) in roots.iter().enumerate() {
        let is_last = i == roots.len() - 1;
        print_node(root, &children_of, "", is_last);
    }
    Ok(())
}

fn print_node(
    dep: &ResolvedDep,
    children_of: &std::collections::HashMap<String, Vec<&ResolvedDep>>,
    prefix: &str,
    is_last: bool,
) {
    let connector = if is_last { "└─ " } else { "├─ " };
    println!("{}{}{}", prefix, connector, dep.gav.notation());

    let children = children_of.get(&dep.gav.notation());
    if let Some(kids) = children {
        let child_prefix = format!("{}{}  ", prefix, if is_last { " " } else { "│" });
        for (j, child) in kids.iter().enumerate() {
            let last = j == kids.len() - 1;
            print_node(child, children_of, &child_prefix, last);
        }
    }
}

// ---------------------------------------------------------------------------
// --why explanation
// ---------------------------------------------------------------------------

fn explain_why(coord: &str, tree: &DepTree) -> Result<()> {
    // Accept "group:artifact" or "group:artifact:version".
    let ga_key = {
        let parts: Vec<&str> = coord.splitn(3, ':').collect();
        if parts.len() < 2 {
            bail!(
                "invalid coordinate {:?} — expected \"group:artifact\" or \
                 \"group:artifact:version\"",
                coord
            );
        }
        format!("{}:{}", parts[0].trim(), parts[1].trim())
    };

    let chosen = tree
        .resolved
        .iter()
        .find(|d| format!("{}:{}", d.gav.group, d.gav.artifact) == ga_key)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "\"{}\" is not in the resolved dependency tree.\n\
                 Tip: run `curie deps` (without --why) to see all resolved artifacts.",
                ga_key
            )
        })?;

    println!("{}  (depth {})", chosen.gav.notation(), chosen.depth);
    println!();

    // Reconstruct the introduction chain for the chosen version.
    println!("  Introduced by:");
    let chain = build_chain(chosen, &tree.resolved);
    println!("    {} → {}  (chosen — depth {})",
        chain_to_string(&chain), chosen.gav.notation(), chosen.depth);
    println!();

    // Skipped losers for the same GA.
    let losers = tree.skipped.get(&ga_key);
    match losers {
        None => println!("  No version conflicts."),
        Some(skips) => {
            let mut sorted = skips.to_vec();
            sorted.sort_by_key(|s| s.depth);
            println!("  Skipped (nearest-wins):");
            for loser in &sorted {
                // Build the introduction chain for the losing candidate.
                let loser_chain: Vec<curie_deps::Gav> = loser.via.as_ref()
                    .and_then(|v| tree.resolved.iter().find(|d| d.gav.notation() == v.notation()))
                    .map(|via_dep| {
                        let mut c = build_chain(via_dep, &tree.resolved);
                        c.push(via_dep.gav.clone());
                        c
                    })
                    .or_else(|| loser.via.as_ref().map(|v| vec![v.clone()]))
                    .unwrap_or_default();

                println!(
                    "    {} → {}:{}  (depth {})",
                    chain_to_string(&loser_chain),
                    ga_key,
                    loser.version,
                    loser.depth,
                );
            }
            println!();
            println!("  → version {} wins because it is at depth {} (shallowest path wins).",
                chosen.gav.version, chosen.depth);
        }
    }

    Ok(())
}

/// Walk the `via` chain from `dep` back to the root and return the ancestor
/// GAVs in root-first order (not including `dep` itself).
fn build_chain<'a>(dep: &'a ResolvedDep, all: &'a [ResolvedDep]) -> Vec<curie_deps::Gav> {
    let mut chain: Vec<curie_deps::Gav> = Vec::new();
    let mut cursor: Option<&curie_deps::Gav> = dep.via.as_ref();
    while let Some(via_gav) = cursor {
        chain.push(via_gav.clone());
        cursor = all
            .iter()
            .find(|d| d.gav.notation() == via_gav.notation())
            .and_then(|d| d.via.as_ref());
    }
    chain.reverse();
    chain
}

fn chain_to_string(chain: &[curie_deps::Gav]) -> String {
    let mut p = vec!["[declared]".to_string()];
    for g in chain {
        p.push(g.notation());
    }
    p.join(" → ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{AnnotationProcessor, Application, DescriptorKind, *};

    fn minimal_app() -> descriptor::Descriptor {
        descriptor::Descriptor {
            kind: DescriptorKind::Application(Application {
                name: "app".to_string(),
                version: "1.0.0".to_string(),
                group_id: Some("com.example".to_string()),
                main_class: Some("com.example.Main".to_string()),
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            spock: Spock::default(),
            native_image: NativeImage::default(),
            jlink: Jlink::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            fat_jar: FatJar::default(),
            dependencies: BTreeMap::new(),
            test_dependencies: BTreeMap::new(),
            repositories: vec![],
            bom_imports: BTreeMap::new(),
            test_bom_imports: BTreeMap::new(),
            inherited_bom_imports: BTreeMap::new(),
            inherited_test_bom_imports: BTreeMap::new(),
            workspace_dependencies: BTreeMap::new(),
            annotation_processors: BTreeMap::new(),
            test_annotation_processors: BTreeMap::new(),
            inherited_annotation_processors: BTreeMap::new(),
            inherited_test_annotation_processors: BTreeMap::new(),
            annotation_processor_options: BTreeMap::new(),
            test_annotation_processor_options: BTreeMap::new(),
            inherited_annotation_processor_options: BTreeMap::new(),
            inherited_test_annotation_processor_options: BTreeMap::new(),
            publish: PublishConfig::default(),
            plugins: BTreeMap::new(),
            maven: MavenConfig::default(),
            modules: ModulesConfig::default(),
            resources: Resources::default(),
            test_resources: Resources::default(),
        }
    }

    #[test]
    fn resolve_pinned_dependencies_returns_empty_when_disabled() {
        let desc = minimal_app();
        assert!(!desc.maven.pin_transitive_enabled());
        // offline: true — if this fell through to the resolver it would
        // error rather than return an empty Vec, proving the fast path ran.
        let pins = resolve_pinned_dependencies(&desc, true).unwrap();
        assert!(pins.is_empty());
    }

    #[test]
    fn resolve_ap_versions_for_sync_returns_empty_when_no_blank_versions() {
        let mut desc = minimal_app();
        desc.annotation_processors.insert(
            "org.projectlombok:lombok".to_string(),
            AnnotationProcessor::Version("1.18.32".to_string()),
        );
        // offline: true — a fall-through to the resolver would error here.
        let resolved = resolve_ap_versions_for_sync(&desc, true).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn explain_why_errors_for_unknown_coordinate() {
        let tree = DepTree {
            resolved: vec![],
            skipped: std::collections::HashMap::new(),
        };
        let err = explain_why("org.unknown:artifact", &tree).unwrap_err().to_string();
        assert!(err.contains("org.unknown:artifact"), "got: {err}");
        assert!(err.contains("not in the resolved"), "got: {err}");
    }

    #[test]
    fn explain_why_errors_for_bad_coordinate() {
        let tree = DepTree {
            resolved: vec![],
            skipped: std::collections::HashMap::new(),
        };
        let err = explain_why("not-a-valid-coord", &tree).unwrap_err().to_string();
        assert!(err.contains("invalid coordinate"), "got: {err}");
    }
}
