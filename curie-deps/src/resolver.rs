//! Dependency resolver: cache lookup → download → transitive expansion.
//!
//! # Algorithm
//! 1. For each declared `Gav`, check `~/.m2/repository` for the JAR and POM.
//! 2. On cache miss, try each configured repository in order; download using a
//!    unique collocated staging file and atomic rename (with tolerant handling
//!    when another writer wins). An in-process gate prevents concurrent writers
//!    for the same final artifact inside one process.
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
use crate::snapshot_meta;
use anyhow::{bail, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// In-process coordination for concurrent artifact downloads (bug #2 fix).
// Keys are the final local repository path strings (unique per GAV+kind).
// This is the analogue of Maven Resolver's named SyncContext acquire step.
// ---------------------------------------------------------------------------

/// Monotonic id for unique collocated staging files within this process.
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Map from final destination path -> Condvar used to wake waiters.
/// Protected by OnceLock so we have a true process-global without lazy_static.
static INFLIGHT: OnceLock<Mutex<HashMap<String, Arc<Condvar>>>> = OnceLock::new();

fn get_inflight() -> &'static Mutex<HashMap<String, Arc<Condvar>>> {
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns true if this caller became the "responsible" downloader for `key`.
/// Returns false if another thread is already downloading it (caller should wait).
fn claim_or_wait_for_download(key: &str) -> bool {
    let mut map = get_inflight().lock().unwrap();
    if map.contains_key(key) {
        // Another thread is responsible; the caller will wait.
        false
    } else {
        map.insert(key.to_string(), Arc::new(Condvar::new()));
        true
    }
}

/// Called by the responsible thread after it has finished (success or failure).
/// Wakes any threads waiting for this key.
fn release_download_slot(key: &str) {
    let mut map = get_inflight().lock().unwrap();
    if let Some(cvar) = map.remove(key) {
        cvar.notify_all();
    }
}

/// Wait (by polling existence + short sleeps) until the artifact at `dest`
/// appears or a generous timeout elapses. Used by threads that lost the
/// in-flight claim. Polling is simple and sufficient for download timescales.
fn wait_for_artifact_to_appear(dest: &Path) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(300); // very generous; real downloads are much faster
    while !dest.exists() {
        if start.elapsed() > timeout {
            // Give up waiting; the caller will hit the normal exists check
            // (or a later error) and behave correctly.
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

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
    /// When `true`, transitive dependencies whose declared version is a Maven
    /// version range (e.g. `[4.9,)`) are silently skipped instead of causing
    /// a hard error.  Intended for `curie fetch --file` where such deps should
    /// be listed explicitly in the coordinate file.
    pub skip_version_ranges: bool,
    /// When `true`, a major-version conflict (a discarded candidate whose major
    /// differs from the kept version) fails resolution unless the coordinate
    /// opted out via `allowVersionConflict`.  Enable this only for the user's
    /// declared `[dependencies]` / `[test-dependencies]`; internal tool
    /// classpaths (formatters, kotlinc, etc.) resolve curated dep sets the user
    /// cannot annotate, so they leave it `false`.
    pub error_on_version_conflict: bool,
    /// Snapshot pins from `Curie.lock`: keys are
    /// `group:artifact:baseVersion` (e.g. `com.ex:foo:1.0-SNAPSHOT`), values
    /// are the unique timestamped filename version
    /// (`1.0-20260610.123456-3`). When present and
    /// [`Self::update_snapshots`] is `false`, the pin is used and metadata is
    /// not re-fetched. Without a pin (or with `-U`), metadata is always
    /// re-fetched from the repository.
    pub snapshot_pins: HashMap<String, String>,
    /// Force re-resolution of SNAPSHOT metadata (`curie build -U`), ignoring
    /// existing pins.
    pub update_snapshots: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: true,
            bom_imports: vec![],
            offline: false,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: HashMap::new(),
            update_snapshots: false,
        }
    }
}

/// Full result of dependency resolution, including SNAPSHOT pins for
/// `Curie.lock`.
#[derive(Debug)]
pub struct ResolveResult {
    /// Resolved JAR paths in BFS order (classpath order).
    pub jars: Vec<PathBuf>,
    /// Snapshot pins discovered during this resolve:
    /// `group:artifact:baseVersion` → unique timestamped version.
    /// Empty when no SNAPSHOT dependencies were resolved to a unique form.
    pub snapshot_pins: BTreeMap<String, String>,
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
    /// Exclusions declared on this dependency in `Curie.toml`.
    /// Each entry is a `"group:artifact"` string.  These are propagated
    /// transitively: any transitive dependency matching an exclusion is
    /// omitted from the resolved closure.
    pub exclusions: Vec<&'a str>,
    /// Optional classifier (e.g. Some("runtime") for JaCoCo agent, Some("sources")).
    /// When set, the resolved JAR filename will include `-classifier`.
    /// Most user dependencies do not need this.
    pub classifier: Option<&'a str>,
    /// When `true`, the user has accepted a transitive major-version mismatch for
    /// this coordinate (`allowVersionConflict = true` in `Curie.toml`), so
    /// [`resolve`] will not fail the build on a major-version conflict for it.
    pub allow_version_conflict: bool,
}

/// Internal BFS work item carrying per-artifact repository context.
///
/// `depth` and `via` are used by [`resolve_tree`]; [`resolve`] leaves them
/// at their defaults (0 / None).
struct BfsWork {
    gav: Gav,
    /// Repos to use when fetching THIS artifact's POM and JAR.
    fetch_repos: Vec<Repository>,
    /// Repos passed to each of this artifact's transitive dependencies
    /// (used as their `fetch_repos` and `child_repos`).
    child_repos: Vec<Repository>,
    /// BFS depth: 0 = declared by user, 1 = direct transitive, etc.
    depth: usize,
    /// The artifact that introduced this one.  `None` for depth-0 deps.
    via: Option<Gav>,
    /// Accumulated exclusions inherited from all ancestors along the BFS
    /// path.  Each entry is `("groupId", "artifactId")`.  Wildcard `"*"`
    /// in either position matches any group/artifact.
    exclusions: HashSet<(String, String)>,
}

// ---------------------------------------------------------------------------
// Dependency-tree types (used by resolve_tree)
// ---------------------------------------------------------------------------

/// One artifact in the resolved transitive closure.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub gav: Gav,
    /// BFS depth: 0 = declared by user, 1 = direct transitive, etc.
    pub depth: usize,
    /// The artifact that pulled this one in.  `None` for depth-0 (declared) deps.
    pub via: Option<Gav>,
}

/// A version candidate that lost the nearest-wins conflict: a deeper path
/// tried to introduce the same `group:artifact` but was already visited.
#[derive(Debug, Clone)]
pub struct SkippedDep {
    /// The version that was discarded.
    pub version: String,
    pub depth: usize,
    /// The artifact that tried to introduce this version.
    pub via: Option<Gav>,
}

/// Full dependency tree returned by [`resolve_tree`].
#[derive(Debug)]
pub struct DepTree {
    /// All resolved deps in BFS discovery order.
    pub resolved: Vec<ResolvedDep>,
    /// Nearest-wins losers keyed by `"group:artifact"`.
    pub skipped: HashMap<String, Vec<SkippedDep>>,
}

/// Returns `true` when `(group, artifact)` is matched by any entry in
/// `exclusions`.  An exclusion entry with `"*"` in either position acts
/// as a wildcard (e.g. `("org.example", "*")` excludes all artifacts in
/// that group).
fn is_excluded(group: &str, artifact: &str, exclusions: &HashSet<(String, String)>) -> bool {
    if exclusions.is_empty() {
        return false;
    }
    for (eg, ea) in exclusions {
        let group_match = eg == "*" || eg == group;
        let artifact_match = ea == "*" || ea == artifact;
        if group_match && artifact_match {
            return true;
        }
    }
    false
}

/// Parse `"group:artifact"` exclusion strings from Curie.toml into
/// `(group, artifact)` pairs suitable for the `exclusions` set.
fn parse_exclusion_strings(strings: &[&str]) -> HashSet<(String, String)> {
    let mut set = HashSet::new();
    for s in strings {
        if let Some((g, a)) = s.split_once(':') {
            set.insert((g.trim().to_string(), a.trim().to_string()));
        }
    }
    set
}

/// Merge POM-level exclusions (from `<exclusions>` in the dep declaration)
/// into a parent's accumulated exclusion set and return the combined set
/// for the child.
fn merge_exclusions(
    parent_exclusions: &HashSet<(String, String)>,
    dep_exclusions: &[(String, String)],
) -> HashSet<(String, String)> {
    let mut merged = parent_exclusions.clone();
    for (g, a) in dep_exclusions {
        merged.insert((g.clone(), a.clone()));
    }
    merged
}

/// One dependency that declared a non-deterministic Maven version range
/// (e.g. `[2.9.1,2.11)`) instead of a fixed version.
#[derive(Debug, Clone)]
pub struct RangeViolation {
    /// `"group:artifact"` of the ranged dependency.
    pub dep_key: String,
    /// The range expression as written, e.g. `[2.9.1,2.11)`.
    pub range: String,
    /// The artifact whose POM declared the range (or `Curie.toml` for a direct
    /// declaration).
    pub declared_in: Gav,
}

/// Error returned by [`resolve`]/[`resolve_with_pins`] when the dependency graph
/// contains one or more version ranges.  Its `Display` is the canonical
/// pin-in-`Curie.toml` guidance, so `curie build` behaviour is unchanged; the
/// structured `violations` let `curie fetch` propose a tailored fix instead.
#[derive(Debug)]
pub struct VersionRangeError {
    pub violations: Vec<RangeViolation>,
}

impl std::fmt::Display for VersionRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_range_error(&self.violations))
    }
}

impl std::error::Error for VersionRangeError {}

/// A discarded version candidate whose MAJOR component differs from the version
/// the build kept for the same `group:artifact`.  Surfaced as a hard error
/// unless the coordinate opts out via `allowVersionConflict = true`.
struct VersionConflict {
    /// `"group:artifact"`.
    key: String,
    /// The version the build keeps (nearest-wins / first-declared winner).
    chosen: String,
    /// The discarded candidate version with a different major.
    needed: String,
    /// The artifact that required `needed` (`None` = a second declaration of the
    /// same coordinate in `Curie.toml`).
    via: Option<Gav>,
}

fn is_version_range(version: &str) -> bool {
    version.starts_with('[') || version.starts_with('(')
}

/// Leading numeric component of a Maven version (the "major"), if parseable.
/// `"2.17.2" -> 2`, `"5" -> 5`, `"1-beta" -> 1`, `"RELEASE" -> None`.
fn major_component(version: &str) -> Option<u64> {
    version.split(['.', '-']).next()?.parse::<u64>().ok()
}

/// True only when both versions have a parseable major and the majors differ.
/// Unparseable majors (e.g. `"RELEASE"`) never count as a conflict.
fn differs_by_major(a: &str, b: &str) -> bool {
    matches!((major_component(a), major_component(b)), (Some(x), Some(y)) if x != y)
}

fn format_conflict_error(conflicts: &[VersionConflict]) -> String {
    let mut msg = String::from("dependency version conflict (major-version mismatch)");

    for c in conflicts {
        let requirer = match &c.via {
            Some(g) => g.notation(),
            None => "a second declaration in Curie.toml".to_string(),
        };
        msg.push_str(&format!(
            "\n\n  {} — keeping {}, but {} requires {}",
            c.key, c.chosen, requirer, c.needed
        ));
    }

    msg.push_str(
        "\n\nA major-version difference can cause runtime errors (missing classes/methods).\n\
         Fix the version, exclude the transitive dependency, or — if intentional —\n\
         allow it in Curie.toml:",
    );
    for c in conflicts {
        msg.push_str(&format!(
            "\n  \"{}\" = {{ version = \"{}\", allowVersionConflict = true }}",
            c.key, c.chosen
        ));
    }

    msg
}

fn curie_toml_gav() -> Gav {
    Gav {
        group: String::new(),
        artifact: "Curie.toml".to_string(),
        version: String::new(),
        classifier: None,
        extension: None,
        snapshot_version: None,
    }
}

fn format_range_error(violations: &[RangeViolation]) -> String {
    use std::collections::BTreeMap;

    let mut grouped: BTreeMap<&str, Vec<(&str, &Gav)>> = BTreeMap::new();
    for v in violations {
        grouped
            .entry(&v.dep_key)
            .or_default()
            .push((&v.range, &v.declared_in));
    }

    let mut msg = String::from("non-deterministic version ranges in dependency graph");

    for (dep_key, entries) in &grouped {
        msg.push_str(&format!("\n\n  {dep_key}"));
        for (range, declared_in) in entries {
            let location = if declared_in.artifact == "Curie.toml" {
                "Curie.toml".to_string()
            } else {
                declared_in.notation()
            };
            msg.push_str(&format!("\n    \"{range}\"  declared in {location}"));
        }
    }

    msg.push_str("\n\nPin these artifacts in Curie.toml to fix:\n  [dependencies]");
    for dep_key in grouped.keys() {
        msg.push_str(&format!("\n  \"{dep_key}\" = \"<version>\""));
    }

    msg
}

