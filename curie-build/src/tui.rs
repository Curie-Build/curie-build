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
use std::sync::mpsc::{self, SyncSender};

use ansi_to_tui::IntoText;
use crossterm::{cursor, execute};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
    Frame,
};

/// Minimum pane height (1 title row + 8 content rows).
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
    let _ = io::stdout().flush();

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
    for msg in rx {
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

                if let Some(pane_idx) =
                    state.pane_to_slot.iter().position(|&s| s == slot_idx)
                {
                    if success && !state.background_queue.is_empty() {
                        // Hand the pane to the next waiting background job.
                        let next = state.background_queue.pop_front().unwrap();
                        state.pane_to_slot[pane_idx] = next;
                    }
                    // else: keep pane, title will show outcome on next draw.
                } else {
                    // Slot finished in the background — remove from queue so
                    // it no longer appears in the overflow line.
                    state.background_queue.retain(|&s| s != slot_idx);
                }

                let _ = terminal.draw(|f| render_frame(f, &state));
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
    let overflow_rows = if n > visible { 1 } else { 0 };
    let total_rows = (visible * pane_h + overflow_rows) as u16;
    let _ = terminal.set_cursor_position((0, total_rows));
    let _ = io::stdout().flush();
}

// ── Frame renderer (pure, no I/O) ─────────────────────────────────────────

fn render_frame(f: &mut Frame, state: &RenderState) {
    let area = f.area();

    // Build vertical layout: one block per visible pane + optional overflow.
    let has_overflow = state.n > state.visible && !state.background_queue.is_empty();
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

        // Split pane into title row + content area.
        let pane_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(pane_area);

        render_title(f, pane_chunks[0], &slot.name, slot.done);
        render_content(f, pane_chunks[1], &slot.ring);
    }

    // Draw overflow line.
    if has_overflow {
        let overflow_area = chunks[state.pane_to_slot.len()];
        render_overflow(f, overflow_area, &state.background_queue, &state.slots);
    }
}

// ── Pane widgets ──────────────────────────────────────────────────────────

/// Render the title bar for one pane.
///
/// Format:  `── member-name ──────────────── ✓`
fn render_title(f: &mut Frame, area: Rect, name: &str, done: Option<bool>) {
    let dim  = Style::new().add_modifier(Modifier::DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);

    // "── " prefix (3 cols)
    let mut spans = vec![
        Span::styled("── ", dim),
        Span::styled(name.to_string(), bold),
    ];

    // Status symbol (3 cols) or nothing.
    let (status_str, status_cols) = match done {
        None           => ("",    0usize),
        Some(true)     => (" ✓ ", 3),
        Some(false)    => (" ✗ ", 3),
    };
    if !status_str.is_empty() {
        let status_color = if done == Some(true) { Color::Green } else { Color::Red };
        spans.push(Span::styled(status_str, Style::new().fg(status_color)));
    }

    // Dim dash fill to end of area.
    let used = 3 + name.len() + status_cols; // "── " + name + status
    let fill_cols = (area.width as usize).saturating_sub(used);
    if fill_cols > 0 {
        spans.push(Span::styled("─".repeat(fill_cols), dim));
    }

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
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
/// Format:  `  · Background: name1, name2  and 3 more`
fn render_overflow(
    f: &mut Frame,
    area: Rect,
    background_queue: &VecDeque<usize>,
    slots: &[SlotData],
) {
    if background_queue.is_empty() {
        return;
    }

    let prefix = "  \u{00b7} Background: "; // 16 visible cols
    let prefix_cols = 16usize;
    let budget = (area.width as usize).saturating_sub(prefix_cols);

    let names: Vec<&str> = background_queue
        .iter()
        .map(|&i| slots[i].name.as_str())
        .collect();

    let body = build_overflow_names(&names, budget);
    let text = format!("{}{}", prefix, body);
    f.render_widget(Paragraph::new(text), area);
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
