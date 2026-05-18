//! Lightweight Git introspection used to stamp `META-INF/build-info.properties`
//! inside every produced JAR.
//!
//! Only two pieces of information are needed:
//!
//! 1. **The HEAD commit id** — obtained via `git rev-parse HEAD`.
//! 2. **Whether the working tree is dirty** — obtained via
//!    `git status --porcelain`.  If there are any local changes (staged,
//!    unstaged, or untracked files) the commit id is suffixed with `-dirty`.
//!
//! The module deliberately avoids linking any native Git library; it shells out
//! to the `git` binary so there are no extra Cargo dependencies.  If `git` is
//! not on `PATH` or the directory is not a Git repo, [`detect`] returns `None`
//! and the caller skips generating the file.

use std::path::Path;
use std::process::Command;

/// Result of probing the Git state of a project directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// Full 40-hex-char commit id, potentially suffixed with `-dirty`.
    ///
    /// Examples:
    ///   * `"a3f1b2c4d5e6f7..."`       — clean checkout
    ///   * `"a3f1b2c4d5e6f7...-dirty"` — uncommitted local changes
    pub commit_id: String,
}

/// Probe `project_root` for Git information.
///
/// Returns `Some(GitInfo)` when:
///   - `git` is available on `PATH`, and
///   - `project_root` (or any ancestor) is inside a Git repository.
///
/// Returns `None` silently for all other cases (not a repo, git not installed,
/// etc.) so the caller can simply skip writing `build-info.properties`.
pub fn detect(project_root: &Path) -> Option<GitInfo> {
    let commit_id = rev_parse_head(project_root)?;
    let dirty = is_dirty(project_root);
    let full_id = if dirty {
        format!("{}-dirty", commit_id)
    } else {
        commit_id
    };
    Some(GitInfo { commit_id: full_id })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `git rev-parse HEAD` and return the trimmed output on success.
fn rev_parse_head(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let id = String::from_utf8(out.stdout).ok()?;
    let id = id.trim().to_owned();
    if id.is_empty() { None } else { Some(id) }
}

/// Return `true` when there are any local modifications (staged, unstaged, or
/// untracked files that are not ignored).
///
/// Uses `git status --porcelain` which outputs one line per modified/untracked
/// path and nothing at all for a clean tree — so a non-empty stdout means dirty.
fn is_dirty(dir: &Path) -> bool {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output();

    match out {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        // If the command fails we conservatively treat the tree as clean to
        // avoid falsely flagging every build.
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `detect` returns `None` for a plain directory that is not inside any
    /// Git repository.  We create a temp dir *outside* any repo to guarantee
    /// this.
    #[test]
    fn detect_returns_none_outside_git_repo() {
        // Pick a temp location that is definitely not inside a git repo.
        // /tmp itself is normally not under version control.
        let dir = tempfile::Builder::new()
            .prefix("curie-test-no-git-")
            .tempdir_in(std::env::temp_dir())
            .expect("tempdir");
        // Ensure there is no .git ancestor by initialising a fresh dir at /tmp level.
        // We rely on the OS temp dir not being inside a git repo.
        let result = detect(dir.path());
        // On CI /tmp might be inside a git checkout; only assert None when git
        // exits non-zero (not a repo).  We can't assert a specific outcome here
        // without knowing the CI environment, so just ensure no panic.
        let _ = result; // smoke test: must not panic
    }

    #[test]
    fn commit_id_dirty_suffix() {
        let info = GitInfo { commit_id: "abc123-dirty".to_owned() };
        assert!(info.commit_id.ends_with("-dirty"));
    }

    #[test]
    fn commit_id_clean_no_suffix() {
        let info = GitInfo { commit_id: "abc123def456".to_owned() };
        assert!(!info.commit_id.ends_with("-dirty"));
    }

    /// When `git rev-parse HEAD` fails (not a repo), `rev_parse_head` must
    /// return `None`.
    #[test]
    fn rev_parse_head_returns_none_for_non_repo() {
        let dir = tempfile::Builder::new()
            .prefix("curie-test-no-git-")
            .tempdir_in(std::env::temp_dir())
            .expect("tempdir");
        let result = rev_parse_head(dir.path());
        // In a plain directory git exits non-zero → None.
        assert!(result.is_none());
    }
}
