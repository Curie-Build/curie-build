//! `curie audit` — emit a CycloneDX 1.6 SBOM and check the dependency
//! closure against the OSV vulnerability database.
//!
//! # Behaviour
//!
//! 1. Resolve the production dependency closure (and optionally test deps).
//! 2. Emit `target/sbom.cdx.json` (CycloneDX 1.6 JSON).
//! 3. Unless `--offline`, POST the closure to `https://api.osv.dev/v1/querybatch`
//!    and collect any findings.
//! 4. If `--full` is set, fetch full vuln detail for each finding via
//!    `GET https://api.osv.dev/v1/vulns/{id}`.
//! 5. Print findings; exit 1 when the max CVSS score ≥ `--severity` threshold
//!    (default 7.0), or on any finding when `--full` is not set.

use crate::build::{central_repos, extra_repos};
use crate::descriptor::{self, Descriptor};
use anyhow::{bail, Context, Result};
use curie_deps::{DepEntry, ResolveOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for a single `curie audit` run.
#[derive(Debug, Clone)]
pub struct AuditOptions {
    /// Include test-scope deps in the SBOM and vulnerability scan.
    pub include_test: bool,
    /// Skip OSV network call; only emit the SBOM.
    pub offline: bool,
    /// Fetch full vuln detail from OSV (aliases, fixed versions, severity).
    /// When `false` (default) only the vuln ID is shown and exit 1 is
    /// triggered on any finding.
    pub full: bool,
    /// CVSS score threshold for a non-zero exit (default 7.0).
    /// Only meaningful when `--full` is set; ignored otherwise.
    pub severity: f32,
    /// Override the SBOM output path (default: `<project_root>/target/sbom.cdx.json`).
    pub output: Option<std::path::PathBuf>,
}

impl Default for AuditOptions {
    fn default() -> Self {
        AuditOptions {
            include_test: false,
            offline: false,
            full: false,
            severity: 7.0,
            output: None,
        }
    }
}

/// A single vulnerability finding for one component.
#[derive(Debug, Clone)]
pub struct Finding {
    /// PURL of the affected component.
    pub purl: String,
    /// OSV vulnerability ID (e.g. `GHSA-xxxx-yyyy-zzzz`).
    pub id: String,
    /// Summary line (only present when `--full` was requested).
    pub summary: Option<String>,
    /// Fixed-in versions (only present when `--full` was requested).
    pub fixed: Vec<String>,
    /// Numeric CVSS score (only present when `--full` was requested).
    pub score: Option<f32>,
}

/// Aggregated result of one audit run.
#[derive(Debug)]
pub struct AuditReport {
    /// Path where the SBOM was written.
    pub sbom_path: std::path::PathBuf,
    /// All findings, in the order returned by OSV.
    pub findings: Vec<Finding>,
    /// Maximum CVSS score across all findings (`None` when no findings or
    /// `--full` was not used).
    pub max_score: Option<f32>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full audit pipeline for one project directory.
pub fn run_audit(project_root: &Path, opts: &AuditOptions) -> Result<AuditReport> {
    let desc = descriptor::load(project_root)?;
    if desc.is_workspace() {
        bail!("`curie audit` cannot run on a workspace root; target a member with --project");
    }
    run_audit_with_desc(project_root, &desc, opts)
}

/// Run the audit pipeline given a pre-loaded descriptor.  Used by the
/// workspace fan-out path which loads descriptors once.
pub fn run_audit_with_desc(
    project_root: &Path,
    desc: &Descriptor,
    opts: &AuditOptions,
) -> Result<AuditReport> {
    // 1. Resolve dependency closure.
    let components = resolve_components(project_root, desc, opts)?;

    // 2. Emit SBOM.
    let sbom_path = opts.output.clone().unwrap_or_else(|| {
        project_root.join("target").join("sbom.cdx.json")
    });
    if let Some(parent) = sbom_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory {}", parent.display()))?;
    }
    let bom = build_bom(desc, &components);
    let json = serde_json::to_string_pretty(&bom).context("failed to serialise SBOM")?;
    std::fs::write(&sbom_path, json)
        .with_context(|| format!("failed to write SBOM to {}", sbom_path.display()))?;
    println!("  SBOM written    {}", sbom_path.display());

    // 3. Optionally scan against OSV.
    if opts.offline || components.is_empty() {
        return Ok(AuditReport { sbom_path, findings: vec![], max_score: None });
    }

    let raw_findings = osv_querybatch(&components)?;
    if raw_findings.is_empty() {
        println!("  No vulnerabilities found.");
        return Ok(AuditReport { sbom_path, findings: vec![], max_score: None });
    }

    // 4. Optionally enrich with full details.
    let findings = if opts.full {
        enrich_findings(raw_findings)?
    } else {
        raw_findings
    };

    // 5. Print findings.
    print_findings(&findings, opts.full);

    let max_score = findings.iter().filter_map(|f| f.score).reduce(f32::max);

    Ok(AuditReport { sbom_path, findings, max_score })
}

// ---------------------------------------------------------------------------
// Dependency resolution → component list
// ---------------------------------------------------------------------------

/// Scope of a dependency in the SBOM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepScope {
    Required, // production
    Optional, // test-only
}

