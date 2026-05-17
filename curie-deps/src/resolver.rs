//! Dependency resolver: cache lookup → download → transitive expansion.
//!
//! # Algorithm
//! 1. For each declared `Gav`, check `~/.m2/repository` for the JAR and POM.
//! 2. On cache miss, try each configured repository in order; download to a
//!    `.part` file, rename atomically on success.
//! 3. Parse the POM to discover compile-scoped transitive dependencies.
//! 4. Recurse (BFS) until the full closure is resolved.  Only POMs are
//!    fetched during BFS — this is Phase 1.
//! 5. **Phase 2**: download all JARs in parallel (up to 8 concurrent threads)
//!    once the full transitive closure is known.
//! 6. Return all resolved JAR paths in stable topological order
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
//!
//! # Offline mode
//! When [`ResolveOptions::offline`] is `true`, network calls are skipped
//! entirely.  Any artifact not already present in `~/.m2/repository` is an
//! immediate error.

use crate::gav::Gav;
use crate::pom::{self, BomRef, Pom};
use crate::repo::{default_repositories, Repository};
use anyhow::{bail, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Options for the resolver.
pub struct ResolveOptions {
    /// Additional repositories to check after Maven Central.
    pub extra_repos: Vec<Repository>,
    /// When `true`, show a progress bar on stderr while downloading JARs.
    pub progress: bool,
    /// BOMs to import, in ascending priority order (later index wins).
    /// Each entry is a GAV for a POM-packaged artifact whose
    /// `<dependencyManagement>` block provides version constraints.
    pub bom_imports: Vec<Gav>,
    /// When `true`, skip all network calls.  Any artifact that is not already
    /// present in the local `~/.m2/repository` cache causes an immediate error.
    pub offline: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            extra_repos: vec![],
            progress: true,
            bom_imports: vec![],
            offline: false,
        }
    }
}

