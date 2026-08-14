//! Interactive TUI for browsing build results (`curie inspect`).
//!
//! Top-level view modes switch what the members tree and detail pane show:
//! **Logs**, **Tests**, **Coverage**, and **Deps**.
//! Colours follow the One Dark palette on a pure black background.

use std::collections::{HashMap, HashSet};
use std::io::Stdout;
use std::path::{Path, PathBuf};

use ansi_to_tui::IntoText;
use anyhow::Result;
use crossterm::{
    cursor,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use crate::coverage::{
    load_source_lines, try_load_member_coverage, LineHit, MemberCoverage, SourceFileCoverage,
    SourceLine,
};
use crate::deps::{self, MemberDepsView};
use crate::descriptor;
use crate::parallel::parse_meta;
use crate::workspace;

// ── One Dark palette (black background) ───────────────────────────────────
//
// Accents match the classic One Dark / `onedark.vim` theme; the canvas stays
// pure black so the TUI sits cleanly on a dark terminal.

mod theme {
    use ratatui::style::Color;

    pub const BG: Color = Color::Black;
    pub const FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
    pub const COMMENT: Color = Color::Rgb(0x5c, 0x63, 0x70);
    pub const RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
    pub const GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
    pub const YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
    pub const BLUE: Color = Color::Rgb(0x61, 0xaf, 0xef);
    pub const MAGENTA: Color = Color::Rgb(0xc6, 0x78, 0xdd);
    pub const CYAN: Color = Color::Rgb(0x56, 0xb6, 0xc2);
    #[allow(dead_code)]
    pub const ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
    /// Near-black strip behind headers / annotations (not pure black so it reads).
    pub const SURFACE: Color = Color::Rgb(0x14, 0x16, 0x1b);
    /// Selected row background.
    pub const SELECTION: Color = Color::Rgb(0x2c, 0x31, 0x3c);
}

// ── Public types ──────────────────────────────────────────────────────────

/// Lightweight descriptor of a single build target, passed in by the caller.
#[derive(Clone)]
pub(crate) struct LogTarget {
    pub declared: String,
    pub path: PathBuf,
}

// ── Internal types ────────────────────────────────────────────────────────

#[derive(Clone)]
enum LogState {
    Ok {
        duration_ms: u64,
    },
    Failed {
        duration_ms: u64,
    },
    /// `.log` exists but no `.meta` sidecar — metadata unavailable.
    NoMetadata,
    /// No `.log` or `.meta`.
    NoLog,
}

struct Job {
    declared: String,
    state: LogState,
    /// `None` when no `.meta` is present.
    started_ms: Option<u64>,
    /// `"HH:MM:SS"` in local timezone, or `""` when unknown.
    started_disp: String,
    lines: Vec<String>,
    /// UUIDv7 shared by all jobs in one build run; `None` when no `.meta`.
    build_id: Option<String>,
    /// Per-test results from `target/build.tests.json`; empty when absent.
    tests: Vec<TestEntry>,
    /// Coverage from `target/coverage/`; `None` when absent.
    coverage: Option<MemberCoverage>,
    /// Declared dependencies from `Curie.toml` (with workspace inheritance).
    deps: Option<MemberDepsView>,
}

#[derive(Clone)]
struct TestEntry {
    name: String,
    #[allow(dead_code)]
    class_name: String,
    duration_ms: u64,
    status: TestStatus,
    failure: Option<String>,
    /// Absolute path to the per-test output `.txt` file, or `None`.
    output_file: Option<PathBuf>,
}

#[derive(Clone, PartialEq)]
enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

/// The active filter applied to the merged log.
#[derive(Clone)]
enum Filter {
    All,
    /// Show only jobs whose `declared` path equals `p` or starts with `p/`.
    Prefix(String),
}

struct TreeNode {
    /// Indented display text shown in the members pane.
    label: String,
    /// Used as the log block title when this node is selected.
    title: String,
    filter: Filter,
    /// `None` for the root "all jobs" row and for container rows.
    state: Option<LogState>,
    selectable: bool,
    /// `Some((job_idx, test_idx))` for test method leaf nodes.
    test_ref: Option<(usize, usize)>,
    /// `Some((badge_text, status))` — drives right-aligned coloured badge for test nodes.
    test_badge: Option<(String, TestStatus)>,
    /// `Some((job_idx, class_name))` for class group nodes.
    class_ref: Option<(usize, String)>,
    /// Index into `jobs` for every node that belongs to a specific job (leaf or child).
    job_idx: Option<usize>,
    /// Compact coverage badge (`"87.3% / 74.1%"`) for job / source nodes; `None` when absent.
    coverage_badge: Option<String>,
    /// `Some(job_idx)` for the "Coverage" group under a job.
    #[allow(dead_code)]
    coverage_group_ref: Option<usize>,
    /// `Some((job_idx, source_idx))` for a source-file leaf under Coverage.
    coverage_source_ref: Option<(usize, usize)>,
    /// Right-aligned badge for Deps mode (e.g. dep counts).
    deps_badge: Option<String>,
}

enum Row {
    Header {
        job: usize,
    },
    /// `line` is an index into `jobs[job].lines`.
    Body {
        job: usize,
        line: usize,
    },
    /// A coloured annotation line at the top of a test's log view.
    TestAnnotation {
        text: String,
        color: Color,
    },
    /// `line` is an index into `InspectState::test_lines`.
    TestBody {
        line: usize,
    },
    /// Coverage summary or uncovered-class line shown above a job's log.
    CoverageLine {
        text: String,
        color: Color,
    },
    /// Annotated source line (coverage drill-down).
    SourceBody {
        line: usize,
    },
}

#[derive(Debug, PartialEq, Clone)]
enum ActivePane {
    Members,
    Log,
    Search,
}

/// Top-level view: what the members tree and detail pane present.
#[derive(PartialEq, Clone, Copy, Debug)]
enum ViewMode {
    Logs,
    Tests,
    Coverage,
    Deps,
}

impl ViewMode {
    const ALL: [ViewMode; 4] = [
        ViewMode::Logs,
        ViewMode::Tests,
        ViewMode::Coverage,
        ViewMode::Deps,
    ];

    fn label(self) -> &'static str {
        match self {
            ViewMode::Logs => "Logs",
            ViewMode::Tests => "Tests",
            ViewMode::Coverage => "Coverage",
            ViewMode::Deps => "Deps",
        }
    }

    fn detail_title_prefix(self) -> &'static str {
        match self {
            ViewMode::Logs => "Log",
            ViewMode::Tests => "Tests",
            ViewMode::Coverage => "Coverage",
            ViewMode::Deps => "Deps",
        }
    }

    fn next(self) -> Self {
        match self {
            ViewMode::Logs => ViewMode::Tests,
            ViewMode::Tests => ViewMode::Coverage,
            ViewMode::Coverage => ViewMode::Deps,
            ViewMode::Deps => ViewMode::Logs,
        }
    }

    fn prev(self) -> Self {
        match self {
            ViewMode::Logs => ViewMode::Deps,
            ViewMode::Tests => ViewMode::Logs,
            ViewMode::Coverage => ViewMode::Tests,
            ViewMode::Deps => ViewMode::Coverage,
        }
    }

    fn from_digit(c: char) -> Option<Self> {
        match c {
            '1' => Some(ViewMode::Logs),
            '2' => Some(ViewMode::Tests),
            '3' => Some(ViewMode::Coverage),
            '4' => Some(ViewMode::Deps),
            _ => None,
        }
    }
}

#[derive(PartialEq, Clone)]
enum InputMode {
    Normal,
    /// User is typing a grep pattern for log content.
    Grep,
    /// User is typing a job name filter.
    JobSearch,
}

struct InspectState {
    /// Stored for `reload`.
    targets: Vec<LogTarget>,
    /// Workspace or standalone project root (descriptor loading).
    ws_root: PathBuf,
    action: String,
    jobs: Vec<Job>,
    nodes: Vec<TreeNode>,
    selected_idx: usize,
    tree_scroll: usize,
    rows: Vec<Row>,
    scroll: usize,
    show_members: bool,
    active_pane: ActivePane,
    /// Logs / Tests / Coverage / Deps.
    mode: ViewMode,
    filter: Filter,
    log_title: String,
    /// Terminal height minus chrome rows; kept in sync from the event loop.
    pane_h: u16,
    /// Visible log height (inner block rows); updated each frame for scroll clamping.
    log_vis_h: usize,
    utc_offset: time::UtcOffset,
    input_mode: InputMode,
    /// Current log-content grep pattern.
    grep: String,
    /// Current job-name search pattern.
    job_search: String,
    /// Job indices that have been expanded (tests, sources, or dep scopes).
    expanded_jobs: HashSet<usize>,
    /// `(job_idx, class_name)` pairs that have been expanded to show test method rows.
    expanded_classes: HashSet<(usize, String)>,
    /// Descriptors keyed by member `declared` path (workspace inheritance applied).
    descriptors: HashMap<String, descriptor::Descriptor>,
    /// Cached resolved tree lines for `(job_idx, tests)` — compile (`false`) / test (`true`).
    resolved_deps: HashMap<(usize, bool), Result<Vec<String>, String>>,
    /// Lines from the currently selected test's output file (empty when not in test view).
    test_lines: Vec<String>,
    /// Annotated source lines when a coverage source file is selected.
    source_lines: Vec<SourceLine>,
    /// Pane to return to when the search bar is dismissed with Enter or Esc.
    pre_search_pane: ActivePane,
    /// Job indices whose log lines contain the current grep pattern (empty when grep inactive).
    grep_job_matches: HashSet<usize>,
    /// Job indices whose build_id is older than the latest build_id seen across all jobs.
    stale_jobs: HashSet<usize>,
    /// Horizontal character offset for the log pane (scrolled with Left/Right).
    h_scroll: usize,
}

// ── Entry point ───────────────────────────────────────────────────────────

pub(crate) fn run_inspect_ui(
    ws_root: &std::path::Path,
    targets: &[LogTarget],
    action: &str,
    preselect: Option<usize>,
) -> Result<()> {
    // Query local offset before spawning threads or entering raw mode.
    let utc_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    let descriptors = load_descriptors(ws_root, targets);
    let jobs = load_jobs(targets, action, utc_offset, &descriptors);
    let stale_jobs = collect_stale_jobs(&jobs);
    let expanded_jobs: HashSet<usize> = HashSet::new();
    let expanded_classes: HashSet<(usize, String)> = HashSet::new();
    let mode = ViewMode::Logs;
    let nodes = build_tree_nodes(&jobs, mode, &expanded_jobs, &expanded_classes);
    let filter = Filter::All;
    let rows = build_rows(&jobs, &filter, "", "");

    let mut state = InspectState {
        targets: targets.to_vec(),
        ws_root: ws_root.to_path_buf(),
        action: action.to_string(),
        jobs,
        nodes,
        selected_idx: 0,
        tree_scroll: 0,
        rows,
        scroll: 0,
        show_members: true,
        active_pane: ActivePane::Members,
        mode,
        filter,
        log_title: "all jobs".to_string(),
        pane_h: 24,
        log_vis_h: 20,
        utc_offset,
        input_mode: InputMode::Normal,
        grep: String::new(),
        job_search: String::new(),
        expanded_jobs,
        expanded_classes,
        descriptors,
        resolved_deps: HashMap::new(),
        test_lines: Vec::new(),
        source_lines: Vec::new(),
        pre_search_pane: ActivePane::Members,
        grep_job_matches: HashSet::new(),
        stale_jobs,
        h_scroll: 0,
    };

    // Auto-focus first failure (switches to Tests when a failing test exists).
    if !auto_select_failed(&mut state) {
        if let Some(idx) = preselect {
            if idx < state.jobs.len() {
                if let Some(ni) = find_node_for_declared(&state.nodes, &state.jobs[idx].declared) {
                    state.selected_idx = ni;
                    apply_selection(&mut state);
                }
            }
        }
    }

    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        EnableMouseCapture,
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = event_loop(&mut term, &mut state);

    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        term.backend_mut(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show,
    );
    let _ = term.show_cursor();
    result
}

// ── Loading ───────────────────────────────────────────────────────────────

/// Load descriptors with workspace inheritance when `ws_root` is a workspace.
fn load_descriptors(
    ws_root: &Path,
    targets: &[LogTarget],
) -> HashMap<String, descriptor::Descriptor> {
    let mut map = HashMap::new();
    if let Ok(ws) = workspace::load(ws_root) {
        for m in ws.members {
            map.insert(m.declared, m.descriptor);
        }
        // Ensure every inspect target is present even if names differ slightly.
        for t in targets {
            if map.contains_key(&t.declared) {
                continue;
            }
            if let Ok(d) = descriptor::load(&t.path) {
                map.insert(t.declared.clone(), d);
            }
        }
        return map;
    }
    for t in targets {
        if let Ok(d) = descriptor::load(&t.path) {
            map.insert(t.declared.clone(), d);
        }
    }
    map
}

fn load_jobs(
    targets: &[LogTarget],
    action: &str,
    utc_offset: time::UtcOffset,
    descriptors: &HashMap<String, descriptor::Descriptor>,
) -> Vec<Job> {
    targets
        .iter()
        .map(|t| {
            let log_path = t.path.join("target").join(format!("{action}.log"));
            let meta_path = t.path.join("target").join(format!("{action}.meta"));
            let meta = parse_meta(&meta_path);

            let state = match (meta.as_ref(), log_path.exists()) {
                (Some(m), _) if m.exit_code == 0 => LogState::Ok {
                    duration_ms: m.duration_ms,
                },
                (Some(m), _) => LogState::Failed {
                    duration_ms: m.duration_ms,
                },
                (None, true) => LogState::NoMetadata,
                (None, false) => LogState::NoLog,
            };

            let (started_ms, started_disp) = meta
                .as_ref()
                .map(|m| {
                    (
                        Some(m.started_ms),
                        format_hms_local(m.started_ms, utc_offset),
                    )
                })
                .unwrap_or((None, String::new()));

            let build_id = meta.as_ref().map(|m| m.build_id.clone());
            let lines = if log_path.exists() {
                load_log(&log_path)
            } else {
                Vec::new()
            };
            let tests = parse_test_sidecar(&t.path);
            let coverage = try_load_member_coverage(&t.path);
            let deps = descriptors
                .get(&t.declared)
                .map(MemberDepsView::from_descriptor);

            Job {
                declared: t.declared.clone(),
                state,
                started_ms,
                started_disp,
                lines,
                build_id,
                tests,
                coverage,
                deps,
            }
        })
        .collect()
}

/// Return the set of job indices whose build_id is older than the latest seen.
/// When all jobs share the same id (or have none), the set is empty.
fn collect_stale_jobs(jobs: &[Job]) -> HashSet<usize> {
    let latest = jobs
        .iter()
        .filter_map(|j| j.build_id.as_deref())
        .max()
        .map(str::to_string);
    let Some(latest_id) = latest else {
        return HashSet::new();
    };
    let has_mixed = jobs
        .iter()
        .filter_map(|j| j.build_id.as_deref())
        .any(|id| id != latest_id);
    if !has_mixed {
        return HashSet::new();
    }
    jobs.iter()
        .enumerate()
        .filter(|(_, j)| j.build_id.as_deref().is_some_and(|id| id != latest_id))
        .map(|(i, _)| i)
        .collect()
}

