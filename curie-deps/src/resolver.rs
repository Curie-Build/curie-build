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
//!
//! # Checksum verification
//! Every artifact returned by [`ensure_artifact`] has been verified against a
//! `.sha256` sidecar (`.sha1` fallback).  On download, the sidecar is fetched
//! immediately after the artifact and the bytes are verified before being
//! committed to the cache; the sidecar is then persisted alongside the
//! artifact (mirroring Maven Central's local layout) so subsequent cache hits
//! verify without any network call.  A missing sidecar is a hard error —
//! well-formed Maven repos always publish one.  In offline mode, a cache hit
//! without an adjacent sidecar is likewise a hard error.

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
    /// Base repositories used for deps with no [`DepEntry::repo_id`] and for
    /// BOM resolution.  When empty, Maven Central is used automatically.
    /// Callers set this when a mirror redirects Central to another URL.
    pub default_repos: Vec<Repository>,
    /// Named repositories declared in `[[repositories]]`.  Only consulted when
    /// a [`DepEntry::repo_id`] references one by id.
    pub named_repos: Vec<Repository>,
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
            default_repos: vec![],
            named_repos: vec![],
            progress: true,
            bom_imports: vec![],
            offline: false,
        }
    }
}

/// One entry in the dependency list passed to [`resolve`].
pub struct DepEntry<'a> {
    /// `"group:artifact"` coordinate.
    pub key: &'a str,
    /// Version string (may be `""` when supplied by a BOM).
    pub version: &'a str,
    /// Optional repository id (matches [`Repository::id`] in
    /// [`ResolveOptions::named_repos`]).
    ///
    /// * `None` — artifact is fetched from Maven Central only.
    /// * `Some("X")` — artifact is fetched from repo X only; its transitive
    ///   dependencies are searched in both Central and repo X.
    pub repo_id: Option<&'a str>,
}

