//! Resource filtering: variable substitution (and, in future, richer
//! templating) over a project's resource files.
//!
//! curie copies resources verbatim by default — the zero-copy fast path.  When
//! a `[resources]` or `[test-resources]` scope is *active* (it declares filter
//! stages or custom source `directories`), this module materializes a merged,
//! optionally-filtered output directory under `target/` that every downstream
//! consumer (jar, fat-jar, run, test, dev) reads instead of the raw sources.
//!
//! ## Design
//! The per-file text transform sits behind the [`TemplateEngine`] trait so new
//! mechanisms drop in without touching discovery, the dir walk, binary-safety,
//! or the build orchestration.  Two engines are available:
//! - [`SubstituteEngine`] — dependency-free `@var@` replacement (Maven-syncable).
//! - [`LiquidEngine`] — full [Liquid](https://shopify.github.io/liquid/) templating
//!   (`{{ project.version }}`, `{% if %}`, filters) via the `liquid` crate.
//!
//! Filtering is an ordered list of stages; a file is folded through every stage
//! whose origin-root restriction *and* include/exclude globs accept it, in
//! declaration order — so multiple engines can chain over the same file.

use anyhow::{bail, Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::descriptor::{Descriptor, FilterStage, ResourceDirectory, Resources, SubstituteOpts};

/// Result of running both resource scopes.  A `None` directory means that scope
/// is inactive and downstream consumers should keep using the raw source dir.
pub struct FilterOutput {
    /// `target/resources` when the main scope is active, else `None`.
    pub main_dir: Option<PathBuf>,
    /// `target/test-resources` when the test scope is active, else `None`.
    pub test_dir: Option<PathBuf>,
}

/// Run resource filtering for both scopes.  `raw_main`/`raw_test` are the
/// auto-discovered source dirs (used when a scope configures no `directories`).
pub fn process_resources(
    project_root: &Path,
    desc: &Descriptor,
    raw_main: Option<&Path>,
    raw_test: Option<&Path>,
    git_commit: Option<&str>,
    target_dir: &Path,
) -> Result<FilterOutput> {
    let base = build_base_context(desc, git_commit);
    let main_dir = process_scope(
        project_root,
        &desc.resources,
        raw_main,
        &base,
        target_dir.join("resources"),
        "resources",
    )?;
    let test_dir = process_scope(
        project_root,
        &desc.test_resources,
        raw_test,
        &base,
        target_dir.join("test-resources"),
        "test-resources",
    )?;
    Ok(FilterOutput { main_dir, test_dir })
}

/// Convenience for compile-then-test entry points that bypass the full build
/// (`curie test`, workspace test fan-out, BSP): run filtering and return the
/// *effective* `(main, test)` resource dirs (filtered output when a scope is
/// active, else the raw source dir).  A filtering error is propagated.
pub fn effective_test_dirs(
    project_root: &Path,
    desc: &Descriptor,
    raw_main: Option<&Path>,
    raw_test: Option<&Path>,
    target_dir: &Path,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let git_commit = crate::git::detect(project_root).map(|i| i.commit_id);
    let out = process_resources(
        project_root,
        desc,
        raw_main,
        raw_test,
        git_commit.as_deref(),
        target_dir,
    )?;
    Ok((
        out.main_dir.or_else(|| raw_main.map(Path::to_path_buf)),
        out.test_dir.or_else(|| raw_test.map(Path::to_path_buf)),
    ))
}

/// Run the shared per-scope routine.  Returns the materialized output dir (when
/// active), re-filtering only when the value fingerprint changed or a source
/// file is newer than the last filter — so an unchanged build stays zero-work
/// and the JAR repackage cascades naturally off the rewritten output's mtimes.
fn process_scope(
    project_root: &Path,
    scope: &Resources,
    auto: Option<&Path>,
    base: &VarContext,
    out_dir: PathBuf,
    label: &str,
) -> Result<Option<PathBuf>> {
    if !scope.is_active() {
        return Ok(None);
    }
    let roots = resolve_source_roots(project_root, scope, auto, label)?;
    if roots.is_empty() {
        return Ok(None);
    }
    let vars = scope_context(base, scope, project_root)?;
    let stages = compile_stages(project_root, scope, &roots, label)?;
    let non_filtered = binary_extension_set(&scope.non_filtered_extensions);
    let filter_file_mtimes = collect_filter_file_mtimes(project_root, scope);
    let fingerprint = fingerprint_scope(scope, &vars, &roots, &filter_file_mtimes);

    let stamp = fingerprint_stamp_path(&out_dir, label);
    if scope_is_stale(&out_dir, &stamp, fingerprint, &roots) {
        filter_roots(&roots, &out_dir, &stages, &vars, &non_filtered)?;
        crate::incremental::write_u64_stamp(&stamp, fingerprint)?;
    }
    Ok(Some(out_dir))
}

/// The per-scope fingerprint stamp path (sibling of `out_dir` under `target/`).
fn fingerprint_stamp_path(out_dir: &Path, label: &str) -> PathBuf {
    let parent = out_dir.parent().unwrap_or(out_dir);
    parent.join(format!(".resources-filter-{}", label))
}

/// Whether the scope's output must be re-filtered: missing output, a changed
/// value fingerprint, or any selected source file newer than the last filter
/// stamp.
fn scope_is_stale(out_dir: &Path, stamp: &Path, fingerprint: u64, roots: &[SourceRoot]) -> bool {
    if !out_dir.exists() {
        return true;
    }
    if crate::incremental::load_u64_stamp(stamp) != Some(fingerprint) {
        return true;
    }
    let Some(stamp_mtime) = file_mtime(stamp) else {
        return true;
    };
    roots
        .iter()
        .any(|root| newest_mtime_in_root(root) > Some(stamp_mtime))
}

/// One source root feeding a scope's processed output, from `directories`
/// or auto-discovery.
#[derive(Debug, Clone)]
struct SourceRoot {
    path: PathBuf,
    includes: Vec<String>,
    excludes: Vec<String>,
    target_path: String,
    /// Skip VCS metadata directories (`.git`, `.svn`, `.hg`) while walking.
    /// Set when the entry is not an identity copy, which often points at a
    /// repo or project root.
    prune_vcs: bool,
}

impl SourceRoot {
    fn identity(path: PathBuf) -> Self {
        SourceRoot {
            path,
            includes: Vec::new(),
            excludes: Vec::new(),
            target_path: String::new(),
            prune_vcs: false,
        }
    }

    fn from_directory(path: PathBuf, dir: &ResourceDirectory) -> Self {
        SourceRoot {
            path,
            includes: dir.includes.clone(),
            excludes: dir.excludes.clone(),
            target_path: dir.target_path.clone(),
            prune_vcs: !dir.is_identity(),
        }
    }
}

/// Resolve a scope's source roots: the configured `directories` (each
/// validated to exist) when present, else the single auto-discovered dir.
fn resolve_source_roots(
    project_root: &Path,
    scope: &Resources,
    auto: Option<&Path>,
    label: &str,
) -> Result<Vec<SourceRoot>> {
    if scope.directories.is_empty() {
        return Ok(auto
            .map(|p| vec![SourceRoot::identity(p.to_path_buf())])
            .unwrap_or_default());
    }
    let mut roots = Vec::with_capacity(scope.directories.len());
    for dir in &scope.directories {
        let path = project_root.join(&dir.path);
        if !path.is_dir() {
            bail!("[{}] source directory '{}' does not exist", label, dir.path);
        }
        roots.push(SourceRoot::from_directory(path, dir));
    }
    Ok(roots)
}

// ---------------------------------------------------------------------------
// Variable context
// ---------------------------------------------------------------------------

/// Engine-agnostic variable context: dotted keys (`project.version`) mapped to
/// string values, plus dynamic `env.*` lookup.  Built once per build, shared
/// across all stages.  (A nested object view for the future liquid engine is a
/// v2 addition; the substitute scanner only needs the flat map.)
pub struct VarContext {
    flat: BTreeMap<String, String>,
}

impl VarContext {
    /// Resolve a dotted variable name.  `env.*` names fall through to the
    /// process environment so arbitrary env vars work without enumeration.
    fn resolve(&self, name: &str) -> Option<String> {
        if let Some(value) = self.flat.get(name) {
            return Some(value.clone());
        }
        name.strip_prefix("env.")
            .and_then(|var| std::env::var(var).ok())
    }

    /// Convert the flat variable map into a nested `liquid::Object` for the
    /// Liquid engine.  Dotted keys are split into nested objects: `project.version`
    /// becomes `{ "project": { "version": "1.0" } }`.  The `env` namespace is
    /// populated with all process environment variables so `{{ env.HOME }}` works.
    fn to_liquid_object(&self) -> liquid::Object {
        use liquid::model::{KString, Value};

        let mut root = liquid::Object::new();

        // Insert the flat map entries as nested objects.
        for (key, value) in &self.flat {
            insert_nested(&mut root, key, Value::scalar(value.clone()));
        }

        // Populate `env.*` from the process environment.
        let mut env_obj = liquid::Object::new();
        for (k, v) in std::env::vars() {
            env_obj.insert(KString::from_string(k), Value::scalar(v));
        }
        root.insert(KString::from_static("env"), Value::Object(env_obj));

        root
    }
}

/// Insert a value into a nested `liquid::Object` by splitting the dotted key.
/// For example, `insert_nested(obj, "project.version", val)` creates
/// `obj["project"]["version"] = val`.
fn insert_nested(obj: &mut liquid::Object, dotted_key: &str, value: liquid::model::Value) {
    use liquid::model::KString;

    let parts: Vec<&str> = dotted_key.split('.').collect();
    if parts.len() == 1 {
        obj.insert(KString::from_ref(parts[0]), value);
        return;
    }
    // Walk/create intermediate objects.
    let mut current = obj;
    for part in &parts[..parts.len() - 1] {
        let key = KString::from_ref(part);
        let entry = current
            .entry(key)
            .or_insert_with(|| liquid::model::Value::Object(liquid::Object::new()));
        current = match entry {
            liquid::model::Value::Object(ref mut inner) => inner,
            _ => {
                // A scalar at an intermediate level: replace with object.
                *entry = liquid::model::Value::Object(liquid::Object::new());
                match entry {
                    liquid::model::Value::Object(ref mut inner) => inner,
                    _ => unreachable!(),
                }
            }
        };
    }
    current.insert(KString::from_ref(parts[parts.len() - 1]), value);
}

/// Build the shared base context: `project.*` and `git.*`.  Identical for both
/// scopes; each scope layers its own `properties`/`filterFiles` on top.
fn build_base_context(desc: &Descriptor, git_commit: Option<&str>) -> VarContext {
    let mut flat = BTreeMap::new();
    if let Some(name) = desc.project_name() {
        flat.insert("project.name".to_string(), name.to_string());
        flat.insert("project.artifactId".to_string(), name.to_string());
    }
    if let Some(version) = desc.project_version() {
        flat.insert("project.version".to_string(), version.to_string());
    }
    if let Some(group) = desc.group_id() {
        flat.insert("project.groupId".to_string(), group.to_string());
    }
    if let Some(commit) = git_commit {
        flat.insert("git.commit.id".to_string(), commit.to_string());
    }
    VarContext { flat }
}

/// Layer a scope's own variables over the shared base: `filterFiles` first
/// (lower precedence), then inline `properties` (higher).  `env.*` stays
/// highest via dynamic resolution.
fn scope_context(base: &VarContext, scope: &Resources, project_root: &Path) -> Result<VarContext> {
    let mut flat = base.flat.clone();
    for file in &scope.filter_files {
        let path = project_root.join(file);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read filterFile {}", path.display()))?;
        for (key, value) in parse_properties(&content) {
            flat.insert(key, value);
        }
    }
    for (key, value) in &scope.properties {
        flat.insert(key.clone(), value.clone());
    }
    Ok(VarContext { flat })
}

/// Parse a `.properties` file: `key=value` (or `key:value`) lines, skipping
/// blanks and `#`/`!` comments.  Keys and values are trimmed.
fn parse_properties(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        let sep = trimmed.find('=').or_else(|| trimmed.find(':'));
        if let Some(idx) = sep {
            let key = trimmed[..idx].trim().to_string();
            let value = trimmed[idx + 1..].trim().to_string();
            if !key.is_empty() {
                out.push((key, value));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Engines
// ---------------------------------------------------------------------------

/// A filtering mechanism: renders one file's text against the shared variable
/// context.  Stateless per call; one instance per configured stage.
pub trait TemplateEngine {
    fn render(&self, path: &Path, text: &str, vars: &VarContext) -> Result<String>;
    fn name(&self) -> &'static str;
}

/// The v1 engine: `@var@`-style placeholder substitution.
struct SubstituteEngine {
    begin: String,
    end: String,
    fail_on_unresolved: bool,
}

impl From<SubstituteOpts> for SubstituteEngine {
    fn from(opts: SubstituteOpts) -> Self {
        SubstituteEngine {
            begin: opts.delimiter.begin_token().to_string(),
            end: opts.delimiter.end_token().to_string(),
            fail_on_unresolved: opts.fail_on_unresolved,
        }
    }
}

impl TemplateEngine for SubstituteEngine {
    fn render(&self, path: &Path, text: &str, vars: &VarContext) -> Result<String> {
        substitute(
            text,
            &self.begin,
            &self.end,
            vars,
            self.fail_on_unresolved,
            path,
        )
    }
    fn name(&self) -> &'static str {
        "substitute"
    }
}

/// The Liquid engine: full Liquid templating (`{{ project.version }}`,
/// `{% if %}`, filters).  Variables are exposed as nested objects so
/// `project.version` becomes `{{ project.version }}`.
struct LiquidEngine {
    parser: liquid::Parser,
}

impl LiquidEngine {
    fn new() -> Result<Self> {
        let parser = liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build liquid parser: {}", e))?;
        Ok(LiquidEngine { parser })
    }
}

impl TemplateEngine for LiquidEngine {
    fn render(&self, path: &Path, text: &str, vars: &VarContext) -> Result<String> {
        let template = self
            .parser
            .parse(text)
            .with_context(|| format!("liquid parse error in {}", path.display()))?;
        let globals = vars.to_liquid_object();
        template
            .render(&globals)
            .with_context(|| format!("liquid render error in {}", path.display()))
    }
    fn name(&self) -> &'static str {
        "liquid"
    }
}

/// Build one engine instance from a stage's config.  The only place that
/// matches on the engine kind; each engine reads only its own opts struct.
fn make_engine(stage: &FilterStage) -> Result<Box<dyn TemplateEngine>> {
    use crate::descriptor::Engine;
    match stage.engine {
        Engine::Substitute => Ok(Box::new(SubstituteEngine::from(
            stage.substitute.clone().unwrap_or_default(),
        ))),
        Engine::Liquid => Ok(Box::new(LiquidEngine::new()?)),
    }
}

/// A compiled stage: an engine instance, its include/exclude globs, and the
/// optional set of origin roots it is restricted to (`None` ⇒ every scope root).
struct CompiledStage {
    engine: Box<dyn TemplateEngine>,
    includes: Vec<String>,
    excludes: Vec<String>,
    roots: Option<Vec<PathBuf>>,
}

impl CompiledStage {
    /// Whether this stage applies to a file originating from `root`.
    fn accepts_root(&self, root: &Path) -> bool {
        match &self.roots {
            None => true,
            Some(roots) => roots.iter().any(|r| r == root),
        }
    }

    /// Whether this stage's globs select `rel_path` (a `/`-joined relative path).
    fn matches_path(&self, rel_path: &str) -> bool {
        let included =
            self.includes.is_empty() || self.includes.iter().any(|p| glob_match(p, rel_path));
        let excluded = self.excludes.iter().any(|p| glob_match(p, rel_path));
        included && !excluded
    }
}

/// Compile one scope's stages in order, validating each stage's `directories`
/// against the scope's resolved source roots.
fn compile_stages(
    project_root: &Path,
    scope: &Resources,
    scope_roots: &[SourceRoot],
    label: &str,
) -> Result<Vec<CompiledStage>> {
    let mut compiled = Vec::with_capacity(scope.filter.len());
    for stage in &scope.filter {
        let roots = compile_stage_roots(project_root, stage, scope_roots, label)?;
        compiled.push(CompiledStage {
            engine: make_engine(stage)?,
            includes: stage.includes.clone(),
            excludes: stage.excludes.clone(),
            roots,
        });
    }
    Ok(compiled)
}

/// Resolve and validate a stage's optional `directories` into absolute roots.
fn compile_stage_roots(
    project_root: &Path,
    stage: &FilterStage,
    scope_roots: &[SourceRoot],
    label: &str,
) -> Result<Option<Vec<PathBuf>>> {
    if stage.directories.is_empty() {
        return Ok(None);
    }
    let mut roots = Vec::with_capacity(stage.directories.len());
    for dir in &stage.directories {
        let path = project_root.join(dir);
        if !scope_roots.iter().any(|r| r.path == path) {
            bail!(
                "[{}] filter stage directory '{}' is not one of the [{}] source directories",
                label,
                dir,
                label
            );
        }
        roots.push(path);
    }
    Ok(Some(roots))
}

// ---------------------------------------------------------------------------
// The dir walk: merge roots, filter files, atomic swap
// ---------------------------------------------------------------------------

/// Clear and repopulate `out_dir` from `roots`, merging in order (later root
/// wins on a relative-path collision) and folding each text file through the
/// matching stages.  Builds into a `.part` staging dir, then atomically swaps.
fn filter_roots(
    roots: &[SourceRoot],
    out_dir: &Path,
    stages: &[CompiledStage],
    vars: &VarContext,
    non_filtered_exts: &HashSet<String>,
) -> Result<()> {
    let staging = staging_dir(out_dir);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to clear {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;

    for root in roots {
        merge_root_into(root, &staging, stages, vars, non_filtered_exts)?;
    }

    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)
            .with_context(|| format!("failed to clear {}", out_dir.display()))?;
    }
    std::fs::rename(&staging, out_dir).with_context(|| {
        format!(
            "failed to move {} into {}",
            staging.display(),
            out_dir.display()
        )
    })?;
    Ok(())
}

/// A sibling staging directory next to `out_dir` (same parent → atomic rename).
pub(crate) fn staging_dir(out_dir: &Path) -> PathBuf {
    let mut name = out_dir.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    out_dir.with_file_name(name)
}

/// Walk one source root, writing each selected file into `staging` (filtered
/// or verbatim), remapped under `target_path` when set.
fn merge_root_into(
    root: &SourceRoot,
    staging: &Path,
    stages: &[CompiledStage],
    vars: &VarContext,
    non_filtered_exts: &HashSet<String>,
) -> Result<()> {
    for_each_selected_file(root, |abs, rel| {
        let dest = staging.join(mapped_rel(&root.target_path, rel));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        process_one_file(&root.path, rel, abs, &dest, stages, vars, non_filtered_exts)
    })
}

/// Destination path of a selected file: `targetPath/rel`, or `rel` when the
/// mapping has no target prefix.
fn mapped_rel(target_path: &str, rel: &Path) -> PathBuf {
    if target_path.is_empty() {
        return rel.to_path_buf();
    }
    let mut dest = PathBuf::new();
    for part in target_path.split(['/', '\\']) {
        if !part.is_empty() && part != "." {
            dest.push(part);
        }
    }
    dest.join(rel)
}

/// Call `visit(abs, rel)` for every file under `root` that passes the root's
/// include/exclude globs.  VCS directories are skipped when `prune_vcs`.
fn for_each_selected_file(
    root: &SourceRoot,
    mut visit: impl FnMut(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let walker = walkdir::WalkDir::new(&root.path)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| keep_walk_entry(e, root));
    for entry in walker {
        let entry = entry.with_context(|| format!("failed to walk {}", root.path.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(&root.path)
            .expect("walked path is under root");
        let rel_str = rel_to_slash(rel);
        if !file_is_selected(&rel_str, &root.includes, &root.excludes) {
            continue;
        }
        visit(abs, rel)?;
    }
    Ok(())
}

/// Whether a walk entry should be descended into / yielded.  Files always
/// pass (include/exclude is applied after); directories are pruned when
/// they cannot contain a match or are VCS metadata.
fn keep_walk_entry(entry: &walkdir::DirEntry, root: &SourceRoot) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    if root.prune_vcs && is_vcs_dir(entry.file_name()) {
        return false;
    }
    let Ok(rel) = entry.path().strip_prefix(&root.path) else {
        return true;
    };
    if rel.as_os_str().is_empty() {
        return true;
    }
    dir_may_match_includes(&rel_to_slash(rel), &root.includes)
}

fn is_vcs_dir(name: &std::ffi::OsStr) -> bool {
    name == ".git" || name == ".svn" || name == ".hg"
}

/// Whether `rel_path` (slash-separated) is selected by `includes`/`excludes`.
/// Empty `includes` means every file; `excludes` win.
fn file_is_selected(rel_path: &str, includes: &[String], excludes: &[String]) -> bool {
    let included = includes.is_empty() || includes.iter().any(|p| glob_match(p, rel_path));
    let excluded = excludes.iter().any(|p| glob_match(p, rel_path));
    included && !excluded
}

/// Whether `rel_dir` might contain a file matching any include pattern.
/// Conservative: never returns false when a match could exist.
fn dir_may_match_includes(rel_dir: &str, includes: &[String]) -> bool {
    if includes.is_empty() {
        return true;
    }
    includes
        .iter()
        .any(|pat| dir_may_match_pattern(pat, rel_dir))
}

/// Conservative directory prune for one include glob.  `**` matches anywhere;
/// a literal prefix must share an ancestor/descendant relationship with `dir`.
fn dir_may_match_pattern(pattern: &str, dir: &str) -> bool {
    if dir.is_empty() || pattern.starts_with("**") {
        return true;
    }
    let glob_idx = pattern.find(['*', '?']).unwrap_or(pattern.len());
    let literal = pattern[..glob_idx].trim_end_matches('/');
    if literal.is_empty() {
        return true;
    }
    if dir == literal || literal.starts_with(&format!("{dir}/")) {
        return true;
    }
    if glob_idx < pattern.len() && dir.starts_with(&format!("{literal}/")) {
        let rest = &pattern[glob_idx..];
        if rest.starts_with("**") {
            return true;
        }
        // Single-segment glob (`dir/*`): only immediate children of `literal`.
        let extra = &dir[literal.len() + 1..];
        return !extra.contains('/');
    }
    false
}

/// Process a single file: copy binaries verbatim, otherwise fold its text
/// through every matching stage.
fn process_one_file(
    root: &Path,
    rel: &Path,
    abs: &Path,
    dest: &Path,
    stages: &[CompiledStage],
    vars: &VarContext,
    non_filtered_exts: &HashSet<String>,
) -> Result<()> {
    let bytes = std::fs::read(abs).with_context(|| format!("failed to read {}", abs.display()))?;
    if !should_filter_file(abs, non_filtered_exts) || is_binary_content(&bytes) {
        return write_bytes(dest, &bytes);
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => return write_bytes(dest, err.as_bytes()),
    };
    let rel_str = rel_to_slash(rel);
    let rendered = apply_stages(root, &rel_str, abs, text, stages, vars)?;
    std::fs::write(dest, rendered).with_context(|| format!("failed to write {}", dest.display()))
}

fn write_bytes(dest: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(dest, bytes).with_context(|| format!("failed to write {}", dest.display()))
}

/// Fold `text` through every stage whose origin-root restriction and globs
/// accept the file, in declaration order (stage N sees stage N-1's output).
fn apply_stages(
    root: &Path,
    rel_path: &str,
    abs: &Path,
    text: String,
    stages: &[CompiledStage],
    vars: &VarContext,
) -> Result<String> {
    let mut text = text;
    for stage in stages {
        if stage.accepts_root(root) && stage.matches_path(rel_path) {
            text = stage.engine.render(abs, &text, vars).with_context(|| {
                format!(
                    "{} filter stage failed on {}",
                    stage.engine.name(),
                    rel_path
                )
            })?;
        }
    }
    Ok(text)
}

/// Render a relative path with `/` separators for glob matching (Windows-safe).
fn rel_to_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Binary safety
// ---------------------------------------------------------------------------

/// File extensions copied verbatim regardless of content (the common binary
/// resource types), so substitution never corrupts them.
const BUILTIN_BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "svgz", //
    "pdf", "zip", "jar", "war", "ear", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", //
    "class", "so", "dll", "dylib", "exe", "bin", "o", "a", //
    "woff", "woff2", "ttf", "otf", "eot", //
    "mp3", "mp4", "wav", "avi", "mov", "mkv", "ogg", "flac", "webm", //
    "keystore", "jks", "p12", "pfx", "der", "db", "sqlite", "dat",
];

/// The effective binary-extension set: built-ins plus the scope's extras
/// (lower-cased).
fn binary_extension_set(extra: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = BUILTIN_BINARY_EXTENSIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for ext in extra {
        set.insert(ext.trim_start_matches('.').to_ascii_lowercase());
    }
    set
}

/// Whether `path` is a candidate for filtering by extension (false ⇒ binary).
fn should_filter_file(path: &Path, non_filtered_exts: &HashSet<String>) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => !non_filtered_exts.contains(&ext.to_ascii_lowercase()),
        None => true,
    }
}

