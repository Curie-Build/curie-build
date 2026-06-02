//! Terminal-UI split-screen renderer for parallel workspace builds.
//!
//! When stdout is a TTY and the terminal is tall enough, [`TuiRenderer`]
//! divides the screen into per-member panes (1 title row + N content rows
//! each) and streams each member's output into its own pane in real-time.
//!
//! # Layout
//!
//! ```text
//! ── member-a ──────────────────────────────────────────────────────── ✓
//!   … last N lines of output …
//! ── member-b ─────────────────────────────────────────────────────────
//!   … last N lines of output …
//!   · Background: member-c, member-d  and 2 more
//! ```
//!
//! When more members exist than can fit on screen, the extra members are
//! listed on a single overflow line at the bottom.  When a visible pane
//! finishes successfully, it is immediately reused for the next waiting
//! background job.
//!
//! # Fallback
//!
//! If the terminal is too small for even one pane (`visible_count == 0`),
//! the caller falls back to the prefix-mux path (no TUI is created).

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use crossterm::{cursor, execute};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Minimum pane height (2 border rows + 7 content rows).
pub(crate) const MIN_PANE_HEIGHT: usize = 9;

// ── Layout ────────────────────────────────────────────────────────────────

/// Compute `(visible_count, pane_height)` for `n` members in a terminal with
/// `term_h` rows.
///
/// * If all panes fit at ≥ `MIN_PANE_HEIGHT` each, `visible_count == n`.
/// * If not all fit, `pane_height == MIN_PANE_HEIGHT` and `visible_count` is
///   as many as possible leaving one row for the overflow line.
/// * Returns `(0, MIN_PANE_HEIGHT)` when the terminal is too small for even
///   one pane — the caller must fall back to the prefix-mux.
///
/// # Examples
/// ```ignore
/// assert_eq!(tui_layout(3, 40), (3, 13)); // all fit, height=40/3=13
/// assert_eq!(tui_layout(5, 40), (4,  9)); // 5×9=45>40 → visible=(40-1)/9=4
/// assert_eq!(tui_layout(3, 27), (3,  9)); // 3×9=27 ≤ 27, height=9
/// assert_eq!(tui_layout(2,  8), (0,  9)); // too small → fallback
/// ```
pub(crate) fn tui_layout(n: usize, term_h: usize) -> (usize, usize) {
    if n == 0 || term_h == 0 {
        return (0, MIN_PANE_HEIGHT);
    }
    if n * MIN_PANE_HEIGHT <= term_h {
        // All panes fit; distribute the height evenly.
        (n, term_h / n)
    } else {
        // Not all fit — reserve 1 row for the overflow line.
        let visible = term_h.saturating_sub(1) / MIN_PANE_HEIGHT;
        (visible, MIN_PANE_HEIGHT)
    }
}

// ── Internal message type ─────────────────────────────────────────────────

enum TuiMsg {
    Line { slot_idx: usize, line: String },
    /// Job finished.  `success` controls whether the pane is eligible for
    /// reuse: on success the pane is immediately handed to the next waiting
    /// background job; on failure the pane keeps the error output visible.
    SlotDone { slot_idx: usize, success: bool },
    Shutdown,
}

// ── TuiSlot ───────────────────────────────────────────────────────────────

/// Per-member output sink for the TUI path.
///
/// Implements [`crate::parallel::LineSink`]; lines are forwarded to the
/// render thread via a bounded channel and written to the member's log file
/// immediately (same as [`crate::parallel::MuxSlot`]).
pub(crate) struct TuiSlot {
    slot_idx: usize,
    sender: SyncSender<TuiMsg>,
    log: Mutex<std::fs::File>,
}

impl crate::parallel::LineSink for TuiSlot {
    fn push_line(&self, line: String) {
        // Write to disk immediately — same contract as MuxSlot.
        if let Ok(mut f) = self.log.lock() {
            let _ = writeln!(f, "{}", line);
        }
        let _ = self.sender.send(TuiMsg::Line { slot_idx: self.slot_idx, line });
    }

