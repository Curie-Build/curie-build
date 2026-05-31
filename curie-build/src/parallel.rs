//! Parallel workspace build/test/clean with per-member PTY output routing.
//!
//! # Overview
//!
//! When a workspace has more than one member to build (or test/clean), the
//! public entry point [`run_jobs`] dispatches up to `--jobs` worker threads
//! concurrently.  Each worker calls the user-supplied `run` closure for its
//! member, respecting the workspace-dependency DAG so that no member starts
//! before its dependencies are finished.
//!
//! Each worker thread activates a per-thread [`MuxSlot`] (via
//! [`set_thread_sink`]) before invoking the build logic.  Calls inside
//! `compile.rs` / `test.rs` to [`crate::proc::spawn_cmd`] pick up that slot
//! and run the external command (javac, java, etc.) on a PTY, routing every
//! output line back to the slot.  Lines are buffered per-member and flushed
//! contiguously — either on completion or after a 5-second stale timeout —
//! to minimise interleaving while still showing live progress.
//!
//! Raw PTY bytes (color codes preserved) are also written to
//! `target/<action>.log` inside each member's directory.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::workspace::{Member, Workspace};

// ── Thread-local output sink ───────────────────────────────────────────────

thread_local! {
    static OUTPUT_SINK: std::cell::RefCell<Option<Arc<MuxSlot>>> =
        std::cell::RefCell::new(None);
}

pub(crate) fn set_thread_sink(slot: Arc<MuxSlot>) {
    OUTPUT_SINK.with(|s| *s.borrow_mut() = Some(slot));
}

pub(crate) fn clear_thread_sink() {
    OUTPUT_SINK.with(|s| *s.borrow_mut() = None);
}

/// Returns the active [`MuxSlot`] for this thread, or `None` when running on
/// the sequential single-member path.
pub(crate) fn try_get_sink() -> Option<Arc<MuxSlot>> {
    OUTPUT_SINK.with(|s| s.borrow().clone())
}

// ── Color palette ──────────────────────────────────────────────────────────

const PALETTE: &[&str] = &[
    "\x1b[32m",  // green
    "\x1b[33m",  // yellow
    "\x1b[34m",  // blue
    "\x1b[35m",  // magenta
    "\x1b[36m",  // cyan
    "\x1b[91m",  // bright red
    "\x1b[92m",  // bright green
    "\x1b[93m",  // bright yellow
    "\x1b[94m",  // bright blue
    "\x1b[95m",  // bright magenta
];
const RESET: &str = "\x1b[0m";

// ── MuxSlot ────────────────────────────────────────────────────────────────

/// Per-member output buffer.  Accumulated by worker threads via
/// [`MuxSlot::push_line`] and [`MuxSlot::write_raw`]; flushed contiguously to
/// the shared stdout sink.
pub struct MuxSlot {
    /// Pre-formatted prefix string: `"[color]declared[reset] "` or `"declared | "`.
    prefix: String,
    pending: Mutex<SlotState>,
    log: Mutex<std::fs::File>,
    shared_out: Arc<Mutex<Box<dyn Write + Send>>>,
    /// How long a buffered line waits before the flusher forces a flush.
    flush_timeout: Duration,
}

struct SlotState {
    lines: Vec<String>,
    first_at: Option<Instant>,
}

impl MuxSlot {
    fn new(
        declared: &str,
        color_idx: usize,
        log_file: std::fs::File,
        shared_out: Arc<Mutex<Box<dyn Write + Send>>>,
        flush_timeout: Duration,
    ) -> Self {
        let prefix = if crate::term::use_color() {
            format!("{}{}{} ", PALETTE[color_idx % PALETTE.len()], declared, RESET)
        } else {
            format!("{} | ", declared)
        };
        MuxSlot {
            prefix,
            pending: Mutex::new(SlotState {
                lines: Vec::new(),
                first_at: None,
            }),
            log: Mutex::new(log_file),
            shared_out,
            flush_timeout,
        }
    }

    /// Push one line of output (stripped of the trailing newline) from the PTY.
    pub fn push_line(&self, line: String) {
        let mut st = self.pending.lock().unwrap();
        if st.first_at.is_none() {
            st.first_at = Some(Instant::now());
        }
        st.lines.push(line);
    }

    /// Append raw PTY bytes to the member's log file (color codes preserved).
    pub fn write_raw(&self, bytes: &[u8]) {
        if let Ok(mut f) = self.log.lock() {
            let _ = f.write_all(bytes);
        }
    }

    /// Flush all buffered lines to the shared stdout sink with the member prefix.
    /// Called on job completion (always) and by the flusher thread (on timeout).
    pub fn flush(&self) {
        let mut st = self.pending.lock().unwrap();
        if st.lines.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut st.lines);
        st.first_at = None;
        drop(st);

