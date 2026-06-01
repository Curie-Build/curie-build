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
    /// `None` = still running, `Some(true)` = success, `Some(false)` = failed.
    done: Option<bool>,
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
        .map(|name| SlotData { name, done: None, ring: VecDeque::new() })
        .collect();

    // pane_to_slot[pane_idx] = slot_idx currently displayed in that pane.
    // Initially pane 0 → slot 0, pane 1 → slot 1, etc.
    let mut pane_to_slot: Vec<usize> = (0..visible).collect();

    // Background slots not yet assigned to any pane.
    let mut background_queue: VecDeque<usize> = (visible..n).collect();

    // ── Initial draw ──────────────────────────────────────────────────────
    // Hide cursor and disable line-wrap to prevent pane bleed.
    let _ = write!(out, "\x1b[?25l\x1b[?7l");
    clear_screen(&mut out);
    for pane_idx in 0..visible {
        draw_title(&mut out, pane_idx, pane_h, &slots[pane_idx].name, None, term_w);
        blank_content(&mut out, pane_idx, pane_h);
    }
    if !background_queue.is_empty() {
        draw_overflow_line(&mut out, &background_queue, &slots, visible, pane_h, term_w);
    }
    let _ = out.flush();

    // ── Message loop ──────────────────────────────────────────────────────
    for msg in rx {
        match msg {
            TuiMsg::Line { slot_idx, line } => {
                slots[slot_idx].ring.push_back(line);
                // Only redraw if this slot is currently in a visible pane.
                if let Some(pane_idx) =
                    pane_to_slot.iter().position(|&s| s == slot_idx)
                {
                    redraw_content(
                        &mut out,
                        pane_idx,
                        pane_h,
                        &slots[slot_idx].ring,
                        term_w,
                    );
                }
                // Background slots: lines accumulate in the ring and will be
                // shown when the slot is promoted to a pane.
                let _ = out.flush();
            }

            TuiMsg::SlotDone { slot_idx, success } => {
                slots[slot_idx].done = Some(success);

                if let Some(pane_idx) =
                    pane_to_slot.iter().position(|&s| s == slot_idx)
                {
                    // This slot was in a visible pane.
                    if success && !background_queue.is_empty() {
                        // Hand the pane to the next waiting job immediately.
                        let next = background_queue.pop_front().unwrap();
                        pane_to_slot[pane_idx] = next;
                        draw_title(
                            &mut out, pane_idx, pane_h,
                            &slots[next].name, None, term_w,
                        );
                        // Show whatever the background job has accumulated so far.
                        // We need to borrow `slots[next].ring` but we also need
                        // `slots` mutably above — clone the ring for the redraw.
                        let ring_snapshot: VecDeque<String> =
                            slots[next].ring.iter().cloned().collect();
                        redraw_content(
                            &mut out, pane_idx, pane_h, &ring_snapshot, term_w,
                        );
                    } else {
                        // Keep the pane, update the title to show the outcome.
                        draw_title(
                            &mut out, pane_idx, pane_h,
                            &slots[slot_idx].name, Some(success), term_w,
                        );
                    }
                } else {
                    // This slot was in the background queue — it finished before
                    // being promoted.  Remove it from the queue so it no longer
                    // appears in the overflow line.
                    background_queue.retain(|&s| s != slot_idx);
                }

                // Redraw overflow line (queue may have shrunk).
                if n > visible {
                    draw_overflow_line(
                        &mut out, &background_queue, &slots, visible, pane_h, term_w,
                    );
                }
                let _ = out.flush();
            }

            TuiMsg::Shutdown => break,
        }
    }

    // ── Cleanup: park cursor below all drawn content ───────────────────────
    let overflow_rows = if n > visible { 1 } else { 0 };
    let total_rows = visible * pane_h + overflow_rows;
    // Restore: full scroll region, line-wrap, cursor visibility.
    let _ = write!(out, "\x1b[r\x1b[?7h\x1b[?25h");
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
fn erase_to_eol(out: &mut impl Write) {
    let _ = write!(out, "\x1b[K");
}