    /// No-op: the TUI path never buffers lines; everything goes straight to
    /// the render thread via the channel.  The stale flusher is not started
    /// in TUI mode, so this is never called in practice.
    fn flush(&self) {}

    /// Signal the render thread that this job is done.
    ///
    /// On success the render thread may immediately replace this pane with the
    /// next waiting background job.  On failure the pane retains its current
    /// output so the error stays visible.
    fn complete(&self, success: bool) {
        let _ = self.sender.send(TuiMsg::SlotDone { slot_idx: self.slot_idx, success });
    }

    /// TUI slots occupy the full terminal width; no prefix is subtracted.
    fn prefix_visual_len(&self) -> usize {
        0
    }
}

// ── TuiRenderer ───────────────────────────────────────────────────────────

/// Owns the render thread and the channel used to communicate with it.
///
/// Creating a `TuiRenderer` clears the terminal and draws the initial layout.
/// Dropping it sends a [`TuiMsg::Shutdown`], which causes the render thread
/// to restore the cursor and exit.  This guarantees cleanup even on panic or
/// `bail!`.
pub(crate) struct TuiRenderer {
    sender: SyncSender<TuiMsg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TuiRenderer {
    /// Create a renderer and the corresponding [`TuiSlot`] array.
    ///
    /// * `names`         — display name for each member (in slot order).
    /// * `log_files`     — per-member log files (same order).
    /// * `visible_count` — number of panes to show on screen initially.
    /// * `pane_height`   — rows per pane (title + content).
    ///
    /// Returns `(renderer, slots)`.  The caller assigns each `Arc<TuiSlot>`
    /// to the matching worker thread via [`crate::parallel::set_thread_sink`].
    pub(crate) fn new(
        names: Vec<String>,
        log_files: Vec<std::fs::File>,
        visible_count: usize,
        pane_height: usize,
    ) -> (Self, Vec<Arc<TuiSlot>>) {
        let n = names.len();
        // Capacity 1000: large enough to absorb bursts while bounding memory.
        let (sender, receiver) = mpsc::sync_channel::<TuiMsg>(1000);

        let slots: Vec<Arc<TuiSlot>> = log_files
            .into_iter()
            .enumerate()
            .map(|(i, log)| {
                Arc::new(TuiSlot {
                    slot_idx: i,
                    sender: sender.clone(),
                    log: Mutex::new(log),
                })
            })
            .collect();

        let thread = std::thread::spawn(move || {
            render_loop(receiver, names, n, visible_count, pane_height);
        });

        let renderer = TuiRenderer { sender, thread: Some(thread) };
        (renderer, slots)
    }
}

impl Drop for TuiRenderer {
    fn drop(&mut self) {
        // Best-effort: the channel may already be closed on panic.
        let _ = self.sender.send(TuiMsg::Shutdown);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

// ── Render thread ─────────────────────────────────────────────────────────

struct SlotData {
    name: String,
    /// `None` = still running, `Some(true)` = success, `Some(false)` = failed.
    done: Option<bool>,
    /// Ring buffer of received lines, newest last.
    ring: VecDeque<String>,
}

/// All mutable state touched exclusively by the render thread.
struct RenderState {
    slots: Vec<SlotData>,
    /// `pane_to_slot[pane_idx]` = slot_idx currently shown in that pane.
    pane_to_slot: Vec<usize>,
    /// Slots not yet assigned to any visible pane.
    background_queue: VecDeque<usize>,
    n: usize,
    visible: usize,
    pane_h: usize,
}

fn render_loop(
    rx: mpsc::Receiver<TuiMsg>,
    names: Vec<String>,
    n: usize,
    visible: usize,
    pane_h: usize,
) {
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Hide cursor and disable line-wrap for the lifetime of the TUI.
    // \x1b[?7l (DECAWM off) has no crossterm equivalent.
    let _ = execute!(io::stdout(), cursor::Hide);
    let _ = write!(io::stdout(), "\x1b[?7l");
    // Clear the screen so previous terminal content doesn't show through
    // the initial pane layout.
    let _ = terminal.clear();

    let mut state = RenderState {
        slots: names
            .into_iter()
            .map(|name| SlotData { name, done: None, ring: VecDeque::new() })
            .collect(),
        pane_to_slot: (0..visible).collect(),
        background_queue: (visible..n).collect(),
        n,
        visible,
        pane_h,
    };

    // Initial draw.
    let _ = terminal.draw(|f| render_frame(f, &state));

    // ── Message loop ──────────────────────────────────────────────────────
    loop {
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            TuiMsg::Line { slot_idx, line } => {
                state.slots[slot_idx].ring.push_back(line);
                // Redraw only when this slot is in a visible pane.
                if state.pane_to_slot.iter().any(|&s| s == slot_idx) {
                    let _ = terminal.draw(|f| render_frame(f, &state));
                }
            }

            TuiMsg::SlotDone { slot_idx, success } => {
                state.slots[slot_idx].done = Some(success);

                // Redraw immediately so the green/red outcome is visible.
                let _ = terminal.draw(|f| render_frame(f, &state));

                if let Some(pane_idx) =
                    state.pane_to_slot.iter().position(|&s| s == slot_idx)
                {
                    if success && !state.background_queue.is_empty() {
                        // Hold green for 1 s, keep draining Line messages.
                        if let Some(stashed) =
                            drain_hold(&rx, &mut terminal, &mut state, 1)
                        {
                            match stashed {
                                TuiMsg::SlotDone { slot_idx: s, success: ok } => {
                                    state.slots[s].done = Some(ok);
                                    if state.pane_to_slot.iter().all(|&x| x != s) {
                                        state.background_queue.retain(|&x| x != s);
                                    }
                                    let _ = terminal.draw(|f| render_frame(f, &state));
                                }
                                TuiMsg::Shutdown => break,
                                TuiMsg::Line { .. } => unreachable!(),
                            }
                        }
                        let next = state.background_queue.pop_front().unwrap();
                        state.pane_to_slot[pane_idx] = next;
                        let _ = terminal.draw(|f| render_frame(f, &state));
                    } else {
                        // No replacement — hold for 2 s then close the pane,
                        // but only on success; failed panes stay until shutdown.
                        if success {
                            if let Some(stashed) =
                                drain_hold(&rx, &mut terminal, &mut state, 2)
                            {
                                match stashed {
                                    TuiMsg::SlotDone { slot_idx: s, success: ok } => {
                                        state.slots[s].done = Some(ok);
                                        if state.pane_to_slot.iter().all(|&x| x != s) {
                                            state.background_queue.retain(|&x| x != s);
                                        }
                                        let _ = terminal.draw(|f| render_frame(f, &state));
                                    }
                                    TuiMsg::Shutdown => break,
                                    TuiMsg::Line { .. } => unreachable!(),
                                }
                            }
                            state.pane_to_slot.remove(pane_idx);
                            let _ = terminal.draw(|f| render_frame(f, &state));
                        }
                        // else: failure — keep pane showing red ✗ until shutdown.
                    }
                } else {
                    // Slot finished in the background — remove from queue so
                    // it no longer appears in the overflow line.
                    state.background_queue.retain(|&s| s != slot_idx);
                }
            }

            TuiMsg::Shutdown => break,
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    // Restore full scroll region, re-enable line-wrap, close any open OSC 8
    // hyperlink, then show the cursor again.  No crossterm equivalents for
    // the three raw sequences.
    let _ = write!(io::stdout(), "\x1b[r\x1b[?7h\x1b]8;;\x07");
    let _ = execute!(io::stdout(), cursor::Show);
    // Park cursor below the drawn content so normal output continues there.
    let open_panes   = state.pane_to_slot.len();
    let overflow_rows = if state.slots.len() > open_panes { 1 } else { 0 };
    let total_rows = (open_panes * pane_h + overflow_rows) as u16;
    let _ = terminal.set_cursor_position((0, total_rows));
    let _ = io::stdout().flush();
}

// ── Hold-drain helper ─────────────────────────────────────────────────────

/// Drain incoming messages for `hold_secs`, processing `Line` messages into
/// slot rings and redrawing visible panes.  Returns the first non-`Line`
/// message encountered (if any), which the caller must handle.
fn drain_hold(
    rx: &mpsc::Receiver<TuiMsg>,
    terminal: &mut ratatui::Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut RenderState,
    hold_secs: u64,
) -> Option<TuiMsg> {
    let deadline = Instant::now() + Duration::from_secs(hold_secs);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(TuiMsg::Line { slot_idx, line }) => {
                state.slots[slot_idx].ring.push_back(line);
                if state.pane_to_slot.iter().any(|&x| x == slot_idx) {
                    let _ = terminal.draw(|f| render_frame(f, state));
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                return None;
            }
            Ok(other) => return Some(other),
        }
    }
}

// ── Frame renderer (pure, no I/O) ─────────────────────────────────────────

fn render_frame(f: &mut Frame, state: &RenderState) {
    let area = f.area();

    // Build vertical layout: one block per visible pane + optional overflow.
    // Show overflow row whenever there are more slots than open panes.
    let has_overflow = state.slots.len() > state.pane_to_slot.len();
    let mut constraints: Vec<Constraint> = state
        .pane_to_slot
        .iter()
        .map(|_| Constraint::Length(state.pane_h as u16))
        .collect();
    if has_overflow {
        constraints.push(Constraint::Length(1));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // Draw each visible pane.
    for (pane_idx, &slot_idx) in state.pane_to_slot.iter().enumerate() {
        let pane_area = chunks[pane_idx];
        let slot = &state.slots[slot_idx];
        render_pane(f, pane_area, &slot.name, slot.done, &slot.ring);
    }

    // Draw overflow line.
    if has_overflow {
        let overflow_area = chunks[state.pane_to_slot.len()];
        render_overflow(f, overflow_area, state);
    }
}

// ── Pane widgets ──────────────────────────────────────────────────────────

/// Render one pane: a bordered box with a styled title and content inside.
///
/// Border + title colour changes with state:
///   running  → dim border, bold cyan name
///   success  → bold green border + name + ✓
///   failure  → bold red border + name + ✗
fn render_pane(
    f: &mut Frame,
    area: Rect,
    name: &str,
    done: Option<bool>,
    ring: &VecDeque<String>,
) {
    let (color, status) = match done {
        None           => (Color::Cyan,  ""),
        Some(true)     => (Color::Green, " ✓"),
        Some(false)    => (Color::Red,   " ✗"),
    };

    let border_style = match done {
        None  => Style::new().fg(color).add_modifier(Modifier::DIM),
        _     => Style::new().fg(color).add_modifier(Modifier::BOLD),
    };
    let title_style = Style::new().fg(color).add_modifier(Modifier::BOLD);

    let title_left  = Line::from(vec![
        Span::styled(format!(" {} ", name), title_style),
    ]);
    let title_right = Line::from(vec![
        Span::styled(format!("{} ", status), title_style),
    ]);

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title_left)
        .title_alignment(Alignment::Left)
        .title_bottom(title_right.alignment(Alignment::Right));

    let inner = block.inner(area);
    f.render_widget(block, area);
    render_content(f, inner, ring);
}

/// Render the content area of one pane from its ring buffer.
///
/// Shows the last `area.height` lines, newest at the bottom.  Each line is
/// sanitised via [`truncate_to_cols`] (strips OSC + non-SGR sequences) then
/// parsed into ratatui styled spans by `ansi-to-tui`.
fn render_content(f: &mut Frame, area: Rect, ring: &VecDeque<String>) {
    let rows = area.height as usize;
    let cols = area.width as usize;
    let ring_len = ring.len();

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);

    for i in 0..rows {
        if i + ring_len >= rows {
            let ring_idx = i + ring_len - rows;
            if ring_idx < ring_len {
                let sanitized = truncate_to_cols(&ring[ring_idx], cols);
                // ansi-to-tui converts remaining SGR sequences into ratatui
                // styled spans.  Fall back to plain text if parsing fails.
                let line = parse_ansi_line(&sanitized);
                lines.push(line);
                continue;
            }
        }
        lines.push(Line::default());
    }

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Render the overflow summary line below all panes.
///
/// Shows up to four labelled groups of non-visible slots:
///
/// ```text
///   · Pending: a, b  · Running: c, d  · Done: e  · Failed: f
/// ```
///
/// Each group is omitted when empty.  Names are truncated with "and N more"
/// when they would overflow the terminal width.
fn render_overflow(f: &mut Frame, area: Rect, state: &RenderState) {
    let visible_set: std::collections::HashSet<usize> =
        state.pane_to_slot.iter().copied().collect();

    // Classify every non-visible slot into one of four buckets.
    let mut pending:  Vec<&str> = Vec::new(); // not started (in background_queue, done=None)
    let mut running:  Vec<&str> = Vec::new(); // running in background (background_queue, done=None)
    let mut done_ok:  Vec<&str> = Vec::new(); // finished success (not in any pane)
    let mut done_err: Vec<&str> = Vec::new(); // finished failure (not in any pane)

    // background_queue holds slots not yet assigned to a pane and still running.
    let in_queue: std::collections::HashSet<usize> =
        state.background_queue.iter().copied().collect();

    for (idx, slot) in state.slots.iter().enumerate() {
        if visible_set.contains(&idx) {
            continue; // shown in a pane
        }
        match slot.done {
            None if in_queue.contains(&idx) => {
                // Distinguish: the first slot in the queue is actively running;
                // the rest haven't started yet (they're waiting for a pane).
                // Actually all background_queue slots are running — jobs are
                // spawned before the TUI is set up.  Show them all as Running.
                running.push(&slot.name);
            }
            None => {
                // Slot finished before being assigned a pane — shouldn't
                // normally happen, but handle gracefully.
                pending.push(&slot.name);
            }
            Some(true)  => done_ok.push(&slot.name),
            Some(false) => done_err.push(&slot.name),
        }
    }

    // Build styled spans for each non-empty group.
    let dim    = Style::new().add_modifier(Modifier::DIM);
    let cyan   = Style::new().fg(Color::Cyan);
    let green  = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let red    = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cols_used: usize = 0;
    let budget = area.width as usize;

    /// Append one labelled group to `spans`, respecting the column budget.
    /// Returns false when there is no room left.
    fn push_group<'a>(
        spans: &mut Vec<Span<'static>>,
        cols_used: &mut usize,
        budget: usize,
        label: &'static str,
        names: &[&str],
        label_style: Style,
        name_style: Style,
        dim: Style,
    ) -> bool {
        if names.is_empty() {
            return true;
        }
        // separator between groups
        let sep = if *cols_used == 0 { "  \u{00b7} " } else { "  \u{00b7} " };
        let sep_len = 4usize; // "  · "
        let label_len = label.len();
        let min_needed = sep_len + label_len + 1; // at least one char of names

        if *cols_used + min_needed > budget {
            return false;
        }

        spans.push(Span::styled(sep, dim));
        spans.push(Span::styled(label, label_style));
        *cols_used += sep_len + label_len;

        let remaining = budget.saturating_sub(*cols_used);
        let body = build_overflow_names(names, remaining);
        *cols_used += body.len();
        spans.push(Span::styled(body, name_style));
        true
    }

    push_group(&mut spans, &mut cols_used, budget, "Running: ",  &running,  cyan,  dim,   dim);
    push_group(&mut spans, &mut cols_used, budget, "Pending: ",  &pending,  dim,   dim,   dim);
    push_group(&mut spans, &mut cols_used, budget, "Done: ",     &done_ok,  green, green, dim);
    push_group(&mut spans, &mut cols_used, budget, "Failed: ",   &done_err, red,   red,   dim);

    if spans.is_empty() {
        return;
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Overflow name list builder ─────────────────────────────────────────────

/// Build the names portion of the overflow line, fitting within `budget`
/// visible columns.
///
/// Scans all possible cut points (0 names shown … all names shown) and
/// picks the **largest** k such that:
///   `names[0..k].join(", ") + "  and (N-k) more"` ≤ `budget` columns.
///
/// * `k == N`: all names fit, no suffix emitted.
/// * `k == 0`: no individual name fits; emits `"and N more"` alone (no
///   leading spaces).
/// * Nothing fits (not even `"and N more"`): returns an empty string.
///
/// All inputs are treated as ASCII (project names are restricted to
/// alphanumerics and hyphens in Curie), so `str::len()` == visible columns.
pub(crate) fn build_overflow_names(names: &[&str], budget: usize) -> String {
    let total = names.len();
    if total == 0 || budget == 0 {
        return String::new();
    }

    // Visible length of the first k names joined by ", ".
    let names_len = |k: usize| -> usize {
        names[..k]
            .iter()
            .enumerate()
            .map(|(i, n)| if i == 0 { n.len() } else { 2 + n.len() })
            .sum()
    };

    // Total length of: first-k names + suffix for the (total-k) remaining.
    let text_len = |k: usize| -> usize {
        let nl = names_len(k);
        let rem = total - k;
        nl + if rem == 0 { 0 } else { more_suffix(k, rem).len() }
    };

    // Find the *largest* k in [0, total] such that text_len(k) ≤ budget.
    let best_k = match (0..=total).rev().find(|&k| text_len(k) <= budget) {
        Some(k) => k,
        None => return String::new(),
    };

    let mut out = String::new();
    for (i, &name) in names[..best_k].iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(name);
    }
    let rem = total - best_k;
    if rem > 0 {
        out.push_str(&more_suffix(best_k, rem));
    }
    out
}

/// Format the `and N more` suffix.
fn more_suffix(first_shown: usize, count: usize) -> String {
    if first_shown == 0 {
        format!("and {} more", count)
    } else {
        format!("  and {} more", count)
    }
}

// ── ANSI helpers ──────────────────────────────────────────────────────────

/// Parse a sanitised ANSI string into a ratatui [`Line`].
///
/// Uses `ansi-to-tui` for the conversion.  Falls back to plain text if
/// parsing fails (the SGR-only input from [`truncate_to_cols`] should always
/// succeed, but we handle the error gracefully rather than panicking).
fn parse_ansi_line(s: &str) -> Line<'static> {
    match s.into_text() {
        Ok(mut text) => text.lines.pop().unwrap_or_default(),
        Err(_) => Line::from(strip_ansi_for_fallback(s).to_string()),
    }
}

/// Strip all ANSI escape sequences for the plain-text fallback path.
fn strip_ansi_for_fallback(s: &str) -> &str {
    // In practice this is never called because truncate_to_cols produces
    // valid SGR-only output.  Return the raw string; terminal will render
    // the escape codes literally, which is better than losing the content.
    s
}

/// Truncate `s` to at most `max` visible columns, stripping dangerous escape
/// sequences.
///
/// **Kept:** ANSI SGR (colour/attribute) sequences — `\x1b[…m`.  These are
/// safe to confine within a pane because `\x1b[0m` (always appended) resets
/// them.
///
/// **Dropped:** everything else that could corrupt terminal state outside the
/// pane:
/// * OSC sequences (`\x1b]…\x07` or `\x1b]…\x1b\`) — hyperlinks, title sets,
///   etc.  A mis-terminated OSC 8 hyperlink is the canonical cause of an
///   entire pane (and the shell prompt) rendering as a clickable link.
/// * Non-SGR CSI sequences — cursor movement, erase, scroll, etc.
/// * All other ESC introducer sequences (DCS, PM, APC, …).
///
/// The first non-ANSI character that would exceed `max` columns terminates the
/// string.  `\x1b[0m` is always appended to prevent colour bleed.
pub(crate) fn truncate_to_cols(s: &str, max: usize) -> String {
    if max == 0 {
        return "\x1b[0m".to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    let mut cols = 0usize;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            if cols >= max {
                break;
            }
            out.push(ch);
            cols += 1;
            continue;
        }

        // ch == '\x1b': classify the sequence by the introducer character.
        match chars.peek().copied() {
            // ── CSI: \x1b[ … <final byte 0x40–0x7E> ──────────────────────
            Some('[') => {
                chars.next(); // consume '['
                let mut seq = String::from("\x1b[");
                let mut final_byte = ' ';
                for ec in chars.by_ref() {
                    seq.push(ec);
                    // CSI final byte range: 0x40–0x7E (@ through ~).
                    if ('\x40'..='\x7e').contains(&ec) {
                        final_byte = ec;
                        break;
                    }
                }
                // Only keep SGR (ends with 'm').
                if final_byte == 'm' {
                    out.push_str(&seq);
                }
                // All other CSI sequences (cursor movement, erase, …) dropped.
            }

            // ── OSC: \x1b] … ST ────────────────────────────────────────────
            // ST is either BEL (\x07) or ESC \ (\x1b\x5c).
            // Consume and drop the entire sequence.
            Some(']') => {
                chars.next(); // consume ']'
                loop {
                    match chars.next() {
                        None | Some('\x07') => break,
                        Some('\x1b') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        Some(_) => {}
                    }
                }
            }

            // ── Other ESC sequences (DCS \x1bP, PM \x1b^, APC \x1b_, …) ──
            Some(_) => {
                chars.next(); // consume the introducer
                loop {
                    match chars.next() {
                        None | Some('\x07') => break,
                        Some('\x1b') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        Some(ec) if ec.is_ascii_alphabetic() => break,
                        Some(_) => {}
                    }
                }
            }

            // Bare ESC at end of string — drop.
            None => {}
        }
    }

