//! Shared test-only helpers for environment isolation.
//!
//! `HOME` is a process-global variable, and cargo runs tests in parallel within
//! a single test binary.  Any test that overrides `HOME` must therefore
//! serialize against *every* other such test — across all modules — or the
//! override of one test leaks into another and artifact-cache paths resolve to
//! the wrong directory (the source of the flaky `download_artifact_*` tests).
//!
//! A single process-wide lock here provides that serialization; [`set_home`]
//! returns an RAII guard that holds the lock and restores the previous `HOME`
//! on drop, even if the test panics.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that holds the global HOME lock and restores the previous `HOME`
/// value when dropped.  Keep it alive for as long as the override is needed:
/// `let _home = testenv::set_home(dir.path());`.
pub struct HomeGuard {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            // SAFETY: the HOME lock is held for the lifetime of this guard, so
            // no other test thread reads or writes HOME concurrently.
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

/// Override `HOME` to `home` for the lifetime of the returned guard,
/// serialized against all other HOME overrides in this binary.
///
/// Lock poisoning is ignored: a panicking test still restores HOME via its own
/// guard's `Drop`, so the lock state stays usable for subsequent tests.
pub fn set_home(home: &Path) -> HomeGuard {
    let lock = HOME_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let prev = std::env::var("HOME").ok();
    // SAFETY: we hold the HOME lock for the lifetime of the returned guard.
    unsafe {
        std::env::set_var("HOME", home);
    }
    HomeGuard { _lock: lock, prev }
}