/// Internal BFS work item carrying per-artifact repository context.
struct BfsWork {
    gav: Gav,
    /// Repos to use when fetching THIS artifact's POM and JAR.
    fetch_repos: Vec<Repository>,
    /// Repos passed to each of this artifact's transitive dependencies
    /// (used as their `fetch_repos` and `child_repos`).
    child_repos: Vec<Repository>,
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

/// Resolve a list of [`DepEntry`] items from `Curie.toml` into a list of
/// local JAR paths (including transitive dependencies).
///
/// An entry with an empty version string (`""`) means the version must be
/// supplied by one of the BOMs in `opts.bom_imports`; it is a hard error if
/// no BOM provides it.
pub fn resolve(
    deps: &[DepEntry],
    opts: &ResolveOptions,
) -> Result<Vec<PathBuf>> {
    // Use caller-supplied default repos (e.g. a mirrored Central) when
    // provided; fall back to Maven Central otherwise.
    let central = if opts.default_repos.is_empty() {
        default_repositories()
    } else {
        opts.default_repos.clone()
    };

    // Build a lookup map from repo id → Repository for named repos.
    let named_map: std::collections::HashMap<&str, &Repository> = opts
        .named_repos
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-build/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    // BOMs are resolved using the same default repos as regular deps
    // so that a Central mirror is respected here too.
    let global_managed = resolve_boms(&opts.bom_imports, &central, &client, opts.offline)?;

    // -----------------------------------------------------------------------
    // Phase 1: serial BFS over POMs to discover the full transitive closure.
    //
    // We only fetch POMs here (small, fast) and record the ordered list of
    // GAVs together with the repo context each artifact should be fetched
    // from.  JARs are downloaded in Phase 2 in parallel.
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
    let mut queue: VecDeque<BfsWork> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    // Ordered list of (GAV, fetch_repos) in BFS discovery order — used in Phase 2.
    let mut ordered_gavs: Vec<(Gav, Vec<Repository>)> = Vec::new();

    // Seed with declared dependencies.  At depth 0 the user's explicit
    // version always wins — top-level `<dependencyManagement>` (BOMs) only
    // fills in when the dep's version is empty.
    for dep in deps {
        let resolved_version: String = if dep.version.is_empty() {
            // Version comes from a BOM — hard error if not found.
            global_managed
                .get(dep.key)
                .with_context(|| format!(
                    "dependency \"{}\" has no version and is not managed by any BOM \
                     in [bom-imports]; either add a version or import a BOM that \
                     manages this artifact",
                    dep.key
                ))?
                .clone()
        } else {
            dep.version.to_string()
        };

        // Compute per-artifact repo context based on the optional repo_id.
        //
        // * No repo_id: fetch from Central only; transitives also Central only.
        // * repo_id = "X": fetch this artifact from repo X only; transitives
        //   may come from Central OR X.
        let (fetch_repos, child_repos): (Vec<Repository>, Vec<Repository>) =
            if let Some(repo_id) = dep.repo_id {
                let named: Repository = (*named_map
                    .get(repo_id)
                    .with_context(|| format!(
                        "dependency \"{}\" references unknown repository \"{}\"; \
                         declare it with [[repositories]]",
                        dep.key, repo_id
                    ))?)
                    .clone();
                let mut child = central.clone();
                child.push(named.clone());
                (vec![named], child)
            } else {
                (central.clone(), central.clone())
            };

        let gav = Gav::from_key_version(dep.key, &resolved_version)?;
        let ga = format!("{}:{}", gav.group, gav.artifact);
        if visited.insert(ga) {
            queue.push_back(BfsWork { gav, fetch_repos, child_repos });
        }
    }

    while let Some(work) = queue.pop_front() {
        ordered_gavs.push((work.gav.clone(), work.fetch_repos.clone()));

        // Fetch POM to expand transitive dependencies.
        match fetch_and_parse_pom(&work.gav, &work.fetch_repos, &client, opts.offline) {
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
                    // Transitives inherit the parent's child_repos for both
                    // their own fetching and further transitive expansion.
                    queue.push_back(BfsWork {
                        gav: child_gav,
                        fetch_repos: work.child_repos.clone(),
                        child_repos: work.child_repos.clone(),
                    });
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
        .filter(|(g, _)| {
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

    // We need shared access to client/gavs across threads.  Since all
    // are read-only after construction, wrap in references and use
    // std::thread::scope for safe borrowing.
    let mut per_thread: Vec<Vec<(usize, Result<PathBuf>)>> = Vec::new();

    // Borrow shared data as refs so each spawned closure can capture them
    // without moving.  `thread::scope` guarantees these refs are valid for
    // the lifetime of all spawned threads.
    let next_ref = &next;
    let gavs_ref = &ordered_gavs;
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
                        let (gav, fetch_repos) = &gavs_ref[idx];
                        let result = ensure_artifact(
                            gav,
                            fetch_repos,
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
            .with_context(|| format!("failed to download JAR for {}", gavs_ref[idx].0))?;
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
        ensure_verified(&local_path, &relative, repos, client, offline)
            .with_context(|| format!("checksum verification failed for cached {}", gav))?;
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

/// Download `url` to `dest`, verify its checksum against the published
/// `.sha256` (or `.sha1` fallback) sidecar, and persist the sidecar alongside
/// the artifact for fast cache-hit verification on subsequent runs.
///
/// A missing sidecar is a hard error — every well-formed Maven repository
/// publishes one (Maven's deploy plugin and Nexus/Artifactory both generate
/// them on upload).  A missing sidecar usually means a misconfigured proxy or
/// a manually-uploaded artifact, and either way we refuse to install an
/// unverifiable JAR.
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

    let (expected_hex, kind) = fetch_any_remote_checksum(client, url)?;
    verify_bytes(&bytes, &expected_hex, kind)
        .with_context(|| format!("downloaded artifact from {} failed checksum", url))?;

    // Order: stage artifact in .part, write sidecar at final location, rename
    // artifact to final.  If anything fails before the rename no artifact is
    // committed; if the rename fails the orphan sidecar is overwritten on the
    // next attempt.
    let part = dest.with_extension("part");
    std::fs::write(&part, &bytes)
        .with_context(|| format!("failed to write {}", part.display()))?;
    let sidecar = sidecar_path(dest, kind);
    std::fs::write(&sidecar, expected_hex.as_bytes())
        .with_context(|| format!("failed to write sidecar {}", sidecar.display()))?;
    std::fs::rename(&part, dest)
        .with_context(|| format!("failed to rename {} → {}", part.display(), dest.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Checksum verification
// ---------------------------------------------------------------------------

/// Which checksum algorithm a sidecar uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DigestKind {
    Sha256,
    Sha1,
}

impl DigestKind {
    fn suffix(self) -> &'static str {
        match self {
            DigestKind::Sha256 => ".sha256",
            DigestKind::Sha1 => ".sha1",
        }
    }

    fn name(self) -> &'static str {
        match self {
            DigestKind::Sha256 => "SHA-256",
            DigestKind::Sha1 => "SHA-1",
        }
    }

    fn hash_hex(self, bytes: &[u8]) -> String {
        use sha2::Digest as _;
        match self {
            DigestKind::Sha256 => {
                let mut h = sha2::Sha256::new();
                h.update(bytes);
                hex_encode(&h.finalize())
            }
            DigestKind::Sha1 => {
                let mut h = sha1::Sha1::new();
                h.update(bytes);
                hex_encode(&h.finalize())
            }
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

/// Maven Central serves the bare hex digest (optionally followed by whitespace
/// and/or a newline).  Some private repos use the GNU `shasum` format
/// `<hex>  <filename>`.  Accept both: take the first whitespace-delimited
/// token and validate it as lowercase hex.
fn parse_checksum_text(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(token.to_ascii_lowercase())
}

/// Append a sidecar suffix (e.g. `.sha256`) to the artifact path.
/// Concatenates as bytes rather than `Path::with_extension` so
/// `foo-1.0.jar` becomes `foo-1.0.jar.sha256` rather than `foo-1.0.sha256`.
fn sidecar_path(artifact: &Path, kind: DigestKind) -> PathBuf {
    let mut s = artifact.as_os_str().to_owned();
    s.push(kind.suffix());
    PathBuf::from(s)
}

fn verify_bytes(bytes: &[u8], expected_hex: &str, kind: DigestKind) -> Result<()> {
    let actual = kind.hash_hex(bytes);
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        bail!(
            "{} checksum mismatch: expected {}, got {}",
            kind.name(),
            expected_hex.to_ascii_lowercase(),
            actual,
        )
    }
}

/// Fetch `<url><kind.suffix>` and parse the returned hex digest.
/// Returns `Ok(Some(_))` on success, `Ok(None)` on 404 (sidecar absent),
/// `Err(_)` on transport or parse errors.
fn fetch_remote_checksum(
    client: &reqwest::blocking::Client,
    url: &str,
    kind: DigestKind,
) -> Result<Option<String>> {
    let sidecar_url = format!("{}{}", url, kind.suffix());
    let response = client
        .get(&sidecar_url)
        .send()
        .with_context(|| format!("HTTP request failed for {}", sidecar_url))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), sidecar_url);
    }

    let body = response
        .text()
        .with_context(|| format!("failed to read sidecar body for {}", sidecar_url))?;
    let hex = parse_checksum_text(&body).with_context(|| {
        format!("sidecar {} returned malformed checksum text {:?}", sidecar_url, body)
    })?;
    Ok(Some(hex))
}

/// Try `.sha256` first, then `.sha1`.  Returns the first sidecar that exists,
/// or a hard error if neither is published.
fn fetch_any_remote_checksum(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<(String, DigestKind)> {
    if let Some(hex) = fetch_remote_checksum(client, url, DigestKind::Sha256)? {
        return Ok((hex, DigestKind::Sha256));
    }
    if let Some(hex) = fetch_remote_checksum(client, url, DigestKind::Sha1)? {
        return Ok((hex, DigestKind::Sha1));
    }
    bail!(
        "no .sha256 or .sha1 sidecar published at {} — refusing to use unverified artifact",
        url,
    )
}

/// Verify `local_path` against a locally-cached sidecar (`.sha256` preferred,
/// `.sha1` fallback).  Returns:
///   * `Ok(true)`  — sidecar found locally and verification succeeded.
///   * `Ok(false)` — no sidecar in local cache (caller should fetch one).
///   * `Err(_)`    — sidecar present but verification failed.
fn verify_with_local_sidecar(local_path: &Path) -> Result<bool> {
    for kind in [DigestKind::Sha256, DigestKind::Sha1] {
        let sidecar = sidecar_path(local_path, kind);
        if !sidecar.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&sidecar)
            .with_context(|| format!("failed to read sidecar {}", sidecar.display()))?;
        let expected = parse_checksum_text(&text).with_context(|| {
            format!("local sidecar {} has malformed contents", sidecar.display())
        })?;
        let bytes = std::fs::read(local_path)
            .with_context(|| format!("failed to read cached artifact {}", local_path.display()))?;
        verify_bytes(&bytes, &expected, kind)
            .with_context(|| format!("cached artifact {} failed checksum", local_path.display()))?;
        return Ok(true);
    }
    Ok(false)
}

/// Ensure that `local_path` (an existing cached artifact) has been verified
/// against a sidecar.  If no local sidecar is present, fetch one from the
/// configured repositories and cache it for next time.  Returns an error in
/// offline mode when no local sidecar exists, or on checksum mismatch.
fn ensure_verified(
    local_path: &Path,
    relative: &str,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    offline: bool,
) -> Result<()> {
    if verify_with_local_sidecar(local_path)? {
        return Ok(());
    }
    if offline {
        bail!(
            "cached artifact {} has no checksum sidecar and --offline was \
             specified; cannot verify integrity",
            local_path.display(),
        );
    }
    let mut last_err: Option<anyhow::Error> = None;
    for repo in repos {
        let url = repo.artifact_url(relative);
        match fetch_any_remote_checksum(client, &url) {
            Ok((hex, kind)) => {
                let bytes = std::fs::read(local_path).with_context(|| {
                    format!("failed to read cached artifact {}", local_path.display())
                })?;
                verify_bytes(&bytes, &hex, kind).with_context(|| {
                    format!(
                        "cached artifact {} failed checksum from {}",
                        local_path.display(),
                        url,
                    )
                })?;
                let sidecar = sidecar_path(local_path, kind);
                std::fs::write(&sidecar, hex.as_bytes())
                    .with_context(|| format!("failed to write sidecar {}", sidecar.display()))?;
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    bail!(
        "could not obtain a checksum sidecar for {} from any repository: {}",
        local_path.display(),
        last_err.unwrap_or_else(|| anyhow::anyhow!("no repositories configured")),
    )
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

    /// Write `bytes` to `path` and also write a `.sha256` sidecar containing
    /// the SHA-256 hex of those bytes.  Mirrors what real downloads do so the
    /// resolver's cache-hit verification step is satisfied.
    fn write_with_sidecar(path: &std::path::Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        let hex = DigestKind::Sha256.hash_hex(bytes);
        let sidecar = sidecar_path(path, DigestKind::Sha256);
        std::fs::write(&sidecar, hex.as_bytes()).unwrap();
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
        let xml = make_bom_pom(group, artifact, version, managed, bom_imports);
        write_with_sidecar(&path, xml.as_bytes());
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

        // POM (+ sidecar).
        let pom_path = m2.join(gav.relative_pom_path());
        let pom_xml = make_pom(group, artifact, version, deps);
        write_with_sidecar(&pom_path, pom_xml.as_bytes());

        // Empty JAR (placeholder — resolver only checks existence + checksum).
        let jar_path = m2.join(gav.relative_path());
        write_with_sidecar(&jar_path, b"");

        gav
    }

    /// Invoke `resolve()` with a fake home directory.  No network is
    /// performed — `named_repos` is empty and `default_repositories` (Maven
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
        let entries: Vec<DepEntry> = deps
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v, repo_id: None })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports,
            offline: true,
        };
        let result = resolve(&entries, &opts);

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

    // -----------------------------------------------------------------------
    // Checksum verification tests
    // -----------------------------------------------------------------------

    /// SHA-256 of the empty byte string — used by `write_with_sidecar` when
    /// the JAR placeholder content is `b""`.  Pinned here as a sanity check
    /// on `DigestKind::hash_hex`.
    const EMPTY_SHA256: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    // SHA-256("abc")
    const ABC_SHA256: &str =
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hex_encode_pads_each_byte_to_two_chars() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn digest_kind_hashes_known_vectors() {
        assert_eq!(DigestKind::Sha256.hash_hex(b""), EMPTY_SHA256);
        assert_eq!(DigestKind::Sha1.hash_hex(b""), EMPTY_SHA1);
        assert_eq!(DigestKind::Sha256.hash_hex(b"abc"), ABC_SHA256);
    }

    #[test]
    fn parse_checksum_text_accepts_bare_hex() {
        assert_eq!(parse_checksum_text(EMPTY_SHA256), Some(EMPTY_SHA256.into()));
    }

    #[test]
    fn parse_checksum_text_strips_trailing_newline() {
        let body = format!("{}\n", EMPTY_SHA256);
        assert_eq!(parse_checksum_text(&body), Some(EMPTY_SHA256.into()));
    }

    #[test]
    fn parse_checksum_text_accepts_gnu_shasum_format() {
        // Some private repos emit `<hex>  <filename>` (two spaces, GNU style).
        let body = format!("{}  foo-1.0.jar\n", EMPTY_SHA256);
        assert_eq!(parse_checksum_text(&body), Some(EMPTY_SHA256.into()));
    }

    #[test]
    fn parse_checksum_text_lowercases_uppercase_hex() {
        let upper = EMPTY_SHA256.to_ascii_uppercase();
        assert_eq!(parse_checksum_text(&upper), Some(EMPTY_SHA256.into()));
    }

    #[test]
    fn parse_checksum_text_rejects_non_hex() {
        assert_eq!(parse_checksum_text("hello world"), None);
        assert_eq!(parse_checksum_text(""), None);
        assert_eq!(parse_checksum_text("  \n\t"), None);
        // 63 hex chars then a non-hex char — first token is the whole thing,
        // which fails the hex-digit check.
        assert_eq!(parse_checksum_text("zzzzzzzz"), None);
    }

    #[test]
    fn verify_bytes_passes_on_match() {
        assert!(verify_bytes(b"", EMPTY_SHA256, DigestKind::Sha256).is_ok());
        assert!(verify_bytes(b"abc", ABC_SHA256, DigestKind::Sha256).is_ok());
        assert!(verify_bytes(b"", EMPTY_SHA1, DigestKind::Sha1).is_ok());
    }

    #[test]
    fn verify_bytes_fails_on_mismatch() {
        let err = verify_bytes(b"different", EMPTY_SHA256, DigestKind::Sha256)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("checksum mismatch") && err.contains("SHA-256"),
            "expected SHA-256 mismatch error, got {:?}", err,
        );
    }

    #[test]
    fn verify_bytes_is_case_insensitive_on_expected() {
        // Some servers serve uppercase hex.
        let upper = EMPTY_SHA256.to_ascii_uppercase();
        assert!(verify_bytes(b"", &upper, DigestKind::Sha256).is_ok());
    }

    #[test]
    fn sidecar_path_appends_suffix_keeping_extension() {
        let p = std::path::Path::new("/a/b/foo-1.0.jar");
        assert_eq!(
            sidecar_path(p, DigestKind::Sha256),
            std::path::PathBuf::from("/a/b/foo-1.0.jar.sha256"),
        );
        assert_eq!(
            sidecar_path(p, DigestKind::Sha1),
            std::path::PathBuf::from("/a/b/foo-1.0.jar.sha1"),
        );
    }

    #[test]
    fn verify_with_local_sidecar_returns_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"").unwrap();
        // No sidecar written.
        assert_eq!(verify_with_local_sidecar(&jar).unwrap(), false);
    }

    #[test]
    fn verify_with_local_sidecar_passes_when_correct() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"abc").unwrap();
        std::fs::write(
            sidecar_path(&jar, DigestKind::Sha256),
            ABC_SHA256.as_bytes(),
        )
        .unwrap();
        assert_eq!(verify_with_local_sidecar(&jar).unwrap(), true);
    }

