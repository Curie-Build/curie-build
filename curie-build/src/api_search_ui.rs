//! Interactive TUI for Maven Central REST API-backed artifact search.
//!
//! Same layout and key bindings as `search_ui`, but queries Maven Central
//! over HTTP instead of a local Tantivy index.  Searches are debounced
//! (400 ms after the last keystroke) and run on a background thread so the
//! UI stays responsive while the request is in flight.

use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{Attribute, Color, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};

use crate::api_search::{search_api, ArtifactHit};

const SEARCH_LIMIT: usize = 50;
const DEBOUNCE_MS: u64 = 400;
const HEADER: &str =
    "  curie add (API)    \u{2191}\u{2193} navigate    Enter select    Esc cancel";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum SearchStatus {
    Idle,
    Searching,
    Done(usize),
    Error(String),
}

pub(crate) struct UiState {
    pub(crate) query:        String,
    pub(crate) results:      Vec<ArtifactHit>,
    pub(crate) selected_idx: usize,
    scroll_offset:           usize,
    status:                  SearchStatus,
    last_query_sent:         String,
    last_keystroke:          Instant,
}

impl UiState {
    fn new() -> Self {
        UiState {
            query:           String::new(),
            results:         Vec::new(),
            selected_idx:    0,
            scroll_offset:   0,
            status:          SearchStatus::Idle,
            last_query_sent: String::new(),
            last_keystroke:  Instant::now(),
        }
    }

    /// Reset timing so returning from the version picker doesn't trigger a
    /// stale debounce.  Query and results are kept intact.
    pub(crate) fn reset_timing(&mut self) {
        // Setting last_keystroke far in the past is harmless: since
        // last_query_sent == query, query_pending is false and no search fires.
        self.last_keystroke = Instant::now();
    }

    fn move_up(&mut self) {
        if self.selected_idx > 0 {
            self.selected_idx -= 1;
        }
    }

    fn move_down(&mut self) {
        if self.selected_idx + 1 < self.results.len() {
            self.selected_idx += 1;
        }
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.results.clear();
        self.selected_idx = 0;
        self.scroll_offset = 0;
        self.status = SearchStatus::Idle;
    }

    fn adjust_scroll(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.selected_idx >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.selected_idx + 1 - visible_rows;
        }
        if self.selected_idx < self.scroll_offset {
            self.scroll_offset = self.selected_idx;
        }
    }
}

// ---------------------------------------------------------------------------
// Main event loop
// ---------------------------------------------------------------------------

type SearchResult = Result<Vec<ArtifactHit>, String>;

/// Run the API search UI.  Pass `initial_state` to restore query/results/selection
/// when returning from the version picker.  Always returns the final state.
pub(crate) fn run_ui_inner(
    stdout: &mut impl Write,
    initial_state: Option<UiState>,
) -> Result<(Option<String>, UiState)> {
    let mut state = match initial_state {
        Some(mut s) => { s.reset_timing(); s }
        None        => UiState::new(),
    };
    let debounce = Duration::from_millis(DEBOUNCE_MS);

    // Dummy channel replaced on first search fire.
    let (_, mut rx): (mpsc::Sender<SearchResult>, Receiver<SearchResult>) = mpsc::channel();

    redraw(stdout, &state)?;

    loop {
        // -- deliver results from background thread (non-blocking) --------------
        if let Ok(res) = rx.try_recv() {
            apply_results(&mut state, res);
            redraw(stdout, &state)?;
        }

        // -- fire debounced search if the query changed and settled -------------
        let query_pending = !state.query.is_empty() && state.query != state.last_query_sent;
        if query_pending && state.last_keystroke.elapsed() >= debounce {
            fire_search(&mut state, &mut rx);
            redraw(stdout, &state)?;
        }

        // -- wait for the next key event (bounded by debounce remaining) --------
        let timeout = if query_pending {
            debounce
                .saturating_sub(state.last_keystroke.elapsed())
                .max(Duration::from_millis(10))
        } else {
            Duration::from_millis(50) // keep polling the result channel
        };

        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                    if handle_key(&mut state, code, modifiers) {
                        // selection confirmed — return chosen coordinate
                        if !state.results.is_empty() {
                            let coord = state.results[state.selected_idx].coord.clone();
                            return Ok((Some(coord), state));
                        }
                    } else if matches!(code, KeyCode::Esc)
                        || matches!((code, modifiers), (KeyCode::Char('c'), KeyModifiers::CONTROL))
                    {
                        return Ok((None, state));
                    }
                }
                Event::Resize(..) => {}
                _ => {}
            }

            let (_, height) = terminal::size().unwrap_or((80, 24));
            state.adjust_scroll((height as usize).saturating_sub(4));
            redraw(stdout, &state)?;
        }
    }
}

