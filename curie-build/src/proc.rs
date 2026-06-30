//! Subprocess spawning abstraction.
//!
//! [`spawn_cmd`] is a drop-in replacement for `cmd.status()` that routes the
//! child process's output through a PTY to the parallel output mux when a
//! per-thread sink is active, and falls back to the plain `Command::status()`
//! path otherwise.
//!
//! [`spawn_cmd_with_stdin`] is the same idea for children that also need a
//! finite stdin payload (plugin `generate-sources` envelopes): stdout/stderr
//! still go through the mux when a sink is active (piped capture → line
//! forwarding, same `LineSink` as the PTY path), while stdin is always a
//! dedicated pipe with EOF after the payload.
//!
//! Call sites in `compile.rs` and `test.rs` swap
//!   `cmd.status().context("…")?`   →   `proc::spawn_cmd(&mut cmd).context("…")?`
//! and check `.success()` on the returned [`Status`], which has the same API.

use anyhow::{Context, Result};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

/// Opaque process-exit wrapper.
pub struct Status {
    success: bool,
    code: Option<i32>,
}

impl Status {
    pub fn success(&self) -> bool {
        self.success
    }

    /// Process exit code when known (Unix); `None` if terminated by signal etc.
    pub fn code(&self) -> Option<i32> {
        self.code
    }
}

fn status_from_exit(s: std::process::ExitStatus) -> Status {
    Status {
        success: s.success(),
        code: s.code(),
    }
}

/// Run `cmd`, routing its output to the per-thread parallel mux slot (via PTY)
/// when one is active, or running it normally otherwise.
pub fn spawn_cmd(cmd: &mut Command) -> Result<Status> {
    if let Some(sink) = crate::parallel::try_get_sink() {
        spawn_pty(cmd, &sink)
    } else {
        let s = cmd.status().context("command failed to start")?;
        Ok(status_from_exit(s))
    }
}

/// Run `cmd` with `stdin_data` written to the child's stdin (then EOF).
///
/// - **No sink (sequential):** stdout/stderr inherit the process terminal
///   (same UX as historical plugin generate-sources).
/// - **Sink active (parallel / TUI):** stdout and stderr are captured and
///   forwarded line-by-line to the [`crate::parallel::LineSink`] (same
///   destination as the PTY path used by [`spawn_cmd`]). Stdin remains a
///   normal pipe so the child sees EOF after the payload — required for
///   JSON envelopes; a PTY alone cannot provide that EOF reliably.
pub fn spawn_cmd_with_stdin(cmd: &mut Command, stdin_data: &[u8]) -> Result<Status> {
    if let Some(sink) = crate::parallel::try_get_sink() {
        spawn_with_stdin_captured(cmd, stdin_data, &sink)
    } else {
        spawn_with_stdin_inherit(cmd, stdin_data)
    }
}

// ── stdin + inherit (sequential) ───────────────────────────────────────────

fn spawn_with_stdin_inherit(cmd: &mut Command, stdin_data: &[u8]) -> Result<Status> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("command failed to start")?;
    let write_th = take_stdin_writer(&mut child, stdin_data);

    let status = child.wait().context("failed to wait for child process")?;
    let write_res = write_th.join().expect("stdin writer thread panicked");
    check_write_result_vs_status(write_res, status)?;
    Ok(status_from_exit(status))
}

// ── stdin + captured stdout/stderr → LineSink (parallel) ──────────────────

fn spawn_with_stdin_captured(
    cmd: &mut Command,
    stdin_data: &[u8],
    sink: &Arc<dyn crate::parallel::LineSink + Send + Sync>,
) -> Result<Status> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("command failed to start")?;
    let write_th = take_stdin_writer(&mut child, stdin_data);

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let sink_out = Arc::clone(sink);
    let sink_err = Arc::clone(sink);
    let out_th = thread::spawn(move || forward_lines(stdout, &sink_out));
    let err_th = thread::spawn(move || forward_lines(stderr, &sink_err));

    let status = child.wait().context("failed to wait for child process")?;
    let write_res = write_th.join().expect("stdin writer thread panicked");
    let _ = out_th.join();
    let _ = err_th.join();
    check_write_result_vs_status(write_res, status)?;
    Ok(status_from_exit(status))
}