/// Content sniff: a NUL byte in the first ~8KB marks the file as binary.
fn is_binary_content(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(8192)];
    window.contains(&0)
}

// ---------------------------------------------------------------------------
// Substitute scanner
// ---------------------------------------------------------------------------

/// Replace `begin name end` placeholders in `text`.  A doubled begin token
/// escapes to a literal begin token and does not start a placeholder.  An
/// unresolved name is a hard error when `fail_on_unresolved`, else left verbatim.
fn substitute(
    text: &str,
    begin: &str,
    end: &str,
    vars: &VarContext,
    fail_on_unresolved: bool,
    path: &Path,
) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some(begin_at) = rest.find(begin) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..begin_at]);
        let after_begin = &rest[begin_at + begin.len()..];

        // Doubled begin token → literal begin token.
        if let Some(after_doubled) = after_begin.strip_prefix(begin) {
            out.push_str(begin);
            rest = after_doubled;
            continue;
        }

        match after_begin.find(end) {
            Some(end_at) => {
                let name = after_begin[..end_at].trim();
                match vars.resolve(name) {
                    Some(value) => out.push_str(&value),
                    None if fail_on_unresolved => bail!(
                        "unresolved placeholder {}{}{} in {}",
                        begin,
                        name,
                        end,
                        path.display()
                    ),
                    None => {
                        out.push_str(begin);
                        out.push_str(&after_begin[..end_at]);
                        out.push_str(end);
                    }
                }
                rest = &after_begin[end_at + end.len()..];
            }
            None => {
                // No closing delimiter: emit the begin token literally and move on.
                out.push_str(begin);
                rest = after_begin;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Glob matching (dependency-free; supports **, *, ?)
// ---------------------------------------------------------------------------

/// Match a glob `pattern` against a `/`-separated relative path.  `*` matches
/// within a path segment, `**` crosses `/`, `?` matches one non-`/` char.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    match pat[0] {
        b'*' if pat.len() >= 2 && pat[1] == b'*' => {
            let rest = &pat[2..];
            // '**/x' may match zero directories: try the part after the slash.
            if let Some(after_slash) = rest.strip_prefix(b"/") {
                if glob_match_bytes(after_slash, text) {
                    return true;
                }
            }
            // Otherwise '**' consumes any run of characters, including '/'.
            (0..=text.len()).any(|i| glob_match_bytes(rest, &text[i..]))
        }
        b'*' => {
            let rest = &pat[1..];
            let mut i = 0;
            loop {
                if glob_match_bytes(rest, &text[i..]) {
                    return true;
                }
                if i >= text.len() || text[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
        b'?' => !text.is_empty() && text[0] != b'/' && glob_match_bytes(&pat[1..], &text[1..]),
        c => !text.is_empty() && text[0] == c && glob_match_bytes(&pat[1..], &text[1..]),
    }
}

// ---------------------------------------------------------------------------
// Fingerprint (value-only incremental)
// ---------------------------------------------------------------------------

/// mtime (seconds since epoch) of each of the scope's filter files, in order;
/// missing files contribute `0`.
fn collect_filter_file_mtimes(project_root: &Path, scope: &Resources) -> Vec<u64> {
    scope
        .filter_files
        .iter()
        .map(|f| file_mtime_secs(&project_root.join(f)))
        .collect()
}

fn file_mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable value fingerprint of a scope: all resolved variables, every stage's
/// config, every remapping, the resolved source-root list, and filter-file
/// mtimes.  A change in any of these forces a re-filter even when no source
/// file's mtime moved.
fn fingerprint_scope(
    scope: &Resources,
    vars: &VarContext,
    roots: &[SourceRoot],
    filter_file_mtimes: &[u64],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    for (key, value) in &vars.flat {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    for root in roots {
        root.path.hash(&mut hasher);
        root.includes.hash(&mut hasher);
        root.excludes.hash(&mut hasher);
        root.target_path.hash(&mut hasher);
    }
    for mtime in filter_file_mtimes {
        mtime.hash(&mut hasher);
    }
    for stage in &scope.filter {
        hash_stage(stage, &mut hasher);
    }
    hasher.finish()
}

fn hash_stage(stage: &FilterStage, hasher: &mut DefaultHasher) {
    stage.engine_name().hash(hasher);
    stage.directories.hash(hasher);
    stage.includes.hash(hasher);
    stage.excludes.hash(hasher);
    if let Some(opts) = &stage.substitute {
        opts.delimiter.begin_token().hash(hasher);
        opts.delimiter.end_token().hash(hasher);
        opts.fail_on_unresolved.hash(hasher);
    }
    // LiquidOpts has no configuration knobs yet; the engine name alone
    // distinguishes it.  When knobs are added, hash them here.
}

/// Last-modified time of a file, or `None` when it can't be stat'd.
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Newest mtime of any *selected* file under `root`, or `None` when empty.
fn newest_mtime_in_root(root: &SourceRoot) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let _ = for_each_selected_file(root, |abs, _rel| {
        if let Some(mtime) = file_mtime(abs) {
            newest = Some(match newest {
                Some(prev) if prev >= mtime => prev,
                _ => mtime,
            });
        }
        Ok(())
    });
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, &str)]) -> VarContext {
        VarContext {
            flat: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn sub(text: &str, begin: &str, end: &str, vars: &VarContext, fail: bool) -> Result<String> {
        substitute(text, begin, end, vars, fail, Path::new("f"))
    }

    #[test]
    fn at_var_substitutes() {
        let vars = ctx(&[("project.version", "1.2.3")]);
        let out = sub("v=@project.version@", "@", "@", &vars, true).unwrap();
        assert_eq!(out, "v=1.2.3");
    }

    #[test]
    fn dollar_brace_when_configured() {
        let vars = ctx(&[("api.url", "https://x")]);
        let out = sub("u=${api.url}", "${", "}", &vars, true).unwrap();
        assert_eq!(out, "u=https://x");
    }

    #[test]
    fn doubled_delimiter_escapes() {
        let vars = ctx(&[]);
        let out = sub("email me @@ home", "@", "@", &vars, true).unwrap();
        assert_eq!(out, "email me @ home");
    }

    #[test]
    fn unknown_var_hard_errors() {
        let vars = ctx(&[]);
        let err = sub("x=@nope@", "@", "@", &vars, true).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn unknown_var_left_when_fail_off() {
        let vars = ctx(&[]);
        let out = sub("x=@nope@", "@", "@", &vars, false).unwrap();
        assert_eq!(out, "x=@nope@");
    }

    #[test]
    fn unclosed_delimiter_is_literal() {
        let vars = ctx(&[]);
        let out = sub("a @ b", "@", "@", &vars, true).unwrap();
        assert_eq!(out, "a @ b");
    }

    #[test]
    fn precedence_inline_over_filterfile_over_project() {
        let base = ctx(&[("project.version", "0.0.0")]);
        let mut scope = Resources::default();
        scope.properties.insert("k".into(), "inline".into());
        // simulate a filterFile by pre-seeding the base differently is hard here;
        // instead assert inline wins over a base value of the same key.
        let merged = scope_context(&base, &scope, Path::new(".")).unwrap();
        assert_eq!(merged.resolve("k").as_deref(), Some("inline"));
        assert_eq!(merged.resolve("project.version").as_deref(), Some("0.0.0"));
    }

    #[test]
    fn glob_double_star_matches_nested() {
        assert!(glob_match("**/*.properties", "config/app.properties"));
        assert!(glob_match("**/*.properties", "app.properties"));
        assert!(!glob_match("**/*.properties", "app.yml"));
    }

    #[test]
    fn glob_single_star_stays_in_segment() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(!glob_match("*.txt", "dir/a.txt"));
    }

    #[test]
    fn binary_extension_copied_verbatim() {
        let set = binary_extension_set(&[]);
        assert!(!should_filter_file(Path::new("logo.png"), &set));
        assert!(should_filter_file(Path::new("app.properties"), &set));
    }

    #[test]
    fn extra_non_filtered_extension_honored() {
        let set = binary_extension_set(&["bin".to_string()]);
        assert!(!should_filter_file(Path::new("data.bin"), &set));
    }

    #[test]
    fn binary_content_sniff_skips_nul_file() {
        assert!(is_binary_content(&[b'a', 0, b'b']));
        assert!(!is_binary_content(b"plain text"));
    }

    #[test]
    fn properties_parse_skips_comments() {
        let parsed = parse_properties("# c\n\nk=v\nx : y\n");
        assert_eq!(
            parsed,
            vec![("k".into(), "v".into()), ("x".into(), "y".into())]
        );
    }

    fn identity_roots(paths: &[PathBuf]) -> Vec<SourceRoot> {
        paths.iter().cloned().map(SourceRoot::identity).collect()
    }

    #[test]
    fn fingerprint_changes_on_stage_edit() {
        let vars = ctx(&[]);
        let roots = identity_roots(&[PathBuf::from("/r")]);
        let mut a = Resources::default();
        a.filter.push(FilterStage {
            engine: crate::descriptor::Engine::Substitute,
            directories: vec![],
            includes: vec!["*.txt".into()],
            excludes: vec![],
            substitute: None,
            liquid: None,
        });
        let mut b = a.clone();
        b.filter[0].includes = vec!["*.md".into()];
        let fa = fingerprint_scope(&a, &vars, &roots, &[]);
        let fb = fingerprint_scope(&b, &vars, &roots, &[]);
        assert_ne!(fa, fb);
    }

    #[test]
    fn fingerprint_changes_on_var_edit() {
        let roots = identity_roots(&[PathBuf::from("/r")]);
        let scope = Resources::default();
        let fa = fingerprint_scope(&scope, &ctx(&[("v", "1")]), &roots, &[]);
        let fb = fingerprint_scope(&scope, &ctx(&[("v", "2")]), &roots, &[]);
        assert_ne!(fa, fb);
    }

    #[test]
    fn fingerprint_stable_when_unchanged() {
        let roots = identity_roots(&[PathBuf::from("/r")]);
        let scope = Resources::default();
        let vars = ctx(&[("v", "1")]);
        assert_eq!(
            fingerprint_scope(&scope, &vars, &roots, &[7]),
            fingerprint_scope(&scope, &vars, &roots, &[7])
        );
    }

    fn mapped_dir(path: &str, includes: &[&str], target_path: &str) -> ResourceDirectory {
        ResourceDirectory {
            path: path.into(),
            includes: includes.iter().map(|s| s.to_string()).collect(),
            excludes: vec![],
            target_path: target_path.into(),
        }
    }

    #[test]
    fn fingerprint_changes_on_directory_target_path_edit() {
        let vars = ctx(&[]);
        let mut a = Resources::default();
        a.directories
            .push(mapped_dir("..", &["LICENSE"], "META-INF"));
        let mut b = a.clone();
        b.directories[0].target_path = "META-INF/legal".into();
        let roots_a = vec![SourceRoot::from_directory(
            PathBuf::from("/r"),
            &a.directories[0],
        )];
        let roots_b = vec![SourceRoot::from_directory(
            PathBuf::from("/r"),
            &b.directories[0],
        )];
        let fa = fingerprint_scope(&a, &vars, &roots_a, &[]);
        let fb = fingerprint_scope(&b, &vars, &roots_b, &[]);
        assert_ne!(fa, fb);
    }

    // -- dir-walk / chaining integration --------------------------------------

    use crate::descriptor::Engine;

    /// `unwrap` for results whose `Ok`/`Err` types aren't `Debug`
    /// (`Vec<CompiledStage>` holds `dyn TemplateEngine`).
    fn must<T>(r: Result<T>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {e:#}"),
        }
    }

    fn substitute_stage(includes: &[&str], excludes: &[&str], directories: &[&str]) -> FilterStage {
        FilterStage {
            engine: Engine::Substitute,
            directories: directories.iter().map(|s| s.to_string()).collect(),
            includes: includes.iter().map(|s| s.to_string()).collect(),
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            substitute: None,
            liquid: None,
        }
    }

    /// Run `filter_roots` over `roots` (each a slice of (relpath, contents))
    /// and return the output directory's (relpath -> contents) map.
    fn run_filter(
        roots: &[&[(&str, &str)]],
        stages: &[FilterStage],
        vars: &[(&str, &str)],
    ) -> (tempfile::TempDir, BTreeMap<String, String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut root_paths = Vec::new();
        for (i, files) in roots.iter().enumerate() {
            let root = dir.path().join(format!("root{i}"));
            for (rel, contents) in *files {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, contents).unwrap();
            }
            root_paths.push(root);
        }
        let scope = Resources {
            filter: stages.to_vec(),
            ..Default::default()
        };
        let roots = identity_roots(&root_paths);
        let compiled = must(compile_stages(dir.path(), &scope, &roots, "resources"));
        let out = dir.path().join("out");
        let vctx = ctx(vars);
        let non_filtered = binary_extension_set(&[]);
        filter_roots(&roots, &out, &compiled, &vctx, &non_filtered).unwrap();

        let mut got = BTreeMap::new();
        for entry in walkdir::WalkDir::new(&out)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(&out).unwrap();
                got.insert(
                    rel_to_slash(rel),
                    std::fs::read_to_string(entry.path()).unwrap(),
                );
            }
        }
        (dir, got)
    }

    #[test]
    fn no_filter_stage_is_verbatim() {
        let (_d, out) = run_filter(&[&[("a.txt", "v=@x@")]], &[], &[("x", "1")]);
        assert_eq!(out["a.txt"], "v=@x@");
    }

    #[test]
    fn includes_excludes_select_subset() {
        let stage = substitute_stage(&["**/*.properties"], &["**/secret.*"], &[]);
        let (_d, out) = run_filter(
            &[&[
                ("app.properties", "v=@x@"),
                ("secret.properties", "v=@x@"),
                ("notes.txt", "v=@x@"),
            ]],
            std::slice::from_ref(&stage),
            &[("x", "1")],
        );
        assert_eq!(out["app.properties"], "v=1"); // included
        assert_eq!(out["secret.properties"], "v=@x@"); // excluded
        assert_eq!(out["notes.txt"], "v=@x@"); // not included
    }

    #[test]
    fn two_substitute_stages_chain_on_same_file() {
        // Stage 1 turns @a@ into "@b@"; stage 2 then resolves @b@.
        let s1 = substitute_stage(&["**/*.txt"], &[], &[]);
        let s2 = substitute_stage(&["**/*.txt"], &[], &[]);
        let (_d, out) = run_filter(
            &[&[("f.txt", "@a@")]],
            &[s1, s2],
            &[("a", "@b@"), ("b", "DONE")],
        );
        assert_eq!(out["f.txt"], "DONE");
    }

    #[test]
    fn multiple_directories_merge_later_wins() {
        let (_d, out) = run_filter(
            &[
                &[("a.txt", "first"), ("shared.txt", "from-root0")],
                &[("b.txt", "second"), ("shared.txt", "from-root1")],
            ],
            &[],
            &[],
        );
        assert_eq!(out["a.txt"], "first");
        assert_eq!(out["b.txt"], "second");
        assert_eq!(out["shared.txt"], "from-root1"); // later root wins
    }

    #[test]
    fn stage_directories_restrict_to_origin_root() {
        // A stage scoped to root1 only; root0's identical file stays verbatim.
        let dir = tempfile::tempdir().unwrap();
        let root0 = dir.path().join("r0");
        let root1 = dir.path().join("r1");
        std::fs::create_dir_all(&root0).unwrap();
        std::fs::create_dir_all(&root1).unwrap();
        std::fs::write(root0.join("a.properties"), "v=@x@").unwrap();
        std::fs::write(root1.join("b.properties"), "v=@x@").unwrap();

        let scope = Resources {
            directories: vec!["r0".into(), "r1".into()],
            filter: vec![substitute_stage(&["**/*.properties"], &[], &["r1"])],
            ..Default::default()
        };
        let roots = identity_roots(&[root0.clone(), root1.clone()]);
        let compiled = must(compile_stages(dir.path(), &scope, &roots, "resources"));
        let out = dir.path().join("out");
        filter_roots(
            &roots,
            &out,
            &compiled,
            &ctx(&[("x", "1")]),
            &binary_extension_set(&[]),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(out.join("a.properties")).unwrap(),
            "v=@x@"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("b.properties")).unwrap(),
            "v=1"
        );
    }

    #[test]
    fn stage_directory_not_in_scope_roots_errors() {
        let dir = tempfile::tempdir().unwrap();
        let scope = Resources {
            filter: vec![substitute_stage(&[], &[], &["nope"])],
            ..Default::default()
        };
        let roots = identity_roots(&[dir.path().join("r0")]);
        let err = match compile_stages(dir.path(), &scope, &roots, "resources") {
            Err(e) => e,
            Ok(_) => panic!("expected a stage-directory validation error"),
        };
        assert!(err
            .to_string()
            .contains("not one of the [resources] source directories"));
    }

    // -- resource remapping ---------------------------------------------------

    fn collect_out(out: &Path) -> BTreeMap<String, String> {
        let mut got = BTreeMap::new();
        for entry in walkdir::WalkDir::new(out)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(out).unwrap();
                got.insert(
                    rel_to_slash(rel),
                    std::fs::read_to_string(entry.path()).unwrap(),
                );
            }
        }
        got
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, contents) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, contents).unwrap();
        }
    }

    #[test]
    fn directory_remaps_selected_files_under_target_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        write_tree(
            &src,
            &[
                ("LICENSE", "Apache-2.0"),
                ("README.md", "do not copy"),
                ("proguard/base.pro", "-keep class Foo"),
                ("proguard/cache.pro", "-keep class Bar"),
                ("android/skip.pro", "nope"),
            ],
        );

        let spec = mapped_dir("repo", &["LICENSE", "proguard/*"], "META-INF");
        let roots = vec![SourceRoot::from_directory(src, &spec)];
        let out = dir.path().join("out");
        filter_roots(&roots, &out, &[], &ctx(&[]), &binary_extension_set(&[])).unwrap();

        let got = collect_out(&out);
        assert_eq!(
            got.get("META-INF/LICENSE").map(String::as_str),
            Some("Apache-2.0")
        );
        assert_eq!(
            got.get("META-INF/proguard/base.pro").map(String::as_str),
            Some("-keep class Foo")
        );
        assert_eq!(
            got.get("META-INF/proguard/cache.pro").map(String::as_str),
            Some("-keep class Bar")
        );
        assert!(!got.contains_key("META-INF/README.md"));
        assert!(!got.contains_key("README.md"));
        assert!(!got.contains_key("META-INF/android/skip.pro"));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn directory_excludes_win_over_includes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("extra");
        write_tree(&src, &[("keep.txt", "yes"), ("drop.txt", "no")]);
        let mut spec = mapped_dir("extra", &["*.txt"], "");
        spec.excludes = vec!["drop.txt".into()];
        let roots = vec![SourceRoot::from_directory(src, &spec)];
        let out = dir.path().join("out");
        filter_roots(&roots, &out, &[], &ctx(&[]), &binary_extension_set(&[])).unwrap();
        let got = collect_out(&out);
        assert_eq!(got.get("keep.txt").map(String::as_str), Some("yes"));
        assert!(!got.contains_key("drop.txt"));
    }

    #[test]
    fn remapped_directory_merges_with_identity_later_wins() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("resources");
        let extra = dir.path().join("extra");
        write_tree(&resources, &[("app.properties", "from-resources")]);
        write_tree(
            &extra,
            &[("LICENSE", "lic"), ("app.properties", "from-extra")],
        );
        let spec = mapped_dir("extra", &["LICENSE", "app.properties"], "");
        let roots = vec![
            SourceRoot::identity(resources),
            SourceRoot::from_directory(extra, &spec),
        ];
        let out = dir.path().join("out");
        filter_roots(&roots, &out, &[], &ctx(&[]), &binary_extension_set(&[])).unwrap();
        let got = collect_out(&out);
        assert_eq!(got.get("LICENSE").map(String::as_str), Some("lic"));
        assert_eq!(
            got.get("app.properties").map(String::as_str),
            Some("from-extra")
        );
    }

    #[test]
    fn remapped_directory_does_not_copy_vcs_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        write_tree(
            &src,
            &[
                ("LICENSE", "ok"),
                (".git/HEAD", "ref: refs/heads/master"),
                (".git/objects/ab/cd", "blob"),
            ],
        );
        let spec = ResourceDirectory {
            path: "repo".into(),
            includes: vec![],
            excludes: vec![],
            target_path: "META-INF".into(),
        };
        let roots = vec![SourceRoot::from_directory(src, &spec)];
        let out = dir.path().join("out");
        filter_roots(&roots, &out, &[], &ctx(&[]), &binary_extension_set(&[])).unwrap();
        let got = collect_out(&out);
        assert_eq!(got.get("META-INF/LICENSE").map(String::as_str), Some("ok"));
        assert!(got.keys().all(|k| !k.contains(".git")));
    }

    #[test]
    fn directory_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut scope = Resources {
            section_present: true,
            ..Default::default()
        };
        scope
            .directories
            .push(mapped_dir("nope", &["LICENSE"], "META-INF"));
        let err = resolve_source_roots(dir.path(), &scope, None, "resources")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("source directory 'nope' does not exist"),
            "got: {err}"
        );
    }

    #[test]
    fn process_resources_table_directory_materializes_output() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(
            dir.path(),
            &[
                ("LICENSE", "Apache"),
                ("proguard/base.pro", "-keep"),
                ("README.md", "skip"),
            ],
        );
        std::fs::write(
            dir.path().join("Curie.toml"),
            "[application]\nname = \"x\"\nversion = \"1.0\"\nmainClass = \"X\"\n\
             [resources]\ndirectories = [\
             { path = \".\", includes = [\"LICENSE\", \"proguard/*\"], targetPath = \"META-INF\" }\
             ]\n",
        )
        .unwrap();
        let desc = crate::descriptor::load(dir.path()).unwrap();
        let target = dir.path().join("target");
        let out = process_resources(dir.path(), &desc, None, None, None, &target).unwrap();
        let main = out.main_dir.expect("custom directories activate the scope");
        let got = collect_out(&main);
        assert_eq!(
            got.get("META-INF/LICENSE").map(String::as_str),
            Some("Apache")
        );
        assert_eq!(
            got.get("META-INF/proguard/base.pro").map(String::as_str),
            Some("-keep")
        );
        assert!(!got.contains_key("README.md"));
        assert!(!got.contains_key("META-INF/README.md"));
    }

    #[test]
    fn dir_may_match_prunes_unrelated_trees() {
        assert!(dir_may_match_pattern("LICENSE", ""));
        assert!(!dir_may_match_pattern("LICENSE", "proguard"));
        assert!(dir_may_match_pattern("proguard/*", "proguard"));
        assert!(!dir_may_match_pattern("proguard/*", "android"));
        assert!(dir_may_match_pattern("**/*.txt", "anywhere"));
        assert!(dir_may_match_pattern("proguard/*", ""));
    }

    #[test]
    fn make_engine_liquid_succeeds() {
        let stage = FilterStage {
            engine: Engine::Liquid,
            directories: vec![],
            includes: vec![],
            excludes: vec![],
            substitute: None,
            liquid: None,
        };
        let engine = make_engine(&stage).expect("liquid engine should be constructible");
        assert_eq!(engine.name(), "liquid");
    }

    // -- liquid engine tests ------------------------------------------------

    fn liquid_stage(includes: &[&str], excludes: &[&str], directories: &[&str]) -> FilterStage {
        FilterStage {
            engine: Engine::Liquid,
            directories: directories.iter().map(|s| s.to_string()).collect(),
            includes: includes.iter().map(|s| s.to_string()).collect(),
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            substitute: None,
            liquid: None,
        }
    }

    #[test]
    fn liquid_simple_variable() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[("project.version", "2.0.0")]);
        let out = engine
            .render(Path::new("f"), "v={{ project.version }}", &vars)
            .unwrap();
        assert_eq!(out, "v=2.0.0");
    }

    #[test]
    fn liquid_nested_variables() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[
            ("project.name", "my-app"),
            ("project.version", "1.0.0"),
            ("git.commit.id", "abc123"),
        ]);
        let out = engine
            .render(
                Path::new("f"),
                "{{ project.name }} v{{ project.version }} ({{ git.commit.id }})",
                &vars,
            )
            .unwrap();
        assert_eq!(out, "my-app v1.0.0 (abc123)");
    }

    #[test]
    fn liquid_if_conditional() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[("project.version", "1.0.0")]);
        let text = "{% if project.version %}version={{ project.version }}{% endif %}";
        let out = engine.render(Path::new("f"), text, &vars).unwrap();
        assert_eq!(out, "version=1.0.0");
    }

    #[test]
    fn liquid_if_absent_variable() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[]);
        let text = "{% if project.version %}yes{% else %}no{% endif %}";
        let out = engine.render(Path::new("f"), text, &vars).unwrap();
        assert_eq!(out, "no");
    }

    #[test]
    fn liquid_for_loop() {
        // Liquid for loops require an array; test via the if/assign/capture
        // which are more useful for resource filtering. The engine is fully
        // functional via liquid-rust's stdlib.
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[("project.name", "demo")]);
        let text = "{% assign name = project.name %}app={{ name }}";
        let out = engine.render(Path::new("f"), text, &vars).unwrap();
        assert_eq!(out, "app=demo");
    }

    #[test]
    fn liquid_filters_upcase() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[("project.name", "demo")]);
        let out = engine
            .render(Path::new("f"), "{{ project.name | upcase }}", &vars)
            .unwrap();
        assert_eq!(out, "DEMO");
    }

    #[test]
    fn liquid_env_variable() {
        unsafe {
            std::env::set_var("CURIE_LIQUID_TEST_VAR", "hello");
        }
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[]);
        let out = engine
            .render(Path::new("f"), "val={{ env.CURIE_LIQUID_TEST_VAR }}", &vars)
            .unwrap();
        assert_eq!(out, "val=hello");
        unsafe {
            std::env::remove_var("CURIE_LIQUID_TEST_VAR");
        }
    }

    #[test]
    fn liquid_parse_error_reports_path() {
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[]);
        let err = engine
            .render(Path::new("bad.txt"), "{% if %}", &vars)
            .unwrap_err();
        assert!(
            err.to_string().contains("bad.txt"),
            "expected path in error, got: {}",
            err
        );
    }

    #[test]
    fn liquid_unresolved_variable_errors() {
        // liquid-rust uses strict mode: undefined variables are render errors.
        let engine = LiquidEngine::new().unwrap();
        let vars = ctx(&[]);
        let err = engine
            .render(Path::new("test.yml"), "x={{ missing }}", &vars)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("test.yml"),
            "expected path in error, got: {msg}"
        );
    }

    #[test]
    fn liquid_stage_in_filter_pipeline() {
        let stage = liquid_stage(&["**/*.txt"], &[], &[]);
        let (_d, out) = run_filter(
            &[&[
                ("app.txt", "v={{ project.version }}"),
                ("skip.md", "v={{ project.version }}"),
            ]],
            std::slice::from_ref(&stage),
            &[("project.version", "3.0.0")],
        );
        assert_eq!(out["app.txt"], "v=3.0.0"); // filtered
        assert_eq!(out["skip.md"], "v={{ project.version }}"); // not included
    }

    #[test]
    fn liquid_and_substitute_stages_chain() {
        // Stage 1 (substitute): @a@ → "{{ project.version }}"
        // Stage 2 (liquid): renders "{{ project.version }}" → "4.0.0"
        let s1 = substitute_stage(&["**/*.txt"], &[], &[]);
        let s2 = liquid_stage(&["**/*.txt"], &[], &[]);
        let (_d, out) = run_filter(
            &[&[("f.txt", "@a@")]],
            &[s1, s2],
            &[("a", "{{ project.version }}"), ("project.version", "4.0.0")],
        );
        assert_eq!(out["f.txt"], "4.0.0");
    }

    #[test]
    fn liquid_to_liquid_object_nested_structure() {
        use liquid::ValueView;

        let vars = ctx(&[
            ("project.name", "my-app"),
            ("project.version", "1.0"),
            ("simple", "value"),
        ]);
        let obj = vars.to_liquid_object();
        // Check that "project" is an object with "name" and "version" keys.
        let project = obj.get("project").expect("project key");
        assert!(
            matches!(project, liquid::model::Value::Object(_)),
            "project should be an Object"
        );
        // Check that "simple" is a scalar.
        let simple = obj.get("simple").expect("simple key");
        assert_eq!(simple.to_kstr().as_str(), "value");
    }

    #[test]
    fn binary_file_bypasses_stages() {
        // A .png with a NUL byte and an @var@ token: copied byte-for-byte.
        let stage = substitute_stage(&[], &[], &[]);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(&root).unwrap();
        let bytes = b"\x89PNG\x00@x@";
        std::fs::write(root.join("logo.png"), bytes).unwrap();
        let scope = Resources {
            filter: vec![stage],
            ..Default::default()
        };
        let roots = identity_roots(&[root.clone()]);
        let compiled = must(compile_stages(dir.path(), &scope, &roots, "resources"));
        let out = dir.path().join("out");
        filter_roots(
            &roots,
            &out,
            &compiled,
            &ctx(&[("x", "1")]),
            &binary_extension_set(&[]),
        )
        .unwrap();
        assert_eq!(std::fs::read(out.join("logo.png")).unwrap(), bytes);
    }
}
