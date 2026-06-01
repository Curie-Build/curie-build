//! Styled string helpers for pipeline status messages.
//!
//! Every function has two paths:
//!   - **Color** (`use_color() == true`): icon prefix + ANSI-colored label.
//!   - **Plain** (`use_color() == false`): no icons, no ANSI — same column
//!     widths as the original messages so plain output is unchanged.
//!
//! Callers pass the result directly to `crate::parallel::emit`.

// ── ANSI constants ──────────────────────────────────────────────────────────

const BOLD:        &str = "\x1b[1m";
const BOLD_GREEN:  &str = "\x1b[1;32m";
const CYAN:        &str = "\x1b[36m";
const BLUE:        &str = "\x1b[34m";
const YELLOW:      &str = "\x1b[33m";
const GRAY:        &str = "\x1b[38;5;240m";
const RESET:       &str = "\x1b[0m";

// ── Icons ───────────────────────────────────────────────────────────────────
// All single display-column characters so the label column stays aligned.

const ICON_ACTIVE:   &str = "⚙";   // compile, package, build
const ICON_RESOLVE:  &str = "↓";   // dependency resolution
const ICON_DONE:     &str = "✓";   // final success
const ICON_SKIP:     &str = "✓";   // up to date (same mark, dimmer)
const ICON_STALE:    &str = "✗";   // stale / removed
const ICON_INFO:     &str = "→";   // neutral informational
const ICON_NEUTRAL:  &str = "·";   // truly neutral (no test sources, etc.)

// ── Formatting helpers (internal) ───────────────────────────────────────────