/// Set the terminal scroll region to [top, bottom] (both 1-based, inclusive).
/// While this region is active, scroll operations (including auto-scroll when
/// writing past the last line) are confined to those rows only — content in
/// other panes is never pushed up or down.
///
/// Call `reset_scroll_region` when done to restore the full-screen region.
fn set_scroll_region(out: &mut impl Write, top: usize, bottom: usize) {
    let _ = write!(out, "\x1b[{};{}r", top, bottom);
}

/// Restore the terminal to a full-screen scroll region.
fn reset_scroll_region(out: &mut impl Write) {
    let _ = write!(out, "\x1b[r");
}



/// Draw (or redraw) the title bar for pane `pane_idx`.
///
/// Format (no reverse-video background):
/// ```text
/// ── member-name ──────────────────────────────────── ✓
/// ```
/// * `done = None`        → running (plain dashes)
/// * `done = Some(true)`  → green ✓ before the right-hand dashes
/// * `done = Some(false)` → red ✗ before the right-hand dashes
fn draw_title(
    out: &mut impl Write,
    pane_idx: usize,
    pane_h: usize,
    name: &str,
    done: Option<bool>,
    term_w: usize,
) {
    let screen_row = pane_idx * pane_h + 1;
    move_to(out, screen_row, 1);

    // Left prefix: "── " (3 visible columns)
    // Name: bold
    // Status: " ✓ " or " ✗ " or nothing (each 3 visible cols: space + symbol + space)
    // Fill: dim "─" characters to pad to term_w

    let left_prefix = "\x1b[2m\u{2500}\u{2500} \x1b[0m"; // dim "── "
    let left_cols: usize = 3;

    let name_bold = format!("\x1b[1m{}\x1b[0m", name);
    let name_cols = name.len(); // names are ASCII

    let (status_str, status_cols) = match done {
        None           => ("".to_string(),                          0),
        Some(true)     => (" \x1b[32m\u{2713}\x1b[0m ".to_string(), 3), // " ✓ "
        Some(false)    => (" \x1b[31m\u{2717}\x1b[0m ".to_string(), 3), // " ✗ "
    };

    let used = left_cols + name_cols + status_cols;
    let fill_cols = term_w.saturating_sub(used);
    let fill = format!("\x1b[2m{}\x1b[0m", "\u{2500}".repeat(fill_cols));

    let _ = write!(out, "{}{}{}{}\x1b[K", left_prefix, name_bold, status_str, fill);
}

/// Clear all content rows of a pane (used when first drawn or when reused).
fn blank_content(out: &mut impl Write, pane_idx: usize, pane_h: usize) {
    let content_rows = pane_h - 1;
    let first_content_row = pane_idx * pane_h + 2;
    let last_content_row = first_content_row + content_rows - 1;
    set_scroll_region(out, first_content_row, last_content_row);
    for i in 0..content_rows {
        move_to(out, first_content_row + i, 1);
        erase_to_eol(out);
    }
    reset_scroll_region(out);
}

/// Redraw the content area of pane `pane_idx` from `ring`.
///
/// Content rows are `pane_h - 1` lines tall.  Lines are shown newest-at-bottom:
/// for row index `i` (0 = top of content area), display `ring[i + ring_len - rows]`
/// when `i + ring_len >= rows`, otherwise leave the row blank.
///
/// A per-pane DECSTBM scroll region is set for the duration of the write so
/// that any accidental terminal auto-scroll (e.g. writing at the very last
/// content row) only scrolls within this pane and never pushes adjacent pane
/// titles off screen.
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
    let last_content_row = first_content_row + content_rows - 1;

    // Restrict scroll region to this pane's content rows only.
    set_scroll_region(out, first_content_row, last_content_row);

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

    // Restore full-screen scroll region so subsequent moves aren't confined.
    reset_scroll_region(out);
}