/// A resolved component ready for SBOM + OSV.
#[derive(Debug, Clone)]
struct Component {
    group: String,
    artifact: String,
    version: String,
    scope: DepScope,
}

impl Component {
    fn purl(&self) -> String {
        format!(
            "pkg:maven/{}/{}@{}",
            self.group, self.artifact, self.version
        )
    }
}

fn resolve_components(
    project_root: &Path,
    desc: &Descriptor,
    opts: &AuditOptions,
) -> Result<Vec<Component>> {
    let opts_prod = ResolveOptions {
        default_repos: central_repos(),
        named_repos: extra_repos(desc),
        progress: false,
        bom_imports: desc.prod_bom_gavs()?,
        offline: opts.offline,
    };

    let prod_entries: Vec<DepEntry> = desc
        .dependencies
        .iter()
        .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository() })
        .collect();

    let mut components: Vec<Component> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    if !prod_entries.is_empty() {
        let tree = curie_deps::resolve_tree(&prod_entries, &opts_prod)
            .context("failed to resolve production dependencies")?;
        for dep in tree.resolved {
            let key = format!("{}:{}", dep.gav.group, dep.gav.artifact);
            if seen.insert(key) {
                components.push(Component {
                    group: dep.gav.group,
                    artifact: dep.gav.artifact,
                    version: dep.gav.version,
                    scope: DepScope::Required,
                });
            }
        }
    }

    if opts.include_test {
        let opts_test = ResolveOptions {
            default_repos: central_repos(),
            named_repos: extra_repos(desc),
            progress: false,
            bom_imports: desc.test_bom_gavs()?,
            offline: opts.offline,
        };
        let test_entries: Vec<DepEntry> = desc
            .test_dependencies
            .iter()
            .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository() })
            .collect();
        if !test_entries.is_empty() {
            let tree = curie_deps::resolve_tree(&test_entries, &opts_test)
                .context("failed to resolve test dependencies")?;
            for dep in tree.resolved {
                let key = format!("{}:{}", dep.gav.group, dep.gav.artifact);
                if seen.insert(key) {
                    components.push(Component {
                        group: dep.gav.group,
                        artifact: dep.gav.artifact,
                        version: dep.gav.version,
                        scope: DepScope::Optional,
                    });
                }
            }
        }
    }

    Ok(components)
}