/// Walk the parent POM chain (up to 10 levels) and merge properties +
/// managed_versions into `pom`. Parent values only fill gaps — own values win.
///
/// A missing `<parent>` is the normal terminating case (returns `Ok`).  A parent
/// that is declared but cannot be fetched, read, or parsed is a hard error:
/// silently continuing would leave `pom` with missing properties / managed
/// versions and cause its transitive dependencies to be dropped, producing an
/// incomplete classpath (bug #15).  `child` is the coordinate whose chain we are
/// walking, used only for error context.
fn merge_parent_chain(
    pom: &mut Pom,
    child: &Gav,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    opts: &ResolveOptions,
) -> Result<()> {
    let mut depth = 0;
    let mut current_parent = pom.parent.clone();

    while let Some(parent_ref) = current_parent {
        depth += 1;
        if depth > 10 {
            // Cycle / pathological depth guard — not a fetch failure.
            break;
        }

        let mut parent_gav = Gav {
            group: parent_ref.group_id.clone(),
            artifact: parent_ref.artifact_id.clone(),
            version: parent_ref.version.clone(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        prepare_snapshot_gav(&mut parent_gav, repos, client, opts)?;

        let pom_path = ensure_artifact(
            &parent_gav,
            repos,
            client,
            ArtifactKind::Pom,
            opts.offline,
            None,
            None,
        )
        .with_context(|| format!("failed to fetch parent POM {parent_gav} (parent of {child})"))?;
        let xml = std::fs::read_to_string(&pom_path).with_context(|| {
            format!(
                "failed to read parent POM {} (parent of {child})",
                pom_path.display()
            )
        })?;
        let parent_pom = pom::parse(&xml).with_context(|| {
            format!("failed to parse parent POM {parent_gav} (parent of {child})")
        })?;

        // Properties: parent fills gaps.
        for (k, v) in &parent_pom.properties {
            pom.properties.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // Managed versions: parent fills gaps.
        for (k, v) in &parent_pom.managed_versions {
            pom.managed_versions
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        // Managed scopes: parent fills gaps (same Maven rule as versions).
        for (k, v) in &parent_pom.managed_scopes {
            pom.managed_scopes
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        // BOM imports from parent are appended (parent has lower priority than own).
        for bom_ref in &parent_pom.bom_imports {
            pom.bom_imports.push(bom_ref.clone());
        }
        // Dependencies: parent fills gaps. A child that redeclares the same
        // group:artifact keeps its own version, scope, and exclusions.
        // Maven's effective POM includes inherited parent <dependencies>
        // (test-parameter-injector publishes snakeyaml on its parent POM).
        for dep in &parent_pom.dependencies {
            let already = pom.dependencies.iter().any(|d| {
                d.group_id == dep.group_id
                    && d.artifact_id == dep.artifact_id
                    && d.classifier == dep.classifier
            });
            if !already {
                pom.dependencies.push(dep.clone());
            }
        }

        current_parent = parent_pom.parent.clone();
    }
    apply_dependency_management(pom);
    Ok(())
}

/// Fill missing version/scope on each dependency from this POM's
/// `<dependencyManagement>` (already merged with the parent chain).
/// Maven's effective POM does this before deciding whether a dependency
/// is transitive: a managed `provided` scope must not leak to consumers.
fn apply_dependency_management(pom: &mut Pom) {
    for dep in &mut pom.dependencies {
        let key = format!("{}:{}", dep.group_id, dep.artifact_id);
        if dep.version.is_none() {
            if let Some(v) = pom.managed_versions.get(&key) {
                dep.version = Some(v.clone());
            }
        }
        if dep.scope.is_none() {
            if let Some(s) = pom.managed_scopes.get(&key) {
                dep.scope = Some(s.clone());
            }
        }
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
    opts: &ResolveOptions,
) -> Result<Pom> {
    let mut resolved = gav.clone();
    prepare_snapshot_gav(&mut resolved, repos, client, opts)?;
    let pom_path = ensure_artifact(
        &resolved,
        repos,
        client,
        ArtifactKind::Pom,
        opts.offline,
        None,
        None,
    )
    .with_context(|| format!("failed to fetch POM for {}", gav))?;
    let xml = std::fs::read_to_string(&pom_path)
        .with_context(|| format!("failed to read POM {}", pom_path.display()))?;
    let mut pom = pom::parse(&xml).with_context(|| format!("failed to parse POM for {}", gav))?;
    merge_parent_chain(&mut pom, gav, repos, client, opts)?;

    // Resolve the POM's own <dependencyManagement> BOM imports so that
    // dependencies declared without an explicit version (e.g. spock-core's
    // `junit-platform-engine` whose version is managed by the embedded
    // `junit-bom`) can be resolved via `managed_versions`.  The merge
    // uses "existing wins" so the POM's own explicit entries are not
    // overwritten by the BOM imports.
    //
    // We intentionally use a SHALLOW one-level expansion here — fetch each
    // BOM POM and extract its managed_versions directly, without calling
    // fetch_and_parse_pom recursively (which would cause mutual recursion
    // with resolve_boms and overflow on BOM cycles).
    if !pom.bom_imports.is_empty() {
        for bom_ref in &pom.bom_imports {
            // Resolve groupId, artifactId AND version against the importing
            // POM's properties before building the coordinate.  Skipping when
            // any placeholder remains keeps an unresolved `${...}` from ever
            // reaching the filesystem (which previously created junk cache
            // directories literally named e.g. `${idp.groupId}`).
            let Some(mut bom_gav) = resolve_bom_ref_gav(bom_ref, &pom) else {
                continue;
            };
            let _ = prepare_snapshot_gav(&mut bom_gav, repos, client, opts);
            // Fetch the BOM POM directly without further BOM-import expansion.
            if let Ok(path) = ensure_artifact(
                &bom_gav,
                repos,
                client,
                ArtifactKind::Pom,
                opts.offline,
                None,
                None,
            ) {
                if let Ok(xml) = std::fs::read_to_string(&path) {
                    if let Ok(bom_pom) = pom::parse(&xml) {
                        for (k, v) in &bom_pom.managed_versions {
                            pom.managed_versions
                                .entry(resolve_ga_key(&bom_pom, k))
                                .or_insert_with(|| bom_pom.resolve_value(v));
                        }
                    }
                }
            }
        }
    }

    Ok(pom)
}

/// Build a reusable blocking HTTP client with curie's standard settings.
fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("curie-build/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

/// Return the effective list of default repositories: caller-supplied when
/// non-empty, otherwise Maven Central.
fn effective_repos(default_repos: &[Repository]) -> Vec<Repository> {
    if default_repos.is_empty() {
        default_repositories()
    } else {
        default_repos.to_vec()
    }
}

/// Fetch one BFS level's POMs in parallel (up to `PARALLEL_POM_FETCHES`
/// threads).  Returns results indexed by the same order as `level`.
fn parallel_pom_fetch(
    level: &[BfsWork],
    client: &Client,
    opts: &ResolveOptions,
) -> Vec<Option<Result<Pom>>> {
    const PARALLEL_POM_FETCHES: usize = 8;
    let level_n = level.len();
    let thread_count = PARALLEL_POM_FETCHES.min(level_n);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut pom_results: Vec<Option<Result<Pom>>> = (0..level_n).map(|_| None).collect();
    let mut per_thread: Vec<Vec<(usize, Result<Pom>)>> = Vec::new();

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                s.spawn(|| -> Vec<(usize, Result<Pom>)> {
                    let mut local = Vec::new();
                    loop {
                        let i = next_idx.fetch_add(1, Ordering::Relaxed);
                        if i >= level_n {
                            break;
                        }
                        let work = &level[i];
                        local.push((
                            i,
                            fetch_and_parse_pom(&work.gav, &work.fetch_repos, client, opts),
                        ));
                    }
                    local
                })
            })
            .collect();
        for h in handles {
            per_thread.push(h.join().unwrap_or_default());
        }
    });

    for thread_results in per_thread {
        for (i, result) in thread_results {
            pom_results[i] = Some(result);
        }
    }
    pom_results
}

pub fn resolve_boms(
    bom_gavs: &[Gav],
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    offline: bool,
) -> Result<HashMap<String, String>> {
    let opts = ResolveOptions {
        offline,
        ..ResolveOptions::default()
    };
    resolve_boms_with_opts(bom_gavs, repos, client, &opts)
}

fn resolve_boms_with_opts(
    bom_gavs: &[Gav],
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    opts: &ResolveOptions,
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

                let pom = fetch_and_parse_pom(&gav, repos, client, opts)
                    .with_context(|| format!("failed to fetch BOM POM for {}", gav))?;

                // Collect this BOM's own managed versions for deferred application.
                let own_entries: HashMap<String, String> = pom
                    .managed_versions
                    .iter()
                    .map(|(k, v)| (resolve_ga_key(&pom, k), pom.resolve_value(v)))
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
                    if let Some(nested_gav) = resolve_bom_ref_gav(bom_ref, &pom) {
                        queue.push_front(BomWork::Fetch(nested_gav));
                    }
                }
            }
        }
    }

    Ok(managed)
}

/// Resolve a BOM reference's full coordinate (groupId, artifactId, version)
/// against the importing POM's properties.
///
/// Returns `None` if any component still contains an unresolved `${...}`
/// placeholder — this guards against feeding a placeholder coordinate to
/// [`ensure_artifact`], which would otherwise create a junk cache directory
/// literally named e.g. `${idp.groupId}`.
fn resolve_bom_ref_gav(bom_ref: &BomRef, importing_pom: &Pom) -> Option<Gav> {
    let group = importing_pom.try_resolve_value(&bom_ref.group_id)?;
    let artifact = importing_pom.try_resolve_value(&bom_ref.artifact_id)?;
    let version = resolve_bom_ref_version(bom_ref, importing_pom)?;
    Some(Gav {
        group,
        artifact,
        version,
        classifier: None,
        extension: None,
        snapshot_version: None,
    })
}

/// Resolve the version of a nested BOM reference, using the importing POM's
/// properties and managed versions for `${...}` substitution.
fn resolve_bom_ref_version(bom_ref: &BomRef, importing_pom: &Pom) -> Option<String> {
    importing_pom
        .try_resolve_value(&bom_ref.version)
        .or_else(|| {
            // Try managed_versions as a last resort.
            let key = format!("{}:{}", bom_ref.group_id, bom_ref.artifact_id);
            importing_pom
                .managed_versions
                .get(&key)
                .and_then(|v| importing_pom.try_resolve_value(v))
        })
}

/// Resolve `${...}` placeholders in a `groupId:artifactId` managed-versions key
/// against `pom`'s properties.  BOMs frequently express managed coordinates with
/// `${project.groupId}` (e.g. Shibboleth's `idp-bom`); leaving the key literal
/// means later `group:artifact` lookups silently miss.  Keys without a single
/// `:` separator are returned unchanged.
fn resolve_ga_key(pom: &Pom, key: &str) -> String {
    match key.split_once(':') {
        Some((group, artifact)) => {
            format!(
                "{}:{}",
                pom.resolve_value(group),
                pom.resolve_value(artifact)
            )
        }
        None => key.to_string(),
    }
}

/// Resolve the full transitive dependency tree and return rich metadata
/// about depth, introduction paths, and nearest-wins conflict decisions.
///
/// Runs Phase 1 of the resolver (BFS over POMs) but skips Phase 2 (JAR
/// downloads) — suitable for `curie deps` queries that don't need the JARs.
pub fn resolve_tree(deps: &[DepEntry], opts: &ResolveOptions) -> Result<DepTree> {
    let central = effective_repos(&opts.default_repos);

    let named_map: std::collections::HashMap<&str, &Repository> = opts
        .named_repos
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();

    let client = build_http_client()?;

    let global_managed = resolve_boms_with_opts(&opts.bom_imports, &central, &client, opts)?;

    let mut visited: HashSet<String> = HashSet::new();
    let mut resolved: Vec<ResolvedDep> = Vec::new();
    let mut skipped: HashMap<String, Vec<SkippedDep>> = HashMap::new();
    let mut range_violations: Vec<RangeViolation> = Vec::new();

    // Seed depth-0 from declared deps.
    let mut current_level: Vec<BfsWork> = Vec::new();
    for dep in deps {
        let resolved_version: String = if dep.version.is_empty() {
            global_managed
                .get(dep.key)
                .with_context(|| {
                    format!(
                        "dependency \"{}\" has no version and is not managed by any BOM",
                        dep.key
                    )
                })?
                .clone()
        } else {
            dep.version.to_string()
        };

        if is_version_range(&resolved_version) {
            range_violations.push(RangeViolation {
                dep_key: dep.key.to_string(),
                range: resolved_version,
                declared_in: curie_toml_gav(),
            });
            continue;
        }

        let (fetch_repos, child_repos) = if let Some(repo_id) = dep.repo_id {
            let named: Repository = (*named_map.get(repo_id).with_context(|| {
                format!(
                    "dependency \"{}\" references unknown repository \"{}\"",
                    dep.key, repo_id
                )
            })?)
            .clone();
            let mut child = central.clone();
            child.push(named.clone());
            (vec![named], child)
        } else {
            (central.clone(), central.clone())
        };

        let gav = Gav::from_key_version_classifier(dep.key, &resolved_version, dep.classifier)?;
        let ga = format!("{}:{}", gav.group, gav.artifact);
        let user_exclusions = parse_exclusion_strings(&dep.exclusions);
        if visited.insert(ga) {
            current_level.push(BfsWork {
                gav,
                fetch_repos,
                child_repos,
                depth: 0,
                via: None,
                exclusions: user_exclusions,
            });
        }
    }

    while !current_level.is_empty() {
        let pom_results = parallel_pom_fetch(&current_level, &client, opts);

        // A POM that failed to fetch/read/parse (including any parent in its
        // chain) is fatal: silently skipping it would drop its transitive
        // subtree and yield an incomplete classpath (bug #15).
        for (i, r) in pom_results.iter().enumerate() {
            if let Some(Err(e)) = r {
                bail!("failed to resolve {}: {:#}", current_level[i].gav, e);
            }
        }

        let mut next_level: Vec<BfsWork> = Vec::new();
        for (i, work) in current_level.iter().enumerate() {
            resolved.push(ResolvedDep {
                gav: work.gav.clone(),
                depth: work.depth,
                via: work.via.clone(),
            });

            if let Some(Ok(pom)) = &pom_results[i] {
                for dep in pom.dependencies.iter().filter(|d| d.is_compile_scope()) {
                    let group = pom.resolve_value(&dep.group_id);
                    let artifact = pom.resolve_value(&dep.artifact_id);
                    let ga_key = format!("{}:{}", group, artifact);
                    let child_depth = work.depth + 1;

                    // Check against accumulated exclusions from all ancestors.
                    if is_excluded(&group, &artifact, &work.exclusions) {
                        continue;
                    }

                    if visited.contains(&ga_key) {
                        if let Some(raw_version) = resolve_transitive_version(
                            &ga_key,
                            dep.version.as_deref(),
                            pom,
                            &global_managed,
                        ) {
                            skipped.entry(ga_key).or_default().push(SkippedDep {
                                version: raw_version,
                                depth: child_depth,
                                via: Some(work.gav.clone()),
                            });
                        }
                        continue;
                    }

                    let raw_version = match resolve_transitive_version(
                        &ga_key,
                        dep.version.as_deref(),
                        pom,
                        &global_managed,
                    ) {
                        Some(v) => v,
                        None => continue,
                    };

                    if is_version_range(&raw_version) {
                        range_violations.push(RangeViolation {
                            dep_key: ga_key,
                            range: raw_version,
                            declared_in: work.gav.clone(),
                        });
                        continue;
                    }

                    let child_exclusions = merge_exclusions(&work.exclusions, &dep.exclusions);
                    let child_gav = Gav {
                        group,
                        artifact,
                        version: raw_version,
                        classifier: dep.classifier.clone(),
                        extension: None,
                        snapshot_version: None,
                    };
                    visited.insert(ga_key);
                    next_level.push(BfsWork {
                        fetch_repos: work.child_repos.clone(),
                        child_repos: work.child_repos.clone(),
                        depth: child_depth,
                        via: Some(work.gav.clone()),
                        gav: child_gav,
                        exclusions: child_exclusions,
                    });
                }
            }
        }

        current_level = next_level;
    }

    if !range_violations.is_empty() {
        return Err(anyhow::Error::new(VersionRangeError {
            violations: range_violations,
        }));
    }

    Ok(DepTree { resolved, skipped })
}