/// Draw (or redraw) the overflow summary line below all panes.
///
/// Format: `  · Background: name1, name2  and 3 more`
///
/// Names are listed until the line would overflow, at which point the
/// remaining count is shown as `  and N more`.
fn draw_overflow_line(
    out: &mut impl Write,
    background_queue: &VecDeque<usize>,
    slots: &[SlotData],
    visible: usize,
    pane_h: usize,
    term_w: usize,
) {
    let overflow_row = visible * pane_h + 1;
    move_to(out, overflow_row, 1);
    erase_to_eol(out);

    if background_queue.is_empty() {
        return;
    }

    let names: Vec<&str> = background_queue
        .iter()
        .map(|&i| slots[i].name.as_str())
        .collect();

    let prefix = "  \u{00b7} Background: "; // "  · Background: " — 16 visible cols
    let prefix_cols = 16;
    let budget = term_w.saturating_sub(prefix_cols);
    let body = build_overflow_names(&names, budget);

    let _ = write!(out, "{}{}", prefix, body);
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
    // Iterating from the top (most names) means we stop at the first match.
    let best_k = match (0..=total).rev().find(|&k| text_len(k) <= budget) {
        Some(k) => k,
        None => return String::new(), // nothing fits, not even "and N more"
    };

    // Build the result.
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
///
/// When `first_shown == 0` no preceding text exists, so no leading separator
/// is added.  Otherwise two spaces separate it from the last visible name.
fn more_suffix(first_shown: usize, count: usize) -> String {
    if first_shown == 0 {
        format!("and {} more", count)
    } else {
        format!("  and {} more", count)
    }
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

    // ── build_overflow_names ──────────────────────────────────────────────

    #[test]
    fn overflow_all_fit() {
        let names = ["alpha", "beta", "gamma"];
        assert_eq!(build_overflow_names(&names, 40), "alpha, beta, gamma");
    }

    #[test]
    fn overflow_longest_fitting_prefix_shown() {
        // "alpha" + "  and 2 more"(12) = 17 ≤ 18.
        // "alpha, beta-long"(16) + "  and 1 more"(12) = 28 > 18.
        // "alpha, beta-long, gamma"(23) > 18.
        // → best is k=1: "alpha  and 2 more".
        let names = ["alpha", "beta-long", "gamma"];
        assert_eq!(build_overflow_names(&names, 18), "alpha  and 2 more");
    }

    #[test]
    fn overflow_first_name_too_long_shows_and_more() {
        // "very-long-name"(14) > 10; "and 1 more"(10) == 10 → fits.
        let names = ["very-long-name"];
        assert_eq!(build_overflow_names(&names, 10), "and 1 more");
    }

    #[test]
    fn overflow_nothing_fits_returns_empty() {
        // Even "and 2 more"(10) > 3.
        let names = ["a", "b"];
        assert_eq!(build_overflow_names(&names, 3), "");
    }

    #[test]
    fn overflow_single_name_fits() {
        assert_eq!(build_overflow_names(&["hello"], 20), "hello");
    }

    #[test]
    fn overflow_single_name_too_long_and_suffix_also_too_long_returns_empty() {
        // "hello-world"(11) > 5; "and 1 more"(10) > 5.
        let names = ["hello-world"];
        assert_eq!(build_overflow_names(&names, 5), "");
    }

    #[test]
    fn overflow_empty_names() {
        assert_eq!(build_overflow_names(&[], 40), "");
    }

    #[test]
    fn overflow_two_names_both_fit() {
        // "a, b" = 4 ≤ 10.
        let names = ["a", "b"];
        assert_eq!(build_overflow_names(&names, 10), "a, b");
    }

    #[test]
    fn overflow_and_more_standalone_when_no_names_fit() {
        // "alpha"(5) + "  and 3 more"(12) = 17 > 12.
        // "and 4 more"(10) ≤ 12 and k=4 text_len = 14 > 12.  Best k=0.
        let names = ["alpha", "beta", "gamma", "delta"];
        assert_eq!(build_overflow_names(&names, 12), "and 4 more");
    }

    #[test]
    fn overflow_all_names_fit_no_suffix() {
        // "a, b, c, d"(10) ≤ 15 → show all, no suffix.
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
    fn truncate_empty_string() {
        assert_eq!(truncate_to_cols("", 10), "\x1b[0m");
    }
}
