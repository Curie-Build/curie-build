//! `curie deps` — print the dependency tree and explain version selection.
//!
//! Also provides declared-dependency views and resolved tree lines for
//! `curie inspect` (Dependencies mode).

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
    let scope_label = if tests { "Test dependencies" } else { "Dependencies" };
    let dep_map = if tests {
        &desc.test_dependencies
    } else {
        &desc.dependencies
    };

    if dep_map.is_empty() {
        println!(
            "{} for {} v{}",
            scope_label,
            desc.buildable_name(),
            desc.buildable_version(),
        );
        println!("  (none)");
        return Ok(());
    }

    let tree = resolve_dep_tree(desc, tests, offline)?;

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
            snapshot_pins: Default::default(),
            update_snapshots: false,
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
            snapshot_pins: Default::default(),
            update_snapshots: false,
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
// Inspect: declared dependency view + resolved tree lines
// ---------------------------------------------------------------------------

/// One declared compile/test dependency for display (and resolve fallback).
#[derive(Debug, Clone)]
pub struct DepItem {
    /// Coordinate key (`group:artifact`).
    pub coord: String,
    /// Version string as declared; empty means BOM-managed.
    pub version: String,
    /// Optional note shown after the coordinate (e.g. `javaAgent`).
    pub note: String,
}

impl DepItem {
    /// Single-line display, e.g. `com.foo:bar:1.0` or `com.foo:bar  (BOM)`.
    pub fn display_line(&self) -> String {
        let base = if self.version.is_empty() {
            format!("{}  (BOM)", self.coord)
        } else {
            format!("{}:{}", self.coord, self.version)
        };
        if self.note.is_empty() {
            base
        } else {
            format!("{base}  ({})", self.note)
        }
    }
}

/// Declared compile/test dependency surface of one project / workspace member.
///
/// BOM imports are not listed separately: they only feed version resolution
/// when building the compile/test trees in the detail pane.
#[derive(Debug, Clone)]
pub struct MemberDepsView {
    pub kind_label: String,
    pub name: String,
    pub version: String,
    pub compile: Vec<DepItem>,
    pub test: Vec<DepItem>,
}

impl MemberDepsView {
    pub fn from_descriptor(desc: &descriptor::Descriptor) -> Self {
        let (name, version) = match (desc.project_name(), desc.project_version()) {
            (Some(n), Some(v)) => (n.to_string(), v.to_string()),
            _ => ("(workspace)".to_string(), String::new()),
        };

        let compile = desc
            .dependencies
            .iter()
            .map(|(k, v)| {
                let mut note = String::new();
                if v.java_agent() {
                    note.push_str("javaAgent");
                }
                DepItem {
                    coord: k.clone(),
                    version: v.version().to_string(),
                    note,
                }
            })
            .collect();

        let test = desc
            .test_dependencies
            .iter()
            .map(|(k, v)| {
                let mut note = String::new();
                if v.java_agent() {
                    note.push_str("javaAgent");
                }
                DepItem {
                    coord: k.clone(),
                    version: v.version().to_string(),
                    note,
                }
            })
            .collect();

        Self {
            kind_label: desc.kind_label().to_string(),
            name,
            version,
            compile,
            test,
        }
    }

    /// Count of declared compile + test dependencies (inspect badge).
    pub fn compile_test_count(&self) -> usize {
        self.compile.len() + self.test.len()
    }
}

/// Resolve production or test `[dependencies]` into a full tree.
///
/// BOM imports from the descriptor (including workspace-inherited ones) are
/// passed to the resolver so version-less coordinates get concrete versions —
/// they are not emitted as a separate tree section.
pub fn resolve_dep_tree(
    desc: &descriptor::Descriptor,
    tests: bool,
    offline: bool,
) -> Result<DepTree> {
    let dep_map = if tests {
        &desc.test_dependencies
    } else {
        &desc.dependencies
    };
    let bom_gavs = if tests {
        desc.test_bom_gavs()?
    } else {
        desc.prod_bom_gavs()?
    };
    if dep_map.is_empty() {
        return Ok(DepTree {
            resolved: vec![],
            skipped: std::collections::HashMap::new(),
        });
    }
    let entries: Vec<DepEntry> = dep_map
        .iter()
        .map(|(k, v)| DepEntry {
            key: k,
            version: v.version(),
            repo_id: v.repository(),
            exclusions: v.exclusions(),
            classifier: None,
            allow_version_conflict: v.allow_version_conflict(),
        })
        .collect();
    let opts = ResolveOptions {
        default_repos: central_repos(),
        named_repos: extra_repos(desc),
        progress: false,
        bom_imports: bom_gavs,
        offline,
        skip_version_ranges: false,
        error_on_version_conflict: false,
        snapshot_pins: Default::default(),
        update_snapshots: false,
        };
    curie_deps::resolve_tree(&entries, &opts)
}