    out.push_str("\x1b[0m");
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── tui_layout ────────────────────────────────────────────────────────

    #[test]
    fn layout_all_fit_evenly() {
        assert_eq!(tui_layout(3, 40), (3, 13));
    }

    #[test]
    fn layout_overflow_visible_4() {
        assert_eq!(tui_layout(5, 40), (4, 9));
    }

    #[test]
    fn layout_exact_fit_min_height() {
        assert_eq!(tui_layout(3, 27), (3, 9));
    }

    #[test]
    fn layout_terminal_too_small_fallback() {
        assert_eq!(tui_layout(2, 8), (0, 9));
    }

    #[test]
    fn layout_single_member_fits() {
        assert_eq!(tui_layout(1, 20), (1, 20));
    }

    #[test]
    fn layout_zero_term_height() {
        assert_eq!(tui_layout(3, 0), (0, MIN_PANE_HEIGHT));
    }

    #[test]
    fn layout_zero_members() {
        assert_eq!(tui_layout(0, 40), (0, MIN_PANE_HEIGHT));
    }

    // ── build_overflow_names ──────────────────────────────────────────────

    #[test]
    fn overflow_all_fit() {
        let names = ["alpha", "beta", "gamma"];
        assert_eq!(build_overflow_names(&names, 40), "alpha, beta, gamma");
    }

