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
//! Panes are assigned lazily: a member gets a pane only once its build job has
//! actually started running (signalled via [`crate::parallel::LineSink::start`]).
//! Members whose jobs are still pending — or were never dispatched because an
//! earlier failure cancelled the rest of the build — never occupy a pane; they
//! appear compactly in the "Pending" overflow line instead of as empty boxes.
//!
//! When more members are running than can fit on screen, the extra members are
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
use crossterm::{cursor, execute, terminal};
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
    /// Worker thread began running this job.  The render thread assigns it a
    /// free pane (or queues it as a running background job).  Only started jobs
    /// ever occupy a pane, so pending/never-dispatched jobs don't show as empty
    /// boxes — they appear in the "Pending" overflow line instead.
    SlotStarted { slot_idx: usize },
    /// Job cancelled by an earlier build failure — it will never run.  Shown in
    /// the "Skipped (<reason>)" overflow group instead of "Pending".
    SlotSkipped { slot_idx: usize, reason: String },
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
    /// Signal the render thread that this job has begun executing so it can be
    /// assigned a visible pane.  Jobs that never start (e.g. cancelled after an
    /// earlier failure) never send this and so never show an empty pane.
    fn start(&self) {
        let _ = self.sender.send(TuiMsg::SlotStarted { slot_idx: self.slot_idx });
    }

    /// Signal that this job was cancelled by an earlier failure and will never
    /// run, so it is shown as "Skipped (<reason>)" rather than "Pending".
    fn skip(&self, reason: &str) {
        let _ = self.sender.send(TuiMsg::SlotSkipped {
            slot_idx: self.slot_idx,
            reason: reason.to_string(),
        });
    }

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
    /// * `visible_count` — maximum number of panes shown on screen at once;
    ///   open panes share the full terminal height between them.
    ///
    /// Returns `(renderer, slots)`.  The caller assigns each `Arc<TuiSlot>`
    /// to the matching worker thread via [`crate::parallel::set_thread_sink`].
    pub(crate) fn new(
        names: Vec<String>,
        log_files: Vec<std::fs::File>,
        visible_count: usize,
        done_label: String,
    ) -> (Self, Vec<Arc<TuiSlot>>) {
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
            render_loop(receiver, names, visible_count, done_label);
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
    /// Cancelled by an earlier failure — never ran.  Shown as "Skipped".
    skipped: bool,
    /// Ring buffer of received lines, newest last.
    ring: VecDeque<String>,
}

/// All mutable state touched exclusively by the render thread.
struct RenderState {
    slots: Vec<SlotData>,
    /// `pane_to_slot[pane_idx]` = slot_idx currently shown in that pane.
    pane_to_slot: Vec<usize>,
    /// Started slots not currently shown in a visible pane (running in the
    /// background; eligible for promotion when a pane frees up).
    background_queue: VecDeque<usize>,
    visible: usize,
    /// Short reason the build was aborted, shown after the "Skipped" label.
    /// Set from the first [`TuiMsg::SlotSkipped`].
    skip_reason: Option<String>,
    /// Label shown for successfully completed jobs (e.g. "Done", "Formatted", "Cleaned").
    done_label: String,
}

