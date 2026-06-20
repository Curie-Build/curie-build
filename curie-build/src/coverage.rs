//! JaCoCo-based code coverage support.
//!
//! When coverage is enabled (via `[test] coverage = true` in Curie.toml or
//! `curie test --coverage`), the test runner:
//!
//!   1. Resolves the JaCoCo agent JAR from Maven Central.
//!   2. Attaches it as `-javaagent:<path>=destfile=<exec>` to the test JVM.
//!   3. After tests pass, resolves the JaCoCo CLI JAR and generates an
//!      HTML + CSV report under `target/coverage/`.
//!   4. Prints a short summary (line/branch coverage %) and a link to the
//!      HTML report.

use anyhow::{Context, Result};
use curie_deps::resolver::{resolve, DepEntry, ResolveOptions};
use std::path::{Path, PathBuf};

/// Default JaCoCo version resolved from Maven Central.
pub const DEFAULT_JACOCO_VERSION: &str = "0.8.13";

/// Maven coordinate for the JaCoCo agent JAR.
const JACOCO_AGENT_COORD: &str = "org.jacoco:org.jacoco.agent";

/// Maven coordinate for the JaCoCo CLI (used for report generation).
const JACOCO_CLI_COORD: &str = "org.jacoco:org.jacoco.cli";

/// Resolve the JaCoCo agent JAR using the `runtime` classifier.
///
/// Requesting the classifier directly via DepEntry produces the shaded
/// agent JAR that can be used immediately with `-javaagent:` (it contains
/// the Premain-Class in its own manifest).
pub fn resolve_agent_jar(
    default_repos: &[curie_deps::repo::Repository],
    extra_repos: &[curie_deps::repo::Repository],
    offline: bool,
) -> Result<PathBuf> {
    let jars = resolve(
        &[DepEntry {
            key: JACOCO_AGENT_COORD,
            version: DEFAULT_JACOCO_VERSION,
            repo_id: None,
            exclusions: vec![],
            classifier: Some("runtime"),
            allow_version_conflict: false,
        }],
        &ResolveOptions {
            default_repos: default_repos.to_vec(),
            named_repos: extra_repos.to_vec(),
            progress: false,
            bom_imports: vec![],
            offline,
            skip_version_ranges: false, error_on_version_conflict: false,
        },
    )
    .context("failed to resolve JaCoCo agent")?;

    select_jar_from_resolve(&jars, "jacoco.agent", "runtime")
        .with_context(|| "JaCoCo agent JAR not found after resolution")
}

/// Small helper: prefer a jar whose name contains both the coord fragment
/// and the classifier; fall back to any jar containing the coord fragment.
/// Keeps the two resolve_*_jar functions short and descriptive.
fn select_jar_from_resolve(
    jars: &[std::path::PathBuf],
    coord_fragment: &str,
    classifier: &str,
) -> Result<std::path::PathBuf> {
    jars.iter()
        .find(|p| jar_filename_matches(p, coord_fragment, Some(classifier)))
        .or_else(|| jars.iter().find(|p| jar_filename_matches(p, coord_fragment, None)))
        .cloned()
        .with_context(|| format!("no jar matched {} with classifier {}", coord_fragment, classifier))
}

