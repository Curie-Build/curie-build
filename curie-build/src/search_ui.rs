//! fzf-style interactive artifact search UI for `curie add`.
//!
//! Layout (rows, 0-indexed from top):
//! ```text
//!  0       │  curie add  ↑↓ navigate  Enter select  Esc cancel
//!  1..H-4  │  [result list]
//!  H-3     │
//!  H-2     │    42 / 487 312 artifacts
//!  H-1     │  > query_text
//! ```

use std::io::Write;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{Attribute, Color, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};

use crate::search_index::{search, total_count, ArtifactRecord, IndexHandle};

const SEARCH_LIMIT: usize = 50;
const HEADER: &str = "  curie add    \u{2191}\u{2193} navigate    Enter select    Esc cancel";

// ---------------------------------------------------------------------------
// Internal UI state
// ---------------------------------------------------------------------------

struct UiState {
    query: String,
    results: Vec<ArtifactRecord>,
    selected_idx: usize,
    scroll_offset: usize,
    total: u64,
}

impl UiState {
    fn new(total: u64) -> Self {
        UiState {
            query: String::new(),
            results: Vec::new(),
            selected_idx: 0,
            scroll_offset: 0,
            total,
        }
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
    }

    fn update_results(&mut self, handle: &IndexHandle) {
        self.results = search(handle, &self.query, SEARCH_LIMIT).unwrap_or_default();
        self.selected_idx = 0;
        self.scroll_offset = 0;
    }

    /// Adjust `scroll_offset` so that `selected_idx` stays in view.
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
// Main loop
// ---------------------------------------------------------------------------

pub(crate) fn run_ui_inner(handle: &IndexHandle, stdout: &mut impl Write) -> Result<Option<String>> {
    let total = total_count(handle);
    let mut state = UiState::new(total);

    redraw(stdout, &state)?;

    loop {
        let ev = crossterm::event::read()?;
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                match (code, modifiers) {
                    (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }

                    (KeyCode::Enter, _) => {
                        if !state.results.is_empty() {
                            return Ok(Some(state.results[state.selected_idx].coord.clone()));
                        }
                    }

                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        state.move_up();
                    }

                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        state.move_down();
                    }

                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        state.clear_query();
                    }

                    (KeyCode::Backspace, _) => {
                        state.query.pop();
                        state.update_results(handle);
                    }

                    (KeyCode::Char(c), mods)
                        if mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT =>
                    {
                        state.query.push(c);
                        state.update_results(handle);
                    }