fn parse_test_sidecar(member_root: &std::path::Path) -> Vec<TestEntry> {
    let path = member_root.join("target").join("build.tests.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let raw: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    raw.into_iter()
        .filter_map(|v| parse_test_entry(v, member_root))
        .collect()
}

fn parse_test_entry(v: serde_json::Value, member_root: &std::path::Path) -> Option<TestEntry> {
    let name = v["name"].as_str()?.to_string();
    let class_name = v["class_name"].as_str().unwrap_or("").to_string();
    let duration_ms = v["duration_ms"].as_u64().unwrap_or(0);
    let status = match v["status"].as_str().unwrap_or("") {
        "failed" => TestStatus::Failed,
        "skipped" => TestStatus::Skipped,
        _ => TestStatus::Passed,
    };
    let failure = v["failure"].as_str().map(str::to_string);
    // output_file in JSON is relative to target/, e.g. "test-output/com/..."
    let output_file = v["output_file"]
        .as_str()
        .map(|p| member_root.join("target").join(p));
    Some(TestEntry {
        name,
        class_name,
        duration_ms,
        status,
        failure,
        output_file,
    })
}

fn load_log(path: &std::path::Path) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

// ── Tree construction ─────────────────────────────────────────────────────

fn build_tree_nodes(
    jobs: &[Job],
    mode: ViewMode,
    expanded_jobs: &HashSet<usize>,
    expanded_classes: &HashSet<(usize, String)>,
) -> Vec<TreeNode> {
    let root_label = match mode {
        ViewMode::Logs => "all jobs",
        ViewMode::Tests => "all tests",
        ViewMode::Coverage => "all coverage",
        ViewMode::Deps => "all dependencies",
    };
    let mut nodes = vec![tree_node_plain(root_label, root_label, Filter::All)];

    let mut current_dirs: Vec<String> = Vec::new();

    for (job_idx, job) in jobs.iter().enumerate() {
        let parts: Vec<&str> = job.declared.split('/').collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        let name = parts.last().copied().unwrap_or(&job.declared);

        // Common prefix depth with the previously emitted directory stack.
        let common = {
            let mut n = 0;
            for (a, b) in dirs.iter().zip(current_dirs.iter()) {
                if *a == b.as_str() {
                    n += 1;
                } else {
                    break;
                }
            }
            n
        };

        for depth in common..dirs.len() {
            let path_here = dirs[..=depth].join("/");
            let indent = "  ".repeat(depth + 1);
            nodes.push(tree_node_plain(
                &format!("{indent}{}/", dirs[depth]),
                &format!("{path_here}/"),
                Filter::Prefix(path_here),
            ));
        }

        let depth = dirs.len();
        let indent = "  ".repeat(depth + 1);
        push_job_nodes(
            &mut nodes,
            job,
            job_idx,
            name,
            depth,
            &indent,
            mode,
            expanded_jobs,
            expanded_classes,
        );

        current_dirs = dirs.iter().map(|s| s.to_string()).collect();
    }

    nodes
}

#[allow(clippy::too_many_arguments)]
fn push_job_nodes(
    nodes: &mut Vec<TreeNode>,
    job: &Job,
    job_idx: usize,
    name: &str,
    depth: usize,
    indent: &str,
    mode: ViewMode,
    expanded_jobs: &HashSet<usize>,
    expanded_classes: &HashSet<(usize, String)>,
) {
    let has_tests = !job.tests.is_empty();
    let has_coverage = job
        .coverage
        .as_ref()
        .is_some_and(|c| !c.sources.is_empty() || !c.report.classes.is_empty());

    // Deps mode never expands past the project — compile/test trees live in the detail pane.
    let (expandable, coverage_badge, test_summary, deps_badge) = match mode {
        ViewMode::Logs => (false, None, None, None),
        ViewMode::Tests => (
            has_tests,
            None,
            if has_tests {
                Some(test_summary_badge(&job.tests))
            } else {
                None
            },
            None,
        ),
        ViewMode::Coverage => (
            has_coverage,
            job.coverage.as_ref().map(|c| c.report.summary.badge()),
            None,
            None,
        ),
        ViewMode::Deps => {
            let badge = job.deps.as_ref().map(|d| {
                let n = d.compile_test_count();
                if n == 1 {
                    "1 dep".to_string()
                } else {
                    format!("{n} deps")
                }
            });
            (false, None, None, badge)
        }
    };

    let expand_marker = if expandable {
        if expanded_jobs.contains(&job_idx) {
            " ▾"
        } else {
            " ▸"
        }
    } else {
        ""
    };

    // In Tests mode, surface the pass/fail summary as the right badge instead of duration.
    // In Coverage mode, surface the coverage %. Deps shows count. Logs keeps outcome via `state`.
    let (state, job_cov_badge, job_test_badge) = match mode {
        ViewMode::Logs => (Some(job.state.clone()), None, None),
        ViewMode::Tests => {
            if let Some(summary) = test_summary {
                let status = if job.tests.iter().any(|t| t.status == TestStatus::Failed) {
                    TestStatus::Failed
                } else if job.tests.iter().any(|t| t.status == TestStatus::Skipped)
                    && job.tests.iter().all(|t| t.status != TestStatus::Failed)
                    && !job.tests.iter().any(|t| t.status == TestStatus::Passed)
                {
                    TestStatus::Skipped
                } else {
                    TestStatus::Passed
                };
                (None, None, Some((summary, status)))
            } else {
                (Some(job.state.clone()), None, None)
            }
        }
        ViewMode::Coverage => (Some(job.state.clone()), coverage_badge, None),
        ViewMode::Deps => (None, None, None),
    };

    nodes.push(TreeNode {
        label: format!("{indent}{name}{expand_marker}"),
        title: job.declared.clone(),
        filter: Filter::Prefix(job.declared.clone()),
        state,
        selectable: true,
        test_ref: None,
        test_badge: job_test_badge,
        class_ref: None,
        job_idx: Some(job_idx),
        coverage_badge: job_cov_badge,
        coverage_group_ref: None,
        coverage_source_ref: None,
        deps_badge,
    });

    if !expandable || !expanded_jobs.contains(&job_idx) {
        return;
    }

    let child_indent = "  ".repeat(depth + 2);
    let leaf_indent = "  ".repeat(depth + 3);

    match mode {
        ViewMode::Logs | ViewMode::Deps => {}
        ViewMode::Tests => {
            for (class_name, tests_in_class) in group_by_class(&job.tests) {
                let class_expanded = expanded_classes.contains(&(job_idx, class_name.clone()));
                let class_marker = if class_expanded { " ▾" } else { " ▸" };
                let short_class = class_name.rsplit('.').next().unwrap_or(&class_name);
                nodes.push(TreeNode {
                    label: format!("{child_indent}{short_class}{class_marker}"),
                    title: format!("{} › {}", job.declared, class_name),
                    filter: Filter::Prefix(job.declared.clone()),
                    state: None,
                    selectable: true,
                    test_ref: None,
                    test_badge: None,
                    class_ref: Some((job_idx, class_name.clone())),
                    job_idx: Some(job_idx),
                    coverage_badge: None,
                    coverage_group_ref: None,
                    coverage_source_ref: None,
                    deps_badge: None,
                });

                if class_expanded {
                    for &test_idx in &tests_in_class {
                        let test = &job.tests[test_idx];
                        let badge = test_badge(&test.status, test.duration_ms);
                        nodes.push(TreeNode {
                            label: format!("{leaf_indent}{}", test.name),
                            title: format!("{} › {} › {}", job.declared, class_name, test.name),
                            filter: Filter::Prefix(job.declared.clone()),
                            state: None,
                            selectable: true,
                            test_ref: Some((job_idx, test_idx)),
                            test_badge: Some((badge, test.status.clone())),
                            class_ref: None,
                            job_idx: Some(job_idx),
                            coverage_badge: None,
                            coverage_group_ref: None,
                            coverage_source_ref: None,
                            deps_badge: None,
                        });
                    }
                }
            }
        }
        ViewMode::Coverage => {
            if let Some(cov) = job.coverage.as_ref() {
                if !cov.sources.is_empty() {
                    for (src_idx, src) in cov.sources.iter().enumerate() {
                        nodes.push(coverage_source_node(
                            &child_indent,
                            job,
                            job_idx,
                            src_idx,
                            &src.file_name,
                            src.badge(),
                        ));
                    }
                } else {
                    for (src_idx, class) in cov.report.classes.iter().enumerate() {
                        nodes.push(coverage_source_node(
                            &child_indent,
                            job,
                            job_idx,
                            src_idx,
                            &class.class_name,
                            class_badge(class),
                        ));
                    }
                }
            }
        }
    }
}

fn test_summary_badge(tests: &[TestEntry]) -> String {
    let passed = tests
        .iter()
        .filter(|t| t.status == TestStatus::Passed)
        .count();
    let failed = tests
        .iter()
        .filter(|t| t.status == TestStatus::Failed)
        .count();
    let skipped = tests
        .iter()
        .filter(|t| t.status == TestStatus::Skipped)
        .count();
    if failed > 0 {
        format!("{passed}✓ {failed}✗")
    } else if skipped > 0 {
        format!("{passed}✓ {skipped}⊘")
    } else {
        format!("{passed}✓")
    }
}

fn class_badge(class: &crate::coverage::ClassCoverage) -> String {
    format!("{:.1}% / {:.1}%", class.line_pct(), class.branch_pct())
}

fn coverage_source_node(
    indent: &str,
    job: &Job,
    job_idx: usize,
    src_idx: usize,
    name: &str,
    badge: String,
) -> TreeNode {
    TreeNode {
        label: format!("{indent}{name}"),
        title: format!("{} › {}", job.declared, name),
        filter: Filter::Prefix(job.declared.clone()),
        state: None,
        selectable: true,
        test_ref: None,
        test_badge: None,
        class_ref: None,
        job_idx: Some(job_idx),
        coverage_badge: Some(badge),
        coverage_group_ref: None,
        coverage_source_ref: Some((job_idx, src_idx)),
        deps_badge: None,
    }
}

/// Return `(class_name, Vec<test_idx>)` in stable order of first appearance.
fn group_by_class(tests: &[TestEntry]) -> Vec<(String, Vec<usize>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (i, t) in tests.iter().enumerate() {
        let key = if t.class_name.is_empty() {
            "(unknown)".to_string()
        } else {
            t.class_name.clone()
        };
        if !map.contains_key(&key) {
            order.push(key.clone());
        }
        map.entry(key).or_default().push(i);
    }
    order
        .into_iter()
        .map(|k| {
            let v = map.remove(&k).unwrap();
            (k, v)
        })
        .collect()
}

fn tree_node_plain(label: &str, title: &str, filter: Filter) -> TreeNode {
    TreeNode {
        label: label.to_string(),
        title: title.to_string(),
        filter,
        state: None,
        selectable: true,
        test_ref: None,
        test_badge: None,
        class_ref: None,
        job_idx: None,
        coverage_badge: None,
        coverage_group_ref: None,
        coverage_source_ref: None,
        deps_badge: None,
    }
}

/// Find the node whose filter prefix equals `declared` exactly (leaf lookup).
fn find_node_for_declared(nodes: &[TreeNode], declared: &str) -> Option<usize> {
    nodes
        .iter()
        .position(|n| matches!(&n.filter, Filter::Prefix(p) if p == declared))
}

// ── Filtering ─────────────────────────────────────────────────────────────

fn job_matches(filter: &Filter, declared: &str) -> bool {
    match filter {
        Filter::All => true,
        Filter::Prefix(p) => declared == p || declared.starts_with(&format!("{p}/")),
    }
}

fn job_search_matches(declared: &str, job_search: &str) -> bool {
    if job_search.is_empty() {
        return true;
    }
    declared.to_lowercase().contains(&job_search.to_lowercase())
}

// ── Row building ──────────────────────────────────────────────────────────

/// Build the flat list of rows for the log pane.
///
/// - `filter`:     workspace/project filter from the members pane.
/// - `grep`:       non-empty → only body rows whose text contains the pattern.
/// - `job_search`: non-empty → only jobs whose `declared` path contains the pattern.
fn build_rows(jobs: &[Job], filter: &Filter, grep: &str, job_search: &str) -> Vec<Row> {
    let mut indices: Vec<usize> = (0..jobs.len())
        .filter(|&i| job_matches(filter, &jobs[i].declared))
        .filter(|&i| job_search_matches(&jobs[i].declared, job_search))
        .collect();

    indices.sort_by(|&a, &b| {
        let sa = jobs[a].started_ms.unwrap_or(u64::MAX);
        let sb = jobs[b].started_ms.unwrap_or(u64::MAX);
        sa.cmp(&sb)
            .then_with(|| jobs[a].declared.cmp(&jobs[b].declared))
    });

    let mut rows = Vec::new();
    if grep.is_empty() {
        for ji in indices {
            rows.push(Row::Header { job: ji });
            for li in 0..jobs[ji].lines.len() {
                rows.push(Row::Body { job: ji, line: li });
            }
        }
    } else {
        let grep_lower = grep.to_lowercase();
        for ji in indices {
            let matching: Vec<usize> = (0..jobs[ji].lines.len())
                .filter(|&li| jobs[ji].lines[li].to_lowercase().contains(&grep_lower))
                .collect();
            if !matching.is_empty() {
                rows.push(Row::Header { job: ji });
                for li in matching {
                    rows.push(Row::Body { job: ji, line: li });
                }
            }
        }
    }
    rows
}

// ── Selection and reload ──────────────────────────────────────────────────

fn apply_selection(state: &mut InspectState) {
    let node = &state.nodes[state.selected_idx];
    let test_ref = node.test_ref;
    let cov_src = node.coverage_source_ref;
    let title = node.title.clone();
    let filter = node.filter.clone();

    state.log_title = title;
    state.filter = filter;
    state.h_scroll = 0;

    if let Some((job_idx, test_idx)) = test_ref {
        state.source_lines.clear();
        load_test_view(state, job_idx, test_idx);
    } else if let Some((job_idx, src_idx)) = cov_src {
        state.test_lines.clear();
        load_source_view(state, job_idx, src_idx);
    } else {
        state.test_lines.clear();
        state.source_lines.clear();
        rebuild_rows(state);
    }

    sync_tree_scroll(state);
}

fn load_source_view(state: &mut InspectState, job_idx: usize, src_idx: usize) {
    let Some(cov) = state.jobs.get(job_idx).and_then(|j| j.coverage.as_ref()) else {
        state.source_lines.clear();
        state.rows = vec![Row::CoverageLine {
            text: "(no coverage data)".to_string(),
            color: theme::COMMENT,
        }];
        state.scroll = 0;
        return;
    };

    // Prefer HTML source pages; CSV-only classes have no per-line detail.
    if let Some(src) = cov.sources.get(src_idx) {
        state.source_lines = load_source_lines(&src.html_path).unwrap_or_default();
        let mut rows = vec![Row::CoverageLine {
            text: format!(
                "{}  {}  (green=covered  yellow=partial  red=missed)",
                src.display_name(),
                src.badge(),
            ),
            color: coverage_color(src.line_pct()),
        }];
        if state.source_lines.is_empty() {
            rows.push(Row::CoverageLine {
                text: format!("(could not read {})", src.html_path.display()),
                color: theme::COMMENT,
            });
        } else {
            for i in 0..state.source_lines.len() {
                rows.push(Row::SourceBody { line: i });
            }
        }
        state.rows = rows;
    } else if let Some(class) = cov.report.classes.get(src_idx) {
        state.source_lines.clear();
        state.rows = vec![
            Row::CoverageLine {
                text: format!("{}  {}", class.qualified_name(), class_badge(class)),
                color: coverage_color(class.line_pct()),
            },
            Row::CoverageLine {
                text: "(no source HTML — re-run tests with coverage to generate the report)"
                    .to_string(),
                color: theme::COMMENT,
            },
        ];
    } else {
        state.source_lines.clear();
        state.rows = vec![Row::CoverageLine {
            text: "(unknown source)".to_string(),
            color: theme::COMMENT,
        }];
    }
    let max = state.rows.len().saturating_sub(state.log_vis_h.max(1));
    state.scroll = state.scroll.min(max);
}

fn load_test_view(state: &mut InspectState, job_idx: usize, test_idx: usize) {
    let test = &state.jobs[job_idx].tests[test_idx];
    let failure = test.failure.clone();
    let status = test.status.clone();
    state.test_lines = test
        .output_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();

    let mut rows: Vec<Row> = Vec::new();
    if let Some(msg) = failure {
        rows.push(Row::TestAnnotation {
            text: msg,
            color: theme::RED,
        });
    }
    for li in 0..state.test_lines.len() {
        rows.push(Row::TestBody { line: li });
    }
    if rows.is_empty() {
        let placeholder_color = match status {
            TestStatus::Passed => theme::GREEN,
            TestStatus::Failed => theme::RED,
            TestStatus::Skipped => theme::YELLOW,
        };
        rows.push(Row::TestAnnotation {
            text: "(no output captured)".to_string(),
            color: placeholder_color,
        });
    }
    state.rows = rows;
    let max = state.rows.len().saturating_sub(state.log_vis_h.max(1));
    state.scroll = state.scroll.min(max);
}

/// Job indices whose log lines contain `grep` (case-insensitive). Empty when grep is inactive.
fn grep_hits(jobs: &[Job], grep: &str) -> HashSet<usize> {
    if grep.is_empty() {
        return HashSet::new();
    }
    let pattern = grep.to_lowercase();
    (0..jobs.len())
        .filter(|&i| {
            jobs[i]
                .lines
                .iter()
                .any(|l| l.to_lowercase().contains(&pattern))
        })
        .collect()
}

/// Rebuild rows from current filter/grep/job_search, then clamp scroll.
/// When the selected node is a test leaf or coverage source, re-applies that view.
fn rebuild_rows(state: &mut InspectState) {
    state.grep_job_matches = grep_hits(&state.jobs, &state.grep);
    if let Some((job_idx, test_idx)) = state.nodes.get(state.selected_idx).and_then(|n| n.test_ref)
    {
        load_test_view(state, job_idx, test_idx);
        return;
    }
    if let Some((job_idx, src_idx)) = state
        .nodes
        .get(state.selected_idx)
        .and_then(|n| n.coverage_source_ref)
    {
        load_source_view(state, job_idx, src_idx);
        return;
    }
    state.test_lines.clear();
    state.source_lines.clear();

    match state.mode {
        ViewMode::Logs => {
            state.rows = build_rows(&state.jobs, &state.filter, &state.grep, &state.job_search);
        }
        ViewMode::Tests => {
            if let Some(rows) = tests_panel_for_selection(state) {
                state.rows = rows;
            } else {
                // Directory / "all tests": list matching jobs' test summaries.
                state.rows = build_tests_overview_rows(state);
            }
        }
        ViewMode::Coverage => {
            let mut rows = build_rows(&state.jobs, &state.filter, &state.grep, &state.job_search);
            if let Some(cov_rows) = coverage_panel_for_selection(state) {
                let insert_at = rows
                    .iter()
                    .position(|r| matches!(r, Row::Header { .. }))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                for (offset, row) in cov_rows.into_iter().enumerate() {
                    rows.insert(insert_at + offset, row);
                }
            }
            state.rows = rows;
        }
        ViewMode::Deps => {
            if let Some(job_idx) = deps_selected_job(state) {
                state.rows = build_deps_panel_rows(state, job_idx);
            } else {
                state.rows = build_deps_overview_rows(state);
            }
        }
    }
    let max = state.rows.len().saturating_sub(state.log_vis_h.max(1));
    state.scroll = state.scroll.min(max);
}

/// Exact project node selected in Deps mode (not a directory container or root).
fn deps_selected_job(state: &InspectState) -> Option<usize> {
    let node = state.nodes.get(state.selected_idx)?;
    let job_idx = node.job_idx?;
    let Filter::Prefix(p) = &node.filter else {
        return None;
    };
    let job = state.jobs.get(job_idx)?;
    if job.declared != *p {
        return None;
    }
    Some(job_idx)
}

/// Detail pane for one project: compile tree then test tree.
/// BOM imports are not listed separately — they only feed version resolution.
fn build_deps_panel_rows(state: &mut InspectState, job_idx: usize) -> Vec<Row> {
    let (header, compile_n, test_n) = {
        let Some(job) = state.jobs.get(job_idx) else {
            return vec![Row::CoverageLine {
                text: "(unknown job)".to_string(),
                color: theme::COMMENT,
            }];
        };
        let Some(view) = job.deps.as_ref() else {
            return vec![Row::CoverageLine {
                text: "No Curie.toml descriptor loaded for this member".to_string(),
                color: theme::COMMENT,
            }];
        };
        (
            format!("{}  {} v{}", view.kind_label, view.name, view.version),
            view.compile.len(),
            view.test.len(),
        )
    };

    let mut rows = vec![Row::CoverageLine {
        text: header,
        color: theme::CYAN,
    }];

    append_resolved_scope_section(&mut rows, state, job_idx, false, "Compile", compile_n);
    append_resolved_scope_section(&mut rows, state, job_idx, true, "Test", test_n);

    if compile_n == 0 && test_n == 0 {
        rows.push(Row::CoverageLine {
            text: "  (no compile or test dependencies)".to_string(),
            color: theme::COMMENT,
        });
    }
    rows
}

fn append_resolved_scope_section(
    rows: &mut Vec<Row>,
    state: &mut InspectState,
    job_idx: usize,
    tests: bool,
    label: &str,
    declared_n: usize,
) {
    rows.push(Row::CoverageLine {
        text: format!("{label}  ({declared_n} declared)"),
        color: theme::YELLOW,
    });
    if declared_n == 0 {
        rows.push(Row::CoverageLine {
            text: "  (none)".to_string(),
            color: theme::COMMENT,
        });
        return;
    }
    match ensure_resolved_deps(state, job_idx, tests) {
        Ok(lines) => {
            for line in lines {
                rows.push(Row::CoverageLine {
                    text: line,
                    color: theme::FG,
                });
            }
        }
        Err(err) => {
            // Fall back to declared coordinates when offline resolve fails.
            if let Some(view) = state.jobs.get(job_idx).and_then(|j| j.deps.as_ref()) {
                let items = if tests { &view.test } else { &view.compile };
                for item in items {
                    rows.push(Row::CoverageLine {
                        text: format!("  {}", item.display_line()),
                        color: theme::FG,
                    });
                }
            }
            rows.push(Row::CoverageLine {
                text: format!("  (resolve offline failed: {err})"),
                color: theme::RED,
            });
            rows.push(Row::CoverageLine {
                text: "  Tip: run a build so ~/.m2 is populated, or use `curie deps`.".to_string(),
                color: theme::COMMENT,
            });
        }
    }
}

fn build_deps_overview_rows(state: &InspectState) -> Vec<Row> {
    let mut rows = Vec::new();
    for job in &state.jobs {
        if !job_matches(&state.filter, &job.declared) {
            continue;
        }
        if !job_search_matches(&job.declared, &state.job_search) {
            continue;
        }
        let Some(view) = job.deps.as_ref() else {
            continue;
        };
        let n = view.compile_test_count();
        rows.push(Row::CoverageLine {
            text: format!(
                "{}  {}  — {} compile, {} test",
                job.declared,
                view.kind_label,
                view.compile.len(),
                view.test.len(),
            ),
            color: if n > 0 { theme::CYAN } else { theme::COMMENT },
        });
    }
    if rows.is_empty() {
        rows.push(Row::CoverageLine {
            text: "No projects in this selection".to_string(),
            color: theme::COMMENT,
        });
    } else {
        rows.insert(
            0,
            Row::CoverageLine {
                text: "Select a project to view compile and test dependency trees".to_string(),
                color: theme::COMMENT,
            },
        );
    }
    rows
}

/// Resolve (or fetch from cache) the offline dependency tree for compile/test.
fn ensure_resolved_deps(
    state: &mut InspectState,
    job_idx: usize,
    tests: bool,
) -> Result<Vec<String>, String> {
    if let Some(cached) = state.resolved_deps.get(&(job_idx, tests)) {
        return cached.clone();
    }
    let declared = state
        .jobs
        .get(job_idx)
        .map(|j| j.declared.clone())
        .unwrap_or_default();
    let result = match state.descriptors.get(&declared) {
        Some(desc) => deps::resolve_dep_tree(desc, tests, true /* offline */)
            .map(|tree| deps::format_tree_lines(&tree))
            .map_err(|e| e.to_string()),
        None => Err("descriptor not loaded".to_string()),
    };
    state.resolved_deps.insert((job_idx, tests), result.clone());
    result
}

fn tests_panel_for_selection(state: &InspectState) -> Option<Vec<Row>> {
    let node = state.nodes.get(state.selected_idx)?;
    if node.test_ref.is_some() || node.class_ref.is_some() {
        return None;
    }
    let job_idx = node.job_idx?;
    let Filter::Prefix(p) = &node.filter else {
        return None;
    };
    let job = state.jobs.get(job_idx)?;
    if job.declared != *p {
        return None;
    }
    Some(build_tests_panel_rows(job))
}

fn build_tests_panel_rows(job: &Job) -> Vec<Row> {
    let mut rows = Vec::new();
    if job.tests.is_empty() {
        rows.push(Row::CoverageLine {
            text: "No test results recorded (target/build.tests.json absent)".to_string(),
            color: theme::COMMENT,
        });
        return rows;
    }
    let passed = job
        .tests
        .iter()
        .filter(|t| t.status == TestStatus::Passed)
        .count();
    let failed = job
        .tests
        .iter()
        .filter(|t| t.status == TestStatus::Failed)
        .count();
    let skipped = job
        .tests
        .iter()
        .filter(|t| t.status == TestStatus::Skipped)
        .count();
    let color = if failed > 0 {
        theme::RED
    } else if skipped > 0 {
        theme::YELLOW
    } else {
        theme::GREEN
    };
    rows.push(Row::CoverageLine {
        text: format!(
            "Tests  {} passed  {} failed  {} skipped  — expand job ▸ class ▸ method for output",
            passed, failed, skipped
        ),
        color,
    });
    for (class_name, idxs) in group_by_class(&job.tests) {
        let f = idxs
            .iter()
            .filter(|&&i| job.tests[i].status == TestStatus::Failed)
            .count();
        let p = idxs
            .iter()
            .filter(|&&i| job.tests[i].status == TestStatus::Passed)
            .count();
        let s = idxs.len() - f - p;
        let c = if f > 0 {
            theme::RED
        } else if s > 0 {
            theme::YELLOW
        } else {
            theme::GREEN
        };
        rows.push(Row::CoverageLine {
            text: format!("  {class_name}  {p}✓ {f}✗ {s}⊘"),
            color: c,
        });
    }
    rows
}

fn build_tests_overview_rows(state: &InspectState) -> Vec<Row> {
    let mut rows = Vec::new();
    for (ji, job) in state.jobs.iter().enumerate() {
        if !job_matches(&state.filter, &job.declared) {
            continue;
        }
        if !job_search_matches(&job.declared, &state.job_search) {
            continue;
        }
        if job.tests.is_empty() {
            continue;
        }
        let _ = ji;
        rows.extend(build_tests_panel_rows(job));
    }
    if rows.is_empty() {
        rows.push(Row::CoverageLine {
            text: "No test results in this selection".to_string(),
            color: theme::COMMENT,
        });
    }
    rows
}

/// Coverage panel rows when the selected node is a job member (Coverage mode).
const COVERAGE_PANEL_LIMIT: usize = 8;

fn coverage_panel_for_selection(state: &InspectState) -> Option<Vec<Row>> {
    if state.mode != ViewMode::Coverage {
        return None;
    }
    let node = state.nodes.get(state.selected_idx)?;
    // Job-level only; skip source leaves, test nodes, and pure containers.
    if node.test_ref.is_some() || node.class_ref.is_some() || node.coverage_source_ref.is_some() {
        return None;
    }
    let job_idx = node.job_idx?;
    // Exact job node (filter matches declared path), not a directory prefix container.
    let Filter::Prefix(p) = &node.filter else {
        return None;
    };
    if state.jobs.get(job_idx)?.declared != *p {
        return None;
    }
    let cov = state.jobs.get(job_idx)?.coverage.as_ref()?;
    Some(build_coverage_panel_rows(cov))
}

fn build_coverage_panel_rows(cov: &MemberCoverage) -> Vec<Row> {
    let report = &cov.report;
    let mut rows = Vec::new();
    let summary_color = coverage_color(report.summary.line_pct());
    rows.push(Row::CoverageLine {
        text: format!(
            "Coverage  {}  — expand job ▸ source file for per-line hits",
            report.summary.summary_line()
        ),
        color: summary_color,
    });
    // Prefer source-file list when HTML pages exist.
    if !cov.sources.is_empty() {
        let mut sources: Vec<&SourceFileCoverage> = cov.sources.iter().collect();
        sources.sort_by(|a, b| {
            b.line_missed.cmp(&a.line_missed).then_with(|| {
                a.line_pct()
                    .partial_cmp(&b.line_pct())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        for src in sources.into_iter().take(COVERAGE_PANEL_LIMIT) {
            rows.push(Row::CoverageLine {
                text: format!(
                    "  {}  {:.1}% lines ({} missed)",
                    src.display_name(),
                    src.line_pct(),
                    src.line_missed,
                ),
                color: coverage_color(src.line_pct()),
            });
        }
        if cov.sources.len() > COVERAGE_PANEL_LIMIT {
            rows.push(Row::CoverageLine {
                text: format!(
                    "  … and {} more source files",
                    cov.sources.len() - COVERAGE_PANEL_LIMIT
                ),
                color: theme::COMMENT,
            });
        }
    } else {
        let uncovered = report.top_uncovered(COVERAGE_PANEL_LIMIT);
        if uncovered.is_empty() {
            rows.push(Row::CoverageLine {
                text: "  all classes fully covered".to_string(),
                color: theme::GREEN,
            });
        } else {
            for class in uncovered {
                let text = if class.has_branches() {
                    format!(
                        "  {}  {:.1}% lines ({} missed), {:.1}% branches",
                        class.qualified_name(),
                        class.line_pct(),
                        class.line_missed,
                        class.branch_pct(),
                    )
                } else {
                    format!(
                        "  {}  {:.1}% lines ({} missed)",
                        class.qualified_name(),
                        class.line_pct(),
                        class.line_missed,
                    )
                };
                rows.push(Row::CoverageLine {
                    text,
                    color: coverage_color(class.line_pct()),
                });
            }
        }
    }
    rows
}

fn coverage_color(line_pct: f64) -> Color {
    if line_pct >= 80.0 {
        theme::GREEN
    } else if line_pct >= 50.0 {
        theme::YELLOW
    } else {
        theme::RED
    }
}

fn rebuild_tree(state: &mut InspectState) {
    state.nodes = build_tree_nodes(
        &state.jobs,
        state.mode,
        &state.expanded_jobs,
        &state.expanded_classes,
    );
}

fn switch_mode(state: &mut InspectState, mode: ViewMode) {
    if state.mode == mode {
        return;
    }
    let prev_job = state.nodes.get(state.selected_idx).and_then(|n| n.job_idx);
    state.mode = mode;
    // Expansion state is mode-specific enough that a clean slate is clearer.
    state.expanded_jobs.clear();
    state.expanded_classes.clear();
    state.test_lines.clear();
    state.source_lines.clear();
    state.scroll = 0;
    state.h_scroll = 0;
    rebuild_tree(state);
    // Prefer the same job if it still exists in the new tree.
    if let Some(ji) = prev_job {
        if let Some(declared) = state.jobs.get(ji).map(|j| j.declared.clone()) {
            if let Some(ni) = find_node_for_declared(&state.nodes, &declared) {
                state.selected_idx = ni;
                apply_selection(state);
                return;
            }
        }
    }
    state.selected_idx = 0;
    apply_selection(state);
}

fn reload(state: &mut InspectState) {
    let targets = state.targets.clone();
    let action = state.action.clone();
    let utc_offset = state.utc_offset;
    let ws_root = state.ws_root.clone();
    state.descriptors = load_descriptors(&ws_root, &targets);
    state.jobs = load_jobs(&targets, &action, utc_offset, &state.descriptors);
    state.stale_jobs = collect_stale_jobs(&state.jobs);
    state.resolved_deps.clear();
    // Refresh deps views attached to jobs (already done in load_jobs).
    rebuild_tree(state);
    state.selected_idx = state.selected_idx.min(state.nodes.len().saturating_sub(1));
    rebuild_rows(state);
}

/// Focus the first failing job.  If it has failing tests, switch to Tests mode,
/// expand the job+class, and select the first failing test.
fn auto_select_failed(state: &mut InspectState) -> bool {
    let failed_job = state
        .jobs
        .iter()
        .enumerate()
        .find(|(_, j)| matches!(j.state, LogState::Failed { .. }));

    let Some((job_idx, _)) = failed_job else {
        return false;
    };

    // Look for the first failing test in this job.
    let first_failing_test = state.jobs[job_idx]
        .tests
        .iter()
        .enumerate()
        .find(|(_, t)| t.status == TestStatus::Failed)
        .map(|(i, _)| {
            let class_name = state.jobs[job_idx].tests[i].class_name.clone();
            (i, class_name)
        });

    if let Some((test_idx, class_name)) = first_failing_test {
        state.mode = ViewMode::Tests;
        state.expanded_jobs.insert(job_idx);
        state.expanded_classes.insert((job_idx, class_name.clone()));
        rebuild_tree(state);

        if let Some(ni) = state
            .nodes
            .iter()
            .position(|n| n.test_ref == Some((job_idx, test_idx)))
        {
            state.selected_idx = ni;
            apply_selection(state);
            return true;
        }
    }

    // No failing test: stay on Logs and select the failed job node.
    let declared = state.jobs[job_idx].declared.clone();
    if let Some(ni) = find_node_for_declared(&state.nodes, &declared) {
        state.selected_idx = ni;
        apply_selection(state);
    }
    true
}

fn sync_tree_scroll(state: &mut InspectState) {
    let visible = (state.pane_h as usize).saturating_sub(2); // borders
    let sel = state.selected_idx;
    if sel < state.tree_scroll {
        state.tree_scroll = sel;
    } else if visible > 0 && sel >= state.tree_scroll + visible {
        state.tree_scroll = sel + 1 - visible;
    }
}

// ── Navigation ────────────────────────────────────────────────────────────

fn next_node(nodes: &[TreeNode], from: usize) -> usize {
    let n = nodes.len();
    if n == 0 {
        return 0;
    }
    let mut i = (from + 1) % n;
    while i != from && !nodes[i].selectable {
        i = (i + 1) % n;
    }
    i
}

fn prev_node(nodes: &[TreeNode], from: usize) -> usize {
    let n = nodes.len();
    if n == 0 {
        return 0;
    }
    let mut i = (from + n - 1) % n;
    while i != from && !nodes[i].selectable {
        i = (i + n - 1) % n;
    }
    i
}

// ── Display helpers ───────────────────────────────────────────────────────

fn gutter_color(state: &LogState) -> Color {
    match state {
        LogState::Ok { .. } => theme::GREEN,
        LogState::Failed { .. } => theme::RED,
        LogState::NoMetadata => theme::YELLOW,
        LogState::NoLog => theme::COMMENT,
    }
}

fn badge_str(state: &LogState) -> String {
    match state {
        LogState::Ok { duration_ms } => format!("✓ {}", fmt_duration(*duration_ms)),
        LogState::Failed { duration_ms } => format!("✗ {}", fmt_duration(*duration_ms)),
        LogState::NoMetadata => "skipped".to_string(),
        LogState::NoLog => "(no log)".to_string(),
    }
}

fn badge_style(state: &LogState) -> Style {
    match state {
        LogState::Ok { .. } => Style::default()
            .fg(theme::GREEN)
            .add_modifier(Modifier::BOLD),
        LogState::Failed { .. } => Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        LogState::NoMetadata => Style::default()
            .fg(theme::YELLOW)
            .add_modifier(Modifier::DIM),
        LogState::NoLog => Style::default().add_modifier(Modifier::DIM),
    }
}

fn test_badge(status: &TestStatus, duration_ms: u64) -> String {
    match status {
        TestStatus::Passed => format!("✓ {}ms", duration_ms),
        TestStatus::Failed => format!("✗ {}ms", duration_ms),
        TestStatus::Skipped => "⊘ skipped".to_string(),
    }
}

#[allow(dead_code)]
fn test_badge_style(status: &TestStatus) -> Style {
    match status {
        TestStatus::Passed => Style::default().fg(theme::GREEN),
        TestStatus::Failed => Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        TestStatus::Skipped => Style::default()
            .fg(theme::YELLOW)
            .add_modifier(Modifier::DIM),
    }
}

fn fmt_duration(ms: u64) -> String {
    let tenths = ms / 100;
    format!("{}.{}s", tenths / 10, tenths % 10)
}

/// Format epoch-milliseconds as `HH:MM:SS` in the given UTC offset.
fn format_hms_local(epoch_ms: u64, offset: time::UtcOffset) -> String {
    let secs = (epoch_ms / 1000) as i64 + offset.whole_seconds() as i64;
    let time_s = secs.rem_euclid(86400) as u64;
    let h = time_s / 3600;
    let m = (time_s % 3600) / 60;
    let s = time_s % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Format epoch-milliseconds as `HH:MM:SS` in UTC (used by tests).
#[cfg(test)]
fn format_hms_utc(epoch_ms: u64) -> String {
    format_hms_local(epoch_ms, time::UtcOffset::UTC)
}

// ── Rendering ─────────────────────────────────────────────────────────────

/// Screen regions for hit-testing mouse events (mirrors `render_frame` layout).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneLayout {
    tabs: Rect,
    members: Option<Rect>,
    detail: Rect,
    search: Option<Rect>,
}

/// Compute pane rectangles for a terminal area. Kept in lockstep with `render_frame`.
fn compute_layout(area: Rect, show_members: bool, in_input: bool) -> PaneLayout {
    let constraints: Vec<Constraint> = if in_input {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ]
    };

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let body = vchunks[2];
    let (members, detail) = if show_members {
        let hchunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(body);
        (Some(hchunks[0]), hchunks[1])
    } else {
        (None, body)
    };

    PaneLayout {
        tabs: vchunks[1],
        members,
        detail,
        search: if in_input { Some(vchunks[3]) } else { None },
    }
}

/// Inner content area of a single-line border box (matches `Block::borders(ALL).inner`).
fn bordered_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// Column ranges for mode tabs, matching the text layout of `mode_tabs_line`.
/// Returns half-open `[start, end)` columns relative to the start of the tabs row.
fn mode_tab_ranges() -> Vec<(ViewMode, u16, u16)> {
    let mut ranges = Vec::with_capacity(ViewMode::ALL.len());
    // Leading "  " padding in mode_tabs_line.
    let mut col: u16 = 2;
    for (i, mode) in ViewMode::ALL.iter().enumerate() {
        if i > 0 {
            col = col.saturating_add(3); // " │ "
        }
        let label_w = (mode.label().chars().count() + 2) as u16; // " {label} "
        let end = col.saturating_add(label_w);
        ranges.push((*mode, col, end));
        col = end;
    }
    ranges
}

/// Which mode tab contains absolute terminal column `column` on the tabs row.
fn mode_tab_at(tabs: Rect, column: u16) -> Option<ViewMode> {
    if column < tabs.x || column >= tabs.x.saturating_add(tabs.width) {
        return None;
    }
    let rel = column - tabs.x;
    for (mode, start, end) in mode_tab_ranges() {
        if rel >= start && rel < end {
            return Some(mode);
        }
    }
    None
}

fn render_frame(f: &mut Frame, state: &InspectState) {
    let total = f.area();
    let in_input = state.input_mode != InputMode::Normal;
    let layout = compute_layout(total, state.show_members, in_input);

    // hint row is always y=0 (Length 1); recompute only for rendering.
    let hint_area = Rect {
        x: total.x,
        y: total.y,
        width: total.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(hint_text(state)).style(Style::default().fg(theme::COMMENT).bg(theme::BG)),
        hint_area,
    );
    f.render_widget(mode_tabs_line(state.mode), layout.tabs);

    // Body fills between tabs and optional search bar.
    let body_y = layout.tabs.y.saturating_add(layout.tabs.height);
    let body_bottom = layout
        .search
        .map(|s| s.y)
        .unwrap_or_else(|| total.y.saturating_add(total.height));
    let body = Rect {
        x: total.x,
        y: body_y,
        width: total.width,
        height: body_bottom.saturating_sub(body_y),
    };
    f.render_widget(Block::default().style(Style::default().bg(theme::BG)), body);

    if let Some(members) = layout.members {
        render_members_block(f, state, members);
        render_log_block(f, state, layout.detail);
    } else {
        render_log_block(f, state, layout.detail);
    }

    if let Some(search) = layout.search {
        render_status_bar(f, state, search);
    }
}

fn mode_tabs_line(mode: ViewMode) -> Paragraph<'static> {
    let mut spans = vec![Span::styled(
        "  ",
        Style::default().fg(theme::COMMENT).bg(theme::BG),
    )];
    for (i, m) in ViewMode::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " │ ",
                Style::default().fg(theme::COMMENT).bg(theme::BG),
            ));
        }
        let label = format!(" {} ", m.label());
        let style = if *m == mode {
            Style::default()
                .fg(theme::BG)
                .bg(theme::BLUE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::COMMENT).bg(theme::BG)
        };
        spans.push(Span::styled(label, style));
    }
    spans.push(Span::styled(
        "   1/2/3/4 or [/] switch mode",
        Style::default().fg(theme::COMMENT).bg(theme::BG),
    ));
    Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG))
}

