//! Interactive TUI for selecting a dependency version during `curie add`.
//!
//! Layout (0-indexed rows):
//! ```text
//!  0       │  select version for <coord>    ↑↓ navigate    Enter select    Esc cancel
//!  1..H-3  │  [version list]
//!  H-2     │
//!  H-1     │  <N versions — oldest shown last>
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

use crate::search_ui::truncate_str;
use crate::update::{is_stable, latest_stable};

const MARKER: &str = "  \u{25CF} ";
const MARKER_W: usize = 4;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The version the user confirmed in the picker.
pub enum VersionPick {
    /// Write `""` to Curie.toml — version is managed by a BOM.
    BomManaged,
    /// Write this explicit version string.
    Explicit(String),
}

// ---------------------------------------------------------------------------
// Internal entry model
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Entry {
    /// Text shown in the list (the version string or a BOM label).
    label: String,
    /// Short annotation shown right-aligned (empty if none).
    tag: &'static str,
    kind: EntryKind,
}

#[derive(Clone)]
enum EntryKind {
    Bom,
    Version(String),
}

fn build_entries(
    all_versions: &[String], // oldest → newest
    bom_version: Option<&str>,
) -> (Vec<Entry>, usize) {
    let stable = latest_stable(all_versions);

    let mut entries: Vec<Entry> = Vec::new();

    // BOM entry at the top when a BOM manages this artifact.
    if let Some(bv) = bom_version {
        entries.push(Entry {
            label: format!("\"\"  (BOM-managed: {})", bv),
            tag: "[BOM]",
            kind: EntryKind::Bom,
        });
    }

    // Explicit versions, newest first.
    for v in all_versions.iter().rev() {
        let tag: &'static str = if bom_version.is_none() && stable.as_deref() == Some(v.as_str()) {
            "[recommended]"
        } else {
            ""
        };
        entries.push(Entry { label: v.clone(), tag, kind: EntryKind::Version(v.clone()) });
    }

    // Choose preselected index.
    let preselected = if bom_version.is_some() {
        0 // BOM entry
    } else if let Some(sv) = &stable {
        entries
            .iter()
            .position(|e| matches!(&e.kind, EntryKind::Version(v) if v == sv))
            .unwrap_or(0)
    } else {
        0 // newest
    };

    (entries, preselected)
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

struct UiState {
    entries:      Vec<Entry>,
    selected_idx: usize,
    scroll_off:   usize,
    total:        usize,
}

impl UiState {
    fn new(entries: Vec<Entry>, preselected: usize) -> Self {
        let total = entries.len();
        UiState { entries, selected_idx: preselected, scroll_off: 0, total }
    }

    fn move_up(&mut self) {
        if self.selected_idx > 0 { self.selected_idx -= 1; }
    }

    fn move_down(&mut self) {
        if self.selected_idx + 1 < self.total { self.selected_idx += 1; }
    }

