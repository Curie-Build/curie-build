//! Interactive TUI for browsing merged build logs (`curie inspect`).
//!
//! By default shows a single full-width log pane with every job's output merged
//! and sorted by job start time.  An optional members pane (toggled with Tab/m)
//! lets the user filter the merged log to a single workspace subtree or project.

use std::collections::HashSet;
use std::io::Stdout;
use std::path::PathBuf;

use ansi_to_tui::IntoText;
use anyhow::Result;
use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use crate::parallel::parse_meta;

// ── Public types ──────────────────────────────────────────────────────────

/// Lightweight descriptor of a single build target, passed in by the caller.
#[derive(Clone)]
pub(crate) struct LogTarget {
    pub declared: String,
    pub path:     PathBuf,
}

// ── Internal types ────────────────────────────────────────────────────────

#[derive(Clone)]
enum LogState {
    Ok     { duration_ms: u64 },
    Failed { duration_ms: u64 },
    /// `.log` exists but no `.meta` sidecar — metadata unavailable.
    NoMetadata,
    /// No `.log` or `.meta`.
    NoLog,
}

struct Job {
    declared:     String,
    state:        LogState,
    /// `None` when no `.meta` is present.
    started_ms:   Option<u64>,
    /// `"HH:MM:SS"` in local timezone, or `""` when unknown.
    started_disp: String,
    lines:        Vec<String>,
    /// UUIDv7 shared by all jobs in one build run; `None` when no `.meta`.
    build_id:  Option<String>,
    /// Per-test results from `target/build.tests.json`; empty when absent.
    tests:     Vec<TestEntry>,
}

#[derive(Clone)]
struct TestEntry {
    name:        String,
    #[allow(dead_code)]
    class_name:  String,
    duration_ms: u64,
    status:      TestStatus,
    failure:     Option<String>,
    /// Absolute path to the per-test output `.txt` file, or `None`.
    output_file: Option<PathBuf>,
}

#[derive(Clone, PartialEq)]
enum TestStatus { Passed, Failed, Skipped }

/// The active filter applied to the merged log.
#[derive(Clone)]
enum Filter {
    All,
    /// Show only jobs whose `declared` path equals `p` or starts with `p/`.
    Prefix(String),
}

struct TreeNode {
    /// Indented display text shown in the members pane.
    label:      String,
    /// Used as the log block title when this node is selected.
    title:      String,
    filter:     Filter,
    /// `None` for the root "all jobs" row and for container rows.
    state:      Option<LogState>,
    selectable: bool,
    /// `Some((job_idx, test_idx))` for test method leaf nodes.
    test_ref:   Option<(usize, usize)>,
    /// `Some((badge_text, status))` — drives right-aligned coloured badge for test nodes.
    test_badge: Option<(String, TestStatus)>,
    /// `Some((job_idx, class_name))` for class group nodes.
    class_ref:  Option<(usize, String)>,
    /// Index into `jobs` for every node that belongs to a specific job (leaf or child).
    job_idx:    Option<usize>,
}

enum Row {
    Header { job: usize },
    /// `line` is an index into `jobs[job].lines`.
    Body   { job: usize, line: usize },
    /// A coloured annotation line at the top of a test's log view.
    TestAnnotation { text: String, color: Color },
    /// `line` is an index into `InspectState::test_lines`.
    TestBody       { line: usize },
}

#[derive(PartialEq, Clone)]
enum ActivePane { Members, Log, Search }

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
    targets:      Vec<LogTarget>,
    action:       String,
    jobs:         Vec<Job>,
    nodes:        Vec<TreeNode>,
    selected_idx: usize,
    tree_scroll:  usize,
    rows:         Vec<Row>,
    scroll:       usize,
    show_members: bool,
    active_pane:  ActivePane,
    filter:       Filter,
    log_title:    String,
    /// Terminal height minus the header row; kept in sync from the event loop.
    pane_h:       u16,
    /// Visible log height (inner block rows); updated each frame for scroll clamping.
    log_vis_h:    usize,
    utc_offset:   time::UtcOffset,
    input_mode:   InputMode,
    /// Current log-content grep pattern.
    grep:         String,
    /// Current job-name search pattern.
    job_search:   String,
    /// Job indices that have been expanded to show class group rows.
    expanded_jobs: HashSet<usize>,
    /// `(job_idx, class_name)` pairs that have been expanded to show test method rows.
    expanded_classes: HashSet<(usize, String)>,
    /// Lines from the currently selected test's output file (empty when not in test view).
    test_lines:        Vec<String>,
    /// Pane to return to when the search bar is dismissed with Enter or Esc.
    pre_search_pane:   ActivePane,
    /// Job indices whose log lines contain the current grep pattern (empty when grep inactive).
    grep_job_matches:  HashSet<usize>,
    /// Job indices whose build_id is older than the latest build_id seen across all jobs.
    stale_jobs:        HashSet<usize>,
    /// Horizontal character offset for the log pane (scrolled with Left/Right).
    h_scroll:          usize,
}

// ── Entry point ───────────────────────────────────────────────────────────