fn hint_text(state: &InspectState) -> &'static str {
    match &state.active_pane {
        ActivePane::Search => {
            "curie inspect  type to filter  Enter apply  Tab switch pane  Esc clear  · mouse ok"
        }
        ActivePane::Members => {
            if state.input_mode != InputMode::Normal {
                "curie inspect  \u{2191}\u{2193}/jk select  Tab switch  Esc clear search  q quit  · click/wheel"
            } else {
                "curie inspect  \u{2191}\u{2193}/jk select  Enter/\u{2192} expand  click select  wheel scroll  Tab detail  q quit"
            }
        }
        ActivePane::Log => {
            if state.input_mode != InputMode::Normal {
                "curie inspect  PgUp/Dn scroll  Tab switch  Esc clear search  q quit  · click/wheel"
            } else {
                "curie inspect  jk/PgUp/Dn scroll  wheel scroll  click focus  Tab members  / grep  q quit"
            }
        }
    }
}

fn render_members_block(f: &mut Frame, state: &InspectState, area: Rect) {
    // Only highlight border when Members pane itself is focused (not when Search is active).
    let is_active = state.active_pane == ActivePane::Members;
    let border_style = if is_active {
        Style::default().fg(theme::CYAN).bg(theme::BG)
    } else {
        Style::default().fg(theme::COMMENT).bg(theme::BG)
    };
    let title = match state.mode {
        ViewMode::Logs => "Members",
        ViewMode::Tests => "Tests",
        ViewMode::Coverage => "Coverage",
        ViewMode::Deps => "Dependencies",
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(theme::BG));

    let inner = block.inner(area);
    let vis_h = inner.height as usize;
    let inner_w = inner.width as usize;
    let js = &state.job_search;
    let gm = &state.grep_job_matches;
    let stale = &state.stale_jobs;

    let lines: Vec<Line<'static>> = state
        .nodes
        .iter()
        .enumerate()
        .skip(state.tree_scroll)
        .take(vis_h)
        .map(|(i, node)| member_line(node, i == state.selected_idx, inner_w, js, gm, stale))
        .collect();

    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn member_line(
    node: &TreeNode,
    is_selected: bool,
    inner_w: usize,
    job_search: &str,
    grep_matches: &HashSet<usize>,
    stale_jobs: &HashSet<usize>,
) -> Line<'static> {
    match &node.state {
        Some(log_state) => {
            let ji = node.job_idx;
            // Dim when either search filter is active and this job doesn't match.
            let job_search_dim = !job_search.is_empty()
                && !node
                    .title
                    .to_lowercase()
                    .contains(&job_search.to_lowercase());
            let grep_dim =
                !grep_matches.is_empty() && ji.is_none_or(|i| !grep_matches.contains(&i));
            let is_stale = ji.is_some_and(|i| stale_jobs.contains(&i));
            let search_dim = job_search_dim || grep_dim;

            let state_badge = if is_stale {
                format!("{} (prev)", badge_str(log_state))
            } else {
                badge_str(log_state)
            };
            let bstyle = if is_stale || search_dim {
                Style::default().fg(theme::COMMENT)
            } else {
                badge_style(log_state)
            };

            // Right-aligned: optional coverage badge, then state badge.
            let cov = node.coverage_badge.as_deref().unwrap_or("");
            let cov_w = if cov.is_empty() {
                0
            } else {
                cov.chars().count() + 1
            };
            let badge_w = state_badge.chars().count() + cov_w;
            let label_w = inner_w.saturating_sub(badge_w + 1);
            let label: String = node.label.chars().take(label_w).collect();
            let padding = " ".repeat(label_w.saturating_sub(label.chars().count()) + 1);
            let label_style = if is_stale || search_dim {
                Style::default().fg(theme::COMMENT)
            } else {
                Style::default().fg(theme::FG)
            };
            let cov_style = if is_stale || search_dim {
                Style::default().fg(theme::COMMENT)
            } else {
                Style::default().fg(theme::CYAN)
            };

            let mut spans = vec![
                Span::styled(label, label_style),
                Span::styled(padding, label_style),
            ];
            if !cov.is_empty() {
                spans.push(Span::styled(format!("{cov} "), cov_style));
            }
            spans.push(Span::styled(state_badge, bstyle));
            let mut line = Line::from(spans);
            if is_selected {
                line = line.patch_style(Style::default().bg(theme::SELECTION));
            }
            line
        }
        None => {
            if let Some((badge_text, status)) = &node.test_badge {
                // Test leaf / Tests-mode job summary: right-aligned coloured badge.
                let badge_color = match status {
                    TestStatus::Passed => theme::GREEN,
                    TestStatus::Failed => theme::RED,
                    TestStatus::Skipped => theme::YELLOW,
                };
                let badge_modifier = if *status == TestStatus::Failed {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                };
                let bstyle = Style::default()
                    .fg(badge_color)
                    .add_modifier(badge_modifier);
                right_aligned_badge_line(&node.label, badge_text, bstyle, inner_w, is_selected)
            } else if let Some(badge_text) = &node.coverage_badge {
                // Coverage source file: cyan badge with line/branch %.
                let bstyle = Style::default().fg(theme::CYAN);
                right_aligned_badge_line(&node.label, badge_text, bstyle, inner_w, is_selected)
            } else if let Some(badge_text) = &node.deps_badge {
                // Deps mode: count or version badge.
                let bstyle = Style::default().fg(theme::MAGENTA);
                right_aligned_badge_line(&node.label, badge_text, bstyle, inner_w, is_selected)
            } else {
                // Class group node or directory container: dimmed.
                let style = if is_selected {
                    Style::default().fg(theme::FG).bg(theme::SELECTION)
                } else {
                    Style::default().fg(theme::COMMENT)
                };
                Line::styled(node.label.clone(), style)
            }
        }
    }
}

