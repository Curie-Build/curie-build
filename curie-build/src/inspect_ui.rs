//! Interactive TUI for browsing merged build logs (`curie inspect`).
//!
//! By default shows a single full-width log pane with every job's output merged
//! and sorted by job start time.  An optional members pane (toggled with Tab/m)
//! lets the user filter the merged log to a single workspace subtree or project.

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
    /// `.log` exists but no `.meta` sidecar (pre-v2 build).
    Legacy,
    /// No `.log` or `.meta`.
    NoLog,
}

struct Job {
    declared:     String,
    state:        LogState,
    /// `None` when no `.meta` is present.
    started_ms:   Option<u64>,
    /// `"HH:MM:SS"` UTC, or `""` when unknown.
    started_disp: String,
    lines:        Vec<String>,
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
    label:      String,
    /// Used as the log block title when this node is selected.
    title:      String,
    filter:     Filter,
    /// `None` for the root "all jobs" row and for container rows.
    state:      Option<LogState>,
    selectable: bool,
}

enum Row {
    Header { job: usize },
    /// `line` is an index into `jobs[job].lines`.
    Body   { job: usize, line: usize },
}

#[derive(PartialEq)]
enum ActivePane { Members, Log }

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
}

// ── Entry point ───────────────────────────────────────────────────────────