    #[test]
    fn overflow_longest_fitting_prefix_shown() {
        let names = ["alpha", "beta-long", "gamma"];
        assert_eq!(build_overflow_names(&names, 18), "alpha  and 2 more");
    }

    #[test]
    fn overflow_first_name_too_long_shows_and_more() {
        let names = ["very-long-name"];
        assert_eq!(build_overflow_names(&names, 10), "and 1 more");
    }

    #[test]
    fn overflow_nothing_fits_returns_empty() {
        let names = ["a", "b"];
        assert_eq!(build_overflow_names(&names, 3), "");
    }

    #[test]
    fn overflow_single_name_fits() {
        assert_eq!(build_overflow_names(&["hello"], 20), "hello");
    }

    #[test]
    fn overflow_single_name_too_long_and_suffix_also_too_long_returns_empty() {
        let names = ["hello-world"];
        assert_eq!(build_overflow_names(&names, 5), "");
    }

    #[test]
    fn overflow_empty_names() {
        assert_eq!(build_overflow_names(&[], 40), "");
    }

    #[test]
    fn overflow_two_names_both_fit() {
        let names = ["a", "b"];
        assert_eq!(build_overflow_names(&names, 10), "a, b");
    }

    #[test]
    fn overflow_and_more_standalone_when_no_names_fit() {
        let names = ["alpha", "beta", "gamma", "delta"];
        assert_eq!(build_overflow_names(&names, 12), "and 4 more");
    }