fn render_loop(
    rx: mpsc::Receiver<TuiMsg>,
    names: Vec<String>,
    visible: usize,
    done_label: String,
) {
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Enter the alternate screen so the terminal's primary-screen content is
    // preserved and automatically restored when we leave.
    let _ = execute!(io::stdout(), terminal::EnterAlternateScreen);
    // Hide cursor and disable line-wrap for the lifetime of the TUI.
    // \x1b[?7l (DECAWM off) has no crossterm equivalent.
    let _ = execute!(io::stdout(), cursor::Hide);
    let _ = write!(io::stdout(), "\x1b[?7l");
    let _ = terminal.clear();

    // Panes start empty and are filled lazily as jobs report they have begun
    // running (TuiMsg::SlotStarted).  This keeps a pane from ever showing a job
    // that is still pending — or one that was never dispatched because an
    // earlier failure cancelled the rest of the build.
    let mut state = RenderState {
        slots: names
            .into_iter()
            .map(|name| SlotData { name, done: None, skipped: false, ring: VecDeque::new() })
            .collect(),
        pane_to_slot: Vec::new(),
        background_queue: VecDeque::new(),
        visible,
        skip_reason: None,
        done_label,
    };

    // Initial draw.
    let _ = terminal.draw(|f| render_frame(f, &state));

    // ── Message loop ──────────────────────────────────────────────────────
    // `pending` holds messages that were stashed during a hold window or
    // drained by drain_available and must be re-processed next iteration.
    //
    // A VecDeque (not Option) is used so that drain_available can queue a
    // non-Line message without dropping one that drain_hold already stashed.
    // That scenario is common in fast incremental builds: multiple SlotDone
    // messages arrive in rapid succession, the first is stashed by drain_hold
    // and a second is received by drain_available — without the queue the
    // second would be silently discarded, causing apply_shutdown to mark the
    // corresponding slot as Skipped.
    let mut pending: std::collections::VecDeque<TuiMsg> = std::collections::VecDeque::new();
    let mut dbg = DbgLog::open();
    loop {
        let msg = match pending.pop_front().or_else(|| rx.recv().ok()) {
            Some(m) => m,
            None    => break,
        };
        if dbg.is_on() {
            dbg.log(&format!("RECV pending_remaining={} msg={}", pending.len(), describe_msg(&msg)));
        }
        match msg {
            TuiMsg::SlotStarted { slot_idx } => {
                note_started(&mut state, slot_idx);
                let _ = terminal.draw(|f| render_frame(f, &state));
            }

            TuiMsg::SlotSkipped { slot_idx, reason } => {
                note_skipped(&mut state, slot_idx, reason);
                let _ = terminal.draw(|f| render_frame(f, &state));
            }

            TuiMsg::Line { slot_idx, line } => {
                state.slots[slot_idx].ring.push_back(line);
                // Drain all buffered lines before drawing so we repaint once
                // per batch rather than once per line (fixes 80s→2s regression
                // for jobs that produce tens-of-thousands of output lines).
                drain_available(&rx, &mut state, &mut pending);
                let _ = terminal.draw(|f| render_frame(f, &state));
            }

            TuiMsg::SlotDone { slot_idx, success } => {
                state.slots[slot_idx].done = Some(success);

                // Redraw immediately so the green/red outcome is visible.
                let _ = terminal.draw(|f| render_frame(f, &state));

                if let Some(pane_idx) =
                    state.pane_to_slot.iter().position(|&s| s == slot_idx)
                {
                    if success && !state.background_queue.is_empty() {
                        // Rule 1: hold green 1 s, then replace with next job.
                        if let Some(stashed) =
                            drain_hold(&rx, &mut terminal, &mut state, 1)
                        {
                            if dbg.is_on() {
                                dbg.log(&format!("DRAIN_HOLD(1s) pending={} stashed={}", pending.len(), describe_msg(&stashed)));
                            }
                            match stashed {
                                // Shutdown during the hold still runs the close
                                // rules so a held-green pane isn't left open.
                                TuiMsg::Shutdown => {
                                    log_apply_shutdown(&mut dbg, &state, &pending);
                                    resolve_outstanding_done_messages(&rx, &mut state, &pending);
                                    apply_shutdown(&mut state);
                                    let _ = terminal.draw(|f| render_frame(f, &state));
                                    break;
                                }
                                // Background slot finished during hold — handle
                                // fully now so it is removed from the queue and
                                // won't be popped as the next promoted job.  A
                                // failure takes over a running pane immediately.
                                TuiMsg::SlotDone { slot_idx: s, success: ok }
                                    if state.pane_to_slot.iter().all(|&x| x != s) =>
                                {
                                    state.slots[s].done = Some(ok);
                                    state.background_queue.retain(|&x| x != s);
                                    if !ok {
                                        place_failed_in_pane(&mut state, s);
                                    }
                                    let _ = terminal.draw(|f| render_frame(f, &state));
                                }
                                // Visible pane slot finished — needs its own
                                // hold/close cycle; re-process via pending.
                                other => pending.push_back(other),
                            }
                        }
                        // Pop next still-running slot; skip any that finished
                        // during the hold and were already removed from queue.
                        let next = loop {
                            match state.background_queue.pop_front() {
                                None                                            => break None,
                                Some(s) if state.slots[s].done.is_none()       => break Some(s),
                                Some(_)                                         => continue,
                            }
                        };
                        if let Some(next) = next {
                            state.pane_to_slot[pane_idx] = next;
                        } else {
                            state.pane_to_slot.remove(pane_idx);
                        }
                        // Drain any lines already queued for the promoted slot
                        // so the first draw shows content rather than a blank.
                        drain_available(&rx, &mut state, &mut pending);
                        if dbg.is_on() {
                            dbg.log(&format!("DRAIN_AVAIL pending_now={}", pending.len()));
                        }
                        let _ = terminal.draw(|f| render_frame(f, &state));
                    } else if success {
                        // Rule 2: no replacement — hold 2 s then close.
                        if let Some(stashed) =
                            drain_hold(&rx, &mut terminal, &mut state, 2)
                        {
                            if dbg.is_on() {
                                dbg.log(&format!("DRAIN_HOLD(2s) pending={} stashed={}", pending.len(), describe_msg(&stashed)));
                            }
                            match stashed {
                                // Shutdown during the hold still runs the close
                                // rules so this held-green pane isn't left open.
                                TuiMsg::Shutdown => {
                                    log_apply_shutdown(&mut dbg, &state, &pending);
                                    resolve_outstanding_done_messages(&rx, &mut state, &pending);
                                    apply_shutdown(&mut state);
                                    let _ = terminal.draw(|f| render_frame(f, &state));
                                    break;
                                }
                                TuiMsg::SlotDone { slot_idx: s, success: ok }
                                    if state.pane_to_slot.iter().all(|&x| x != s) =>
                                {
                                    state.slots[s].done = Some(ok);
                                    state.background_queue.retain(|&x| x != s);
                                    if !ok {
                                        place_failed_in_pane(&mut state, s);
                                    }
                                    let _ = terminal.draw(|f| render_frame(f, &state));
                                }
                                other => pending.push_back(other),
                            }
                        }
                        state.pane_to_slot.remove(pane_idx);
                        let _ = terminal.draw(|f| render_frame(f, &state));
                    }
                    // Failure: keep pane showing red ✗ until shutdown.
                } else {
                    // Slot finished in the background — remove from queue so
                    // it no longer appears in the overflow line.  On failure,
                    // surface it in a pane immediately (taking over a running
                    // pane) so its error is visible right away.
                    state.background_queue.retain(|&s| s != slot_idx);
                    if !success {
                        place_failed_in_pane(&mut state, slot_idx);
                    }
                    let _ = terminal.draw(|f| render_frame(f, &state));
                }
            }

            TuiMsg::Shutdown => {
                log_apply_shutdown(&mut dbg, &state, &pending);
                resolve_outstanding_done_messages(&rx, &mut state, &pending);
                apply_shutdown(&mut state);
                let _ = terminal.draw(|f| render_frame(f, &state));
                break;
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    // Restore scroll region / line-wrap / hyperlink state while still on the
    // alternate screen so those escape sequences don't affect the primary screen.
    let _ = write!(io::stdout(), "\x1b[r\x1b[?7h\x1b]8;;\x07");
    // Leave the alternate screen — restores whatever the terminal was showing
    // before the TUI started.  Show the cursor on the primary screen.
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    // Print the last TAIL_LINES of each failed member below the restored content
    // so the errors are visible before the caller's summary lines appear.
    print_failed_tails(&state, TAIL_LINES, &mut io::stdout());
    print_build_summary(&state, &mut io::stdout());
    let _ = io::stdout().flush();
}

// ── Pane assignment ────────────────────────────────────────────────────────

/// Record that `slot_idx` has begun running.
///
/// Fills a free pane if fewer than `visible` panes are open; otherwise enqueues
/// the slot as a running background job (shown in the "Running" overflow group
/// and eligible for promotion when a visible pane finishes successfully).
fn note_started(state: &mut RenderState, slot_idx: usize) {
    if state.pane_to_slot.len() < state.visible {
        state.pane_to_slot.push(slot_idx);
    } else {
        state.background_queue.push_back(slot_idx);
    }
}

/// Record that `slot_idx` was cancelled by an earlier failure, capturing the
/// short `reason` the build was aborted the first time one is seen (all skipped
/// jobs share the same reason).
fn note_skipped(state: &mut RenderState, slot_idx: usize, reason: String) {
    state.slots[slot_idx].skipped = true;
    if state.skip_reason.is_none() {
        state.skip_reason = Some(reason);
    }
}

/// Number of trailing output lines printed per failed job after the build.
const TAIL_LINES: usize = 15;

// ── Shutdown helpers ──────────────────────────────────────────────────────

/// Apply every `SlotDone` message waiting in `pending` or the channel before
/// `apply_shutdown` is called.
///
/// When `Shutdown` interrupts a hold window, `pending` may already contain
/// `SlotDone` messages queued by an earlier `drain_available` call in the
/// same outer `SlotDone` processing block.  Additionally, the channel may
/// hold `SlotDone` messages that were sent by workers before the renderer
/// was dropped but haven't been received yet.  Without this flush,
/// `apply_shutdown` would see those slots as `done == None` and incorrectly
/// mark them as skipped.
fn resolve_outstanding_done_messages(
    rx:      &mpsc::Receiver<TuiMsg>,
    state:   &mut RenderState,
    pending: &std::collections::VecDeque<TuiMsg>,
) {
    // Apply SlotDone messages already held in `pending`.
    for msg in pending {
        if let TuiMsg::SlotDone { slot_idx, success } = msg {
            if state.slots[*slot_idx].done.is_none() {
                state.slots[*slot_idx].done = Some(*success);
            }
        }
    }
    // Drain any remaining messages from the channel.
    loop {
        match rx.try_recv() {
            Ok(TuiMsg::SlotDone { slot_idx, success }) => {
                if state.slots[slot_idx].done.is_none() {
                    state.slots[slot_idx].done = Some(success);
                }
            }
            Ok(_) => {} // other messages (Line, SlotStarted, …) irrelevant at shutdown
            Err(_) => break,
        }
    }
}

// ── Debug logger ──────────────────────────────────────────────────────────

/// Optional TUI message-flow logger, active only when `CURIE_TUI_DEBUG=1`.
///
/// Writes timestamped lines to `/tmp/curie-tui-debug.log`.  Useful for
/// reproducing "spurious Skipped" races: each received message, the
/// pending-queue depth at key decision points, and every `apply_shutdown`
/// invocation with the slots that still have `done == None` are recorded.
struct DbgLog(Option<std::io::BufWriter<std::fs::File>>);

impl DbgLog {
    fn open() -> Self {
        if std::env::var_os("CURIE_TUI_DEBUG").is_some() {
            let f = std::fs::OpenOptions::new()
                .create(true).append(true)
                .open("/tmp/curie-tui-debug.log").ok();
            DbgLog(f.map(std::io::BufWriter::new))
        } else {
            DbgLog(None)
        }
    }

    fn log(&mut self, msg: &str) {
        if let Some(ref mut w) = self.0 {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let _ = writeln!(w, "[{:.3}] {}", t.as_secs_f64(), msg);
            let _ = w.flush();
        }
    }

    fn is_on(&self) -> bool { self.0.is_some() }
}

fn describe_msg(msg: &TuiMsg) -> String {
    match msg {
        TuiMsg::SlotStarted { slot_idx }       => format!("SlotStarted(slot={slot_idx})"),
        TuiMsg::SlotSkipped { slot_idx, reason }
                                                => format!("SlotSkipped(slot={slot_idx} reason={reason:?})"),
        TuiMsg::Line        { slot_idx, .. }   => format!("Line(slot={slot_idx})"),
        TuiMsg::SlotDone    { slot_idx, success }
                                                => format!("SlotDone(slot={slot_idx} ok={success})"),
        TuiMsg::Shutdown                        => "Shutdown".to_string(),
    }
}

fn log_apply_shutdown(dbg: &mut DbgLog, state: &RenderState, pending: &std::collections::VecDeque<TuiMsg>) {
    if !dbg.is_on() { return; }
    let none_slots: Vec<usize> = state.slots.iter().enumerate()
        .filter(|(_, s)| s.done.is_none() && !s.skipped)
        .map(|(i, _)| i)
        .collect();
    let pending_done: Vec<String> = pending.iter()
        .filter_map(|m| if let TuiMsg::SlotDone { slot_idx, success } = m {
            Some(format!("SD({slot_idx} ok={success})"))
        } else { None })
        .collect();
    dbg.log(&format!(
        "APPLY_SHUTDOWN none_slots={none_slots:?} pending_done=[{}]",
        pending_done.join(", ")
    ));
}

/// Print the last `max_lines` lines of every failed slot to `out`.
///
/// Called after the alternate screen is left so output appears on the primary
/// screen below the restored content, before the caller's summary lines.
///
/// Lines are sanitised via [`truncate_to_cols`] to strip OSC / non-SGR escape
/// sequences while keeping ANSI colour codes, preventing a stray hyperlink
/// escape from poisoning the terminal.  Column truncation is disabled (the
/// terminal wraps naturally).
///
/// Each failed slot is preceded by a bold-red header and followed by a blank
/// line separator.
fn print_failed_tails(state: &RenderState, max_lines: usize, out: &mut dyn io::Write) {
    let failed: Vec<&SlotData> = state
        .slots
        .iter()
        .filter(|s| s.done == Some(false))
        .collect();
    if failed.is_empty() {
        return;
    }
    // Strip unsafe escapes but do not truncate columns — the terminal wraps.
    const NO_COL_LIMIT: usize = usize::MAX >> 1;
    for slot in &failed {
        let _ = writeln!(out, "\x1b[1;31m── {} ──\x1b[0m", slot.name);
        let skip = slot.ring.len().saturating_sub(max_lines);
        for line in slot.ring.iter().skip(skip) {
            let _ = writeln!(out, "  {}", truncate_to_cols(line, NO_COL_LIMIT));
        }
        let _ = writeln!(out);
    }
}

/// Print one ANSI-coloured overflow line for a single group.
///
/// Mirrors the format of [`render_overflow_lines`]: `"  · {label}{names}\n"`.
/// No output is produced when `names` is empty.
fn print_summary_group(
    out:        &mut dyn io::Write,
    label:      &str,
    names:      &[&str],
    label_ansi: &str,
    name_ansi:  &str,
    term_w:     usize,
) {
    if names.is_empty() {
        return;
    }
    let prefix     = "  \u{00b7} ";
    let prefix_len = 4;
    let budget     = term_w.saturating_sub(prefix_len + label.len());
    let body       = build_overflow_names(names, budget);
    let _ = writeln!(out, "{prefix}{label_ansi}{label}{name_ansi}{body}\x1b[0m");
}

/// Print the Skipped / Done / Failed summary lines after the build.
///
/// Replicates the three post-build-relevant overflow groups from the live TUI
/// (same prefix, same colours, same member-name lists via
/// [`build_overflow_names`]), in the same order they appear on screen:
/// Skipped → Done → Failed.  Running and Pending are transient and omitted.
///
/// Unlike [`classify_overflow_slots`] (which only classifies slots **not** in
/// a visible pane), this function inspects every slot directly so that failed
/// members whose pane was kept open are still included in the `Failed:` line.
///
/// Called after [`print_failed_tails`] so it appears below the error tails.
fn print_build_summary(state: &RenderState, out: &mut dyn io::Write) {
    let term_w = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);

    let skipped:  Vec<&str> = state.slots.iter()
        .filter(|s| s.skipped)
        .map(|s| s.name.as_str())
        .collect();
    let done_ok:  Vec<&str> = state.slots.iter()
        .filter(|s| s.done == Some(true))
        .map(|s| s.name.as_str())
        .collect();
    let done_err: Vec<&str> = state.slots.iter()
        .filter(|s| s.done == Some(false))
        .map(|s| s.name.as_str())
        .collect();

    let skip_label = match state.skip_reason.as_deref() {
        Some(r) => format!("Skipped ({}): ", r),
        None    => "Skipped: ".to_string(),
    };

    print_summary_group(out, &skip_label, &skipped,  "\x1b[33m",   "\x1b[2m",  term_w);
    print_summary_group(out, &format!("{}: ", state.done_label), &done_ok,  "\x1b[1;32m", "\x1b[32m", term_w);
    print_summary_group(out, "Failed: ",  &done_err, "\x1b[1;31m", "\x1b[31m", term_w);
}

/// Apply the shutdown close rules to the render state.
///
/// * Rule 3: close every pane showing a successful job, so only failed panes
///   (red ✗) remain on screen after the build ends.
/// * Any job that never started was cancelled by an earlier failure (or an
///   interrupted build) and is marked "Skipped" rather than "Pending".
///
/// Called from every shutdown path — including when `Shutdown` interrupts a
/// pane's hold window — so a held-green pane is never left open.
fn apply_shutdown(state: &mut RenderState) {
    state.pane_to_slot.retain(|&s| state.slots[s].done != Some(true));
    for slot in &mut state.slots {
        if slot.done.is_none() {
            slot.skipped = true;
        }
    }
}

/// Surface a background job that just failed by giving it a visible pane so its
/// error is seen immediately rather than only in the "Failed" overflow line.
///
/// * If a pane slot is free, the failure simply opens a new pane.
/// * Otherwise it takes over a pane that is still **running**, and that running
///   job is moved to the front of the background queue (so it returns to a pane
///   as soon as one frees up).  Panes already showing a finished result
///   (success or failure) are never evicted.
/// * If every pane already shows a result and none is running, the failure
///   stays in the "Failed" overflow group until a pane frees up.
///
/// The caller must have already set `done` and removed `slot_idx` from the
/// background queue.
fn place_failed_in_pane(state: &mut RenderState, slot_idx: usize) {
    if state.pane_to_slot.len() < state.visible {
        state.pane_to_slot.push(slot_idx);
        return;
    }
    if let Some(pane_idx) = state
        .pane_to_slot
        .iter()
        .position(|&s| state.slots[s].done.is_none())
    {
        let demoted = state.pane_to_slot[pane_idx];
        state.pane_to_slot[pane_idx] = slot_idx;
        state.background_queue.push_front(demoted);
    }
}

/// Split `total` rows evenly among `count` open panes, giving any remainder to
/// the topmost panes.  The open panes always consume the whole available height,
/// so a pane closing lets the survivors grow into the vacated space.
fn distribute_pane_heights(total: u16, count: usize) -> Vec<u16> {
    if count == 0 {
        return Vec::new();
    }
    let count = count as u16;
    let base  = total / count;
    let extra = total % count;
    (0..count)
        .map(|i| base + if i < extra { 1 } else { 0 })
        .collect()
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
    let deadline       = Instant::now() + Duration::from_secs(hold_secs);
    let frame_interval = Duration::from_millis(33); // cap at ~30 fps
    let mut last_draw  = Instant::now() - frame_interval;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(TuiMsg::Line { slot_idx, line }) => {
                state.slots[slot_idx].ring.push_back(line);
                if last_draw.elapsed() >= frame_interval {
                    let _ = terminal.draw(|f| render_frame(f, state));
                    last_draw = Instant::now();
                }
            }
            // A new job starting during the hold fills a free pane or joins the
            // running overflow group; it never disturbs the finishing pane.
            Ok(TuiMsg::SlotStarted { slot_idx }) => {
                note_started(state, slot_idx);
                let _ = terminal.draw(|f| render_frame(f, state));
            }
            Ok(TuiMsg::SlotSkipped { slot_idx, reason }) => {
                note_skipped(state, slot_idx, reason);
                let _ = terminal.draw(|f| render_frame(f, state));
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                return None;
            }
            Ok(other) => return Some(other),
        }
    }
}