fn right_aligned_badge_line(
    label_raw: &str,
    badge_text: &str,
    badge_style: Style,
    inner_w: usize,
    is_selected: bool,
) -> Line<'static> {
    let badge_w = badge_text.chars().count();
    let label_w = inner_w.saturating_sub(badge_w + 1);
    let label: String = label_raw.chars().take(label_w).collect();
    let padding = " ".repeat(label_w.saturating_sub(label.chars().count()) + 1);
    let label_style = Style::default().fg(theme::FG);
    let mut line = Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(padding, label_style),
        Span::styled(badge_text.to_string(), badge_style),
    ]);
    if is_selected {
        line = line.patch_style(Style::default().bg(theme::SELECTION));
    }
    line
}

/// Virtualized log renderer: only converts the visible row slice to `Line`s (O(vis_h)).
fn render_log_block(f: &mut Frame, state: &InspectState, area: Rect) {
    // Only highlight border when Log pane itself is focused (not when Search is active).
    let is_active = !state.show_members || state.active_pane == ActivePane::Log;
    let border_style = if is_active {
        Style::default().fg(theme::CYAN).bg(theme::BG)
    } else {
        Style::default().fg(theme::COMMENT).bg(theme::BG)
    };

    let title = format!("{}: {}", state.mode.detail_title_prefix(), state.log_title);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(theme::BG));

    let inner = block.inner(area);
    let vis_h = inner.height as usize;
    let start = state.scroll.min(state.rows.len());
    let end = (start + vis_h).min(state.rows.len());

    let h_off = state.h_scroll;
    let lines: Vec<Line<'static>> = state.rows[start..end]
        .iter()
        .map(|row| {
            let line = render_row(
                row,
                &state.jobs,
                &state.test_lines,
                &state.source_lines,
                &state.grep,
            );
            trim_line_left(line, h_off)
        })
        .collect();

    f.render_widget(block, area);
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Remove the first `offset` characters from a line's span list, preserving per-span styles.
fn trim_line_left(line: Line<'static>, offset: usize) -> Line<'static> {
    if offset == 0 {
        return line;
    }
    let mut skip = offset;
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .filter_map(|span| {
            let n = span.content.chars().count();
            if skip >= n {
                skip -= n;
                None
            } else if skip > 0 {
                let trimmed: String = span.content.chars().skip(skip).collect();
                skip = 0;
                Some(Span::styled(trimmed, span.style))
            } else {
                Some(span)
            }
        })
        .collect();
    Line::from(spans)
}

fn render_row(
    row: &Row,
    jobs: &[Job],
    test_lines: &[String],
    source_lines: &[SourceLine],
    grep: &str,
) -> Line<'static> {
    match row {
        Row::Header { job } => {
            let j = &jobs[*job];
            let color = gutter_color(&j.state);
            header_line(j, color)
        }
        Row::Body { job, line } => {
            let j = &jobs[*job];
            let color = gutter_color(&j.state);
            body_line(&j.lines[*line], color, grep)
        }
        Row::TestAnnotation { text, color } => {
            let gutter = Span::styled(
                "▌ ",
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            );
            let msg = Span::styled(
                text.clone(),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            );
            Line::from(vec![gutter, msg]).style(Style::default().bg(theme::SURFACE))
        }
        Row::TestBody { line } => {
            let text = test_lines.get(*line).map(String::as_str).unwrap_or("");
            body_line(text, theme::CYAN, grep)
        }
        Row::CoverageLine { text, color } => {
            let gutter = Span::styled(
                "▌ ",
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            );
            let msg = Span::styled(text.clone(), Style::default().fg(*color));
            Line::from(vec![gutter, msg]).style(Style::default().bg(theme::SURFACE))
        }
        Row::SourceBody { line } => render_source_line(source_lines.get(*line), grep),
    }
}