                    _ => {}
                }
            }

            Event::Resize(..) => {}
            _ => {}
        }

        // Keep selection in view before rendering
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let result_rows = (height as usize).saturating_sub(4);
        state.adjust_scroll(result_rows);

        redraw(stdout, &state)?;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn redraw(stdout: &mut impl Write, state: &UiState) -> Result<()> {
    let (width, height) = terminal::size()?;
    let w = width as usize;
    let h = height as usize;

    // Number of rows available for the result list (rows 1..h-4)
    let result_rows = h.saturating_sub(4);
    let st = state.scroll_offset;

    execute!(stdout, cursor::Hide)?;

    // Row 0: header
    write_row(stdout, 0, HEADER, w, false)?;

    // Rows 1..=result_rows: results
    for screen_row in 0..result_rows {
        let result_idx = st + screen_row;
        let row = (screen_row + 1) as u16;
        execute!(stdout, cursor::MoveTo(0, row))?;
        execute!(stdout, terminal::Clear(ClearType::CurrentLine))?;
        if result_idx < state.results.len() {
            let is_sel = result_idx == state.selected_idx;
            let line = format_result_line(&state.results[result_idx], is_sel, w);
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
    let sep_row = h.saturating_sub(3) as u16;
    execute!(stdout, cursor::MoveTo(0, sep_row))?;
    execute!(stdout, terminal::Clear(ClearType::CurrentLine))?;

    // Row h-2: status line
    let status_row = h.saturating_sub(2) as u16;
    execute!(stdout, cursor::MoveTo(0, status_row))?;
    execute!(stdout, terminal::Clear(ClearType::CurrentLine))?;
    let status = format_status(state.results.len(), state.total);
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "{}", truncate_str(&status, w))?;
    execute!(stdout, SetForegroundColor(Color::Reset))?;

    // Row h-1: query input
    let query_row = h.saturating_sub(1) as u16;
    execute!(stdout, cursor::MoveTo(0, query_row))?;
    execute!(stdout, terminal::Clear(ClearType::CurrentLine))?;
    let prompt = format!("> {}", state.query);
    write!(stdout, "{}", truncate_str(&prompt, w))?;

    // Position cursor at end of query for typing feedback
    let cursor_col = (2 + state.query.chars().count()).min(w.saturating_sub(1)) as u16;
    execute!(stdout, cursor::MoveTo(cursor_col, query_row), cursor::Show)?;

    stdout.flush()?;
    Ok(())
}

fn write_row(
    stdout: &mut impl Write,
    row: u16,
    text: &str,
    width: usize,
    bold: bool,
) -> Result<()> {
    execute!(stdout, cursor::MoveTo(0, row))?;
    execute!(stdout, terminal::Clear(ClearType::CurrentLine))?;
    if bold {
        execute!(stdout, SetAttribute(Attribute::Bold))?;
    }
    write!(stdout, "{}", truncate_str(text, width))?;
    if bold {
        execute!(stdout, SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Line formatting helpers (also tested in unit tests below)
// ---------------------------------------------------------------------------

pub(crate) fn format_result_line(record: &ArtifactRecord, _is_selected: bool, width: usize) -> String {
    let marker = "  \u{25CF} "; // "  ● "
    let marker_width = 4;

    let version_part = if record.version.is_empty() {
        String::new()
    } else {
        format!("  {}", record.version)
    };
    let version_width = version_part.chars().count();

    let coord_width = record.coord.chars().count();
    let used = marker_width + coord_width + version_width;
    let meta_budget = width.saturating_sub(used + 2);

    let meta = if !record.name.is_empty() {
        record.name.as_str()
    } else {
        record.description.as_str()
    };

    let meta_part = if meta.is_empty() {
        String::new()
    } else {
        let chars: Vec<char> = meta.chars().collect();
        let display: String = if chars.len() > meta_budget {
            let take = meta_budget.saturating_sub(1);
            let s: String = chars[..take].iter().collect();
            format!("{}…", s)
        } else {
            meta.to_string()
        };
        format!("  {}", display)
    };

    let full = format!("{}{}{}{}", marker, record.coord, meta_part, version_part);
    truncate_str(&full, width)
}

pub(crate) fn format_status(matched: usize, total: u64) -> String {
    let total_fmt = format_number(total);
    format!("  {} / {} artifacts", matched, total_fmt)
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('\u{202F}'); // narrow no-break space (thousands separator)
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

pub(crate) fn truncate_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[..max_chars].iter().collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(coord: &str, name: &str, desc: &str, ver: &str) -> ArtifactRecord {
        ArtifactRecord {
            coord: coord.to_string(),
            name: name.to_string(),
            description: desc.to_string(),
            version: ver.to_string(),
        }
    }

    #[test]
    fn result_display_line_truncates_long_description() {
        let long_desc = "A".repeat(200);
        let r = make_record("com.example:foo", "", &long_desc, "1.0.0");
        let line = format_result_line(&r, false, 80);
        assert!(line.chars().count() <= 80, "line too wide: {} chars", line.chars().count());
    }

    #[test]
    fn result_display_line_shows_coord_and_version() {
        let r = make_record("com.google.guava:guava", "Guava", "", "33.0.0-jre");
        let line = format_result_line(&r, false, 120);
        assert!(line.contains("com.google.guava:guava"));
        assert!(line.contains("33.0.0-jre"));
    }

    #[test]
    fn result_display_line_fits_narrow_terminal() {
        let r = make_record("com.example:artifact", "Name", "Desc", "1.0");
        let line = format_result_line(&r, false, 40);
        assert!(line.chars().count() <= 40);
    }

    #[test]
    fn visible_range_keeps_selection_in_view_scrolling_down() {
        let mut state = UiState::new(100);
        state.results = (0..20)
            .map(|i| ArtifactRecord {
                coord: format!("com.example:lib{}", i),
                name: String::new(),
                description: String::new(),
                version: String::new(),
            })
            .collect();
        state.selected_idx = 15; // select beyond initial view
        state.adjust_scroll(10); // 10 visible rows
        assert!(state.scroll_offset <= 15);
        assert!(state.selected_idx < state.scroll_offset + 10);
    }

    #[test]
    fn visible_range_keeps_selection_in_view_scrolling_up() {
        let mut state = UiState::new(100);
        state.results = (0..20)
            .map(|i| ArtifactRecord {
                coord: format!("com.example:lib{}", i),
                name: String::new(),
                description: String::new(),
                version: String::new(),
            })
            .collect();
        state.scroll_offset = 10;
        state.selected_idx = 3; // scroll back up to item 3
        state.adjust_scroll(10);
        assert_eq!(state.scroll_offset, 3);
    }

    #[test]
    fn format_status_formats_large_numbers() {
        let s = format_status(42, 487_312);
        assert!(s.contains("42"));
        assert!(s.contains("487"));
    }

    #[test]
    fn truncate_str_within_limit_unchanged() {
        let s = "hello";
        assert_eq!(truncate_str(s, 10), s);
    }

    #[test]
    fn truncate_str_at_limit() {
        let s = "hello world";
        let t = truncate_str(s, 5);
        assert_eq!(t, "hello");
    }
}