/// Drain all immediately-available messages from `rx` without blocking.
///
/// `Line` messages are stored in their slot rings.  The first non-`Line`
/// message is pushed onto `pending` and draining stops — subsequent
/// non-`Line` messages remain in the channel for the next iteration of the
/// main loop.
///
/// Using `&mut VecDeque<TuiMsg>` rather than `&mut Option<TuiMsg>` is
/// intentional: if `pending` already holds a message from a prior
/// `drain_hold` call, we must not drop the newly-received non-`Line`
/// message.  Pushing to the back preserves FIFO delivery order.
fn drain_available(
    rx: &mpsc::Receiver<TuiMsg>,
    state: &mut RenderState,
    pending: &mut std::collections::VecDeque<TuiMsg>,
) {
    loop {
        match rx.try_recv() {
            Ok(TuiMsg::Line { slot_idx, line }) => {
                state.slots[slot_idx].ring.push_back(line);
            }
            Ok(TuiMsg::SlotStarted { slot_idx }) => {
                note_started(state, slot_idx);
            }
            Ok(TuiMsg::SlotSkipped { slot_idx, reason }) => {
                note_skipped(state, slot_idx, reason);
            }
            Ok(other) => {
                pending.push_back(other);
                break;
            }
            Err(_) => break,
        }
    }
}