/// Resolve a list of [`DepEntry`] items into their final declared `Gav` form
/// (versions filled in from BOMs where applicable).
///
/// This is the same seeding logic as [`resolve`] but stops there — no BFS,
/// no POM/JAR downloads.  Intended for tooling such as `curie publish` that
/// needs to know the resolved versions of the declared deps but doesn't
/// need the full transitive closure.
pub fn resolve_declared_gavs(deps: &[DepEntry], opts: &ResolveOptions) -> Result<Vec<Gav>> {
    let central = effective_repos(&opts.default_repos);
    let client = build_http_client()?;
    let global_managed = resolve_boms_with_opts(&opts.bom_imports, &central, &client, opts)?;

    let mut out = Vec::with_capacity(deps.len());
    for dep in deps {
        let version: String = if dep.version.is_empty() {
            global_managed
                .get(dep.key)
                .with_context(|| {
                    format!(
                        "dependency \"{}\" has no version and is not managed by any BOM",
                        dep.key
                    )
                })?
                .clone()
        } else {
            dep.version.to_string()
        };
        let g = Gav::from_key_version_classifier(dep.key, &version, dep.classifier)?;
        out.push(g);
    }
    Ok(out)
}

/// Resolve a list of [`DepEntry`] items from `Curie.toml` into a list of
/// local JAR paths (including transitive dependencies).
///
/// An entry with an empty version string (`""`) means the version must be
/// supplied by one of the BOMs in `opts.bom_imports`; it is a hard error if
/// no BOM provides it.
///
/// Prefer [`resolve_full`] when the caller needs SNAPSHOT pins for `Curie.lock`.
pub fn resolve(deps: &[DepEntry], opts: &ResolveOptions) -> Result<Vec<PathBuf>> {
    Ok(resolve_full(deps, opts, &[])?.jars)
}

/// Like [`resolve`], but treats each `group:artifact` key in `pins` as already
/// resolved before the transitive walk begins.  A transitive occurrence of a
/// pinned coordinate — including one declaring a version range — is then skipped
/// exactly as a sibling root would skip it.  `curie fetch` uses this so that one
/// supplied coordinate suppresses the matching transitive range in every other
/// coordinate's tree while still fetching each coordinate independently.
pub fn resolve_with_pins(
    deps: &[DepEntry],
    opts: &ResolveOptions,
    pins: &[String],
) -> Result<Vec<PathBuf>> {
    Ok(resolve_full(deps, opts, pins)?.jars)
}