fn jar_filename_matches(path: &std::path::Path, coord: &str, classifier: Option<&str>) -> bool {
    let name = match path.file_name().and_then(|f| f.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if !name.contains(coord) {
        return false;
    }
    match classifier {
        Some(c) => name.contains(c),
        None => true,
    }
}

/// Resolve the JaCoCo CLI jar using the `nodeps` classifier (a shaded,
/// dependency-free fat jar convenient for invoking the report tool).
pub fn resolve_cli_jar(
    default_repos: &[curie_deps::repo::Repository],
    extra_repos: &[curie_deps::repo::Repository],
    offline: bool,
) -> Result<PathBuf> {
    let jars = resolve(
        &[DepEntry {
            key: JACOCO_CLI_COORD,
            version: DEFAULT_JACOCO_VERSION,
            repo_id: None,
            exclusions: vec![],
            classifier: Some("nodeps"),
            allow_version_conflict: false,
        }],
        &ResolveOptions {
            default_repos: default_repos.to_vec(),
            named_repos: extra_repos.to_vec(),
            progress: false,
            bom_imports: vec![],
            offline,
            skip_version_ranges: false, error_on_version_conflict: false,
        },
    )
    .context("failed to resolve JaCoCo CLI")?;

    select_jar_from_resolve(&jars, "jacoco.cli", "nodeps")
        .with_context(|| "JaCoCo CLI JAR not found after resolution")
}

/// Build the `-javaagent:…` argument for the test JVM.
///
/// The agent is configured to write execution data to `dest_file`.
pub fn agent_jvm_arg(agent_jar: &Path, dest_file: &Path) -> String {
    format!(
        "-javaagent:{}=destfile={}",
        agent_jar.display(),
        dest_file.display()
    )
}

/// Generate an HTML + CSV coverage report using the JaCoCo CLI.
///
/// `cli_jar`    — resolved JaCoCo CLI (nodeps) JAR.
/// `exec_file`  — `target/coverage/jacoco.exec` produced by the agent.
/// `classes_dir` — `target/classes` containing the compiled production bytecode.
/// `source_dirs` — source roots (Maven-style or flat-package) for linking
///                 source files into the HTML report.
/// `report_dir`  — output directory for the HTML report (`target/coverage/`).
///
/// Returns `Ok(summary)` with a short textual summary of line/branch coverage.
pub fn generate_report(
    cli_jar: &Path,
    exec_file: &Path,
    classes_dir: &Path,
    source_dirs: &[PathBuf],
    report_dir: &Path,
) -> Result<CoverageSummary> {
    let html_dir = report_dir.join("html");
    let csv_file = report_dir.join("coverage.csv");

    std::fs::create_dir_all(&html_dir)
        .with_context(|| format!("failed to create {}", html_dir.display()))?;

    let mut cmd = std::process::Command::new("java");
    cmd.arg("-jar").arg(cli_jar);
    cmd.arg("report").arg(exec_file);
    cmd.arg("--classfiles").arg(classes_dir);
    for src in source_dirs {
        cmd.arg("--sourcefiles").arg(src);
    }
    cmd.arg("--html").arg(&html_dir);
    cmd.arg("--csv").arg(&csv_file);

    let status = crate::proc::spawn_cmd(&mut cmd)
        .context("failed to invoke JaCoCo CLI for report generation")?;

    if !status.success() {
        anyhow::bail!("JaCoCo report generation failed");
    }

    // Parse the CSV for a short summary.
    parse_csv_summary(&csv_file)
}

/// Short coverage summary extracted from the CSV report.
#[derive(Debug, Clone)]
pub struct CoverageSummary {
    pub line_covered: u64,
    pub line_missed: u64,
    pub branch_covered: u64,
    pub branch_missed: u64,
}

impl CoverageSummary {
    /// Line coverage percentage (0–100).
    pub fn line_pct(&self) -> f64 {
        let total = self.line_covered + self.line_missed;
        if total == 0 {
            100.0
        } else {
            (self.line_covered as f64 / total as f64) * 100.0
        }
    }

    /// Branch coverage percentage (0–100).
    pub fn branch_pct(&self) -> f64 {
        let total = self.branch_covered + self.branch_missed;
        if total == 0 {
            100.0
        } else {
            (self.branch_covered as f64 / total as f64) * 100.0
        }
    }

    /// One-line human-readable summary, e.g. "72.5% lines, 55.0% branches".
    pub fn summary_line(&self) -> String {
        format!("{:.1}% lines, {:.1}% branches", self.line_pct(), self.branch_pct())
    }
}

/// Parse the JaCoCo CSV report and aggregate totals.
///
/// JaCoCo CSV columns (0-indexed):
///   0  GROUP
///   1  PACKAGE
///   2  CLASS
///   3  INSTRUCTION_MISSED
///   4  INSTRUCTION_COVERED
///   5  BRANCH_MISSED
///   6  BRANCH_COVERED
///   7  LINE_MISSED
///   8  LINE_COVERED
///   9  COMPLEXITY_MISSED
///  10  COMPLEXITY_COVERED
///  11  METHOD_MISSED
///  12  METHOD_COVERED
fn parse_csv_summary(csv_path: &Path) -> Result<CoverageSummary> {
    let content = std::fs::read_to_string(csv_path)
        .with_context(|| format!("failed to read {}", csv_path.display()))?;

    let mut line_covered: u64 = 0;
    let mut line_missed: u64 = 0;
    let mut branch_covered: u64 = 0;
    let mut branch_missed: u64 = 0;

    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            continue; // skip header
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 9 {
            continue;
        }
        branch_missed  += cols[5].parse::<u64>().unwrap_or(0);
        branch_covered += cols[6].parse::<u64>().unwrap_or(0);
        line_missed    += cols[7].parse::<u64>().unwrap_or(0);
        line_covered   += cols[8].parse::<u64>().unwrap_or(0);
    }

    Ok(CoverageSummary {
        line_covered,
        line_missed,
        branch_covered,
        branch_missed,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_jvm_arg_format() {
        let arg = agent_jvm_arg(
            Path::new("/cache/jacocoagent.jar"),
            Path::new("/project/target/coverage/jacoco.exec"),
        );
        assert!(arg.starts_with("-javaagent:/cache/jacocoagent.jar=destfile="));
        assert!(arg.contains("jacoco.exec"));
    }

    #[test]
    fn coverage_summary_percentages() {
        let s = CoverageSummary {
            line_covered: 75,
            line_missed: 25,
            branch_covered: 10,
            branch_missed: 10,
        };
        assert!((s.line_pct() - 75.0).abs() < 0.01);
        assert!((s.branch_pct() - 50.0).abs() < 0.01);
    }

    #[test]
    fn coverage_summary_zero_totals_is_100() {
        let s = CoverageSummary {
            line_covered: 0,
            line_missed: 0,
            branch_covered: 0,
            branch_missed: 0,
        };
        assert!((s.line_pct() - 100.0).abs() < 0.01);
        assert!((s.branch_pct() - 100.0).abs() < 0.01);
    }

    #[test]
    fn coverage_summary_line_format() {
        let s = CoverageSummary {
            line_covered: 80,
            line_missed: 20,
            branch_covered: 6,
            branch_missed: 4,
        };
        let line = s.summary_line();
        assert!(line.contains("80.0%"), "got: {line}");
        assert!(line.contains("60.0%"), "got: {line}");
        assert!(line.contains("lines"), "got: {line}");
        assert!(line.contains("branches"), "got: {line}");
    }

    #[test]
    fn parse_csv_summary_aggregates_rows() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("coverage.csv");
        std::fs::write(
            &csv,
            "GROUP,PACKAGE,CLASS,INSTRUCTION_MISSED,INSTRUCTION_COVERED,\
             BRANCH_MISSED,BRANCH_COVERED,LINE_MISSED,LINE_COVERED,\
             COMPLEXITY_MISSED,COMPLEXITY_COVERED,METHOD_MISSED,METHOD_COVERED\n\
             g,p,A,10,20,2,4,3,7,1,2,0,1\n\
             g,p,B,5,15,1,3,2,8,0,1,0,1\n",
        )
        .unwrap();

        let summary = parse_csv_summary(&csv).unwrap();
        assert_eq!(summary.line_covered, 15);
        assert_eq!(summary.line_missed, 5);
        assert_eq!(summary.branch_covered, 7);
        assert_eq!(summary.branch_missed, 3);
    }

    #[test]
    fn parse_csv_summary_empty_body_returns_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("coverage.csv");
        std::fs::write(
            &csv,
            "GROUP,PACKAGE,CLASS,INSTRUCTION_MISSED,INSTRUCTION_COVERED,\
             BRANCH_MISSED,BRANCH_COVERED,LINE_MISSED,LINE_COVERED,\
             COMPLEXITY_MISSED,COMPLEXITY_COVERED,METHOD_MISSED,METHOD_COVERED\n",
        )
        .unwrap();

        let summary = parse_csv_summary(&csv).unwrap();
        assert_eq!(summary.line_covered, 0);
        assert_eq!(summary.line_missed, 0);
    }

    #[test]
    fn default_jacoco_version_is_semver() {
        let parts: Vec<&str> = DEFAULT_JACOCO_VERSION.split('.').collect();
        assert!(parts.len() >= 2, "expected semver, got: {}", DEFAULT_JACOCO_VERSION);
    }
}
