//! Authoritative source → class-file mapping persisted between builds.
//!
//! The javac wrapper records, at compile time, which class files each
//! source produced.  Curie writes that mapping to `target/.classes.toml`
//! and reads it on the next build to drive precise stale-class removal —
//! eliminating the edge cases the previous filename-stem heuristic missed
//! (multiple top-level classes per file; non-public types named
//! differently from the file).
//!
//! Two phases on every build:
//!
//! 1. **Pre-compile**: any source in the previous manifest that is no
//!    longer in the current source set must have ALL of its previously
//!    produced classes deleted, before javac runs.  Stops a stale
//!    class file from being picked up via the classes-dir-on-classpath
//!    fall-through that javac does.
//!
//! 2. **Post-compile**: for any source present in both old and new
//!    manifests, the set difference (old − new) is stale — the source
//!    survived but it no longer produces that particular class (e.g. a
//!    package-private companion type was removed from the file).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// On-disk shape of `target/.classes.toml`.
///
/// ```toml
/// [sources]
/// "/abs/path/to/Foo.java" = ["com/foo/Foo.class", "com/foo/Foo$Inner.class"]
/// "/abs/path/to/Baz.java" = ["com/foo/Baz.class"]
/// ```
///
/// Source paths are whatever `JavaFileObject.toUri().getPath()` yielded
/// in the wrapper — absolute on Unix.  Class paths are relative to the
/// `-d` output directory and use `/` plus `$` for nested types (binary
/// names), exactly as written to disk by javac.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Manifest {
    #[serde(default)]
    pub sources: BTreeMap<String, Vec<String>>,
}

/// Load the manifest written by the wrapper at `path`.
///
/// Missing file returns `Ok(None)` (first build of the project, or after
/// `curie clean`).  Parse failures are surfaced — a malformed manifest
/// is a bug we want to know about, not silently ignore.
pub fn load(path: &Path) -> Result<Option<Manifest>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: Manifest = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(manifest))
}

/// Compute the set of class files (relative to the classes dir) that are
/// stale given the previous and (optionally) the new manifest.
///
/// `current_sources` is the set of source paths the current compile
/// invocation will pass to javac, in the same form the manifest uses
/// (absolute / canonical).  It's only consulted when `now` is `None`.
///
/// `pre_compile_ignore_prefix` is consulted only when `now == None`.
/// Sources in the old manifest whose path starts with this prefix are
/// NOT treated as "deleted" even when they're absent from
/// `current_sources`.  Used to exempt annotation-processor outputs
/// (under `target/generated-sources/`) from pre-compile pruning: those
/// generated sources are never in the user-source set, but they'll be
/// re-emitted by javac this compile and the post-compile diff catches
/// any that the AP stops producing.  Without this carve-out, a
/// no-change rebuild would churn through every AP-generated class.
///
/// Semantics:
/// - `now = None` (pre-compile): only sources in `old` that no longer
///   appear in `current_sources` AND don't sit under
///   `pre_compile_ignore_prefix` contribute; all of their classes are
///   reported.
/// - `now = Some(m)` (post-compile): for each source in `old`:
///   - if `m` has the same source, report `old[src] − m[src]`,
///   - else report all of `old[src]` (source compiled but produced no
///     classes, e.g. an empty file, or wasn't compiled at all).
pub fn stale_classes(
    old: &Manifest,
    now: Option<&Manifest>,
    current_sources: &HashSet<String>,
    pre_compile_ignore_prefix: Option<&str>,
) -> Vec<String> {
    let mut stale = Vec::new();
    for (src, old_classes) in &old.sources {
        match now {
            None => {
                if !current_sources.contains(src) {
                    // AP-generated sources live under target/generated-sources/;
                    // skip them in pre-prune (post-compile catches the
                    // "generator stopped producing this" case correctly).
                    if pre_compile_ignore_prefix
                        .map(|p| src.starts_with(p))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    stale.extend(old_classes.iter().cloned());
                }
            }
            Some(new) => match new.sources.get(src) {
                Some(new_classes) => {
                    let new_set: HashSet<&String> = new_classes.iter().collect();
                    for c in old_classes {
                        if !new_set.contains(c) {
                            stale.push(c.clone());
                        }
                    }
                }
                None => {
                    stale.extend(old_classes.iter().cloned());
                }
            },
        }
    }
    stale
}