/// Full dependency resolution including the SNAPSHOT pin map for lockfile
/// writers.  See [`resolve`] / [`resolve_with_pins`] for the algorithm.
pub fn resolve_full(
    deps: &[DepEntry],
    opts: &ResolveOptions,
    pins: &[String],
) -> Result<ResolveResult> {
    let central = effective_repos(&opts.default_repos);

    // Build a lookup map from repo id → Repository for named repos.
    let named_map: std::collections::HashMap<&str, &Repository> = opts
        .named_repos
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();

    let client = build_http_client()?;

    // BOMs are resolved using the same default repos as regular deps
    // so that a Central mirror is respected here too.
    let global_managed = resolve_boms_with_opts(&opts.bom_imports, &central, &client, opts)?;

    // -----------------------------------------------------------------------
    // Phase 1: level-synchronised parallel BFS over POMs.
    //
    // All POMs at the same BFS depth are independent of each other and are
    // fetched concurrently (up to PARALLEL_POM_FETCHES threads).  Once a
    // level's POMs are all parsed, their children form the next level.
    //
    // This preserves both Maven conflict-resolution rules:
    //   - Nearest wins: shallower levels are fully resolved before deeper ones.
    //   - First declared wins (same depth): the serial result-collection pass
    //     processes items in their original declaration order.
    // -----------------------------------------------------------------------

    let mut visited: HashSet<String> = HashSet::new();
    // Chosen version per `group:artifact`, recorded as each GA is committed.
    // Used to detect major-version conflicts against later discarded candidates.
    let mut chosen: HashMap<String, String> = HashMap::new();
    // Coordinates the user opted out of conflict errors for (`allowVersionConflict`).
    let mut allow_conflict: HashSet<String> = HashSet::new();
    let mut conflicts: Vec<VersionConflict> = Vec::new();
    // Ordered list of (GAV, fetch_repos) in BFS discovery order — used in Phase 2.
    let mut ordered_gavs: Vec<(Gav, Vec<Repository>)> = Vec::new();
    let mut range_violations: Vec<RangeViolation> = Vec::new();

    // Seed the first level from declared dependencies.  At depth 0 the user's
    // explicit version always wins — BOMs only fill in when the version is empty.
    let mut current_level: Vec<BfsWork> = Vec::new();
    for dep in deps {
        let resolved_version: String = if dep.version.is_empty() {
            // Version comes from a BOM — hard error if not found.
            global_managed
                .get(dep.key)
                .with_context(|| {
                    format!(
                        "dependency \"{}\" has no version and is not managed by any BOM \
                     in [bom-imports]; either add a version or import a BOM that \
                     manages this artifact",
                        dep.key
                    )
                })?
                .clone()
        } else {
            dep.version.to_string()
        };

        if is_version_range(&resolved_version) {
            range_violations.push(RangeViolation {
                dep_key: dep.key.to_string(),
                range: resolved_version,
                declared_in: curie_toml_gav(),
            });
            continue;
        }

        // Compute per-artifact repo context based on the optional repo_id.
        //
        // * No repo_id: fetch from Central only; transitives also Central only.
        // * repo_id = "X": fetch this artifact from repo X only; transitives
        //   may come from Central OR X.
        let (fetch_repos, child_repos): (Vec<Repository>, Vec<Repository>) =
            if let Some(repo_id) = dep.repo_id {
                let named: Repository = (*named_map.get(repo_id).with_context(|| {
                    format!(
                        "dependency \"{}\" references unknown repository \"{}\"; \
                         declare it with [[repositories]]",
                        dep.key, repo_id
                    )
                })?)
                .clone();
                let mut child = central.clone();
                child.push(named.clone());
                (vec![named], child)
            } else {
                (central.clone(), central.clone())
            };

        let gav = if let Some(c) = dep.classifier {
            let mut g = Gav::from_key_version(dep.key, &resolved_version)?;
            g.classifier = Some(c.to_string());
            g
        } else {
            Gav::from_key_version(dep.key, &resolved_version)?
        };
        let ga = format!("{}:{}", gav.group, gav.artifact);
        if dep.allow_version_conflict {
            allow_conflict.insert(ga.clone());
        }
        let user_exclusions = parse_exclusion_strings(&dep.exclusions);
        if visited.insert(ga.clone()) {
            chosen.insert(ga, resolved_version);
            current_level.push(BfsWork {
                gav,
                fetch_repos,
                child_repos,
                depth: 0,
                via: None,
                exclusions: user_exclusions,
            });
        } else if let Some(kept) = chosen.get(&ga) {
            // A second declaration of the same group:artifact is dropped (first
            // wins).  If the dropped version's major differs, that's a conflict.
            if differs_by_major(kept, &resolved_version) {
                conflicts.push(VersionConflict {
                    key: ga,
                    chosen: kept.clone(),
                    needed: resolved_version,
                    via: None,
                });
            }
        }
    }

    // Mark pinned coordinates as already resolved.  Done *after* root seeding so
    // a pin that is also a declared root is still fetched; its only effect is to
    // make later transitive occurrences (including ranges) skip as "visited".
    for ga in pins {
        visited.insert(ga.clone());
    }

    // Phase 1 spinner — shows a running count of resolved POMs.  Cleared
    // silently at the end so fully-cached runs produce no visible output.
    let phase1_spinner: Option<ProgressBar> = if opts.progress {
        let sp = ProgressBar::new_spinner();
        sp.set_style(
            ProgressStyle::with_template("  Resolving      {spinner} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        sp.enable_steady_tick(Duration::from_millis(80));
        Some(sp)
    } else {
        None
    };

    while !current_level.is_empty() {
        // Parallel fetch: each thread pulls the next item from `current_level`
        // via an atomic index and stores (index, pom_result).
        let pom_results = parallel_pom_fetch(&current_level, &client, opts);

        // A POM that failed to fetch/read/parse (including any parent in its
        // chain) is fatal: silently skipping it would drop its transitive
        // subtree and yield an incomplete classpath (bug #15).
        for (i, r) in pom_results.iter().enumerate() {
            if let Some(Err(e)) = r {
                bail!("failed to resolve {}: {:#}", current_level[i].gav, e);
            }
        }

        // Serial pass: collect results in level order, deduplicate via `visited`,
        // and build the next level.  Processing in declaration order preserves
        // "first declared wins" for same-depth conflicts.
        let mut next_level: Vec<BfsWork> = Vec::new();
        for (i, work) in current_level.iter().enumerate() {
            // A pom-packaged (aggregator) artifact has no JAR of its own: expand
            // its dependencies but don't download it or put it on the classpath.
            // Applies to both directly-declared and transitive nodes (bug #11).
            let is_aggregator = matches!(&pom_results[i], Some(Ok(p)) if p.is_pom_packaging());
            if !is_aggregator {
                ordered_gavs.push((work.gav.clone(), work.fetch_repos.clone()));
            }

            if let Some(Ok(pom)) = &pom_results[i] {
                for dep in pom.dependencies.iter().filter(|d| d.is_compile_scope()) {
                    let group = pom.resolve_value(&dep.group_id);
                    let artifact = pom.resolve_value(&dep.artifact_id);
                    let ga_key = format!("{}:{}", group, artifact);

                    // Check against accumulated exclusions from all ancestors.
                    if is_excluded(&group, &artifact, &work.exclusions) {
                        continue;
                    }

                    // Nearest-wins short-circuit: already committed to a version
                    // for this GA at a shallower depth — skip.  If the discarded
                    // candidate's major differs from the chosen one, record a
                    // conflict (surfaced as an error later unless opted out).
                    if visited.contains(&ga_key) {
                        if let Some(candidate) = resolve_transitive_version(
                            &ga_key,
                            dep.version.as_deref(),
                            pom,
                            &global_managed,
                        ) {
                            if let Some(kept) = chosen.get(&ga_key) {
                                if differs_by_major(kept, &candidate) {
                                    conflicts.push(VersionConflict {
                                        key: ga_key.clone(),
                                        chosen: kept.clone(),
                                        needed: candidate,
                                        via: Some(work.gav.clone()),
                                    });
                                }
                            }
                        }
                        continue;
                    }

                    let raw_version = match resolve_transitive_version(
                        &ga_key,
                        dep.version.as_deref(),
                        pom,
                        &global_managed,
                    ) {
                        Some(v) => v,
                        None => continue, // unresolvable — drop this dep
                    };

                    if is_version_range(&raw_version) {
                        if !opts.skip_version_ranges {
                            range_violations.push(RangeViolation {
                                dep_key: ga_key,
                                range: raw_version,
                                declared_in: work.gav.clone(),
                            });
                        }
                        continue;
                    }

                    let child_exclusions = merge_exclusions(&work.exclusions, &dep.exclusions);
                    let child_gav = Gav {
                        group,
                        artifact,
                        version: raw_version,
                        classifier: dep.classifier.clone(),
                        extension: None,
                        snapshot_version: None,
                    };
                    chosen.insert(ga_key.clone(), child_gav.version.clone());
                    visited.insert(ga_key);
                    // Transitives inherit the parent's child_repos.
                    next_level.push(BfsWork {
                        gav: child_gav,
                        fetch_repos: work.child_repos.clone(),
                        child_repos: work.child_repos.clone(),
                        depth: 0,
                        via: None,
                        exclusions: child_exclusions,
                    });
                }
            }
        }

        if let Some(sp) = &phase1_spinner {
            sp.set_message(format!("{} POM(s)", ordered_gavs.len()));
        }

        current_level = next_level;
    }

    if let Some(sp) = phase1_spinner {
        sp.finish_and_clear();
    }

    if !range_violations.is_empty() {
        return Err(anyhow::Error::new(VersionRangeError {
            violations: range_violations,
        }));
    }

    // Major-version conflicts are a hard error for user-declared dependency
    // graphs (opt-in via `error_on_version_conflict`), unless the specific
    // coordinate opted out with `allowVersionConflict = true` in Curie.toml.
    if opts.error_on_version_conflict {
        let unresolved: Vec<VersionConflict> = conflicts
            .into_iter()
            .filter(|c| !allow_conflict.contains(&c.key))
            .collect();
        if !unresolved.is_empty() {
            bail!("{}", format_conflict_error(&unresolved));
        }
    }

    // -----------------------------------------------------------------------
    // Resolve unique SNAPSHOT filenames before Phase 2 so JAR paths and
    // lockfile pins reflect the timestamped artifact.
    // -----------------------------------------------------------------------
    for (gav, fetch_repos) in &mut ordered_gavs {
        prepare_snapshot_gav(gav, fetch_repos, &client, opts)?;
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
        return Ok(ResolveResult {
            jars: vec![],
            snapshot_pins: BTreeMap::new(),
        });
    }

    // Count how many JARs are not yet in the local cache — only those will
    // be downloaded and shown on the progress bar.
    let missing: u64 = ordered_gavs
        .iter()
        .filter(|(g, _)| {
            g.local_repository_path()
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
            ProgressStyle::with_template("  Downloading     [{bar:40.cyan/blue}] {pos}/{len}")
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

    let mut snapshot_pins = BTreeMap::new();
    for (gav, _) in &ordered_gavs {
        if gav.is_snapshot() {
            if let Some(sv) = &gav.snapshot_version {
                snapshot_pins.insert(gav.snapshot_pin_key(), sv.clone());
            }
        }
    }

    Ok(ResolveResult {
        jars: ordered_jars,
        snapshot_pins,
    })
}

// ---------------------------------------------------------------------------
// SNAPSHOT unique-version resolution
// ---------------------------------------------------------------------------

/// Populate `gav.snapshot_version` for a `-SNAPSHOT` coordinate.
///
/// Order of preference:
/// 1. Existing pin in [`ResolveOptions::snapshot_pins`] (from `Curie.lock`),
///    unless [`ResolveOptions::update_snapshots`] (`-U`) forces a refresh.
/// 2. Version-level `maven-metadata.xml` from the first repo that publishes it
///    (always re-fetched when unpinned — no Maven-style daily cache policy).
/// 3. Leave `snapshot_version = None` (non-unique / local-install layout).
///
/// Deleting `Curie.lock` and building without `-U` therefore behaves the same
/// as `-U` for SNAPSHOT resolution: both re-query repository metadata.
fn prepare_snapshot_gav(
    gav: &mut Gav,
    repos: &[Repository],
    client: &Client,
    opts: &ResolveOptions,
) -> Result<()> {
    if !gav.is_snapshot() {
        return Ok(());
    }

    let pin_key = gav.snapshot_pin_key();

    if !opts.update_snapshots {
        if let Some(unique) = opts.snapshot_pins.get(&pin_key) {
            gav.snapshot_version = Some(unique.clone());
            return Ok(());
        }
        // Keep an already-resolved unique version (e.g. re-prepare).
        if gav.snapshot_version.is_some() {
            return Ok(());
        }
    } else {
        // Force refresh: drop any prior resolution so metadata is consulted.
        gav.snapshot_version = None;
    }

    if opts.offline {
        // Offline without a pin: use non-unique layout if present in cache.
        return Ok(());
    }

    // Unpinned (or -U): always re-fetch version-level metadata from the repo.
    // There is no same-day / interval freshness window — Curie.lock is the only
    // pin; without it every resolve asks the repository again.
    let relative_meta = gav.relative_snapshot_metadata_path();
    let mut last_err: Option<anyhow::Error> = None;

    for repo in repos {
        let url = repo.artifact_url(&relative_meta);
        let local_meta = local_snapshot_metadata_path(gav, &repo.id);

        let xml = match fetch_text(client, &url) {
            Ok(Some(body)) => {
                // Best-effort cache for offline recovery on later network errors;
                // never used as a freshness gate on the happy path.
                if let Some(parent) = local_meta.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&local_meta, &body);
                body
            }
            Ok(None) => continue, // 404 — try next repo
            Err(e) => {
                last_err = Some(e);
                // Network failure: fall back to last cached metadata if any.
                if local_meta.exists() {
                    match std::fs::read_to_string(&local_meta) {
                        Ok(body) => body,
                        Err(_) => continue,
                    }
                } else {
                    continue;
                }
            }
        };

        let meta = snapshot_meta::parse_snapshot_metadata(&xml)
            .with_context(|| format!("failed to parse snapshot metadata for {gav} from {url}"))?;

        let classifier = gav.classifier.as_deref();
        // Prefer jar entry (classpath), then pom — both usually share the value.
        let unique = meta
            .resolve_unique_version(&gav.version, "jar", classifier)
            .or_else(|| meta.resolve_unique_version(&gav.version, "pom", None));

        if let Some(u) = unique {
            gav.snapshot_version = Some(u);
        }
        // Whether unique or non-unique, this repo answered — stop searching.
        return Ok(());
    }

    if let Some(e) = last_err {
        // All repos failed with errors (not just 404).  Surface the last one
        // only when we also have no non-unique local artifact to fall back to.
        let non_unique = gav.local_repository_path()?;
        if !non_unique.exists() {
            return Err(e).context(format!(
                "failed to resolve unique snapshot version for {gav}"
            ));
        }
    }

    // No metadata → non-unique layout (local install / some Nexus configs).
    Ok(())
}

/// Cached version-level metadata path under `~/.m2`, keyed by repo id
/// (Maven layout: `maven-metadata-<repoId>.xml`). Used only as a network-error
/// fallback; unpinned resolves always try the remote first.
fn local_snapshot_metadata_path(gav: &Gav, repo_id: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".m2")
        .join("repository")
        .join(gav.group_path())
        .join(&gav.artifact)
        .join(&gav.version)
        .join(format!("maven-metadata-{repo_id}.xml"))
}

/// Fetch a URL as text. Supports `http(s)://` and `file://` (and bare
/// `file:` relative forms that have already been absolutised by the caller).
/// Returns `Ok(None)` on HTTP 404 so callers can try the next repository.
fn fetch_text(client: &Client, url: &str) -> Result<Option<String>> {
    if let Some(path) = file_url_to_path(url) {
        if !path.exists() {
            return Ok(None);
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        return Ok(Some(body));
    }

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed for {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }
    let body = response
        .text()
        .with_context(|| format!("failed to read response body for {url}"))?;
    Ok(Some(body))
}

/// Convert a `file://` / `file:` URL to a filesystem path. Returns `None` for
/// non-file schemes.
///
/// Accepted forms:
/// - `file:///abs/path` → `/abs/path`
/// - `file://localhost/abs/path` → `/abs/path`
/// - `file:/abs/path` → `/abs/path`
/// - `file:relative/path` → `relative/path`
/// - `file://relative/path` → `relative/path` (no host authority; used when
///   the caller has not yet absolutised a project-relative repo)
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:")?;
    if let Some(stripped) = rest.strip_prefix("//") {
        // file:///abs → stripped starts with '/'
        if stripped.starts_with('/') {
            return Some(PathBuf::from(stripped));
        }
        // file://localhost/abs
        if let Some(p) = stripped.strip_prefix("localhost/") {
            return Some(PathBuf::from(format!("/{p}")));
        }
        if stripped == "localhost" {
            return Some(PathBuf::from("/"));
        }
        // file://relative/or/windows — treat the whole remainder as a path
        // (do NOT strip a fake "host" segment).
        return Some(PathBuf::from(stripped));
    }
    // file:/abs or file:relative
    Some(PathBuf::from(rest))
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
        if let Some(resolved) = pom.try_resolve_value(bom_v) {
            return Some(resolved);
        }
        // BOM value still references a ${...}; fall through to other sources.
    }

    // 2. Dep's explicit version, resolved against properties.
    if let Some(v) = dep_explicit {
        if let Some(resolved) = pom.try_resolve_value(v) {
            return Some(resolved);
        }
        // Unresolved property: try dep's own managed_versions, then global.
        return pom
            .managed_versions
            .get(ga_key)
            .or_else(|| global_managed.get(ga_key))
            .and_then(|mv| pom.try_resolve_value(mv));
    }

    // 3. No explicit version — fall back to dep's own managed_versions, then
    //    global BOM map.
    let mv = pom
        .managed_versions
        .get(ga_key)
        .or_else(|| global_managed.get(ga_key))?;
    pom.try_resolve_value(mv)
}

// ---------------------------------------------------------------------------

enum ArtifactKind {
    Jar,
    Pom,
}

/// Download the POM and JAR for a single artifact into the local Maven
/// cache, without resolving its transitive dependencies.  Returns the
/// cached JAR path.  Used by `curie fetch <gav> --no-transitive`.
pub fn fetch_artifact(gav: &Gav, repos: &[Repository], offline: bool) -> Result<PathBuf> {
    let client = build_http_client()?;
    let opts = ResolveOptions {
        offline,
        ..ResolveOptions::default()
    };
    let mut resolved = gav.clone();
    prepare_snapshot_gav(&mut resolved, repos, &client, &opts)?;
    ensure_artifact(
        &resolved,
        repos,
        &client,
        ArtifactKind::Pom,
        offline,
        None,
        None,
    )?;
    ensure_artifact(
        &resolved,
        repos,
        &client,
        ArtifactKind::Jar,
        offline,
        None,
        None,
    )
}

/// Download only the POM for an artifact — no JAR.  Used for BOM and
/// parent-POM pre-fetching where no JAR exists.  Returns the cached POM path.
pub fn fetch_pom_only(gav: &Gav, repos: &[Repository], offline: bool) -> Result<PathBuf> {
    let client = build_http_client()?;
    let opts = ResolveOptions {
        offline,
        ..ResolveOptions::default()
    };
    let mut resolved = gav.clone();
    prepare_snapshot_gav(&mut resolved, repos, &client, &opts)?;
    ensure_artifact(
        &resolved,
        repos,
        &client,
        ArtifactKind::Pom,
        offline,
        None,
        None,
    )
}

// ---------------------------------------------------------------------------
// maven-metadata.xml — available-version discovery (for resolving ranges).
// ---------------------------------------------------------------------------

/// Fetch the list of available versions for a `"group:artifact"` key from the
/// first repository in `repos` that publishes a `maven-metadata.xml` for it.
///
/// Unlike artifact downloads, `maven-metadata.xml` is mutable and not
/// checksum-pinned, so it is fetched with a plain GET and never cached in
/// `~/.m2/repository`.  Used by `curie fetch` to turn a version range into a
/// concrete suggestion; a range cannot be resolved offline, so `offline` is a
/// hard error.
pub fn fetch_available_versions(
    ga: &str,
    repos: &[Repository],
    offline: bool,
) -> Result<Vec<String>> {
    if offline {
        bail!("cannot resolve a version range for {ga} without network access (--offline)");
    }
    let (group, artifact) = ga
        .split_once(':')
        .with_context(|| format!("invalid coordinate key {ga:?}; expected \"group:artifact\""))?;
    let relative = format!(
        "{}/{}/maven-metadata.xml",
        group.replace('.', "/"),
        artifact
    );

    let client = build_http_client()?;
    for repo in &effective_repos(repos) {
        let url = repo.artifact_url(&relative);
        if let Some(xml) = http_get_text(&client, &url)? {
            let versions =
                parse_metadata_versions(&xml).with_context(|| format!("failed to parse {url}"))?;
            if !versions.is_empty() {
                return Ok(versions);
            }
        }
    }
    bail!("no maven-metadata.xml with versions found for {ga} in any repository");
}

/// Plain-text GET that returns `None` on a 404 (so the caller can try the next
/// repository) and errors on any other non-success status or transport failure.
/// Supports `file://` repository URLs.
fn http_get_text(client: &Client, url: &str) -> Result<Option<String>> {
    fetch_text(client, url)
}

/// Extract the `<versioning><versions><version>…</version></versions></versioning>`
/// entries from a `maven-metadata.xml` document, in document order.
fn parse_metadata_versions(xml: &str) -> Result<Vec<String>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut versions = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();

    loop {
        match reader.read_event().context("XML read error")? {
            Event::Start(e) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())
                    .context("invalid UTF-8 in metadata tag name")?
                    .to_string();
                path.push(tag);
                text.clear();
            }
            Event::Text(e) => {
                let decoded = e.decode().context("invalid encoding in metadata text")?;
                text.push_str(&decoded);
            }
            Event::End(_) => {
                if path_ends_with(&path, &["versioning", "versions", "version"]) {
                    let v = text.trim();
                    if !v.is_empty() {
                        versions.push(v.to_string());
                    }
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(versions)
}

/// True when the tail of `path` equals `tail` (element by element).
fn path_ends_with(path: &[String], tail: &[&str]) -> bool {
    path.len() >= tail.len()
        && path[path.len() - tail.len()..]
            .iter()
            .zip(tail)
            .all(|(a, b)| a == b)
}

/// Download an arbitrary artifact file (any classifier and any file extension)
/// into the local Maven cache.
///
/// This is the hardened path used by normal dependency resolution:
/// - On cache hit the sidecar is used for fast verification (or fetched +
///   persisted if missing).
/// - Writes are performed atomically via a unique collocated staging file.
/// - The provided `repos` (including mirrors from `~/.curie/config.toml` and
///   project `[[repositories]]`) are respected.
/// - A properly configured HTTP client (timeout + User-Agent) is used.
///
/// Intended for plugin artifacts (e.g. `protoc` executables, generator JARs
/// with custom extensions or classifiers) so they benefit from the same
/// safety guarantees as ordinary dependencies.
pub fn fetch_artifact_file(
    group: &str,
    artifact: &str,
    version: &str,
    classifier: Option<&str>,
    extension: &str,
    repos: &[Repository],
    offline: bool,
) -> Result<PathBuf> {
    let key = format!("{}:{}", group, artifact);
    let mut gav = Gav::from_key_version_classifier(&key, version, classifier)?;
    gav.extension = if extension.is_empty() {
        None
    } else {
        Some(extension.to_string())
    };

    let client = build_http_client()?;
    ensure_artifact(&gav, repos, &client, ArtifactKind::Jar, offline, None, None)
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
        ArtifactKind::Jar => gav.local_repository_path()?,
        ArtifactKind::Pom => gav.pom_local_repository_path()?,
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

    // Ensure parent directory exists (before any download attempt).
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }

    // -------------------------------------------------------------------
    // In-process coordination gate (Layer A).
    // Only one thread in this process may perform the network+commit work
    // for a given final artifact path at a time. Others wait for it to appear.
    // This prevents the classic "two threads writing the same .part" race
    // inside parallel_pom_fetch, Phase-2 JAR downloads, and run_jobs.
    // -------------------------------------------------------------------
    let local_key = local_path.to_string_lossy().to_string();
    let responsible = claim_or_wait_for_download(&local_key);

    if !responsible {
        // Another thread is fetching this exact artifact (same POM or JAR).
        // Wait for the file to materialise, then treat as a normal cache hit.
        wait_for_artifact_to_appear(&local_path);
        if local_path.exists() {
            ensure_verified(&local_path, &relative, repos, client, offline)
                .with_context(|| format!("checksum verification failed for cached {}", gav))?;
            return Ok(local_path);
        }
        // Rare: timed out waiting. Fall through and try ourselves (we will
        // still claim below on the next iteration of an outer loop if we add one,
        // but for simplicity we proceed to attempt; the FS layer will still protect).
    }

    // We are responsible for this artifact (or the waiter gave up). Show progress.
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
                if responsible {
                    release_download_slot(&local_key);
                }
                return Ok(local_path);
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    // All repos failed (or we were a waiter that fell through).
    // Only the responsible party should release.
    if responsible {
        release_download_slot(&local_key);
    }

    let err = last_err.unwrap_or_else(|| anyhow::anyhow!("no repositories configured"));
    bail!("failed to download {} from all repositories: {}", gav, err);
}

/// Build a unique collocated staging path next to `dest`.
/// The name preserves the original filename so that a POM and a JAR for the
/// same `group:artifact:version` never collide on the staging file.
fn compute_unique_staging_path(dest: &Path) -> PathBuf {
    let pid = std::process::id();
    let seq = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let orig = dest
        .file_name()
        .expect("destination must have a filename component")
        .to_string_lossy();
    dest.with_file_name(format!("{}.part.{}.{}", orig, pid, seq))
}

/// Atomically stage `bytes` for `dest` using a unique temp file, then rename.
/// The sidecar is written only after the content file is visible at `dest`.
/// If the rename loses a race and `dest` now exists we treat it as success
/// (another writer won) and clean up.
fn stage_and_rename_atomically(
    dest: &Path,
    bytes: &[u8],
    sidecar: &Path,
    sidecar_bytes: &[u8],
) -> Result<()> {
    let part = compute_unique_staging_path(dest);
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create staging dir {}", parent.display()))?;
    }
    std::fs::write(&part, bytes).with_context(|| format!("failed to write {}", part.display()))?;

    match std::fs::rename(&part, dest) {
        Ok(()) => {
            // Winner path: content is now visible; persist sidecar next to it.
            std::fs::write(sidecar, sidecar_bytes)
                .with_context(|| format!("failed to write sidecar {}", sidecar.display()))?;
            Ok(())
        }
        Err(e) => {
            if dest.exists() {
                let _ = std::fs::remove_file(&part);
                // Best-effort: ensure a sidecar exists (idempotent write).
                if !sidecar.exists() {
                    let _ = std::fs::write(sidecar, sidecar_bytes);
                }
                Ok(())
            } else {
                Err(e).with_context(|| {
                    format!(
                        "failed to rename {} \u{2192} {}",
                        part.display(),
                        dest.display()
                    )
                })
            }
        }
    }
}

