//! `curie update` — check every versioned dependency against Maven metadata
//! and update `Curie.toml` in-place with the latest stable releases.
//!
//! # Behaviour
//!
//! 1. Collect all versioned entries from five TOML sections:
//!    `[dependencies]`, `[test-dependencies]` (unless `--no-test`),
//!    `[bom-imports]`, `[test-bom-imports]` (unless `--no-test`),
//!    and `[annotation-processors]`.
//!    Entries with an empty version string (BOM-managed) are skipped.
//! 2. For each entry, fetch `maven-metadata.xml` from the appropriate
//!    repository and find the latest *stable* release (no SNAPSHOT, alpha,
//!    beta, RC, CR, M\d, or milestone suffixes).
//! 3. Print a table comparing current vs available version.
//! 4. Unless `--check`, rewrite `Curie.toml` in-place using `toml_edit` so
//!    that comments and formatting are preserved.
//! 5. In `--check` mode exit 1 when any updates are available.

use crate::build::{central_repos, extra_repos};
use crate::descriptor::{self, Descriptor};
use anyhow::{Context, Result};
use curie_deps::repo::Repository;
use std::path::Path;
use toml_edit::DocumentMut;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for a single `curie update` run.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Skip the OSV network call; print nothing and exit cleanly.
    pub offline: bool,
    /// Report available updates but do not rewrite `Curie.toml`.
    /// Exit 1 when any updates are available.
    pub check: bool,
    /// Include `[test-dependencies]` and `[test-bom-imports]`.
    pub include_test: bool,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        UpdateOptions { offline: false, check: false, include_test: true }
    }
}

/// Result of a single `curie update` run (one project / workspace member).
#[derive(Debug)]
pub struct UpdateReport {
    /// Every versioned entry that was checked, with its outcome.
    pub entries: Vec<UpdateEntry>,
}

impl UpdateReport {
    /// `true` when at least one entry has a newer version available.
    pub fn has_updates(&self) -> bool {
        self.entries.iter().any(|e| e.latest.as_deref() != Some(e.current.as_str())
            && e.latest.is_some())
    }
}

/// One dependency entry and the outcome of checking it.
#[derive(Debug)]
pub struct UpdateEntry {
    /// Fully-qualified `group:artifact` coordinate.
    pub coord: String,
    /// Version as declared in `Curie.toml`.
    pub current: String,
    /// Latest stable version found in the repository, or `None` when the
    /// fetch failed or no stable version exists.
    pub latest: Option<String>,
    /// Which TOML section this entry came from (for display purposes).
    pub section: &'static str,
}

/// Top-level entry point used by the standalone (non-workspace) path.
pub fn run_update(project_root: &Path, opts: &UpdateOptions) -> Result<UpdateReport> {
    let desc = descriptor::load(project_root)?;
    run_update_with_desc(project_root, &desc, opts)
}

