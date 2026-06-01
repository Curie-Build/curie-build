use std::io::IsTerminal as _;
use terminal_size::{terminal_size, Height, Width};

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
/// Queried via the `terminal_size` crate rather than the `COLUMNS` env var,
/// because `COLUMNS` is a shell-local variable that is usually *not* exported
/// to child processes — relying on it silently falls back to a default and
/// produces a too-wide value.  Returns `None` when stdout is not a terminal
/// (e.g. piped to a file) or the query fails.
pub(crate) fn width() -> Option<u16> {
    terminal_size().map(|(Width(w), _)| w).filter(|&w| w > 0)
}

/// The height (in rows) of the controlling terminal on stdout.
///
/// Returns `None` when stdout is not a terminal or the query fails.
pub(crate) fn height() -> Option<u16> {
    terminal_size().map(|(_, Height(h))| h).filter(|&h| h > 0)
}