// ── Frame renderer (pure, no I/O) ─────────────────────────────────────────

fn render_frame(f: &mut Frame, state: &RenderState) {
    let area = f.area();

    let groups        = classify_overflow_slots(state);
    let overflow_rows = count_nonempty_groups(&groups) as u16;

    // Open panes share all the height not taken by the overflow lines, so when
    // a pane closes the survivors grow to reclaim the freed rows.
    let avail   = area.height.saturating_sub(overflow_rows);
    let heights = distribute_pane_heights(avail, state.pane_to_slot.len());

    let mut constraints: Vec<Constraint> =
        heights.iter().map(|&h| Constraint::Length(h)).collect();
    for _ in 0..overflow_rows {
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

    // Draw one line per non-empty group.
    let base = state.pane_to_slot.len();
    render_overflow_lines(
        f,
        &chunks[base..],
        area.width as usize,
        &groups,
        state.skip_reason.as_deref(),
        &state.done_label,
    );
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

// ── Overflow classification ────────────────────────────────────────────────

struct OverflowGroups<'a> {
    running:  Vec<&'a str>,
    pending:  Vec<&'a str>,
    skipped:  Vec<&'a str>,
    done_ok:  Vec<&'a str>,
    done_err: Vec<&'a str>,
}

fn classify_overflow_slots(state: &RenderState) -> OverflowGroups<'_> {
    let visible_set: std::collections::HashSet<usize> =
        state.pane_to_slot.iter().copied().collect();
    let in_queue: std::collections::HashSet<usize> =
        state.background_queue.iter().copied().collect();

    let mut g = OverflowGroups {
        running:  Vec::new(),
        pending:  Vec::new(),
        skipped:  Vec::new(),
        done_ok:  Vec::new(),
        done_err: Vec::new(),
    };

    for (idx, slot) in state.slots.iter().enumerate() {
        if visible_set.contains(&idx) {
            continue;
        }
        match slot.done {
            // Cancelled by an earlier failure — never ran.
            None if slot.skipped            => g.skipped.push(&slot.name),
            None if in_queue.contains(&idx) => g.running.push(&slot.name),
            None                            => g.pending.push(&slot.name),
            Some(true)                      => g.done_ok.push(&slot.name),
            Some(false)                     => g.done_err.push(&slot.name),
        }
    }
    g
}

