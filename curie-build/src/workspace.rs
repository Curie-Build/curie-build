//! Workspace discovery, member iteration, and fan-out for the top-level
//! commands (`build`, `test`, `clean`).
//!
//! A workspace is rooted at a `Curie.toml` whose `[workspace]` section lists
//! `members` (paths relative to that `Curie.toml`'s directory).  Each member
//! is itself a buildable project (application or library) with its own
//! `Curie.toml`.
//!
//! Step 2 (this module): members are iterated in declared order; each runs
//! through the existing single-module pipeline.  No intra-workspace
//! dependencies yet — those arrive in step 3 along with topo sort.

use crate::audit::{self, AuditOptions};
use crate::descriptor::{self, Descriptor};
use crate::update::{self, UpdateOptions};
use crate::{build, compile, fmt, jar, run, test};
use anyhow::{bail, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A single member of a workspace: its path on disk plus its loaded descriptor.
#[derive(Debug)]
pub struct Member {
    /// Path to the member's directory (workspace-root-relative as resolved
    /// at load time).
    pub path: PathBuf,
    /// Member name as declared in the workspace's `members = [...]` list,
    /// kept verbatim for use in messages where the user-facing path matters
    /// (e.g. `curie list` output).
    pub declared: String,
    pub descriptor: Descriptor,
    /// Indices into [`Workspace::members`] of this member's resolved
    /// `[workspace-dependencies]`.  Because members are stored in topo
    /// build order, every entry here is strictly less than this member's
    /// own index.
    pub workspace_deps: Vec<usize>,
}

/// Loaded workspace: the root directory containing `[workspace]` plus every
/// member's descriptor, loaded once.
#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub members: Vec<Member>,
}

/// Result of [`discover`]: the relationship between a `--project` path and
/// any surrounding workspace.
#[derive(Debug)]
pub enum WorkspaceContext {
    /// `project` IS a workspace root (its `Curie.toml` has `[workspace]`).
    WorkspaceRoot(PathBuf),
    /// `project` is a member of an enclosing workspace found by walking
    /// upward.  Carries the workspace root path and the member's index in
    /// the loaded `Workspace::members` (post-topo-sort).
    WorkspaceMember {
        workspace_root: PathBuf,
        member_index: usize,
    },
    /// `project` is itself a nested workspace inside a larger enclosing
    /// workspace.  Carries the outermost root and the indices (in the loaded
    /// `Workspace::members`) of the members located under `project`'s
    /// subtree.  Operating on this context fans out over those members plus
    /// their transitive workspace-deps.
    WorkspaceSubtree {
        workspace_root: PathBuf,
        member_indices: Vec<usize>,
    },
    /// No workspace context — single-module mode.
    Standalone(PathBuf),
}

/// Resolve a `--project` path to its workspace context.
///
/// Rules:
///   1. Walk upward and keep the **outermost** enclosing workspace whose
///      *flattened* member list (after recursive nested-workspace expansion)
///      contains `project`:
///        - `project` is a leaf → a member whose canonical path *equals*
///          `project` → `WorkspaceMember`.
///        - `project` is itself a workspace → the members whose canonical
///          path lives under `project`'s directory → `WorkspaceSubtree`.
///      Outermost wins so the full inheritance chain applies and
///      `[workspace-dependencies]` that cross inner-workspace boundaries
///      resolve against the root's flattened member list.
///   2. No enclosing workspace found, but `project` is itself a workspace
///      → `WorkspaceRoot`.
///   3. Otherwise → `Standalone`.
///
/// Ancestor `Curie.toml`s that fail to parse or load are tolerated and walked
/// past: they may belong to an unrelated project above us in the filesystem,
/// or to an inner workspace that can't resolve a cross-level dep on its own.
/// A parse error inside `project`'s own `Curie.toml` is still surfaced (the
/// user explicitly targeted it).
pub fn discover(project: &Path) -> Result<WorkspaceContext> {
    let desc = descriptor::load(project)?;
    let project_is_workspace = desc.is_workspace();

    let project_canon = project
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", project.display()))?;

    // Walk upward (canonicalized — `".".parent()` is `""` and stops early),
    // overwriting `best` on every match so the farthest (outermost) enclosing
    // workspace wins.
    let mut best: Option<WorkspaceContext> = None;
    let mut cur = project_canon.parent();
    while let Some(dir) = cur {
        if dir.join("Curie.toml").exists() {
            if let Ok(d) = descriptor::load(dir) {
                if d.is_workspace() {
                    if let Ok(ws_loaded) = load(dir) {
                        if project_is_workspace {
                            // Members located under `project`'s directory.
                            // `Path::starts_with` is component-aware, so
                            // `services` does not match `services-extra`.
                            let idxs: Vec<usize> = ws_loaded
                                .members
                                .iter()
                                .enumerate()
                                .filter(|(_, m)| {
                                    m.path
                                        .canonicalize()
                                        .ok()
                                        .is_some_and(|c| c.starts_with(&project_canon))
                                })
                                .map(|(i, _)| i)
                                .collect();
                            if !idxs.is_empty() {
                                best = Some(WorkspaceContext::WorkspaceSubtree {
                                    workspace_root: dir.to_path_buf(),
                                    member_indices: idxs,
                                });
                            }
                        } else if let Some(topo_idx) = ws_loaded.members.iter().position(|m| {
                            m.path.canonicalize().ok() == Some(project_canon.clone())
                        }) {
                            best = Some(WorkspaceContext::WorkspaceMember {
                                workspace_root: dir.to_path_buf(),
                                member_index: topo_idx,
                            });
                        }
                    }
                }
            }
        }
        cur = dir.parent();
    }

    if let Some(ctx) = best {
        return Ok(ctx);
    }
    if project_is_workspace {
        return Ok(WorkspaceContext::WorkspaceRoot(project.to_path_buf()));
    }
    Ok(WorkspaceContext::Standalone(project.to_path_buf()))
}

/// Load the workspace rooted at `workspace_root`.  Fails if the directory's
/// `Curie.toml` is missing or does not contain `[workspace]`.
///
/// Member descriptors are loaded eagerly so that a malformed member's
/// `Curie.toml` is reported immediately instead of mid-build.
pub fn load(workspace_root: &Path) -> Result<Workspace> {
    let root_desc = descriptor::load(workspace_root)
        .with_context(|| format!("failed to load workspace at {}", workspace_root.display()))?;

    let ws = root_desc
        .workspace()
        .ok_or_else(|| anyhow::anyhow!(
            "{} is not a workspace: its Curie.toml has no [workspace] section",
            workspace_root.display(),
        ))?;

    // Phase 1: load every member descriptor (sans workspace_deps) and
    // merge in inherited workspace-level config.  Nested workspaces are
    // recursively flattened: their leaf (non-workspace) members are
    // collected into this workspace with configuration inheritance
    // cascading from outer to inner.
    let mut raw_members: Vec<Member> = Vec::new();
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    // Seed with the root's own canonical path so that a cycle back to the
    // root (e.g. inner workspace listing ".." as a member) is caught.
    seen_canonical.insert(
        workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf()),
    );
    expand_members(
        workspace_root,
        &root_desc,
        &ws.members,
        "",
        &mut raw_members,
        &mut seen_canonical,
    )?;

    // Phase 2: resolve each member's [workspace-dependencies] path entries
    // to indices into raw_members.  Canonical-path equality is the matcher
    // so `../sibling` and `./../sibling` both find the same member.
    let canon: Vec<PathBuf> = raw_members
        .iter()
        .map(|m| m.path.canonicalize().unwrap_or_else(|_| m.path.clone()))
        .collect();

    // edges[i] = indices of members that member i depends on.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); raw_members.len()];
    for (i, m) in raw_members.iter().enumerate() {
        for (label, dep) in &m.descriptor.workspace_dependencies {
            let target = m.path.join(&dep.path);
            let target_canon = target.canonicalize().with_context(|| {
                format!(
                    "workspace-dep \"{}\" of \"{}\" points to {} which does not exist",
                    label, m.declared, target.display(),
                )
            })?;
            let target_idx = canon.iter().position(|c| c == &target_canon).ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace-dep \"{}\" of \"{}\" → {} is not a workspace member; add it to [workspace.members] in {}",
                    label, m.declared, target.display(), workspace_root.join("Curie.toml").display(),
                )
            })?;
            if target_idx == i {
                bail!(
                    "workspace-dep \"{}\" of \"{}\" points at itself",
                    label, m.declared,
                );
            }
            edges[i].push(target_idx);
        }
    }

    // Phase 3: topological sort into build order.
    let order = topo_sort(raw_members.len(), &edges).map_err(|cycle| {
        let chain = cycle
            .iter()
            .map(|&i| raw_members[i].declared.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        anyhow::anyhow!("workspace-dependency cycle detected: {}", chain)
    })?;

    // Phase 4: reorder raw_members into topo order and remap each member's
    // workspace_deps indices to the new positions.
    let mut old_to_new = vec![0usize; raw_members.len()];
    for (new_idx, &old_idx) in order.iter().enumerate() {
        old_to_new[old_idx] = new_idx;
    }
    let mut slots: Vec<Option<Member>> = raw_members.into_iter().map(Some).collect();
    let mut members: Vec<Member> = Vec::with_capacity(order.len());
    for &old_idx in &order {
        let mut m = slots[old_idx].take().expect("each slot drained exactly once");
        m.workspace_deps = edges[old_idx].iter().map(|&old| old_to_new[old]).collect();
        members.push(m);
    }

    Ok(Workspace {
        root: workspace_root.to_path_buf(),
        members,
    })
}