pub(crate) fn run_inspect_ui(
    _ws_root:  &std::path::Path,
    targets:   &[LogTarget],
    action:    &str,
    preselect: Option<usize>,
) -> Result<()> {
    let jobs   = load_jobs(targets, action);
    let nodes  = build_tree_nodes(&jobs);
    let filter = Filter::All;
    let rows   = build_rows(&jobs, &filter);

    let mut state = InspectState {
        targets:      targets.to_vec(),
        action:       action.to_string(),
        jobs,
        nodes,
        selected_idx: 0,
        tree_scroll:  0,
        rows,
        scroll:       0,
        show_members: false,
        active_pane:  ActivePane::Log,
        filter,
        log_title:    "all jobs".to_string(),
        pane_h:       24,
    };

    // Position tree cursor without changing the filter — filter stays All.
    if let Some(idx) = preselect {
        if idx < state.jobs.len() {
            if let Some(ni) = find_node_for_declared(&state.nodes, &state.jobs[idx].declared) {
                state.selected_idx = ni;
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

fn load_jobs(targets: &[LogTarget], action: &str) -> Vec<Job> {
    targets.iter().map(|t| {
        let log_path  = t.path.join("target").join(format!("{action}.log"));
        let meta_path = t.path.join("target").join(format!("{action}.meta"));

        let meta = parse_meta(&meta_path);

        let state = match (meta.as_ref(), log_path.exists()) {
            (Some(m), _) if m.exit_code == 0 => LogState::Ok     { duration_ms: m.duration_ms },
            (Some(m), _)                     => LogState::Failed  { duration_ms: m.duration_ms },
            (None, true)                     => LogState::Legacy,
            (None, false)                    => LogState::NoLog,
        };

        let (started_ms, started_disp) = meta.as_ref()
            .map(|m| (Some(m.started_ms), format_hms_utc(m.started_ms)))
            .unwrap_or((None, String::new()));

        let lines = if log_path.exists() { load_log(&log_path) } else { Vec::new() };

        Job { declared: t.declared.clone(), state, started_ms, started_disp, lines }
    }).collect()
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

fn build_tree_nodes(jobs: &[Job]) -> Vec<TreeNode> {
    let mut nodes = vec![TreeNode {
        label:      "all jobs".to_string(),
        title:      "all jobs".to_string(),
        filter:     Filter::All,
        state:      None,
        selectable: true,
    }];

    let mut current_dirs: Vec<String> = Vec::new();

    for job in jobs {
        let parts: Vec<&str> = job.declared.split('/').collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        let name = parts.last().copied().unwrap_or(&job.declared);

        // Common prefix length with the previously emitted directory stack.
        // Use a for-loop: take_while passes &Item, causing &&str vs &str issues.
        let common = {
            let mut n = 0;
            for (a, b) in dirs.iter().zip(current_dirs.iter()) {
                if *a == b.as_str() { n += 1; } else { break; }
            }
            n
        };

        // Container nodes for each new directory segment.
        for depth in common..dirs.len() {
            let path_here = dirs[..=depth].join("/");
            let indent    = "  ".repeat(depth + 1);
            nodes.push(TreeNode {
                label:      format!("{indent}{}/", dirs[depth]),
                title:      format!("{path_here}/"),
                filter:     Filter::Prefix(path_here),
                state:      None,
                selectable: true,
            });
        }

        // Leaf node.
        let depth  = dirs.len();
        let indent = "  ".repeat(depth + 1);
        nodes.push(TreeNode {
            label:      format!("{indent}{name}"),
            title:      job.declared.clone(),
            filter:     Filter::Prefix(job.declared.clone()),
            state:      Some(job.state.clone()),
            selectable: true,
        });

        current_dirs = dirs.iter().map(|s| s.to_string()).collect();
    }

    nodes
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

// ── Row building ──────────────────────────────────────────────────────────

fn build_rows(jobs: &[Job], filter: &Filter) -> Vec<Row> {
    let mut indices: Vec<usize> = (0..jobs.len())
        .filter(|&i| job_matches(filter, &jobs[i].declared))
        .collect();

    // Earlier start time first; unknown start (no meta) sorts last.
    indices.sort_by(|&a, &b| {
        let sa = jobs[a].started_ms.unwrap_or(u64::MAX);
        let sb = jobs[b].started_ms.unwrap_or(u64::MAX);
        sa.cmp(&sb).then_with(|| jobs[a].declared.cmp(&jobs[b].declared))
    });

    let mut rows = Vec::new();
    for ji in indices {
        rows.push(Row::Header { job: ji });
        for li in 0..jobs[ji].lines.len() {
            rows.push(Row::Body { job: ji, line: li });
        }
    }
    rows
}

// ── Selection and reload ──────────────────────────────────────────────────

fn apply_selection(state: &mut InspectState) {
    let node        = &state.nodes[state.selected_idx];
    state.filter    = node.filter.clone();
    state.log_title = node.title.clone();
    state.rows      = build_rows(&state.jobs, &state.filter);
    state.scroll    = 0;
    sync_tree_scroll(state);
}

fn reload(state: &mut InspectState) {
    let targets = state.targets.clone();
    let action  = state.action.clone();
    state.jobs  = load_jobs(&targets, &action);
    state.nodes = build_tree_nodes(&state.jobs);
    state.selected_idx = state.selected_idx.min(state.nodes.len().saturating_sub(1));
    state.rows  = build_rows(&state.jobs, &state.filter);
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
        LogState::Ok     { .. } => Color::Green,
        LogState::Failed { .. } => Color::Red,
        LogState::Legacy        => Color::Yellow,
        LogState::NoLog         => Color::DarkGray,
    }
}

fn badge_str(state: &LogState) -> String {
    match state {
        LogState::Ok     { duration_ms } => format!("✓ {}", fmt_duration(*duration_ms)),
        LogState::Failed { duration_ms } => format!("✗ {}", fmt_duration(*duration_ms)),
        LogState::Legacy                 => "(legacy)".to_string(),
        LogState::NoLog                  => "(no log)".to_string(),
    }
}

fn badge_style(state: &LogState) -> Style {
    match state {
        LogState::Ok     { .. } => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        LogState::Failed { .. } => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        LogState::Legacy        => Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
        LogState::NoLog         => Style::default().add_modifier(Modifier::DIM),
    }
}

fn fmt_duration(ms: u64) -> String {
    let tenths = ms / 100;
    format!("{}.{}s", tenths / 10, tenths % 10)
}

/// Format epoch-milliseconds as `HH:MM:SS` (UTC).
fn format_hms_utc(epoch_ms: u64) -> String {
    let secs   = epoch_ms / 1000;
    let time_s = secs % 86400;
    let h = time_s / 3600;
    let m = (time_s % 3600) / 60;
    let s = time_s % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

// ── Rendering ─────────────────────────────────────────────────────────────

fn render_frame(f: &mut Frame, state: &InspectState) {
    let total = f.area();

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(total);

    // Header hint line
    let hint = if state.show_members && state.active_pane == ActivePane::Members {
        "curie inspect  \u{2191}\u{2193}/jk select  PgUp/Dn scroll  Tab log  r reload  q quit"
    } else {
        "curie inspect  PgUp/Dn scroll  g/G top/bot  m members  r reload  q quit"
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
        vchunks[0],
    );

    // Body
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
}

fn render_members_block(f: &mut Frame, state: &InspectState, area: Rect) {
    let is_active = state.active_pane == ActivePane::Members;
    let border_style = if is_active { Style::default().fg(Color::Cyan) } else { Style::default() };

    let block = Block::default()
        .title("Members")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner   = block.inner(area);
    let vis_h   = inner.height as usize;
    let inner_w = inner.width  as usize;

    let lines: Vec<Line<'static>> = state.nodes.iter()
        .enumerate()
        .skip(state.tree_scroll)
        .take(vis_h)
        .map(|(i, node)| member_line(node, i == state.selected_idx, inner_w))
        .collect();

    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn member_line(node: &TreeNode, is_selected: bool, inner_w: usize) -> Line<'static> {
    match &node.state {
        Some(state) => {
            let badge   = badge_str(state);
            let bstyle  = badge_style(state);
            let badge_w = badge.chars().count();
            let label_w = inner_w.saturating_sub(badge_w + 1);
            let label:  String = node.label.chars().take(label_w).collect();
            let padding = " ".repeat(label_w.saturating_sub(label.chars().count()) + 1);

            let mut line = Line::from(vec![
                Span::raw(label),
                Span::raw(padding),
                Span::styled(badge, bstyle),
            ]);
            if is_selected {
                line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        }
        None => {
            let style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if matches!(node.filter, Filter::Prefix(_)) {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::styled(node.label.clone(), style)
        }
    }
}

fn render_log_block(f: &mut Frame, state: &InspectState, area: Rect) {
    let is_active = !state.show_members || state.active_pane == ActivePane::Log;
    let border_style = if is_active { Style::default().fg(Color::Cyan) } else { Style::default() };

    let title = format!("Log: {}", state.log_title);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let scroll = state.scroll.min(u16::MAX as usize) as u16;
    let text   = build_log_text(state);
    let para   = Paragraph::new(text).block(block).scroll((scroll, 0));

    f.render_widget(para, area);
}

fn build_log_text(state: &InspectState) -> Text<'static> {
    let lines: Vec<Line<'static>> = state.rows.iter().map(|row| match row {
        Row::Header { job } => {
            let j     = &state.jobs[*job];
            let color = gutter_color(&j.state);
            header_line(j, color)
        }
        Row::Body { job, line } => {
            let j     = &state.jobs[*job];
            let color = gutter_color(&j.state);
            body_line(&j.lines[*line], color)
        }
    }).collect();
    Text::from(lines)
}

fn header_line(job: &Job, color: Color) -> Line<'static> {
    let gutter  = Span::styled("▎ ", Style::default().fg(color));
    let content = if job.started_disp.is_empty() {
        format!("{}  {}", job.declared, badge_str(&job.state))
    } else {
        format!("{}  started {}  {}", job.declared, job.started_disp, badge_str(&job.state))
    };
    let text = Span::styled(content, Style::default().fg(color).add_modifier(Modifier::BOLD));
    Line::from(vec![gutter, text])
}

fn body_line(text: &str, color: Color) -> Line<'static> {
    let gutter = Span::styled("▎", Style::default().fg(color));
    let mut spans = vec![gutter];
    spans.extend(parse_ansi_line(text));
    Line::from(spans)
}

fn parse_ansi_line(s: &str) -> Vec<Span<'static>> {
    match s.into_text() {
        Ok(mut text) => text.lines.pop().map(|l| l.spans).unwrap_or_default(),
        Err(_)       => vec![Span::raw(s.to_string())],
    }
}

// ── Event loop ────────────────────────────────────────────────────────────

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state:    &mut InspectState,
) -> Result<()> {
    loop {
        terminal.draw(|f| render_frame(f, state))?;

        // Keep pane_h in sync; used by scroll arithmetic and sync_tree_scroll.
        let size = terminal.size()?;
        state.pane_h = size.height.saturating_sub(1);

        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if !handle_key(state, key) {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {} // next draw picks up new dimensions
            _ => {}
        }
    }
}

fn handle_key(state: &mut InspectState, key: KeyEvent) -> bool {
    let members_active = state.show_members && state.active_pane == ActivePane::Members;
    let log_ph         = (state.pane_h as usize).saturating_sub(2); // inner height

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return false,

        // Members pane toggle / pane switch
        KeyCode::Tab | KeyCode::Char('m') => toggle_members(state),

        // Tree navigation (members pane must be active)
        KeyCode::Up | KeyCode::Char('k') if members_active => {
            state.selected_idx = prev_node(&state.nodes, state.selected_idx);
            apply_selection(state);
        }
        KeyCode::Down | KeyCode::Char('j') if members_active => {
            state.selected_idx = next_node(&state.nodes, state.selected_idx);
            apply_selection(state);
        }

        // Log scrolling (always available)
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(log_ph.max(1));
        }
        KeyCode::PageDown => {
            let max = state.rows.len().saturating_sub(1);
            state.scroll = (state.scroll + log_ph.max(1)).min(max);
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

        // Reload from disk
        KeyCode::Char('r') => reload(state),

        _ => {}
    }
    true
}

