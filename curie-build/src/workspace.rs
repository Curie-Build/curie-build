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

use crate::descriptor::{self, Descriptor};
use crate::{build, compile, test};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
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

/// Load the workspace rooted at `workspace_root`.  Fails if the directory's
/// `Curie.toml` is missing or does not contain `[workspace]`.
///
/// Member descriptors are loaded eagerly so that a malformed member's
/// `Curie.toml` is reported immediately instead of mid-build.
pub fn load(workspace_root: &Path) -> Result<Workspace> {
    let root_desc = descriptor::load(workspace_root)
        .with_context(|| format!("failed to load workspace at {}", workspace_root.display()))?;

    let ws = root_desc
        .workspace
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "{} is not a workspace: its Curie.toml has no [workspace] section",
            workspace_root.display(),
        ))?;

    // Phase 1: load every member descriptor (sans workspace_deps) and
    // merge in inherited workspace-level config.
    let mut raw_members: Vec<Member> = Vec::with_capacity(ws.members.len());
    for declared in &ws.members {
        let path = workspace_root.join(declared);
        if !path.exists() {
            bail!(
                "workspace member \"{}\" not found at {}",
                declared,
                path.display(),
            );
        }
        let mut descriptor = descriptor::load(&path)
            .with_context(|| format!("failed to load workspace member \"{}\"", declared))?;
        if descriptor.is_workspace() {
            bail!(
                "workspace member \"{}\" is itself a workspace; nested workspaces are not supported",
                declared,
            );
        }
        inherit_from_workspace(&mut descriptor, &root_desc);
        raw_members.push(Member {
            path,
            declared: declared.clone(),
            descriptor,
            workspace_deps: Vec::new(),
        });
    }

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
fn inherit_from_workspace(member: &mut Descriptor, ws: &Descriptor) {
    if member.java.source_compatibility.is_none() {
        member.java.source_compatibility = ws.java.source_compatibility.clone();
    }
    if !ws.repositories.is_empty() {
        let mut combined = ws.repositories.clone();
        combined.append(&mut member.repositories);
        member.repositories = combined;
    }
    member.inherited_bom_imports = ws.bom_imports.clone();
    member.inherited_test_bom_imports = ws.test_bom_imports.clone();
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

/// Print the workspace's members to stdout: one line per member with the
/// declared name, kind, and version.  Format is stable enough to grep
/// without being a committed-API contract.
pub fn list(workspace_root: &Path) -> Result<()> {
    let ws = load(workspace_root)?;
    println!(
        "Workspace {} ({} member{})",
        ws.root.display(),
        ws.members.len(),
        if ws.members.len() == 1 { "" } else { "s" },
    );

    // Pad the declared-name column so the kind/version columns line up.
    let name_w = ws.members.iter().map(|m| m.declared.len()).max().unwrap_or(0);

    for m in &ws.members {
        println!(
            "  {:<width$}  {:<11}  v{}",
            m.declared,
            m.descriptor.kind(),
            m.descriptor.project_version(),
            width = name_w,
        );
    }
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
fn collect_dep_classpath(deps: &[usize], artifacts: &[MemberArtifact]) -> Vec<PathBuf> {
    let mut cp: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for &i in deps {
        let a = &artifacts[i];
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

/// Iterate workspace members in topo order, prefix each with a "[i/n] name"
/// banner, invoke `run` (which returns the member's own Maven dep JARs so
/// the contribution can be assembled), and accumulate artifacts so later
/// members see their workspace-deps' classpaths.
fn fan_out<F>(workspace_root: &Path, action: &str, mut run: F) -> Result<()>
where
    F: FnMut(&Member, &[PathBuf]) -> Result<Vec<PathBuf>>,
{
    let ws = load(workspace_root)?;
    let n = ws.members.len();
    println!(
        "Workspace {} {} ({} member{})",
        ws.root.display(),
        action,
        n,
        if n == 1 { "" } else { "s" },
    );
    println!();

    let mut artifacts: Vec<MemberArtifact> = Vec::with_capacity(n);
    for (i, m) in ws.members.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, n, m.declared);
        let extra_cp = collect_dep_classpath(&m.workspace_deps, &artifacts);
        let own_dep_jars = run(m, &extra_cp)
            .with_context(|| format!("workspace member \"{}\" failed", m.declared))?;

        let classes_dir = m.path.join("target").join("classes");
        let mut contribution = extra_cp; // already deduped
        for j in own_dep_jars {
            contribution.push(j);
        }
        artifacts.push(MemberArtifact { classes_dir, classpath_contribution: contribution });
        println!();
    }
    Ok(())
}

/// Fan `curie build` out over every member in topo order.  Each member's
/// build receives its workspace-deps' classes_dir + transitive Maven JARs
/// on the compile/test classpath; produces a JAR; stops at first failure.
pub fn build_all(workspace_root: &Path, opts: build::BuildOptions) -> Result<()> {
    fan_out(workspace_root, "build", |m, extra_cp| {
        let output = build::build_with_desc(&m.path, &m.descriptor, opts, extra_cp)?;
        Ok(output.dep_jars)
    })
}

/// Fan `curie test` out over every member in topo order.  Same threading
/// as build, but skips packaging and Docker — using upstream `classes_dir`
/// as the workspace-dep entry means downstream tests don't need the
/// upstream JAR to exist.
pub fn test_all(workspace_root: &Path, filter: Option<&str>, offline: bool) -> Result<()> {
    fan_out(workspace_root, "test", |m, extra_cp| {
        println!(
            "Testing {} v{}",
            m.descriptor.project_name(),
            m.descriptor.project_version(),
        );
        let compiled = compile::compile(&m.path, &m.descriptor, offline, extra_cp)?;
        test::run_tests(
            &m.path,
            &m.descriptor,
            &compiled.classes_dir,
            &compiled.dep_jars,
            compiled.resources_dir.as_deref(),
            compiled.test_resources_dir.as_deref(),
            filter,
            offline,
            extra_cp,
        )?;
        Ok(compiled.dep_jars)
    })
}

/// Fan `curie clean` out over every member.  Order doesn't matter for
/// clean, but reusing `fan_out` keeps banner output consistent.
pub fn clean_all(workspace_root: &Path) -> Result<()> {
    fan_out(workspace_root, "clean", |m, _extra_cp| {
        build::clean(&m.path)?;
        Ok(Vec::new())
    })
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
        assert_eq!(ws.members[0].descriptor.project_name(), "a");
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
    fn load_nested_workspace_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"inner\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("inner")).unwrap();
        std::fs::write(
            dir.path().join("inner").join("Curie.toml"),
            "[workspace]\nmembers = []\n",
        )
        .unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("nested"), "got: {err}");
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

    #[test]
    fn repositories_inherit_prepended() {
        let ws = load_ws_with_content(
            "[workspace]\nmembers = [\"a\"]\n\
             [[repositories]]\nname = \"ws-repo\"\nurl = \"https://ws.example.com\"\n",
            &[("a", "[library]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    [[repositories]]\nname = \"own-repo\"\nurl = \"https://own.example.com\"\n")],
        ).unwrap();
        let repos = &ws.members[0].descriptor.repositories;
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "ws-repo");
        assert_eq!(repos[1].name, "own-repo");
    }
}