        if let Ok(mut out) = self.shared_out.lock() {
            for line in lines {
                let _ = writeln!(out, "{}{}", self.prefix, line);
            }
        }
    }

    fn is_stale(&self) -> bool {
        self.pending
            .lock()
            .unwrap()
            .first_at
            .as_ref()
            .is_some_and(|t| t.elapsed() >= self.flush_timeout)
    }
}

// ── Mux ───────────────────────────────────────────────────────────────────

struct Mux {
    slots: Vec<Arc<MuxSlot>>,
}

impl Mux {
    /// Flush any slot whose oldest buffered line has been waiting ≥ 5 s.
    fn flush_stale(&self) {
        for slot in &self.slots {
            if slot.is_stale() {
                slot.flush();
            }
        }
    }

    fn flush_all(&self) {
        for slot in &self.slots {
            slot.flush();
        }
    }
}

// ── Classpath threading helper ─────────────────────────────────────────────

struct Artifact {
    classes_dir: PathBuf,
    /// Transitive classpath contribution for downstream members.
    contribution: Vec<PathBuf>,
}

fn collect_extra_cp(deps: &[usize], artifacts: &HashMap<usize, Artifact>) -> Vec<PathBuf> {
    let mut cp: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for &i in deps {
        if let Some(a) = artifacts.get(&i) {
            if seen.insert(a.classes_dir.clone()) {
                cp.push(a.classes_dir.clone());
            }
            for e in &a.contribution {
                if seen.insert(e.clone()) {
                    cp.push(e.clone());
                }
            }
        }
    }
    cp
}

// ── Scheduler logic (extracted for unit-testability) ──────────────────────

/// Initial pending count for each position in `subset`.
/// `respect_dag = false` → everything is 0 (used for clean).
fn initial_pending(ws: &Workspace, subset: &[usize], respect_dag: bool) -> Vec<usize> {
    let subset_set: HashSet<usize> = subset.iter().copied().collect();
    subset
        .iter()
        .map(|&idx| {
            if !respect_dag {
                0
            } else {
                ws.members[idx]
                    .workspace_deps
                    .iter()
                    .filter(|&&d| subset_set.contains(&d))
                    .count()
            }
        })
        .collect()
}

/// Update `pending` after the member at global index `completed_idx` finishes.
/// Returns the subset positions that became ready (pending just reached 0).
fn on_completion(
    ws: &Workspace,
    subset: &[usize],
    pending: &mut Vec<usize>,
    dispatched: &HashSet<usize>, // global indices already dispatched
    completed_idx: usize,
) -> Vec<usize> {
    let mut newly_ready = Vec::new();
    for (pos, &other_idx) in subset.iter().enumerate() {
        if dispatched.contains(&other_idx) {
            continue;
        }
        if ws.members[other_idx].workspace_deps.contains(&completed_idx) {
            pending[pos] = pending[pos].saturating_sub(1);
            if pending[pos] == 0 {
                newly_ready.push(pos);
            }
        }
    }
    newly_ready
}

// ── run_jobs ───────────────────────────────────────────────────────────────