    fn adjust_scroll(&mut self, visible: usize) {
        if visible == 0 { return; }
        if self.selected_idx >= self.scroll_off + visible {
            self.scroll_off = self.selected_idx + 1 - visible;
        }
        if self.selected_idx < self.scroll_off {
            self.scroll_off = self.selected_idx;
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Open the interactive version picker.
///
/// * `all_versions` — versions in oldest-to-newest order (from `maven-metadata.xml`).
/// * `bom_version`  — the version string managed by the project's BOM, if any.
///
/// Returns the user's choice, or `None` if cancelled.
pub fn run_version_ui(
    coord: &str,
    all_versions: &[String],
    bom_version: Option<&str>,
) -> Result<Option<VersionPick>> {
    let (entries, preselected) = build_entries(all_versions, bom_version);

    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = run_ui_inner(coord, entries, preselected, &mut stdout);

    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();

    result
}

fn run_ui_inner(
    coord: &str,
    entries: Vec<Entry>,
    preselected: usize,
    stdout: &mut impl Write,
) -> Result<Option<VersionPick>> {
    let mut state = UiState::new(entries, preselected);

    // Make sure the preselected row is in view on first draw.
    let (_, h) = terminal::size().unwrap_or((80, 24));
    state.adjust_scroll((h as usize).saturating_sub(4));
    redraw(stdout, coord, &state)?;

    loop {
        let ev = crossterm::event::read()?;
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                match (code, modifiers) {
                    (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(None),

                    (KeyCode::Enter, _) => {
                        let pick = match &state.entries[state.selected_idx].kind {
                            EntryKind::Bom          => VersionPick::BomManaged,
                            EntryKind::Version(v)   => VersionPick::Explicit(v.clone()),
                        };
                        return Ok(Some(pick));
                    }

                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        state.move_up();
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        state.move_down();
                    }

                    _ => {}
                }
            }
            Event::Resize(..) => {}
            _ => {}
        }

        let (_, height) = terminal::size().unwrap_or((80, 24));
        state.adjust_scroll((height as usize).saturating_sub(4));
        redraw(stdout, coord, &state)?;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn redraw(stdout: &mut impl Write, coord: &str, state: &UiState) -> Result<()> {
    let (width, height) = terminal::size()?;
    let w = width as usize;
    let h = height as usize;
    let result_rows = h.saturating_sub(4);
    let st = state.scroll_off;

    execute!(stdout, cursor::Hide)?;

    // Row 0: header
    let header = format!(
        "  select version for {}    \u{2191}\u{2193} navigate    Enter select    Esc cancel",
        coord
    );
    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::CurrentLine))?;
    write!(stdout, "{}", truncate_str(&header, w))?;

    // Rows 1..result_rows: version list
    for screen_row in 0..result_rows {
        let idx = st + screen_row;
        execute!(
            stdout,
            cursor::MoveTo(0, (screen_row + 1) as u16),
            terminal::Clear(ClearType::CurrentLine)
        )?;
        if idx < state.entries.len() {
            let entry = &state.entries[idx];
            let is_sel = idx == state.selected_idx;
            let line = format_entry(entry, is_sel, w);
            if is_sel {
                execute!(stdout, SetAttribute(Attribute::Reverse))?;
                write!(stdout, "{}", line)?;
                execute!(stdout, SetAttribute(Attribute::Reset))?;
            } else {
                // Dim non-stable versions slightly so stable ones stand out.
                let is_unstable = matches!(&entry.kind, EntryKind::Version(v) if !is_stable(v));
                if is_unstable {
                    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
                }
                write!(stdout, "{}", line)?;
                if is_unstable {
                    execute!(stdout, SetForegroundColor(Color::Reset))?;
                }
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
    let status = format!("  {} versions", state.total);
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "{}", truncate_str(&status, w))?;
    execute!(stdout, SetForegroundColor(Color::Reset))?;

    // Row h-1: blank
    execute!(
        stdout,
        cursor::MoveTo(0, h.saturating_sub(1) as u16),
        terminal::Clear(ClearType::CurrentLine)
    )?;

    stdout.flush()?;
    Ok(())
}

fn format_entry(entry: &Entry, _is_selected: bool, width: usize) -> String {
    let tag_part = if entry.tag.is_empty() {
        String::new()
    } else {
        format!("  {}", entry.tag)
    };
    let label_w = entry.label.chars().count();
    let tag_w   = tag_part.chars().count();
    let used    = MARKER_W + label_w + tag_w;
    // Pad between label and tag so tag is right-aligned, but cap at available width.
    let pad = width.saturating_sub(used);
    let full = format!("{}{}{}{}", MARKER, entry.label, " ".repeat(pad), tag_part);
    truncate_str(&full, width)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_entries_bom_first_then_newest() {
        let versions = vec!["1.0".to_string(), "1.1".to_string(), "2.0".to_string()];
        let (entries, preselected) = build_entries(&versions, Some("1.1"));
        assert_eq!(preselected, 0);
        assert!(matches!(entries[0].kind, EntryKind::Bom));
        // Second entry should be newest (2.0)
        assert!(matches!(&entries[1].kind, EntryKind::Version(v) if v == "2.0"));
    }

    #[test]
    fn build_entries_no_bom_preselects_stable() {
        let versions = vec!["1.0".to_string(), "2.0-RC1".to_string(), "1.5".to_string()];
        let (entries, preselected) = build_entries(&versions, None);
        // Latest stable is 1.5 (2.0-RC1 is not stable; 1.0 < 1.5)
        let selected_label = &entries[preselected].label;
        assert_eq!(selected_label, "1.5");
    }

    #[test]
    fn build_entries_no_stable_preselects_newest() {
        let versions = vec!["1.0-RC1".to_string(), "1.0-RC2".to_string()];
        let (entries, preselected) = build_entries(&versions, None);
        assert_eq!(preselected, 0);
        assert!(matches!(&entries[0].kind, EntryKind::Version(v) if v == "1.0-RC2"));
    }

    #[test]
    fn format_entry_fits_width() {
        let e = Entry { label: "3.0.0-RELEASE".to_string(), tag: "[recommended]", kind: EntryKind::Version("3.0.0-RELEASE".to_string()) };
        let line = format_entry(&e, false, 80);
        assert!(line.chars().count() <= 80);
        assert!(line.contains("3.0.0-RELEASE"));
        assert!(line.contains("[recommended]"));
    }
}