pub(crate) fn run_inspect_ui(
    _ws_root:  &std::path::Path,
    targets:   &[LogTarget],
    action:    &str,
    preselect: Option<usize>,
) -> Result<()> {
    // Query local offset before spawning threads or entering raw mode.
    let utc_offset = time::UtcOffset::current_local_offset()
        .unwrap_or(time::UtcOffset::UTC);

    let jobs   = load_jobs(targets, action, utc_offset);
    let stale_jobs   = collect_stale_jobs(&jobs);
    let expanded_jobs:    HashSet<usize>           = HashSet::new();
    let expanded_classes: HashSet<(usize, String)> = HashSet::new();
    let nodes  = build_tree_nodes(&jobs, &expanded_jobs, &expanded_classes);
    let filter = Filter::All;
    let rows   = build_rows(&jobs, &filter, "", "");

    let mut state = InspectState {
        targets:      targets.to_vec(),
        action:       action.to_string(),
        jobs,
        nodes,
        selected_idx:    0,
        tree_scroll:     0,
        rows,
        scroll:          0,
        show_members:    true,
        active_pane:     ActivePane::Members,
        filter,
        log_title:       "all jobs".to_string(),
        pane_h:          24,
        log_vis_h:       20,
        utc_offset,
        input_mode:      InputMode::Normal,
        grep:            String::new(),
        job_search:      String::new(),
        expanded_jobs,
        expanded_classes,
        test_lines:       Vec::new(),
        pre_search_pane:  ActivePane::Members,
        grep_job_matches: HashSet::new(),
        stale_jobs,
        h_scroll:         0,
    };

    // Auto-focus first failure; fall back to explicit preselect index.
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
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = event_loop(&mut term, &mut state);

    let _ = terminal::disable_raw_mode();
    let _ = execute!(term.backend_mut(), terminal::LeaveAlternateScreen, cursor::Show);
    let _ = term.show_cursor();
    result
}

// ── Loading ───────────────────────────────────────────────────────────────

fn load_jobs(targets: &[LogTarget], action: &str, utc_offset: time::UtcOffset) -> Vec<Job> {
    targets.iter().map(|t| {
        let log_path  = t.path.join("target").join(format!("{action}.log"));
        let meta_path = t.path.join("target").join(format!("{action}.meta"));
        let meta      = parse_meta(&meta_path);

        let state = match (meta.as_ref(), log_path.exists()) {
            (Some(m), _) if m.exit_code == 0 => LogState::Ok     { duration_ms: m.duration_ms },
            (Some(m), _)                     => LogState::Failed  { duration_ms: m.duration_ms },
            (None,    true)                  => LogState::NoMetadata,
            (None,    false)                 => LogState::NoLog,
        };

        let (started_ms, started_disp) = meta.as_ref()
            .map(|m| (Some(m.started_ms), format_hms_local(m.started_ms, utc_offset)))
            .unwrap_or((None, String::new()));

        let build_id = meta.as_ref().map(|m| m.build_id.clone());
        let lines    = if log_path.exists() { load_log(&log_path) } else { Vec::new() };
        let tests    = parse_test_sidecar(&t.path);

        Job { declared: t.declared.clone(), state, started_ms, started_disp, lines, build_id, tests }
    }).collect()
}

/// Return the set of job indices whose build_id is older than the latest seen.
/// When all jobs share the same id (or have none), the set is empty.
fn collect_stale_jobs(jobs: &[Job]) -> HashSet<usize> {
    let latest = jobs.iter()
        .filter_map(|j| j.build_id.as_deref())
        .max()
        .map(str::to_string);
    let Some(latest_id) = latest else { return HashSet::new(); };
    let has_mixed = jobs.iter()
        .filter_map(|j| j.build_id.as_deref())
        .any(|id| id != latest_id);
    if !has_mixed { return HashSet::new(); }
    jobs.iter().enumerate()
        .filter(|(_, j)| j.build_id.as_deref().is_some_and(|id| id != latest_id))
        .map(|(i, _)| i)
        .collect()
}

fn parse_test_sidecar(member_root: &std::path::Path) -> Vec<TestEntry> {
    let path = member_root.join("target").join("build.tests.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c)  => c,
        Err(_) => return Vec::new(),
    };
    let raw: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v)  => v,
        Err(_) => return Vec::new(),
    };
    raw.into_iter()
        .filter_map(|v| parse_test_entry(v, member_root))
        .collect()
}

fn parse_test_entry(v: serde_json::Value, member_root: &std::path::Path) -> Option<TestEntry> {
    let name        = v["name"].as_str()?.to_string();
    let class_name  = v["class_name"].as_str().unwrap_or("").to_string();
    let duration_ms = v["duration_ms"].as_u64().unwrap_or(0);
    let status = match v["status"].as_str().unwrap_or("") {
        "failed"  => TestStatus::Failed,
        "skipped" => TestStatus::Skipped,
        _         => TestStatus::Passed,
    };
    let failure = v["failure"].as_str().map(str::to_string);
    // output_file in JSON is relative to target/, e.g. "test-output/com/..."
    let output_file = v["output_file"].as_str()
        .map(|p| member_root.join("target").join(p));
    Some(TestEntry { name, class_name, duration_ms, status, failure, output_file })
}