/// Entry point used by both the standalone path and workspace fan-out.
pub fn run_update_with_desc(
    project_root: &Path,
    desc: &Descriptor,
    opts: &UpdateOptions,
) -> Result<UpdateReport> {
    if opts.offline {
        println!("  offline mode — skipping update check");
        return Ok(UpdateReport { entries: vec![] });
    }

    let default_repos = central_repos();
    let named_repos = extra_repos(desc);

    // -----------------------------------------------------------------------
    // 1. Collect all versioned entries.
    // -----------------------------------------------------------------------
    let mut items: Vec<DepItem> = Vec::new();

    // [bom-imports]
    for (coord, version) in &desc.bom_imports {
        if !version.is_empty() {
            items.push(DepItem { coord: coord.clone(), version: version.clone(),
                repo_id: None, section: "bom-imports" });
        }
    }
    // [test-bom-imports]
    if opts.include_test {
        for (coord, version) in &desc.test_bom_imports {
            if !version.is_empty() {
                items.push(DepItem { coord: coord.clone(), version: version.clone(),
                    repo_id: None, section: "test-bom-imports" });
            }
        }
    }
    // [dependencies]
    for (coord, val) in &desc.dependencies {
        let v = val.version();
        if !v.is_empty() {
            items.push(DepItem { coord: coord.clone(), version: v.to_string(),
                repo_id: val.repository().map(str::to_string), section: "dependencies" });
        }
    }
    // [test-dependencies]
    if opts.include_test {
        for (coord, val) in &desc.test_dependencies {
            let v = val.version();
            if !v.is_empty() {
                items.push(DepItem { coord: coord.clone(), version: v.to_string(),
                    repo_id: val.repository().map(str::to_string), section: "test-dependencies" });
            }
        }
    }
    // [annotation-processors]
    for (coord, ap) in &desc.annotation_processors {
        let v = ap.version();
        if !v.is_empty() {
            items.push(DepItem { coord: coord.clone(), version: v.to_string(),
                repo_id: None, section: "annotation-processors" });
        }
    }

    if items.is_empty() {
        println!("  no versioned dependencies to check");
        return Ok(UpdateReport { entries: vec![] });
    }

    println!("  Checking updates for {} versioned dependenc{}…",
        items.len(), if items.len() == 1 { "y" } else { "ies" });

    // -----------------------------------------------------------------------
    // 2. Fetch maven-metadata.xml for each item.
    // -----------------------------------------------------------------------
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-update/0.1")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut entries: Vec<UpdateEntry> = Vec::new();

    for item in &items {
        let repo_url = resolve_repo_url(&item.repo_id, &named_repos, &default_repos);
        let latest = fetch_latest_stable(&client, &repo_url, &item.coord);
        entries.push(UpdateEntry {
            coord: item.coord.clone(),
            current: item.version.clone(),
            latest,
            section: item.section,
        });
    }

    // -----------------------------------------------------------------------
    // 3. Print table.
    // -----------------------------------------------------------------------
    print_update_table(&entries);

    // -----------------------------------------------------------------------
    // 4. Rewrite Curie.toml (unless --check).
    // -----------------------------------------------------------------------
    if !opts.check {
        let updated = entries.iter().filter(|e| {
            e.latest.as_deref().map(|l| l != e.current.as_str()).unwrap_or(false)
        }).count();
        if updated > 0 {
            rewrite_toml(project_root, &entries)?;
            println!("  {} update(s) applied to Curie.toml", updated);
        }
    } else {
        let available = entries.iter().filter(|e| {
            e.latest.as_deref().map(|l| l != e.current.as_str()).unwrap_or(false)
        }).count();
        if available > 0 {
            println!("  {} update(s) available — re-run without --check to apply", available);
        }
    }

    Ok(UpdateReport { entries })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct DepItem {
    coord: String,
    version: String,
    repo_id: Option<String>,
    section: &'static str,
}

/// Pick the base URL for a dep: named-repo override → first default repo.
fn resolve_repo_url(
    repo_id: &Option<String>,
    named: &[Repository],
    default: &[Repository],
) -> String {
    if let Some(id) = repo_id {
        if let Some(r) = named.iter().find(|r| &r.id == id) {
            return r.url.trim_end_matches('/').to_string();
        }
    }
    default
        .first()
        .map(|r| r.url.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://repo1.maven.org/maven2".to_string())
}

/// Construct the `maven-metadata.xml` URL for a `group:artifact` coordinate.
fn metadata_url(repo_base: &str, coord: &str) -> Option<String> {
    let (group, artifact) = coord.split_once(':')?;
    let group_path = group.replace('.', "/");
    Some(format!("{}/{}/{}/maven-metadata.xml", repo_base, group_path, artifact))
}

/// Fetch `maven-metadata.xml` and return the latest stable version, or `None`
/// on any error (network failure, parse error, no stable versions).
fn fetch_latest_stable(
    client: &reqwest::blocking::Client,
    repo_base: &str,
    coord: &str,
) -> Option<String> {
    let url = metadata_url(repo_base, coord)?;
    let body = client.get(&url).send().ok()?.text().ok()?;
    let versions = parse_versions(&body);
    latest_stable(&versions)
}

/// Extract all `<version>…</version>` values from a `maven-metadata.xml` body.
fn parse_versions(xml: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        rest = &rest[start + "<version>".len()..];
        if let Some(end) = rest.find("</version>") {
            versions.push(rest[..end].trim().to_string());
            rest = &rest[end + "</version>".len()..];
        }
    }
    versions
}

/// Return the highest stable version from a list, or `None` if none qualify.
pub fn latest_stable(versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter(|v| is_stable(v))
        .max_by(|a, b| version_cmp(a, b))
        .cloned()
}

/// Returns `true` when the version string looks like a stable release.
///
/// Rejects anything containing (case-insensitively):
/// `SNAPSHOT`, `alpha`, `beta`, `rc`, `cr`, `milestone`,
/// or a `-M<digits>` / `.M<digits>` milestone suffix.
pub fn is_stable(version: &str) -> bool {
    let v = version.to_ascii_lowercase();
    if v.contains("snapshot") { return false; }
    if v.contains("alpha")    { return false; }
    if v.contains("beta")     { return false; }
    if v.contains("milestone") { return false; }
    // rc / cr as whole token (bounded by non-alpha or end), e.g. "3.0.0.rc1",
    // "1.0-RC2", "1.0.CR3" — but not "source" which contains no standalone rc.
    if contains_token(&v, "rc") { return false; }
    if contains_token(&v, "cr") { return false; }
    // Maven milestone: "-M1", ".M12", etc.
    if is_maven_milestone(&v) { return false; }
    true
}

/// `true` when `v` contains `token` bounded on both sides by non-alpha chars
/// (or start/end of string), optionally followed by digits.  So `rc`, `rc1`,
/// `rc12` all match the token `rc`; but `source` does NOT match `rc` because
/// `rc` there is preceded by `u` (alphabetic).
fn contains_token(v: &str, token: &str) -> bool {
    let bytes = v.as_bytes();
    let tlen = token.len();
    let vlen = bytes.len();
    let tbytes = token.as_bytes();
    let mut i = 0usize;
    while i + tlen <= vlen {
        if bytes[i..i + tlen] == *tbytes {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphabetic();
            // Right boundary: end-of-string, non-alphanumeric, OR digits
            // (digits indicate rc1/rc2 etc — still a release candidate).
            // Only pure alpha continuation (e.g. "rcfoo") is rejected.
            let right_ok = i + tlen == vlen
                || !bytes[i + tlen].is_ascii_alphabetic();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `true` when the version contains a Maven milestone suffix like `-M1`, `.M3`.
fn is_maven_milestone(v: &str) -> bool {
    let bytes = v.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if (bytes[i] == b'-' || bytes[i] == b'.') && i + 1 < len && bytes[i + 1] == b'm' {
            // check that the rest is digits
            let rest = &bytes[i + 2..];
            if !rest.is_empty() && rest.iter().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Lexicographic-numeric version comparison: split on `.` and `-`, compare
/// numeric parts numerically and string parts lexicographically.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa = version_parts(a);
    let pb = version_parts(b);
    for (x, y) in pa.iter().zip(pb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(n), Ok(m)) => n.cmp(&m),
            _ => x.as_str().cmp(y.as_str()),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    pa.len().cmp(&pb.len())
}

fn version_parts(v: &str) -> Vec<String> {
    v.split(|c| c == '.' || c == '-').map(str::to_string).collect()
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

fn print_update_table(entries: &[UpdateEntry]) {
    // Determine column widths.
    let coord_w  = entries.iter().map(|e| e.coord.len()).max().unwrap_or(10);
    let cur_w    = entries.iter().map(|e| e.current.len()).max().unwrap_or(7);

    let use_color = use_color();

    let mut any_shown = false;
    for e in entries {
        let latest = match &e.latest {
            None    => continue,   // fetch failed — skip silently
            Some(l) => l,
        };
        let up_to_date = latest == &e.current;
        if up_to_date {
            continue; // only print entries that have updates
        }
        any_shown = true;
        let arrow = if use_color { "\x1b[32m→\x1b[0m" } else { "→" };
        let new_ver = if use_color {
            format!("\x1b[32m{}\x1b[0m", latest)
        } else {
            latest.clone()
        };
        println!("  {:<coord_w$}  {:<cur_w$}  {}  {}",
            e.coord, e.current, arrow, new_ver,
            coord_w = coord_w, cur_w = cur_w);
    }
    if !any_shown {
        println!("  all dependencies are up to date");
    }
}

fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    #[cfg(unix)]
    {
        extern "C" { fn isatty(fd: i32) -> i32; }
        unsafe { isatty(1) != 0 }
    }
    #[cfg(not(unix))]
    { false }
}

// ---------------------------------------------------------------------------
// Curie.toml rewrite via toml_edit
// ---------------------------------------------------------------------------

/// Rewrite the version strings in `<project_root>/Curie.toml` for every
/// entry in `updates` that has a newer `latest` version.
fn rewrite_toml(project_root: &Path, entries: &[UpdateEntry]) -> Result<()> {
    let toml_path = project_root.join("Curie.toml");
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("failed to read {}", toml_path.display()))?;
    let mut doc: DocumentMut = content.parse()
        .with_context(|| format!("failed to parse {} as TOML", toml_path.display()))?;

    for entry in entries {
        let new_ver = match &entry.latest {
            Some(v) if v != &entry.current => v.as_str(),
            _ => continue,
        };
        update_version_in_doc(&mut doc, entry.section, &entry.coord, new_ver);
    }

    std::fs::write(&toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", toml_path.display()))?;
    Ok(())
}

/// Find the `coord` key in the given TOML `section` table and update its
/// version to `new_ver`, handling both the shorthand string form and the
/// inline-table detailed form.
fn update_version_in_doc(
    doc: &mut DocumentMut,
    section: &str,
    coord: &str,
    new_ver: &str,
) {
    let table = match doc.get_mut(section).and_then(|v| v.as_table_mut()) {
        Some(t) => t,
        None => return,
    };
    let item = match table.get_mut(coord) {
        Some(i) => i,
        None => return,
    };

    // Shorthand: `"group:artifact" = "1.0.0"`
    if let Some(s) = item.as_str() {
        if !s.is_empty() {
            *item = toml_edit::value(new_ver);
        }
        return;
    }

    // Detailed: `"group:artifact" = { version = "1.0.0", ... }`
    if let Some(tbl) = item.as_value_mut().and_then(|v| v.as_inline_table_mut()) {
        if let Some(ver_item) = tbl.get_mut("version") {
            if let Some(s) = ver_item.as_str() {
                if !s.is_empty() {
                    *ver_item = toml_edit::Value::from(new_ver);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- stability filter ---------------------------------------------------

    #[test]
    fn is_stable_release_true() {
        assert!(is_stable("1.2.3"));
        assert!(is_stable("2.17.2"));
        assert!(is_stable("3.3.4"));
        assert!(is_stable("42.7.11"));
        assert!(is_stable("11.0.22"));
        // Groovy-style: stable
        assert!(is_stable("2.4-groovy-5.0"));
    }

    #[test]
    fn is_stable_snapshot_false() {
        assert!(!is_stable("1.0.0-SNAPSHOT"));
        assert!(!is_stable("2.5.0.SNAPSHOT"));
        assert!(!is_stable("3.0-snapshot"));
    }

    #[test]
    fn is_stable_alpha_false() {
        assert!(!is_stable("1.0.0-alpha1"));
        assert!(!is_stable("2.0.0.Alpha2"));
        assert!(!is_stable("3.0-ALPHA"));
    }

    #[test]
    fn is_stable_beta_false() {
        assert!(!is_stable("1.0-beta1"));
        assert!(!is_stable("2.0.0.Beta3"));
    }

    #[test]
    fn is_stable_rc_false() {
        assert!(!is_stable("1.0.0.RC1"));
        assert!(!is_stable("2.0-rc2"));
        assert!(!is_stable("3.0.0.CR1"));
        assert!(!is_stable("4.0-cr3"));
    }

    #[test]
    fn is_stable_milestone_false() {
        assert!(!is_stable("1.0.0-M1"));
        assert!(!is_stable("2.0.0.M12"));
        assert!(!is_stable("3.0-milestone1"));
        assert!(!is_stable("3.0-milestone-1"));
    }

    #[test]
    fn is_stable_does_not_falsely_reject() {
        // "source" contains "rc" as substring but not as a token
        assert!(is_stable("1.0-sources"));
        // "increment" contains "cr" but not as a token
        assert!(is_stable("1.0-incremental"));
        // "micro" starts with "m" but isn't -M\d
        assert!(is_stable("4.0.0.micro"));
    }

    // -- latest_stable ------------------------------------------------------

    #[test]
    fn latest_stable_picks_highest() {
        let vs: Vec<String> = ["1.0.0", "1.2.0", "1.1.5"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(latest_stable(&vs).as_deref(), Some("1.2.0"));
    }

    #[test]
    fn latest_stable_skips_snapshot() {
        let vs: Vec<String> = ["1.0.0", "1.1.0-SNAPSHOT", "1.0.9"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(latest_stable(&vs).as_deref(), Some("1.0.9"));
    }

    #[test]
    fn latest_stable_all_unstable_returns_none() {
        let vs: Vec<String> = ["1.0.0-RC1", "1.1.0-SNAPSHOT"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(latest_stable(&vs), None);
    }

    // -- TOML rewrite -------------------------------------------------------

    #[test]
    fn toml_rewrite_shorthand_version() {
        let toml = r#"
[dependencies]
"com.example:foo" = "1.0.0"
"#;
        let mut doc: DocumentMut = toml.parse().unwrap();
        update_version_in_doc(&mut doc, "dependencies", "com.example:foo", "2.0.0");
        let out = doc.to_string();
        assert!(out.contains("\"2.0.0\""), "got: {}", out);
        assert!(!out.contains("\"1.0.0\""), "got: {}", out);
    }

    #[test]
    fn toml_rewrite_detailed_version() {
        let toml = r#"
[dependencies]
"net.example:bar" = { version = "1.5.0", repository = "my-repo" }
"#;
        let mut doc: DocumentMut = toml.parse().unwrap();
        update_version_in_doc(&mut doc, "dependencies", "net.example:bar", "2.0.0");
        let out = doc.to_string();
        assert!(out.contains("\"2.0.0\""), "got: {}", out);
        assert!(!out.contains("\"1.5.0\""), "got: {}", out);
        // repository field must be preserved
        assert!(out.contains("repository"), "got: {}", out);
    }

    #[test]
    fn toml_rewrite_skips_empty_version() {
        let toml = r#"
[dependencies]
"org.example:managed" = ""
"#;
        let mut doc: DocumentMut = toml.parse().unwrap();
        // should be a no-op: empty string is BOM-managed
        update_version_in_doc(&mut doc, "dependencies", "org.example:managed", "1.0.0");
        let out = doc.to_string();
        // the value should still be empty (our guard: only update non-empty)
        assert!(out.contains("= \"\""), "got: {}", out);
    }

    #[test]
    fn toml_rewrite_preserves_comments() {
        let toml = "# top comment\n[bom-imports]\n# bom comment\n\"org.example:bom\" = \"1.0\"\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        update_version_in_doc(&mut doc, "bom-imports", "org.example:bom", "2.0");
        let out = doc.to_string();
        assert!(out.contains("# top comment"), "got: {}", out);
        assert!(out.contains("# bom comment"), "got: {}", out);
        assert!(out.contains("\"2.0\""), "got: {}", out);
    }

    // -- parse_versions -----------------------------------------------------

    #[test]
    fn parse_versions_extracts_all() {
        let xml = "<versioning><versions>\
            <version>1.0</version><version>1.1</version><version>2.0-SNAPSHOT</version>\
            </versions></versioning>";
        let vs = parse_versions(xml);
        assert_eq!(vs, vec!["1.0", "1.1", "2.0-SNAPSHOT"]);
    }

    // -- version_cmp --------------------------------------------------------

    #[test]
    fn version_cmp_numeric() {
        assert_eq!(version_cmp("1.10.0", "1.9.0"), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp("2.0.0", "1.99.99"), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp("1.0.0", "1.0.0"), std::cmp::Ordering::Equal);
    }
}
