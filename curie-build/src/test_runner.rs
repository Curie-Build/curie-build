use anyhow::{bail, Context, Result};
use crate::jar::build_manifest;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;

const RUNNER_SOURCE: &str = include_str!("../test-runner/CurieTestRunner.java");

#[cfg(windows)]
const CP_SEP: char = ';';
#[cfg(not(windows))]
const CP_SEP: char = ':';

/// Ensure the test runner JAR is compiled and cached; return its path.
///
/// The JAR is cached under `~/.cache/curie/test-runner-<hash>.jar` keyed by
/// the SHA-256 of the embedded Java source so a changed runner invalidates the
/// old cache entry automatically.
pub fn ensure_runner_jar(standalone_jar: &Path) -> Result<PathBuf> {
    let hash = source_hash();
    let cache = dirs::cache_dir()
        .context("could not determine user cache directory")?
        .join("curie");
    let jar_path = cache.join(format!("test-runner-{hash}.jar"));

    if jar_path.exists() {
        return Ok(jar_path);
    }

    std::fs::create_dir_all(&cache)
        .with_context(|| format!("failed to create {}", cache.display()))?;

    let tmp_dir = tempfile::tempdir().context("failed to create temp dir for runner compilation")?;
    let src_path = tmp_dir.path().join("CurieTestRunner.java");
    std::fs::write(&src_path, RUNNER_SOURCE)
        .context("failed to write CurieTestRunner.java to temp dir")?;

    let classes_dir = tmp_dir.path().join("classes");
    std::fs::create_dir_all(&classes_dir)
        .context("failed to create classes dir")?;

    let javac = find_javac().context("javac not found — Curie requires a JDK, not just a JRE")?;

    let status = Command::new(&javac)
        .arg("--release").arg("21")
        .arg("-cp").arg(standalone_jar)
        .arg("-d").arg(&classes_dir)
        .arg(&src_path)
        .status()
        .with_context(|| format!("failed to invoke {}", javac.display()))?;

    if !status.success() {
        bail!("javac failed compiling CurieTestRunner.java");
    }

    let tmp_jar = jar_path.with_extension("jar.part");
    create_jar(&classes_dir, &tmp_jar)
        .context("failed to create test-runner JAR")?;

    // Atomic rename so a crashed write can't leave a partial JAR.
    if let Err(e) = std::fs::rename(&tmp_jar, &jar_path) {
        if !jar_path.exists() {
            return Err(e).context("failed to move test-runner JAR to cache");
        }
        let _ = std::fs::remove_file(&tmp_jar);
    }

    Ok(jar_path)
}

/// Build a `Command` ready to run the test runner.
///
/// The caller should not modify the returned command further unless they need
/// to set environment variables; all positional arguments are already set.
pub fn build_runner_command(
    runner_jar: &Path,
    standalone_jar: &Path,
    test_classpath: &str,
    output_path: &Path,
    output_dir: &Path,
    include_classname: Option<&str>,
    enable_preview: bool,
    agent_jars: &[std::path::PathBuf],
    extra_jvm_args: &[String],
) -> Command {
    let classpath = format!(
        "{}{}{}{}{}",
        standalone_jar.display(), CP_SEP,
        runner_jar.display(), CP_SEP,
        test_classpath,
    );

    let mut cmd = Command::new("java");
    if enable_preview {
        cmd.arg("--enable-preview");
    }
    for agent in agent_jars {
        cmd.arg(format!("-javaagent:{}", agent.display()));
    }
    for arg in extra_jvm_args {
        cmd.arg(arg);
    }
    cmd.arg("-cp").arg(&classpath)
        .arg("com.curie.runner.CurieTestRunner")
        .arg(output_path)
        .arg(output_dir);

    if let Some(pattern) = include_classname {
        cmd.arg(format!("--include-classname={pattern}"));
    }

    cmd
}

fn source_hash() -> String {
    let digest = Sha256::digest(RUNNER_SOURCE.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..8].to_string()
}

fn find_javac() -> Option<PathBuf> {
    // Try: same directory as the `java` binary on PATH
    if let Ok(java) = which::which("java") {
        let candidate = java.parent().map(|p| p.join("javac"))?;
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Fallback: javac directly on PATH
    which::which("javac").ok()
}

/// Package all `.class` files under `classes_dir` into a JAR at `jar_path`.
fn create_jar(classes_dir: &Path, jar_path: &Path) -> Result<()> {
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("failed to create {}", jar_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Use the common manifest builder (standardizes on \r\n and ensures we
    // go through the single implementation used by every other JAR writer).
    zip.start_file("META-INF/MANIFEST.MF", options)?;
    zip.write_all(build_manifest(None, None, None).as_bytes())?;

    for entry in walkdir::WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let rel = entry
            .path()
            .strip_prefix(classes_dir)
            .expect("walkdir yields paths under classes_dir")
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(&rel, options)?;
        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        zip.write_all(&bytes)?;
    }

    zip.finish().context("failed to finalise runner JAR")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_source_is_non_empty() {
        assert!(!RUNNER_SOURCE.is_empty());
        assert!(RUNNER_SOURCE.contains("CurieTestRunner"));
        assert!(RUNNER_SOURCE.contains("TestExecutionListener"));
    }

    #[test]
    fn source_hash_is_eight_hex_chars() {
        let h = source_hash();
        assert_eq!(h.len(), 8, "hash must be 8 hex chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn classpath_separator_is_platform_correct() {
        #[cfg(windows)]
        assert_eq!(CP_SEP, ';');
        #[cfg(not(windows))]
        assert_eq!(CP_SEP, ':');
    }
}