/// Process one key press.  Returns `true` when the user confirmed a selection.
fn handle_key(state: &mut UiState, code: KeyCode, modifiers: KeyModifiers) -> bool {
    match (code, modifiers) {
        (KeyCode::Enter, _) => return !state.results.is_empty(),

        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => state.move_up(),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => state.move_down(),

        (KeyCode::Char('u'), KeyModifiers::CONTROL) => state.clear_query(),

        (KeyCode::Backspace, _) => {
            state.query.pop();
            state.last_keystroke = Instant::now();
        }

        (KeyCode::Char(c), mods)
            if mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT =>
        {
            state.query.push(c);
            state.last_keystroke = Instant::now();
        }

        _ => {}
    }
    false
}

/// Spawn a background HTTP search and replace the result channel.
fn fire_search(state: &mut UiState, rx: &mut Receiver<SearchResult>) {
    state.last_query_sent = state.query.clone();
    state.status = SearchStatus::Searching;
    let (new_tx, new_rx) = mpsc::channel::<SearchResult>();
    *rx = new_rx;
    let q = state.query.clone();
    std::thread::spawn(move || {
        let result = search_api(&q, SEARCH_LIMIT).map_err(|e| e.to_string());
        let _ = new_tx.send(result);
    });
}

fn apply_results(state: &mut UiState, res: SearchResult) {
    match res {
        Ok(hits) => {
            let count = hits.len();
            state.results = hits;
            state.selected_idx = 0;
            state.scroll_offset = 0;
            state.status = SearchStatus::Done(count);
        }
        Err(msg) => {
            state.status = SearchStatus::Error(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn redraw(stdout: &mut impl Write, state: &UiState) -> Result<()> {
    let (width, height) = terminal::size()?;
    let w = width as usize;
    let h = height as usize;
    let result_rows = h.saturating_sub(4);
    let st = state.scroll_offset;

    execute!(stdout, cursor::Hide)?;

    // Row 0: header
    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::CurrentLine))?;
    write!(stdout, "{}", truncate_str(HEADER, w))?;

    // Rows 1..result_rows: results
    for screen_row in 0..result_rows {
        let result_idx = st + screen_row;
        execute!(
            stdout,
            cursor::MoveTo(0, (screen_row + 1) as u16),
            terminal::Clear(ClearType::CurrentLine)
        )?;
        if result_idx < state.results.len() {
            let is_sel = result_idx == state.selected_idx;
            let line = format_result_line(&state.results[result_idx], w);
            if is_sel {
                execute!(stdout, SetAttribute(Attribute::Reverse))?;
                write!(stdout, "{}", line)?;
                execute!(stdout, SetAttribute(Attribute::Reset))?;
            } else {
                write!(stdout, "{}", line)?;
            }
        }
    }

    // Row h-3: blank separator
    execute!(
        stdout,
        cursor::MoveTo(0, h.saturating_sub(3) as u16),
        terminal::Clear(ClearType::CurrentLine)
    )?;

    // Row h-2: status
    execute!(
        stdout,
        cursor::MoveTo(0, h.saturating_sub(2) as u16),
        terminal::Clear(ClearType::CurrentLine)
    )?;
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "{}", truncate_str(&format_status(&state.status), w))?;
    execute!(stdout, SetForegroundColor(Color::Reset))?;

    // Row h-1: query prompt
    let query_row = h.saturating_sub(1) as u16;
    execute!(stdout, cursor::MoveTo(0, query_row), terminal::Clear(ClearType::CurrentLine))?;
    let prompt = format!("> {}", state.query);
    write!(stdout, "{}", truncate_str(&prompt, w))?;

    let cursor_col = (2 + state.query.chars().count()).min(w.saturating_sub(1)) as u16;
    execute!(stdout, cursor::MoveTo(cursor_col, query_row), cursor::Show)?;

    stdout.flush()?;
    Ok(())
}

fn format_result_line(hit: &ArtifactHit, w: usize) -> String {
    let marker = "  \u{25CF} ";
    let marker_width = 4;
    let version_part = if hit.latest_version.is_empty() {
        String::new()
    } else {
        format!("  {}", hit.latest_version)
    };
    let full = format!("{}{}{}", marker, hit.coord, version_part);
    let _ = marker_width; // keep for layout symmetry with the Tantivy version
    truncate_str(&full, w)
}

pub(crate) fn truncate_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[..max_chars].iter().collect()
    }
}

fn format_status(status: &SearchStatus) -> String {
    match status {
        SearchStatus::Idle       => "  type to search Maven Central".to_string(),
        SearchStatus::Searching  => "  Searching\u{2026}".to_string(),
        SearchStatus::Done(n)    => format!("  {} results", n),
        SearchStatus::Error(msg) => format!("  API error: {}", msg),
    }
}
