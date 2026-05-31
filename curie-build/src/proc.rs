//! Subprocess spawning abstraction.
//!
//! [`spawn_cmd`] is a drop-in replacement for `cmd.status()` that routes the
//! child process's output through a PTY to the parallel output mux when a
//! per-thread sink is active, and falls back to the plain `Command::status()`
//! path otherwise.
//!
//! Call sites in `compile.rs` and `test.rs` swap
//!   `cmd.status().context("…")?`   →   `proc::spawn_cmd(&mut cmd).context("…")?`
//! and check `.success()` on the returned [`Status`], which has the same API.

use anyhow::{Context, Result};
use std::process::Command;

/// Opaque process-exit wrapper.  Only `.success()` is used by callers.
pub struct Status(bool);

impl Status {
    pub fn success(&self) -> bool {
        self.0
    }
}

/// Run `cmd`, routing its output to the per-thread parallel mux slot (via PTY)
/// when one is active, or running it normally otherwise.
pub fn spawn_cmd(cmd: &mut Command) -> Result<Status> {
    if let Some(slot) = crate::parallel::try_get_sink() {
        spawn_pty(cmd, &slot)
    } else {
        let s = cmd.status().context("command failed to start")?;
        Ok(Status(s.success()))
    }
}

// ── PTY path (Unix only) ──────────────────────────────────────────────────

#[cfg(unix)]
fn spawn_pty(
    cmd: &mut Command,
    slot: &std::sync::Arc<crate::parallel::MuxSlot>,
) -> Result<Status> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Read;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: terminal_cols(),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    // Build a portable-pty CommandBuilder from the std Command.
    let mut cb = CommandBuilder::new(cmd.get_program());
    for arg in cmd.get_args() {
        cb.arg(arg);
    }
    for (k, v) in cmd.get_envs() {
        match v {
            Some(val) => {
                cb.env(k, val);
            }
            None => {
                cb.env_remove(k);
            }
        }
    }
    // Always set CWD explicitly — portable-pty may not inherit the parent's
    // working directory on all platforms when none is given.
    let cwd = cmd
        .get_current_dir()
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    cb.cwd(&cwd);

    let mut child = pair
        .slave
        .spawn_command(cb)
        .context("failed to spawn command on PTY")?;
    // Release our slave handle so reading master gets EOF when the child exits.
    drop(pair.slave);

    // Read PTY master until EOF (slave closes when child exits).
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY master reader")?;

    let mut line_buf = String::new();
    let mut byte_buf = [0u8; 4096];
    loop {
        match reader.read(&mut byte_buf) {
            Ok(0) | Err(_) => break, // EOF or EIO (macOS)
            Ok(n) => {
                let bytes = &byte_buf[..n];
                slot.write_raw(bytes);
                // Split into lines and forward to the mux slot.
                for ch in String::from_utf8_lossy(bytes).chars() {
                    if ch == '\n' {
                        let line = std::mem::take(&mut line_buf);
                        let line = line.trim_end_matches('\r').to_string();
                        if !line.is_empty() {
                            slot.push_line(line);
                        }
                    } else if ch != '\r' {
                        line_buf.push(ch);
                    }
                }
            }
        }
    }
    if !line_buf.is_empty() {
        slot.push_line(line_buf);
    }

    let exit = child.wait().context("failed to wait for child process")?;
    Ok(Status(exit.success()))
}

// Non-Unix: no PTY support — fall back to normal spawn (sink was set but
// we can't honour it; this path only occurs in non-Unix builds).
#[cfg(not(unix))]
fn spawn_pty(
    cmd: &mut Command,
    _slot: &std::sync::Arc<crate::parallel::MuxSlot>,
) -> Result<Status> {
    let s = cmd.status().context("command failed to start")?;
    Ok(Status(s.success()))
}

fn terminal_cols() -> u16 {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(220)
}