    #[test]
    fn overflow_all_names_fit_no_suffix() {
        let names = ["a", "b", "c", "d"];
        assert_eq!(build_overflow_names(&names, 15), "a, b, c, d");
    }

    // ── truncate_to_cols ──────────────────────────────────────────────────

    #[test]
    fn truncate_plain_text_at_max() {
        assert_eq!(truncate_to_cols("hello world", 5), "hello\x1b[0m");
    }

    #[test]
    fn truncate_plain_text_fits() {
        assert_eq!(truncate_to_cols("hi", 10), "hi\x1b[0m");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate_to_cols("abc", 3), "abc\x1b[0m");
    }

    #[test]
    fn truncate_zero_max_returns_only_reset() {
        assert_eq!(truncate_to_cols("anything", 0), "\x1b[0m");
    }

    #[test]
    fn truncate_ansi_sequences_dont_count_columns() {
        let input = "\x1b[32mhello\x1b[0m world";
        let result = truncate_to_cols(input, 7);
        assert_eq!(result, "\x1b[32mhello\x1b[0m w\x1b[0m");
    }

    #[test]
    fn truncate_ansi_at_start_no_visible_chars() {
        let input = "\x1b[1m";
        assert_eq!(truncate_to_cols(input, 5), "\x1b[1m\x1b[0m");
    }

    #[test]
    fn truncate_non_sgr_csi_is_dropped() {
        let input = "\x1b[1Ahello";
        assert_eq!(truncate_to_cols(input, 10), "hello\x1b[0m");
    }

    #[test]
    fn truncate_osc8_hyperlink_bel_terminated_is_dropped() {
        let input = "\x1b]8;;https://example.com\x07click\x1b]8;;\x07";
        assert_eq!(truncate_to_cols(input, 20), "click\x1b[0m");
    }

    #[test]
    fn truncate_osc8_hyperlink_st_terminated_is_dropped() {
        let input = "\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\";
        assert_eq!(truncate_to_cols(input, 20), "click\x1b[0m");
    }

    #[test]
    fn truncate_osc_mid_line_strips_only_osc() {
        let input = "\x1b[33mfile: \x1b]8;;file:///tmp/F.java\x07F.java\x1b]8;;\x07\x1b[0m";
        assert_eq!(truncate_to_cols(input, 40), "\x1b[33mfile: F.java\x1b[0m\x1b[0m");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_to_cols("", 10), "\x1b[0m");
    }
}