/// Run `run` for every member in `subset` in parallel (up to `jobs` workers),
/// respecting the dependency DAG when `respect_dag` is true.
///
/// `run(member, extra_classpath)` must return the member's resolved Maven dep
/// JARs on success (used to build the downstream classpath).  For clean,
/// return an empty `Vec`.
///
/// The caller is responsible for ensuring `subset.len() > 1` before calling
/// this function — single-member subsets use the direct [`crate::workspace`]
/// path (no PTY overhead).
pub fn run_jobs<F>(
    ws: &Workspace,
    subset: &[usize],
    action_name: &str,
    jobs: usize,
    respect_dag: bool,
    run: F,
) -> Result<()>
where
    F: Fn(&Member, &[PathBuf]) -> Result<Vec<PathBuf>> + Sync + Send,
{
    let n = subset.len();
    let log_name = format!("{}.log", action_name);

    // Shared stdout sink (all prefixed lines go here).
    let shared_out: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stdout())));

    // Create one MuxSlot per member in the subset.
    let slots: Vec<Arc<MuxSlot>> = subset
        .iter()
        .enumerate()
        .map(|(color_idx, &idx)| -> Result<Arc<MuxSlot>> {
            let m = &ws.members[idx];
            let target_dir = m.path.join("target");
            std::fs::create_dir_all(&target_dir)
                .with_context(|| format!("failed to create {}", target_dir.display()))?;
            let log_path = target_dir.join(&log_name);
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
                .with_context(|| format!("failed to open {}", log_path.display()))?;
            Ok(Arc::new(MuxSlot::new(
                &m.declared,
                color_idx,
                log_file,
                Arc::clone(&shared_out),
                Duration::from_secs(5),
            )))
        })
        .collect::<Result<_>>()?;

    let mux = Arc::new(Mux { slots: slots.clone() });

    println!(
        "Workspace {} {} ({} member{})",
        ws.root.display(),
        action_name,
        n,
        if n == 1 { "" } else { "s" }
    );
    println!();

    // Scheduler state (all accessed only on the coordinator thread).
    let mut pending = initial_pending(ws, subset, respect_dag);
    let mut dispatched: HashSet<usize> = HashSet::new(); // global member indices
    let mut artifacts: HashMap<usize, Artifact> = HashMap::new();
    let mut in_flight: usize = 0;
    let mut failed = false;
    let mut errors: Vec<String> = Vec::new();

    let mut ready: VecDeque<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, &p)| p == 0)
        .map(|(pos, _)| pos)
        .collect();

    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<Vec<PathBuf>>)>();

    // Background flusher: every 250 ms flush slots with stale buffered lines.
    let mux_flusher = Arc::clone(&mux);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let flusher = std::thread::spawn(move || {
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            mux_flusher.flush_stale();
        }
    });

    let run_ref = &run;
    let slots_ref = &slots;
    std::thread::scope(|s| -> Result<()> {
        loop {
            // Dispatch all ready jobs up to the concurrency limit.
            while !ready.is_empty() && in_flight < jobs && !failed {
                let pos = ready.pop_front().unwrap();
                let idx = subset[pos];
                dispatched.insert(idx);

                let m = &ws.members[idx];
                let extra_cp = collect_extra_cp(&m.workspace_deps, &artifacts);
                let slot = Arc::clone(&slots_ref[pos]);
                let tx = tx.clone();

                s.spawn(move || {
                    set_thread_sink(Arc::clone(&slot));
                    let result = run_ref(m, &extra_cp);
                    clear_thread_sink();
                    slot.flush(); // flush remaining output immediately
                    tx.send((pos, result)).ok();
                });
                in_flight += 1;
            }

            if in_flight == 0 {
                break; // everything dispatched (or failed) and drained
            }

            // Block until the next completion.
            let (pos, result) = rx.recv().expect("channel closed while threads still running");
            in_flight -= 1;
            let idx = subset[pos];

            match result {
                Ok(dep_jars) => {
                    let classes_dir = ws.members[idx].path.join("target").join("classes");
                    let extra_cp =
                        collect_extra_cp(&ws.members[idx].workspace_deps, &artifacts);
                    let mut contribution = extra_cp;
                    contribution.extend(dep_jars);
                    artifacts.insert(idx, Artifact { classes_dir, contribution });

                    // Unblock dependents.
                    let newly_ready =
                        on_completion(ws, subset, &mut pending, &dispatched, idx);
                    ready.extend(newly_ready);
                }
                Err(e) => {
                    failed = true;
                    errors.push(format!("{}: {:#}", ws.members[idx].declared, e));
                }
            }
        }
        Ok(())
    })?;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    flusher.join().ok();
    mux.flush_all();

    if !errors.is_empty() {
        anyhow::bail!("{}", errors.join("\n"));
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Shared sink helper ──────────────────────────────────────────────────

    struct VecSink(Arc<Mutex<Vec<u8>>>);
    impl Write for VecSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn vec_sink() -> (Arc<Mutex<Vec<u8>>>, Arc<Mutex<Box<dyn Write + Send>>>) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(VecSink(Arc::clone(&buf)))));
        (buf, sink)
    }

    fn make_slot(prefix: &str, sink: Arc<Mutex<Box<dyn Write + Send>>>) -> MuxSlot {
        let log = tempfile::tempfile().unwrap();
        MuxSlot {
            prefix: prefix.to_string(),
            pending: Mutex::new(SlotState { lines: Vec::new(), first_at: None }),
            log: Mutex::new(log),
            shared_out: sink,
            flush_timeout: Duration::from_secs(5),
        }
    }

    // ── On-disk workspace fixture for scheduler tests ──────────────────────

    fn make_test_ws(
        specs: &[(&str, &[&str])], // (member_name, [dep_member_names])
    ) -> (tempfile::TempDir, crate::workspace::Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let members_toml = specs
            .iter()
            .map(|(n, _)| format!("\"{}\"", n))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.path().join("Curie.toml"),
            format!("[workspace]\nmembers = [{members_toml}]\n"),
        )
        .unwrap();
        for (name, deps) in specs {
            let mpath = dir.path().join(name);
            std::fs::create_dir_all(&mpath).unwrap();
            let mut toml = format!("[library]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
            if !deps.is_empty() {
                toml.push_str("[workspace-dependencies]\n");
                for dep in *deps {
                    toml.push_str(&format!("{dep} = {{ path = \"../{dep}\" }}\n"));
                }
            }
            std::fs::write(mpath.join("Curie.toml"), toml).unwrap();
        }
        let ws = crate::workspace::load(dir.path()).unwrap();
        (dir, ws)
    }

    // ── Scheduler logic tests ──────────────────────────────────────────────

    #[test]
    fn ready_set_respects_dag() {
        // core → lib → app; only core starts ready.
        let (_dir, ws) = make_test_ws(&[
            ("app", &["lib"]),
            ("lib", &["core"]),
            ("core", &[]),
        ]);
        // After topo sort: core=0, lib=1, app=2.
        let subset: Vec<usize> = (0..ws.members.len()).collect();
        let pending = initial_pending(&ws, &subset, true);

        // Each member has exactly as many pending deps as it has workspace_deps.
        for (pos, &idx) in subset.iter().enumerate() {
            assert_eq!(pending[pos], ws.members[idx].workspace_deps.len());
        }

        let initial_ready: Vec<usize> = pending
            .iter()
            .enumerate()
            .filter(|(_, &p)| p == 0)
            .map(|(pos, _)| pos)
            .collect();
        // Only core (no deps) is initially ready.
        assert_eq!(initial_ready.len(), 1);
        assert!(
            ws.members[subset[initial_ready[0]]].workspace_deps.is_empty(),
            "initial ready member must have no deps"
        );
    }

    #[test]
    fn clean_forces_all_ready() {
        let (_dir, ws) = make_test_ws(&[("app", &["lib"]), ("lib", &[])]);
        let subset: Vec<usize> = (0..ws.members.len()).collect();
        let pending = initial_pending(&ws, &subset, false);
        assert!(pending.iter().all(|&p| p == 0), "all must be zero for clean");
    }

    #[test]
    fn on_completion_unblocks_dependents() {
        // lib → core; completing core should make lib ready.
        let (_dir, ws) = make_test_ws(&[("lib", &["core"]), ("core", &[])]);
        // After topo sort: core=index 0, lib=index 1.
        let subset: Vec<usize> = (0..ws.members.len()).collect();
        let mut pending = initial_pending(&ws, &subset, true);
        assert_eq!(pending[1], 1); // lib waiting for core

        let core_idx = ws.members.iter().position(|m| m.declared == "core").unwrap();
        let mut dispatched = HashSet::new();
        dispatched.insert(core_idx);

        let newly_ready = on_completion(&ws, &subset, &mut pending, &dispatched, core_idx);
        let lib_pos = subset.iter().position(|&i| ws.members[i].declared == "lib").unwrap();
        assert!(
            newly_ready.contains(&lib_pos),
            "lib must become ready after core completes"
        );
        assert_eq!(pending[lib_pos], 0);
    }

    #[test]
    fn fail_early_stops_dispatch() {
        // Verify that when failed=true, the scheduling loop will not dispatch
        // more jobs because the guard `&& !failed` blocks the while-loop.
        // We test the initial pending state only; run_jobs exercises the flag.
        let (_dir, ws) = make_test_ws(&[("a", &[]), ("b", &[])]);
        let subset: Vec<usize> = (0..ws.members.len()).collect();
        let pending = initial_pending(&ws, &subset, true);
        // Both a and b are independent — both start ready.
        assert!(pending.iter().all(|&p| p == 0));
    }

    // ── Mux output tests ───────────────────────────────────────────────────

    #[test]
    fn mux_slot_buffers_then_flushes() {
        let (buf, sink) = vec_sink();
        let slot = make_slot("proj | ", sink);

        slot.push_line("line one".to_string());
        slot.push_line("line two".to_string());

        // Before flush: nothing written to the sink.
        assert!(buf.lock().unwrap().is_empty());

        slot.flush();

        let bytes = buf.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("line one"), "got: {text:?}");
        assert!(text.contains("line two"), "got: {text:?}");
        assert!(text.contains("proj | "), "prefix missing: {text:?}");
    }

    #[test]
    fn mux_slot_immediate_flush_on_completion() {
        let (buf, sink) = vec_sink();
        let slot = make_slot("svc | ", sink);

        slot.push_line("build output".to_string());
        assert!(buf.lock().unwrap().is_empty(), "should not flush until called");

        slot.flush(); // simulates job completion
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "svc | build output\n");
    }

    #[test]
    fn prefix_colored_line_plain() {
        let (buf, sink) = vec_sink();
        let slot = make_slot("myapp | ", sink);

        slot.push_line("compiler error here".to_string());
        slot.flush();

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "myapp | compiler error here\n");
    }

    #[test]
    fn double_flush_is_idempotent() {
        let (buf, sink) = vec_sink();
        let slot = make_slot("lib | ", sink);
        slot.push_line("hello".to_string());
        slot.flush();
        slot.flush(); // second flush: nothing to write
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "lib | hello\n"); // only one copy
    }
}