/// Delete each class file (relative path joined onto `classes_dir`).
/// Missing files are silently ignored — they may have already been
/// cleaned up by an earlier prune step, or by `curie clean`.
pub fn delete_classes(classes_dir: &Path, relative: &[String]) -> Result<usize> {
    let mut removed = 0;
    for rel in relative {
        let p = classes_dir.join(rel);
        if p.exists() {
            std::fs::remove_file(&p)
                .with_context(|| format!("failed to remove stale class {}", p.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(entries: &[(&str, &[&str])]) -> Manifest {
        let mut m = Manifest::default();
        for (src, classes) in entries {
            m.sources.insert(
                (*src).to_string(),
                classes.iter().map(|c| (*c).to_string()).collect(),
            );
        }
        m
    }

    fn sources_set(s: &[&str]) -> HashSet<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    // -- pre-compile prune --------------------------------------------------

    #[test]
    fn pre_compile_drops_classes_of_deleted_sources() {
        let old = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class"]),
            ("/a/Bar.java", &["com/Bar.class", "com/Bar$Inner.class"]),
        ]);
        // Bar.java was deleted; only Foo.java survives.
        let current = sources_set(&["/a/Foo.java"]);
        let stale = stale_classes(&old, None, &current, None);
        assert_eq!(stale, vec!["com/Bar.class", "com/Bar$Inner.class"]);
    }

    #[test]
    fn pre_compile_keeps_classes_of_surviving_sources() {
        let old = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class", "com/Foo$Inner.class"]),
        ]);
        let current = sources_set(&["/a/Foo.java"]);
        let stale = stale_classes(&old, None, &current, None);
        assert!(stale.is_empty(), "no source deleted → nothing stale");
    }

    /// Regression: AP-generated sources live under `target/` and are
    /// never in the user-source set, but they're re-emitted by javac
    /// every build.  Pre-prune must NOT delete their classes — otherwise
    /// every rebuild reports "stale classes removed" and pointlessly
    /// rebuilds the annotation-processor output.
    #[test]
    fn pre_compile_ignores_paths_under_generated_prefix() {
        let old = manifest_with(&[
            ("/proj/src/com/Foo.java", &["com/Foo.class"]),
            (
                "/proj/target/generated-sources/annotations/com/AutoValue_Foo.java",
                &["com/AutoValue_Foo.class"],
            ),
        ]);
        let current = sources_set(&["/proj/src/com/Foo.java"]);
        // Without the carve-out the AutoValue_Foo.class would be deleted.
        let stale_no_carve = stale_classes(&old, None, &current, None);
        assert_eq!(stale_no_carve, vec!["com/AutoValue_Foo.class"]);
        // With the carve-out it stays put — post-compile handles it.
        let stale = stale_classes(&old, None, &current, Some("/proj/target"));
        assert!(
            stale.is_empty(),
            "AP-generated source under target/ must not be pre-pruned",
        );
    }

    // -- post-compile prune -------------------------------------------------

    #[test]
    fn post_compile_drops_classes_kept_source_no_longer_produces() {
        // Foo.java used to declare a companion `class Bar`; that line is
        // gone, so the next build doesn't emit Bar.class.
        let old = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class", "com/Bar.class"]),
        ]);
        let now = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class"]),
        ]);
        let stale = stale_classes(&old, Some(&now), &HashSet::new(), None);
        assert_eq!(stale, vec!["com/Bar.class"]);
    }

    #[test]
    fn post_compile_drops_everything_for_source_missing_from_new_manifest() {
        // Foo.java edited so it now produces no classes (e.g. left only
        // `package foo;`).  All its old classes are stale.
        let old = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class", "com/Foo$Inner.class"]),
        ]);
        let now = Manifest::default();
        let stale = stale_classes(&old, Some(&now), &HashSet::new(), None);
        assert_eq!(stale, vec!["com/Foo.class", "com/Foo$Inner.class"]);
    }

    #[test]
    fn post_compile_no_changes_means_nothing_stale() {
        let old = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class", "com/Foo$Inner.class"]),
        ]);
        let now = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class", "com/Foo$Inner.class"]),
        ]);
        let stale = stale_classes(&old, Some(&now), &HashSet::new(), None);
        assert!(stale.is_empty());
    }

    #[test]
    fn post_compile_new_source_contributes_no_staleness() {
        // Bar.java is new this build — only in `now`, not in `old`.
        let old = manifest_with(&[("/a/Foo.java", &["com/Foo.class"])]);
        let now = manifest_with(&[
            ("/a/Foo.java", &["com/Foo.class"]),
            ("/a/Bar.java", &["com/Bar.class"]),
        ]);
        let stale = stale_classes(&old, Some(&now), &HashSet::new(), None);
        assert!(stale.is_empty());
    }

    // -- load / round-trip --------------------------------------------------

    #[test]
    fn load_missing_manifest_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn load_parses_wrapper_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("classes.toml");
        std::fs::write(&path, r#"
[sources]
"/abs/Foo.java" = ["com/Foo.class", "com/Foo$Inner.class"]
"/abs/Bar.java" = ["com/Bar.class"]
"#).unwrap();
        let m = load(&path).unwrap().unwrap();
        assert_eq!(m.sources.len(), 2);
        assert_eq!(m.sources["/abs/Foo.java"], vec!["com/Foo.class", "com/Foo$Inner.class"]);
    }

    // -- delete_classes -----------------------------------------------------

    #[test]
    fn delete_classes_removes_existing_skips_missing() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        let a = classes.join("com").join("A.class");
        let b = classes.join("com").join("B.class");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let removed = delete_classes(
            &classes,
            &["com/A.class".to_string(), "com/ghost.class".to_string()],
        ).unwrap();

        assert_eq!(removed, 1);
        assert!(!a.exists());
        assert!(b.exists(), "unrelated file untouched");
    }
}