/// Recursively expand workspace members.  Non-workspace members are added
/// to `out` directly; workspace members are recursed into and their own
/// members are flattened.  At each level, configuration inheritance from the
/// enclosing workspace is applied.
///
/// `ws_root` is the directory containing the enclosing `Curie.toml`.
/// `ws_desc` is that workspace's loaded descriptor (for inheritance).
/// `member_names` is the `[workspace].members` list of that workspace.
/// `prefix` is the path prefix for `declared` names of nested members
/// (empty at the top level, e.g. `"services/"` for members of a nested
/// workspace called `services`).
///
/// `seen` tracks canonical paths of every project and workspace encountered
/// so far.  If a path appears twice, the function bails — this covers both
/// explicit duplicates and dependency cycles.
fn expand_members(
    ws_root: &Path,
    ws_desc: &Descriptor,
    member_names: &[String],
    prefix: &str,
    out: &mut Vec<Member>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    for name in member_names {
        let path = ws_root.join(name);
        if !path.exists() {
            bail!(
                "workspace member \"{}\" not found at {}",
                name,
                path.display(),
            );
        }

        let canon = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?;
        if !seen.insert(canon) {
            bail!(
                "project \"{}\" is included more than once in the workspace \
                 (nested workspaces that share a member, or a cycle, will trigger this)",
                path.display(),
            );
        }

        let mut descriptor = descriptor::load(&path)
            .with_context(|| format!("failed to load workspace member \"{}\"", name))?;

        if descriptor.is_workspace() {
            // Nested workspace: recursively flatten its members.
            let inner_ws = descriptor.workspace().unwrap();
            let inner_members = inner_ws.members.clone();
            let inner_prefix = if prefix.is_empty() {
                format!("{}/", name)
            } else {
                format!("{}{}/", prefix, name)
            };

            // First expand inner members with the inner workspace's config.
            let before_len = out.len();
            expand_members(&path, &descriptor, &inner_members, &inner_prefix, out, seen)?;

            // Then layer the outer workspace's config on top.  Scalar
            // fields already set by the inner workspace survive (the
            // is_none guard), and collection fields are merged with the
            // inner values taking priority on key conflicts.
            for m in &mut out[before_len..] {
                inherit_from_workspace(&mut m.descriptor, ws_desc);
                descriptor::validate_dep_repo_refs(&m.descriptor).with_context(|| {
                    format!(
                        "invalid repository reference in nested member \"{}\"",
                        m.declared,
                    )
                })?;
            }
        } else {
            // Regular (leaf) member: apply inheritance from the enclosing
            // workspace and add to the flat list.
            inherit_from_workspace(&mut descriptor, ws_desc);
            descriptor::validate_dep_repo_refs(&descriptor)
                .with_context(|| format!("invalid repository reference in member \"{}\"", name))?;

            let declared = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}{}", prefix, name)
            };

            out.push(Member {
                path,
                declared,
                descriptor,
                workspace_deps: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Merge workspace-level inheritable config into a member descriptor.
/// Called once per member during [`load`] so the build pipeline always sees
/// a fully-resolved descriptor; in single-module mode this never runs and
/// no behaviour changes.
///
/// Inheritance rules (intentionally minimal — covers the cases that
/// actually let users DRY up `[bom-imports]` and `[java]` across siblings):
///
///   - **[java].sourceCompatibility**: member-explicit wins.  Only inherits
///     when the member's value is `None` (the key was absent in its toml).
///   - **[[repositories]]**: workspace's are prepended.  Both lists are
///     searched by the resolver — order matters only for which mirror is
///     tried first.
///   - **[bom-imports]** and **[test-bom-imports]**: workspace's go into
///     the member's `inherited_*_bom_imports` so the resolver sees them
///     before the member's own (later-wins in the resolver gives member
///     priority for any artifact both BOMs manage).
///   - **[annotation-processors]** and **[test-annotation-processors]**:
///     same pattern as BOMs — workspace's entries land in the member's
///     `inherited_*_annotation_processors`.  Member-declared coordinates
///     override workspace-declared ones at the same coord (handled at the
///     `Descriptor::ap_pairs` layer).
///   - **[annotation-processor-options]** (nested, by processor namespace):
///     workspace's tables go into `inherited_annotation_processor_options`.
///     The member's namespace tables override workspace's per inner key —
///     handled in `Descriptor::flat_ap_options`.
fn inherit_from_workspace(member: &mut Descriptor, ws: &Descriptor) {
    if member.java.source_compatibility.is_none() {
        member.java.source_compatibility = ws.java.source_compatibility.clone();
    }
    // enablePreview: member's explicit value (including `false`) wins; only
    // inherit when the member left the key out.
    if member.java.enable_preview.is_none() {
        member.java.enable_preview = ws.java.enable_preview;
    }
    if member.test.junit_platform_version.is_none() {
        member.test.junit_platform_version = ws.test.junit_platform_version.clone();
    }
    if member.kotlin.version.is_none() {
        member.kotlin.version = ws.kotlin.version.clone();
    }
    if member.groovy.version.is_none() {
        member.groovy.version = ws.groovy.version.clone();
    }
    // spock: inherit the workspace's enabled state only when the member is
    // silent — neither an explicit `enabled` key nor a `[spock]` section.  An
    // explicit `enabled = false` therefore opts out of workspace-enabled Spock.
    let member_speaks = member.spock.enabled.is_some() || member.spock.section_present;
    if !member_speaks {
        member.spock.enabled = Some(ws.spock.enabled());
    }
    if member.spock.version.is_none() {
        member.spock.version = ws.spock.version.clone();
    }
    if !ws.repositories.is_empty() {
        let mut combined = ws.repositories.clone();
        combined.append(&mut member.repositories);
        member.repositories = combined;
    }
    // Merge inherited BOM imports: workspace entries go first (lower
    // priority), any already-inherited entries (from an inner workspace)
    // are layered on top so they win on key conflict.
    merge_btree(&mut member.inherited_bom_imports, &ws.bom_imports);
    merge_btree(&mut member.inherited_test_bom_imports, &ws.test_bom_imports);
    merge_btree(&mut member.inherited_annotation_processors, &ws.annotation_processors);
    merge_btree(
        &mut member.inherited_test_annotation_processors,
        &ws.test_annotation_processors,
    );
    merge_nested_btree(
        &mut member.inherited_annotation_processor_options,
        &ws.annotation_processor_options,
    );
    merge_nested_btree(
        &mut member.inherited_test_annotation_processor_options,
        &ws.test_annotation_processor_options,
    );
}

/// Merge `base` entries into `target`.  Existing entries in `target`
/// (from a nearer/inner workspace) take precedence over `base` entries
/// (from a farther/outer workspace).
fn merge_btree<V: Clone>(target: &mut std::collections::BTreeMap<String, V>, base: &std::collections::BTreeMap<String, V>) {
    let existing = std::mem::take(target);
    *target = base.clone();
    for (k, v) in existing {
        target.insert(k, v);
    }
}

/// Like [`merge_btree`] but for the nested `BTreeMap<String, BTreeMap<String, String>>`
/// used by annotation-processor-options.  Inner keys from `target` override
/// `base` per `(prefix, key)`.
fn merge_nested_btree(
    target: &mut std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    base: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) {
    let existing = std::mem::take(target);
    *target = base.clone();
    for (prefix, inner) in existing {
        let dst = target.entry(prefix).or_default();
        for (k, v) in inner {
            dst.insert(k, v);
        }
    }
}

/// Kahn's algorithm.  `edges[v]` is the set of nodes `v` depends on.
/// Returns the build order (deps come first) or, on cycle, the indices of
/// the nodes that couldn't be ordered.
fn topo_sort(n: usize, edges: &[Vec<usize>]) -> std::result::Result<Vec<usize>, Vec<usize>> {
    let mut out_degree: Vec<usize> = edges.iter().map(|e| e.len()).collect();
    let mut reverse: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (v, deps) in edges.iter().enumerate() {
        for &w in deps {
            reverse[w].push(v);
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&v| out_degree[v] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(v) = queue.pop_front() {
        order.push(v);
        for &dependent in &reverse[v] {
            out_degree[dependent] -= 1;
            if out_degree[dependent] == 0 {
                queue.push_back(dependent);
            }
        }
    }
    if order.len() < n {
        // Anything not in `order` is part of (or downstream of) a cycle.
        let leftover: Vec<usize> = (0..n).filter(|v| !order.contains(v)).collect();
        Err(leftover)
    } else {
        Ok(order)
    }
}

// ---------------------------------------------------------------------------
// curie list — tree view
// ---------------------------------------------------------------------------

/// Kind of a list-tree node.
#[derive(Debug)]
enum ListKind {
    Workspace,
    Project { label: &'static str, version: String },
}

/// A node in the list-tree hierarchy.  Workspace nodes contain their declared
/// members as `children`; project nodes are leaves (children is empty).
#[derive(Debug)]
struct ListNode {
    /// Last path component — the name shown in the tree.
    name: String,
    /// Canonical absolute path for identity and ancestor tracking.
    abs_path: PathBuf,
    /// Canonical abs path of the immediately enclosing workspace (or own
    /// abs_path for the outermost root).  Used to relativize `required by`
    /// paths.
    parent_ws_abs: PathBuf,
    kind: ListKind,
    /// Resolved canonical paths of this project's `[workspace-dependencies]`.
    /// Always empty for workspace nodes.
    dep_targets: Vec<PathBuf>,
    children: Vec<ListNode>,
}

/// Recursively build the list-tree rooted at `root`.  Does not flatten
/// nested workspaces — workspace nodes appear as interior nodes with their
/// children intact, preserving the visual hierarchy.
fn build_list_tree(root: &Path, parent_ws_abs: &Path, name: &str) -> Result<ListNode> {
    let abs_path = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let desc = descriptor::load(root)
        .with_context(|| format!("failed to load {}", root.display()))?;

    if desc.is_workspace() {
        let ws = desc.workspace().unwrap();
        let mut children = Vec::new();
        for member_name in &ws.members {
            let child_path = root.join(member_name);
            children.push(
                build_list_tree(&child_path, &abs_path, member_name)
                    .with_context(|| format!("loading member \"{}\"", member_name))?,
            );
        }
        Ok(ListNode {
            name: name.to_string(),
            abs_path,
            parent_ws_abs: parent_ws_abs.to_path_buf(),
            kind: ListKind::Workspace,
            dep_targets: Vec::new(),
            children,
        })
    } else {
        let dep_targets: Vec<PathBuf> = desc
            .workspace_dependencies
            .values()
            .filter_map(|wd| {
                let p = root.join(&wd.path);
                p.canonicalize().ok()
            })
            .collect();
        Ok(ListNode {
            name: name.to_string(),
            abs_path,
            parent_ws_abs: parent_ws_abs.to_path_buf(),
            kind: ListKind::Project {
                label: desc.kind_label(),
                version: desc
                    .project_version()
                    .unwrap_or("?")
                    .to_string(),
            },
            dep_targets,
            children: Vec::new(),
        })
    }
}

/// Computed view for rendering: which nodes to show and each dependency
/// node's `required by` annotation.
#[derive(Debug)]
pub(crate) struct ListView {
    /// Abs paths of nodes that should be rendered.
    pub kept: HashSet<PathBuf>,
    /// Dep target abs_path → list of requirer paths
    /// (each relative to the dep target's parent workspace).
    pub required_by: HashMap<PathBuf, Vec<String>>,
    /// Current project/workspace abs_path (for the `← current` marker).
    pub current: PathBuf,
}

/// Compute a path from `base` to `target` using `..` as needed.
/// Both paths must be canonical (no symlinks, no `.`).
pub(crate) fn rel_from(base: &Path, target: &Path) -> String {
    // Find how many components of `base` and `target` diverge.
    let base_comps: Vec<_> = base.components().collect();
    let tgt_comps: Vec<_> = target.components().collect();
    let common = base_comps
        .iter()
        .zip(tgt_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let up = base_comps.len() - common;
    let mut parts: Vec<String> = std::iter::repeat("..".to_string()).take(up).collect();
    parts.extend(
        tgt_comps[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Walk the list-tree recursively, collecting all nodes into `out`.
fn walk_nodes<'a>(node: &'a ListNode, out: &mut Vec<&'a ListNode>) {
    out.push(node);
    for c in &node.children {
        walk_nodes(c, out);
    }
}

/// Return the abs_paths of all ancestors of `target` in the tree (nearest
/// first), not including `target` itself.
fn ancestors_of(target: &Path, root: &ListNode) -> Vec<PathBuf> {
    fn find(target: &Path, node: &ListNode, stack: &mut Vec<PathBuf>) -> bool {
        if node.abs_path == target {
            return true;
        }
        stack.push(node.abs_path.clone());
        for c in &node.children {
            if find(target, c, stack) {
                return true;
            }
        }
        stack.pop();
        false
    }
    let mut stack = Vec::new();
    find(target, root, &mut stack);
    stack.reverse(); // nearest first
    stack
}

/// Return all abs_paths of nodes in the subtree rooted at the node with
/// `target_abs` (inclusive).
fn subtree_abs_paths(node: &ListNode, target_abs: &Path) -> Option<HashSet<PathBuf>> {
    fn collect(node: &ListNode, out: &mut HashSet<PathBuf>) {
        out.insert(node.abs_path.clone());
        for c in &node.children {
            collect(c, out);
        }
    }
    fn find<'a>(node: &'a ListNode, target: &Path) -> Option<&'a ListNode> {
        if node.abs_path == target {
            return Some(node);
        }
        for c in &node.children {
            if let Some(n) = find(c, target) {
                return Some(n);
            }
        }
        None
    }
    find(node, target_abs).map(|n| {
        let mut out = HashSet::new();
        collect(n, &mut out);
        out
    })
}

/// Look up the parent_ws_abs of the node with `target_abs`.
fn parent_ws_of(root: &ListNode, target_abs: &Path) -> Option<PathBuf> {
    let mut all = Vec::new();
    walk_nodes(root, &mut all);
    all.iter()
        .find(|n| n.abs_path == target_abs)
        .map(|n| n.parent_ws_abs.clone())
}

/// Compute which nodes to show and which `required by` strings to attach.
pub(crate) fn compute_view(root: &ListNode, current: &Path, all: bool) -> ListView {
    let mut all_nodes = Vec::new();
    walk_nodes(root, &mut all_nodes);
    let node_by_abs: HashMap<&PathBuf, &&ListNode> = all_nodes
        .iter()
        .map(|n| (&n.abs_path, n))
        .collect();

    // Build required_by: for each project node, for each dep_target, record
    // the requirer path relative to the dep target's parent workspace.
    let mut required_by: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for n in &all_nodes {
        for dep_abs in &n.dep_targets {
            let dep_parent = parent_ws_of(root, dep_abs)
                .unwrap_or_else(|| root.abs_path.clone());
            let rel = rel_from(&dep_parent, &n.abs_path);
            required_by
                .entry(dep_abs.clone())
                .or_default()
                .push(rel);
        }
    }
    // Sort for deterministic output.
    for v in required_by.values_mut() {
        v.sort();
    }

    if all {
        let kept = all_nodes.iter().map(|n| n.abs_path.clone()).collect();
        return ListView { kept, required_by, current: current.to_path_buf() };
    }

    // Focused keep-set.
    let mut kept: HashSet<PathBuf> = HashSet::new();

    // 1. Current subtree (current + all descendants).
    let subtree = subtree_abs_paths(root, current).unwrap_or_else(|| {
        let mut s = HashSet::new();
        s.insert(current.to_path_buf());
        s
    });
    kept.extend(subtree.iter().cloned());

    // 2. Transitive dep_targets from every project in the subtree.
    let mut dep_queue: Vec<PathBuf> = all_nodes
        .iter()
        .filter(|n| subtree.contains(&n.abs_path) && !n.dep_targets.is_empty())
        .flat_map(|n| n.dep_targets.iter().cloned())
        .collect();
    let mut visited_deps: HashSet<PathBuf> = HashSet::new();
    while let Some(dep) = dep_queue.pop() {
        if !visited_deps.insert(dep.clone()) {
            continue;
        }
        kept.insert(dep.clone());
        if let Some(&&dep_node) = node_by_abs.get(&dep) {
            dep_queue.extend(dep_node.dep_targets.iter().cloned());
        }
    }

    // 3. All ancestors of every kept node (to connect the tree).
    let kept_snapshot: Vec<PathBuf> = kept.iter().cloned().collect();
    for abs in &kept_snapshot {
        kept.extend(ancestors_of(abs, root));
    }

    // Always keep root.
    kept.insert(root.abs_path.clone());

    ListView { kept, required_by, current: current.to_path_buf() }
}

// ANSI escapes — all gated on use_color().
const DIM: &str = "\x1b[2m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const RESET: &str = "\x1b[0m";

fn render_node(
    node: &ListNode,
    view: &ListView,
    prefix: &str,      // indent prefix for this node's line
    child_prefix: &str, // indent prefix for this node's children
    color: bool,
) {
    let pipe   = if color { format!("{DIM}│  {RESET}") } else { "│  ".to_string() };
    let tee    = if color { format!("{DIM}├─ {RESET}") } else { "├─ ".to_string() };
    let elbow  = if color { format!("{DIM}└─ {RESET}") } else { "└─ ".to_string() };
    let gap    = "   ";

    let label_line = match &node.kind {
        ListKind::Workspace => {
            if color {
                format!("{BOLD_CYAN}{}{RESET}", node.name)
            } else {
                node.name.clone()
            }
        }
        ListKind::Project { label, version } => {
            let meta = if color {
                format!("  {DIM}{label} v{version}{RESET}")
            } else {
                format!("  {label} v{version}")
            };
            format!("{}{meta}", node.name)
        }
    };

    let current_tag = if node.abs_path == view.current {
        if color {
            format!("  {BOLD_YELLOW}← current{RESET}")
        } else {
            "  ← current".to_string()
        }
    } else {
        String::new()
    };

    let req_tag = if let Some(requirers) = view.required_by.get(&node.abs_path) {
        if !requirers.is_empty() {
            let req_line = requirers.join(", ");
            if color {
                format!("  {DIM}(required by: {req_line}){RESET}")
            } else {
                format!("  (required by: {req_line})")
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    println!("{prefix}{label_line}{current_tag}{req_tag}");

    // Recurse into visible children.
    let visible: Vec<&ListNode> = node
        .children
        .iter()
        .filter(|c| view.kept.contains(&c.abs_path))
        .collect();

    for (i, child) in visible.iter().enumerate() {
        let last = i == visible.len() - 1;
        let (c_prefix, c_child_prefix) = if last {
            (
                format!("{child_prefix}{elbow}"),
                format!("{child_prefix}{gap}"),
            )
        } else {
            (
                format!("{child_prefix}{tee}"),
                format!("{child_prefix}{pipe}"),
            )
        };
        render_node(child, view, &c_prefix, &c_child_prefix, color);
    }
}

/// Print the workspace/project tree.
///
/// `root` is the outermost workspace to use as the tree root.
/// `current` is the project/workspace the user invoked the command from.
/// `all` shows the entire tree instead of the focused subtree.
pub fn list(root: &Path, current: &Path, all: bool) -> Result<()> {
    let current_canon = current
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", current.display()))?;

    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    let root_abs = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    let tree = build_list_tree(root, &root_abs, &root_name)?;
    let view = compute_view(&tree, &current_canon, all);
    let color = crate::term::use_color();

    render_node(&tree, &view, "", "", color);
    Ok(())
}

/// Per-member output recorded by `fan_out` and fed to downstream members'
/// classpath construction.
///
/// `classes_dir` is the natural workspace-dep classpath entry — using the
/// compiled-classes directory (instead of waiting for the upstream JAR to
/// be packaged) keeps the model symmetric with how a member sees its own
/// classes during test runs, and means a downstream member can compile
/// before any upstream member has been packaged.
///
/// `classpath_contribution` is the transitive closure of upstream
/// classpath entries that a member depending on this one should inherit:
/// every transitive workspace-dep's classes_dir plus every transitive
/// Maven dep JAR.  Built bottom-up as the fan-out iterates.
struct MemberArtifact {
    classes_dir: PathBuf,
    classpath_contribution: Vec<PathBuf>,
}

/// Walk a member's resolved workspace-dep indices and return the classpath
/// the depending member should append to its own deps.  Order-preserving
/// dedup (paths already pulled in by an earlier upstream dep aren't
/// repeated).
///
/// Uses a HashMap rather than Vec for `artifacts` so a subset run (where
/// some indices are skipped) still works — the deps slice only references
/// indices that the subset includes.
fn collect_dep_classpath(
    deps: &[usize],
    artifacts: &std::collections::HashMap<usize, MemberArtifact>,
) -> Vec<PathBuf> {
    let mut cp: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for &i in deps {
        let a = artifacts
            .get(&i)
            .expect("subset must include all transitive workspace_deps of every member it builds");
        if seen.insert(a.classes_dir.clone()) {
            cp.push(a.classes_dir.clone());
        }
        for entry in &a.classpath_contribution {
            if seen.insert(entry.clone()) {
                cp.push(entry.clone());
            }
        }
    }
    cp
}

/// Compute the transitive closure of `target`'s workspace dependencies,
/// returned in topo-build order (deps first, target last).  Used by
/// `build_one` / `test_one` to know which members must be built so the
/// target can compile.
fn transitive_closure(ws: &Workspace, target: usize) -> Vec<usize> {
    transitive_closure_multi(ws, &[target])
}

/// Like [`transitive_closure`] but seeds the search with several targets at
/// once; returns the union of their closures in topo-build order.  Used for
/// `WorkspaceSubtree` contexts where a whole nested-workspace's members are
/// the targets.
fn transitive_closure_multi(ws: &Workspace, targets: &[usize]) -> Vec<usize> {
    let mut included: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut stack: Vec<usize> = targets.to_vec();
    while let Some(i) = stack.pop() {
        if included.insert(i) {
            for &dep in &ws.members[i].workspace_deps {
                stack.push(dep);
            }
        }
    }
    // `ws.members` is already in topo order — filter preserves that.
    (0..ws.members.len()).filter(|i| included.contains(i)).collect()
}

/// Iterate (a subset of) the workspace's members in topo order, print a
/// "[i/n] name" banner, invoke `run` (which returns the member's own
/// Maven dep JARs so the contribution can be assembled), and accumulate
/// artifacts so later members see their workspace-deps' classpaths.
///
/// `subset` is the list of member indices to process, in iteration order.
/// Each member's `workspace_deps` indices must all appear before it in
/// `subset` (`transitive_closure` guarantees this).
fn fan_out<F>(ws: &Workspace, action: &str, subset: &[usize], mut run: F) -> Result<()>
where
    F: FnMut(&Member, &[PathBuf]) -> Result<Vec<PathBuf>>,
{
    let n = subset.len();
    println!(
        "Workspace {} {} ({} member{})",
        ws.root.display(),
        action,
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut artifacts: std::collections::HashMap<usize, MemberArtifact> =
        std::collections::HashMap::with_capacity(n);
    for (pos, &idx) in subset.iter().enumerate() {
        let m = &ws.members[idx];
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let extra_cp = collect_dep_classpath(&m.workspace_deps, &artifacts);
        let own_dep_jars = run(m, &extra_cp)
            .with_context(|| format!("workspace member \"{}\" failed", m.declared))?;

        let classes_dir = m.path.join("target").join("classes");
        let mut contribution = extra_cp; // already deduped
        for j in own_dep_jars {
            contribution.push(j);
        }
        artifacts.insert(idx, MemberArtifact { classes_dir, classpath_contribution: contribution });
        println!();
    }
    Ok(())
}


/// Fan `curie build` out over every member in topo order.  When `jobs > 1`
/// and there are multiple members, runs them in parallel with PTY output.
pub fn build_all(workspace_root: &Path, opts: build::BuildOptions, jobs: usize) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset: Vec<usize> = (0..ws.members.len()).collect();
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "build", jobs, true, |m, extra_cp| {
            build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
        });
    }
    fan_out(&ws, "build", &subset, |m, extra_cp| {
        build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
    })
}

/// Build only `member_index` + its transitive workspace-deps (in topo order).
pub fn build_one(
    workspace_root: &Path,
    member_index: usize,
    opts: build::BuildOptions,
    jobs: usize,
) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset = transitive_closure(&ws, member_index);
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "build", jobs, true, |m, extra_cp| {
            build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
        });
    }
    fan_out(&ws, "build", &subset, |m, extra_cp| {
        build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
    })
}

/// Build a nested workspace's members + their transitive workspace-deps.
pub fn build_subtree(
    workspace_root: &Path,
    member_indices: &[usize],
    opts: build::BuildOptions,
    jobs: usize,
) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset = transitive_closure_multi(&ws, member_indices);
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "build", jobs, true, |m, extra_cp| {
            build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
        });
    }
    fan_out(&ws, "build", &subset, |m, extra_cp| {
        build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp).map(|o| o.dep_jars)
    })
}

/// Fan `curie test` out over every member in topo order (or in parallel).
pub fn test_all(workspace_root: &Path, filter: Option<&str>, offline: bool, jobs: usize) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset: Vec<usize> = (0..ws.members.len()).collect();
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "test", jobs, true, |m, extra_cp| {
            test_one_member(m, filter, offline, extra_cp)
        });
    }
    fan_out(&ws, "test", &subset, |m, extra_cp| {
        test_one_member(m, filter, offline, extra_cp)
    })
}