fn load_log(path: &std::path::Path) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c)  => c,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    while lines.last().map_or(false, |l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

// ── Tree construction ─────────────────────────────────────────────────────

fn build_tree_nodes(
    jobs:             &[Job],
    expanded_jobs:    &HashSet<usize>,
    expanded_classes: &HashSet<(usize, String)>,
) -> Vec<TreeNode> {
    let mut nodes = vec![tree_node_plain("all jobs", "all jobs", Filter::All)];

    let mut current_dirs: Vec<String> = Vec::new();

    for (job_idx, job) in jobs.iter().enumerate() {
        let parts: Vec<&str> = job.declared.split('/').collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        let name = parts.last().copied().unwrap_or(&job.declared);

        // Common prefix depth with the previously emitted directory stack.
        let common = {
            let mut n = 0;
            for (a, b) in dirs.iter().zip(current_dirs.iter()) {
                if *a == b.as_str() { n += 1; } else { break; }
            }
            n
        };

        for depth in common..dirs.len() {
            let path_here = dirs[..=depth].join("/");
            let indent    = "  ".repeat(depth + 1);
            nodes.push(tree_node_plain(
                &format!("{indent}{}/", dirs[depth]),
                &format!("{path_here}/"),
                Filter::Prefix(path_here),
            ));
        }

        let depth  = dirs.len();
        let indent = "  ".repeat(depth + 1);
        let has_tests = !job.tests.is_empty();
        let expand_marker = if has_tests {
            if expanded_jobs.contains(&job_idx) { " ▾" } else { " ▸" }
        } else { "" };
        nodes.push(TreeNode {
            label:      format!("{indent}{name}{expand_marker}"),
            title:      job.declared.clone(),
            filter:     Filter::Prefix(job.declared.clone()),
            state:      Some(job.state.clone()),
            selectable: true,
            test_ref:   None,
            test_badge: None,
            class_ref:  None,
            job_idx:    Some(job_idx),
        });

        // When job is expanded: add one class-group node per unique class, then
        // when a class is also expanded add the test method leaf nodes under it.
        if has_tests && expanded_jobs.contains(&job_idx) {
            let class_indent = "  ".repeat(depth + 2);
            let test_indent  = "  ".repeat(depth + 3);

            for (class_name, tests_in_class) in group_by_class(&job.tests) {
                let class_expanded = expanded_classes.contains(&(job_idx, class_name.clone()));
                let class_marker   = if class_expanded { " ▾" } else { " ▸" };
                let short_class = class_name.rsplit('.').next().unwrap_or(&class_name);
                nodes.push(TreeNode {
                    label:      format!("{class_indent}{short_class}{class_marker}"),
                    title:      format!("{} › {}", job.declared, class_name),
                    filter:     Filter::Prefix(job.declared.clone()),
                    state:      None,
                    selectable: true,
                    test_ref:   None,
                    test_badge: None,
                    class_ref:  Some((job_idx, class_name.clone())),
                    job_idx:    Some(job_idx),
                });

                if class_expanded {
                    for &test_idx in &tests_in_class {
                        let test = &job.tests[test_idx];
                        let badge = test_badge(&test.status, test.duration_ms);
                        nodes.push(TreeNode {
                            label:      format!("{test_indent}{}", test.name),
                            title:      format!("{} › {} › {}", job.declared, class_name, test.name),
                            filter:     Filter::Prefix(job.declared.clone()),
                            state:      None,
                            selectable: true,
                            test_ref:   Some((job_idx, test_idx)),
                            test_badge: Some((badge, test.status.clone())),
                            class_ref:  None,
                            job_idx:    Some(job_idx),
                        });
                    }
                }
            }
        }

        current_dirs = dirs.iter().map(|s| s.to_string()).collect();
    }

    nodes
}

/// Return `(class_name, Vec<test_idx>)` in stable order of first appearance.
fn group_by_class(tests: &[TestEntry]) -> Vec<(String, Vec<usize>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (i, t) in tests.iter().enumerate() {
        let key = if t.class_name.is_empty() { "(unknown)".to_string() } else { t.class_name.clone() };
        if !map.contains_key(&key) { order.push(key.clone()); }
        map.entry(key).or_default().push(i);
    }
    order.into_iter().map(|k| { let v = map.remove(&k).unwrap(); (k, v) }).collect()
}

fn tree_node_plain(label: &str, title: &str, filter: Filter) -> TreeNode {
    TreeNode {
        label:      label.to_string(),
        title:      title.to_string(),
        filter,
        state:      None,
        selectable: true,
        test_ref:   None,
        test_badge: None,
        class_ref:  None,
        job_idx:    None,
    }
}

/// Find the node whose filter prefix equals `declared` exactly (leaf lookup).
fn find_node_for_declared(nodes: &[TreeNode], declared: &str) -> Option<usize> {
    nodes.iter().position(|n| {
        matches!(&n.filter, Filter::Prefix(p) if p == declared)
    })
}

// ── Filtering ─────────────────────────────────────────────────────────────

fn job_matches(filter: &Filter, declared: &str) -> bool {
    match filter {
        Filter::All       => true,
        Filter::Prefix(p) => declared == p || declared.starts_with(&format!("{p}/")),
    }
}

fn job_search_matches(declared: &str, job_search: &str) -> bool {
    if job_search.is_empty() { return true; }
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
        sa.cmp(&sb).then_with(|| jobs[a].declared.cmp(&jobs[b].declared))
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
    let node     = &state.nodes[state.selected_idx];
    let test_ref = node.test_ref;
    let title    = node.title.clone();
    let filter   = node.filter.clone();

    state.log_title = title;
    state.filter    = filter;
    state.h_scroll  = 0;

    if let Some((job_idx, test_idx)) = test_ref {
        load_test_view(state, job_idx, test_idx);
    } else {
        state.test_lines.clear();
        rebuild_rows(state);
    }

    sync_tree_scroll(state);
}

fn load_test_view(state: &mut InspectState, job_idx: usize, test_idx: usize) {
    let test    = &state.jobs[job_idx].tests[test_idx];
    let failure = test.failure.clone();
    let status  = test.status.clone();
    state.test_lines = test.output_file.as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();

    let mut rows: Vec<Row> = Vec::new();
    if let Some(msg) = failure {
        rows.push(Row::TestAnnotation { text: msg, color: Color::Red });
    }
    for li in 0..state.test_lines.len() {
        rows.push(Row::TestBody { line: li });
    }
    if rows.is_empty() {
        let placeholder_color = match status {
            TestStatus::Passed  => Color::Green,
            TestStatus::Failed  => Color::Red,
            TestStatus::Skipped => Color::Yellow,
        };
        rows.push(Row::TestAnnotation { text: "(no output captured)".to_string(), color: placeholder_color });
    }
    state.rows   = rows;
    let max      = state.rows.len().saturating_sub(state.log_vis_h.max(1));
    state.scroll = state.scroll.min(max);
}

/// Job indices whose log lines contain `grep` (case-insensitive). Empty when grep is inactive.
fn grep_hits(jobs: &[Job], grep: &str) -> HashSet<usize> {
    if grep.is_empty() { return HashSet::new(); }
    let pattern = grep.to_lowercase();
    (0..jobs.len())
        .filter(|&i| jobs[i].lines.iter().any(|l| l.to_lowercase().contains(&pattern)))
        .collect()
}

/// Rebuild rows from current filter/grep/job_search, then clamp scroll.
/// When the selected node is a test leaf, re-applies the test view.
fn rebuild_rows(state: &mut InspectState) {
    state.grep_job_matches = grep_hits(&state.jobs, &state.grep);
    if let Some((job_idx, test_idx)) = state.nodes.get(state.selected_idx).and_then(|n| n.test_ref) {
        load_test_view(state, job_idx, test_idx);
        return;
    }
    state.test_lines.clear();
    state.rows   = build_rows(&state.jobs, &state.filter, &state.grep, &state.job_search);
    let max      = state.rows.len().saturating_sub(state.log_vis_h.max(1));
    state.scroll = state.scroll.min(max);
}

fn reload(state: &mut InspectState) {
    let targets    = state.targets.clone();
    let action     = state.action.clone();
    let utc_offset = state.utc_offset;
    state.jobs       = load_jobs(&targets, &action, utc_offset);
    state.stale_jobs = collect_stale_jobs(&state.jobs);
    state.nodes    = build_tree_nodes(&state.jobs, &state.expanded_jobs, &state.expanded_classes);
    state.selected_idx = state.selected_idx.min(state.nodes.len().saturating_sub(1));
    rebuild_rows(state);
}

/// Focus the first failing job.  If it has failing tests, expand the job+class
/// and select the first failing test.  Returns `true` when a failure was found.
fn auto_select_failed(state: &mut InspectState) -> bool {
    let failed_job = state.jobs.iter().enumerate()
        .find(|(_, j)| matches!(j.state, LogState::Failed { .. }));

    let Some((job_idx, _)) = failed_job else { return false; };

    // Look for the first failing test in this job.
    let first_failing_test = state.jobs[job_idx].tests.iter().enumerate()
        .find(|(_, t)| t.status == TestStatus::Failed)
        .map(|(i, _)| {
            let class_name = state.jobs[job_idx].tests[i].class_name.clone();
            (i, class_name)
        });

    if let Some((test_idx, class_name)) = first_failing_test {
        state.expanded_jobs.insert(job_idx);
        state.expanded_classes.insert((job_idx, class_name.clone()));
        state.nodes = build_tree_nodes(&state.jobs, &state.expanded_jobs, &state.expanded_classes);

        if let Some(ni) = state.nodes.iter().position(|n| n.test_ref == Some((job_idx, test_idx))) {
            state.selected_idx = ni;
            apply_selection(state);
            return true;
        }
    }

    // No failing test: just select the failed job node.
    let declared = state.jobs[job_idx].declared.clone();
    if let Some(ni) = find_node_for_declared(&state.nodes, &declared) {
        state.selected_idx = ni;
        apply_selection(state);
    }
    true
}

fn sync_tree_scroll(state: &mut InspectState) {
    let visible = (state.pane_h as usize).saturating_sub(2);
    let sel     = state.selected_idx;
    if sel < state.tree_scroll {
        state.tree_scroll = sel;
    } else if visible > 0 && sel >= state.tree_scroll + visible {
        state.tree_scroll = sel + 1 - visible;
    }
}

// ── Navigation ────────────────────────────────────────────────────────────

fn next_node(nodes: &[TreeNode], from: usize) -> usize {
    let n = nodes.len();
    if n == 0 { return 0; }
    let mut i = (from + 1) % n;
    while i != from && !nodes[i].selectable { i = (i + 1) % n; }
    i
}

fn prev_node(nodes: &[TreeNode], from: usize) -> usize {
    let n = nodes.len();
    if n == 0 { return 0; }
    let mut i = (from + n - 1) % n;
    while i != from && !nodes[i].selectable { i = (i + n - 1) % n; }
    i
}

// ── Display helpers ───────────────────────────────────────────────────────

fn gutter_color(state: &LogState) -> Color {
    match state {
        LogState::Ok         { .. } => Color::Green,
        LogState::Failed     { .. } => Color::Red,
        LogState::NoMetadata        => Color::Yellow,
        LogState::NoLog             => Color::DarkGray,
    }
}

fn badge_str(state: &LogState) -> String {
    match state {
        LogState::Ok         { duration_ms } => format!("✓ {}", fmt_duration(*duration_ms)),
        LogState::Failed     { duration_ms } => format!("✗ {}", fmt_duration(*duration_ms)),
        LogState::NoMetadata                 => "skipped".to_string(),
        LogState::NoLog                      => "(no log)".to_string(),
    }
}

fn badge_style(state: &LogState) -> Style {
    match state {
        LogState::Ok         { .. } => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        LogState::Failed     { .. } => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        LogState::NoMetadata        => Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
        LogState::NoLog             => Style::default().add_modifier(Modifier::DIM),
    }
}

fn test_badge(status: &TestStatus, duration_ms: u64) -> String {
    match status {
        TestStatus::Passed  => format!("✓ {}ms", duration_ms),
        TestStatus::Failed  => format!("✗ {}ms", duration_ms),
        TestStatus::Skipped => "⊘ skipped".to_string(),
    }
}

#[allow(dead_code)]
fn test_badge_style(status: &TestStatus) -> Style {
    match status {
        TestStatus::Passed  => Style::default().fg(Color::Green),
        TestStatus::Failed  => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        TestStatus::Skipped => Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
    }
}

fn fmt_duration(ms: u64) -> String {
    let tenths = ms / 100;
    format!("{}.{}s", tenths / 10, tenths % 10)
}

/// Format epoch-milliseconds as `HH:MM:SS` in the given UTC offset.
fn format_hms_local(epoch_ms: u64, offset: time::UtcOffset) -> String {
    let secs   = (epoch_ms / 1000) as i64 + offset.whole_seconds() as i64;
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

fn render_frame(f: &mut Frame, state: &InspectState) {
    let total    = f.area();
    let in_input = state.input_mode != InputMode::Normal;

    let constraints: Vec<Constraint> = if in_input {
        vec![Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]
    } else {
        vec![Constraint::Length(1), Constraint::Min(0)]
    };

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(total);

    f.render_widget(
        Paragraph::new(hint_text(state)).style(Style::default().add_modifier(Modifier::DIM)),
        vchunks[0],
    );

    if state.show_members {
        let hchunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(vchunks[1]);
        render_members_block(f, state, hchunks[0]);
        render_log_block(f, state, hchunks[1]);
    } else {
        render_log_block(f, state, vchunks[1]);
    }

    if in_input {
        render_status_bar(f, state, vchunks[2]);
    }
}

fn hint_text(state: &InspectState) -> &'static str {
    match &state.active_pane {
        ActivePane::Search => {
            "curie inspect  type to filter  Enter apply  Tab switch pane  Esc clear"
        }
        ActivePane::Members => {
            if state.input_mode != InputMode::Normal {
                "curie inspect  \u{2191}\u{2193}/jk select  Tab switch  Esc clear search  q quit"
            } else {
                "curie inspect  \u{2191}\u{2193}/jk select  Enter/\u{2192} expand  \u{2190} collapse  Tab log  m hide  / grep  f job  r reload  q quit"
            }
        }
        ActivePane::Log => {
            if state.input_mode != InputMode::Normal {
                "curie inspect  PgUp/Dn scroll  Tab switch  Esc clear search  q quit"
            } else {
                "curie inspect  jk/PgUp/Dn scroll  g/G top/bot  Tab members  m toggle  / grep  f job  r reload  q quit"
            }
        }
    }
}

fn render_members_block(f: &mut Frame, state: &InspectState, area: Rect) {
    // Only highlight border when Members pane itself is focused (not when Search is active).
    let is_active    = state.active_pane == ActivePane::Members;
    let border_style = if is_active { Style::default().fg(Color::Cyan) } else { Style::default() };

    let block = Block::default()
        .title("Members")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner   = block.inner(area);
    let vis_h   = inner.height as usize;
    let inner_w = inner.width  as usize;
    let js      = &state.job_search;
    let gm      = &state.grep_job_matches;
    let stale   = &state.stale_jobs;

    let lines: Vec<Line<'static>> = state.nodes.iter()
        .enumerate()
        .skip(state.tree_scroll)
        .take(vis_h)
        .map(|(i, node)| member_line(node, i == state.selected_idx, inner_w, js, gm, stale))
        .collect();

    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn member_line(
    node:         &TreeNode,
    is_selected:  bool,
    inner_w:      usize,
    job_search:   &str,
    grep_matches: &HashSet<usize>,
    stale_jobs:   &HashSet<usize>,
) -> Line<'static> {
    match &node.state {
        Some(log_state) => {
            let ji = node.job_idx;
            // Dim when either search filter is active and this job doesn't match.
            let job_search_dim = !job_search.is_empty()
                && !node.title.to_lowercase().contains(&job_search.to_lowercase());
            let grep_dim = !grep_matches.is_empty()
                && ji.map_or(true, |i| !grep_matches.contains(&i));
            let is_stale = ji.map_or(false, |i| stale_jobs.contains(&i));
            let search_dim = job_search_dim || grep_dim;

            let badge = if is_stale {
                format!("{} (prev)", badge_str(log_state))
            } else {
                badge_str(log_state)
            };
            let bstyle = if is_stale || search_dim {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
            } else {
                badge_style(log_state)
            };
            let badge_w = badge.chars().count();
            let label_w = inner_w.saturating_sub(badge_w + 1);
            let label:  String = node.label.chars().take(label_w).collect();
            let padding = " ".repeat(label_w.saturating_sub(label.chars().count()) + 1);
            let label_style = if is_stale || search_dim {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            let mut line = Line::from(vec![
                Span::styled(label, label_style),
                Span::styled(padding, label_style),
                Span::styled(badge, bstyle),
            ]);
            if is_selected {
                line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        }
        None => {
            if let Some((badge_text, status)) = &node.test_badge {
                // Test leaf node: right-aligned coloured badge.
                let badge_color = match status {
                    TestStatus::Passed  => Color::Green,
                    TestStatus::Failed  => Color::Red,
                    TestStatus::Skipped => Color::Yellow,
                };
                let badge_modifier = if *status == TestStatus::Failed { Modifier::BOLD } else { Modifier::empty() };
                let bstyle  = Style::default().fg(badge_color).add_modifier(badge_modifier);
                let badge_w = badge_text.chars().count();
                let label_w = inner_w.saturating_sub(badge_w + 1);
                let label:  String = node.label.chars().take(label_w).collect();
                let padding = " ".repeat(label_w.saturating_sub(label.chars().count()) + 1);
                let mut line = Line::from(vec![
                    Span::raw(label),
                    Span::raw(padding),
                    Span::styled(badge_text.clone(), bstyle),
                ]);
                if is_selected {
                    line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
                }
                line
            } else {
                // Class group node or directory container: dimmed.
                let base_style = Style::default().add_modifier(Modifier::DIM);
                let style = if is_selected {
                    base_style.add_modifier(Modifier::REVERSED)
                } else {
                    base_style
                };
                Line::styled(node.label.clone(), style)
            }
        }
    }
}

/// Virtualized log renderer: only converts the visible row slice to `Line`s (O(vis_h)).
fn render_log_block(f: &mut Frame, state: &InspectState, area: Rect) {
    // Only highlight border when Log pane itself is focused (not when Search is active).
    let is_active = !state.show_members || state.active_pane == ActivePane::Log;
    let border_style = if is_active { Style::default().fg(Color::Cyan) } else { Style::default() };

    let title = format!("Log: {}", state.log_title);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    let vis_h = inner.height as usize;
    let start = state.scroll.min(state.rows.len());
    let end   = (start + vis_h).min(state.rows.len());

    let h_off = state.h_scroll;
    let lines: Vec<Line<'static>> = state.rows[start..end]
        .iter()
        .map(|row| {
            let line = render_row(row, &state.jobs, &state.test_lines, &state.grep);
            trim_line_left(line, h_off)
        })
        .collect();

    f.render_widget(block, area);
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Remove the first `offset` characters from a line's span list, preserving per-span styles.
fn trim_line_left(line: Line<'static>, offset: usize) -> Line<'static> {
    if offset == 0 { return line; }
    let mut skip = offset;
    let spans: Vec<Span<'static>> = line.spans.into_iter().filter_map(|span| {
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
    }).collect();
    Line::from(spans)
}

fn render_row(row: &Row, jobs: &[Job], test_lines: &[String], grep: &str) -> Line<'static> {
    match row {
        Row::Header { job } => {
            let j     = &jobs[*job];
            let color = gutter_color(&j.state);
            header_line(j, color)
        }
        Row::Body { job, line } => {
            let j     = &jobs[*job];
            let color = gutter_color(&j.state);
            body_line(&j.lines[*line], color, grep)
        }
        Row::TestAnnotation { text, color } => {
            let gutter = Span::styled("▌ ", Style::default().fg(*color).add_modifier(Modifier::BOLD));
            let msg    = Span::styled(text.clone(), Style::default().fg(*color).add_modifier(Modifier::BOLD));
            Line::from(vec![gutter, msg]).style(Style::default().bg(Color::Indexed(236)))
        }
        Row::TestBody { line } => {
            let text = test_lines.get(*line).map(String::as_str).unwrap_or("");
            body_line(text, Color::Cyan, grep)
        }
    }
}

fn header_line(job: &Job, color: Color) -> Line<'static> {
    let gutter  = Span::styled("▌ ", Style::default().fg(color).add_modifier(Modifier::BOLD));
    let content = if job.started_disp.is_empty() {
        format!("{}  {}", job.declared, badge_str(&job.state))
    } else {
        format!("{}  started {}  {}", job.declared, job.started_disp, badge_str(&job.state))
    };
    let text = Span::styled(content, Style::default().fg(color).add_modifier(Modifier::BOLD));
    Line::from(vec![gutter, text])
        .style(Style::default().bg(Color::Indexed(236)))
}

fn body_line(text: &str, color: Color, grep: &str) -> Line<'static> {
    let gutter     = Span::styled("▌ ", Style::default().fg(color).add_modifier(Modifier::BOLD));
    let body_spans = parse_ansi_line(text);
    let mut spans  = vec![gutter];
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
    let highlight     = Style::default().bg(Color::Yellow).fg(Color::Black);
    let mut result    = Vec::new();

    for span in spans {
        let content       = span.content.to_string();
        let content_lower = content.to_lowercase();
        if content_lower.contains(&pattern_lower) {
            split_span_at_pattern(&content, &content_lower, &pattern_lower,
                                  span.style, highlight, &mut result);
        } else {
            result.push(span);
        }
    }
    result
}