/// Cycle: hidden → Members-active → Log-active → hidden → …
fn toggle_members(state: &mut InspectState) {
    if !state.show_members {
        state.show_members = true;
        state.active_pane  = ActivePane::Members;
    } else if state.active_pane == ActivePane::Members {
        state.active_pane = ActivePane::Log;
    } else {
        state.show_members = false;
        state.active_pane  = ActivePane::Log;
    }
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
            lines: vec!["line one".to_string(), "line two".to_string()],
        }
    }

    // ── build_tree_nodes ─────────────────────────────────────────────────

    #[test]
    fn tree_root_then_flat_members() {
        let jobs  = vec![make_job("alpha", None, Some(0)), make_job("beta", None, Some(0))];
        let nodes = build_tree_nodes(&jobs);
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
        let nodes = build_tree_nodes(&jobs);
        // root + container(services/) + leaf(api) + leaf(web)
        assert_eq!(nodes.len(), 4);
        assert!(nodes[1].label.contains("services/"));
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "services"));
        assert!(matches!(&nodes[2].filter, Filter::Prefix(p) if p == "services/api"));
        assert!(matches!(&nodes[3].filter, Filter::Prefix(p) if p == "services/web"));
    }

    #[test]
    fn tree_single_member() {
        let jobs  = vec![make_job("mylib", None, Some(0))];
        let nodes = build_tree_nodes(&jobs);
        assert_eq!(nodes.len(), 2); // root + leaf
        assert!(matches!(&nodes[1].filter, Filter::Prefix(p) if p == "mylib"));
    }

    #[test]
    fn tree_deep_nesting() {
        let jobs  = vec![make_job("a/b/c/leaf", None, Some(0))];
        let nodes = build_tree_nodes(&jobs);
        // root + a/ + b/ + c/ + leaf = 5
        assert_eq!(nodes.len(), 5);
        assert!(matches!(&nodes[4].filter, Filter::Prefix(p) if p == "a/b/c/leaf"));
    }

    #[test]
    fn tree_container_filter_prefix() {
        let jobs = vec![
            make_job("svc/api", None, Some(0)),
            make_job("svc/web", None, Some(0)),
        ];
        let nodes  = build_tree_nodes(&jobs);
        let svc    = nodes.iter().find(|n| n.label.contains("svc/")).unwrap();
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

    // ── build_rows ───────────────────────────────────────────────────────

    #[test]
    fn rows_sorted_by_start_time() {
        let jobs = vec![
            make_job("b", Some(2000), Some(0)),
            make_job("a", Some(1000), Some(0)),
        ];
        let rows = build_rows(&jobs, &Filter::All);
        // Job 1 (a, earlier start) should come first.
        assert!(matches!(rows[0], Row::Header { job: 1 }));
        // Job 0 (b) follows after a's 2 body lines.
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
        let rows = build_rows(&jobs, &f);
        let headers: Vec<usize> = rows.iter().filter_map(|r| {
            if let Row::Header { job } = r { Some(*job) } else { None }
        }).collect();
        assert_eq!(headers, vec![0, 1]); // api then web; app excluded
    }

    #[test]
    fn rows_unknown_start_sorts_last() {
        let jobs = vec![
            make_job("notime", None,       Some(0)),
            make_job("known",  Some(1000), Some(0)),
        ];
        let rows    = build_rows(&jobs, &Filter::All);
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
        let rows    = build_rows(&jobs, &Filter::All);
        let headers = rows.iter().filter(|r| matches!(r, Row::Header { .. })).count();
        assert_eq!(headers, 3);
    }

    // ── navigation ───────────────────────────────────────────────────────

    #[test]
    fn next_node_wraps() {
        let jobs  = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs); // [root, a, b]
        assert_eq!(next_node(&nodes, 2), 0); // wraps from last to root
    }

    #[test]
    fn prev_node_wraps() {
        let jobs  = vec![make_job("a", None, Some(0)), make_job("b", None, Some(0))];
        let nodes = build_tree_nodes(&jobs);
        assert_eq!(prev_node(&nodes, 0), 2); // wraps from root to last
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
}