/// Walk the parent POM chain (up to 10 levels) and merge properties +
/// managed_versions into `pom`. Parent values only fill gaps — own values win.
fn merge_parent_chain(
    pom: &mut Pom,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    offline: bool,
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

        let pom_path = match ensure_artifact(&parent_gav, repos, client, ArtifactKind::Pom, offline, None, None) {
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

/// Fetch (or load from cache) the POM for `gav`, parse it, and merge its
/// parent chain.  Returns the fully-resolved [`Pom`] ready for dependency
/// expansion.
///
/// This is the scaffolding shared by [`resolve_boms`] and the main BFS in
/// [`resolve`]: both need a POM, its properties resolved, and its parent-chain
/// managed versions merged in before they can inspect dependencies or
/// managed-version entries.
fn fetch_and_parse_pom(
    gav: &Gav,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    offline: bool,
) -> Result<Pom> {
    let pom_path = ensure_artifact(gav, repos, client, ArtifactKind::Pom, offline, None, None)
        .with_context(|| format!("failed to fetch POM for {}", gav))?;
    let xml = std::fs::read_to_string(&pom_path)
        .with_context(|| format!("failed to read POM {}", pom_path.display()))?;
    let mut pom = pom::parse(&xml)
        .with_context(|| format!("failed to parse POM for {}", gav))?;
    merge_parent_chain(&mut pom, repos, client, offline);
    Ok(pom)
}

fn resolve_boms(
    bom_gavs: &[Gav],
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    offline: bool,
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

                let pom = fetch_and_parse_pom(&gav, repos, client, offline)
                    .with_context(|| format!("failed to fetch BOM POM for {}", gav))?;

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

/// Resolve a list of `(key, version)` pairs from `Curie.toml` into a list of
/// local JAR paths (including transitive dependencies).
///
/// `deps` is a slice of `("group:artifact", "version")` pairs as declared
/// in the `[dependencies]` table of `Curie.toml`.  An empty version string
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
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    // Pre-resolve all BOM managed versions before starting the BFS.
    let global_managed = resolve_boms(&opts.bom_imports, &repos, &client, opts.offline)?;

    // -----------------------------------------------------------------------
    // Phase 1: serial BFS over POMs to discover the full transitive closure.
    //
    // We only fetch POMs here (small, fast) and record the ordered list of
    // GAVs.  JARs are downloaded in Phase 2 in parallel.
    // -----------------------------------------------------------------------
    //
    // BFS queue + visited set keyed on `group:artifact` (NOT full GAV).
    //
    // Maven's conflict-resolution rule is **nearest wins**: when the same
    // artifact appears at multiple depths with different versions, the
    // shallowest occurrence wins.  BFS naturally produces shallowest-first
    // visitation, so a GA-keyed visited set gives us this for free: once we
    // commit to a version for a `group:artifact`, every later encounter
    // (necessarily at equal or greater depth) is skipped.
    //
    // At equal depth, the first dep encountered in BFS order wins — which
    // matches Maven's "first declared wins" tiebreaker.
    let mut queue: VecDeque<Gav> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    // Ordered list of GAVs in BFS discovery order — used in Phase 2.
    let mut ordered_gavs: Vec<Gav> = Vec::new();

    // Seed with declared dependencies.  At depth 0 the user's explicit
    // version always wins — top-level `<dependencyManagement>` (BOMs) only
    // fills in when the dep's version is empty.
    for (key, version) in deps {
        let resolved_version: String = if version.is_empty() {
            // Version comes from a BOM — hard error if not found.
            global_managed
                .get(*key)
                .with_context(|| format!(
                    "dependency \"{}\" has no version and is not managed by any BOM \
                     in [bom-imports]; either add a version or import a BOM that \
                     manages this artifact",
                    key
                ))?
                .clone()
        } else {
            (*version).to_string()
        };

        let gav = Gav::from_key_version(key, &resolved_version)?;
        let ga = format!("{}:{}", gav.group, gav.artifact);
        if visited.insert(ga) {
            queue.push_back(gav);
        }
    }

    while let Some(gav) = queue.pop_front() {
        ordered_gavs.push(gav.clone());

        // Fetch POM to expand transitive dependencies.
        match fetch_and_parse_pom(&gav, &repos, &client, opts.offline) {
            Ok(pom) => {
                for dep in pom.dependencies.iter().filter(|d| d.is_compile_scope()) {
                    let group = pom.resolve_value(&dep.group_id);
                    let artifact = pom.resolve_value(&dep.artifact_id);
                    let ga_key = format!("{}:{}", group, artifact);

                    // Nearest-wins short-circuit: a shallower BFS layer already
                    // committed to a version for this GA — skip without even
                    // resolving the version.
                    if visited.contains(&ga_key) {
                        continue;
                    }

                    let raw_version = match resolve_transitive_version(
                        &ga_key,
                        dep.version.as_deref(),
                        &pom,
                        &global_managed,
                    ) {
                        Some(v) => v,
                        None => continue, // unresolvable — drop this dep
                    };

                    let child_gav = Gav { group, artifact, version: raw_version };
                    visited.insert(ga_key);
                    queue.push_back(child_gav);
                }
            }
            Err(_) => {
                // POM unavailable — continue without transitive expansion.
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: download JARs in parallel.
    //
    // We spawn up to PARALLEL_DOWNLOADS threads, each pulling one JAR at a
    // time from the shared work queue.  Results are collected into a
    // pre-allocated Vec<Result<PathBuf>> indexed by the original BFS order so
    // the returned classpath is deterministic.
    // -----------------------------------------------------------------------
    const PARALLEL_DOWNLOADS: usize = 8;

    let n = ordered_gavs.len();
    if n == 0 {
        return Ok(vec![]);
    }

    // Count how many JARs are not yet in the local cache — only those will
    // be downloaded and shown on the progress bar.
    let missing: u64 = ordered_gavs
        .iter()
        .filter(|g| {
            g.local_cache_path()
                .map(|p| !p.exists())
                .unwrap_or(false)
        })
        .count() as u64;

    // Build a MultiProgress only when there is something to download and the
    // caller opted in to progress reporting.
    //
    // Layout:
    //   summary bar:  "  Downloading     [=========>---]  3/8"
    //   per-thread:   "    ⠸ org.foo:bar:1.2.3"   (one line per active thread)
    let thread_count = PARALLEL_DOWNLOADS.min(n);

    let (mp, summary_pb, thread_pbs): (
        Option<MultiProgress>,
        Option<ProgressBar>,
        Vec<Option<ProgressBar>>,
    ) = if opts.progress && missing > 0 {
        let mp = MultiProgress::new();

        let summary = mp.add(ProgressBar::new(missing));
        summary.set_style(
            ProgressStyle::with_template(
                "  Downloading     [{bar:40.cyan/blue}] {pos}/{len}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );

        let spinner_style = ProgressStyle::with_template("    {spinner} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");

        let thread_pbs: Vec<Option<ProgressBar>> = (0..thread_count)
            .map(|_| {
                let sp = mp.add(ProgressBar::new_spinner());
                sp.set_style(spinner_style.clone());
                Some(sp)
            })
            .collect();

        (Some(mp), Some(summary), thread_pbs)
    } else {
        let nones = (0..thread_count).map(|_| None).collect();
        (None, None, nones)
    };

    // Shared atomic index into `ordered_gavs`.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut jar_results: Vec<Option<Result<PathBuf>>> = (0..n).map(|_| None).collect();

    // We need shared access to repos/client/gavs across threads.  Since all
    // are read-only after construction, wrap in references and use
    // std::thread::scope for safe borrowing.
    let mut per_thread: Vec<Vec<(usize, Result<PathBuf>)>> = Vec::new();

    // Borrow shared data as refs so each spawned closure can capture them
    // without moving.  `thread::scope` guarantees these refs are valid for
    // the lifetime of all spawned threads.
    let next_ref = &next;
    let gavs_ref = &ordered_gavs;
    let repos_ref = &repos;
    let client_ref = &client;
    let offline = opts.offline;

    std::thread::scope(|s| -> Result<()> {
        let handles: Vec<_> = thread_pbs
            .iter()
            .map(|thread_pb| {
                let summary_pb = summary_pb.clone();
                let thread_pb = thread_pb.clone();
                s.spawn(move || -> Vec<(usize, Result<PathBuf>)> {
                    let mut local = Vec::new();
                    loop {
                        let idx = next_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if idx >= n {
                            break;
                        }
                        let gav = &gavs_ref[idx];
                        let result = ensure_artifact(
                            gav,
                            repos_ref,
                            client_ref,
                            ArtifactKind::Jar,
                            offline,
                            summary_pb.as_ref(),
                            thread_pb.as_ref(),
                        );
                        local.push((idx, result));
                    }
                    // Thread is done — clear its spinner line.
                    if let Some(sp) = &thread_pb {
                        sp.finish_and_clear();
                    }
                    local
                })
            })
            .collect();

        for handle in handles {
            per_thread.push(handle.join().unwrap_or_default());
        }
        Ok(())
    })?;

    // Downloads complete — clear all progress output.
    if let Some(bar) = summary_pb {
        bar.finish_and_clear();
    }
    if let Some(mp) = mp {
        let _ = mp.clear();
    }

    for thread_results in per_thread {
        for (idx, result) in thread_results {
            jar_results[idx] = Some(result);
        }
    }

    // Collect in BFS order, propagating any download errors.
    let mut ordered_jars = Vec::with_capacity(n);
    for (idx, slot) in jar_results.into_iter().enumerate() {
        let path = slot
            .unwrap_or_else(|| bail!("internal: no result for index {}", idx))
            .with_context(|| format!("failed to download JAR for {}", gavs_ref[idx]))?;
        ordered_jars.push(path);
    }

    Ok(ordered_jars)
}

/// Resolve the version a transitive dependency should be pinned to, applying
/// Maven precedence rules:
///
/// 1. **Top-level BOM override** (`global_managed`): the user's own
///    `<dependencyManagement>` (via `[bom-imports]`) wins over any version the
///    dep's own POM declares.  This is what lets a project pin all Jackson
///    artifacts to a single version even when transitive POMs hard-code a
///    different one.
/// 2. **Dep's explicit `<version>`** (resolved against the importing POM's
///    properties).  Used only when the top-level BOM does not manage this GA.
/// 3. **Dep's own `<dependencyManagement>`** (own + merged parent chain),
///    consulted when the dep declares no version or only an unresolvable
///    `${property}` reference.
///
/// Returns `None` when the version still contains an unresolved `${...}` after
/// every fallback — the caller drops the dep rather than emit a broken GAV.
fn resolve_transitive_version(
    ga_key: &str,
    dep_explicit: Option<&str>,
    pom: &Pom,
    global_managed: &HashMap<String, String>,
) -> Option<String> {
    // 1. Top-level BOM override (Maven's <dependencyManagement> at the project
    //    POM wins over transitive explicit versions).
    if let Some(bom_v) = global_managed.get(ga_key) {
        let resolved = pom.resolve_value(bom_v);
        if !resolved.contains("${") {
            return Some(resolved);
        }
        // BOM value still references a ${...}; fall through to other sources.
    }

    // 2. Dep's explicit version, resolved against properties.
    if let Some(v) = dep_explicit {
        let resolved = pom.resolve_value(v);
        if !resolved.contains("${") {
            return Some(resolved);
        }
        // Unresolved property: try dep's own managed_versions, then global.
        if let Some(mv) = pom
            .managed_versions
            .get(ga_key)
            .or_else(|| global_managed.get(ga_key))
        {
            let r = pom.resolve_value(mv);
            if !r.contains("${") {
                return Some(r);
            }
        }
        return None;
    }

    // 3. No explicit version — fall back to dep's own managed_versions, then
    //    global BOM map.
    let mv = pom
        .managed_versions
        .get(ga_key)
        .or_else(|| global_managed.get(ga_key))?;
    let resolved = pom.resolve_value(mv);
    if resolved.contains("${") {
        None
    } else {
        Some(resolved)
    }
}

// ---------------------------------------------------------------------------

enum ArtifactKind {
    Jar,
    Pom,
}

/// Return the local path for an artifact, downloading it if necessary.
///
/// When `offline` is `true`, any cache miss is an immediate error — no HTTP
/// call is attempted.
///
/// `summary_pb` is the top-level counter bar (incremented on each successful
/// download).  `thread_pb` is this thread's spinner line (message set to the
/// GAV being fetched, cleared when the download finishes).
fn ensure_artifact(
    gav: &Gav,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    kind: ArtifactKind,
    offline: bool,
    summary_pb: Option<&ProgressBar>,
    thread_pb: Option<&ProgressBar>,
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

    if offline {
        bail!(
            "artifact {} is not in the local cache and --offline was specified",
            gav
        );
    }

    // Ensure parent directory exists.
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }

    // Show which artifact is being fetched on this thread's spinner line.
    if let Some(sp) = thread_pb {
        sp.set_message(gav.notation());
        sp.enable_steady_tick(std::time::Duration::from_millis(80));
    }

    // Try each repository in order.
    let mut last_err: Option<anyhow::Error> = None;
    for repo in repos {
        let url = repo.artifact_url(&relative);
        match download(client, &url, &local_path) {
            Ok(()) => {
                if let Some(bar) = summary_pb {
                    bar.inc(1);
                }
                return Ok(local_path);
            }
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
        let result = resolve_boms(bom_gavs, &repos, &client, true);
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

    // -----------------------------------------------------------------------
    // Maven conflict-resolution tests for `resolve()`
    //
    // These exercise the full BFS using fake POMs + empty JAR files written
    // to a temp `.m2/repository`.  The resolver short-circuits on
    // `local_path.exists()`, so empty JAR files are sufficient — we only
    // care which versions end up in the output, not their contents.
    // -----------------------------------------------------------------------

    /// Build a regular (non-BOM) POM with a flat `<dependencies>` list.
    /// Every dependency is rendered with `<scope>compile</scope>`.
    fn make_pom(
        group: &str,
        artifact: &str,
        version: &str,
        deps: &[(&str, &str, &str)],  // (group, artifact, version)
    ) -> String {
        let mut xml = format!(
            r#"<?xml version="1.0"?>
<project>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{version}</version>
  <dependencies>
"#
        );
        for (g, a, v) in deps {
            xml.push_str(&format!(
                "    <dependency>\
\n      <groupId>{g}</groupId>\
\n      <artifactId>{a}</artifactId>\
\n      <version>{v}</version>\
\n      <scope>compile</scope>\
\n    </dependency>\n"
            ));
        }
        xml.push_str("  </dependencies>\n</project>");
        xml
    }

    /// Write both a POM and an empty placeholder JAR into the fake local
    /// Maven cache rooted at `home_dir`.  Returns the artifact's Gav.
    fn write_fake_artifact(
        home_dir: &std::path::Path,
        group: &str,
        artifact: &str,
        version: &str,
        deps: &[(&str, &str, &str)],
    ) -> Gav {
        let gav = Gav {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
        };
        let m2 = home_dir.join(".m2").join("repository");

        // POM.
        let pom_path = m2.join(gav.relative_pom_path());
        std::fs::create_dir_all(pom_path.parent().unwrap()).unwrap();
        std::fs::write(&pom_path, make_pom(group, artifact, version, deps)).unwrap();

        // Empty JAR (placeholder — resolver only checks existence).
        let jar_path = m2.join(gav.relative_path());
        std::fs::write(&jar_path, b"").unwrap();

        gav
    }

    /// Invoke `resolve()` with a fake home directory.  No network is
    /// performed — `extra_repos` is empty and `default_repositories` (Maven
    /// Central) is unreachable in the test, so every artifact must be
    /// pre-written via `write_fake_artifact`.
    fn run_resolve(
        home_dir: &std::path::Path,
        deps: &[(&str, &str)],
        bom_imports: Vec<Gav>,
    ) -> Result<Vec<PathBuf>> {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());

        // All artifacts are pre-cached; use offline mode so any accidental
        // cache miss produces an immediate error rather than a network attempt.
        let opts = ResolveOptions {
            extra_repos: vec![],
            progress: false,
            bom_imports,
            offline: true,
        };
        let result = resolve(deps, &opts);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    /// Extract the `group:artifact:version` of every resolved JAR for
    /// readable assertions.  Reverses `relative_path()`.
    fn jar_gavs(jars: &[PathBuf]) -> Vec<String> {
        jars.iter()
            .map(|p| {
                // …/group/path/artifact/version/artifact-version.jar
                // Take the last three components: artifact/version/filename.
                let comps: Vec<_> = p.components().collect();
                let n = comps.len();
                // Walk back: filename → version dir → artifact dir → group dirs.
                let filename = comps[n - 1].as_os_str().to_string_lossy().into_owned();
                let version = comps[n - 2].as_os_str().to_string_lossy().into_owned();
                let artifact = comps[n - 3].as_os_str().to_string_lossy().into_owned();
                // Group is everything between `.m2/repository/` and the
                // artifact dir, joined with dots.
                let mut group_parts: Vec<String> = Vec::new();
                let mut seen_repo = false;
                for c in &comps[..n - 3] {
                    let s = c.as_os_str().to_string_lossy().into_owned();
                    if seen_repo {
                        group_parts.push(s);
                    } else if s == "repository" {
                        seen_repo = true;
                    }
                }
                let _ = filename; // unused; kept for clarity in destructuring
                format!("{}:{}:{}", group_parts.join("."), artifact, version)
            })
            .collect()
    }

    #[test]
    fn declared_dep_overrides_transitive_version() {
        // User declares foo:bar 1.0 directly AND foo:other 1.0 which
        // transitively pulls foo:bar 2.0.  Maven nearest-wins: bar 1.0.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(
            dir.path(), "foo", "other", "1.0",
            &[("foo", "bar", "2.0")],
        );

        let result = run_resolve(
            dir.path(),
            &[("foo:bar", "1.0"), ("foo:other", "1.0")],
            vec![],
        ).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "expected foo:bar:1.0 in {:?}", gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:2.0".to_string()),
            "foo:bar:2.0 must not appear (nearest wins): {:?}", gavs,
        );
    }

    #[test]
    fn first_declared_wins_at_same_depth() {
        // Two declared deps, both at depth 0, each pulling a different
        // version of foo:bar at depth 1.  BFS visits a's children before
        // b's children → a's version (1.0) wins.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(
            dir.path(), "grp", "a", "1.0",
            &[("foo", "bar", "1.0")],
        );
        write_fake_artifact(
            dir.path(), "grp", "b", "1.0",
            &[("foo", "bar", "2.0")],
        );

        let result = run_resolve(
            dir.path(),
            // Note: BTreeMap ordering would sort these alphabetically;
            // the resolver receives the &[(&str, &str)] slice in caller
            // order, so a comes before b here.
            &[("grp:a", "1.0"), ("grp:b", "1.0")],
            vec![],
        ).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "first-declared a's choice (foo:bar:1.0) must win: {:?}", gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:2.0".to_string()),
            "b's choice (foo:bar:2.0) must lose to a's: {:?}", gavs,
        );
    }

    #[test]
    fn top_level_bom_overrides_transitive_explicit_version() {
        // User declares dep on grp:lib 1.0 which transitively pins
        // foo:bar 2.0.  User also imports a BOM that pins foo:bar to 9.9.9.
        // Maven rule: top-level <dependencyManagement> wins over transitive
        // explicit versions.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "9.9.9", &[]);
        write_fake_artifact(
            dir.path(), "grp", "lib", "1.0",
            &[("foo", "bar", "2.0")],
        );
        let bom = write_fake_bom(
            dir.path(),
            "com.example", "pin-bom", "1.0.0",
            &[("foo", "bar", "9.9.9")],
            &[],
        );

        let result = run_resolve(
            dir.path(),
            &[("grp:lib", "1.0")],
            vec![bom],
        ).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:9.9.9".to_string()),
            "top-level BOM (9.9.9) must override transitive explicit (2.0): {:?}", gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:2.0".to_string()),
            "transitive explicit 2.0 must be overridden by BOM: {:?}", gavs,
        );
    }

    #[test]
    fn user_explicit_version_wins_over_top_level_bom() {
        // User declares foo:bar 1.0 directly AND imports a BOM pinning bar
        // to 9.9.9.  Maven rule: a top-level <dependency> with an explicit
        // version wins over the project's own <dependencyManagement>.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "9.9.9", &[]);
        let bom = write_fake_bom(
            dir.path(),
            "com.example", "pin-bom", "1.0.0",
            &[("foo", "bar", "9.9.9")],
            &[],
        );

        let result = run_resolve(
            dir.path(),
            &[("foo:bar", "1.0")],
            vec![bom],
        ).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "user's explicit declaration (1.0) must win over BOM (9.9.9): {:?}", gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:9.9.9".to_string()),
            "BOM-pinned version must lose to user's explicit version: {:?}", gavs,
        );
    }
}