fn split_span_at_pattern(
    content:       &str,
    content_lower: &str,
    pattern_lower: &str,
    base_style:    Style,
    highlight:     Style,
    out:           &mut Vec<Span<'static>>,
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
                let end   = start + pat_len;
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
        Err(_)       => vec![Span::raw(s.to_string())],
    }
}

fn render_status_bar(f: &mut Frame, state: &InspectState, area: Rect) {
    let content = match &state.input_mode {
        InputMode::Grep      => format!("/ {}█", state.grep),
        InputMode::JobSearch => format!("f {}█", state.job_search),
        InputMode::Normal    => return,
    };
    // Cyan background when the search bar has keyboard focus; dimmer when a pane does.
    let style = if state.active_pane == ActivePane::Search {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray).bg(Color::Black)
    };
    f.render_widget(Paragraph::new(content).style(style), area);
}

const H_SCROLL_STEP: usize = 4;

// ── Event loop ────────────────────────────────────────────────────────────

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state:    &mut InspectState,
) -> Result<()> {
    loop {
        terminal.draw(|f| render_frame(f, state))?;

        let size      = terminal.size()?;
        state.pane_h  = size.height.saturating_sub(1);
        state.log_vis_h = (state.pane_h as usize).saturating_sub(2);

        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if !handle_key(state, key) {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn handle_key(state: &mut InspectState, key: KeyEvent) -> bool {
    if state.active_pane == ActivePane::Search {
        return handle_key_search(state, key);
    }

    let members_active = state.show_members && state.active_pane == ActivePane::Members;
    let searching      = state.input_mode != InputMode::Normal;
    let log_ph         = state.log_vis_h.max(1);

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
        KeyCode::Char('g') => { state.scroll = 0; }
        KeyCode::Char('G') => { state.scroll = state.rows.len().saturating_sub(1); }

        KeyCode::Char('r') => reload(state),

        KeyCode::Char('/') => {
            state.pre_search_pane = state.active_pane.clone();
            state.input_mode      = InputMode::Grep;
            state.active_pane     = ActivePane::Search;
            state.grep.clear();
            rebuild_rows(state);
        }
        KeyCode::Char('f') => {
            state.pre_search_pane = state.active_pane.clone();
            state.input_mode      = InputMode::JobSearch;
            state.active_pane     = ActivePane::Search;
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
                InputMode::Grep      => { state.grep.pop(); }
                InputMode::JobSearch => { state.job_search.pop(); }
                _                    => {}
            }
            rebuild_rows(state);
        }
        KeyCode::Char(c) => {
            match state.input_mode {
                InputMode::Grep      => state.grep.push(c),
                InputMode::JobSearch => state.job_search.push(c),
                _                    => {}
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

/// Toggle expansion at the current tree level (job → classes, class → tests).
fn toggle_test_expansion(state: &mut InspectState) {
    let node = &state.nodes[state.selected_idx];

    if node.test_ref.is_some() {
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

/// Collapse: ← on a test leaf → collapse parent class; on a class → collapse parent job.
fn collapse_test_node(state: &mut InspectState) {
    let node = &state.nodes[state.selected_idx];

    if let Some((job_idx, test_idx)) = node.test_ref {
        // On a test leaf: collapse the parent class.
        let class_name = state.jobs[job_idx].tests[test_idx].class_name.clone();
        state.expanded_classes.remove(&(job_idx, class_name.clone()));
        rebuild_tree_and_reselect_class(state, job_idx, &class_name);
    } else if let Some((job_idx, _)) = node.class_ref.clone() {
        // On a class node: collapse the parent job.
        state.expanded_jobs.remove(&job_idx);
        rebuild_tree_and_reselect(state, Some(job_idx));
    }
}

/// Find the job index that the current node maps to (for job-level nodes only).
fn job_idx_for_node(state: &InspectState) -> Option<usize> {
    let node = &state.nodes[state.selected_idx];
    if node.test_ref.is_some() { return None; }
    let Filter::Prefix(p) = &node.filter else { return None; };
    state.jobs.iter().position(|j| &j.declared == p)
}

/// Rebuild tree nodes and re-select the job node for `job_idx` (or keep current selection).
fn rebuild_tree_and_reselect(state: &mut InspectState, job_idx: Option<usize>) {
    let prev_declared = job_idx.map(|i| state.jobs[i].declared.clone());
    state.nodes = build_tree_nodes(&state.jobs, &state.expanded_jobs, &state.expanded_classes);
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
    state.nodes = build_tree_nodes(&state.jobs, &state.expanded_jobs, &state.expanded_classes);
    if let Some(ni) = state.nodes.iter().position(|n| {
        n.class_ref.as_ref().map_or(false, |(ji, cn)| *ji == job_idx && cn == class_name)
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
            if state.show_members { ActivePane::Members } else { ActivePane::Log }
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
        state.active_pane  = ActivePane::Members;
    }
}

/// Clear the active search pattern and return focus to the pre-search pane.
fn clear_search(state: &mut InspectState) {
    match state.input_mode {
        InputMode::Grep      => state.grep.clear(),
        InputMode::JobSearch => state.job_search.clear(),
        InputMode::Normal    => {}
    }
    state.input_mode  = InputMode::Normal;
    state.active_pane = state.pre_search_pane.clone();
    rebuild_rows(state);
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(declared: &str, started_ms: Option<u64>, exit_code: Option<i32>) -> Job {
        let state = match exit_code {
            Some(0) => LogState::Ok     { duration_ms: 1000 },
            Some(_) => LogState::Failed { duration_ms: 800  },
            None    => LogState::NoLog,
        };
        let started_disp = started_ms.map(format_hms_utc).unwrap_or_default();
        Job {
            declared:     declared.to_string(),
            state,
            started_ms,
            started_disp,
            lines:    vec!["line one".to_string(), "line two".to_string()],
            build_id: None,
            tests:    vec![],
        }
    }

    // ── build_tree_nodes ─────────────────────────────────────────────────

    #[test]
    fn tree_root_then_flat_members() {
        let jobs  = vec![make_job("alpha", None, Some(0)), make_job("beta", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
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
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 4);
        assert!(nodes[1].label.contains("services/"));
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "services"));
        assert!(matches!(&nodes[2].filter, Filter::Prefix(p) if p == "services/api"));
        assert!(matches!(&nodes[3].filter, Filter::Prefix(p) if p == "services/web"));
    }

    #[test]
    fn tree_single_member() {
        let jobs  = vec![make_job("mylib", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 2);
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "mylib"));
    }

    #[test]
    fn tree_deep_nesting() {
        let jobs  = vec![make_job("a/b/c/leaf", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
        assert_eq!(nodes.len(), 5);
        assert!(matches!(&nodes[4].filter, Filter::Prefix(p) if p == "a/b/c/leaf"));
    }

    #[test]
    fn tree_container_filter_prefix() {
        let jobs = vec![
            make_job("svc/api", None, Some(0)),
            make_job("svc/web", None, Some(0)),
        ];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
        let svc   = nodes.iter().find(|n| n.label.contains("svc/")).unwrap();
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
        assert!( job_matches(&f, "services/api"));
        assert!(!job_matches(&f, "services/web"));
        assert!(!job_matches(&f, "services"));
    }

    #[test]
    fn match_subtree() {
        let f = Filter::Prefix("services".to_string());
        assert!( job_matches(&f, "services/api"));
        assert!( job_matches(&f, "services/web"));
        assert!(!job_matches(&f, "services-extra")); // must be a path boundary
    }

    #[test]
    fn match_prefix_not_a_path_boundary() {
        let f = Filter::Prefix("svc".to_string());
        assert!(!job_matches(&f, "svc-extra"));
        assert!( job_matches(&f, "svc/child"));
        assert!( job_matches(&f, "svc"));
    }

    // ── job_search_matches ───────────────────────────────────────────────

    #[test]
    fn job_search_empty_matches_all() {
        assert!(job_search_matches("services/api", ""));
        assert!(job_search_matches("anything", ""));
    }

    #[test]
    fn job_search_case_insensitive() {
        assert!( job_search_matches("Services/Api", "api"));
        assert!( job_search_matches("services/api", "API"));
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
            make_job("app",     Some(3000), Some(0)),
        ];
        let f    = Filter::Prefix("svc".to_string());
        let rows = build_rows(&jobs, &f, "", "");
        let headers: Vec<usize> = rows.iter().filter_map(|r| {
            if let Row::Header { job } = r { Some(*job) } else { None }
        }).collect();
        assert_eq!(headers, vec![0, 1]);
    }

    #[test]
    fn rows_unknown_start_sorts_last() {
        let jobs = vec![
            make_job("notime", None,       Some(0)),
            make_job("known",  Some(1000), Some(0)),
        ];
        let rows    = build_rows(&jobs, &Filter::All, "", "");
        let headers: Vec<usize> = rows.iter().filter_map(|r| {
            if let Row::Header { job } = r { Some(*job) } else { None }
        }).collect();
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
        let rows    = build_rows(&jobs, &Filter::All, "", "");
        let headers = rows.iter().filter(|r| matches!(r, Row::Header { .. })).count();
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
        let rows    = build_rows(&jobs, &Filter::All, "line", "");
        let headers: Vec<usize> = rows.iter().filter_map(|r| {
            if let Row::Header { job } = r { Some(*job) } else { None }
        }).collect();
        assert_eq!(headers, vec![0]); // only job a whose lines contain "line"
    }

    #[test]
    fn rows_job_search_filter() {
        let jobs = vec![
            make_job("services/api", Some(1000), Some(0)),
            make_job("services/web", Some(2000), Some(0)),
            make_job("app",          Some(3000), Some(0)),
        ];
        let rows    = build_rows(&jobs, &Filter::All, "", "api");
        let headers: Vec<usize> = rows.iter().filter_map(|r| {
            if let Row::Header { job } = r { Some(*job) } else { None }
        }).collect();
        assert_eq!(headers, vec![0]); // only services/api
    }

    // ── navigation ───────────────────────────────────────────────────────

    #[test]
    fn next_node_wraps() {
        let jobs  = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
        assert_eq!(next_node(&nodes, 2), 0);
    }

    #[test]
    fn prev_node_wraps() {
        let jobs  = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs, &HashSet::new(), &HashSet::new());
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
        let spans  = vec![Span::raw("hello world")];
        let result = highlight_spans(spans, "xyz");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
    }

    #[test]
    fn highlight_spans_single_match() {
        let spans  = vec![Span::raw("hello world")];
        let result = highlight_spans(spans, "world");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hello ");
        assert_eq!(result[1].content, "world");
        assert_eq!(result[1].style, Style::default().bg(Color::Yellow).fg(Color::Black));
    }

    #[test]
    fn highlight_spans_case_insensitive() {
        let spans  = vec![Span::raw("Hello World")];
        let result = highlight_spans(spans, "world");
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].content, "World"); // original case preserved
    }

    #[test]
    fn highlight_spans_multiple_matches() {
        let spans  = vec![Span::raw("abcabc")];
        let result = highlight_spans(spans, "abc");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "abc");
        assert_eq!(result[1].content, "abc");
    }
}
