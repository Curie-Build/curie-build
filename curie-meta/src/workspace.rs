use crate::descriptor::{self, Descriptor};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

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
pub fn discover(project: &Path) -> Result<WorkspaceContext> {
    let desc = descriptor::load(project)?;
    let project_is_workspace = desc.is_workspace();

    let project_canon = project
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", project.display()))?;

    let mut best: Option<WorkspaceContext> = None;
    let mut cur = project_canon.parent();
    while let Some(dir) = cur {
        if dir.join("Curie.toml").exists() {
            if let Ok(d) = descriptor::load(dir) {
                if d.is_workspace() {
                    if let Ok(ws_loaded) = load(dir) {
                        if project_is_workspace {
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

    let mut raw_members: Vec<Member> = Vec::new();
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
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

    let canon: Vec<PathBuf> = raw_members
        .iter()
        .map(|m| m.path.canonicalize().unwrap_or_else(|_| m.path.clone()))
        .collect();

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

    let order = topo_sort(raw_members.len(), &edges).map_err(|cycle| {
        let chain = cycle
            .iter()
            .map(|&i| raw_members[i].declared.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        anyhow::anyhow!("workspace-dependency cycle detected: {}", chain)
    })?;

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

/// Recursively expand workspace members, flattening nested workspaces.
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

        let mut member_desc = descriptor::load(&path)
            .with_context(|| format!("failed to load workspace member \"{}\"", name))?;

        if member_desc.is_workspace() {
            let inner_ws = member_desc.workspace().unwrap();
            let inner_members = inner_ws.members.clone();
            let inner_prefix = if prefix.is_empty() {
                format!("{}/", name)
            } else {
                format!("{}{}/", prefix, name)
            };

            let before_len = out.len();
            expand_members(&path, &member_desc, &inner_members, &inner_prefix, out, seen)?;

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
            inherit_from_workspace(&mut member_desc, ws_desc);
            descriptor::validate_dep_repo_refs(&member_desc)
                .with_context(|| format!("invalid repository reference in member \"{}\"", name))?;

            let declared = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}{}", prefix, name)
            };

            out.push(Member {
                path,
                declared,
                descriptor: member_desc,
                workspace_deps: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Merge workspace-level inheritable config into a member descriptor.
fn inherit_from_workspace(member: &mut Descriptor, ws: &Descriptor) {
    if member.java.source_compatibility.is_none() {
        member.java.source_compatibility = ws.java.source_compatibility.clone();
    }
    if member.java.enable_preview.is_none() {
        member.java.enable_preview = ws.java.enable_preview;
    }
    if member.test.junit_platform_version.is_none() {
        member.test.junit_platform_version = ws.test.junit_platform_version.clone();
    }
    if member.test.coverage.is_none() {
        member.test.coverage = ws.test.coverage;
    }
    if member.kotlin.version.is_none() {
        member.kotlin.version = ws.kotlin.version.clone();
    }
    if member.groovy.version.is_none() {
        member.groovy.version = ws.groovy.version.clone();
    }
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
/// (from a nearer/inner workspace) take precedence over `base` entries.
pub(crate) fn merge_btree<V: Clone>(
    target: &mut std::collections::BTreeMap<String, V>,
    base: &std::collections::BTreeMap<String, V>,
) {
    let existing = std::mem::take(target);
    *target = base.clone();
    for (k, v) in existing {
        target.insert(k, v);
    }
}

/// Like [`merge_btree`] but for nested `BTreeMap<String, BTreeMap<String, String>>`.
pub(crate) fn merge_nested_btree(
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
pub(crate) fn topo_sort(n: usize, edges: &[Vec<usize>]) -> std::result::Result<Vec<usize>, Vec<usize>> {
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
        let leftover: Vec<usize> = (0..n).filter(|v| !order.contains(v)).collect();
        Err(leftover)
    } else {
        Ok(order)
    }
}

// ---------------------------------------------------------------------------
// curie list — tree view
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ListKind {
    Workspace,
    Project { label: &'static str, version: String },
}

/// A node in the list-tree hierarchy.
#[derive(Debug)]
pub struct ListNode {
    name: String,
    abs_path: PathBuf,
    parent_ws_abs: PathBuf,
    kind: ListKind,
    dep_targets: Vec<PathBuf>,
    children: Vec<ListNode>,
}

/// Recursively build the list-tree rooted at `root`.
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
pub struct ListView {
    pub kept: HashSet<PathBuf>,
    pub required_by: HashMap<PathBuf, Vec<String>>,
    pub current: PathBuf,
}

/// Compute a path from `base` to `target` using `..` as needed.
/// Both paths must be canonical (no symlinks, no `.`).
pub fn rel_from(base: &Path, target: &Path) -> String {
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

fn walk_nodes<'a>(node: &'a ListNode, out: &mut Vec<&'a ListNode>) {
    out.push(node);
    for c in &node.children {
        walk_nodes(c, out);
    }
}

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
    stack.reverse();
    stack
}

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

fn parent_ws_of(root: &ListNode, target_abs: &Path) -> Option<PathBuf> {
    let mut all = Vec::new();
    walk_nodes(root, &mut all);
    all.iter()
        .find(|n| n.abs_path == target_abs)
        .map(|n| n.parent_ws_abs.clone())
}

/// Compute which nodes to show and which `required by` strings to attach.
pub fn compute_view(root: &ListNode, current: &Path, all: bool) -> ListView {
    let mut all_nodes = Vec::new();
    walk_nodes(root, &mut all_nodes);
    let node_by_abs: HashMap<&PathBuf, &&ListNode> = all_nodes
        .iter()
        .map(|n| (&n.abs_path, n))
        .collect();

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
    for v in required_by.values_mut() {
        v.sort();
    }

    if all {
        let kept = all_nodes.iter().map(|n| n.abs_path.clone()).collect();
        return ListView { kept, required_by, current: current.to_path_buf() };
    }

    let mut kept: HashSet<PathBuf> = HashSet::new();

    let subtree = subtree_abs_paths(root, current).unwrap_or_else(|| {
        let mut s = HashSet::new();
        s.insert(current.to_path_buf());
        s
    });
    kept.extend(subtree.iter().cloned());

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

    let kept_snapshot: Vec<PathBuf> = kept.iter().cloned().collect();
    for abs in &kept_snapshot {
        kept.extend(ancestors_of(abs, root));
    }

    kept.insert(root.abs_path.clone());

    ListView { kept, required_by, current: current.to_path_buf() }
}

const DIM: &str = "\x1b[2m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const RESET: &str = "\x1b[0m";

fn render_node(
    node: &ListNode,
    view: &ListView,
    prefix: &str,
    child_prefix: &str,
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
/// `color` enables ANSI color output.
pub fn list(root: &Path, current: &Path, all: bool, color: bool) -> Result<()> {
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

    render_node(&tree, &view, "", "", color);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn topo_sort_no_edges_is_input_order() {
        let order = topo_sort(3, &[vec![], vec![], vec![]]).unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn topo_sort_linear_chain() {
        let order = topo_sort(3, &[vec![1], vec![2], vec![]]).unwrap();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn topo_sort_diamond() {
        let order = topo_sort(4, &[vec![1, 2], vec![3], vec![3], vec![]]).unwrap();
        assert_eq!(order[0], 3);
        assert_eq!(order[3], 0);
    }

    #[test]
    fn topo_sort_cycle_is_reported() {
        let err = topo_sort(2, &[vec![1], vec![0]]).unwrap_err();
        assert_eq!(err.len(), 2);
        assert!(err.contains(&0) && err.contains(&1));
    }

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
        let dir = make_ws_with_deps(&[
            ("app", &[("lib", "../lib")]),
            ("lib", &[]),
        ]);
        let ws = load(dir.path()).unwrap();
        let names: Vec<&str> = ws.members.iter().map(|m| m.declared.as_str()).collect();
        assert_eq!(names, vec!["lib", "app"]);
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

    fn load_ws_with_content(ws_toml: &str, members: &[(&str, &str)]) -> Result<Workspace> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Curie.toml"), ws_toml).unwrap();
        for (name, content) in members {
            let mpath = dir.path().join(name);
            std::fs::create_dir_all(&mpath).unwrap();
            std::fs::write(mpath.join("Curie.toml"), content).unwrap();
        }
        let result = load(dir.path());
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
        let gavs = d.prod_bom_gavs().unwrap();
        assert_eq!(gavs.len(), 1);
        assert_eq!(gavs[0].to_string(), "org.x:bom:1.0");
    }

    #[test]
    fn member_bom_appears_after_workspace_bom_in_gav_order() {
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
        assert_eq!(gavs, vec!["ws:prod:1", "own:prod:1", "ws:test:1", "own:test:1"]);
    }

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
        assert_eq!(
            flat,
            vec![
                ("dagger.fastInit".to_string(), "enabled".to_string()),
                ("dagger.formatGeneratedSource".to_string(), "disabled".to_string()),
            ],
        );
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

    fn make_nested_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();

        std::fs::write(
            r.join("Curie.toml"),
            "[workspace]\nmembers = [\"core-lib\", \"services\"]\n\
             [java]\nsourceCompatibility = \"17\"\n\
             [bom-imports]\n\"ws:bom\" = \"1.0\"\n",
        ).unwrap();

        std::fs::create_dir_all(r.join("core-lib")).unwrap();
        std::fs::write(
            r.join("core-lib").join("Curie.toml"),
            "[library]\nname = \"core-lib\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        std::fs::create_dir_all(r.join("services")).unwrap();
        std::fs::write(
            r.join("services").join("Curie.toml"),
            "[workspace]\nmembers = [\"mid-lib\", \"apps\"]\n\
             [test-bom-imports]\n\"ws:test-bom\" = \"2.0\"\n",
        ).unwrap();

        std::fs::create_dir_all(r.join("services").join("mid-lib")).unwrap();
        std::fs::write(
            r.join("services").join("mid-lib").join("Curie.toml"),
            "[library]\nname = \"mid-lib\"\nversion = \"0.1.0\"\n\
             [workspace-dependencies]\ncore = { path = \"../../core-lib\" }\n",
        ).unwrap();

        std::fs::create_dir_all(r.join("services").join("apps")).unwrap();
        std::fs::write(
            r.join("services").join("apps").join("Curie.toml"),
            "[workspace]\nmembers = [\"leaf-app\"]\n",
        ).unwrap();

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
        assert_eq!(names, vec!["core-lib", "services/mid-lib", "services/apps/leaf-app"]);
    }

    #[test]
    fn nested_config_inheritance_cascades_through_levels() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();

        let leaf = ws.members.iter().find(|m| m.declared.contains("leaf-app")).unwrap();
        assert_eq!(leaf.descriptor.java.effective(), "17");

        assert_eq!(
            leaf.descriptor.inherited_bom_imports.get("ws:bom").map(String::as_str),
            Some("1.0"),
        );

        assert_eq!(
            leaf.descriptor.inherited_test_bom_imports.get("ws:test-bom").map(String::as_str),
            Some("2.0"),
        );

        let mid = ws.members.iter().find(|m| m.declared.contains("mid-lib")).unwrap();
        assert_eq!(mid.descriptor.java.effective(), "17");
    }

    #[test]
    fn nested_workspace_deps_resolve_across_levels() {
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();

        let leaf_idx = ws.members.iter().position(|m| m.declared.contains("leaf-app")).unwrap();
        let leaf = &ws.members[leaf_idx];
        assert_eq!(leaf.workspace_deps.len(), 2);

        let mid_idx = ws.members.iter().position(|m| m.declared.contains("mid-lib")).unwrap();
        let mid = &ws.members[mid_idx];
        assert_eq!(mid.workspace_deps.len(), 1);
    }

    #[test]
    fn nested_duplicate_project_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();

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
        assert_eq!(repos, vec!["outer-repo", "inner-repo", "leaf-repo"]);
    }

    #[test]
    fn discover_finds_member_inside_nested_workspace() {
        let dir = make_nested_workspace();
        let leaf_path = dir.path().join("services").join("apps").join("leaf-app");

        match discover(&leaf_path).unwrap() {
            WorkspaceContext::WorkspaceMember { workspace_root, member_index } => {
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
    fn nested_transitive_closure_test() {
        // Exercise load: leaf-app depends on mid-lib and core-lib, so loading
        // the workspace should correctly order all 3 members.
        let dir = make_nested_workspace();
        let ws = load(dir.path()).unwrap();
        let leaf_idx = ws.members.iter()
            .position(|m| m.declared.contains("leaf-app")).unwrap();
        // leaf-app has 2 workspace_deps
        assert_eq!(ws.members[leaf_idx].workspace_deps.len(), 2);
    }

    #[test]
    fn discover_from_intermediate_workspace_dir_returns_subtree() {
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
    fn discover_leaf_prefers_outermost_workspace() {
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
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n[java]\nenablePreview = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(ws.members[0].descriptor.java.preview_enabled());

        let ws2 = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(!ws2.members[0].descriptor.java.preview_enabled());
    }

    #[test]
    fn enable_preview_member_opts_out_with_explicit_false() {
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

    #[test]
    fn list_tree_has_workspace_and_project_nodes() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();

        assert!(matches!(tree.kind, ListKind::Workspace));
        assert_eq!(tree.children.len(), 2);
        let core = &tree.children[0];
        assert!(matches!(&core.kind, ListKind::Project { label, .. } if *label == "library"));
        let services = &tree.children[1];
        assert!(matches!(services.kind, ListKind::Workspace));
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

        assert!(view.kept.contains(&root_abs));
        assert!(view.kept.contains(&services_abs));
        assert!(
            view.kept.contains(&core_abs),
            "core-lib is a dep of subtree members so it must be in kept",
        );
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

        let core_reqs = view.required_by.get(&core_abs).expect("core-lib must have required_by");
        let mid_rel  = rel_from(&root_abs, &mid_abs);
        let leaf_rel = rel_from(&root_abs, &leaf_abs);
        assert!(core_reqs.contains(&mid_rel),  "missing {mid_rel} in core-lib required_by: {core_reqs:?}");
        assert!(core_reqs.contains(&leaf_rel), "missing {leaf_rel} in core-lib required_by: {core_reqs:?}");

        let services_abs = root.join("services").canonicalize().unwrap();
        let mid_reqs = view.required_by.get(&mid_abs).expect("mid-lib must have required_by");
        let leaf_from_services = rel_from(&services_abs, &leaf_abs);
        assert!(mid_reqs.contains(&leaf_from_services), "missing {leaf_from_services}: {mid_reqs:?}");
    }

    #[test]
    fn list_view_all_keeps_everything() {
        let dir = make_nested_workspace();
        let root = dir.path();
        let root_abs = root.canonicalize().unwrap();
        let tree = build_list_tree(root, &root_abs, "root").unwrap();
        let view = compute_view(&tree, &root_abs, true);

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

    #[test]
    fn coverage_inherits_from_workspace_when_member_omits() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\n[test]\ncoverage = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(
            ws.members[0].descriptor.test.coverage_enabled(),
            "member must inherit coverage=true from workspace",
        );
        assert_eq!(ws.members[0].descriptor.test.coverage, Some(true));
    }

    #[test]
    fn coverage_member_explicit_false_overrides_workspace_true() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\n[test]\ncoverage = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [test]\ncoverage = false\n")],
        ).unwrap();
        assert!(
            !ws.members[0].descriptor.test.coverage_enabled(),
            "member coverage=false must override workspace coverage=true",
        );
        assert_eq!(ws.members[0].descriptor.test.coverage, Some(false));
    }

    #[test]
    fn coverage_defaults_to_false_when_neither_sets_it() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        assert!(
            !ws.members[0].descriptor.test.coverage_enabled(),
            "coverage must default to false when neither workspace nor member sets it",
        );
    }

    #[test]
    fn coverage_inherits_alongside_junit_version() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\n[test]\njunitPlatformVersion = \"5.10.0\"\ncoverage = true\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n")],
        ).unwrap();
        let t = &ws.members[0].descriptor.test;
        assert!(t.coverage_enabled());
        assert_eq!(t.junit_platform_version(), "5.10.0");
    }
}
