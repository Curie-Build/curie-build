//! Terminal-UI split-screen renderer for parallel workspace builds.
//!
//! When stdout is a TTY and the terminal is tall enough, [`TuiRenderer`]
//! divides the screen into per-member panes (1 title row + N content rows
//! each) and streams each member's output into its own pane in real-time.
//!
//! # Layout
//!
//! ```text
//! ┌─ member-a ──────────────────────────────────────────────────────────────┐
//! │ … last N lines of output …                                              │
//! ├─ member-b ──────────────────────────────────────────────────────────────┤
//! │ … last N lines of output …                                              │
//! └─────────────────────────────────────────────────────────────────────────┘
//!   · Background: member-c, member-d  (1/4)
//! ```
//!
//! When more members exist than can fit on screen, the extra members are
//! listed on a single overflow line at the bottom.
//!
//! # Fallback
//!
//! If the terminal is too small for even one pane (`visible_count == 0`),
//! the caller falls back to the prefix-mux path (no TUI is created).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, SyncSender};

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
    SlotDone { slot_idx: usize },
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

    /// Mark this slot as finished in the render thread.
    ///
    /// Called by `run_jobs` immediately after the member's job completes.
    /// No buffering exists in the TUI path, so there is nothing to drain.
    fn flush(&self) {
        let _ = self.sender.send(TuiMsg::SlotDone { slot_idx: self.slot_idx });
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
    /// * `visible_count` — number of panes to show on screen.
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

        let term_w = crate::term::width().unwrap_or(80) as usize;

        let thread = std::thread::spawn(move || {
            render_loop(receiver, names, n, visible_count, pane_height, term_w);
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
    done: bool,
    /// Ring buffer of received lines, newest last.
    ring: VecDeque<String>,
}

fn render_loop(
    rx: mpsc::Receiver<TuiMsg>,
    names: Vec<String>,
    n: usize,
    visible: usize,
    pane_h: usize,
    term_w: usize,
) {
    let mut out = std::io::stdout();

    let mut slots: Vec<SlotData> = names
        .into_iter()
        .map(|name| SlotData { name, done: false, ring: VecDeque::new() })
        .collect();

    // ── Initial draw ──────────────────────────────────────────────────────
    clear_screen(&mut out);
    for pane_idx in 0..visible {
        draw_title(&mut out, pane_idx, pane_h, &slots[pane_idx].name, false, term_w);
        // Blank out all content rows.
        let content_rows = pane_h - 1;
        for row_offset in 0..content_rows {
            let screen_row = pane_idx * pane_h + 2 + row_offset; // 1-indexed; +1 title, +1 for 1-based
            move_to(&mut out, screen_row, 1);
            erase_line(&mut out);
        }
    }
    if n > visible {
        draw_overflow_line(&mut out, visible, pane_h, &slots, 0, n, term_w);
    }
    let _ = out.flush();

    // ── Message loop ──────────────────────────────────────────────────────
    for msg in rx {
        match msg {
            TuiMsg::Line { slot_idx, line } => {
                slots[slot_idx].ring.push_back(line);
                if slot_idx < visible {
                    redraw_content(
                        &mut out,
                        slot_idx,
                        pane_h,
                        &slots[slot_idx].ring,
                        term_w,
                    );
                } else if n > visible {
                    let done_count = slots.iter().filter(|s| s.done).count();
                    draw_overflow_line(&mut out, visible, pane_h, &slots, done_count, n, term_w);
                }
                let _ = out.flush();
            }
            TuiMsg::SlotDone { slot_idx } => {
                slots[slot_idx].done = true;
                if slot_idx < visible {
                    draw_title(
                        &mut out,
                        slot_idx,
                        pane_h,
                        &slots[slot_idx].name,
                        true,
                        term_w,
                    );
                }
                if n > visible {
                    let done_count = slots.iter().filter(|s| s.done).count();
                    draw_overflow_line(&mut out, visible, pane_h, &slots, done_count, n, term_w);
                }
                let _ = out.flush();
            }
            TuiMsg::Shutdown => break,
        }
    }

    // ── Cleanup: park cursor below all drawn content ───────────────────────
    let total_rows = visible * pane_h + if n > visible { 1 } else { 0 };
    move_to(&mut out, total_rows + 1, 1);
    let _ = out.flush();
}

// ── ANSI primitives ───────────────────────────────────────────────────────

/// Move the cursor to the given 1-based (row, col).
fn move_to(out: &mut impl Write, row: usize, col: usize) {
    let _ = write!(out, "\x1b[{};{}H", row, col);
}

/// Erase the entire screen and move cursor to top-left.
fn clear_screen(out: &mut impl Write) {
    let _ = write!(out, "\x1b[2J\x1b[H");
}

/// Erase from cursor to end of line (without moving cursor).
fn erase_line(out: &mut impl Write) {
    let _ = write!(out, "\x1b[K");
}

// ── Pane drawing ──────────────────────────────────────────────────────────

/// Draw (or redraw) the title bar for pane `pane_idx`.
///
/// Title bar is reverse-video, padded to `term_w`, with a ` ✓` suffix when
/// `done` is true.
fn draw_title(
    out: &mut impl Write,
    pane_idx: usize,
    pane_h: usize,
    name: &str,
    done: bool,
    term_w: usize,
) {
    // Row 1 of pane is the title row (1-indexed screen rows).
    let screen_row = pane_idx * pane_h + 1;
    move_to(out, screen_row, 1);
    let suffix = if done { " ✓" } else { "" };
    let label = format!(" {}{}", name, suffix);
    // Pad to fill the full width so the reverse bar stretches edge-to-edge.
    let pad = term_w.saturating_sub(label.len());
    let _ = write!(out, "\x1b[7m{}{}\x1b[0m", label, " ".repeat(pad));
}

/// Redraw the content area of pane `pane_idx` from the slot's ring buffer.
///
/// Content rows are `pane_h - 1` lines tall.  Lines are shown newest-at-bottom:
/// for row index `i` (0 = top of content area), display `ring[i + ring_len - rows]`
/// when `i + ring_len >= rows`, otherwise leave the row blank.
fn redraw_content(
    out: &mut impl Write,
    pane_idx: usize,
    pane_h: usize,
    ring: &VecDeque<String>,
    term_w: usize,
) {
    let content_rows = pane_h - 1;
    let ring_len = ring.len();
    // Title is at screen row `pane_idx * pane_h + 1`; content starts one below.
    let first_content_row = pane_idx * pane_h + 2;

    for i in 0..content_rows {
        let screen_row = first_content_row + i;
        move_to(out, screen_row, 1);
        // Reset + erase to right — prevents leftover chars when lines shrink.
        let _ = write!(out, "\x1b[0m\x1b[K");

        if i + ring_len >= content_rows {
            let ring_idx = i + ring_len - content_rows;
            if ring_idx < ring_len {
                let truncated = truncate_to_cols(&ring[ring_idx], term_w);
                let _ = write!(out, "{}\x1b[0m\x1b[K", truncated);
            }
        }
        // else: row stays blank
    }
}

/// Draw (or redraw) the overflow summary line below all panes.
///
/// Format: `  · Background: name1, name2  (done/total)`
fn draw_overflow_line(
    out: &mut impl Write,
    visible: usize,
    pane_h: usize,
    slots: &[SlotData],
    done_count: usize,
    total: usize,
    term_w: usize,
) {
    let overflow_row = visible * pane_h + 1;
    move_to(out, overflow_row, 1);
    erase_line(out);

    let bg_names: Vec<&str> = slots[visible..].iter().map(|s| s.name.as_str()).collect();
    let names_str = bg_names.join(", ");
    let text = format!("  \u{00b7} Background: {}  ({}/{})", names_str, done_count, total);
    let truncated = truncate_to_cols(&text, term_w);
    let _ = write!(out, "{}", truncated);
}

// ── Text truncation ───────────────────────────────────────────────────────

/// Truncate `s` to at most `max` visible columns.
///
/// ANSI CSI escape sequences (e.g. `\x1b[32m`) are passed through without
/// consuming column budget.  The first non-ANSI character that would exceed
/// `max` columns terminates the string.  A final `\x1b[0m` is always appended
/// to prevent colour bleed from the last coloured segment into the next line.
pub(crate) fn truncate_to_cols(s: &str, max: usize) -> String {
    if max == 0 {
        return "\x1b[0m".to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    let mut cols = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Pass the escape character through.
            out.push(ch);
            // Collect subsequent characters until a terminating letter.
            // Most ANSI sequences end with a single ASCII letter (e.g. 'm').
            for ec in chars.by_ref() {
                out.push(ec);
                if ec.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if cols >= max {
                break;
            }
            out.push(ch);
            cols += 1;
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
        // 3 panes × MIN=9 = 27 ≤ 40; height = 40/3 = 13.
        assert_eq!(tui_layout(3, 40), (3, 13));
    }

    #[test]
    fn layout_overflow_visible_4() {
        // 5×9=45 > 40; visible = (40-1)/9 = 4, pane_h = 9.
        assert_eq!(tui_layout(5, 40), (4, 9));
    }

    #[test]
    fn layout_exact_fit_min_height() {
        // 3×9 = 27 ≤ 27; all visible, height = 27/3 = 9.
        assert_eq!(tui_layout(3, 27), (3, 9));
    }

    #[test]
    fn layout_terminal_too_small_fallback() {
        // 2×9=18 > 8; visible=(8-1)/9=0 → caller must fall back.
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

    // ── truncate_to_cols ──────────────────────────────────────────────────

    #[test]
    fn truncate_plain_text_at_max() {
        // "hello world" → first 5 chars + reset.
        assert_eq!(truncate_to_cols("hello world", 5), "hello\x1b[0m");
    }

    #[test]
    fn truncate_plain_text_fits() {
        // Shorter than max → full string + reset.
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
        // "\x1b[32m" is 5 bytes but 0 visible columns.
        // "hello" = 5 columns; " w" adds 2 more → 7 total.
        let input = "\x1b[32mhello\x1b[0m world";
        let result = truncate_to_cols(input, 7);
        assert_eq!(result, "\x1b[32mhello\x1b[0m w\x1b[0m");
    }

    #[test]
    fn truncate_ansi_at_start_no_visible_chars() {
        // Only escape sequence, no visible content.
        let input = "\x1b[1m";
        assert_eq!(truncate_to_cols(input, 5), "\x1b[1m\x1b[0m");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_to_cols("", 10), "\x1b[0m");
    }
}