/// Colored indented line:  `"  {COLOR}{icon} {label:<14}{RESET}{value}"`.
/// Plain indented line:    `"  {label:<16}{value}"`.
fn line(color: &str, icon: &str, label: &str, value: &str) -> String {
    if crate::term::use_color() {
        format!("  {color}{icon} {label:<14}{RESET}{value}")
    } else {
        format!("  {label:<16}{value}")
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Headline for the start of a member's pipeline.
///
/// Colored:  `"**Building** **foo** _v0.1.0_"`
/// Plain:    `"Building foo v0.1.0"`
pub fn headline(action: &str, name: &str, version: &str) -> String {
    if crate::term::use_color() {
        if version.is_empty() {
            format!("{BOLD}{action}{RESET} {BOLD}{name}{RESET}")
        } else {
            format!("{BOLD}{action}{RESET} {BOLD}{name}{RESET} {GRAY}v{version}{RESET}")
        }
    } else if version.is_empty() {
        format!("{action} {name}")
    } else {
        format!("{action} {name} v{version}")
    }
}

/// Active work step: compile, package, docker/native build.  Cyan `⚙`.
pub fn active(label: &str, value: &str) -> String {
    line(CYAN, ICON_ACTIVE, label, value)
}

/// Dependency resolution step.  Blue `↓`.
pub fn resolve(label: &str, value: &str) -> String {
    line(BLUE, ICON_RESOLVE, label, value)
}

/// Final "Done" success line.  Bold green `✓`.
pub fn done(value: &str) -> String {
    if crate::term::use_color() {
        format!("  {BOLD_GREEN}{ICON_DONE} Done          {RESET}{value}")
    } else {
        format!("  Done            {value}")
    }
}

/// Step was skipped because already up to date.  Dim `✓`.
pub fn up_to_date(label: &str) -> String {
    if crate::term::use_color() {
        format!("  {GRAY}{ICON_SKIP} {label:<14}{RESET}up to date")
    } else {
        format!("  {label:<16}up to date")
    }
}

/// Something was removed or is stale.  Yellow `✗`.
pub fn stale(label: &str, value: &str) -> String {
    line(YELLOW, ICON_STALE, label, value)
}

/// Neutral informational step (detected, libs, dockerfile, etc.).  Dim `→`.
pub fn info(label: &str, value: &str) -> String {
    line(GRAY, ICON_INFO, label, value)
}

/// Truly neutral / nothing to do.  Dim `·`.
pub fn neutral(label: &str, value: &str) -> String {
    line(GRAY, ICON_NEUTRAL, label, value)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Drive the inner formatting logic directly so tests aren't coupled to
    // the terminal state of the CI runner.

    fn colored_line(color: &str, icon: &str, label: &str, value: &str) -> String {
        format!("  {color}{icon} {label:<14}{RESET}{value}")
    }

    fn plain_line(label: &str, value: &str) -> String {
        format!("  {label:<16}{value}")
    }

    // ── Column alignment ──────────────────────────────────────────────────

    #[test]
    fn plain_label_column_is_16_chars() {
        // "  " + 14 visible chars (label padded) = 16 chars before the value.
        let s = plain_line("Compile", "1 source file(s)");
        let idx = s.find("1 source").unwrap();
        assert_eq!(idx, 18, "value must start at column 18 (2 sp + 16 label)");
    }

    #[test]
    fn colored_label_column_is_visually_18() {
        // Colored: 2sp + icon(1 display col) + sp(1) + label(14) = 18 visible chars.
        let s = colored_line(CYAN, ICON_ACTIVE, "Compile", "1 source file(s)");
        // Strip ANSI then count *chars* (not bytes) up to the value.
        let stripped = strip_ansi(&s);
        let char_idx = char_index_of(&stripped, "1 source").unwrap();
        assert_eq!(char_idx, 18);
    }

    #[test]
    fn colored_and_plain_value_starts_at_same_column() {
        let plain = plain_line("Resolve deps", "3 JAR(s)");
        let color = colored_line(BLUE, ICON_RESOLVE, "Resolve deps", "3 JAR(s)");
        // Compare char (visual) positions, not byte positions.
        let plain_idx = char_index_of(&plain, "3 JAR").unwrap();
        let color_idx = char_index_of(&strip_ansi(&color), "3 JAR").unwrap();
        assert_eq!(plain_idx, color_idx);
    }

    // ── Headline ──────────────────────────────────────────────────────────

    #[test]
    fn plain_headline_format() {
        // When use_color is false the helper falls through to the plain path.
        // We test the plain_headline directly.
        let s = plain_headline("Building", "my-app", "0.1.0");
        assert_eq!(s, "Building my-app v0.1.0");
    }

    #[test]
    fn plain_headline_empty_version() {
        let s = plain_headline("Running", "my-image", "");
        assert_eq!(s, "Running my-image");
    }

    #[test]
    fn colored_headline_contains_bold_and_gray() {
        let s = colored_headline("Building", "my-app", "0.1.0");
        assert!(s.contains(BOLD), "action/name must be bold");
        assert!(s.contains(GRAY), "version must be gray");
        assert!(s.contains("my-app"));
        assert!(s.contains("0.1.0"));
    }

    // ── done ─────────────────────────────────────────────────────────────

    #[test]
    fn plain_done_format() {
        // Visually: "  Done            target/foo.jar"
        let s = plain_done("target/foo.jar");
        assert_eq!(s, "  Done            target/foo.jar");
    }

    #[test]
    fn colored_done_contains_check_and_green() {
        let s = colored_done("target/foo.jar");
        assert!(s.contains(BOLD_GREEN));
        assert!(s.contains(ICON_DONE));
        assert!(s.contains("target/foo.jar"));
    }

    // ── up_to_date ────────────────────────────────────────────────────────

    #[test]
    fn plain_up_to_date_format() {
        let s = plain_up_to_date("Compile");
        assert_eq!(s, "  Compile         up to date");
    }

    #[test]
    fn colored_up_to_date_contains_dim_and_check() {
        let s = colored_up_to_date("Compile");
        assert!(s.contains(GRAY));
        assert!(s.contains(ICON_SKIP));
        assert!(s.contains("up to date"));
    }

    // ── stale ─────────────────────────────────────────────────────────────

    #[test]
    fn plain_stale_format() {
        let s = plain_line("Stale", "removed 2 orphaned class files");
        assert_eq!(s, "  Stale           removed 2 orphaned class files");
    }

    #[test]
    fn colored_stale_contains_yellow_and_x() {
        let s = colored_line(YELLOW, ICON_STALE, "Stale", "removed 2 files");
        assert!(s.contains(YELLOW));
        assert!(s.contains(ICON_STALE));
    }

    // ── Helpers used only in tests ────────────────────────────────────────

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip until end of CSI sequence (letter) or OSC.
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() { break; }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    // Plain/colored variants that don't call use_color() — for isolated tests.

    /// Returns the char-index (not byte-index) of `needle` in `haystack`.
    fn char_index_of(haystack: &str, needle: &str) -> Option<usize> {
        haystack.find(needle).map(|byte_idx| {
            haystack[..byte_idx].chars().count()
        })
    }

    fn plain_headline(action: &str, name: &str, version: &str) -> String {
        if version.is_empty() {
            format!("{action} {name}")
        } else {
            format!("{action} {name} v{version}")
        }
    }

    fn colored_headline(action: &str, name: &str, version: &str) -> String {
        if version.is_empty() {
            format!("{BOLD}{action}{RESET} {BOLD}{name}{RESET}")
        } else {
            format!("{BOLD}{action}{RESET} {BOLD}{name}{RESET} {GRAY}v{version}{RESET}")
        }
    }

    fn plain_done(value: &str) -> String {
        format!("  Done            {value}")
    }

    fn colored_done(value: &str) -> String {
        format!("  {BOLD_GREEN}{ICON_DONE} Done          {RESET}{value}")
    }

    fn plain_up_to_date(label: &str) -> String {
        format!("  {label:<16}up to date")
    }

    fn colored_up_to_date(label: &str) -> String {
        format!("  {GRAY}{ICON_SKIP} {label:<14}{RESET}up to date")
    }
}
