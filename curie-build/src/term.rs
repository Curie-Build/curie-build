//! Terminal capability helpers.
//!
//! Thin wrappers around [`crossterm`] so the rest of the codebase does not
//! import crossterm directly for these basic queries.

use std::io::IsTerminal as _;

/// Returns `true` when stdout is connected to a terminal.
///
/// Does **not** check `NO_COLOR` — use [`use_color`] for colour decisions.
/// Used by the TUI split-screen path, which activates solely based on TTY
/// state regardless of colour preferences.
pub(crate) fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Returns `true` when stdout is a terminal and `NO_COLOR` is not set.
pub(crate) fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    is_tty()
}

/// The width (in columns) of the controlling terminal on stdout.
///
/// Returns `None` when stdout is not a terminal or the query fails.
pub(crate) fn width() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(w, _)| w).filter(|&w| w > 0)
}

/// The height (in rows) of the controlling terminal on stdout.
///
/// Returns `None` when stdout is not a terminal or the query fails.
pub(crate) fn height() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(_, h)| h).filter(|&h| h > 0)
}
