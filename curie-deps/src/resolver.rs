//! Dependency resolver: cache lookup → download → transitive expansion.
//!
//! # Algorithm
//! 1. For each declared `Gav`, check `~/.m2/repository` for the JAR and POM.
//! 2. On cache miss, try each configured repository in order; download to a
//!    `.part` file, rename atomically on success.
//! 3. Parse the POM to discover compile-scoped transitive dependencies.
//! 4. Recurse (BFS) until the full closure is resolved.
//! 5. Return all resolved JAR paths in stable topological order
//!    (declared deps first, then their transitive deps breadth-first).
//!
//! # BOM imports
//! Before the BFS begins, all BOMs listed in [`ResolveOptions::bom_imports`] are
//! fetched and their `<dependencyManagement>` entries are merged into a
//! `global_managed` map.  Dependencies declared with an empty version string
//! are resolved against this map (hard error if not found).  Transitive deps
//! with no version fall back to `global_managed` silently.
//!
//! BOMs are processed with **later-declared wins** semantics: if two BOMs both
//! manage `org.foo:bar`, the one appearing later in `bom_imports` takes
//! precedence.  BOMs that themselves import other BOMs (via
//! `<scope>import</scope><type>pom</type>` in their own `<dependencyManagement>`)
//! are resolved recursively; the importing BOM's own entries win over the
//! entries from BOMs it imports.

use crate::gav::Gav;
use crate::pom::{self, BomRef, Pom};
use crate::repo::{default_repositories, Repository};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Options for the resolver.
pub struct ResolveOptions {
    /// Additional repositories to check after Maven Central.
    pub extra_repos: Vec<Repository>,
    /// When `true`, print progress to stdout.
    pub verbose: bool,
    /// BOMs to import, in ascending priority order (later index wins).
    /// Each entry is a GAV for a POM-packaged artifact whose
    /// `<dependencyManagement>` block provides version constraints.
    pub bom_imports: Vec<Gav>,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            extra_repos: vec![],
            verbose: true,
            bom_imports: vec![],
        }
    }
}