fn take_stdin_writer(
    child: &mut std::process::Child,
    stdin_data: &[u8],
) -> thread::JoinHandle<io::Result<()>> {
    let mut stdin = child.stdin.take().expect("stdin was requested as piped");
    let data = stdin_data.to_vec();
    thread::spawn(move || {
        let res = stdin.write_all(&data);
        drop(stdin); // EOF
        res
    })
}

/// Prefer the child's exit status when it failed (real error is usually on
/// captured/inherited stderr); only surface stdin write errors if the child
/// reported success.
fn check_write_result_vs_status(
    write_res: io::Result<()>,
    status: std::process::ExitStatus,
) -> Result<()> {
    if let Err(e) = write_res {
        if status.success() {
            return Err(e).context("failed to write stdin to child process");
        }
        // Child failed; EPIPE on stdin write is expected — report status below.
    }
    if !status.success() {
        anyhow::bail!("command exited with status {:?}", status.code());
    }
    Ok(())
}

fn forward_lines(mut reader: impl Read, sink: &Arc<dyn crate::parallel::LineSink + Send + Sync>) {
    let mut line_buf = String::new();
    let mut byte_buf = [0u8; 4096];
    loop {
        match reader.read(&mut byte_buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for ch in String::from_utf8_lossy(&byte_buf[..n]).chars() {
                    if ch == '\n' {
                        let line = std::mem::take(&mut line_buf);
                        let line = line.trim_end_matches('\r').to_string();
                        if !line.is_empty() {
                            sink.push_line(line);
                        }
                    } else if ch != '\r' {
                        line_buf.push(ch);
                    }
                }
            }
        }
    }
    if !line_buf.is_empty() {
        sink.push_line(line_buf);
    }
}

// ── PTY path (Unix only) ──────────────────────────────────────────────────

#[cfg(unix)]
fn spawn_pty(
    cmd: &mut Command,
    sink: &Arc<dyn crate::parallel::LineSink + Send + Sync>,
) -> Result<Status> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pty_system = native_pty_system();
    // Report a narrower PTY width so child output doesn't wrap past the edge
    // of the terminal: subtract the prefix column ("name │ ") and keep a
    // floor of 40 so very-long prefixes don't produce unusably narrow output.
    let cols = terminal_cols()
        .saturating_sub(sink.prefix_visual_len() as u16)
        .max(40);
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols,
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
    // Override COLUMNS so tools that read it (docker, native-image progress
    // bars, etc.) also use the reduced content width rather than the parent's
    // full-terminal value.
    cb.env("COLUMNS", cols.to_string());

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

    forward_lines(&mut reader, sink);

    let exit = child.wait().context("failed to wait for child process")?;
    Ok(Status {
        success: exit.success(),
        code: exit_code_from_pty(&exit),
    })
}

#[cfg(unix)]
fn exit_code_from_pty(exit: &portable_pty::ExitStatus) -> Option<i32> {
    // portable-pty ExitStatus: success() only in older API; try code if available
    if exit.success() {
        Some(0)
    } else {
        // No stable .code() on all versions — leave unknown on failure
        None
    }
}

// Non-Unix: no PTY support — fall back to normal spawn (sink was set but
// we can't honour it; this path only occurs in non-Unix builds).
#[cfg(not(unix))]
fn spawn_pty(
    cmd: &mut Command,
    _sink: &Arc<dyn crate::parallel::LineSink + Send + Sync>,
) -> Result<Status> {
    let s = cmd.status().context("command failed to start")?;
    Ok(status_from_exit(s))
}

fn terminal_cols() -> u16 {
    // Prefer the real terminal width (TIOCGWINSZ); fall back to COLUMNS, then
    // to a sane default when stdout is not a TTY (e.g. piped to a file).
    crate::term::width()
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(120)
}