/// Format a [`DepTree`] as lines with box-drawing connectors (no trailing newline).
pub fn format_tree_lines(tree: &DepTree) -> Vec<String> {
    if tree.resolved.is_empty() {
        return vec!["(none)".to_string()];
    }
    let mut children_of: std::collections::HashMap<String, Vec<&ResolvedDep>> =
        std::collections::HashMap::new();
    let roots: Vec<&ResolvedDep> = tree
        .resolved
        .iter()
        .filter(|d| d.via.is_none())
        .collect();
    for dep in &tree.resolved {
        if let Some(via) = &dep.via {
            children_of.entry(via.notation()).or_default().push(dep);
        }
    }
    let mut lines = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let is_last = i == roots.len() - 1;
        append_node_lines(root, &children_of, "", is_last, &mut lines);
    }
    lines
}

fn append_node_lines(
    dep: &ResolvedDep,
    children_of: &std::collections::HashMap<String, Vec<&ResolvedDep>>,
    prefix: &str,
    is_last: bool,
    lines: &mut Vec<String>,
) {
    let connector = if is_last { "└─ " } else { "├─ " };
    lines.push(format!("{}{}{}", prefix, connector, dep.gav.notation()));
    if let Some(kids) = children_of.get(&dep.gav.notation()) {
        let child_prefix = format!("{}{}  ", prefix, if is_last { " " } else { "│" });
        for (j, child) in kids.iter().enumerate() {
            let last = j == kids.len() - 1;
            append_node_lines(child, children_of, &child_prefix, last, lines);
        }
    }
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

    for line in format_tree_lines(tree) {
        println!("{line}");
    }
    Ok(())
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

    #[test]
    fn member_deps_view_from_descriptor_compile_and_test_only() {
        let mut desc = minimal_app();
        desc.dependencies.insert(
            "com.example:foo".into(),
            DependencyValue::Version("1.0".into()),
        );
        desc.test_dependencies.insert(
            "org.junit.jupiter:junit-jupiter".into(),
            DependencyValue::Version("5.10.0".into()),
        );
        // BOMs / workspace / APs must not appear as separate lists.
        desc.workspace_dependencies.insert(
            "core".into(),
            WorkspaceDep {
                path: "../core".into(),
                version: None,
            },
        );
        desc.annotation_processors.insert(
            "org.projectlombok:lombok".into(),
            AnnotationProcessor::Version("1.18.30".into()),
        );
        desc.bom_imports
            .insert("com.fasterxml.jackson:jackson-bom".into(), "2.17.2".into());

        let view = MemberDepsView::from_descriptor(&desc);
        assert_eq!(view.kind_label, "application");
        assert_eq!(view.compile.len(), 1);
        assert_eq!(view.test.len(), 1);
        assert_eq!(view.compile_test_count(), 2);
        assert_eq!(view.compile[0].coord, "com.example:foo");
        assert_eq!(view.test[0].coord, "org.junit.jupiter:junit-jupiter");
    }

    #[test]
    fn format_tree_lines_renders_connectors() {
        use curie_deps::Gav;
        let root = Gav::from_key_version("a:root", "1").unwrap();
        let child = Gav::from_key_version("a:child", "1").unwrap();
        let tree = DepTree {
            resolved: vec![
                ResolvedDep {
                    gav: root.clone(),
                    depth: 0,
                    via: None,
                },
                ResolvedDep {
                    gav: child,
                    depth: 1,
                    via: Some(root),
                },
            ],
            skipped: std::collections::HashMap::new(),
        };
        let lines = format_tree_lines(&tree);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("a:root:1"));
        assert!(lines[1].contains("a:child:1"));
        assert!(lines[0].starts_with("└─ ") || lines[0].starts_with("├─ "));
    }

    #[test]
    fn dep_item_display_bom_managed() {
        let bom = DepItem {
            coord: "com.foo:bar".into(),
            version: String::new(),
            note: String::new(),
        };
        assert!(bom.display_line().contains("(BOM)"));
        let pinned = DepItem {
            coord: "com.foo:bar".into(),
            version: "1.2.3".into(),
            note: "javaAgent".into(),
        };
        assert_eq!(pinned.display_line(), "com.foo:bar:1.2.3  (javaAgent)");
    }
}