/// Download `url` to `dest`, verify its checksum against the published
/// `.sha256` (or `.sha1` fallback) sidecar, and persist the sidecar alongside
/// the artifact for fast cache-hit verification on subsequent runs.
///
/// Supports `http(s)://` and `file://` repository URLs.
///
/// A missing sidecar is a hard error — every well-formed Maven repository
/// publishes one (Maven's deploy plugin and Nexus/Artifactory both generate
/// them on upload).  A missing sidecar usually means a misconfigured proxy or
/// a manually-uploaded artifact, and either way we refuse to install an
/// unverifiable JAR.
fn download(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<()> {
    let bytes = if let Some(path) = file_url_to_path(url) {
        if !path.exists() {
            bail!("HTTP 404 for {}", url);
        }
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?
    } else {
        let response = client
            .get(url)
            .send()
            .with_context(|| format!("HTTP request failed for {}", url))?;

        if !response.status().is_success() {
            bail!("HTTP {} for {}", response.status(), url);
        }

        response
            .bytes()
            .with_context(|| format!("failed to read response body for {}", url))?
            .to_vec()
    };

    let (expected_hex, kind) = fetch_any_remote_checksum(client, url)?;
    verify_bytes(&bytes, &expected_hex, kind)
        .with_context(|| format!("downloaded artifact from {} failed checksum", url))?;

    // Use a unique collocated staging file and atomic rename (with tolerant
    // "already exists" success) so that concurrent writers (intra-process or
    // across processes) cannot corrupt the final artifact or fail spuriously.
    // The sidecar is installed only after the content is visible.
    let sidecar = sidecar_path(dest, kind);
    stage_and_rename_atomically(dest, &bytes, &sidecar, expected_hex.as_bytes())
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
    let body = match fetch_text(client, &sidecar_url)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let hex = parse_checksum_text(&body).with_context(|| {
        format!(
            "sidecar {} returned malformed checksum text {:?}",
            sidecar_url, body
        )
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
        managed: &[(&str, &str, &str)], // (group, artifact, version)
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
            classifier: None,
            extension: None,
            snapshot_version: None,
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
            "com.example",
            "my-bom",
            "1.0.0",
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
            "com.example",
            "bom-a",
            "1.0.0",
            &[("org.foo", "bar", "1.0.0")],
            &[],
        );
        let bom_b = write_fake_bom(
            dir.path(),
            "com.example",
            "bom-b",
            "1.0.0",
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
            "com.example",
            "nested-bom",
            "1.0.0",
            &[("org.foo", "bar", "1.0.0")],
            &[],
        );
        // outer-bom imports nested-bom AND overrides org.foo:bar to 9.9.9
        let outer = write_fake_bom(
            dir.path(),
            "com.example",
            "outer-bom",
            "1.0.0",
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
            "com.example",
            "bom-a",
            "1.0.0",
            &[("org.foo", "x", "1.0")],
            &[("com.example", "bom-b", "1.0.0")],
        );
        write_fake_bom(
            dir.path(),
            "com.example",
            "bom-b",
            "1.0.0",
            &[("org.foo", "y", "2.0")],
            &[("com.example", "bom-a", "1.0.0")],
        );
        let bom_a = Gav {
            group: "com.example".into(),
            artifact: "bom-a".into(),
            version: "1.0.0".into(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
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
        deps: &[(&str, &str, &str)], // (group, artifact, version)
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

    /// Like [`make_pom`] but marks the artifact `<packaging>pom</packaging>`
    /// (an aggregator with no JAR of its own).
    fn make_aggregator_pom(
        group: &str,
        artifact: &str,
        version: &str,
        deps: &[(&str, &str, &str)],
    ) -> String {
        let mut xml = format!(
            r#"<?xml version="1.0"?>
<project>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{version}</version>
  <packaging>pom</packaging>
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
        write_fake_artifact_with_pom(
            home_dir,
            group,
            artifact,
            version,
            &make_pom(group, artifact, version, deps),
        )
    }

    /// Like [`write_fake_artifact`], but with caller-supplied POM XML — for
    /// tests that need a `<parent>` reference or `<properties>` beyond what
    /// [`make_pom`] covers.
    fn write_fake_artifact_with_pom(
        home_dir: &std::path::Path,
        group: &str,
        artifact: &str,
        version: &str,
        pom_xml: &str,
    ) -> Gav {
        let gav = Gav {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let m2 = home_dir.join(".m2").join("repository");

        // POM (+ sidecar).
        let pom_path = m2.join(gav.relative_pom_path());
        write_with_sidecar(&pom_path, pom_xml.as_bytes());

        // Empty JAR (placeholder — resolver only checks existence + checksum).
        let jar_path = m2.join(gav.relative_path());
        write_with_sidecar(&jar_path, b"");

        gav
    }

    /// Write a POM-only artifact (no JAR) into the fake local Maven cache —
    /// for parent POMs, which `merge_parent_chain` fetches but never needs a
    /// JAR for.
    fn write_fake_pom(
        home_dir: &std::path::Path,
        group: &str,
        artifact: &str,
        version: &str,
        pom_xml: &str,
    ) -> Gav {
        let gav = Gav {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let pom_path = home_dir
            .join(".m2")
            .join("repository")
            .join(gav.relative_pom_path());
        write_with_sidecar(&pom_path, pom_xml.as_bytes());
        gav
    }

    /// Build a POM with `<properties>` and no dependencies — used as a
    /// parent POM whose properties are inherited by a child's `<parent>`.
    fn make_pom_with_properties(
        group: &str,
        artifact: &str,
        version: &str,
        properties: &[(&str, &str)],
    ) -> String {
        let mut xml = format!(
            r#"<?xml version="1.0"?>
<project>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{version}</version>
  <properties>
"#
        );
        for (k, v) in properties {
            xml.push_str(&format!("    <{k}>{v}</{k}>\n"));
        }
        xml.push_str("  </properties>\n</project>");
        xml
    }

    /// Build a POM with a `<parent>` reference and a flat `<dependencies>`
    /// list, where each dependency's version string is used verbatim — so
    /// it may contain an unresolved `${...}` placeholder for testing
    /// property inheritance through the parent chain.
    fn make_pom_with_parent(
        group: &str,
        artifact: &str,
        version: &str,
        parent: (&str, &str, &str),
        deps: &[(&str, &str, &str)],
    ) -> String {
        let (parent_group, parent_artifact, parent_version) = parent;
        let mut xml = format!(
            r#"<?xml version="1.0"?>
<project>
  <parent>
    <groupId>{parent_group}</groupId>
    <artifactId>{parent_artifact}</artifactId>
    <version>{parent_version}</version>
  </parent>
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

    /// Write a fake classified artifact (e.g. with "runtime" classifier).
    /// The JAR filename will include the classifier suffix.
    /// Returns the corresponding Gav (with classifier set).
    fn write_fake_classified_artifact(
        home_dir: &std::path::Path,
        group: &str,
        artifact: &str,
        version: &str,
        classifier: &str,
        deps: &[(&str, &str, &str)],
    ) -> Gav {
        let key = format!("{}:{}", group, artifact);
        let mut gav = Gav::from_key_version(&key, version).unwrap();
        gav.classifier = Some(classifier.to_string());
        let m2 = home_dir.join(".m2").join("repository");

        // POM is always for the main GAV (no classifier in POM filename).
        let pom_gav = Gav {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let pom_path = m2.join(pom_gav.relative_pom_path());
        let pom_xml = make_pom(group, artifact, version, deps);
        write_with_sidecar(&pom_path, pom_xml.as_bytes());

        // Classified JAR.
        let jar_path = m2.join(gav.relative_path());
        if let Some(parent) = jar_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_with_sidecar(&jar_path, b"fake-classified-jar");

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
            .map(|(k, v)| DepEntry {
                key: k,
                version: v,
                repo_id: None,
                exclusions: vec![],
                classifier: None,
                allow_version_conflict: false,
            })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports,
            offline: true,
            // These helpers exercise the user-dependency path, so conflict
            // errors are enabled (matching compile.rs / test.rs).
            skip_version_ranges: false,
            error_on_version_conflict: true,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve(&entries, &opts);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    /// Like [`run_resolve`] but passes `pins` (a set of `group:artifact` keys)
    /// to [`resolve_with_pins`], so a matching transitive range is skipped.
    fn run_resolve_with_pins(
        home_dir: &std::path::Path,
        deps: &[(&str, &str)],
        pins: &[&str],
    ) -> Result<Vec<PathBuf>> {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());

        let entries: Vec<DepEntry> = deps
            .iter()
            .map(|(k, v)| DepEntry {
                key: k,
                version: v,
                repo_id: None,
                exclusions: vec![],
                classifier: None,
                allow_version_conflict: false,
            })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let pins: Vec<String> = pins.iter().map(|s| s.to_string()).collect();
        let result = resolve_with_pins(&entries, &opts, &pins);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    /// Like [`run_resolve`] but each dep carries an `allow_version_conflict`
    /// flag — for exercising the major-version-conflict error + opt-out.
    fn run_resolve_with_allow(
        home_dir: &std::path::Path,
        deps: &[(&str, &str, bool)],
        bom_imports: Vec<Gav>,
    ) -> Result<Vec<PathBuf>> {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());

        let entries: Vec<DepEntry> = deps
            .iter()
            .map(|(k, v, allow)| DepEntry {
                key: k,
                version: v,
                repo_id: None,
                exclusions: vec![],
                classifier: None,
                allow_version_conflict: *allow,
            })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports,
            offline: true,
            // These helpers exercise the user-dependency path, so conflict
            // errors are enabled (matching compile.rs / test.rs).
            skip_version_ranges: false,
            error_on_version_conflict: true,
            snapshot_pins: Default::default(),
            update_snapshots: false,
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
        // transitively pulls foo:bar 1.5.  Maven nearest-wins: bar 1.0.
        // (Same major on both, so the major-version-conflict check stays quiet.)
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "1.5", &[]);
        write_fake_artifact(dir.path(), "foo", "other", "1.0", &[("foo", "bar", "1.5")]);

        let result = run_resolve(
            dir.path(),
            &[("foo:bar", "1.0"), ("foo:other", "1.0")],
            vec![],
        )
        .unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "expected foo:bar:1.0 in {:?}",
            gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:1.5".to_string()),
            "foo:bar:1.5 must not appear (nearest wins): {:?}",
            gavs,
        );
    }

    #[test]
    fn major_version_conflict_is_an_error() {
        // Declared foo:bar 2.0; foo:other 1.0 transitively needs foo:bar 5.0.
        // Nearest-wins keeps 2.0, but the major differs (2 vs 5) -> hard error.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(dir.path(), "foo", "other", "1.0", &[("foo", "bar", "5.0")]);

        let err = run_resolve(
            dir.path(),
            &[("foo:bar", "2.0"), ("foo:other", "1.0")],
            vec![],
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foo:bar"),
            "should name the conflicting artifact: {msg}"
        );
        assert!(
            msg.contains("2.0") && msg.contains("5.0"),
            "should show both versions: {msg}"
        );
    }

    #[test]
    fn allow_version_conflict_suppresses_error() {
        // Same graph as above, but foo:bar opts out with allowVersionConflict.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(dir.path(), "foo", "other", "1.0", &[("foo", "bar", "5.0")]);

        let result = run_resolve_with_allow(
            dir.path(),
            &[("foo:bar", "2.0", true), ("foo:other", "1.0", false)],
            vec![],
        )
        .unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:2.0".to_string()),
            "expected foo:bar:2.0: {gavs:?}"
        );
    }

    #[test]
    fn minor_version_conflict_is_not_an_error() {
        // Transitive needs foo:bar 2.5 while 2.0 is kept — same major, no error.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "2.0", &[]);
        write_fake_artifact(dir.path(), "foo", "other", "1.0", &[("foo", "bar", "2.5")]);

        let result = run_resolve(
            dir.path(),
            &[("foo:bar", "2.0"), ("foo:other", "1.0")],
            vec![],
        )
        .unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:2.0".to_string()),
            "expected foo:bar:2.0: {gavs:?}"
        );
    }

    #[test]
    fn duplicate_declaration_major_conflict_is_an_error() {
        // The same coordinate declared twice with different majors: the second
        // is dropped (first wins) and the major mismatch is a hard error.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);

        let err = run_resolve(
            dir.path(),
            &[("foo:bar", "1.0"), ("foo:bar", "3.0")],
            vec![],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("foo:bar"),
            "should name foo:bar"
        );

        // Opting out makes it succeed, keeping the first declaration.
        let result = run_resolve_with_allow(
            dir.path(),
            &[("foo:bar", "1.0", true), ("foo:bar", "3.0", true)],
            vec![],
        )
        .unwrap();
        assert!(jar_gavs(&result).contains(&"foo:bar:1.0".to_string()));
    }

    #[test]
    fn major_component_parses_leading_number() {
        assert_eq!(major_component("2.17.2"), Some(2));
        assert_eq!(major_component("5"), Some(5));
        assert_eq!(major_component("1-beta"), Some(1));
        assert_eq!(major_component("RELEASE"), None);
        assert!(differs_by_major("2.0", "5.0"));
        assert!(!differs_by_major("2.0", "2.5"));
        assert!(!differs_by_major("2.0", "RELEASE")); // unparseable -> no conflict
    }

    #[test]
    fn first_declared_wins_at_same_depth() {
        // Two declared deps, both at depth 0, each pulling a different
        // version of foo:bar at depth 1.  BFS visits a's children before
        // b's children → a's version (1.0) wins.  (Both 1.x — same major, so
        // the major-version-conflict check stays quiet.)
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "1.1", &[]);
        write_fake_artifact(dir.path(), "grp", "a", "1.0", &[("foo", "bar", "1.0")]);
        write_fake_artifact(dir.path(), "grp", "b", "1.0", &[("foo", "bar", "1.1")]);

        let result = run_resolve(
            dir.path(),
            // Note: BTreeMap ordering would sort these alphabetically;
            // the resolver receives the &[(&str, &str)] slice in caller
            // order, so a comes before b here.
            &[("grp:a", "1.0"), ("grp:b", "1.0")],
            vec![],
        )
        .unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "first-declared a's choice (foo:bar:1.0) must win: {:?}",
            gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:1.1".to_string()),
            "b's choice (foo:bar:1.1) must lose to a's: {:?}",
            gavs,
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
        write_fake_artifact(dir.path(), "grp", "lib", "1.0", &[("foo", "bar", "2.0")]);
        let bom = write_fake_bom(
            dir.path(),
            "com.example",
            "pin-bom",
            "1.0.0",
            &[("foo", "bar", "9.9.9")],
            &[],
        );

        let result = run_resolve(dir.path(), &[("grp:lib", "1.0")], vec![bom]).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:9.9.9".to_string()),
            "top-level BOM (9.9.9) must override transitive explicit (2.0): {:?}",
            gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:2.0".to_string()),
            "transitive explicit 2.0 must be overridden by BOM: {:?}",
            gavs,
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
            "com.example",
            "pin-bom",
            "1.0.0",
            &[("foo", "bar", "9.9.9")],
            &[],
        );

        let result = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![bom]).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "user's explicit declaration (1.0) must win over BOM (9.9.9): {:?}",
            gavs,
        );
        assert!(
            !gavs.contains(&"foo:bar:9.9.9".to_string()),
            "BOM-pinned version must lose to user's explicit version: {:?}",
            gavs,
        );
    }

    #[test]
    fn dependency_version_from_parent_pom_property_is_resolved() {
        // my-app's POM declares <version>${guava.version}</version> for its
        // guava dependency, but `guava.version` is only defined in its
        // <parent> POM's <properties> — not in my-app's own POM.
        // merge_parent_chain must merge the parent's properties into my-app's
        // before resolve_transitive_version resolves the placeholder.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "com.google.guava", "guava", "30.0-jre", &[]);
        write_fake_pom(
            dir.path(),
            "com.example",
            "parent-pom",
            "1.0",
            &make_pom_with_properties(
                "com.example",
                "parent-pom",
                "1.0",
                &[("guava.version", "30.0-jre")],
            ),
        );
        write_fake_artifact_with_pom(
            dir.path(),
            "com.example",
            "my-app",
            "1.0",
            &make_pom_with_parent(
                "com.example",
                "my-app",
                "1.0",
                ("com.example", "parent-pom", "1.0"),
                &[("com.google.guava", "guava", "${guava.version}")],
            ),
        );

        let result = run_resolve(dir.path(), &[("com.example:my-app", "1.0")], vec![]).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"com.google.guava:guava:30.0-jre".to_string()),
            "guava version from parent POM property must resolve: {:?}",
            gavs,
        );
    }

    #[test]
    fn managed_provided_scope_keeps_inherited_parent_dep_off_classpath() {
        // hamcrest-core 1.1 inherits junit/jmock from hamcrest-parent, but
        // dependencyManagement marks them <scope>provided</scope>.  They
        // must not become compile transitives of a consumer.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "junit", "junit", "4.0", &[]);
        write_fake_artifact(
            dir.path(),
            "jmock",
            "jmock",
            "1.1.0",
            &[("junit", "junit", "3.8.1")],
        );
        write_fake_artifact(dir.path(), "junit", "junit", "3.8.1", &[]);
        write_fake_pom(
            dir.path(),
            "org.hamcrest",
            "hamcrest-parent",
            "1.1",
            r#"<?xml version="1.0"?>
<project>
  <groupId>org.hamcrest</groupId>
  <artifactId>hamcrest-parent</artifactId>
  <version>1.1</version>
  <packaging>pom</packaging>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>junit</groupId>
        <artifactId>junit</artifactId>
        <version>4.0</version>
        <scope>provided</scope>
      </dependency>
      <dependency>
        <groupId>jmock</groupId>
        <artifactId>jmock</artifactId>
        <version>1.1.0</version>
        <scope>provided</scope>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
    </dependency>
    <dependency>
      <groupId>jmock</groupId>
      <artifactId>jmock</artifactId>
    </dependency>
  </dependencies>
</project>"#,
        );
        write_fake_artifact_with_pom(
            dir.path(),
            "org.hamcrest",
            "hamcrest-core",
            "1.1",
            r#"<?xml version="1.0"?>
<project>
  <parent>
    <groupId>org.hamcrest</groupId>
    <artifactId>hamcrest-parent</artifactId>
    <version>1.1</version>
  </parent>
  <artifactId>hamcrest-core</artifactId>
</project>"#,
        );

        let result =
            run_resolve(dir.path(), &[("org.hamcrest:hamcrest-core", "1.1")], vec![]).unwrap();
        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"org.hamcrest:hamcrest-core:1.1".to_string()),
            "leaf must resolve: {gavs:?}"
        );
        assert!(
            !gavs.iter().any(|g| g.starts_with("junit:junit:")),
            "managed-provided junit must not be a compile transitive: {gavs:?}"
        );
        assert!(
            !gavs.iter().any(|g| g.starts_with("jmock:jmock:")),
            "managed-provided jmock must not be a compile transitive: {gavs:?}"
        );
    }

    #[test]
    fn parent_pom_dependencies_are_inherited() {
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "org.yaml", "snakeyaml", "2.0", &[]);
        write_fake_artifact(dir.path(), "junit", "junit", "4.13.2", &[]);
        write_fake_pom(
            dir.path(),
            "com.example",
            "parent-pom",
            "1.0",
            &make_pom(
                "com.example",
                "parent-pom",
                "1.0",
                &[("org.yaml", "snakeyaml", "2.0")],
            ),
        );
        write_fake_artifact_with_pom(
            dir.path(),
            "com.example",
            "child",
            "1.0",
            &make_pom_with_parent(
                "com.example",
                "child",
                "1.0",
                ("com.example", "parent-pom", "1.0"),
                &[("junit", "junit", "4.13.2")],
            ),
        );

        let result = run_resolve(dir.path(), &[("com.example:child", "1.0")], vec![]).unwrap();
        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"org.yaml:snakeyaml:2.0".to_string()),
            "parent <dependencies> must be inherited: {gavs:?}"
        );
        assert!(
            gavs.contains(&"junit:junit:4.13.2".to_string()),
            "child's own dependency must remain: {gavs:?}"
        );
    }

    #[test]
    fn child_dependency_overrides_inherited_parent_dependency() {
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "org.yaml", "snakeyaml", "2.0", &[]);
        write_fake_artifact(dir.path(), "org.yaml", "snakeyaml", "2.2", &[]);
        write_fake_pom(
            dir.path(),
            "com.example",
            "parent-pom",
            "1.0",
            &make_pom(
                "com.example",
                "parent-pom",
                "1.0",
                &[("org.yaml", "snakeyaml", "2.0")],
            ),
        );
        write_fake_artifact_with_pom(
            dir.path(),
            "com.example",
            "child",
            "1.0",
            &make_pom_with_parent(
                "com.example",
                "child",
                "1.0",
                ("com.example", "parent-pom", "1.0"),
                &[("org.yaml", "snakeyaml", "2.2")],
            ),
        );

        let result = run_resolve(dir.path(), &[("com.example:child", "1.0")], vec![]).unwrap();
        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"org.yaml:snakeyaml:2.2".to_string()),
            "child redeclaration must win: {gavs:?}"
        );
        assert!(
            !gavs.contains(&"org.yaml:snakeyaml:2.0".to_string()),
            "parent version must not leak after override: {gavs:?}"
        );
    }

    #[test]
    fn parent_pom_fetch_failure_is_an_error() {
        // my-app declares a <parent> that is NOT present in the cache (offline).
        // Previously merge_parent_chain swallowed the failure and resolution
        // "succeeded" with an incomplete classpath; now it is a hard error
        // naming the missing parent (bug #15).
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact_with_pom(
            dir.path(),
            "com.example",
            "my-app",
            "1.0",
            &make_pom_with_parent(
                "com.example",
                "my-app",
                "1.0",
                ("com.example", "parent-pom", "1.0"), // never written to the cache
                &[],
            ),
        );

        let err = run_resolve(dir.path(), &[("com.example:my-app", "1.0")], vec![]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parent POM") && msg.contains("com.example:parent-pom"),
            "error should name the missing parent: {msg}",
        );
    }

    #[test]
    fn missing_transitive_pom_is_an_error() {
        // grp:lib:1.0 depends on foo:bar:1.0, but bar's POM is absent from the
        // cache.  The BFS must surface the fetch failure rather than silently
        // dropping bar's subtree.
        let dir = tempfile::tempdir().unwrap();
        // Writes lib's POM (listing foo:bar:1.0) + lib's JAR, but NOT bar.
        write_fake_artifact(dir.path(), "grp", "lib", "1.0", &[("foo", "bar", "1.0")]);

        let err = run_resolve(dir.path(), &[("grp:lib", "1.0")], vec![]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foo:bar"),
            "error should name the unfetchable transitive POM: {msg}",
        );
    }

    // -----------------------------------------------------------------------
    // pom-packaged (aggregator) dependency expansion (bug #11)
    // -----------------------------------------------------------------------

    #[test]
    fn transitive_pom_aggregator_is_expanded_without_jar() {
        // grp:app (jar) -> agg:bundle (pom-packaged aggregator) -> foo:bar (jar).
        // The aggregator has no JAR; previously its .jar 404'd and failed the
        // build. It must be expanded for foo:bar but contribute no classpath entry.
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_pom(
            dir.path(),
            "agg",
            "bundle",
            "1.0",
            &make_aggregator_pom("agg", "bundle", "1.0", &[("foo", "bar", "1.0")]),
        );
        write_fake_artifact(dir.path(), "grp", "app", "1.0", &[("agg", "bundle", "1.0")]);

        let gavs = jar_gavs(&run_resolve(dir.path(), &[("grp:app", "1.0")], vec![]).unwrap());
        assert!(
            gavs.contains(&"grp:app:1.0".to_string()),
            "app jar present: {gavs:?}"
        );
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "aggregator's dep present: {gavs:?}"
        );
        assert!(
            !gavs.iter().any(|g| g.starts_with("agg:bundle")),
            "pom-packaged aggregator must not be on the classpath: {gavs:?}",
        );
    }

    #[test]
    fn declared_pom_aggregator_is_expanded_without_jar() {
        // Same aggregator declared DIRECTLY in Curie.toml: it is expanded for its
        // dependencies and contributes no jar (no 404, no rejection).
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_pom(
            dir.path(),
            "agg",
            "bundle",
            "1.0",
            &make_aggregator_pom("agg", "bundle", "1.0", &[("foo", "bar", "1.0")]),
        );

        let gavs = jar_gavs(&run_resolve(dir.path(), &[("agg:bundle", "1.0")], vec![]).unwrap());
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "aggregator's dep present: {gavs:?}"
        );
        assert!(
            !gavs.iter().any(|g| g.starts_with("agg:bundle")),
            "directly-declared pom aggregator must not be on the classpath: {gavs:?}",
        );
    }

    // -----------------------------------------------------------------------
    // Exclusion tests
    // -----------------------------------------------------------------------

    #[test]
    fn pom_exclusion_prevents_transitive_dep() {
        // lib 1.0 depends on foo:bar 1.0, but declares <exclusions>
        // on it.  The POM for lib should exclude bar from the classpath.
        let dir = tempfile::tempdir().unwrap();

        // foo:bar 1.0 depends on foo:baz 1.0 (to test deep exclusion).
        write_fake_artifact(dir.path(), "foo", "baz", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[("foo", "baz", "1.0")]);

        // grp:lib 1.0 depends on foo:bar 1.0 with an exclusion on foo:baz.
        // Build the POM manually to include <exclusions>.
        let lib_gav = Gav {
            group: "grp".into(),
            artifact: "lib".into(),
            version: "1.0".into(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let m2 = dir.path().join(".m2").join("repository");
        let pom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>grp</groupId><artifactId>lib</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>foo</groupId>
      <artifactId>bar</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>foo</groupId>
          <artifactId>baz</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;
        write_with_sidecar(&m2.join(lib_gav.relative_pom_path()), pom_xml.as_bytes());
        write_with_sidecar(&m2.join(lib_gav.relative_path()), b"");

        let result = run_resolve(dir.path(), &[("grp:lib", "1.0")], vec![]).unwrap();

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"grp:lib:1.0".to_string()),
            "lib must be present: {:?}",
            gavs
        );
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "bar must be present: {:?}",
            gavs
        );
        assert!(
            !gavs.contains(&"foo:baz:1.0".to_string()),
            "baz must be excluded by POM exclusion: {:?}",
            gavs,
        );
    }

    #[test]
    fn user_exclusion_from_dep_entry_prevents_transitive_dep() {
        // Same as the POM test above, but the exclusion comes from the
        // DepEntry (simulating Curie.toml exclusions = ["foo:baz"]).
        let dir = tempfile::tempdir().unwrap();

        write_fake_artifact(dir.path(), "foo", "baz", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[("foo", "baz", "1.0")]);

        // Use the run_resolve helper but with exclusions on the dep entry.
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let entries = [DepEntry {
            key: "foo:bar",
            version: "1.0",
            repo_id: None,
            exclusions: vec!["foo:baz"],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve(&entries, &opts).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"foo:bar:1.0".to_string()),
            "bar must be present: {:?}",
            gavs
        );
        assert!(
            !gavs.contains(&"foo:baz:1.0".to_string()),
            "baz must be excluded by user exclusion: {:?}",
            gavs,
        );
    }

    #[test]
    fn wildcard_exclusion_excludes_all_transitives() {
        // lib 1.0 depends on foo:a 1.0 and foo:b 1.0.
        // User declares exclusions = ["*:*"] → neither a nor b should appear.
        let dir = tempfile::tempdir().unwrap();

        write_fake_artifact(dir.path(), "foo", "a", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "b", "1.0", &[]);
        write_fake_artifact(
            dir.path(),
            "grp",
            "lib",
            "1.0",
            &[("foo", "a", "1.0"), ("foo", "b", "1.0")],
        );

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let entries = [DepEntry {
            key: "grp:lib",
            version: "1.0",
            repo_id: None,
            exclusions: vec!["*:*"],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve(&entries, &opts).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let gavs = jar_gavs(&result);
        assert!(
            gavs.contains(&"grp:lib:1.0".to_string()),
            "lib itself must be present: {:?}",
            gavs
        );
        assert_eq!(
            gavs.len(),
            1,
            "only lib should be in result — all transitives excluded: {:?}",
            gavs
        );
    }

    #[test]
    fn exclusion_propagates_transitively() {
        // grp:top → grp:mid → foo:leaf
        // User excludes foo:leaf on grp:top → leaf must not appear even though
        // the exclusion is two levels up.
        let dir = tempfile::tempdir().unwrap();

        write_fake_artifact(dir.path(), "foo", "leaf", "1.0", &[]);
        write_fake_artifact(dir.path(), "grp", "mid", "1.0", &[("foo", "leaf", "1.0")]);
        write_fake_artifact(dir.path(), "grp", "top", "1.0", &[("grp", "mid", "1.0")]);

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let entries = [DepEntry {
            key: "grp:top",
            version: "1.0",
            repo_id: None,
            exclusions: vec!["foo:leaf"],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve(&entries, &opts).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let gavs = jar_gavs(&result);
        assert!(gavs.contains(&"grp:top:1.0".to_string()));
        assert!(gavs.contains(&"grp:mid:1.0".to_string()));
        assert!(
            !gavs.contains(&"foo:leaf:1.0".to_string()),
            "leaf must be excluded transitively: {:?}",
            gavs,
        );
    }

    #[test]
    fn is_excluded_empty_set_returns_false() {
        let empty = HashSet::new();
        assert!(!is_excluded("org.example", "foo", &empty));
    }

    #[test]
    fn is_excluded_exact_match() {
        let mut set = HashSet::new();
        set.insert(("org.example".to_string(), "foo".to_string()));
        assert!(is_excluded("org.example", "foo", &set));
        assert!(!is_excluded("org.example", "bar", &set));
        assert!(!is_excluded("org.other", "foo", &set));
    }

    #[test]
    fn is_excluded_wildcard_artifact() {
        let mut set = HashSet::new();
        set.insert(("org.example".to_string(), "*".to_string()));
        assert!(is_excluded("org.example", "anything", &set));
        assert!(!is_excluded("org.other", "anything", &set));
    }

    #[test]
    fn is_excluded_wildcard_both() {
        let mut set = HashSet::new();
        set.insert(("*".to_string(), "*".to_string()));
        assert!(is_excluded("anything", "anything", &set));
    }

    #[test]
    fn parse_exclusion_strings_basic() {
        let result = parse_exclusion_strings(&["org.example:foo", "com.test:bar"]);
        assert!(result.contains(&("org.example".to_string(), "foo".to_string())));
        assert!(result.contains(&("com.test".to_string(), "bar".to_string())));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_exclusion_strings_ignores_invalid() {
        let result = parse_exclusion_strings(&["no-colon-here"]);
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Checksum verification tests
    // -----------------------------------------------------------------------

    /// SHA-256 of the empty byte string — used by `write_with_sidecar` when
    /// the JAR placeholder content is `b""`.  Pinned here as a sanity check
    /// on `DigestKind::hash_hex`.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    // SHA-256("abc")
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

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
            "expected SHA-256 mismatch error, got {:?}",
            err,
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
        assert!(!verify_with_local_sidecar(&jar).unwrap());
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
        assert!(verify_with_local_sidecar(&jar).unwrap());
    }

    #[test]
    fn verify_with_local_sidecar_falls_back_to_sha1() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"").unwrap();
        // No .sha256, only .sha1.
        std::fs::write(sidecar_path(&jar, DigestKind::Sha1), EMPTY_SHA1.as_bytes()).unwrap();
        assert!(verify_with_local_sidecar(&jar).unwrap());
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
            "expected checksum failure, got {:?}",
            err,
        );
    }

    #[test]
    fn verify_with_local_sidecar_errors_on_malformed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("foo.jar");
        std::fs::write(&jar, b"").unwrap();
        std::fs::write(sidecar_path(&jar, DigestKind::Sha256), b"not-a-hex-digest").unwrap();
        assert!(verify_with_local_sidecar(&jar).is_err());
    }

    #[test]
    fn fetch_artifact_offline_returns_cached_jar() {
        let dir = tempfile::tempdir().unwrap();
        let gav = write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let result = fetch_artifact(&gav, &[], true);
        let expected = gav.local_repository_path().unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn fetch_artifact_offline_errors_when_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let gav = Gav::from_key_version("foo:bar", "1.0").unwrap();

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let result = fetch_artifact(&gav, &[], true);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(result.is_err());
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

        let err = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![]).unwrap_err();
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("checksum"),
            "expected checksum error in chain, got {:?}",
            chain,
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

        let err = run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![]).unwrap_err();
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("sidecar"),
            "expected missing-sidecar error in chain, got {:?}",
            chain,
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
            .map(|(k, v, r)| DepEntry {
                key: k,
                version: v,
                repo_id: *r,
                exclusions: vec![],
                classifier: None,
                allow_version_conflict: false,
            })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos,
            progress: false,
            bom_imports: vec![],
            offline: true,
            // These helpers exercise the user-dependency path, so conflict
            // errors are enabled (matching compile.rs / test.rs).
            skip_version_ranges: false,
            error_on_version_conflict: true,
            snapshot_pins: Default::default(),
            update_snapshots: false,
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
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let entries = [DepEntry {
            key: "foo:bar",
            version: "1.0",
            repo_id: None,
            exclusions: vec![],
            classifier: None,
            allow_version_conflict: false,
        }];
        let result = resolve(&entries, &opts).unwrap();
        assert_eq!(
            result.len(),
            1,
            "should find cached artifact regardless of mirror URL"
        );

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
            "expected unknown-repo error, got {:?}",
            msg,
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
        let result =
            run_resolve_with_repo(dir.path(), &[("foo:bar", "1.0", None)], vec![]).unwrap();
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

    // -----------------------------------------------------------------------
    // resolve_tree tests
    // -----------------------------------------------------------------------

    fn run_resolve_tree(
        home_dir: &std::path::Path,
        deps: &[(&str, &str)],
        bom_imports: Vec<Gav>,
    ) -> Result<DepTree> {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home_dir.to_str().unwrap());

        let entries: Vec<DepEntry> = deps
            .iter()
            .map(|(k, v)| DepEntry {
                key: k,
                version: v,
                repo_id: None,
                exclusions: vec![],
                classifier: None,
                allow_version_conflict: false,
            })
            .collect();
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports,
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve_tree(&entries, &opts);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    #[test]
    fn tree_is_empty_for_no_deps() {
        let dir = tempfile::tempdir().unwrap();
        let tree = run_resolve_tree(dir.path(), &[], vec![]).unwrap();
        assert!(tree.resolved.is_empty());
        assert!(tree.skipped.is_empty());
    }

    #[test]
    fn tree_records_depth_correctly() {
        let dir = tempfile::tempdir().unwrap();
        // foo:bar:1.0 → foo:baz:1.0 → foo:qux:1.0
        write_fake_artifact(dir.path(), "foo", "qux", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "baz", "1.0", &[("foo", "qux", "1.0")]);
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[("foo", "baz", "1.0")]);

        let tree = run_resolve_tree(dir.path(), &[("foo:bar", "1.0")], vec![]).unwrap();
        assert_eq!(tree.resolved.len(), 3);

        let bar = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "bar")
            .unwrap();
        let baz = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "baz")
            .unwrap();
        let qux = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "qux")
            .unwrap();

        assert_eq!(bar.depth, 0);
        assert_eq!(baz.depth, 1);
        assert_eq!(qux.depth, 2);
    }

    #[test]
    fn tree_records_via_correctly() {
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "child", "1.0", &[]);
        write_fake_artifact(
            dir.path(),
            "foo",
            "parent",
            "1.0",
            &[("foo", "child", "1.0")],
        );

        let tree = run_resolve_tree(dir.path(), &[("foo:parent", "1.0")], vec![]).unwrap();

        let parent_dep = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "parent")
            .unwrap();
        let child_dep = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "child")
            .unwrap();

        assert!(parent_dep.via.is_none(), "depth-0 dep has no via");
        let via = child_dep.via.as_ref().expect("child must have a via");
        assert_eq!(via.artifact, "parent");
        assert_eq!(via.version, "1.0");
    }

    #[test]
    fn tree_records_skipped_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        // Direct dep A pulls child:1.0 at depth 1.
        // Direct dep B → C pulls child:2.0 at depth 2 — should be skipped.
        write_fake_artifact(dir.path(), "foo", "child", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "child", "2.0", &[]);
        write_fake_artifact(dir.path(), "foo", "c", "1.0", &[("foo", "child", "2.0")]);
        write_fake_artifact(dir.path(), "foo", "a", "1.0", &[("foo", "child", "1.0")]);
        write_fake_artifact(dir.path(), "foo", "b", "1.0", &[("foo", "c", "1.0")]);

        let tree =
            run_resolve_tree(dir.path(), &[("foo:a", "1.0"), ("foo:b", "1.0")], vec![]).unwrap();

        // child:1.0 was chosen (depth 1 via a).
        let child = tree
            .resolved
            .iter()
            .find(|d| d.gav.artifact == "child")
            .unwrap();
        assert_eq!(child.gav.version, "1.0");

        // child:2.0 (depth 2, via c) should appear in skipped.
        let skips = tree
            .skipped
            .get("foo:child")
            .expect("foo:child should have skipped entries");
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].version, "2.0");
        assert_eq!(skips[0].depth, 2);
        let skip_via = skips[0].via.as_ref().unwrap();
        assert_eq!(skip_via.artifact, "c");
    }

    // -----------------------------------------------------------------------
    // Version range detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn version_range_in_transitive_pom_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // parent:1.0 declares a transitive dep on child with a range version.
        write_fake_artifact(
            dir.path(),
            "foo",
            "parent",
            "1.0",
            &[("foo", "child", "[1.0,2.0)")],
        );

        let err = run_resolve(dir.path(), &[("foo:parent", "1.0")], vec![]).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("[1.0,2.0)"),
            "expected range in error, got {:?}",
            msg,
        );
        assert!(
            msg.contains("foo:parent:1.0"),
            "expected declaring POM in error, got {:?}",
            msg,
        );
        assert!(
            msg.contains("foo:child"),
            "expected the ranged artifact in error, got {:?}",
            msg,
        );
    }

    #[test]
    fn version_range_in_direct_dep_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // No files needed — error must fire before any artifact is fetched.
        let err = run_resolve(dir.path(), &[("foo:bar", "[1.0,)")], vec![]).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("[1.0,)"),
            "expected range in error, got {:?}",
            msg,
        );
        assert!(
            msg.contains("Curie.toml"),
            "expected 'Curie.toml' as the declared-in location, got {:?}",
            msg,
        );
    }

    #[test]
    fn multiple_ranges_grouped_in_one_error() {
        let dir = tempfile::tempdir().unwrap();
        // lib:1.0 declares two transitive deps, both using ranges.
        write_fake_artifact(
            dir.path(),
            "grp",
            "lib",
            "1.0",
            &[("foo", "alpha", "[1.0,2.0)"), ("foo", "beta", "(,1.5]")],
        );

        let err = run_resolve(dir.path(), &[("grp:lib", "1.0")], vec![]).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("[1.0,2.0)"),
            "expected [1.0,2.0) in error: {:?}",
            msg
        );
        assert!(
            msg.contains("(,1.5]"),
            "expected (,1.5] in error: {:?}",
            msg
        );
        assert!(
            msg.contains("foo:alpha"),
            "expected foo:alpha in error: {:?}",
            msg
        );
        assert!(
            msg.contains("foo:beta"),
            "expected foo:beta in error: {:?}",
            msg
        );
        // Both should appear in the pin block.
        assert!(
            msg.contains("Pin these artifacts"),
            "expected pin hint in error: {:?}",
            msg,
        );
    }

    #[test]
    fn exact_versions_are_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(dir.path(), "foo", "bar", "1.0", &[]);
        write_fake_artifact(dir.path(), "foo", "bar", "2.17.2", &[]);
        write_fake_artifact(dir.path(), "grp", "lib", "1.0", &[("foo", "bar", "2.17.2")]);

        // Direct dep with exact version.
        run_resolve(dir.path(), &[("foo:bar", "1.0")], vec![]).unwrap();
        // Transitive dep with exact version.
        run_resolve(dir.path(), &[("grp:lib", "1.0")], vec![]).unwrap();
    }

    #[test]
    fn single_pin_range_notation_is_rejected() {
        // [1.0] is still range syntax even though it pins a single version.
        // Users should write "1.0" instead.
        let dir = tempfile::tempdir().unwrap();
        let err = run_resolve(dir.path(), &[("foo:bar", "[1.0]")], vec![]).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("[1.0]"),
            "expected [1.0] range notation in error, got {:?}",
            msg,
        );
    }

    #[test]
    fn range_error_is_downcastable_to_version_range_error() {
        let dir = tempfile::tempdir().unwrap();
        write_fake_artifact(
            dir.path(),
            "foo",
            "parent",
            "1.0",
            &[("foo", "child", "[1.0,2.0)")],
        );

        let err = run_resolve(dir.path(), &[("foo:parent", "1.0")], vec![]).unwrap_err();
        let range_err = err
            .downcast_ref::<VersionRangeError>()
            .expect("error should downcast to VersionRangeError");
        assert_eq!(range_err.violations.len(), 1);
        assert_eq!(range_err.violations[0].dep_key, "foo:child");
        assert_eq!(range_err.violations[0].range, "[1.0,2.0)");
        assert_eq!(
            range_err.violations[0].declared_in.notation(),
            "foo:parent:1.0"
        );
    }

    #[test]
    fn pinning_a_transitive_range_suppresses_the_error() {
        let dir = tempfile::tempdir().unwrap();
        // parent:1.0 declares child via a range; child:1.5 is available.
        write_fake_artifact(
            dir.path(),
            "foo",
            "parent",
            "1.0",
            &[("foo", "child", "[1.0,2.0)")],
        );
        write_fake_artifact(dir.path(), "foo", "child", "1.5", &[]);

        // Without the pin the transitive range is a hard error.
        assert!(run_resolve(dir.path(), &[("foo:parent", "1.0")], vec![]).is_err());

        // With foo:child pinned (and supplied as a sibling root at a concrete
        // version), the transitive range is skipped and resolution succeeds.
        let jars = run_resolve_with_pins(
            dir.path(),
            &[("foo:parent", "1.0"), ("foo:child", "1.5")],
            &["foo:child"],
        )
        .unwrap();
        assert!(
            jars.iter().any(|p| p.to_string_lossy().contains("child")),
            "expected child JAR to be fetched, got {:?}",
            jars,
        );
    }

    #[test]
    fn parse_metadata_versions_extracts_versions_in_order() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <metadata>
              <groupId>com.google.code.gson</groupId>
              <artifactId>gson</artifactId>
              <version>2.8.0</version>
              <versioning>
                <latest>2.11.0</latest>
                <release>2.11.0</release>
                <versions>
                  <version>2.8.0</version>
                  <version>2.9.1</version>
                  <version>2.10.1</version>
                  <version>2.11.0</version>
                </versions>
              </versioning>
            </metadata>"#;
        let versions = parse_metadata_versions(xml).unwrap();
        assert_eq!(versions, vec!["2.8.0", "2.9.1", "2.10.1", "2.11.0"]);
    }

    #[test]
    fn resolve_respects_classifier_on_declared_dep_entry() {
        // Declared dep with classifier should fetch the -classifier.jar
        // using the updated relative_path logic. Uses a pre-written classified
        // artifact (no network).
        let dir = tempfile::tempdir().unwrap();
        let _gav = write_fake_classified_artifact(
            dir.path(),
            "org.jacoco",
            "org.jacoco.agent",
            "0.8.13",
            "runtime",
            &[], // no further deps for the fake
        );

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().to_str().unwrap());

        let entries = [DepEntry {
            key: "org.jacoco:org.jacoco.agent",
            version: "0.8.13",
            repo_id: None,
            exclusions: vec![],
            classifier: Some("runtime"),
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![],
            progress: false,
            bom_imports: vec![],
            offline: true,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: Default::default(),
            update_snapshots: false,
        };
        let result = resolve(&entries, &opts).unwrap();

        if let Some(h) = prev_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        assert_eq!(result.len(), 1, "expected exactly the classified jar");
        let path = &result[0];
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("runtime"),
            "expected runtime classifier in filename, got {}",
            name
        );
        assert!(path.exists());
    }

    // --- regression: property placeholders in BOM-import coordinates ---------

    /// Write a raw POM XML (with a `.sha256` sidecar) at the cache location for
    /// `gav` under `home_dir`.  Unlike `write_fake_bom` this lets a test author
    /// arbitrary content such as a `<properties>` block.
    fn write_fake_pom_xml(home_dir: &std::path::Path, gav: &Gav, xml: &str) {
        let rel = gav.relative_pom_path();
        let path = home_dir.join(".m2").join("repository").join(&rel);
        write_with_sidecar(&path, xml.as_bytes());
    }

    /// Recursively check whether any directory under `root` has a name that
    /// begins with `${` — the symptom of an unresolved Maven property reaching
    /// the filesystem.
    fn has_unresolved_placeholder_dir(root: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("${") {
                return true;
            }
            if has_unresolved_placeholder_dir(&path) {
                return true;
            }
        }
        false
    }

    #[test]
    fn bom_import_with_property_groupid_is_resolved() {
        let dir = tempfile::tempdir().unwrap();

        // Importing BOM defines properties and imports another BOM using them —
        // mirrors Shibboleth's oidc-common-parent importing idp-bom.
        let importer = Gav {
            group: "net.shibboleth.oidc".to_string(),
            artifact: "oidc-common-parent".to_string(),
            version: "3.3.0".to_string(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let importer_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>net.shibboleth.oidc</groupId>
  <artifactId>oidc-common-parent</artifactId>
  <version>3.3.0</version>
  <properties>
    <idp.groupId>net.shibboleth.idp</idp.groupId>
    <idp.version>5.0.0</idp.version>
  </properties>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>${idp.groupId}</groupId>
        <artifactId>idp-bom</artifactId>
        <version>${idp.version}</version>
        <type>pom</type>
        <scope>import</scope>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;
        write_fake_pom_xml(dir.path(), &importer, importer_xml);

        // The real target BOM at the *resolved* coordinate.
        write_fake_bom(
            dir.path(),
            "net.shibboleth.idp",
            "idp-bom",
            "5.0.0",
            &[("net.shibboleth.idp", "idp-core", "5.0.0")],
            &[],
        );

        let result = run_resolve_boms(dir.path(), &[importer]).unwrap();

        // The managed version from idp-bom is resolved through the property.
        assert_eq!(
            result
                .get("net.shibboleth.idp:idp-core")
                .map(String::as_str),
            Some("5.0.0"),
        );
        // And no junk `${...}` directory was ever created in the cache.
        let repo = dir.path().join(".m2").join("repository");
        assert!(
            !has_unresolved_placeholder_dir(&repo),
            "an unresolved `${{...}}` directory was created under {}",
            repo.display(),
        );
    }

    #[test]
    fn managed_version_keys_with_project_groupid_are_resolved() {
        let dir = tempfile::tempdir().unwrap();

        // A BOM whose managed deps use ${project.groupId} (like idp-bom itself).
        let bom = Gav {
            group: "net.shibboleth.idp".to_string(),
            artifact: "idp-bom".to_string(),
            version: "5.0.0".to_string(),
            classifier: None,
            extension: None,
            snapshot_version: None,
        };
        let bom_xml = r#"<?xml version="1.0"?>
<project>
  <groupId>net.shibboleth.idp</groupId>
  <artifactId>idp-bom</artifactId>
  <version>5.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>${project.groupId}</groupId>
        <artifactId>idp-core</artifactId>
        <version>${project.version}</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;
        write_fake_pom_xml(dir.path(), &bom, bom_xml);

        let result = run_resolve_boms(dir.path(), &[bom]).unwrap();

        // Key must be resolved to the concrete group:artifact, not left literal.
        assert_eq!(
            result
                .get("net.shibboleth.idp:idp-core")
                .map(String::as_str),
            Some("5.0.0"),
        );
        assert!(
            !result.keys().any(|k| k.contains("${")),
            "managed-version key left an unresolved placeholder: {:?}",
            result.keys().collect::<Vec<_>>(),
        );
    }

    // -----------------------------------------------------------------------
    // Concurrent staging / bug #2 regression tests
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_staging_is_safe_and_tolerant() {
        // Two threads stage the exact same destination concurrently using the
        // new helpers. Both must succeed, the final file must be one of the
        // two clean payloads (never a mix), and a sidecar must be present.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("example-1.0.jar");

        let payload_a = b"payload from thread A";
        let side_a = b"sideA";
        let payload_b = b"payload from thread B -- longer to detect corruption";
        let side_b = b"sideB";

        let results: Vec<Result<()>> = std::thread::scope(|s| {
            let h1 = s.spawn(|| {
                let sidecar = sidecar_path(&dest, DigestKind::Sha256);
                stage_and_rename_atomically(&dest, payload_a, &sidecar, side_a)
            });
            let h2 = s.spawn(|| {
                let sidecar = sidecar_path(&dest, DigestKind::Sha256);
                stage_and_rename_atomically(&dest, payload_b, &sidecar, side_b)
            });
            vec![h1.join().unwrap(), h2.join().unwrap()]
        });

        assert!(results[0].is_ok(), "thread1 failed: {:?}", results[0]);
        assert!(results[1].is_ok(), "thread2 failed: {:?}", results[1]);

        let final_bytes = std::fs::read(&dest).unwrap();
        let ok = final_bytes == payload_a || final_bytes == payload_b;
        assert!(ok, "final file was corrupted or mixed: {:?}", final_bytes);

        let sidecar = sidecar_path(&dest, DigestKind::Sha256);
        assert!(
            sidecar.exists(),
            "sidecar should exist after concurrent staging"
        );
    }

    #[test]
    fn unique_staging_names_distinguish_pom_from_jar() {
        // compute_unique_staging_path must never produce the same temp name
        // for a .pom and a .jar of the same GAV (the original with_extension
        // bug).
        let pom_dest = Path::new("/tmp/cache/com/example/foo/1.0/foo-1.0.pom");
        let jar_dest = Path::new("/tmp/cache/com/example/foo/1.0/foo-1.0.jar");

        let p1 = compute_unique_staging_path(pom_dest);
        let p2 = compute_unique_staging_path(jar_dest);

        assert_ne!(p1, p2, "POM and JAR must get distinct staging paths");
        let p1s = p1.to_string_lossy();
        let p2s = p2.to_string_lossy();
        assert!(p1s.contains("foo-1.0.pom.part."), "pom staging: {}", p1s);
        assert!(p2s.contains("foo-1.0.jar.part."), "jar staging: {}", p2s);
    }

    // -----------------------------------------------------------------------
    // SNAPSHOT resolution + pins
    // -----------------------------------------------------------------------

    /// Write a unique-snapshot artifact (timestamped JAR/POM + sidecars) and
    /// version-level `maven-metadata.xml` into a file:// repository root.
    fn write_unique_snapshot_repo(
        repo_root: &std::path::Path,
        group: &str,
        artifact: &str,
        base_version: &str,
        unique_version: &str,
        jar_bytes: &[u8],
    ) {
        let group_path = group.replace('.', "/");
        let dir = repo_root
            .join(&group_path)
            .join(artifact)
            .join(base_version);
        std::fs::create_dir_all(&dir).unwrap();

        let pom_xml = make_pom(group, artifact, base_version, &[]);
        let pom_name = format!("{artifact}-{unique_version}.pom");
        let jar_name = format!("{artifact}-{unique_version}.jar");
        write_with_sidecar(&dir.join(&pom_name), pom_xml.as_bytes());
        write_with_sidecar(&dir.join(&jar_name), jar_bytes);

        let meta = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>{group}</groupId>
  <artifactId>{artifact}</artifactId>
  <version>{base_version}</version>
  <versioning>
    <snapshot>
      <timestamp>{ts}</timestamp>
      <buildNumber>{bn}</buildNumber>
    </snapshot>
    <snapshotVersions>
      <snapshotVersion>
        <extension>jar</extension>
        <value>{unique_version}</value>
      </snapshotVersion>
      <snapshotVersion>
        <extension>pom</extension>
        <value>{unique_version}</value>
      </snapshotVersion>
    </snapshotVersions>
  </versioning>
</metadata>
"#,
            // unique = baseWithoutSNAPSHOT-timestamp-buildNumber
            ts = unique_version
                .rsplit_once('-')
                .and_then(|(left, _bn)| left.rsplit_once('-').map(|(_, ts)| ts))
                .unwrap_or("20260101.000000"),
            bn = unique_version
                .rsplit_once('-')
                .map(|(_, bn)| bn)
                .unwrap_or("1"),
        );
        std::fs::write(dir.join("maven-metadata.xml"), meta).unwrap();
    }

    #[test]
    fn unique_snapshot_resolves_via_metadata_and_returns_pin() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        write_unique_snapshot_repo(
            &repo,
            "com.example",
            "snap-lib",
            "1.0-SNAPSHOT",
            "1.0-20260115.120000-1",
            b"snap-jar-v1",
        );

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().join("home").to_str().unwrap());

        let entries = [DepEntry {
            key: "com.example:snap-lib",
            version: "1.0-SNAPSHOT",
            repo_id: Some("local"),
            exclusions: vec![],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![Repository {
                id: "local".into(),
                name: "Local".into(),
                url: format!("file://{}", repo.display()),
            }],
            progress: false,
            bom_imports: vec![],
            offline: false,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: HashMap::new(),
            update_snapshots: false,
        };
        let result = resolve_full(&entries, &opts, &[]).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(result.jars.len(), 1);
        let jar = result.jars[0].to_string_lossy();
        assert!(
            jar.contains("snap-lib-1.0-20260115.120000-1.jar"),
            "expected unique snapshot JAR path, got {jar}"
        );
        assert_eq!(
            result
                .snapshot_pins
                .get("com.example:snap-lib:1.0-SNAPSHOT")
                .map(String::as_str),
            Some("1.0-20260115.120000-1")
        );
        // Bytes actually came from the unique file.
        assert_eq!(std::fs::read(&result.jars[0]).unwrap(), b"snap-jar-v1");
    }

    #[test]
    fn snapshot_pin_selects_locked_unique_version() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        // Publish *two* unique builds; the pin must select the older one.
        write_unique_snapshot_repo(
            &repo,
            "com.example",
            "snap-lib",
            "1.0-SNAPSHOT",
            "1.0-20260115.120000-1",
            b"snap-v1",
        );
        // Overwrite metadata to point at build 2, and add the v2 artifacts.
        write_unique_snapshot_repo(
            &repo,
            "com.example",
            "snap-lib",
            "1.0-SNAPSHOT",
            "1.0-20260116.120000-2",
            b"snap-v2",
        );

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().join("home").to_str().unwrap());

        let mut pins = HashMap::new();
        pins.insert(
            "com.example:snap-lib:1.0-SNAPSHOT".into(),
            "1.0-20260115.120000-1".into(),
        );

        let entries = [DepEntry {
            key: "com.example:snap-lib",
            version: "1.0-SNAPSHOT",
            repo_id: Some("local"),
            exclusions: vec![],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![Repository {
                id: "local".into(),
                name: "Local".into(),
                url: format!("file://{}", repo.display()),
            }],
            progress: false,
            bom_imports: vec![],
            offline: false,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: pins,
            update_snapshots: false,
        };
        let result = resolve_full(&entries, &opts, &[]).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(std::fs::read(&result.jars[0]).unwrap(), b"snap-v1");
        assert!(result.jars[0]
            .to_string_lossy()
            .contains("1.0-20260115.120000-1"));
    }

    #[test]
    fn update_snapshots_ignores_pin_and_takes_latest() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        write_unique_snapshot_repo(
            &repo,
            "com.example",
            "snap-lib",
            "1.0-SNAPSHOT",
            "1.0-20260115.120000-1",
            b"snap-v1",
        );
        write_unique_snapshot_repo(
            &repo,
            "com.example",
            "snap-lib",
            "1.0-SNAPSHOT",
            "1.0-20260116.120000-2",
            b"snap-v2",
        );

        let _guard = HOME_LOCK.lock().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path().join("home").to_str().unwrap());

        let mut pins = HashMap::new();
        pins.insert(
            "com.example:snap-lib:1.0-SNAPSHOT".into(),
            "1.0-20260115.120000-1".into(),
        );

        let entries = [DepEntry {
            key: "com.example:snap-lib",
            version: "1.0-SNAPSHOT",
            repo_id: Some("local"),
            exclusions: vec![],
            classifier: None,
            allow_version_conflict: false,
        }];
        let opts = ResolveOptions {
            default_repos: vec![],
            named_repos: vec![Repository {
                id: "local".into(),
                name: "Local".into(),
                url: format!("file://{}", repo.display()),
            }],
            progress: false,
            bom_imports: vec![],
            offline: false,
            skip_version_ranges: false,
            error_on_version_conflict: false,
            snapshot_pins: pins,
            update_snapshots: true, // -U
        };
        let result = resolve_full(&entries, &opts, &[]).unwrap();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(std::fs::read(&result.jars[0]).unwrap(), b"snap-v2");
        assert_eq!(
            result
                .snapshot_pins
                .get("com.example:snap-lib:1.0-SNAPSHOT")
                .map(String::as_str),
            Some("1.0-20260116.120000-2")
        );
    }

    #[test]
    fn non_unique_snapshot_works_from_local_cache_offline() {
        let dir = tempfile::tempdir().unwrap();
        // Non-unique layout in ~/.m2: foo-1.0-SNAPSHOT.jar (no metadata needed).
        write_fake_artifact(dir.path(), "com.example", "local-snap", "1.0-SNAPSHOT", &[]);

        let jars = run_resolve(
            dir.path(),
            &[("com.example:local-snap", "1.0-SNAPSHOT")],
            vec![],
        )
        .unwrap();
        assert_eq!(jars.len(), 1);
        assert!(jars[0]
            .to_string_lossy()
            .contains("local-snap-1.0-SNAPSHOT.jar"));
    }

    #[test]
    fn file_url_to_path_parses_absolute() {
        let p = file_url_to_path("file:///tmp/repo/foo.jar").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/repo/foo.jar"));
    }

    #[test]
    fn file_url_to_path_parses_relative_without_host_stripping() {
        // Must not treat "examples" as a host and drop it.
        let p = file_url_to_path("file://examples/snapshot-demo/local-repo/x").unwrap();
        assert_eq!(p, PathBuf::from("examples/snapshot-demo/local-repo/x"));
        let p = file_url_to_path("file:local-repo/x").unwrap();
        assert_eq!(p, PathBuf::from("local-repo/x"));
    }
}