// ---------------------------------------------------------------------------
// CycloneDX 1.6 JSON serialisation
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bom {
    bom_format: &'static str,
    spec_version: &'static str,
    serial_number: String,
    version: u32,
    metadata: BomMetadata,
    components: Vec<BomComponent>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BomMetadata {
    timestamp: String,
    tools: BomTools,
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<BomComponent>,
}

#[derive(Serialize)]
struct BomTools {
    components: Vec<BomToolEntry>,
}

#[derive(Serialize)]
struct BomToolEntry {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BomComponent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    name: String,
    version: String,
    purl: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
}

fn build_bom(desc: &Descriptor, components: &[Component]) -> Bom {
    let serial = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let timestamp = chrono_utc_now();

    // Metadata component is only emitted when groupId is known.
    let meta_component: Option<BomComponent> = desc.group_id().map(|gid| BomComponent {
        kind: if desc.is_library() { "library" } else { "application" },
        group: Some(gid.to_string()),
        name: desc.buildable_name().to_string(),
        version: desc.buildable_version().to_string(),
        purl: format!(
            "pkg:maven/{}/{}@{}",
            gid,
            desc.buildable_name(),
            desc.buildable_version(),
        ),
        scope: None,
    });

    let bom_components: Vec<BomComponent> = components
        .iter()
        .map(|c| BomComponent {
            kind: "library",
            group: Some(c.group.clone()),
            name: c.artifact.clone(),
            version: c.version.clone(),
            purl: c.purl(),
            scope: Some(match c.scope {
                DepScope::Required => "required",
                DepScope::Optional => "optional",
            }),
        })
        .collect();

    Bom {
        bom_format: "CycloneDX",
        spec_version: "1.6",
        serial_number: serial,
        version: 1,
        metadata: BomMetadata {
            timestamp,
            tools: BomTools {
                components: vec![BomToolEntry {
                    kind: "application",
                    name: "curie",
                    version: env!("CARGO_PKG_VERSION"),
                }],
            },
            component: meta_component,
        },
        components: bom_components,
    }
}

/// Return the current UTC time as an ISO-8601 string without pulling in
/// the `chrono` crate (which is not a declared dependency).
fn chrono_utc_now() -> String {
    // Use UNIX_EPOCH + SystemTime for a lightweight timestamp.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Hand-roll a minimal ISO-8601 timestamp (accuracy: 1 second).
    let s = secs;
    let min = s / 60;
    let hr = min / 60;
    let days = hr / 24;
    let sec = s % 60;
    let min = min % 60;
    let hr = hr % 24;
    // Days since epoch → calendar date (Gregorian).
    let (y, mo, d) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, hr, min, sec)
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    // (civil_from_days, shifted to epoch 1970-01-01).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// OSV API
// ---------------------------------------------------------------------------

const OSV_QUERYBATCH: &str = "https://api.osv.dev/v1/querybatch";
const OSV_VULNS: &str = "https://api.osv.dev/v1/vulns";
const OSV_BATCH_SIZE: usize = 1000;

// --- request types ---

#[derive(Serialize)]
struct OsvBatchRequest {
    queries: Vec<OsvQuery>,
}

#[derive(Serialize)]
struct OsvQuery {
    package: OsvPackage,
    // version is intentionally omitted: when querying by purl the version is
    // already encoded in the purl string; sending both causes OSV to return
    // HTTP 400 "version specified in params and PURL query".
}

#[derive(Serialize)]
struct OsvPackage {
    purl: String,
}

// --- response types ---

#[derive(Deserialize)]
struct OsvBatchResponse {
    #[serde(default)]
    results: Vec<OsvQueryResult>,
}

#[derive(Deserialize)]
struct OsvQueryResult {
    #[serde(default)]
    vulns: Vec<OsvVulnRef>,
}

#[derive(Deserialize)]
struct OsvVulnRef {
    id: String,
}

// Full vuln detail (used with --full).
#[derive(Deserialize)]
struct OsvFullVuln {
    id: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    affected: Vec<OsvAffected>,
    #[serde(default)]
    database_specific: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct OsvAffected {
    #[serde(default)]
    ranges: Vec<OsvRange>,
}

#[derive(Deserialize)]
struct OsvRange {
    #[serde(default)]
    events: Vec<OsvEvent>,
}

#[derive(Deserialize)]
struct OsvEvent {
    #[serde(default)]
    fixed: Option<String>,
}

/// Query OSV batch API and return raw (id-only) findings.
fn osv_querybatch(components: &[Component]) -> Result<Vec<Finding>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-audit/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut findings: Vec<Finding> = Vec::new();

    for chunk in components.chunks(OSV_BATCH_SIZE) {
        let queries: Vec<OsvQuery> = chunk
            .iter()
            .map(|c| OsvQuery {
                package: OsvPackage { purl: c.purl() },
            })
            .collect();

        let body = serde_json::to_string(&OsvBatchRequest { queries })
            .context("failed to serialise OSV request")?;

        let resp = client
            .post(OSV_QUERYBATCH)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .context("OSV querybatch request failed")?;

        if !resp.status().is_success() {
            bail!("OSV querybatch returned HTTP {}", resp.status());
        }

        let text = resp.text().context("failed to read OSV response body")?;
        let parsed: OsvBatchResponse =
            serde_json::from_str(&text).context("failed to parse OSV querybatch response")?;

        for (component, result) in chunk.iter().zip(parsed.results.iter()) {
            for vuln in &result.vulns {
                findings.push(Finding {
                    purl: component.purl(),
                    id: vuln.id.clone(),
                    summary: None,
                    fixed: vec![],
                    score: None,
                });
            }
        }
    }

    Ok(findings)
}

/// Fetch full OSV detail for each finding and populate `summary`, `fixed`,
/// and `score`.
fn enrich_findings(raw: Vec<Finding>) -> Result<Vec<Finding>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-audit/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut enriched: Vec<Finding> = Vec::new();

    for mut f in raw {
        let url = format!("{}/{}", OSV_VULNS, f.id);
        let resp = client
            .get(&url)
            .send()
            .with_context(|| format!("failed to fetch OSV vuln {}", f.id))?;

        if !resp.status().is_success() {
            // Best-effort: keep the finding without details.
            enriched.push(f);
            continue;
        }

        let text = resp.text().context("failed to read OSV vuln body")?;
        if let Ok(detail) = serde_json::from_str::<OsvFullVuln>(&text) {
            f.summary = detail.summary;
            f.score = extract_score(&detail.database_specific);
            f.fixed = detail
                .affected
                .iter()
                .flat_map(|a| a.ranges.iter())
                .flat_map(|r| r.events.iter())
                .filter_map(|e| e.fixed.clone())
                .collect();
            f.fixed.sort();
            f.fixed.dedup();
        }
        enriched.push(f);
    }

    Ok(enriched)
}

