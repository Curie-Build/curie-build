//! Workspace discovery and member iteration.
//!
//! A workspace is rooted at a `Curie.toml` whose `[workspace]` section lists
//! `members` (paths relative to that `Curie.toml`'s directory).  Each member
//! is itself a buildable project (application or library) with its own
//! `Curie.toml`.
//!
//! This module is intentionally minimal at step 1: it parses the workspace
//! root, loads each member's descriptor, and exposes the result.  No build
//! ordering, no inter-member dependencies, no config inheritance — those
//! land in later steps.

use crate::descriptor::{self, Descriptor};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// A single member of a workspace: its path on disk plus its loaded descriptor.
#[derive(Debug)]
pub struct Member {
    /// Absolute or root-relative path to the member's directory.  Will be
    /// used by `curie build` / `curie test` once those become
    /// workspace-aware (step 2 of multi-module support).
    #[allow(dead_code)]
    pub path: PathBuf,
    /// Member name as declared in the workspace's `members = [...]` list,
    /// kept verbatim for use in messages where the user-facing path matters
    /// (e.g. `curie list` output).
    pub declared: String,
    pub descriptor: Descriptor,
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

    let mut members: Vec<Member> = Vec::with_capacity(ws.members.len());
    for declared in &ws.members {
        let path = workspace_root.join(declared);
        if !path.exists() {
            bail!(
                "workspace member \"{}\" not found at {}",
                declared,
                path.display(),
            );
        }
        let descriptor = descriptor::load(&path)
            .with_context(|| format!("failed to load workspace member \"{}\"", declared))?;
        if descriptor.is_workspace() {
            bail!(
                "workspace member \"{}\" is itself a workspace; nested workspaces are not supported",
                declared,
            );
        }
        members.push(Member {
            path,
            declared: declared.clone(),
            descriptor,
        });
    }

    Ok(Workspace {
        root: workspace_root.to_path_buf(),
        members,
    })
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
}