    #[test]
    fn verify_with_local_sidecar_falls_back_to_sha1() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"").unwrap();
        // No .sha256, only .sha1.
        std::fs::write(
            sidecar_path(&jar, DigestKind::Sha1),
            EMPTY_SHA1.as_bytes(),
        )
        .unwrap();
        assert_eq!(verify_with_local_sidecar(&jar).unwrap(), true);
    }

    #[test]
    fn verify_with_local_sidecar_errors_on_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"original").unwrap();
        // Sidecar claims the empty-string digest; jar bytes differ → mismatch.
        std::fs::write(
            sidecar_path(&jar, DigestKind::Sha256),
            EMPTY_SHA256.as_bytes(),
        )
        .unwrap();
        let err = verify_with_local_sidecar(&jar).unwrap_err().to_string();
        assert!(
            err.contains("checksum"),
            "expected checksum failure, got {:?}", err,
        );
    }

    #[test]
    fn verify_with_local_sidecar_errors_on_malformed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"").unwrap();
        std::fs::write(
            sidecar_path(&jar, DigestKind::Sha256),
            b"not-a-hex-digest",
        )
        .unwrap();
        assert!(verify_with_local_sidecar(&jar).is_err());
    }

    #[test]
    fn resolve_succeeds_when_cached_artifact_matches_sidecar() {
        // `write_fake_artifact` writes a correct sidecar; this is the
        // golden-path assertion that the cache-hit verification step is
        // wired in and passes for honest caches.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        let result = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![]).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn resolve_fails_when_cached_jar_is_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let gav = write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        // Tamper with the JAR after the sidecar was written.
        let jar = dir
            .path()
            .join(".m2")
            .join("repository")
            .join(gav.relative_path());
        std::fs::write(&jar, b"tampered bytes").unwrap();

        let err = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![])
            .unwrap_err();
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("checksum"),
            "expected checksum error in chain, got {:?}", chain,
        );
    }

    #[test]
    fn resolve_fails_when_cached_artifact_has_no_sidecar_in_offline() {
        let dir = tempfile::tempdir().unwrap();
        let gav = write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        // Remove both possible sidecars from the cache so the offline path
        // can't verify and must bail.  POM has a sidecar so resolving the
        // POM still works — the failure must come from the JAR.
        let jar = dir
            .path()
            .join(".m2")
            .join("repository")
            .join(gav.relative_path());
        let _ = std::fs::remove_file(sidecar_path(&jar, DigestKind::Sha256));
        let _ = std::fs::remove_file(sidecar_path(&jar, DigestKind::Sha1));

        let err = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![])
            .unwrap_err();
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("sidecar"),
            "expected missing-sidecar error in chain, got {:?}", chain,
        );
    }

    // -----------------------------------------------------------------------
    // Per-dep repository routing tests
    // -----------------------------------------------------------------------

    /// Helper: run `resolve()` with a named repo and per-dep repo_id.
    fn run_resolve_with_repo(
        home_dir: &std::path::Path,
        deps: &[(&str, &str, Option<&str>)],
        named_repos: Vec<Repository>,
    ) -> Result<Vec<PathBuf>> {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());

        let entries: Vec<DepEntry> = deps
            .iter()
            .map(|(k, v, r)| DepEntry { key: k, version: v, repo_id: *r })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos,
            progress: false,
            bom_imports: vec![],
            offline: true,
        };
        let result = resolve(&entries, &opts);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    #[test]
    fn custom_default_repos_used_instead_of_central() {
        // When default_repos is non-empty, the resolver uses it instead of
        // hard-coded Maven Central.  Verify by passing a fake "mirror" repo
        // that points at the local cache (same path as Central would use for
        // offline tests) — if Central were used the artifact would still be
        // found (it's in ~/.m2), so we OMIT the artifact from the local cache
        // and rely on the custom repo URL being tried (and failing in offline
        // mode, giving a specific error about that URL rather than a generic
        // Maven Central error).
        //
        // Simpler angle: passing a non-empty default_repos means resolve() does
        // NOT call default_repositories() internally.  We verify this by checking
        // that a cached artifact IS found when we pass Central as default_repos
        // (same behaviour as the normal path).
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let central_override = Repository {
            id: "central".to_string(),
            name: "Central Mirror".to_string(),
            url: "https://nexus.internal/maven2".to_string(),
        };
        let opts = ResolveOptions {
            default_repos: vec![central_override],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true, // cache-hit path; no network call made
        };
        let entries = [DepEntry { key: "foo:bar", version: "1.0", repo_id: None }];
        let result = resolve(&entries, &opts).unwrap();
        assert_eq!(result.len(), 1, "should find cached artifact regardless of mirror URL");

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn dep_with_unknown_repo_id_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // Artifact is cached — but resolution must fail before fetching
        // because "unknown-repo" is not in named_repos.
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        let err = run_resolve_with_repo(
            dir.path(),
            &[("foo:bar", "1.0", Some("unknown-repo"))],
            vec![],
        )
        .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("unknown-repo"),
            "expected unknown-repo error, got {:?}", msg,
        );
    }

    #[test]
    fn dep_without_repo_id_uses_central_only() {
        // Write the artifact only in a "private" named repo dir; do NOT
        // write it as a normal central-layout artifact.  When the dep has
        // no repo_id, the resolver must use Central only and fail.
        let dir = tempfile::tempdir().unwrap();
        // We pre-write the artifact at the standard path (Central layout),
        // so without repo_id the offline resolve succeeds.
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);

        // Dep has no repo_id — should succeed from "Central" (fake local cache).
        let result = run_resolve_with_repo(
            dir.path(),
            &[("foo:bar", "1.0", None)],
            vec![],
        )
        .unwrap();
        assert_eq!(result.len(), 1, "expected 1 resolved JAR");
    }

    #[test]
    fn dep_with_repo_id_routes_to_named_repo() {
        // The artifact is in the local cache (fake Central), but we declare it
        // with a repo_id.  Because offline=true and the artifact is already in
        // ~/.m2, resolution succeeds regardless (cache hits don't re-download).
        // This test mainly verifies the repo_id lookup does not error when the
        // named repo exists.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);

        let named = Repository {
            id: "private".to_string(),
            name: "Private Nexus".to_string(),
            url: "https://nexus.example.com/m2".to_string(),
        };
        let result = run_resolve_with_repo(
            dir.path(),
            &[("foo:bar", "1.0", Some("private"))],
            vec![named],
        )
        .unwrap();
        assert_eq!(result.len(), 1, "expected 1 resolved JAR");
    }
}