/// Extract a numeric CVSS score from the `database_specific` map.
/// GHSA advisories carry a `severity` string; we map it to a numeric value.
fn extract_score(db: &BTreeMap<String, serde_json::Value>) -> Option<f32> {
    if let Some(serde_json::Value::String(s)) = db.get("severity") {
        return Some(match s.to_uppercase().as_str() {
            "CRITICAL" => 9.0,
            "HIGH" => 7.0,
            "MEDIUM" => 4.0,
            "LOW" => 1.0,
            _ => return None,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_findings(findings: &[Finding], full: bool) {
    println!("  {} vulnerability finding(s):", findings.len());
    for f in findings {
        if full {
            let score_str = f
                .score
                .map(|s| format!(" (CVSS {:.1})", s))
                .unwrap_or_default();
            let summary = f.summary.as_deref().unwrap_or("no summary");
            println!("    [{}]{} {} — {}", f.id, score_str, f.purl, summary);
            if !f.fixed.is_empty() {
                println!("      Fixed in: {}", f.fixed.join(", "));
            }
        } else {
            println!("    [{}] {}", f.id, f.purl);
        }
    }
}

// ---------------------------------------------------------------------------
// Exit-code decision
// ---------------------------------------------------------------------------

/// Returns `true` when the audit result should cause a non-zero exit.
///
/// * Without `--full`: any finding → exit 1 (we cannot score IDs alone).
/// * With `--full`: exit 1 when max CVSS ≥ threshold.
pub fn should_exit_nonzero(report: &AuditReport, opts: &AuditOptions) -> bool {
    if report.findings.is_empty() {
        return false;
    }
    if !opts.full {
        // No scores available; be conservative.
        return true;
    }
    match report.max_score {
        Some(s) => s >= opts.severity,
        // Unknown severity → be conservative.
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- timestamp -----------------------------------------------------------

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-03-15 is day 19,797 since epoch (pre-computed).
        // We compute it: 54 years * 365 + leap days …
        // Easier: just verify the round-trip is self-consistent by checking
        // two known dates from a table.
        // 2000-01-01 = 10957 days since epoch
        assert_eq!(days_to_ymd(10957), (2000, 1, 1));
        // 2024-01-01 = 19723 days since epoch
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }

    // -- purl ----------------------------------------------------------------

    #[test]
    fn purl_format() {
        let c = Component {
            group: "org.example".into(),
            artifact: "my-lib".into(),
            version: "1.2.3".into(),
            scope: DepScope::Required,
        };
        assert_eq!(c.purl(), "pkg:maven/org.example/my-lib@1.2.3");
    }

    // -- bom serialisation ---------------------------------------------------

    #[test]
    fn bom_has_correct_format_and_spec() {
        let desc = make_desc(Some("com.example"), "my-app", "1.0.0");
        let bom = build_bom(&desc, &[]);
        assert_eq!(bom.bom_format, "CycloneDX");
        assert_eq!(bom.spec_version, "1.6");
        assert!(bom.serial_number.starts_with("urn:uuid:"));
    }

    #[test]
    fn bom_metadata_component_present_when_group_id_known() {
        let desc = make_desc(Some("com.example"), "my-app", "1.0.0");
        let bom = build_bom(&desc, &[]);
        let meta = bom.metadata.component.expect("should have metadata component");
        assert_eq!(meta.group.as_deref(), Some("com.example"));
        assert_eq!(meta.name, "my-app");
        assert_eq!(meta.version, "1.0.0");
        assert_eq!(meta.purl, "pkg:maven/com.example/my-app@1.0.0");
    }

    #[test]
    fn bom_metadata_component_absent_when_no_group_id() {
        let desc = make_desc(None, "my-lib", "0.1.0");
        let bom = build_bom(&desc, &[]);
        assert!(bom.metadata.component.is_none());
    }

    #[test]
    fn bom_components_include_scope() {
        let desc = make_desc(Some("com.example"), "app", "1.0.0");
        let components = vec![
            Component {
                group: "org.foo".into(),
                artifact: "bar".into(),
                version: "2.0".into(),
                scope: DepScope::Required,
            },
            Component {
                group: "org.test".into(),
                artifact: "junit".into(),
                version: "5.0".into(),
                scope: DepScope::Optional,
            },
        ];
        let bom = build_bom(&desc, &components);
        assert_eq!(bom.components.len(), 2);
        assert_eq!(bom.components[0].scope, Some("required"));
        assert_eq!(bom.components[1].scope, Some("optional"));
    }

    #[test]
    fn bom_serialises_to_valid_json() {
        let desc = make_desc(Some("com.example"), "app", "1.0.0");
        let bom = build_bom(&desc, &[]);
        let json = serde_json::to_string(&bom).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["bomFormat"], "CycloneDX");
        assert_eq!(v["specVersion"], "1.6");
        assert!(v["serialNumber"].as_str().unwrap().starts_with("urn:uuid:"));
    }

    // -- score extraction ---------------------------------------------------

    #[test]
    fn extract_score_critical() {
        let mut m = BTreeMap::new();
        m.insert("severity".into(), serde_json::Value::String("CRITICAL".into()));
        assert_eq!(extract_score(&m), Some(9.0));
    }

    #[test]
    fn extract_score_high() {
        let mut m = BTreeMap::new();
        m.insert("severity".into(), serde_json::Value::String("HIGH".into()));
        assert_eq!(extract_score(&m), Some(7.0));
    }

    #[test]
    fn extract_score_medium() {
        let mut m = BTreeMap::new();
        m.insert("severity".into(), serde_json::Value::String("MEDIUM".into()));
        assert_eq!(extract_score(&m), Some(4.0));
    }

    #[test]
    fn extract_score_low() {
        let mut m = BTreeMap::new();
        m.insert("severity".into(), serde_json::Value::String("LOW".into()));
        assert_eq!(extract_score(&m), Some(1.0));
    }

    #[test]
    fn extract_score_unknown_returns_none() {
        let mut m = BTreeMap::new();
        m.insert("severity".into(), serde_json::Value::String("MODERATE".into()));
        assert_eq!(extract_score(&m), None);
    }

    // -- exit-code logic ----------------------------------------------------

    #[test]
    fn no_findings_never_exits_nonzero() {
        let report = AuditReport {
            sbom_path: std::path::PathBuf::from("sbom.cdx.json"),
            findings: vec![],
            max_score: None,
        };
        let opts = AuditOptions { full: true, severity: 7.0, ..Default::default() };
        assert!(!should_exit_nonzero(&report, &opts));
    }

    #[test]
    fn finding_without_full_always_exits_nonzero() {
        let report = AuditReport {
            sbom_path: std::path::PathBuf::from("sbom.cdx.json"),
            findings: vec![Finding {
                purl: "pkg:maven/a/b@1".into(),
                id: "GHSA-xxxx".into(),
                summary: None,
                fixed: vec![],
                score: None,
            }],
            max_score: None,
        };
        let opts = AuditOptions { full: false, severity: 7.0, ..Default::default() };
        assert!(should_exit_nonzero(&report, &opts));
    }

    #[test]
    fn finding_below_threshold_with_full_does_not_exit() {
        let report = AuditReport {
            sbom_path: std::path::PathBuf::from("sbom.cdx.json"),
            findings: vec![Finding {
                purl: "pkg:maven/a/b@1".into(),
                id: "GHSA-xxxx".into(),
                summary: None,
                fixed: vec![],
                score: Some(4.0),
            }],
            max_score: Some(4.0),
        };
        let opts = AuditOptions { full: true, severity: 7.0, ..Default::default() };
        assert!(!should_exit_nonzero(&report, &opts));
    }

    #[test]
    fn finding_at_threshold_exits_nonzero() {
        let report = AuditReport {
            sbom_path: std::path::PathBuf::from("sbom.cdx.json"),
            findings: vec![Finding {
                purl: "pkg:maven/a/b@1".into(),
                id: "GHSA-xxxx".into(),
                summary: None,
                fixed: vec![],
                score: Some(7.0),
            }],
            max_score: Some(7.0),
        };
        let opts = AuditOptions { full: true, severity: 7.0, ..Default::default() };
        assert!(should_exit_nonzero(&report, &opts));
    }

    // -- helpers ------------------------------------------------------------

    fn make_desc(group_id: Option<&str>, name: &str, version: &str) -> Descriptor {
        use crate::descriptor::*;
        use std::collections::BTreeMap;
        Descriptor {
            kind: DescriptorKind::Library(Library {
                name: name.into(),
                version: version.into(),
                group_id: group_id.map(String::from),
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            spock: Spock::default(),
            native_image: NativeImage::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            dependencies: BTreeMap::new(),
            test_dependencies: BTreeMap::new(),
            repositories: vec![],
            bom_imports: BTreeMap::new(),
            test_bom_imports: BTreeMap::new(),
            inherited_bom_imports: BTreeMap::new(),
            inherited_test_bom_imports: BTreeMap::new(),
            workspace_dependencies: BTreeMap::new(),
            annotation_processors: BTreeMap::new(),
            test_annotation_processors: BTreeMap::new(),
            inherited_annotation_processors: BTreeMap::new(),
            inherited_test_annotation_processors: BTreeMap::new(),
            annotation_processor_options: BTreeMap::new(),
            test_annotation_processor_options: BTreeMap::new(),
            inherited_annotation_processor_options: BTreeMap::new(),
            inherited_test_annotation_processor_options: BTreeMap::new(),
            publish: PublishConfig::default(),
        }
    }
}