/// Walk the parent POM chain (up to 10 levels) and merge properties +
/// managed_versions into `pom`. Parent values only fill gaps — own values win.
fn merge_parent_chain(
    pom: &mut Pom,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
) {
    let mut depth = 0;
    let mut current_parent = pom.parent.clone();

    while let Some(parent_ref) = current_parent {
        depth += 1;
        if depth > 10 {
            break;
        }

        let parent_gav = Gav {
            group: parent_ref.group_id.clone(),
            artifact: parent_ref.artifact_id.clone(),
            version: parent_ref.version.clone(),
        };

        let pom_path = match ensure_artifact(&parent_gav, repos, client, ArtifactKind::Pom) {
            Ok(p) => p,
            Err(_) => break,
        };
        let xml = match std::fs::read_to_string(&pom_path) {
            Ok(s) => s,
            Err(_) => break,
        };
        let parent_pom = match pom::parse(&xml) {
            Ok(p) => p,
            Err(_) => break,
        };

        // Properties: parent fills gaps.
        for (k, v) in &parent_pom.properties {
            pom.properties.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // Managed versions: parent fills gaps.
        for (k, v) in &parent_pom.managed_versions {
            pom.managed_versions.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // BOM imports from parent are appended (parent has lower priority than own).
        for bom_ref in &parent_pom.bom_imports {
            pom.bom_imports.push(bom_ref.clone());
        }

        current_parent = parent_pom.parent.clone();
    }
}

/// Resolve a flat list of BOM GAVs into a combined `managed_versions` map.
///
/// Processing order implements **later-declared wins**:
/// - The caller passes BOMs in ascending priority order (later index = higher priority).
/// - We reverse the list so lower-priority BOMs are processed first, then
///   higher-priority BOMs overwrite with `insert`.
/// - BOMs that themselves import other BOMs (via `<scope>import</scope>` +
///   `<type>pom</type>`) are enqueued for processing immediately after the
///   importing BOM, so the importing BOM's own entries overwrite imported ones.
///
/// Cycles are prevented by a `visited` set keyed on `group:artifact:version`.
/// A work item in the BOM resolution queue.
enum BomWork {
    /// Fetch the BOM POM for this GAV, then expand it.
    Fetch(Gav),
    /// Apply pre-resolved managed versions directly to the output map.
    /// These entries come from a BOM that has already been fetched; they are
    /// deferred until after any nested BOM imports have been processed so that
    /// the importing BOM's own entries overwrite the nested BOMs' entries.
    Apply(HashMap<String, String>),
}

fn resolve_boms(
    bom_gavs: &[Gav],
    repos: &[Repository],
    client: &reqwest::blocking::Client,
) -> Result<HashMap<String, String>> {
    let mut managed: HashMap<String, String> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();

    // Process in forward order: later items in the input list are processed
    // later and therefore overwrite earlier items (later-declared wins).
    let mut queue: VecDeque<BomWork> = bom_gavs.iter().cloned().map(BomWork::Fetch).collect();

    while let Some(work) = queue.pop_front() {
        match work {
            BomWork::Apply(entries) => {
                // Deferred application of a BOM's own managed versions.
                // At this point all nested BOM imports have already been
                // applied, so inserting here lets this BOM's entries win.
                for (k, v) in entries {
                    managed.insert(k, v);
                }
            }

            BomWork::Fetch(gav) => {
                if !visited.insert(gav.notation()) {
                    continue; // already processed — prevent cycles
                }

                let pom_path = ensure_artifact(&gav, repos, client, ArtifactKind::Pom)
                    .with_context(|| format!("failed to fetch BOM POM for {}", gav))?;
                let xml = std::fs::read_to_string(&pom_path)
                    .with_context(|| format!("failed to read BOM POM {}", pom_path.display()))?;
                let mut pom = pom::parse(&xml)
                    .with_context(|| format!("failed to parse BOM POM for {}", gav))?;

                // Merge parent chain so inherited managed versions and properties are
                // available when resolving nested BOM import versions.
                merge_parent_chain(&mut pom, repos, client);

                // Collect this BOM's own managed versions for deferred application.
                let own_entries: HashMap<String, String> = pom
                    .managed_versions
                    .iter()
                    .map(|(k, v)| (k.clone(), pom.resolve_value(v)))
                    .collect();

                // Goal: process nested BOM imports first, then apply this
                // BOM's own entries so they overwrite the nested values.
                //
                // Target front-of-queue order:
                //   Fetch(nested_1), Fetch(nested_2), ..., Apply(own), <rest>
                //
                // Build it by pushing Apply(own) to the front first, then
                // each nested Fetch in reverse so nested_1 lands at the head.
                queue.push_front(BomWork::Apply(own_entries));
                for bom_ref in pom.bom_imports.iter().rev() {
                    let version = resolve_bom_ref_version(bom_ref, &pom);
                    if let Some(v) = version {
                        let nested_gav = Gav {
                            group: bom_ref.group_id.clone(),
                            artifact: bom_ref.artifact_id.clone(),
                            version: v,
                        };
                        queue.push_front(BomWork::Fetch(nested_gav));
                    }
                }
            }
        }
    }

    Ok(managed)
}

/// Resolve the version of a nested BOM reference, using the importing POM's
/// properties and managed versions for `${...}` substitution.
fn resolve_bom_ref_version(bom_ref: &BomRef, importing_pom: &Pom) -> Option<String> {
    let resolved = importing_pom.resolve_value(&bom_ref.version);
    if resolved.contains("${") {
        // Try managed_versions as a last resort.
        let key = format!("{}:{}", bom_ref.group_id, bom_ref.artifact_id);
        importing_pom
            .managed_versions
            .get(&key)
            .map(|v| importing_pom.resolve_value(v))
            .filter(|v| !v.contains("${"))
    } else {
        Some(resolved)
    }
}

/// Resolve a list of `(key, version)` pairs from `curie.toml` into a list of
/// local JAR paths (including transitive dependencies).
///
/// `deps` is a slice of `("group:artifact", "version")` pairs as declared
/// in the `[dependencies]` table of `curie.toml`.  An empty version string
/// (`""`) means the version must be supplied by one of the BOMs in
/// `opts.bom_imports`; it is a hard error if no BOM provides it.
pub fn resolve(
    deps: &[(&str, &str)],
    opts: &ResolveOptions,
) -> Result<Vec<PathBuf>> {
    let repos: Vec<Repository> = {
        let mut r = default_repositories();
        r.extend(opts.extra_repos.iter().cloned());
        r
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-build/0.1")
        .build()
        .context("failed to build HTTP client")?;

    // Pre-resolve all BOM managed versions before starting the BFS.
    let global_managed = resolve_boms(&opts.bom_imports, &repos, &client)?;

    // BFS queue of GAVs to resolve; visited set prevents duplicate work.
    let mut queue: VecDeque<Gav> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut ordered_jars: Vec<PathBuf> = Vec::new();

    // Seed with declared dependencies.
    for (key, version) in deps {
        let resolved_version: &str = if version.is_empty() {
            // Version comes from a BOM — hard error if not found.
            global_managed
                .get(*key)
                .with_context(|| format!(
                    "dependency \"{}\" has no version and is not managed by any BOM \
                     in [bom-imports]; either add a version or import a BOM that \
                     manages this artifact",
                    key
                ))?
                .as_str()
        } else {
            version
        };

        let gav = Gav::from_key_version(key, resolved_version)?;
        if visited.insert(gav.notation()) {
            queue.push_back(gav);
        }
    }

    while let Some(gav) = queue.pop_front() {
        if opts.verbose {
            print!("  Resolving {} ... ", gav);
        }

        // --- JAR ----------------------------------------------------------------
        let jar_path = ensure_artifact(&gav, &repos, &client, ArtifactKind::Jar)?;

        if opts.verbose {
            println!("OK");
        }

        ordered_jars.push(jar_path);

        // --- POM (for transitive deps) ------------------------------------------
        match ensure_artifact(&gav, &repos, &client, ArtifactKind::Pom) {
            Ok(pom_path) => {
                let xml = std::fs::read_to_string(&pom_path)
                    .with_context(|| format!("failed to read POM {}", pom_path.display()))?;
                let mut pom = pom::parse(&xml)
                    .with_context(|| format!("failed to parse POM for {}", gav))?;

                // Walk the full parent chain, merging properties and
                // dependencyManagement entries so that ${property} refs and
                // version-less deps resolve correctly (e.g. jackson-bom).
                merge_parent_chain(&mut pom, &repos, &client);

                for dep in pom.dependencies.iter().filter(|d| d.is_compile_scope()) {
                    let group = pom.resolve_value(&dep.group_id);
                    let artifact = pom.resolve_value(&dep.artifact_id);

                    // Resolve property placeholders in version, then fall back
                    // to dependencyManagement (from this POM or merged parent),
                    // then fall back to global BOM-managed versions.
                    let raw_version = match &dep.version {
                        Some(v) => {
                            let resolved = pom.resolve_value(v);
                            if resolved.contains("${") {
                                // Still unresolved — try managed_versions then global.
                                let key = format!("{}:{}", group, artifact);
                                match pom.managed_versions.get(&key)
                                    .or_else(|| global_managed.get(&key))
                                {
                                    Some(mv) => pom.resolve_value(mv),
                                    None => continue, // give up on this dep
                                }
                            } else {
                                resolved
                            }
                        }
                        None => {
                            // No version at all — look up in dependencyManagement
                            // then fall back to global BOM managed versions.
                            let key = format!("{}:{}", group, artifact);
                            match pom.managed_versions.get(&key)
                                .or_else(|| global_managed.get(&key))
                            {
                                Some(mv) => pom.resolve_value(mv),
                                None => continue, // skip version-less transitive dep
                            }
                        }
                    };

                    // Skip if version still unresolved after all attempts.
                    if raw_version.contains("${") {
                        continue;
                    }

                    let child_gav = Gav {
                        group,
                        artifact,
                        version: raw_version,
                    };

                    if visited.insert(child_gav.notation()) {
                        queue.push_back(child_gav);
                    }
                }
            }
            Err(_) => {
                // POM unavailable — continue without transitive expansion.
            }
        }
    }

    Ok(ordered_jars)
}

// ---------------------------------------------------------------------------

enum ArtifactKind {
    Jar,
    Pom,
}

/// Return the local path for an artifact, downloading it if necessary.
fn ensure_artifact(
    gav: &Gav,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    kind: ArtifactKind,
) -> Result<PathBuf> {
    let local_path = match kind {
        ArtifactKind::Jar => gav.local_cache_path()?,
        ArtifactKind::Pom => gav.local_pom_cache_path()?,
    };
    let relative = match kind {
        ArtifactKind::Jar => gav.relative_path(),
        ArtifactKind::Pom => gav.relative_pom_path(),
    };

    if local_path.exists() {
        return Ok(local_path);
    }

    // Ensure parent directory exists.
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }

    // Try each repository in order.
    let mut last_err: Option<anyhow::Error> = None;
    for repo in repos {
        let url = repo.artifact_url(&relative);
        match download(client, &url, &local_path) {
            Ok(()) => return Ok(local_path),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    bail!(
        "failed to download {} from all repositories: {}",
        gav,
        last_err.unwrap()
    );
}

/// Download `url` to `dest`, using an adjacent `.part` file to avoid partial
/// writes surviving a crash.
fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed for {}", url))?;

    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }

    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read response body for {}", url))?;

    // Write to .part file then rename (atomic on POSIX).
    let part = dest.with_extension("part");
    std::fs::write(&part, &bytes)
        .with_context(|| format!("failed to write {}", part.display()))?;
    std::fs::rename(&part, dest)
        .with_context(|| format!("failed to rename {} → {}", part.display(), dest.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Serialise all tests that mutate HOME to prevent races.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Build a minimal BOM POM XML string.
    fn make_bom_pom(
        group: &str,
        artifact: &str,
        version: &str,
        managed: &[(&str, &str, &str)],   // (group, artifact, version)
        bom_imports: &[(&str, &str, &str)], // (group, artifact, version) with scope=import type=pom
    ) -> String {
        let mut xml = format!(
            r#"<?xml version="1.0"?>
<project>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{version}</version>
  <dependencyManagement>
    <dependencies>
"#
        );
        for (g, a, v) in managed {
            xml.push_str(&format!(
                "      <dependency>\
\n        <groupId>{g}</groupId>\
\n        <artifactId>{a}</artifactId>\
\n        <version>{v}</version>\
\n      </dependency>\n"
            ));
        }
        for (g, a, v) in bom_imports {
            xml.push_str(&format!(
                "      <dependency>\
\n        <groupId>{g}</groupId>\
\n        <artifactId>{a}</artifactId>\
\n        <version>{v}</version>\
\n        <type>pom</type>\
\n        <scope>import</scope>\
\n      </dependency>\n"
            ));
        }
        xml.push_str("    </dependencies>\n  </dependencyManagement>\n</project>");
        xml
    }

    /// Write a BOM POM into a fake local Maven cache under `home_dir` (i.e. at
    /// `<home_dir>/.m2/repository/<rel_path>`) and return the Gav.
    fn write_fake_bom(
        home_dir: &std::path::Path,
        group: &str,
        artifact: &str,
        version: &str,
        managed: &[(&str, &str, &str)],
        bom_imports: &[(&str, &str, &str)],
    ) -> Gav {
        let gav = Gav {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
        };
        let rel = gav.relative_pom_path();
        let path = home_dir.join(".m2").join("repository").join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let xml = make_bom_pom(group, artifact, version, managed, bom_imports);
        std::fs::write(&path, xml).unwrap();
        gav
    }

    /// Invoke `resolve_boms` with a fake home directory so `local_pom_cache_path()`
    /// resolves under `<home_dir>/.m2/repository`.  No network is required — all
    /// POMs must be pre-written by `write_fake_bom`.
    ///
    /// Acquires `HOME_LOCK` to prevent parallel tests from racing on the HOME
    /// environment variable.
    fn run_resolve_boms(
        home_dir: &std::path::Path,
        bom_gavs: &[Gav],
    ) -> Result<HashMap<String, String>> {
        let _guard = HOME_LOCK.lock().unwrap();
        // Override HOME so Gav::local_pom_cache_path() finds our fake cache.
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());
        let repos: Vec<Repository> = vec![]; // no network — all POMs pre-cached
        let client = reqwest::blocking::Client::builder()
            .user_agent("test")
            .build()
            .unwrap();
        let result = resolve_boms(bom_gavs, &repos, &client);
        // Restore HOME.
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    #[test]
    fn single_bom_managed_versions_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let gav = write_fake_bom(
            dir.path(),
            "com.example", "my-bom", "1.0.0",
            &[("org.foo", "bar", "3.2.1"), ("org.foo", "baz", "3.2.1")],
            &[],
        );
        let result = run_resolve_boms(dir.path(), &[gav]).unwrap();
        assert_eq!(result.get("org.foo:bar").map(String::as_str), Some("3.2.1"));
        assert_eq!(result.get("org.foo:baz").map(String::as_str), Some("3.2.1"));
    }

    #[test]
    fn later_bom_wins_over_earlier_bom() {
        let dir = tempfile::tempdir().unwrap();
        let bom_a = write_fake_bom(
            dir.path(),
            "com.example", "bom-a", "1.0.0",
            &[("org.foo", "bar", "1.0.0")],
            &[],
        );
        let bom_b = write_fake_bom(
            dir.path(),
            "com.example", "bom-b", "1.0.0",
            &[("org.foo", "bar", "2.0.0")],
            &[],
        );
        // bom-b is later → should win
        let result = run_resolve_boms(dir.path(), &[bom_a, bom_b]).unwrap();
        assert_eq!(result.get("org.foo:bar").map(String::as_str), Some("2.0.0"));
    }

    #[test]
    fn importing_bom_wins_over_nested_bom_import() {
        let dir = tempfile::tempdir().unwrap();
        // nested-bom says org.foo:bar = 1.0.0
        write_fake_bom(
            dir.path(),
            "com.example", "nested-bom", "1.0.0",
            &[("org.foo", "bar", "1.0.0")],
            &[],
        );
        // outer-bom imports nested-bom AND overrides org.foo:bar to 9.9.9
        let outer = write_fake_bom(
            dir.path(),
            "com.example", "outer-bom", "1.0.0",
            &[("org.foo", "bar", "9.9.9")],
            &[("com.example", "nested-bom", "1.0.0")],
        );
        let result = run_resolve_boms(dir.path(), &[outer]).unwrap();
        // outer-bom wins over nested-bom for the same key
        assert_eq!(result.get("org.foo:bar").map(String::as_str), Some("9.9.9"));
    }

    #[test]
    fn bom_cycle_does_not_loop_forever() {
        let dir = tempfile::tempdir().unwrap();
        // bom-a imports bom-b, bom-b imports bom-a → cycle
        write_fake_bom(
            dir.path(),
            "com.example", "bom-a", "1.0.0",
            &[("org.foo", "x", "1.0")],
            &[("com.example", "bom-b", "1.0.0")],
        );
        write_fake_bom(
            dir.path(),
            "com.example", "bom-b", "1.0.0",
            &[("org.foo", "y", "2.0")],
            &[("com.example", "bom-a", "1.0.0")],
        );
        let bom_a = Gav { group: "com.example".into(), artifact: "bom-a".into(), version: "1.0.0".into() };
        let result = run_resolve_boms(dir.path(), &[bom_a]).unwrap();
        assert_eq!(result.get("org.foo:x").map(String::as_str), Some("1.0"));
        assert_eq!(result.get("org.foo:y").map(String::as_str), Some("2.0"));
    }

    #[test]
    fn empty_bom_list_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_resolve_boms(dir.path(), &[]).unwrap();
        assert!(result.is_empty());
    }
}
