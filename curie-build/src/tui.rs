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
    /// Worker thread began running this job.  The render thread assigns it a
    /// free pane (or queues it as a running background job).  Only started jobs
    /// ever occupy a pane, so pending/never-dispatched jobs don't show as empty
    /// boxes — they appear in the "Pending" overflow line instead.
    SlotStarted { slot_idx: usize },
    /// Job cancelled by an earlier build failure — it will never run.  Shown in
    /// the "Skipped" overflow group instead of "Pending".
    SlotSkipped { slot_idx: usize },
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
    /// run, so it is shown as "Skipped" rather than "Pending".
    fn skip(&self) {
        let _ = self.sender.send(TuiMsg::SlotSkipped { slot_idx: self.slot_idx });
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
            render_loop(receiver, names, visible_count);
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
}

fn render_loop(
    rx: mpsc::Receiver<TuiMsg>,
    names: Vec<String>,
    visible: usize,
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
    };

    // Initial draw.
    let _ = terminal.draw(|f| render_frame(f, &state));

    // ── Message loop ──────────────────────────────────────────────────────
    // `pending` holds a message that was stashed during a hold window and
    // must be re-processed with full SlotDone treatment next iteration.
    let mut pending: Option<TuiMsg> = None;
    loop {
        let msg = match pending.take().or_else(|| rx.recv().ok()) {
            Some(m) => m,
            None    => break,
        };
        match msg {
            TuiMsg::SlotStarted { slot_idx } => {
                note_started(&mut state, slot_idx);
                let _ = terminal.draw(|f| render_frame(f, &state));
            }

            TuiMsg::SlotSkipped { slot_idx } => {
                state.slots[slot_idx].skipped = true;
                let _ = terminal.draw(|f| render_frame(f, &state));
            }

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
                        // Rule 1: hold green 1 s, then replace with next job.
                        if let Some(stashed) =
                            drain_hold(&rx, &mut terminal, &mut state, 1)
                        {
                            match stashed {
                                // Shutdown during the hold still runs the close
                                // rules so a held-green pane isn't left open.
                                TuiMsg::Shutdown => {
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
                                other => pending = Some(other),
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
                        let _ = terminal.draw(|f| render_frame(f, &state));
                    } else if success {
                        // Rule 2: no replacement — hold 2 s then close.
                        if let Some(stashed) =
                            drain_hold(&rx, &mut terminal, &mut state, 2)
                        {
                            match stashed {
                                // Shutdown during the hold still runs the close
                                // rules so this held-green pane isn't left open.
                                TuiMsg::Shutdown => {
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
                                other => pending = Some(other),
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
                apply_shutdown(&mut state);
                let _ = terminal.draw(|f| render_frame(f, &state));
                break;
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    // Restore full scroll region, re-enable line-wrap, close any open OSC 8
    // hyperlink, then show the cursor again.  No crossterm equivalents for
    // the three raw sequences.
    let _ = write!(io::stdout(), "\x1b[r\x1b[?7h\x1b]8;;\x07");
    let _ = execute!(io::stdout(), cursor::Show);
    // Leave the cursor below the drawn content so the caller's following output
    // (e.g. the "error: …" summary) continues on a clean line.
    let open_panes    = state.pane_to_slot.len();
    let overflow_rows = count_nonempty_groups(&classify_overflow_slots(&state)) as u16;
    let term_h        = terminal.size().map(|s| s.height).unwrap_or(0);
    match cursor_park(open_panes, overflow_rows, term_h) {
        CursorPark::Below(row) => {
            let _ = terminal.set_cursor_position((0, row));
        }
        CursorPark::ScrollFromBottom(row) => {
            // The screen is full — move to the bottom row and emit a newline so
            // the terminal scrolls up one line, opening a fresh line below the
            // content rather than overwriting the last drawn row.
            let _ = terminal.set_cursor_position((0, row));
            let _ = write!(io::stdout(), "\r\n");
        }
    }
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

/// Where to leave the cursor after the final frame so the caller's following
/// output (the "error: …" summary) doesn't overwrite a drawn row.
#[derive(Debug, PartialEq, Eq)]
enum CursorPark {
    /// Blank rows remain below the content — park the cursor on this row.
    Below(u16),
    /// Open panes filled the whole screen, so there is no blank row: park on the
    /// bottom row and scroll up one line to open a fresh line below.
    ScrollFromBottom(u16),
}

/// Decide where to park the cursor at cleanup.
///
/// With open panes the layout always fills the terminal (panes take
/// `term_h - overflow_rows`, the overflow lines take the rest), so we must
/// scroll to make room.  With no open panes only the overflow lines were drawn
/// and there is blank space below them to park in.
fn cursor_park(open_panes: usize, overflow_rows: u16, term_h: u16) -> CursorPark {
    if open_panes > 0 {
        CursorPark::ScrollFromBottom(term_h.saturating_sub(1))
    } else {
        CursorPark::Below(overflow_rows)
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
            // A new job starting during the hold fills a free pane or joins the
            // running overflow group; it never disturbs the finishing pane.
            Ok(TuiMsg::SlotStarted { slot_idx }) => {
                note_started(state, slot_idx);
                let _ = terminal.draw(|f| render_frame(f, state));
            }
            Ok(TuiMsg::SlotSkipped { slot_idx }) => {
                state.slots[slot_idx].skipped = true;
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
/// message is stashed into `pending` (if `pending` is currently `None`) and
/// draining stops — subsequent non-`Line` messages remain in the channel for
/// the next iteration of the main loop.
fn drain_available(
    rx: &mpsc::Receiver<TuiMsg>,
    state: &mut RenderState,
    pending: &mut Option<TuiMsg>,
) {
    loop {
        match rx.try_recv() {
            Ok(TuiMsg::Line { slot_idx, line }) => {
                state.slots[slot_idx].ring.push_back(line);
            }
            Ok(TuiMsg::SlotStarted { slot_idx }) => {
                note_started(state, slot_idx);
            }
            Ok(TuiMsg::SlotSkipped { slot_idx }) => {
                state.slots[slot_idx].skipped = true;
            }
            Ok(other) => {
                if pending.is_none() {
                    *pending = Some(other);
                }
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
    render_overflow_lines(f, &chunks[base..], area.width as usize, &groups);
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
fn render_overflow_lines(
    f: &mut Frame,
    areas: &[Rect],
    width: usize,
    groups: &OverflowGroups<'_>,
) {
    let dim    = Style::new().add_modifier(Modifier::DIM);
    let cyan   = Style::new().fg(Color::Cyan);
    let yellow = Style::new().fg(Color::Yellow);
    let green  = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let red    = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);

    struct GroupSpec<'a> {
        label:       &'static str,
        names:       &'a [&'a str],
        label_style: Style,
        name_style:  Style,
    }

    let specs = [
        GroupSpec { label: "Running: ", names: &groups.running,  label_style: cyan,   name_style: dim   },
        GroupSpec { label: "Pending: ", names: &groups.pending,  label_style: dim,    name_style: dim   },
        GroupSpec { label: "Skipped: ", names: &groups.skipped,  label_style: yellow, name_style: dim   },
        GroupSpec { label: "Done: ",    names: &groups.done_ok,  label_style: green,  name_style: green },
        GroupSpec { label: "Failed: ",  names: &groups.done_err, label_style: red,    name_style: red   },
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
            Span::styled(prefix,        dim),
            Span::styled(spec.label,    spec.label_style),
            Span::styled(body,          spec.name_style),
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

    // ── cursor_park ───────────────────────────────────────────────────────

    #[test]
    fn cursor_parks_below_overflow_when_no_panes_open() {
        // Only overflow lines drawn (all panes were successful and closed):
        // park on the blank row right under them.
        assert_eq!(cursor_park(0, 2, 40), CursorPark::Below(2));
    }

    #[test]
    fn cursor_scrolls_when_open_panes_fill_screen() {
        // A failed pane fills the screen → scroll up from the bottom row so the
        // following "error: …" line doesn't overwrite the last drawn row.
        assert_eq!(cursor_park(1, 2, 40), CursorPark::ScrollFromBottom(39));
        assert_eq!(cursor_park(2, 0, 24), CursorPark::ScrollFromBottom(23));
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
