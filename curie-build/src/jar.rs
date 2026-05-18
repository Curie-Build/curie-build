//! Deterministic JAR writing, manifest folding, and classpath helpers.
//!
//! All JAR entries are timestamped to a fixed reproducible-build epoch
//! (2024-01-01 UTC) and sorted lexicographically.  Identical inputs produce
//! byte-identical output.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC.
/// Matches SOURCE_DATE_EPOCH convention used by Debian, Nix, etc.
fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0)
        .expect("epoch constant is valid")
}

/// Build an OS-appropriate classpath string from a list of JAR paths.
pub fn classpath_string(jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    jars.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Build a properly folded `Class-Path` manifest header per the JAR spec.
///
/// The JAR/manifest spec (JVMS §5.3.4, java.util.jar.Manifest) requires:
///   - No line in MANIFEST.MF may exceed 72 bytes (including the `\r\n`).
///   - Continuation lines begin with a single space (which does not count
///     as content); each continuation line is also limited to 72 bytes total.
///
/// So the first line holds 72 - len("Class-Path: ") - 2 = 58 bytes of value,
/// and each subsequent line holds 72 - 1 (space) - 2 (\r\n) = 69 bytes.
fn manifest_class_path(dep_jars: &[PathBuf]) -> String {
    let value = dep_jars
        .iter()
        .filter_map(|p| p.file_name())
        .map(|f| format!("libs/{}", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");

    fold_manifest_header("Class-Path", &value)
}

/// Fold a manifest header value to lines of at most 72 bytes (including \r\n).
/// Returns the complete header block including the trailing \r\n.
fn fold_manifest_header(name: &str, value: &str) -> String {
    // Bytes available on the first line: 72 total - name - ": " - "\r\n"
    let first_capacity = 72usize.saturating_sub(name.len() + 2 + 2);
    // Bytes available on continuation lines: 72 total - " " prefix - "\r\n"
    let cont_capacity = 69usize;

    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut pos = 0usize;
    let mut first = true;

    while pos < bytes.len() {
        let capacity = if first { first_capacity } else { cont_capacity };
        // Advance by at most `capacity` bytes, but don't split a multi-byte
        // UTF-8 sequence (walk back to a char boundary).
        let mut end = (pos + capacity).min(bytes.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &value[pos..end];
        if first {
            out.push_str(name);
            out.push_str(": ");
            first = false;
        } else {
            out.push(' '); // continuation line prefix
        }
        out.push_str(chunk);
        out.push_str("\r\n");
        pos = end;
    }

    // Edge case: empty value.
    if first {
        out.push_str(name);
        out.push_str(": \r\n");
    }

    out
}

/// Populate `libs_dir` with all `dep_jars`, using hardlinks where possible
/// and falling back to a full copy otherwise (e.g. cross-device).
///
/// The directory is wiped before population so that stale JARs from previous
/// builds (version bumps, removed dependencies) are removed.
pub(crate) fn populate_libs_dir(libs_dir: &Path, dep_jars: &[PathBuf]) -> Result<()> {
    // Wipe and recreate for a clean slate.
    if libs_dir.exists() {
        std::fs::remove_dir_all(libs_dir)
            .with_context(|| format!("failed to remove {}", libs_dir.display()))?;
    }
    std::fs::create_dir_all(libs_dir)
        .with_context(|| format!("failed to create {}", libs_dir.display()))?;

    for src in dep_jars {
        let file_name = src
            .file_name()
            .with_context(|| format!("dep JAR has no filename: {}", src.display()))?;
        let dst = libs_dir.join(file_name);

        // Try hardlink first; fall back to copy on any error (cross-device,
        // unsupported filesystem, permissions, etc.).
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst)
                .with_context(|| format!("failed to copy {} to {}", src.display(), dst.display()))?;
        }
    }

    println!("  Libs            {} JAR(s) → target/libs/", dep_jars.len());
    Ok(())
}

/// Write a reproducible JAR:
///   • entries sorted lexicographically
///   • all timestamps set to EPOCH (2024-01-01 00:00:00 UTC)
///   • MANIFEST.MF written first (JAR spec requirement)
///   • Class-Path header added when dep_jars is non-empty
///   • files from `resources_dir` (src/main/resources) embedded at their
///     path relative to that directory, alongside the class files
///   • no extra tool-specific metadata that embeds build time
///   • when `build_info` is `Some`, writes `META-INF/build-info.properties`
///     right after the manifest
pub(crate) fn write_deterministic_jar(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    main_class: Option<&str>,
    dep_jars: &[PathBuf],
    build_info: Option<&str>,
) -> Result<()> {
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("cannot create {}", jar_path.display()))?;

    let mut zip = ZipWriter::new(file);

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(epoch())
        .unix_permissions(0o644);

    let dir_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(epoch())
        .unix_permissions(0o755);

    // --- MANIFEST.MF (must be first entry per JAR spec) ---------------------
    zip.start_file("META-INF/", dir_options)
        .context("failed to write META-INF/ directory entry")?;

    let mut manifest = "Manifest-Version: 1.0\r\n".to_string();
    if let Some(mc) = main_class {
        manifest.push_str(&format!("Main-Class: {}\r\n", mc));
    }
    if !dep_jars.is_empty() {
        // manifest_class_path() returns the fully-folded header block
        // (including the trailing \r\n) per the JAR spec 72-byte line limit.
        manifest.push_str(&manifest_class_path(dep_jars));
    }
    manifest.push_str("\r\n");

    zip.start_file("META-INF/MANIFEST.MF", options)
        .context("failed to start MANIFEST.MF entry")?;
    zip.write_all(manifest.as_bytes())
        .context("failed to write MANIFEST.MF")?;

    // --- build-info.properties (optional) -----------------------------------
    if let Some(props) = build_info {
        zip.start_file("META-INF/build-info.properties", options)
            .context("failed to start META-INF/build-info.properties entry")?;
        zip.write_all(props.as_bytes())
            .context("failed to write META-INF/build-info.properties")?;
    }

    // --- collect entries from classes_dir and resources_dir -----------------
    // We gather (zip_path, fs_path) pairs from both roots, deduplicate by
    // zip_path (class files win over resources for the same path, matching
    // Maven's behaviour), then sort and write.
    let mut entries: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();

    for (root, label) in [
        (Some(classes_dir), "classes"),
        (resources_dir, "resources"),
    ] {
        let Some(root) = root else { continue };
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() || e.file_type().is_dir())
        {
            let rel = entry
                .path()
                .strip_prefix(root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if rel.is_empty() {
                continue; // skip the root dir itself
            }
            let zip_path = if entry.file_type().is_dir() {
                format!("{}/", rel)
            } else {
                rel
            };
            // Skip META-INF from resources — we already wrote the manifest.
            if zip_path.starts_with("META-INF") {
                continue;
            }
            // Class files take precedence; skip if already inserted from classes.
            if label == "resources" && entries.contains_key(&zip_path) {
                continue;
            }
            entries.insert(zip_path, entry.into_path());
        }
    }

    // BTreeMap is already sorted lexicographically.
    for (zip_path, fs_path) in &entries {
        if zip_path.ends_with('/') {
            zip.start_file(zip_path, dir_options)
                .with_context(|| format!("failed to write directory entry {}", zip_path))?;
        } else {
            zip.start_file(zip_path, options)
                .with_context(|| format!("failed to start entry {}", zip_path))?;
            let data = std::fs::read(fs_path)
                .with_context(|| format!("failed to read {}", fs_path.display()))?;
            zip.write_all(&data)
                .with_context(|| format!("failed to write entry {}", zip_path))?;
        }
    }

    zip.finish().context("failed to finalise JAR")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_manifest_header_short_value_fits_on_one_line() {
        // "Class-Path: libs/foo.jar\r\n" is 26 bytes — well under 72.
        let result = fold_manifest_header("Class-Path", "libs/foo.jar");
        assert_eq!(result, "Class-Path: libs/foo.jar\r\n");
    }

    #[test]
    fn fold_manifest_header_long_value_is_folded() {
        // Build a value that definitely exceeds 72 bytes on the first line.
        let value = "libs/aaaa.jar libs/bbbb.jar libs/cccc.jar libs/dddd.jar libs/eeee.jar libs/ffff.jar";
        let result = fold_manifest_header("Class-Path", value);
        for line in result.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(
                line.len() <= 70, // 70 bytes of content + \r\n = 72
                "line exceeds 70 bytes: {:?} ({} bytes)",
                line,
                line.len()
            );
        }
        // The folded result must round-trip: strip header name, join
        // continuation lines, and get back the original value.
        let reconstructed = result
            .split("\r\n")
            .filter(|l| !l.is_empty())
            .enumerate()
            .map(|(i, l)| if i == 0 { l["Class-Path: ".len()..].to_string() } else { l[1..].to_string() })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(reconstructed, value);
    }

    #[test]
    fn fold_manifest_header_empty_value() {
        let result = fold_manifest_header("Class-Path", "");
        assert_eq!(result, "Class-Path: \r\n");
    }

    // -----------------------------------------------------------------------
    // build-info.properties
    // -----------------------------------------------------------------------

    /// Helper: write a minimal JAR with the given build_info and return the
    /// raw bytes so we can inspect the ZIP entries.
    fn jar_bytes_with_build_info(build_info: Option<&str>) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        // A dummy .class file so the JAR is non-trivial.
        std::fs::write(classes_dir.join("Foo.class"), b"\xca\xfe\xba\xbe").unwrap();

        let jar_path = tmp.path().join("out.jar");
        write_deterministic_jar(
            &jar_path,
            &classes_dir,
            None,
            None,
            &[],
            build_info,
        )
        .unwrap();

        std::fs::read(&jar_path).unwrap()
    }

    /// Parse ZIP central-directory entry names from raw bytes.
    fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
        use std::io::Cursor;
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_owned())
            .collect()
    }

    #[test]
    fn no_build_info_when_none() {
        let bytes = jar_bytes_with_build_info(None);
        let names = zip_entry_names(&bytes);
        assert!(
            !names.iter().any(|n| n == "META-INF/build-info.properties"),
            "build-info.properties must not appear when build_info is None; entries: {:?}",
            names,
        );
    }

    #[test]
    fn build_info_entry_present_when_some() {
        let bytes = jar_bytes_with_build_info(Some("git.commit.id=abc123\n"));
        let names = zip_entry_names(&bytes);
        assert!(
            names.iter().any(|n| n == "META-INF/build-info.properties"),
            "build-info.properties must be present when build_info is Some; entries: {:?}",
            names,
        );
    }

    #[test]
    fn build_info_entry_has_correct_content() {
        let content = "git.commit.id=abc123def456\n";
        let bytes = jar_bytes_with_build_info(Some(content));
        use std::io::{Cursor, Read};
        let cursor = Cursor::new(&bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut entry = archive.by_name("META-INF/build-info.properties").unwrap();
        let mut actual = String::new();
        entry.read_to_string(&mut actual).unwrap();
        assert_eq!(actual, content);
    }

    #[test]
    fn build_info_entry_is_after_manifest() {
        let bytes = jar_bytes_with_build_info(Some("git.commit.id=abc\n"));
        let names = zip_entry_names(&bytes);
        let manifest_pos = names.iter().position(|n| n == "META-INF/MANIFEST.MF").unwrap();
        let props_pos = names
            .iter()
            .position(|n| n == "META-INF/build-info.properties")
            .unwrap();
        assert!(
            props_pos > manifest_pos,
            "build-info.properties ({props_pos}) must come after MANIFEST.MF ({manifest_pos})",
        );
    }
}