fn render_source_line(src: Option<&SourceLine>, grep: &str) -> Line<'static> {
    let Some(src) = src else {
        return Line::from("");
    };
    let (gutter_color, text_style, marker) = match src.hit {
        LineHit::Full => (theme::GREEN, Style::default().fg(theme::GREEN), "█"),
        LineHit::Partial => (theme::YELLOW, Style::default().fg(theme::YELLOW), "▒"),
        LineHit::Missed => (theme::RED, Style::default().fg(theme::RED), "█"),
        LineHit::None => (theme::COMMENT, Style::default().fg(theme::COMMENT), "·"),
    };
    let gutter = Span::styled(
        format!("{marker} {:>4} ", src.number),
        Style::default()
            .fg(gutter_color)
            .add_modifier(Modifier::BOLD),
    );
    let body_text = if let Some(title) = &src.title {
        // Keep the branch tooltip on partial/missed lines for context.
        if src.hit == LineHit::Partial || src.hit == LineHit::Missed {
            format!("{}  // {}", src.text, title)
        } else {
            src.text.clone()
        }
    } else {
        src.text.clone()
    };
    let body = if grep.is_empty() {
        vec![Span::styled(body_text, text_style)]
    } else {
        highlight_spans(vec![Span::styled(body_text, text_style)], grep)
    };
    let mut spans = vec![gutter];
    spans.extend(body);
    Line::from(spans)
}

