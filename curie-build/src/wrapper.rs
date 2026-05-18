//! Embedded javac wrapper.
//!
//! At compile time (`build.rs`), `javac-wrapper/Wrapper.java` is compiled
//! and packaged into `OUT_DIR/wrapper.jar`.  The JAR's bytes are
//! [`include_bytes!`]-ed into the Curie binary so end users don't need
//! to bootstrap anything.
//!
//! At runtime, [`ensure`] extracts the JAR to a cache directory the
//! first time it's needed.  Subsequent invocations reuse the cached file.
//! The cache key combines Curie's package version and the JAR's sha256
//! prefix, so a rebuilt Curie always invalidates a previous extraction.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// The compiled wrapper JAR, embedded at build time.
const WRAPPER_JAR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wrapper.jar"));

include!(concat!(env!("OUT_DIR"), "/wrapper_sha8.rs"));
// Brings `pub const WRAPPER_JAR_SHA8: &str = "..."` into scope.

/// Extract the embedded wrapper JAR to a stable cache path and return it.
/// Subsequent calls reuse the existing file (no rewrite, no checksum).
pub fn ensure() -> Result<PathBuf> {
    let cache = dirs::cache_dir()
        .context("could not determine user cache directory")?
        .join("curie");
    let path = cache.join(format!(
        "wrapper-{}-{}.jar",
        env!("CARGO_PKG_VERSION"),
        WRAPPER_JAR_SHA8,
    ));

    if path.exists() {
        return Ok(path);
    }

    std::fs::create_dir_all(&cache)
        .with_context(|| format!("failed to create {}", cache.display()))?;

    // Atomic write: stage at .part and rename so a crashed extraction
    // can't leave a half-written wrapper in the cache.
    let tmp = path.with_extension("jar.part");
    std::fs::write(&tmp, WRAPPER_JAR)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename {} → {}", tmp.display(), path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_jar_is_nonempty() {
        // Sanity check that build.rs actually produced a JAR.
        assert!(WRAPPER_JAR.len() > 256, "embedded wrapper.jar suspiciously small");
        // Standard ZIP/JAR magic: 'P' 'K' 0x03 0x04.
        assert_eq!(&WRAPPER_JAR[..4], b"PK\x03\x04", "not a valid ZIP/JAR header");
    }

    #[test]
    fn ensure_returns_existing_path_on_second_call() {
        let first = ensure().unwrap();
        let second = ensure().unwrap();
        assert_eq!(first, second);
        assert!(first.exists());
    }
}