fn count_nonempty_groups(g: &OverflowGroups<'_>) -> usize {
    [&g.running, &g.pending, &g.skipped, &g.done_ok, &g.done_err]
        .iter()
        .filter(|v| !v.is_empty())
        .count()
}

// ── Overflow rendering ─────────────────────────────────────────────────────

/// Render one row per non-empty group into `areas` (one Rect per row).
///
/// `skip_reason`, when present, is shown after the "Skipped" label as
/// `Skipped (<reason>): …` so the user sees why those jobs never ran.
fn render_overflow_lines(
    f: &mut Frame,
    areas: &[Rect],
    width: usize,
    groups: &OverflowGroups<'_>,
    skip_reason: Option<&str>,
    done_label: &str,
) {
    let dim    = Style::new().add_modifier(Modifier::DIM);
    let cyan   = Style::new().fg(Color::Cyan);
    let yellow = Style::new().fg(Color::Yellow);
    let green  = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let red    = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);

    struct GroupSpec<'a> {
        label:       String,
        names:       &'a [&'a str],
        label_style: Style,
        name_style:  Style,
    }

    let skipped_label = match skip_reason {
        Some(reason) => format!("Skipped ({}): ", reason),
        None         => "Skipped: ".to_string(),
    };

    let specs = [
        GroupSpec { label: "Running: ".into(), names: &groups.running,  label_style: cyan,   name_style: dim   },
        GroupSpec { label: "Pending: ".into(), names: &groups.pending,  label_style: dim,    name_style: dim   },
        GroupSpec { label: skipped_label,      names: &groups.skipped,  label_style: yellow, name_style: dim   },
        GroupSpec { label: format!("{}: ", done_label), names: &groups.done_ok,  label_style: green,  name_style: green },
        GroupSpec { label: "Failed: ".into(),  names: &groups.done_err, label_style: red,    name_style: red   },
    ];

    let mut area_idx = 0;
    for spec in &specs {
        if spec.names.is_empty() {
            continue;
        }
        if area_idx >= areas.len() {
            break;
        }
        let area = areas[area_idx];
        area_idx += 1;

        let prefix     = "  \u{00b7} "; // "  · "
        let prefix_len = 4;
        let label_len  = spec.label.len();
        let budget     = width.saturating_sub(prefix_len + label_len);
        let body       = build_overflow_names(spec.names, budget);

        let line = Line::from(vec![
            Span::styled(prefix,             dim),
            Span::styled(spec.label.clone(), spec.label_style),
            Span::styled(body,               spec.name_style),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
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

    // ── note_started / lazy pane assignment ───────────────────────────────

    fn empty_state(n: usize, visible: usize) -> RenderState {
        RenderState {
            slots: (0..n)
                .map(|i| SlotData {
                    name: format!("m{i}"),
                    done: None,
                    skipped: false,
                    ring: VecDeque::new(),
                })
                .collect(),
            pane_to_slot: Vec::new(),
            background_queue: VecDeque::new(),
            visible,
            skip_reason: None,
            done_label: "Done".to_string(),
        }
    }

    #[test]
    fn started_jobs_fill_panes_then_queue() {
        let mut st = empty_state(4, 2);
        note_started(&mut st, 0);
        note_started(&mut st, 1);
        assert_eq!(st.pane_to_slot, vec![0, 1]);
        assert!(st.background_queue.is_empty());

        // Panes full → the third starter goes to the background queue.
        note_started(&mut st, 2);
        assert_eq!(st.pane_to_slot, vec![0, 1]);
        assert_eq!(st.background_queue, VecDeque::from(vec![2]));
    }

    #[test]
    fn unstarted_slots_are_pending_not_empty_panes() {
        // Only slot 0 has started; 1 and 2 are still pending and must NOT
        // occupy (empty) panes — they belong in the "Pending" overflow group.
        let mut st = empty_state(3, 2);
        note_started(&mut st, 0);

        assert_eq!(st.pane_to_slot, vec![0]);
        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.pending, vec!["m1", "m2"]);
        assert!(groups.running.is_empty());
    }

    #[test]
    fn queued_started_slot_is_running_overflow() {
        // visible=1: slot 0 takes the pane, slot 1 (started) is a running
        // background job, slot 2 (not started) stays pending.
        let mut st = empty_state(3, 1);
        note_started(&mut st, 0);
        note_started(&mut st, 1);

        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.running, vec!["m1"]);
        assert_eq!(groups.pending, vec!["m2"]);
    }

    #[test]
    fn pane_reopens_after_close_for_later_starter() {
        // A pane closed (rule 2) leaves room; a job that starts later must be
        // able to claim the freed pane rather than being queued.
        let mut st = empty_state(4, 2);
        note_started(&mut st, 0);
        note_started(&mut st, 1);
        st.pane_to_slot.remove(0); // simulate slot 0's pane closing
        note_started(&mut st, 2);
        assert_eq!(st.pane_to_slot, vec![1, 2]);
        assert!(st.background_queue.is_empty());
    }

    // ── place_failed_in_pane ──────────────────────────────────────────────

    /// Mark a background slot as finished (as the SlotDone handler does) and
    /// surface it if it failed.
    fn finish_background(st: &mut RenderState, slot_idx: usize, success: bool) {
        st.slots[slot_idx].done = Some(success);
        st.background_queue.retain(|&s| s != slot_idx);
        if !success {
            place_failed_in_pane(st, slot_idx);
        }
    }

    #[test]
    fn failed_background_takes_over_running_pane() {
        // visible=1: slot 0 runs in the pane, slot 1 runs in the background.
        let mut st = empty_state(2, 1);
        note_started(&mut st, 0);
        note_started(&mut st, 1);
        assert_eq!(st.pane_to_slot, vec![0]);
        assert_eq!(st.background_queue, VecDeque::from(vec![1]));

        // slot 1 fails in the background → it takes the pane, slot 0 (still
        // running) is demoted to the front of the queue.
        finish_background(&mut st, 1, false);
        assert_eq!(st.pane_to_slot, vec![1]);
        assert_eq!(st.background_queue, VecDeque::from(vec![0]));
    }

    #[test]
    fn failed_background_uses_free_pane_without_demoting() {
        // visible=2 but only one pane open; a failed background job fills the
        // free slot rather than evicting the running job.
        let mut st = empty_state(2, 2);
        note_started(&mut st, 0);
        st.background_queue.push_back(1); // slot 1 queued (running)

        finish_background(&mut st, 1, false);
        assert_eq!(st.pane_to_slot, vec![0, 1]);
        assert!(st.background_queue.is_empty());
    }

    #[test]
    fn failed_background_picks_first_running_pane() {
        // Two panes: slot 0 already failed (red), slot 1 still running.  A new
        // background failure must take over the running pane, not the failed one.
        let mut st = empty_state(3, 2);
        note_started(&mut st, 0);
        note_started(&mut st, 1);
        st.slots[0].done = Some(false); // pane 0 shows a failure
        st.background_queue.push_back(2);

        finish_background(&mut st, 2, false);
        assert_eq!(st.pane_to_slot, vec![0, 2]); // pane 1 (running) replaced
        assert_eq!(st.background_queue, VecDeque::from(vec![1]));
    }

    #[test]
    fn failed_background_stays_in_overflow_when_no_pane_is_running() {
        // The only pane already shows a failure; a second background failure has
        // nowhere to go and stays in the "Failed" overflow group.
        let mut st = empty_state(2, 1);
        note_started(&mut st, 0);
        st.slots[0].done = Some(false);
        st.background_queue.push_back(1);

        finish_background(&mut st, 1, false);
        assert_eq!(st.pane_to_slot, vec![0]); // unchanged
        assert!(st.background_queue.is_empty());
        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.done_err, vec!["m1"]);
    }

    #[test]
    fn successful_background_does_not_take_a_pane() {
        // A background job that succeeds is just dropped from the queue.
        let mut st = empty_state(2, 1);
        note_started(&mut st, 0);
        note_started(&mut st, 1);

        finish_background(&mut st, 1, true);
        assert_eq!(st.pane_to_slot, vec![0]);
        assert!(st.background_queue.is_empty());
        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.done_ok, vec!["m1"]);
    }

    // ── apply_shutdown ────────────────────────────────────────────────────

    #[test]
    fn shutdown_closes_successful_pane_keeps_failed() {
        // Last frame of a failed build: a green pane and a red pane are both
        // open.  Shutdown must drop the green one and keep only the red.
        let mut st = empty_state(3, 2);
        note_started(&mut st, 0);
        note_started(&mut st, 1);
        st.slots[0].done = Some(true);  // succeeded
        st.slots[1].done = Some(false); // failed
        // slot 2 never started.

        apply_shutdown(&mut st);

        assert_eq!(st.pane_to_slot, vec![1]); // only the failed pane remains
        assert!(st.slots[2].skipped);         // never-started job marked skipped
    }

    #[test]
    fn shutdown_marks_unstarted_jobs_skipped() {
        let mut st = empty_state(2, 1);
        note_started(&mut st, 0);
        st.slots[0].done = Some(false);

        apply_shutdown(&mut st);

        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.skipped, vec!["m1"]);
        assert!(groups.pending.is_empty());
    }

    // ── print_failed_tails ────────────────────────────────────────────────

    #[test]
    fn tail_is_empty_when_no_failures() {
        let st = empty_state(3, 3);
        let mut buf = Vec::<u8>::new();
        print_failed_tails(&st, 15, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn tail_skips_successful_and_pending_slots() {
        let mut st = empty_state(3, 3);
        st.slots[0].done = Some(true);   // succeeded — not printed
        st.slots[1].done = None;          // still running — not printed
        st.slots[2].done = Some(false);  // failed — printed
        st.slots[2].ring.push_back("err line".to_string());

        let mut buf = Vec::<u8>::new();
        print_failed_tails(&st, 15, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("m2"), "header for the failed slot");
        assert!(out.contains("err line"));
        assert!(!out.contains("m0"), "successful slot must not appear");
        assert!(!out.contains("m1"), "pending slot must not appear");
    }

    #[test]
    fn tail_prints_last_n_lines_and_trims_older_ones() {
        let mut st = empty_state(1, 1);
        st.slots[0].done = Some(false);
        for i in 0..20u32 {
            st.slots[0].ring.push_back(format!("line {i}"));
        }

        let mut buf = Vec::<u8>::new();
        print_failed_tails(&st, 15, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        // Last 15 lines (5..=19) must appear.
        for i in 5..20u32 {
            assert!(out.contains(&format!("line {i}")), "missing line {i}");
        }
        // The first 5 lines (0..4) must be absent.
        for i in 0..5u32 {
            assert!(!out.contains(&format!("line {i}\n")), "old line {i} must be trimmed");
        }
    }

    #[test]
    fn tail_prints_all_lines_when_fewer_than_max() {
        let mut st = empty_state(1, 1);
        st.slots[0].done = Some(false);
        for i in 0..5u32 {
            st.slots[0].ring.push_back(format!("e{i}"));
        }

        let mut buf = Vec::<u8>::new();
        print_failed_tails(&st, 15, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        for i in 0..5u32 {
            assert!(out.contains(&format!("e{i}")));
        }
    }

    #[test]
    fn tail_prints_multiple_failed_slots_in_order() {
        let mut st = empty_state(3, 3);
        st.slots[0].done = Some(false);
        st.slots[0].ring.push_back("alpha error".to_string());
        st.slots[1].done = Some(true);   // skipped
        st.slots[2].done = Some(false);
        st.slots[2].ring.push_back("beta error".to_string());

        let mut buf = Vec::<u8>::new();
        print_failed_tails(&st, 15, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        let pos_alpha = out.find("alpha error").unwrap();
        let pos_beta  = out.find("beta error").unwrap();
        assert!(pos_alpha < pos_beta, "slot 0 must appear before slot 2");
    }

    // ── print_build_summary ───────────────────────────────────────────────

    #[test]
    fn summary_empty_when_all_pending() {
        // No slot has finished or been skipped — nothing should be printed.
        let st = empty_state(3, 3);
        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn summary_prints_done_group() {
        let mut st = empty_state(2, 2);
        st.slots[0].done = Some(true);
        st.slots[1].done = Some(true);

        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("Done:"), "Done label missing");
        assert!(out.contains("m0"), "first member missing");
        assert!(out.contains("m1"), "second member missing");
    }

    #[test]
    fn summary_prints_failed_group() {
        let mut st = empty_state(2, 2);
        st.slots[0].done = Some(false);
        st.slots[1].done = Some(false);

        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("Failed:"), "Failed label missing");
        assert!(out.contains("m0"));
        assert!(out.contains("m1"));
    }

    #[test]
    fn summary_prints_skipped_group_with_reason() {
        let mut st = empty_state(3, 1);
        note_started(&mut st, 0);
        st.slots[0].done = Some(false);
        note_skipped(&mut st, 1, "core failed".to_string());
        note_skipped(&mut st, 2, "core failed".to_string());

        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("Skipped (core failed):"), "skipped label with reason missing");
        assert!(out.contains("m1"));
        assert!(out.contains("m2"));
    }

    #[test]
    fn summary_omits_empty_groups() {
        // Only done slots — no Skipped or Failed lines should appear.
        let mut st = empty_state(2, 2);
        st.slots[0].done = Some(true);
        st.slots[1].done = Some(true);

        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        assert!(!out.contains("Failed:"),  "empty Failed group must not appear");
        assert!(!out.contains("Skipped:"), "empty Skipped group must not appear");
    }

    #[test]
    fn summary_all_three_groups_in_order() {
        // Skipped must precede Done; Done must precede Failed — same order as
        // the live TUI overflow lines.
        let mut st = empty_state(3, 1);
        note_started(&mut st, 0);
        st.slots[0].done = Some(false);
        note_skipped(&mut st, 1, "core failed".to_string());
        // Slot 2 succeeded; it never gets a pane but mark it done.
        st.slots[2].done = Some(true);

        let mut buf = Vec::<u8>::new();
        print_build_summary(&st, &mut buf);
        let out = String::from_utf8(buf).unwrap();

        let pos_skipped = out.find("Skipped").unwrap();
        let pos_done    = out.find("Done:").unwrap();
        let pos_failed  = out.find("Failed:").unwrap();

        assert!(pos_skipped < pos_done,   "Skipped must appear before Done");
        assert!(pos_done    < pos_failed, "Done must appear before Failed");
    }

    // ── drain_available regression ─────────────────────────────────────────

    #[test]
    fn drain_available_does_not_drop_slot_done_when_pending_already_set() {
        // Regression: with a fast incremental build, drain_hold stashes SD_1
        // into pending, then drain_available receives SD_2 from the channel.
        // With the old Option<TuiMsg> impl, SD_2 was silently dropped because
        // pending.is_some(); apply_shutdown then marked slot 2 as Skipped.
        use std::collections::VecDeque;
        use std::sync::mpsc;

        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(10);

        // Slot 0 done — put in the channel for drain_available to receive.
        tx.send(TuiMsg::SlotDone { slot_idx: 0, success: true }).unwrap();
        drop(tx); // close so try_recv returns Err after the one message

        let mut state = empty_state(2, 2);
        // Simulate drain_hold having already stashed SD_1 for slot 1.
        let mut pending: VecDeque<TuiMsg> =
            VecDeque::from([TuiMsg::SlotDone { slot_idx: 1, success: true }]);

        drain_available(&rx, &mut state, &mut pending);

        // Both SD_1 (pre-existing) and SD_0 (just received) must be in pending.
        assert_eq!(pending.len(), 2, "drain_available must not drop SD_0");
    }

    // ── resolve_outstanding_done_messages ─────────────────────────────────

    /// Helper: build a VecDeque<TuiMsg> from a list of (slot, success) pairs.
    fn slot_done_queue(pairs: &[(usize, bool)]) -> VecDeque<TuiMsg> {
        pairs.iter().map(|&(s, ok)| TuiMsg::SlotDone { slot_idx: s, success: ok }).collect()
    }

    #[test]
    fn resolve_flushes_slot_done_from_pending() {
        // Both SD_0 (success) and SD_1 (failure) are already in `pending`.
        // After resolve, both slots should have done set — not None.
        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(4);
        drop(tx);

        let mut state = empty_state(2, 2);
        let pending = slot_done_queue(&[(0, true), (1, false)]);

        resolve_outstanding_done_messages(&rx, &mut state, &pending);

        assert_eq!(state.slots[0].done, Some(true));
        assert_eq!(state.slots[1].done, Some(false));
    }

    #[test]
    fn resolve_drains_slot_done_from_channel() {
        // Both SD_0 and SD_1 arrive via the channel (pending is empty).
        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(4);
        tx.send(TuiMsg::SlotDone { slot_idx: 0, success: true  }).unwrap();
        tx.send(TuiMsg::SlotDone { slot_idx: 1, success: false }).unwrap();
        drop(tx);

        let mut state = empty_state(2, 2);
        let pending = VecDeque::new();

        resolve_outstanding_done_messages(&rx, &mut state, &pending);

        assert_eq!(state.slots[0].done, Some(true));
        assert_eq!(state.slots[1].done, Some(false));
    }

    #[test]
    fn resolve_combines_pending_and_channel() {
        // SD_0 is in pending, SD_1 is still in the channel.
        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(4);
        tx.send(TuiMsg::SlotDone { slot_idx: 1, success: false }).unwrap();
        drop(tx);

        let mut state = empty_state(2, 2);
        let pending = slot_done_queue(&[(0, true)]);

        resolve_outstanding_done_messages(&rx, &mut state, &pending);

        assert_eq!(state.slots[0].done, Some(true));
        assert_eq!(state.slots[1].done, Some(false));
    }

    #[test]
    fn resolve_does_not_overwrite_already_done_slot() {
        // Slot 0 already has done=Some(true).  A later SD_0(false) in pending
        // must not overwrite the first result.
        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(4);
        drop(tx);

        let mut state = empty_state(1, 1);
        state.slots[0].done = Some(true);
        let pending = slot_done_queue(&[(0, false)]);

        resolve_outstanding_done_messages(&rx, &mut state, &pending);

        assert_eq!(state.slots[0].done, Some(true), "first result must not be overwritten");
    }

    #[test]
    fn resolve_ignores_non_slot_done_messages_in_channel() {
        // A Line message in the channel must be silently dropped — it is
        // irrelevant at shutdown and must not cause a panic or affect done state.
        let (tx, rx) = mpsc::sync_channel::<TuiMsg>(4);
        tx.send(TuiMsg::Line { slot_idx: 0, line: "hello".to_string() }).unwrap();
        drop(tx);

        let mut state = empty_state(1, 1);
        let pending = VecDeque::new();

        resolve_outstanding_done_messages(&rx, &mut state, &pending);

        assert_eq!(state.slots[0].done, None, "non-SlotDone must be ignored");
    }

    // ── skipped classification ────────────────────────────────────────────

    #[test]
    fn skipped_slots_are_separated_from_pending() {
        // slot 0 runs in a pane; slot 1 was cancelled (skipped); slot 2 is
        // still genuinely pending.
        let mut st = empty_state(3, 1);
        note_started(&mut st, 0);
        st.slots[1].skipped = true;

        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.skipped, vec!["m1"]);
        assert_eq!(groups.pending, vec!["m2"]);
        assert!(groups.running.is_empty());
    }

    #[test]
    fn note_skipped_marks_slots_and_keeps_first_reason() {
        let mut st = empty_state(3, 1);
        note_started(&mut st, 0);
        note_skipped(&mut st, 1, "lombok-greeter failed".to_string());
        // A later skip keeps the original reason (all share the same cause).
        note_skipped(&mut st, 2, "something-else failed".to_string());

        assert!(st.slots[1].skipped);
        assert!(st.slots[2].skipped);
        assert_eq!(st.skip_reason.as_deref(), Some("lombok-greeter failed"));

        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.skipped, vec!["m1", "m2"]);
    }

    #[test]
    fn skipped_takes_precedence_over_queued_running() {
        // A skipped slot is never reported as running even if it lingered in
        // the background queue.
        let mut st = empty_state(2, 1);
        note_started(&mut st, 0);
        st.background_queue.push_back(1);
        st.slots[1].skipped = true;

        let groups = classify_overflow_slots(&st);
        assert_eq!(groups.skipped, vec!["m1"]);
        assert!(groups.running.is_empty());
        assert!(groups.pending.is_empty());
    }

    // ── distribute_pane_heights ───────────────────────────────────────────

    #[test]
    fn heights_divide_evenly() {
        assert_eq!(distribute_pane_heights(40, 4), vec![10, 10, 10, 10]);
    }

    #[test]
    fn heights_remainder_goes_to_top_panes() {
        // 41 / 4 = 10 r1 → first pane gets the extra row.
        assert_eq!(distribute_pane_heights(41, 4), vec![11, 10, 10, 10]);
        // 38 / 4 = 9 r2 → first two panes get an extra row.
        assert_eq!(distribute_pane_heights(38, 4), vec![10, 10, 9, 9]);
    }

    #[test]
    fn heights_survivors_grow_when_a_pane_closes() {
        // Three panes share 39 rows (13 each); after one closes the remaining
        // two reclaim the full height (≈19/20) instead of leaving a gap.
        assert_eq!(distribute_pane_heights(39, 3), vec![13, 13, 13]);
        assert_eq!(distribute_pane_heights(39, 2), vec![20, 19]);
        assert_eq!(distribute_pane_heights(39, 1), vec![39]);
    }

    #[test]
    fn heights_no_panes_is_empty() {
        assert_eq!(distribute_pane_heights(40, 0), Vec::<u16>::new());
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