fn header_line(job: &Job, color: Color) -> Line<'static> {
    let gutter = Span::styled(
        "▌ ",
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let content = if job.started_disp.is_empty() {
        format!("{}  {}", job.declared, badge_str(&job.state))
    } else {
        format!(
            "{}  started {}  {}",
            job.declared,
            job.started_disp,
            badge_str(&job.state)
        )
    };
    let text = Span::styled(
        content,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    Line::from(vec![gutter, text]).style(Style::default().bg(theme::SURFACE))
}

fn body_line(text: &str, color: Color, grep: &str) -> Line<'static> {
    let gutter = Span::styled(
        "▌ ",
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let body_spans = parse_ansi_line(text);
    let mut spans = vec![gutter];
    if grep.is_empty() {
        spans.extend(body_spans);
    } else {
        spans.extend(highlight_spans(body_spans, grep));
    }
    Line::from(spans)
}

/// Split `spans` at occurrences of `pattern` and apply a yellow highlight to each match.
fn highlight_spans(spans: Vec<Span<'static>>, pattern: &str) -> Vec<Span<'static>> {
    let pattern_lower = pattern.to_lowercase();
    let highlight = Style::default().bg(theme::YELLOW).fg(theme::BG);
    let mut result = Vec::new();

    for span in spans {
        let content = span.content.to_string();
        let content_lower = content.to_lowercase();
        if content_lower.contains(&pattern_lower) {
            split_span_at_pattern(
                &content,
                &content_lower,
                &pattern_lower,
                span.style,
                highlight,
                &mut result,
            );
        } else {
            result.push(span);
        }
    }
    result
}

fn split_span_at_pattern(
    content: &str,
    content_lower: &str,
    pattern_lower: &str,
    base_style: Style,
    highlight: Style,
    out: &mut Vec<Span<'static>>,
) {
    let pat_len = pattern_lower.len();
    let mut pos = 0;
    loop {
        match content_lower[pos..].find(pattern_lower) {
            None => {
                if pos < content.len() {
                    out.push(Span::styled(content[pos..].to_string(), base_style));
                }
                break;
            }
            Some(rel) => {
                let start = pos + rel;
                let end = start + pat_len;
                if start > pos {
                    out.push(Span::styled(content[pos..start].to_string(), base_style));
                }
                out.push(Span::styled(content[start..end].to_string(), highlight));
                pos = end;
            }
        }
    }
}

fn parse_ansi_line(s: &str) -> Vec<Span<'static>> {
    match s.into_text() {
        Ok(mut text) => text.lines.pop().map(|l| l.spans).unwrap_or_default(),
        Err(_) => vec![Span::raw(s.to_string())],
    }
}

fn render_status_bar(f: &mut Frame, state: &InspectState, area: Rect) {
    let content = match &state.input_mode {
        InputMode::Grep => format!("/ {}█", state.grep),
        InputMode::JobSearch => format!("f {}█", state.job_search),
        InputMode::Normal => return,
    };
    // Cyan background when the search bar has keyboard focus; dimmer when a pane does.
    let style = if state.active_pane == ActivePane::Search {
        Style::default().fg(theme::BG).bg(theme::CYAN)
    } else {
        Style::default().fg(theme::COMMENT).bg(theme::BG)
    };
    f.render_widget(Paragraph::new(content).style(style), area);
}

const H_SCROLL_STEP: usize = 4;
/// Lines scrolled per mouse-wheel notch.
const MOUSE_SCROLL_STEP: usize = 3;

// ── Event loop ────────────────────────────────────────────────────────────

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut InspectState,
) -> Result<()> {
    loop {
        terminal.draw(|f| render_frame(f, state))?;

        let size = terminal.size()?;
        // Chrome: hint row + mode tabs (+ optional search bar).
        let in_input = state.input_mode != InputMode::Normal;
        let chrome = if in_input { 3 } else { 2 };
        state.pane_h = size.height.saturating_sub(chrome);
        state.log_vis_h = (state.pane_h as usize).saturating_sub(2);

        let layout = compute_layout(
            Rect::new(0, 0, size.width, size.height),
            state.show_members,
            in_input,
        );

        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if !handle_key(state, key) {
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => {
                handle_mouse(state, mouse, &layout);
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

/// Handle a mouse event against the current pane layout. Always continues the loop.
fn handle_mouse(state: &mut InspectState, mouse: MouseEvent, layout: &PaneLayout) {
    let pos = Position {
        x: mouse.column,
        y: mouse.row,
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => handle_mouse_click(state, pos, layout),
        MouseEventKind::ScrollUp => handle_mouse_scroll(state, pos, layout, -1),
        MouseEventKind::ScrollDown => handle_mouse_scroll(state, pos, layout, 1),
        MouseEventKind::ScrollLeft => {
            if layout.detail.contains(pos) {
                state.h_scroll = state.h_scroll.saturating_sub(H_SCROLL_STEP);
            }
        }
        MouseEventKind::ScrollRight if layout.detail.contains(pos) => {
            state.h_scroll += H_SCROLL_STEP;
        }
        _ => {}
    }
}

fn handle_mouse_click(state: &mut InspectState, pos: Position, layout: &PaneLayout) {
    // Mode tabs.
    if layout.tabs.contains(pos) {
        if let Some(mode) = mode_tab_at(layout.tabs, pos.x) {
            switch_mode(state, mode);
        }
        return;
    }

    // Search bar (when visible).
    if let Some(search) = layout.search {
        if search.contains(pos) {
            state.active_pane = ActivePane::Search;
            return;
        }
    }

    // Members tree.
    if let Some(members) = layout.members {
        if members.contains(pos) {
            state.active_pane = ActivePane::Members;
            state.show_members = true;
            let inner = bordered_inner(members);
            if !inner.contains(pos) {
                return;
            }
            let row = state.tree_scroll + (pos.y - inner.y) as usize;
            if row >= state.nodes.len() || !state.nodes[row].selectable {
                return;
            }
            // Second click on the same expandable node toggles expand/collapse.
            if row == state.selected_idx {
                toggle_test_expansion(state);
            } else {
                state.selected_idx = row;
                apply_selection(state);
            }
            return;
        }
    }

    // Detail / log pane.
    if layout.detail.contains(pos) {
        state.active_pane = ActivePane::Log;
    }
}

fn handle_mouse_scroll(
    state: &mut InspectState,
    pos: Position,
    layout: &PaneLayout,
    direction: i32,
) {
    let step = MOUSE_SCROLL_STEP as i32 * direction;
    if let Some(members) = layout.members {
        if members.contains(pos) {
            scroll_tree(state, step);
            return;
        }
    }
    // Default: scroll the detail pane (also when members are hidden).
    if layout.detail.contains(pos) || layout.members.is_none() {
        scroll_detail(state, step);
    }
}

fn scroll_tree(state: &mut InspectState, delta: i32) {
    let visible = (state.pane_h as usize).saturating_sub(2).max(1);
    let max_scroll = state.nodes.len().saturating_sub(visible);
    if delta < 0 {
        state.tree_scroll = state.tree_scroll.saturating_sub((-delta) as usize);
    } else {
        state.tree_scroll = (state.tree_scroll + delta as usize).min(max_scroll);
    }
}

fn scroll_detail(state: &mut InspectState, delta: i32) {
    let max = state.rows.len().saturating_sub(1);
    if delta < 0 {
        state.scroll = state.scroll.saturating_sub((-delta) as usize);
    } else {
        state.scroll = (state.scroll + delta as usize).min(max);
    }
}

fn handle_key(state: &mut InspectState, key: KeyEvent) -> bool {
    if state.active_pane == ActivePane::Search {
        return handle_key_search(state, key);
    }

    let members_active = state.show_members && state.active_pane == ActivePane::Members;
    let searching = state.input_mode != InputMode::Normal;
    let log_ph = state.log_vis_h.max(1);

    match key.code {
        // Esc with active search: clear search instead of quitting.
        KeyCode::Esc if searching => {
            clear_search(state);
        }
        KeyCode::Char('q') | KeyCode::Esc => return false,

        KeyCode::Tab => cycle_panes(state),

        // m: toggle Members pane visibility.
        KeyCode::Char('m') => toggle_members(state),

        // Members navigation.
        KeyCode::Up | KeyCode::Char('k') if members_active => {
            state.selected_idx = prev_node(&state.nodes, state.selected_idx);
            apply_selection(state);
        }
        KeyCode::Down | KeyCode::Char('j') if members_active => {
            state.selected_idx = next_node(&state.nodes, state.selected_idx);
            apply_selection(state);
        }
        KeyCode::Enter | KeyCode::Right if members_active => {
            toggle_test_expansion(state);
        }
        KeyCode::Left if members_active => {
            collapse_test_node(state);
        }

        // Horizontal log scroll (log pane only; members pane uses Left/Right for tree).
        KeyCode::Left => {
            state.h_scroll = state.h_scroll.saturating_sub(H_SCROLL_STEP);
        }
        KeyCode::Right => {
            state.h_scroll += H_SCROLL_STEP;
        }

        // Log scrolling.
        KeyCode::Char('k') | KeyCode::Up => {
            state.scroll = state.scroll.saturating_sub(1);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let max = state.rows.len().saturating_sub(1);
            state.scroll = (state.scroll + 1).min(max);
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(log_ph);
        }
        KeyCode::PageDown => {
            let max = state.rows.len().saturating_sub(1);
            state.scroll = (state.scroll + log_ph).min(max);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = state.scroll.saturating_sub((log_ph / 2).max(1));
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let max = state.rows.len().saturating_sub(1);
            state.scroll = (state.scroll + (log_ph / 2).max(1)).min(max);
        }
        KeyCode::Char('g') => {
            state.scroll = 0;
        }
        KeyCode::Char('G') => {
            state.scroll = state.rows.len().saturating_sub(1);
        }

        KeyCode::Char('r') => reload(state),

        // Top-level view mode: Logs / Tests / Coverage / Deps.
        KeyCode::Char(c @ '1'..='4') if !searching => {
            if let Some(mode) = ViewMode::from_digit(c) {
                switch_mode(state, mode);
            }
        }
        KeyCode::Char(']') if !searching => switch_mode(state, state.mode.next()),
        KeyCode::Char('[') if !searching => switch_mode(state, state.mode.prev()),

        KeyCode::Char('/') => {
            state.pre_search_pane = state.active_pane.clone();
            state.input_mode = InputMode::Grep;
            state.active_pane = ActivePane::Search;
            state.grep.clear();
            rebuild_rows(state);
        }
        KeyCode::Char('f') => {
            state.pre_search_pane = state.active_pane.clone();
            state.input_mode = InputMode::JobSearch;
            state.active_pane = ActivePane::Search;
            state.job_search.clear();
            rebuild_rows(state);
        }

        _ => {}
    }
    true
}

fn handle_key_search(state: &mut InspectState, key: KeyEvent) -> bool {
    let log_ph = state.log_vis_h.max(1);

    match key.code {
        KeyCode::Esc => {
            clear_search(state);
        }
        KeyCode::Enter => {
            // Confirm search and return focus to the previous pane.
            state.active_pane = state.pre_search_pane.clone();
        }
        KeyCode::Tab => {
            // Cycle: Search → Members (or Log if members hidden) → Log → Search.
            cycle_panes(state);
        }
        KeyCode::Backspace => {
            match state.input_mode {
                InputMode::Grep => {
                    state.grep.pop();
                }
                InputMode::JobSearch => {
                    state.job_search.pop();
                }
                _ => {}
            }
            rebuild_rows(state);
        }
        KeyCode::Char(c) => {
            match state.input_mode {
                InputMode::Grep => state.grep.push(c),
                InputMode::JobSearch => state.job_search.push(c),
                _ => {}
            }
            rebuild_rows(state);
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(log_ph);
        }
        KeyCode::PageDown => {
            let max = state.rows.len().saturating_sub(1);
            state.scroll = (state.scroll + log_ph).min(max);
        }
        KeyCode::Left => {
            state.h_scroll = state.h_scroll.saturating_sub(H_SCROLL_STEP);
        }
        KeyCode::Right => {
            state.h_scroll += H_SCROLL_STEP;
        }
        _ => {}
    }
    true
}

/// Toggle expansion at the current tree level (job → children, class → tests).
fn toggle_test_expansion(state: &mut InspectState) {
    if state.mode == ViewMode::Logs || state.mode == ViewMode::Deps {
        return;
    }
    let node = &state.nodes[state.selected_idx];

    if node.test_ref.is_some() || node.coverage_source_ref.is_some() {
        // Already at a leaf; Enter/→ has no further expansion.
        return;
    }

    if let Some((job_idx, class_name)) = node.class_ref.clone() {
        // Class node: toggle class expansion.
        let key = (job_idx, class_name.clone());
        if state.expanded_classes.contains(&key) {
            state.expanded_classes.remove(&key);
        } else {
            state.expanded_classes.insert(key);
        }
        rebuild_tree_and_reselect_class(state, job_idx, &class_name);
        return;
    }

    // Job node: toggle job expansion.
    if let Some(job_idx) = job_idx_for_node(state) {
        if state.expanded_jobs.contains(&job_idx) {
            state.expanded_jobs.remove(&job_idx);
        } else {
            state.expanded_jobs.insert(job_idx);
        }
        rebuild_tree_and_reselect(state, Some(job_idx));
    }
}

/// Collapse: ← on a leaf → collapse parent; on a group → collapse parent job.
fn collapse_test_node(state: &mut InspectState) {
    if state.mode == ViewMode::Logs || state.mode == ViewMode::Deps {
        return;
    }
    let node = &state.nodes[state.selected_idx];

    if let Some((job_idx, test_idx)) = node.test_ref {
        // On a test leaf: collapse the parent class.
        let class_name = state.jobs[job_idx].tests[test_idx].class_name.clone();
        state
            .expanded_classes
            .remove(&(job_idx, class_name.clone()));
        rebuild_tree_and_reselect_class(state, job_idx, &class_name);
    } else if let Some((job_idx, _)) = node.coverage_source_ref {
        // On a coverage source: collapse the parent job.
        state.expanded_jobs.remove(&job_idx);
        rebuild_tree_and_reselect(state, Some(job_idx));
    } else if let Some((job_idx, _)) = node.class_ref.clone() {
        // On a class node: collapse the parent job.
        state.expanded_jobs.remove(&job_idx);
        rebuild_tree_and_reselect(state, Some(job_idx));
    }
}

/// Find the job index that the current node maps to (for job-level nodes only).
fn job_idx_for_node(state: &InspectState) -> Option<usize> {
    let node = &state.nodes[state.selected_idx];
    if node.test_ref.is_some() || node.coverage_source_ref.is_some() || node.class_ref.is_some() {
        return None;
    }
    let Filter::Prefix(p) = &node.filter else {
        return None;
    };
    state.jobs.iter().position(|j| &j.declared == p)
}

/// Rebuild tree nodes and re-select the job node for `job_idx` (or keep current selection).
fn rebuild_tree_and_reselect(state: &mut InspectState, job_idx: Option<usize>) {
    let prev_declared = job_idx.map(|i| state.jobs[i].declared.clone());
    rebuild_tree(state);
    if let Some(declared) = prev_declared {
        if let Some(ni) = find_node_for_declared(&state.nodes, &declared) {
            state.selected_idx = ni;
        }
    }
    state.selected_idx = state.selected_idx.min(state.nodes.len().saturating_sub(1));
    apply_selection(state);
}

/// Rebuild tree nodes and re-select the class node for `(job_idx, class_name)`.
fn rebuild_tree_and_reselect_class(state: &mut InspectState, job_idx: usize, class_name: &str) {
    rebuild_tree(state);
    if let Some(ni) = state.nodes.iter().position(|n| {
        n.class_ref
            .as_ref()
            .is_some_and(|(ji, cn)| *ji == job_idx && cn == class_name)
    }) {
        state.selected_idx = ni;
    }
    state.selected_idx = state.selected_idx.min(state.nodes.len().saturating_sub(1));
    apply_selection(state);
}

/// Cycle panes in order: Members → Log → (Search if active) → Members.
fn cycle_panes(state: &mut InspectState) {
    let searching = state.input_mode != InputMode::Normal;
    state.active_pane = match &state.active_pane {
        ActivePane::Members => ActivePane::Log,
        ActivePane::Log => {
            if searching {
                ActivePane::Search
            } else if state.show_members {
                ActivePane::Members
            } else {
                ActivePane::Log
            }
        }
        ActivePane::Search => {
            if state.show_members {
                ActivePane::Members
            } else {
                ActivePane::Log
            }
        }
    };
    // Switching to Members implicitly shows the pane.
    if state.active_pane == ActivePane::Members {
        state.show_members = true;
    }
}

/// `m` key: toggle Members pane visibility.
fn toggle_members(state: &mut InspectState) {
    if state.show_members {
        state.show_members = false;
        if state.active_pane == ActivePane::Members {
            state.active_pane = ActivePane::Log;
        }
    } else {
        state.show_members = true;
        state.active_pane = ActivePane::Members;
    }
}

/// Clear the active search pattern and return focus to the pre-search pane.
fn clear_search(state: &mut InspectState) {
    match state.input_mode {
        InputMode::Grep => state.grep.clear(),
        InputMode::JobSearch => state.job_search.clear(),
        InputMode::Normal => {}
    }
    state.input_mode = InputMode::Normal;
    state.active_pane = state.pre_search_pane.clone();
    rebuild_rows(state);
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::CoverageReport;

    fn make_job(declared: &str, started_ms: Option<u64>, exit_code: Option<i32>) -> Job {
        let state = match exit_code {
            Some(0) => LogState::Ok { duration_ms: 1000 },
            Some(_) => LogState::Failed { duration_ms: 800 },
            None => LogState::NoLog,
        };
        let started_disp = started_ms.map(format_hms_utc).unwrap_or_default();
        Job {
            declared: declared.to_string(),
            state,
            started_ms,
            started_disp,
            lines: vec!["line one".to_string(), "line two".to_string()],
            build_id: None,
            tests: vec![],
            coverage: None,
            deps: None,
        }
    }

    fn sample_deps_view() -> MemberDepsView {
        MemberDepsView {
            kind_label: "application".into(),
            name: "lib".into(),
            version: "1.0.0".into(),
            compile: vec![
                crate::deps::DepItem {
                    coord: "com.example:core".into(),
                    version: "1.2.3".into(),
                    note: String::new(),
                },
                crate::deps::DepItem {
                    coord: "com.fasterxml.jackson.core:jackson-databind".into(),
                    version: String::new(),
                    note: String::new(),
                },
            ],
            test: vec![crate::deps::DepItem {
                coord: "org.junit.jupiter:junit-jupiter".into(),
                version: String::new(),
                note: String::new(),
            }],
        }
    }

    fn sample_coverage_report() -> MemberCoverage {
        use crate::coverage::{ClassCoverage, CoverageSummary};
        MemberCoverage {
            report: CoverageReport {
                summary: CoverageSummary {
                    line_covered: 80,
                    line_missed: 20,
                    branch_covered: 6,
                    branch_missed: 4,
                },
                classes: vec![
                    ClassCoverage {
                        package: "com.example".into(),
                        class_name: "Foo".into(),
                        line_covered: 5,
                        line_missed: 15,
                        branch_covered: 1,
                        branch_missed: 3,
                    },
                    ClassCoverage {
                        package: "com.example".into(),
                        class_name: "Bar".into(),
                        line_covered: 75,
                        line_missed: 5,
                        branch_covered: 5,
                        branch_missed: 1,
                    },
                ],
            },
            sources: vec![],
        }
    }

    fn sample_member_with_sources(html_path: std::path::PathBuf) -> MemberCoverage {
        use crate::coverage::{ClassCoverage, CoverageSummary, SourceFileCoverage};
        MemberCoverage {
            report: CoverageReport {
                summary: CoverageSummary {
                    line_covered: 9,
                    line_missed: 1,
                    branch_covered: 3,
                    branch_missed: 1,
                },
                classes: vec![ClassCoverage {
                    package: "com.example".into(),
                    class_name: "Foo".into(),
                    line_covered: 9,
                    line_missed: 1,
                    branch_covered: 3,
                    branch_missed: 1,
                }],
            },
            sources: vec![SourceFileCoverage {
                package: "com.example".into(),
                file_name: "Foo.java".into(),
                html_path,
                line_covered: 9,
                line_missed: 1,
                branch_covered: 3,
                branch_missed: 1,
            }],
        }
    }

    // ── build_tree_nodes ─────────────────────────────────────────────────

    #[test]
    fn tree_root_then_flat_members() {
        let jobs = vec![
            make_job("alpha", None, Some(0)),
            make_job("beta", None, Some(0)),
        ];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes[0].title, "all jobs");
        assert!(matches!(nodes[0].filter, Filter::All));
        assert_eq!(nodes[1].label.trim(), "alpha");
        assert_eq!(nodes[2].label.trim(), "beta");
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn tree_nested_prefix() {
        let jobs = vec![
            make_job("services/api", None, Some(0)),
            make_job("services/web", None, Some(0)),
        ];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 4);
        assert!(nodes[1].label.contains("services/"));
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "services"));
        assert!(matches!(&nodes[2].filter, Filter::Prefix(p) if p == "services/api"));
        assert!(matches!(&nodes[3].filter, Filter::Prefix(p) if p == "services/web"));
    }

    #[test]
    fn tree_single_member() {
        let jobs = vec![make_job("mylib", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 2);
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "mylib"));
    }

    #[test]
    fn tree_deep_nesting() {
        let jobs = vec![make_job("a/b/c/leaf", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 5);
        assert!(matches!(&nodes[4].filter, Filter::Prefix(p) if p == "a/b/c/leaf"));
    }

    #[test]
    fn tree_container_filter_prefix() {
        let jobs = vec![
            make_job("svc/api", None, Some(0)),
            make_job("svc/web", None, Some(0)),
        ];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        let svc = nodes.iter().find(|n| n.label.contains("svc/")).unwrap();
        assert!(matches!(&svc.filter, Filter::Prefix(p) if p == "svc"));
    }

    // ── job_matches ──────────────────────────────────────────────────────

    #[test]
    fn match_all() {
        assert!(job_matches(&Filter::All, "anything"));
        assert!(job_matches(&Filter::All, "deep/nested/path"));
    }

    #[test]
    fn match_exact_leaf() {
        let f = Filter::Prefix("services/api".to_string());
        assert!(job_matches(&f, "services/api"));
        assert!(!job_matches(&f, "services/web"));
        assert!(!job_matches(&f, "services"));
    }

    #[test]
    fn match_subtree() {
        let f = Filter::Prefix("services".to_string());
        assert!(job_matches(&f, "services/api"));
        assert!(job_matches(&f, "services/web"));
        assert!(!job_matches(&f, "services-extra")); // must be a path boundary
    }

    #[test]
    fn match_prefix_not_a_path_boundary() {
        let f = Filter::Prefix("svc".to_string());
        assert!(!job_matches(&f, "svc-extra"));
        assert!(job_matches(&f, "svc/child"));
        assert!(job_matches(&f, "svc"));
    }

    // ── job_search_matches ───────────────────────────────────────────────

    #[test]
    fn job_search_empty_matches_all() {
        assert!(job_search_matches("services/api", ""));
        assert!(job_search_matches("anything", ""));
    }

    #[test]
    fn job_search_case_insensitive() {
        assert!(job_search_matches("Services/Api", "api"));
        assert!(job_search_matches("services/api", "API"));
        assert!(!job_search_matches("services/web", "api"));
    }

    // ── build_rows ───────────────────────────────────────────────────────

    #[test]
    fn rows_sorted_by_start_time() {
        let jobs = vec![
            make_job("b", Some(2000), Some(0)),
            make_job("a", Some(1000), Some(0)),
        ];
        let rows = build_rows(&jobs, &Filter::All, "", "");
        assert!(matches!(rows[0], Row::Header { job: 1 }));
        assert!(matches!(rows[3], Row::Header { job: 0 }));
    }

    #[test]
    fn rows_filtered_by_prefix() {
        let jobs = vec![
            make_job("svc/api", Some(1000), Some(0)),
            make_job("svc/web", Some(2000), Some(0)),
            make_job("app", Some(3000), Some(0)),
        ];
        let f = Filter::Prefix("svc".to_string());
        let rows = build_rows(&jobs, &f, "", "");
        let headers: Vec<usize> = rows
            .iter()
            .filter_map(|r| {
                if let Row::Header { job } = r {
                    Some(*job)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(headers, vec![0, 1]);
    }

    #[test]
    fn rows_unknown_start_sorts_last() {
        let jobs = vec![
            make_job("notime", None, Some(0)),
            make_job("known", Some(1000), Some(0)),
        ];
        let rows = build_rows(&jobs, &Filter::All, "", "");
        let headers: Vec<usize> = rows
            .iter()
            .filter_map(|r| {
                if let Row::Header { job } = r {
                    Some(*job)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(headers[0], 1, "known-start job must sort first");
        assert_eq!(headers[1], 0, "no-meta job must sort last");
    }

    #[test]
    fn rows_all_includes_every_job() {
        let jobs = vec![
            make_job("a", Some(1000), Some(0)),
            make_job("b", Some(2000), Some(0)),
            make_job("c", Some(3000), Some(0)),
        ];
        let rows = build_rows(&jobs, &Filter::All, "", "");
        let headers = rows
            .iter()
            .filter(|r| matches!(r, Row::Header { .. }))
            .count();
        assert_eq!(headers, 3);
    }

    #[test]
    fn rows_grep_filters_body_lines() {
        let mut jobs = vec![make_job("a", Some(1000), Some(0))];
        jobs[0].lines = vec![
            "hello world".to_string(),
            "something else".to_string(),
            "hello again".to_string(),
        ];
        let rows = build_rows(&jobs, &Filter::All, "hello", "");
        assert_eq!(rows.len(), 3); // header + 2 matching body rows
        assert!(matches!(rows[0], Row::Header { job: 0 }));
        assert!(matches!(rows[1], Row::Body { job: 0, line: 0 }));
        assert!(matches!(rows[2], Row::Body { job: 0, line: 2 }));
    }

    #[test]
    fn rows_grep_excludes_job_with_no_match() {
        let mut jobs = vec![
            make_job("a", Some(1000), Some(0)),
            make_job("b", Some(2000), Some(0)),
        ];
        jobs[1].lines = vec!["different content".to_string(), "no match here".to_string()];
        let rows = build_rows(&jobs, &Filter::All, "line", "");
        let headers: Vec<usize> = rows
            .iter()
            .filter_map(|r| {
                if let Row::Header { job } = r {
                    Some(*job)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(headers, vec![0]); // only job a whose lines contain "line"
    }

    #[test]
    fn rows_job_search_filter() {
        let jobs = vec![
            make_job("services/api", Some(1000), Some(0)),
            make_job("services/web", Some(2000), Some(0)),
            make_job("app", Some(3000), Some(0)),
        ];
        let rows = build_rows(&jobs, &Filter::All, "", "api");
        let headers: Vec<usize> = rows
            .iter()
            .filter_map(|r| {
                if let Row::Header { job } = r {
                    Some(*job)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(headers, vec![0]); // only services/api
    }

    // ── navigation ───────────────────────────────────────────────────────

    #[test]
    fn next_node_wraps() {
        let jobs = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(next_node(&nodes, 2), 0);
    }

    #[test]
    fn prev_node_wraps() {
        let jobs = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, ViewMode::Logs, &HashSet::new(), &HashSet::new());
        assert_eq!(prev_node(&nodes, 0), 2);
    }

    // ── display helpers ──────────────────────────────────────────────────

    #[test]
    fn fmt_duration_sub_second() {
        assert_eq!(fmt_duration(800), "0.8s");
    }

    #[test]
    fn fmt_duration_over_second() {
        assert_eq!(fmt_duration(1300), "1.3s");
        assert_eq!(fmt_duration(2100), "2.1s");
        assert_eq!(fmt_duration(12345), "12.3s");
    }

    #[test]
    fn format_hms_utc_midnight() {
        assert_eq!(format_hms_utc(0), "00:00:00");
    }

    #[test]
    fn format_hms_utc_known_time() {
        // 12*3600 + 34*60 + 1 = 45241 seconds past midnight
        assert_eq!(format_hms_utc(45241 * 1000), "12:34:01");
    }

    #[test]
    fn format_hms_local_positive_offset() {
        let offset = time::UtcOffset::from_whole_seconds(7200).unwrap(); // UTC+2
        assert_eq!(format_hms_local(0, offset), "02:00:00");
    }

    #[test]
    fn format_hms_local_negative_offset() {
        let offset = time::UtcOffset::from_whole_seconds(-18000).unwrap(); // UTC-5
                                                                           // 12:00:00 UTC → 07:00:00 local
        assert_eq!(format_hms_local(43200 * 1000, offset), "07:00:00");
    }

    // ── badge_str ────────────────────────────────────────────────────────

    #[test]
    fn badge_str_no_metadata() {
        assert_eq!(badge_str(&LogState::NoMetadata), "skipped");
    }

    // ── highlight_spans ──────────────────────────────────────────────────

    #[test]
    fn highlight_spans_no_match() {
        let spans = vec![Span::raw("hello world")];
        let result = highlight_spans(spans, "xyz");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
    }

    #[test]
    fn highlight_spans_single_match() {
        let spans = vec![Span::raw("hello world")];
        let result = highlight_spans(spans, "world");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hello ");
        assert_eq!(result[1].content, "world");
        assert_eq!(
            result[1].style,
            Style::default().bg(theme::YELLOW).fg(theme::BG)
        );
    }

    #[test]
    fn highlight_spans_case_insensitive() {
        let spans = vec![Span::raw("Hello World")];
        let result = highlight_spans(spans, "world");
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].content, "World"); // original case preserved
    }

    #[test]
    fn highlight_spans_multiple_matches() {
        let spans = vec![Span::raw("abcabc")];
        let result = highlight_spans(spans, "abc");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "abc");
        assert_eq!(result[1].content, "abc");
    }

    // ── coverage in inspect ──────────────────────────────────────────────

    #[test]
    fn tree_job_node_carries_coverage_badge() {
        let mut job = make_job("lib", None, Some(0));
        job.coverage = Some(sample_coverage_report());
        let nodes = build_tree_nodes(&[job], ViewMode::Coverage, &HashSet::new(), &HashSet::new());
        let lib = nodes.iter().find(|n| n.title == "lib").unwrap();
        assert_eq!(lib.coverage_badge.as_deref(), Some("80.0% / 60.0%"));
    }

    #[test]
    fn tree_job_without_coverage_has_no_badge() {
        let job = make_job("lib", None, Some(0));
        let nodes = build_tree_nodes(&[job], ViewMode::Logs, &HashSet::new(), &HashSet::new());
        let lib = nodes.iter().find(|n| n.title == "lib").unwrap();
        assert!(lib.coverage_badge.is_none());
    }

    #[test]
    fn coverage_panel_lists_worst_classes_first() {
        let cov = sample_coverage_report();
        let rows = build_coverage_panel_rows(&cov);
        assert!(matches!(&rows[0], Row::CoverageLine { text, .. } if text.contains("80.0% lines")));
        // Foo has more missed lines than Bar.
        match (&rows[1], &rows[2]) {
            (Row::CoverageLine { text: a, .. }, Row::CoverageLine { text: b, .. }) => {
                assert!(a.contains("com.example.Foo"), "got: {a}");
                assert!(a.contains("15 missed"), "got: {a}");
                assert!(b.contains("com.example.Bar"), "got: {b}");
            }
            _ => panic!("expected two class CoverageLine rows"),
        }
    }

    #[test]
    fn coverage_panel_all_covered_message() {
        use crate::coverage::{ClassCoverage, CoverageSummary};
        let cov = MemberCoverage {
            report: CoverageReport {
                summary: CoverageSummary {
                    line_covered: 10,
                    line_missed: 0,
                    branch_covered: 4,
                    branch_missed: 0,
                },
                classes: vec![ClassCoverage {
                    package: "p".into(),
                    class_name: "A".into(),
                    line_covered: 10,
                    line_missed: 0,
                    branch_covered: 4,
                    branch_missed: 0,
                }],
            },
            sources: vec![],
        };
        let rows = build_coverage_panel_rows(&cov);
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1], Row::CoverageLine { text, .. }
            if text.contains("fully covered")));
    }

    #[test]
    fn try_load_member_coverage_loads_csv() {
        let dir = tempfile::tempdir().unwrap();
        let cov_dir = dir.path().join("target").join("coverage");
        std::fs::create_dir_all(&cov_dir).unwrap();
        std::fs::write(
            cov_dir.join("coverage.csv"),
            "GROUP,PACKAGE,CLASS,INSTRUCTION_MISSED,INSTRUCTION_COVERED,\
             BRANCH_MISSED,BRANCH_COVERED,LINE_MISSED,LINE_COVERED,\
             COMPLEXITY_MISSED,COMPLEXITY_COVERED,METHOD_MISSED,METHOD_COVERED\n\
             g,p,A,0,10,0,2,1,9,0,1,0,1\n",
        )
        .unwrap();
        let cov = try_load_member_coverage(dir.path()).unwrap();
        assert_eq!(cov.report.summary.line_covered, 9);
        assert_eq!(cov.report.summary.line_missed, 1);
        assert_eq!(cov.report.classes.len(), 1);
    }

    #[test]
    fn try_load_member_coverage_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(try_load_member_coverage(dir.path()).is_none());
    }

    #[test]
    fn coverage_color_thresholds() {
        assert_eq!(coverage_color(90.0), theme::GREEN);
        assert_eq!(coverage_color(80.0), theme::GREEN);
        assert_eq!(coverage_color(50.0), theme::YELLOW);
        assert_eq!(coverage_color(49.9), theme::RED);
    }

    #[test]
    fn tree_expands_coverage_sources() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("Foo.java.html");
        std::fs::write(
            &html,
            r#"<pre class="source lang-java linenums">class Foo {
<span class="fc" id="L2">  int x;</span>
}
</pre>"#,
        )
        .unwrap();
        let mut job = make_job("lib", None, Some(0));
        job.coverage = Some(sample_member_with_sources(html));
        let mut expanded_jobs = HashSet::new();
        expanded_jobs.insert(0);
        let nodes = build_tree_nodes(&[job], ViewMode::Coverage, &expanded_jobs, &HashSet::new());
        assert!(!nodes.iter().any(|n| n.coverage_group_ref.is_some()));
        let src = nodes
            .iter()
            .find(|n| n.coverage_source_ref == Some((0, 0)))
            .unwrap();
        assert!(src.label.contains("Foo.java"), "got: {}", src.label);
        assert_eq!(src.coverage_badge.as_deref(), Some("90.0% / 75.0%"));
    }

    #[test]
    fn load_source_view_renders_annotated_lines() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("Foo.java.html");
        std::fs::write(
            &html,
            r#"<pre class="source lang-java linenums">package p;
<span class="fc" id="L2">class Foo {}</span>
<span class="nc" id="L3">  void miss() {}</span>
</pre>"#,
        )
        .unwrap();
        let mut job = make_job("lib", None, Some(0));
        job.coverage = Some(sample_member_with_sources(html));
        let mut expanded_jobs = HashSet::new();
        expanded_jobs.insert(0);
        let jobs = vec![job];
        let nodes = build_tree_nodes(&jobs, ViewMode::Coverage, &expanded_jobs, &HashSet::new());
        let src_idx = nodes
            .iter()
            .position(|n| n.coverage_source_ref == Some((0, 0)))
            .unwrap();

        let mut state = InspectState {
            targets: vec![],
            ws_root: PathBuf::from("."),
            action: "build".into(),
            jobs,
            nodes,
            selected_idx: src_idx,
            tree_scroll: 0,
            rows: vec![],
            scroll: 0,
            show_members: true,
            active_pane: ActivePane::Members,
            mode: ViewMode::Coverage,
            filter: Filter::All,
            log_title: String::new(),
            pane_h: 24,
            log_vis_h: 20,
            utc_offset: time::UtcOffset::UTC,
            input_mode: InputMode::Normal,
            grep: String::new(),
            job_search: String::new(),
            expanded_jobs,
            expanded_classes: HashSet::new(),
            descriptors: HashMap::new(),
            resolved_deps: HashMap::new(),
            test_lines: vec![],
            source_lines: vec![],
            pre_search_pane: ActivePane::Members,
            grep_job_matches: HashSet::new(),
            stale_jobs: HashSet::new(),
            h_scroll: 0,
        };
        apply_selection(&mut state);
        assert_eq!(state.source_lines.len(), 3);
        assert_eq!(state.source_lines[1].hit, LineHit::Full);
        assert_eq!(state.source_lines[2].hit, LineHit::Missed);
        assert!(state
            .rows
            .iter()
            .any(|r| matches!(r, Row::SourceBody { line: 1 })));
        assert!(state
            .rows
            .iter()
            .any(|r| matches!(r, Row::SourceBody { line: 2 })));
    }

    #[test]
    fn view_mode_cycles() {
        assert_eq!(ViewMode::Logs.next(), ViewMode::Tests);
        assert_eq!(ViewMode::Tests.next(), ViewMode::Coverage);
        assert_eq!(ViewMode::Coverage.next(), ViewMode::Deps);
        assert_eq!(ViewMode::Deps.next(), ViewMode::Logs);
        assert_eq!(ViewMode::Logs.prev(), ViewMode::Deps);
        assert_eq!(ViewMode::from_digit('2'), Some(ViewMode::Tests));
        assert_eq!(ViewMode::from_digit('4'), Some(ViewMode::Deps));
        assert_eq!(ViewMode::from_digit('9'), None);
    }

    fn make_deps_job() -> Job {
        let mut job = make_job("lib", None, Some(0));
        job.deps = Some(sample_deps_view());
        job
    }

    #[test]
    fn tree_deps_mode_ends_at_project() {
        // Even if expanded_jobs is set, Deps mode must not grow past the project.
        let mut expanded_jobs = HashSet::new();
        expanded_jobs.insert(0);
        let nodes = build_tree_nodes(
            &[make_deps_job()],
            ViewMode::Deps,
            &expanded_jobs,
            &HashSet::new(),
        );
        assert_eq!(nodes.len(), 2); // root + project
        let lib = nodes.iter().find(|n| n.title == "lib").unwrap();
        // sample_deps_view: 2 compile + 1 test = 3 (workspace/AP/BOM not in badge)
        assert_eq!(lib.deps_badge.as_deref(), Some("3 deps"));
        assert!(!lib.label.contains("▸"));
        assert!(!lib.label.contains("▾"));
    }

    #[test]
    fn deps_panel_shows_compile_and_test_only() {
        let nodes = build_tree_nodes(
            &[make_deps_job()],
            ViewMode::Deps,
            &HashSet::new(),
            &HashSet::new(),
        );
        let mut state = InspectState {
            targets: vec![],
            ws_root: PathBuf::from("."),
            action: "build".into(),
            jobs: vec![make_deps_job()],
            nodes,
            selected_idx: 1,
            tree_scroll: 0,
            rows: vec![],
            scroll: 0,
            show_members: true,
            active_pane: ActivePane::Members,
            mode: ViewMode::Deps,
            filter: Filter::Prefix("lib".into()),
            log_title: "lib".into(),
            pane_h: 24,
            log_vis_h: 20,
            utc_offset: time::UtcOffset::UTC,
            input_mode: InputMode::Normal,
            grep: String::new(),
            job_search: String::new(),
            expanded_jobs: HashSet::new(),
            expanded_classes: HashSet::new(),
            descriptors: HashMap::new(),
            resolved_deps: HashMap::new(),
            test_lines: vec![],
            source_lines: vec![],
            pre_search_pane: ActivePane::Members,
            grep_job_matches: HashSet::new(),
            stale_jobs: HashSet::new(),
            h_scroll: 0,
        };
        // Pre-seed resolve cache so the panel does not hit the network/resolver.
        state
            .resolved_deps
            .insert((0, false), Ok(vec!["└─ com.example:core:1.2.3".into()]));
        state.resolved_deps.insert(
            (0, true),
            Ok(vec!["└─ org.junit.jupiter:junit-jupiter:5.10.0".into()]),
        );
        let rows = build_deps_panel_rows(&mut state, 0);
        let texts: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                Row::CoverageLine { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts[0].contains("application"));
        assert!(texts.iter().any(|t| t.starts_with("Compile")));
        assert!(texts.iter().any(|t| t.starts_with("Test")));
        assert!(texts.iter().any(|t| t.contains("com.example:core")));
        assert!(texts.iter().any(|t| t.contains("junit-jupiter")));
        // BOM imports / processors must not appear as separate sections.
        assert!(texts.iter().all(|t| !t.contains("BOM imports")));
        assert!(texts.iter().all(|t| !t.contains("Annotation processors")));
        assert!(texts.iter().all(|t| !t.starts_with("Workspace")));
    }

    #[test]
    fn tree_tests_mode_shows_test_summary_not_coverage() {
        let mut job = make_job("lib", None, Some(0));
        job.tests = vec![
            TestEntry {
                name: "a".into(),
                class_name: "T".into(),
                duration_ms: 1,
                status: TestStatus::Passed,
                failure: None,
                output_file: None,
            },
            TestEntry {
                name: "b".into(),
                class_name: "T".into(),
                duration_ms: 2,
                status: TestStatus::Failed,
                failure: None,
                output_file: None,
            },
        ];
        job.coverage = Some(sample_coverage_report());
        let nodes = build_tree_nodes(&[job], ViewMode::Tests, &HashSet::new(), &HashSet::new());
        let lib = nodes.iter().find(|n| n.title == "lib").unwrap();
        assert!(lib.coverage_badge.is_none());
        assert_eq!(
            lib.test_badge.as_ref().map(|(s, _)| s.as_str()),
            Some("1✓ 1✗")
        );
        assert!(lib.label.contains("▸"));
    }

    #[test]
    fn tree_logs_mode_has_no_expand_for_tests() {
        let mut job = make_job("lib", None, Some(0));
        job.tests = vec![TestEntry {
            name: "a".into(),
            class_name: "T".into(),
            duration_ms: 1,
            status: TestStatus::Passed,
            failure: None,
            output_file: None,
        }];
        let mut expanded = HashSet::new();
        expanded.insert(0);
        let nodes = build_tree_nodes(&[job], ViewMode::Logs, &expanded, &HashSet::new());
        // Even when "expanded", Logs mode never shows test children.
        assert!(nodes
            .iter()
            .all(|n| n.test_ref.is_none() && n.class_ref.is_none()));
        assert!(!nodes[1].label.contains("▸"));
    }

    #[test]
    fn render_source_line_marks_hits() {
        let covered = SourceLine {
            number: 10,
            text: "return 1;".into(),
            hit: LineHit::Full,
            title: None,
        };
        let missed = SourceLine {
            number: 11,
            text: "return 0;".into(),
            hit: LineHit::Missed,
            title: None,
        };
        let line = render_source_line(Some(&covered), "");
        assert!(line.spans[0].content.contains("10"));
        let line = render_source_line(Some(&missed), "");
        assert!(line.spans.iter().any(|s| s.content.contains("return 0;")));
    }

    // ── mouse support ────────────────────────────────────────────────────

    fn make_state(jobs: Vec<Job>, mode: ViewMode) -> InspectState {
        let nodes = build_tree_nodes(&jobs, mode, &HashSet::new(), &HashSet::new());
        InspectState {
            targets: vec![],
            ws_root: PathBuf::from("."),
            action: "build".into(),
            jobs,
            nodes,
            selected_idx: 0,
            tree_scroll: 0,
            rows: vec![],
            scroll: 0,
            show_members: true,
            active_pane: ActivePane::Members,
            mode,
            filter: Filter::All,
            log_title: "all jobs".into(),
            pane_h: 24,
            log_vis_h: 20,
            utc_offset: time::UtcOffset::UTC,
            input_mode: InputMode::Normal,
            grep: String::new(),
            job_search: String::new(),
            expanded_jobs: HashSet::new(),
            expanded_classes: HashSet::new(),
            descriptors: HashMap::new(),
            resolved_deps: HashMap::new(),
            test_lines: vec![],
            source_lines: vec![],
            pre_search_pane: ActivePane::Members,
            grep_job_matches: HashSet::new(),
            stale_jobs: HashSet::new(),
            h_scroll: 0,
        }
    }

    fn term_layout(w: u16, h: u16, show_members: bool, in_input: bool) -> PaneLayout {
        compute_layout(Rect::new(0, 0, w, h), show_members, in_input)
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(column: u16, row: u16, up: bool) -> MouseEvent {
        MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn layout_splits_members_and_detail() {
        let layout = term_layout(100, 30, true, false);
        assert_eq!(layout.tabs.y, 1);
        assert!(layout.members.is_some());
        let members = layout.members.unwrap();
        assert!(members.width < layout.detail.width);
        assert_eq!(members.y, layout.detail.y);
        assert!(layout.search.is_none());
    }

    #[test]
    fn layout_hides_members_and_shows_search() {
        let layout = term_layout(80, 20, false, true);
        assert!(layout.members.is_none());
        assert_eq!(layout.detail.x, 0);
        assert!(layout.search.is_some());
        assert_eq!(layout.search.unwrap().y, 19);
    }

    #[test]
    fn mode_tab_ranges_cover_all_modes_in_order() {
        let ranges = mode_tab_ranges();
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0].0, ViewMode::Logs);
        assert_eq!(ranges[1].0, ViewMode::Tests);
        assert_eq!(ranges[2].0, ViewMode::Coverage);
        assert_eq!(ranges[3].0, ViewMode::Deps);
        // Ranges are contiguous labels with separators between them.
        for i in 0..ranges.len() - 1 {
            assert!(ranges[i].2 <= ranges[i + 1].1);
        }
    }

    #[test]
    fn mode_tab_at_hits_each_label() {
        let layout = term_layout(80, 24, true, false);
        for (mode, start, end) in mode_tab_ranges() {
            let mid = layout.tabs.x + (start + end) / 2;
            assert_eq!(mode_tab_at(layout.tabs, mid), Some(mode), "mode={mode:?}");
        }
        // Padding before first tab is not a hit.
        assert_eq!(mode_tab_at(layout.tabs, layout.tabs.x), None);
    }

    #[test]
    fn mouse_click_mode_tab_switches_mode() {
        let jobs = vec![make_job("lib", Some(1000), Some(0))];
        let mut state = make_state(jobs, ViewMode::Logs);
        let layout = term_layout(80, 24, true, false);
        let (_, start, end) = mode_tab_ranges()
            .into_iter()
            .find(|(m, _, _)| *m == ViewMode::Tests)
            .unwrap();
        let col = layout.tabs.x + (start + end) / 2;
        handle_mouse(&mut state, click(col, layout.tabs.y), &layout);
        assert_eq!(state.mode, ViewMode::Tests);
    }

    #[test]
    fn mouse_click_members_selects_row() {
        let jobs = vec![
            make_job("alpha", Some(1000), Some(0)),
            make_job("beta", Some(2000), Some(0)),
        ];
        let mut state = make_state(jobs, ViewMode::Logs);
        state.active_pane = ActivePane::Log;
        let layout = term_layout(80, 24, true, false);
        let members = layout.members.unwrap();
        let inner = bordered_inner(members);
        // Row 0 is "all jobs"; row 1 is alpha; row 2 is beta.
        let beta_y = inner.y + 2;
        handle_mouse(&mut state, click(inner.x + 1, beta_y), &layout);
        assert_eq!(state.active_pane, ActivePane::Members);
        assert_eq!(state.selected_idx, 2);
        assert_eq!(state.log_title, "beta");
    }

    #[test]
    fn mouse_click_same_expandable_toggles_expansion() {
        let mut job = make_job("lib", None, Some(0));
        job.tests = vec![TestEntry {
            name: "a".into(),
            class_name: "T".into(),
            duration_ms: 1,
            status: TestStatus::Passed,
            failure: None,
            output_file: None,
        }];
        let mut state = make_state(vec![job], ViewMode::Tests);
        // Select the job node (index 1: root + job).
        state.selected_idx = 1;
        let layout = term_layout(80, 24, true, false);
        let members = layout.members.unwrap();
        let inner = bordered_inner(members);
        let job_y = inner.y + 1;
        assert!(state.expanded_jobs.is_empty());
        handle_mouse(&mut state, click(inner.x + 1, job_y), &layout);
        assert!(state.expanded_jobs.contains(&0));
        // Click again collapses.
        handle_mouse(&mut state, click(inner.x + 1, job_y), &layout);
        assert!(!state.expanded_jobs.contains(&0));
    }

    #[test]
    fn mouse_click_detail_focuses_log_pane() {
        let mut state = make_state(vec![make_job("lib", None, Some(0))], ViewMode::Logs);
        state.active_pane = ActivePane::Members;
        let layout = term_layout(80, 24, true, false);
        let d = layout.detail;
        handle_mouse(&mut state, click(d.x + d.width / 2, d.y + 2), &layout);
        assert_eq!(state.active_pane, ActivePane::Log);
    }

    #[test]
    fn mouse_click_search_bar_focuses_search() {
        let mut state = make_state(vec![make_job("lib", None, Some(0))], ViewMode::Logs);
        state.input_mode = InputMode::Grep;
        state.active_pane = ActivePane::Log;
        let layout = term_layout(80, 24, true, true);
        let search = layout.search.expect("search bar visible");
        handle_mouse(&mut state, click(search.x + 1, search.y), &layout);
        assert_eq!(state.active_pane, ActivePane::Search);
    }

    #[test]
    fn mouse_wheel_scrolls_detail_pane() {
        let mut state = make_state(vec![make_job("lib", None, Some(0))], ViewMode::Logs);
        // Enough rows to scroll.
        state.rows = (0..50)
            .map(|i| Row::CoverageLine {
                text: format!("line {i}"),
                color: theme::FG,
            })
            .collect();
        state.scroll = 10;
        let layout = term_layout(80, 24, true, false);
        let d = layout.detail;
        let cx = d.x + d.width / 2;
        let cy = d.y + 2;
        handle_mouse(&mut state, wheel(cx, cy, true), &layout);
        assert_eq!(state.scroll, 10 - MOUSE_SCROLL_STEP);
        handle_mouse(&mut state, wheel(cx, cy, false), &layout);
        assert_eq!(state.scroll, 10);
    }

    #[test]
    fn mouse_wheel_scrolls_members_tree() {
        let jobs: Vec<_> = (0..40)
            .map(|i| make_job(&format!("m{i:02}"), None, Some(0)))
            .collect();
        let mut state = make_state(jobs, ViewMode::Logs);
        state.tree_scroll = 5;
        state.pane_h = 10; // small visible window
        let layout = term_layout(80, 12, true, false);
        let m = layout.members.unwrap();
        handle_mouse(&mut state, wheel(m.x + 2, m.y + 2, false), &layout);
        assert_eq!(state.tree_scroll, 5 + MOUSE_SCROLL_STEP);
        handle_mouse(&mut state, wheel(m.x + 2, m.y + 2, true), &layout);
        assert_eq!(state.tree_scroll, 5);
    }

    #[test]
    fn mouse_wheel_horizontal_scrolls_detail() {
        let mut state = make_state(vec![make_job("lib", None, Some(0))], ViewMode::Logs);
        state.h_scroll = H_SCROLL_STEP;
        let layout = term_layout(80, 24, true, false);
        let d = layout.detail;
        handle_mouse(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollLeft,
                column: d.x + 1,
                row: d.y + 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
        );
        assert_eq!(state.h_scroll, 0);
        handle_mouse(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollRight,
                column: d.x + 1,
                row: d.y + 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
        );
        assert_eq!(state.h_scroll, H_SCROLL_STEP);
    }

    #[test]
    fn scroll_tree_clamps_to_content() {
        let mut state = make_state(vec![make_job("a", None, Some(0))], ViewMode::Logs);
        state.pane_h = 24;
        state.tree_scroll = 0;
        scroll_tree(&mut state, -5);
        assert_eq!(state.tree_scroll, 0);
        scroll_tree(&mut state, 100);
        let visible = (state.pane_h as usize).saturating_sub(2).max(1);
        let max = state.nodes.len().saturating_sub(visible);
        assert_eq!(state.tree_scroll, max);
    }
}