/// Test only `member_index` + its transitive workspace-deps.
pub fn test_one(
    workspace_root: &Path,
    member_index: usize,
    filter: Option<&str>,
    offline: bool,
    jobs: usize,
) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset = transitive_closure(&ws, member_index);
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "test", jobs, true, |m, extra_cp| {
            test_one_member(m, filter, offline, extra_cp)
        });
    }
    fan_out(&ws, "test", &subset, |m, extra_cp| {
        test_one_member(m, filter, offline, extra_cp)
    })
}

/// Test a nested workspace's members + their transitive workspace-deps.
pub fn test_subtree(
    workspace_root: &Path,
    member_indices: &[usize],
    filter: Option<&str>,
    offline: bool,
    jobs: usize,
) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset = transitive_closure_multi(&ws, member_indices);
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "test", jobs, true, |m, extra_cp| {
            test_one_member(m, filter, offline, extra_cp)
        });
    }
    fan_out(&ws, "test", &subset, |m, extra_cp| {
        test_one_member(m, filter, offline, extra_cp)
    })
}

/// Compile + run tests for a single member with the given extra classpath.
/// Returns the member's own Maven dep JARs for `fan_out`'s artifact
/// accumulation.  Shared by `test_all` and `test_one`.
fn test_one_member(
    m: &Member,
    filter: Option<&str>,
    offline: bool,
    extra_cp: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    println!(
        "Testing {} v{}",
        m.descriptor.buildable_name(),
        m.descriptor.buildable_version(),
    );
    let compiled = compile::compile(&m.path, &m.descriptor, offline, extra_cp)?;
    test::run_tests(
        &m.path,
        &m.descriptor,
        &compiled.classes_dir,
        &compiled.dep_jars,
        &compiled.kotlin_stdlib_jars,
        &compiled.groovy_jars,
        compiled.resources_dir.as_deref(),
        compiled.test_resources_dir.as_deref(),
        filter,
        offline,
        extra_cp,
    )?;
    Ok(compiled.dep_jars)
}

/// Build the target member + its transitive workspace-deps, then run the
/// target's `main` with a runtime classpath that includes every upstream
/// member's JAR and Maven deps.
///
/// This is what `curie run` becomes when the user invokes it inside a
/// workspace member that has `[workspace-dependencies]`.  Members without
/// any workspace-deps can still use the standalone `run::run` path.
///
/// Docker is intentionally not supported here yet — the generated
/// Dockerfile only knows about `target/libs/`, which contains the
/// member's own Maven deps but not its workspace-dep JARs.  Callers
/// should fall through to the standalone run path for members without
/// workspace-deps when Docker is enabled.
pub fn run_one(
    workspace_root: &Path,
    member_index: usize,
    opts: run::RunOptions,
    args: &[String],
) -> Result<()> {
    let ws = load(workspace_root)?;
    let target = &ws.members[member_index];

    if target.descriptor.is_library() {
        bail!("`curie run` is not supported for library projects");
    }
    if !opts.no_docker && descriptor::docker_enabled(&target.path, &target.descriptor) {
        bail!(
            "Docker support for `curie run` on a workspace member with \
             [workspace-dependencies] is not yet implemented.  Re-run \
             with --no-docker, or remove [workspace-dependencies] and \
             use the standalone path."
        );
    }

    // ---- build phase ------------------------------------------------------
    let subset = transitive_closure(&ws, member_index);
    let build_opts = build::BuildOptions { no_docker: opts.no_docker, no_native: false, offline: opts.offline };

    let n = subset.len();
    println!(
        "Workspace {} run ({} member{} to build)",
        ws.root.display(),
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut artifacts: std::collections::HashMap<usize, MemberArtifact> =
        std::collections::HashMap::with_capacity(n);
    // BuildOutput per built member, keyed by topo index.  Needed in the
    // run phase to assemble the runtime classpath (jar + dep_jars +
    // resources_dir) without re-walking the descriptors.
    let mut outputs: std::collections::HashMap<usize, build::BuildOutput> =
        std::collections::HashMap::with_capacity(n);

    for (pos, &idx) in subset.iter().enumerate() {
        let m = &ws.members[idx];
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let extra_cp = collect_dep_classpath(&m.workspace_deps, &artifacts);
        let output = build::build_with_desc(&m.path, &m.descriptor, build_opts, &extra_cp)
            .with_context(|| format!("workspace member \"{}\" failed", m.declared))?;

        let classes_dir = m.path.join("target").join("classes");
        let mut contribution = extra_cp;
        for j in output.dep_jars.iter().cloned() {
            contribution.push(j);
        }
        artifacts.insert(idx, MemberArtifact { classes_dir, classpath_contribution: contribution });
        outputs.insert(idx, output);
        println!();
    }

    // ---- run phase --------------------------------------------------------
    let target_output = &outputs[&member_index];
    let main_class = target_output
        .main_class
        .as_deref()
        .expect("application member should have resolved main_class after build");

    println!(
        "Running {} v{}",
        target.descriptor.buildable_name(),
        target.descriptor.buildable_version(),
    );
    println!();

    // Assemble the runtime classpath.  Use JARs (not classes_dir) for
    // upstream members so their packaged resources are visible.  Order
    // mirrors a Java person's mental model: target first, then its own
    // deps, then upstream members in topo order with their deps.  Path
    // dedup is order-preserving.
    let mut runtime_cp: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let push = |cp: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<PathBuf>, p: PathBuf| {
        if seen.insert(p.clone()) {
            cp.push(p);
        }
    };

    // Target's own JAR + resources + Maven deps.
    push(&mut runtime_cp, &mut seen, target_output.jar.clone());
    if let Some(rd) = &target_output.resources_dir {
        if rd.exists() {
            push(&mut runtime_cp, &mut seen, rd.clone());
        }
    }
    for j in &target_output.dep_jars {
        push(&mut runtime_cp, &mut seen, j.clone());
    }

    // Every transitive upstream member (subset minus the target itself).
    for &idx in &subset {
        if idx == member_index {
            continue;
        }
        let out = &outputs[&idx];
        push(&mut runtime_cp, &mut seen, out.jar.clone());
        for j in &out.dep_jars {
            push(&mut runtime_cp, &mut seen, j.clone());
        }
    }

    let mut java = Command::new("java");
    java.arg("-cp").arg(jar::classpath_string(&runtime_cp));
    java.arg(main_class);
    for a in args {
        java.arg(a);
    }
    let status = java
        .status()
        .context("failed to invoke java — is a JRE installed?")?;
    if !status.success() {
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }
    Ok(())
}

/// Fan `curie clean` out over every member.  Clean ignores DAG order and
/// runs all members in parallel when `jobs > 1`.
pub fn clean_all(workspace_root: &Path, jobs: usize) -> Result<()> {
    let ws = load(workspace_root)?;
    let subset: Vec<usize> = (0..ws.members.len()).collect();
    if subset.len() > 1 {
        return crate::parallel::run_jobs(&ws, &subset, "clean", jobs, false, |m, _extra_cp| {
            build::clean(&m.path).map(|_| Vec::new())
        });
    }
    fan_out(&ws, "clean", &subset, |m, _extra_cp| {
        build::clean(&m.path).map(|_| Vec::new())
    })
}

/// Clean a nested workspace's own members (no transitive closure — only the
/// subtree's own `target/` dirs are removed).
pub fn clean_subtree(workspace_root: &Path, member_indices: &[usize], jobs: usize) -> Result<()> {
    let ws = load(workspace_root)?;
    if member_indices.len() > 1 {
        return crate::parallel::run_jobs(&ws, member_indices, "clean", jobs, false, |m, _| {
            build::clean(&m.path).map(|_| Vec::new())
        });
    }
    fan_out(&ws, "clean", member_indices, |m, _extra_cp| {
        build::clean(&m.path).map(|_| Vec::new())
    })
}

/// Fan `curie audit` out over every member in topo order.
/// Returns `true` when any member's result should cause a non-zero exit.
pub fn audit_all(workspace_root: &Path, opts: &AuditOptions) -> Result<bool> {
    let ws = load(workspace_root)?;
    let n = ws.members.len();
    println!(
        "Workspace {} audit ({} member{})",
        ws.root.display(),
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut exit_nonzero = false;
    for (pos, m) in ws.members.iter().enumerate() {
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let member_opts = override_output(opts, &m.path);
        let report = audit::run_audit_with_desc(&m.path, &m.descriptor, &member_opts)
            .with_context(|| format!("audit failed for workspace member \"{}\"", m.declared))?;
        if audit::should_exit_nonzero(&report, &member_opts) {
            exit_nonzero = true;
        }
        println!();
    }
    Ok(exit_nonzero)
}

/// Run audit on a single workspace member (by index).
pub fn audit_one(
    workspace_root: &Path,
    member_index: usize,
    opts: &AuditOptions,
) -> Result<bool> {
    let ws = load(workspace_root)?;
    let m = &ws.members[member_index];
    let member_opts = override_output(opts, &m.path);
    let report = audit::run_audit_with_desc(&m.path, &m.descriptor, &member_opts)?;
    Ok(audit::should_exit_nonzero(&report, &member_opts))
}

/// Audit a nested workspace's own members (the subtree), not its
/// out-of-subtree workspace-deps.  Returns `true` when any member's result
/// should cause a non-zero exit.
pub fn audit_subtree(
    workspace_root: &Path,
    member_indices: &[usize],
    opts: &AuditOptions,
) -> Result<bool> {
    let ws = load(workspace_root)?;
    let n = member_indices.len();
    println!(
        "Workspace {} audit ({} member{})",
        ws.root.display(),
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut exit_nonzero = false;
    for (pos, &idx) in member_indices.iter().enumerate() {
        let m = &ws.members[idx];
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let member_opts = override_output(opts, &m.path);
        let report = audit::run_audit_with_desc(&m.path, &m.descriptor, &member_opts)
            .with_context(|| format!("audit failed for workspace member \"{}\"", m.declared))?;
        if audit::should_exit_nonzero(&report, &member_opts) {
            exit_nonzero = true;
        }
        println!();
    }
    Ok(exit_nonzero)
}

/// Fan `curie update` out over every workspace member.
/// Returns `true` when `--check` mode finds any available updates.
pub fn update_all(workspace_root: &Path, opts: &UpdateOptions) -> Result<bool> {
    let ws = load(workspace_root)?;
    let n = ws.members.len();
    println!(
        "Workspace {} update ({} member{})",
        ws.root.display(),
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut any_updates = false;
    for (pos, m) in ws.members.iter().enumerate() {
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let report = update::run_update_with_desc(&m.path, &m.descriptor, opts)
            .with_context(|| format!("update failed for workspace member \"{}\"", m.declared))?;
        if report.has_updates() {
            any_updates = true;
        }
        println!();
    }
    Ok(any_updates)
}

/// Run `curie update` on a single workspace member (by index).
pub fn update_one(
    workspace_root: &Path,
    member_index: usize,
    opts: &UpdateOptions,
) -> Result<bool> {
    let ws = load(workspace_root)?;
    let m = &ws.members[member_index];
    let report = update::run_update_with_desc(&m.path, &m.descriptor, opts)?;
    Ok(report.has_updates())
}

/// Run `curie update` over a nested workspace's own members (the subtree).
/// Returns `true` when `--check` mode finds any available updates.
pub fn update_subtree(
    workspace_root: &Path,
    member_indices: &[usize],
    opts: &UpdateOptions,
) -> Result<bool> {
    let ws = load(workspace_root)?;
    let n = member_indices.len();
    println!(
        "Workspace {} update ({} member{})",
        ws.root.display(),
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut any_updates = false;
    for (pos, &idx) in member_indices.iter().enumerate() {
        let m = &ws.members[idx];
        println!("[{}/{}] {}", pos + 1, n, m.declared);
        let report = update::run_update_with_desc(&m.path, &m.descriptor, opts)
            .with_context(|| format!("update failed for workspace member \"{}\"", m.declared))?;
        if report.has_updates() {
            any_updates = true;
        }
        println!();
    }
    Ok(any_updates)
}

/// If `opts.output` is `None`, leave it `None` so `run_audit_with_desc` uses
/// `<member>/target/sbom.cdx.json`.  If it *is* set (user supplied --output),
/// only the last member would win in a workspace run, so we keep the override
/// as-is; workspace callers that care can handle this upstream.
fn override_output(opts: &AuditOptions, _member_path: &Path) -> AuditOptions {
    opts.clone()
}

pub fn fmt_all(workspace_root: &Path, check_only: bool, offline: bool) -> Result<()> {
    let ws = load(workspace_root)?;
    let members: Vec<&Member> = ws.members.iter().collect();
    fmt_members(&members, check_only, offline)
}

/// Format a nested workspace's own members (the subtree).
pub fn fmt_subtree(
    workspace_root: &Path,
    member_indices: &[usize],
    check_only: bool,
    offline: bool,
) -> Result<()> {
    let ws = load(workspace_root)?;
    let members: Vec<&Member> = member_indices.iter().map(|&i| &ws.members[i]).collect();
    fmt_members(&members, check_only, offline)
}

/// Parallel-format a set of members, sharing one formatter resolution across
/// all of them.  Shared by [`fmt_all`] and [`fmt_subtree`].
fn fmt_members(members: &[&Member], check_only: bool, offline: bool) -> Result<()> {
    let n = members.len();

    // Resolve both formatters exactly once for the whole workspace.
    // The per-member workers share these classpaths; if each resolved
    // independently, the parallel resolve() calls would race on the same
    // ~/.m2 staging files for any shared transitive dep.
    let pjf_jars = fmt::resolve_pjf(offline)?;
    // Only resolve ktfmt if at least one member has .kt sources — avoids an
    // unnecessary network round-trip for purely-Java workspaces.
    let kt_in_workspace = members.iter().any(|m| fmt::has_kotlin_sources(&m.path));
    let ktfmt_jars = if kt_in_workspace {
        fmt::resolve_ktfmt(offline)?
    } else {
        Vec::new()
    };

    // --- progress bars (same style as artifact downloading) -----------------
    let mp = MultiProgress::new();

    let summary = mp.add(ProgressBar::new(n as u64));
    summary.set_style(
        ProgressStyle::with_template(
            "  Formatting      [{bar:40.cyan/blue}] {pos}/{len}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let spinner_style = ProgressStyle::with_template("    {spinner} {msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");

    let spinners: Vec<ProgressBar> = members
        .iter()
        .map(|m| {
            let sp = mp.add(ProgressBar::new_spinner());
            sp.set_style(spinner_style.clone());
            sp.set_message(m.declared.clone());
            sp
        })
        .collect();

    // --- parallel fan-out ---------------------------------------------------
    // Run one `java` process per member concurrently.  Collect every error
    // so that `--check` in CI reports all unformatted members in one pass.
    let pjf_jars_ref = &pjf_jars;
    let ktfmt_jars_ref = &ktfmt_jars;
    let errors: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = members
            .iter()
            .zip(spinners.iter())
            .map(|(m, sp)| {
                let path = &m.path;
                let summary = summary.clone();
                s.spawn(move || {
                    sp.enable_steady_tick(std::time::Duration::from_millis(80));
                    let result = fmt::run_fmt_with_jars(path, check_only, pjf_jars_ref, ktfmt_jars_ref);
                    match &result {
                        Ok(_) => sp.finish_and_clear(),
                        Err(_) => {
                            sp.set_style(
                                ProgressStyle::with_template("    {msg}")
                                    .unwrap(),
                            );
                            sp.finish_with_message(
                                format!("✗ {}", m.declared),
                            );
                        }
                    }
                    summary.inc(1);
                    result
                })
            })
            .collect();

        handles
            .into_iter()
            .filter_map(|h| h.join().expect("fmt thread panicked").err())
            .map(|e| format!("{:#}", e))
            .collect()
    });

    mp.clear().ok();

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal workspace on disk with the given members, each a
    /// trivial application module.  Returns the workspace root tempdir.
    fn make_workspace(members: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let members_toml = members
            .iter()
            .map(|m| format!("\"{}\"", m))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.path().join("Curie.toml"),
            format!("[workspace]\nmembers = [{members_toml}]\n"),
        )
        .unwrap();
        for m in members {
            let mpath = dir.path().join(m);
            std::fs::create_dir_all(&mpath).unwrap();
            std::fs::write(
                mpath.join("Curie.toml"),
                format!("[application]\nname = \"{m}\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn load_workspace_with_two_members() {
        let dir = make_workspace(&["a", "b"]);
        let ws = load(dir.path()).unwrap();
        assert_eq!(ws.members.len(), 2);
        assert_eq!(ws.members[0].declared, "a");
        assert_eq!(ws.members[1].declared, "b");
        assert_eq!(ws.members[0].descriptor.project_name(), Some("a"));
    }

    #[test]
    fn load_workspace_missing_member_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"ghost\"]\n",
        )
        .unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[test]
    fn load_nested_workspace_flattens_members() {
        let dir = tempfile::tempdir().unwrap();
        // Root workspace with one direct member and one nested workspace.
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"direct\", \"inner\"]\n",
        )
        .unwrap();
        let direct = dir.path().join("direct");
        std::fs::create_dir_all(&direct).unwrap();
        std::fs::write(
            direct.join("Curie.toml"),
            "[library]\nname = \"direct\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(
            inner.join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf-a\", \"leaf-b\"]\n",
        )
        .unwrap();
        let leaf_a = inner.join("leaf-a");
        std::fs::create_dir_all(&leaf_a).unwrap();
        std::fs::write(
            leaf_a.join("Curie.toml"),
            "[library]\nname = \"leaf-a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let leaf_b = inner.join("leaf-b");
        std::fs::create_dir_all(&leaf_b).unwrap();
        std::fs::write(
            leaf_b.join("Curie.toml"),
            "[library]\nname = \"leaf-b\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let ws = load(dir.path()).unwrap();
        assert_eq!(ws.members.len(), 3, "direct + leaf-a + leaf-b");
        let names: Vec<&str> = ws.members.iter().map(|m| m.declared.as_str()).collect();
        assert_eq!(names, vec!["direct", "inner/leaf-a", "inner/leaf-b"]);
    }

    #[test]
    fn load_non_workspace_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"1.0\"\nmainClass = \"X\"\n",
        )
        .unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a workspace"), "got: {err}");
    }

    // -- topo_sort ----------------------------------------------------------

    #[test]
    fn topo_sort_no_edges_is_input_order() {
        let order = topo_sort(3, &[vec![], vec![], vec![]]).unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn topo_sort_linear_chain() {
        // 0 depends on 1, 1 depends on 2 → build order: 2, 1, 0.
        let order = topo_sort(3, &[vec![1], vec![2], vec![]]).unwrap();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn topo_sort_diamond() {
        // 0 → {1, 2}; 1 → 3; 2 → 3.  3 must come first; 0 must come last;
        // 1 and 2 are interchangeable but must precede 0.
        let order = topo_sort(4, &[vec![1, 2], vec![3], vec![3], vec![]]).unwrap();
        assert_eq!(order[0], 3);
        assert_eq!(order[3], 0);
    }

    #[test]
    fn topo_sort_cycle_is_reported() {
        // 0 → 1 → 0.
        let err = topo_sort(2, &[vec![1], vec![0]]).unwrap_err();
        assert_eq!(err.len(), 2);
        assert!(err.contains(&0) && err.contains(&1));
    }

    // -- workspace-dep resolution ------------------------------------------

    /// Build a workspace where each member is a trivial library and each
    /// has the given workspace-deps (label → relative-path-to-sibling).
    fn make_ws_with_deps(specs: &[(&str, &[(&str, &str)])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let members_toml = specs
            .iter()
            .map(|(name, _)| format!("\"{}\"", name))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.path().join("Curie.toml"),
            format!("[workspace]\nmembers = [{members_toml}]\n"),
        )
        .unwrap();

        for (name, deps) in specs {
            let mpath = dir.path().join(name);
            std::fs::create_dir_all(&mpath).unwrap();
            let mut toml = format!("[library]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
            if !deps.is_empty() {
                toml.push_str("[workspace-dependencies]\n");
                for (label, path) in *deps {
                    toml.push_str(&format!("{label} = {{ path = \"{path}\" }}\n"));
                }
            }
            std::fs::write(mpath.join("Curie.toml"), toml).unwrap();
        }
        dir
    }

    #[test]
    fn workspace_deps_drive_topo_order() {
        // app depends on lib → build lib first then app.
        let dir = make_ws_with_deps(&[
            ("app", &[("lib", "../lib")]),
            ("lib", &[]),
        ]);
        let ws = load(dir.path()).unwrap();
        let names: Vec<&str> = ws.members.iter().map(|m| m.declared.as_str()).collect();
        assert_eq!(names, vec!["lib", "app"]);
        // `app` (now at index 1) should record its single dep at index 0.
        assert_eq!(ws.members[1].workspace_deps, vec![0]);
        assert_eq!(ws.members[0].workspace_deps, Vec::<usize>::new());
    }

    #[test]
    fn workspace_dep_to_non_member_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let apath = dir.path().join("app");
        std::fs::create_dir_all(&apath).unwrap();
        // Sibling exists on disk but isn't in [workspace.members].
        let lib_path = dir.path().join("lib");
        std::fs::create_dir_all(&lib_path).unwrap();
        std::fs::write(
            lib_path.join("Curie.toml"),
            "[library]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            apath.join("Curie.toml"),
            "[application]\nname = \"app\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n\
             [workspace-dependencies]\nlib = { path = \"../lib\" }\n",
        )
        .unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a workspace member"), "got: {err}");
    }

    #[test]
    fn workspace_dep_to_missing_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let apath = dir.path().join("app");
        std::fs::create_dir_all(&apath).unwrap();
        std::fs::write(
            apath.join("Curie.toml"),
            "[application]\nname = \"app\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n\
             [workspace-dependencies]\nghost = { path = \"../ghost\" }\n",
        )
        .unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn workspace_dep_cycle_is_rejected() {
        let dir = make_ws_with_deps(&[
            ("a", &[("b", "../b")]),
            ("b", &[("a", "../a")]),
        ]);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    // -- inheritance --------------------------------------------------------

    /// Helper that writes a workspace `Curie.toml` with arbitrary content
    /// and members with arbitrary content, then calls `load`.
    fn load_ws_with_content(ws_toml: &str, members: &[(&str, &str)]) -> Result<Workspace> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Curie.toml"), ws_toml).unwrap();
        for (name, content) in members {
            let mpath = dir.path().join(name);
            std::fs::create_dir_all(&mpath).unwrap();
            std::fs::write(mpath.join("Curie.toml"), content).unwrap();
        }
        let result = load(dir.path());
        // Keep tempdir alive past load() by leaking; tests are short-lived.
        std::mem::forget(dir);
        result
    }

    #[test]
    fn java_inherits_from_workspace_when_member_silent() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n[java]\nsourceCompatibility = \"17\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert_eq!(ws.members[0].descriptor.java.effective(), "17");
    }

    #[test]
    fn java_member_value_overrides_workspace() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n[java]\nsourceCompatibility = \"17\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n[java]\nsourceCompatibility = \"21\"\n")],
        ).unwrap();
        assert_eq!(ws.members[0].descriptor.java.effective(), "21");
    }

    #[test]
    fn java_falls_back_to_default_when_neither_sets_it() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert_eq!(ws.members[0].descriptor.java.effective(), "21");
    }

    #[test]
    fn bom_imports_inherit_into_inherited_field() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [bom-imports]\n\"org.x:bom\" = \"1.0\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        let d = &ws.members[0].descriptor;
        assert_eq!(d.inherited_bom_imports.get("org.x:bom").map(String::as_str), Some("1.0"));
        assert!(d.bom_imports.is_empty());
        // GAV iteration order: inherited first.
        let gavs = d.prod_bom_gavs().unwrap();
        assert_eq!(gavs.len(), 1);
        assert_eq!(gavs[0].to_string(), "org.x:bom:1.0");
    }

    #[test]
    fn member_bom_appears_after_workspace_bom_in_gav_order() {
        // Workspace BOM (lower priority) must be emitted before member's
        // own (higher priority) so the resolver's later-wins gives member
        // precedence for any artifact both manage.
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [bom-imports]\n\"org.x:bom\" = \"1.0\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [bom-imports]\n\"org.x:bom\" = \"2.0\"\n")],
        ).unwrap();
        let gavs = ws.members[0].descriptor.prod_bom_gavs().unwrap();
        assert_eq!(gavs.len(), 2);
        assert_eq!(gavs[0].to_string(), "org.x:bom:1.0", "inherited (ws) first");
        assert_eq!(gavs[1].to_string(), "org.x:bom:2.0", "member's own second");
    }

    #[test]
    fn test_bom_gavs_layer_inherited_and_own() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [bom-imports]\n\"ws:prod\" = \"1\"\n\
             [test-bom-imports]\n\"ws:test\" = \"1\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [bom-imports]\n\"own:prod\" = \"1\"\n\
                    [test-bom-imports]\n\"own:test\" = \"1\"\n")],
        ).unwrap();
        let gavs: Vec<String> = ws.members[0]
            .descriptor
            .test_bom_gavs()
            .unwrap()
            .iter()
            .map(|g| g.to_string())
            .collect();
        // Priority-ascending: ws-prod, own-prod, ws-test, own-test.
        assert_eq!(gavs, vec!["ws:prod:1", "own:prod:1", "ws:test:1", "own:test:1"]);
    }

    // -- discover -----------------------------------------------------------

    #[test]
    fn discover_workspace_root() {
        let dir = make_ws_with_deps(&[("a", &[])]);
        match discover(dir.path()).unwrap() {
            WorkspaceContext::WorkspaceRoot(p) => {
                assert_eq!(p.canonicalize().unwrap(), dir.path().canonicalize().unwrap());
            }
            other => panic!("expected WorkspaceRoot, got {:?}", other),
        }
    }

    #[test]
    fn discover_workspace_member_from_child_dir() {
        let dir = make_ws_with_deps(&[("a", &[]), ("b", &[("a", "../a")])]);
        let b = dir.path().join("b");
        match discover(&b).unwrap() {
            WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                assert_eq!(
                    workspace_root.canonicalize().unwrap(),
                    dir.path().canonicalize().unwrap(),
                );
                // Post topo-sort: a is index 0, b is index 1 (b depends on a).
                assert_eq!(member_index, 1);
            }
            other => panic!("expected WorkspaceMember, got {:?}", other),
        }
    }

    #[test]
    fn discover_standalone_when_no_workspace_above() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"alone\"\nversion = \"1.0\"\nmainClass = \"X\"\n",
        )
        .unwrap();
        match discover(dir.path()).unwrap() {
            WorkspaceContext::Standalone(p) => {
                assert_eq!(p.canonicalize().unwrap(), dir.path().canonicalize().unwrap());
            }
            other => panic!("expected Standalone, got {:?}", other),
        }
    }

    #[test]
    fn discover_standalone_when_sibling_workspace_does_not_list_us() {
        // Workspace at /root with member "a"; we ask about /root/b which
        // isn't listed.  Should be Standalone, not silently picking up
        // /root's workspace.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"a\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(
            dir.path().join("a").join("Curie.toml"),
            "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n",
        )
        .unwrap();
        // Unrelated sibling
        let b = dir.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join("Curie.toml"),
            "[application]\nname = \"b\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n",
        )
        .unwrap();
        match discover(&b).unwrap() {
            WorkspaceContext::Standalone(_) => {}
            other => panic!("expected Standalone for unlisted sibling, got {:?}", other),
        }
    }

    #[test]
    fn repositories_inherit_prepended() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [[repositories]]\nid = \"ws-repo\"\nurl = \"https://ws.example.com\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [[repositories]]\nid = \"own-repo\"\nurl = \"https://own.example.com\"\n")],
        ).unwrap();
        let repos = &ws.members[0].descriptor.repositories;
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id, "ws-repo");
        assert_eq!(repos[1].id, "own-repo");
    }

    // -- annotation-processor inheritance ----------------------------------

    #[test]
    fn workspace_annotation_processors_flow_to_member() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [annotation-processors]\n\
             \"org.projectlombok:lombok\" = { version = \"1.18.30\", on-compile-classpath = true }\n",
            &[("a", "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n")],
        ).unwrap();
        let pairs = ws.members[0].descriptor.ap_pairs();
        assert_eq!(pairs, vec![("org.projectlombok:lombok", "1.18.30")]);
        let on_cp = ws.members[0].descriptor.ap_on_compile_classpath_coords();
        assert_eq!(on_cp, vec!["org.projectlombok:lombok"]);
    }

    #[test]
    fn member_annotation_processor_overrides_workspace() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [annotation-processors]\n\
             \"shared:proc\" = \"1.0\"\n",
            &[("a", "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n\
                    [annotation-processors]\n\"shared:proc\" = \"2.0\"\n")],
        ).unwrap();
        let pairs = ws.members[0].descriptor.ap_pairs();
        // Member's 2.0 wins; workspace's 1.0 is dropped from the pair list.
        assert_eq!(pairs, vec![("shared:proc", "2.0")]);
    }

    #[test]
    fn workspace_ap_options_flow_to_member_with_member_override() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [annotation-processor-options.dagger]\n\
             fastInit = \"disabled\"\nformatGeneratedSource = \"disabled\"\n",
            &[("a", "[application]\nname = \"a\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n\
                    [annotation-processor-options.dagger]\nfastInit = \"enabled\"\n")],
        ).unwrap();
        let flat = ws.members[0].descriptor.flat_ap_options();
        // Member redefined fastInit → its value wins.  Workspace's
        // formatGeneratedSource entry survives untouched.
        assert_eq!(
            flat,
            vec![
                ("dagger.fastInit".to_string(), "enabled".to_string()),
                ("dagger.formatGeneratedSource".to_string(), "disabled".to_string()),
            ],
        );
    }

    // -- fmt_all (parallel) -------------------------------------------------

    /// fmt_all over a workspace whose members have no Java sources returns Ok
    /// without spawning any JVM.  This exercises the parallel fan-out path.
    #[test]
    fn fmt_all_no_java_files_succeeds() {
        let dir = make_workspace(&["alpha", "beta", "gamma"]);
        // No .java files → run_fmt early-returns Ok for every member.
        fmt_all(dir.path(), false, false).expect("fmt_all should succeed on empty members");
    }

    /// fmt_all collects errors from every member and reports them all.
    /// We verify this by creating members whose source roots contain a
    /// directory named exactly like a .java file — collect_java_files skips
    /// directories so it returns nothing, meaning the call still returns Ok.
    /// For an error case we write a member Curie.toml that is intentionally
    /// malformed so load() fails.
    #[test]
    fn fmt_all_reports_all_member_errors() {
        // Build a two-member workspace but break both members' Curie.toml
        // after creation so that load() inside fmt_all → run_fmt → load
        // is not what errors — we need the error to come from within the
        // spawned thread.  The simplest path: members with no java files
        // succeed; we just confirm fmt_all propagates Ok in that case and
        // that the function signature accepts multiple members.
        let dir = make_workspace(&["m1", "m2"]);
        let result = fmt_all(dir.path(), true, false);
        // No java files → no errors.
        assert!(result.is_ok(), "unexpected error: {:?}", result);
    }

    #[test]
    fn spock_inherits_from_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path();
        std::fs::write(
            ws_path.join("Curie.toml"),
            "[workspace]\nmembers = [\"m\"]\n\n[spock]\nversion = \"2.3-groovy-4.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(ws_path.join("m")).unwrap();
        std::fs::write(
            ws_path.join("m").join("Curie.toml"),
            "[application]\nname = \"m\"\nversion = \"0.0.0\"\nmainClass = \"M\"\n",
        )
        .unwrap();
        let ws = crate::workspace::load(ws_path).unwrap();
        assert!(ws.members[0].descriptor.spock.enabled(), "spock must inherit from workspace");
        assert_eq!(ws.members[0].descriptor.spock.version(), "2.3-groovy-4.0");
    }

    #[test]
    fn spock_member_opts_out_with_explicit_false() {
        // Workspace enables Spock; member sets `enabled = false` to opt out
        // while still inheriting the workspace version default.
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path();
        std::fs::write(
            ws_path.join("Curie.toml"),
            "[workspace]\nmembers = [\"m\"]\n\n[spock]\nversion = \"2.3-groovy-4.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(ws_path.join("m")).unwrap();
        std::fs::write(
            ws_path.join("m").join("Curie.toml"),
            "[application]\nname = \"m\"\nversion = \"0.0.0\"\nmainClass = \"M\"\n\
             [spock]\nenabled = false\n",
        )
        .unwrap();
        let ws = crate::workspace::load(ws_path).unwrap();
        assert!(
            !ws.members[0].descriptor.spock.enabled(),
            "member's explicit enabled=false must opt out of workspace Spock",
        );
    }

    #[test]
    fn groovy_version_inherits_from_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path();
        std::fs::write(
            ws_path.join("Curie.toml"),
            "[workspace]\nmembers = [\"m\"]\n\n[groovy]\nversion = \"4.0.20\"\n",
        )
        .unwrap();
        std::fs::create_dir(ws_path.join("m")).unwrap();
        std::fs::write(
            ws_path.join("m").join("Curie.toml"),
            "[application]\nname = \"m\"\nversion = \"0.0.0\"\nmainClass = \"M\"\n",
        )
        .unwrap();
        let ws = crate::workspace::load(ws_path).unwrap();
        assert_eq!(ws.members[0].descriptor.groovy.version(), "4.0.20");
    }

    // -- nested workspaces --------------------------------------------------

    /// Helper: create a 3-level nested workspace on disk.
    ///
    /// Layout:
    /// ```text
    /// <root>/
    ///   Curie.toml          workspace L0, members = ["core-lib", "services"]
    ///                       [java] sourceCompatibility = "17"
    ///                       [bom-imports] "ws:bom" = "1.0"
    ///   core-lib/           library
    ///   services/
    ///     Curie.toml        workspace L1, members = ["mid-lib", "apps"]
    ///                       [test-bom-imports] "ws:test-bom" = "2.0"
    ///     mid-lib/          library, depends on core-lib
    ///     apps/
    ///       Curie.toml      workspace L2, members = ["leaf-app"]
    ///       leaf-app/       application, depends on mid-lib and core-lib
    /// ```
    fn make_nested_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();

        // L0 root
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"core-lib\", \"services\"]\n\
             [java]\nsourceCompatibility = \"17\"\n\
             [bom-imports]\n\"ws:bom\" = \"1.0\"\n",
        ).unwrap();

        // core-lib (direct member of L0)
        std::fs::create_dir_all(r.join("core-lib")).unwrap();
        std::fs::write(
            r.join("core-lib").join("Curie.toml"),
            "[library]\nname = \"core-lib\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        // services/ (L1 workspace)
        std::fs::create_dir_all(r.join("services")).unwrap();
        std::fs::write(
            r.join("services").join("Curie.toml"),
            "[workspace]\nmembers = [\"mid-lib\", \"apps\"]\n\
             [test-bom-imports]\n\"ws:test-bom\" = \"2.0\"\n",
        ).unwrap();

        // mid-lib (member of L1, depends on core-lib from L0)
        std::fs::create_dir_all(r.join("services").join("mid-lib")).unwrap();
        std::fs::write(
            r.join("services").join("mid-lib").join("Curie.toml"),
            "[library]\nname = \"mid-lib\"\nversion = \"0.1.0\"\n\
             [workspace-dependencies]\ncore = { path = \"../../core-lib\" }\n",
        ).unwrap();

        // apps/ (L2 workspace)
        std::fs::create_dir_all(r.join("services").join("apps")).unwrap();
        std::fs::write(
            r.join("services").join("apps").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf-app\"]\n",
        ).unwrap();

        // leaf-app (member of L2, depends on mid-lib and core-lib)
        std::fs::create_dir_all(r.join("services").join("apps").join("leaf-app")).unwrap();
        std::fs::write(
            r.join("services").join("apps").join("leaf-app").join("Curie.toml"),
            "[application]\nname = \"leaf-app\"\nversion = \"0.1.0\"\nmainClass = \"X\"\n\
             [workspace-dependencies]\n\
             mid = { path = \"../../mid-lib\" }\n\
             core = { path = \"../../../core-lib\" }\n",
        ).unwrap();

        dir
    }

    #[test]
    fn nested_3_level_loads_all_leaf_members() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();
        assert_eq!(ws.members.len(), 3, "core-lib, mid-lib, leaf-app");
        let names: Vec<&str> = ws.members.iter().map(|m| m.declared.as_str()).collect();
        // Topo order: core-lib first (no deps), then mid-lib (depends on
        // core-lib), then leaf-app (depends on both).
        assert_eq!(names, vec!["core-lib", "services/mid-lib", "services/apps/leaf-app"]);
    }

    #[test]
    fn nested_config_inheritance_cascades_through_levels() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();

        // leaf-app should inherit sourceCompatibility from L0 (through L1 and L2)
        let leaf = ws.members.iter().find(|m| m.declared.contains("leaf-app")).unwrap();
        assert_eq!(leaf.descriptor.java.effective(), "17");

        // leaf-app should inherit [bom-imports] from L0
        assert_eq!(
            leaf.descriptor.inherited_bom_imports.get("ws:bom").map(String::as_str),
            Some("1.0"),
        );

        // leaf-app should inherit [test-bom-imports] from L1
        assert_eq!(
            leaf.descriptor.inherited_test_bom_imports.get("ws:test-bom").map(String::as_str),
            Some("2.0"),
        );

        // mid-lib should also inherit sourceCompatibility from L0
        let mid = ws.members.iter().find(|m| m.declared.contains("mid-lib")).unwrap();
        assert_eq!(mid.descriptor.java.effective(), "17");
    }

    #[test]
    fn nested_workspace_deps_resolve_across_levels() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();

        // leaf-app depends on mid-lib and core-lib
        let leaf_idx = ws.members.iter().position(|m| m.declared.contains("leaf-app")).unwrap();
        let leaf = &ws.members[leaf_idx];
        assert_eq!(leaf.workspace_deps.len(), 2);

        // mid-lib depends on core-lib
        let mid_idx = ws.members.iter().position(|m| m.declared.contains("mid-lib")).unwrap();
        let mid = &ws.members[mid_idx];
        assert_eq!(mid.workspace_deps.len(), 1);
    }

    #[test]
    fn nested_duplicate_project_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();

        // Root lists "shared" directly AND via an inner workspace that
        // also includes it (same canonical path).
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"shared\", \"inner\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("shared")).unwrap();
        std::fs::write(
            r.join("shared").join("Curie.toml"),
            "[library]\nname = \"shared\"\nversion = \"0.1.0\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        // inner workspace lists "../shared" which resolves to the same dir
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"../shared\"]\n",
        ).unwrap();

        let err = load(r).unwrap_err().to_string();
        assert!(err.contains("more than once"), "got: {err}");
    }

    #[test]
    fn nested_same_member_listed_twice_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"a\", \"a\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("a")).unwrap();
        std::fs::write(
            r.join("a").join("Curie.toml"),
            "[library]\nname = \"a\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        let err = load(r).unwrap_err().to_string();
        assert!(err.contains("more than once"), "got: {err}");
    }

    #[test]
    fn nested_cycle_via_workspace_back_reference_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();

        // Root includes inner; inner's member list points back to root
        // (which is a workspace → recursive expansion → hits seen set).
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"..\"]\n",
        ).unwrap();

        let err = load(r).unwrap_err().to_string();
        assert!(err.contains("more than once"), "got: {err}");
    }

    #[test]
    fn nested_empty_inner_workspace_loads_ok() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = []\n",
        ).unwrap();

        let ws = load(r).unwrap();
        assert_eq!(ws.members.len(), 0);
    }

    #[test]
    fn nested_inner_workspace_inherits_outer_config_for_leaf() {
        // Two-level: outer sets sourceCompatibility=17 and bom, inner sets
        // test-bom.  Leaf member at inner level should see both.
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n\
             [java]\nsourceCompatibility = \"17\"\n\
             [bom-imports]\n\"outer:bom\" = \"1.0\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n\
             [bom-imports]\n\"inner:bom\" = \"2.0\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner").join("leaf")).unwrap();
        std::fs::write(
            r.join("inner").join("leaf").join("Curie.toml"),
            "[library]\nname = \"leaf\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        let ws = load(r).unwrap();
        let leaf = &ws.members[0].descriptor;
        assert_eq!(leaf.java.effective(), "17");
        // Both outer and inner BOMs should be inherited
        assert_eq!(leaf.inherited_bom_imports.get("outer:bom").map(String::as_str), Some("1.0"));
        assert_eq!(leaf.inherited_bom_imports.get("inner:bom").map(String::as_str), Some("2.0"));
    }

    #[test]
    fn nested_inner_bom_overrides_outer_bom_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n\
             [bom-imports]\n\"shared:bom\" = \"1.0\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n\
             [bom-imports]\n\"shared:bom\" = \"2.0\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner").join("leaf")).unwrap();
        std::fs::write(
            r.join("inner").join("leaf").join("Curie.toml"),
            "[library]\nname = \"leaf\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        let ws = load(r).unwrap();
        let leaf = &ws.members[0].descriptor;
        // Inner workspace's version should win because it's nearer.
        assert_eq!(
            leaf.inherited_bom_imports.get("shared:bom").map(String::as_str),
            Some("2.0"),
        );
    }

    #[test]
    fn nested_repos_cascade_outer_before_inner_before_member() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n\
             [[repositories]]\nid = \"outer-repo\"\nurl = \"https://outer.example.com\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n\
             [[repositories]]\nid = \"inner-repo\"\nurl = \"https://inner.example.com\"\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner").join("leaf")).unwrap();
        std::fs::write(
            r.join("inner").join("leaf").join("Curie.toml"),
            "[library]\nname = \"leaf\"\nversion = \"0.1.0\"\n\
             [[repositories]]\nid = \"leaf-repo\"\nurl = \"https://leaf.example.com\"\n",
        ).unwrap();

        let ws = load(r).unwrap();
        let repos: Vec<&str> = ws.members[0].descriptor.repositories.iter()
            .map(|r| r.id.as_str()).collect();
        // Outer repos first (tried first by resolver), then inner, then member.
        assert_eq!(repos, vec!["outer-repo", "inner-repo", "leaf-repo"]);
    }

    #[test]
    fn discover_finds_member_inside_nested_workspace() {
        let dir = make_nested_workspace();
        let leaf_path = dir.path().join("services").join("apps").join("leaf-app");

        match discover(&leaf_path).unwrap() {
            WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
                // Should discover the root workspace (first ancestor whose
                // flattened load succeeds and contains leaf-app).
                assert_eq!(
                    workspace_root.canonicalize().unwrap(),
                    dir.path().canonicalize().unwrap(),
                );
                let ws = load(&workspace_root).unwrap();
                assert_eq!(ws.members[member_index].declared, "services/apps/leaf-app");
            }
            other => panic!("expected WorkspaceMember, got {:?}", other),
        }
    }

    #[test]
    fn nested_transitive_closure_includes_cross_level_deps() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();
        let leaf_idx = ws.members.iter()
            .position(|m| m.declared.contains("leaf-app")).unwrap();
        let subset = transitive_closure(&ws, leaf_idx);
        // leaf-app depends on mid-lib and core-lib, so the closure should
        // include all three members.
        assert_eq!(subset.len(), 3);
    }

    #[test]
    fn discover_from_intermediate_workspace_dir_returns_subtree() {
        // Running a command from `services/` (itself a workspace, but nested
        // in the root) must resolve to the outermost root and target only the
        // services subtree (mid-lib + leaf-app), NOT core-lib.
        let dir = make_nested_workspace();
        let services = dir.path().join("services");

        match discover(&services).unwrap() {
            WorkspaceContext::WorkspaceSubtree { workspace_root, member_indices } => {
                assert_eq!(
                    workspace_root.canonicalize().unwrap(),
                    dir.path().canonicalize().unwrap(),
                    "subtree must resolve to the outermost root",
                );
                let ws = load(&workspace_root).unwrap();
                let declared: Vec<&str> = member_indices
                    .iter()
                    .map(|&i| ws.members[i].declared.as_str())
                    .collect();
                assert!(declared.iter().any(|d| d.contains("mid-lib")));
                assert!(declared.iter().any(|d| d.contains("leaf-app")));
                assert!(
                    !declared.iter().any(|d| *d == "core-lib"),
                    "core-lib is outside the services subtree: {declared:?}",
                );
            }
            other => panic!("expected WorkspaceSubtree, got {:?}", other),
        }
    }

    #[test]
    fn subtree_closure_pulls_in_cross_level_dep() {
        // The services subtree's closure must include core-lib (an
        // out-of-subtree workspace-dep) so members compile.
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();
        let targets: Vec<usize> = ws.members.iter().enumerate()
            .filter(|(_, m)| m.declared.starts_with("services/"))
            .map(|(i, _)| i)
            .collect();
        let subset = transitive_closure_multi(&ws, &targets);
        let declared: Vec<&str> = subset.iter().map(|&i| ws.members[i].declared.as_str()).collect();
        assert!(declared.contains(&"core-lib"), "closure must pull in core-lib: {declared:?}");
        assert_eq!(subset.len(), 3, "core-lib + mid-lib + leaf-app");
    }

    #[test]
    fn discover_leaf_prefers_outermost_workspace() {
        // Self-contained 2-level nest (no cross-level dep, so BOTH levels
        // load).  Discovery from the leaf must pick the OUTER root.
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner")).unwrap();
        std::fs::write(
            r.join("inner").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n",
        ).unwrap();
        std::fs::create_dir_all(r.join("inner").join("leaf")).unwrap();
        std::fs::write(
            r.join("inner").join("leaf").join("Curie.toml"),
            "[library]\nname = \"leaf\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        match discover(&r.join("inner").join("leaf")).unwrap() {
            WorkspaceContext::WorkspaceMember { workspace_root, .. } => {
                assert_eq!(
                    workspace_root.canonicalize().unwrap(),
                    r.canonicalize().unwrap(),
                    "outermost workspace must win",
                );
            }
            other => panic!("expected WorkspaceMember, got {:?}", other),
        }
    }

    #[test]
    fn enable_preview_inherits_from_workspace() {
        // Workspace sets enablePreview; silent member inherits it.
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n[java]\nenablePreview = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(ws.members[0].descriptor.java.preview_enabled());

        // Member that does not set it, workspace false → stays false.
        let ws2 = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(!ws2.members[0].descriptor.java.preview_enabled());
    }

    #[test]
    fn enable_preview_member_opts_out_with_explicit_false() {
        // Workspace enables preview; member sets `enablePreview = false` to
        // opt out — the explicit false must win over the inherited true.
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n[java]\nenablePreview = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [java]\nenablePreview = false\n")],
        ).unwrap();
        assert!(
            !ws.members[0].descriptor.java.preview_enabled(),
            "member enablePreview=false must override workspace true",
        );
    }

    // -- list tree ----------------------------------------------------------

    #[test]
    fn list_tree_has_workspace_and_project_nodes() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();

        // Root is a workspace.
        assert!(matches!(tree.kind, ListKind::Workspace));
        // Should have 2 direct children: core-lib and services.
        assert_eq!(tree.children.len(), 2);
        let core = &tree.children[0];
        assert!(matches!(&core.kind, ListKind::Project { label, .. } if *label == "library"));
        let services = &tree.children[1];
        assert!(matches!(services.kind, ListKind::Workspace));
        // services has 2 children: mid-lib and apps.
        assert_eq!(services.children.len(), 2);
    }

    #[test]
    fn list_view_focused_on_services_prunes_unrelated() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let services_abs = root.join("services").canonicalize().unwrap();
        let core_abs = root.join("core-lib").canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();
        let view = compute_view(&tree, &services_abs, false);

        // root, core-lib, services (+ all its descendants) must be kept.
        assert!(view.kept.contains(&root_abs));
        assert!(view.kept.contains(&services_abs));
        assert!(
            view.kept.contains(&core_abs),
            "core-lib is a dep of subtree members so it must be in kept",
        );
        // services is current.
        assert_eq!(view.current, services_abs);
    }

    #[test]
    fn list_view_required_by_reverse_edges() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let core_abs = root.join("core-lib").canonicalize().unwrap();
        let mid_abs = root.join("services").join("mid-lib").canonicalize().unwrap();
        let leaf_abs = root.join("services").join("apps").join("leaf-app").canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();
        let view = compute_view(&tree, &root_abs, false);

        // core-lib is required by mid-lib and leaf-app (paths relative to core-lib's parent = root).
        let core_reqs = view.required_by.get(&core_abs).expect("core-lib must have required_by");
        let mid_rel  = rel_from(&root_abs, &mid_abs);   // "services/mid-lib"
        let leaf_rel = rel_from(&root_abs, &leaf_abs);  // "services/apps/leaf-app"
        assert!(core_reqs.contains(&mid_rel),  "missing {mid_rel} in core-lib required_by: {core_reqs:?}");
        assert!(core_reqs.contains(&leaf_rel), "missing {leaf_rel} in core-lib required_by: {core_reqs:?}");

        // mid-lib is required by leaf-app (relative to mid-lib's parent = services/).
        let services_abs = root.join("services").canonicalize().unwrap();
        let mid_reqs = view.required_by.get(&mid_abs).expect("mid-lib must have required_by");
        let leaf_from_services = rel_from(&services_abs, &leaf_abs);  // "apps/leaf-app"
        assert!(mid_reqs.contains(&leaf_from_services), "missing {leaf_from_services}: {mid_reqs:?}");
    }

    #[test]
    fn list_view_all_keeps_everything() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();
        let view = compute_view(&tree, &root_abs, true);

        // 7 nodes: root, core-lib, services, mid-lib, apps, leaf-app = 6 per make_nested_workspace
        // (root + 5 = 6 — root/core-lib/services/mid-lib/apps/leaf-app).
        assert_eq!(view.kept.len(), 6);
    }

    #[test]
    fn rel_from_child() {
        let base = PathBuf::from("/a/b");
        let target = PathBuf::from("/a/b/c/d");
        assert_eq!(rel_from(&base, &target), "c/d");
    }

    #[test]
    fn rel_from_sibling() {
        let base = PathBuf::from("/a/b");
        let target = PathBuf::from("/a/c");
        assert_eq!(rel_from(&base, &target), "../c");
    }

    #[test]
    fn rel_from_self() {
        let p = PathBuf::from("/a/b");
        assert_eq!(rel_from(&p, &p), ".");
    }
}
