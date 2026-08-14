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

use crate::incremental::{finalize_staged, staging_path};

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC.
/// Matches SOURCE_DATE_EPOCH convention used by Debian, Nix, etc.
fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0).expect("epoch constant is valid")
}

/// Build an OS-appropriate classpath string from a list of JAR paths.
pub fn classpath_string(jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    jars.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Extract the dotted Maven groupId from a local-repository JAR path of the form
/// `…/repository/<g1>/<g2>/…/<artifact>/<version>/<file>`.  Returns `None` when
/// the path doesn't follow that layout.
fn group_id_from_repo_path(path: &Path) -> Option<String> {
    let segs: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    // Need: repository / <group…(>=1)> / artifact / version / file
    let repo_idx = segs.iter().rposition(|s| s == "repository")?;
    let group_start = repo_idx + 1;
    let group_end = segs.len().checked_sub(3)?; // exclude artifact/version/file
    if group_end <= group_start {
        return None;
    }
    Some(segs[group_start..group_end].join("."))
}

/// Map each dependency JAR to the filename it should take under `libs/`.
///
/// Normally this is just the JAR's own filename.  When two JARs would land on the
/// same filename (same artifact+version but different Maven groups — e.g.
/// `javax.inject:javax.inject:1` vs a re-bundled `com.example:javax.inject:1`),
/// the colliding entries are qualified with their dotted groupId
/// (`javax.inject-javax.inject-1.jar`), mirroring Maven's `prependGroupId`, so
/// neither JAR is lost.  Returned names are aligned 1:1 with `dep_jars` and are a
/// pure function of the slice (so `populate_libs_dir`, the manifest Class-Path,
/// and the docker `target/libs/` copy stay in lockstep when given the same input).
pub(crate) fn libs_entry_names(dep_jars: &[PathBuf]) -> Vec<String> {
    let bases: Vec<String> = dep_jars
        .iter()
        .map(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();

    let mut base_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for b in &bases {
        *base_counts.entry(b.as_str()).or_insert(0) += 1;
    }

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut names = Vec::with_capacity(dep_jars.len());
    for (i, base) in bases.iter().enumerate() {
        // Qualify only when the bare filename collides with another JAR's.
        let mut candidate = if base_counts.get(base.as_str()).copied().unwrap_or(0) > 1 {
            match group_id_from_repo_path(&dep_jars[i]) {
                Some(group) => format!("{group}-{base}"),
                None => insert_before_extension(base, &format!("-{i}")),
            }
        } else {
            base.clone()
        };
        // Paranoia: guarantee global uniqueness even if two qualified names still
        // match (e.g. identical group, or the group couldn't be derived).
        while !used.insert(candidate.clone()) {
            candidate = insert_before_extension(&candidate, &format!("-{i}"));
        }
        names.push(candidate);
    }
    names
}

/// Insert `suffix` before the file extension: `foo-1.0.jar` + `-2` ->
/// `foo-1.0-2.jar`.  Appends to the end when there is no extension.
fn insert_before_extension(name: &str, suffix: &str) -> String {
    match name.rfind('.') {
        Some(dot) => format!("{}{}{}", &name[..dot], suffix, &name[dot..]),
        None => format!("{name}{suffix}"),
    }
}

/// Build the *value* portion of the Class-Path header (the "libs/..." space-
/// separated list). Used by the writer to feed the common build_manifest.
///
/// Uses the same [`libs_entry_names`] mapping as [`populate_libs_dir`], so the
/// Class-Path entries match the files actually written to `target/libs/`.
fn manifest_class_path_value(dep_jars: &[PathBuf]) -> String {
    libs_entry_names(dep_jars)
        .iter()
        .map(|name| format!("libs/{name}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format (fold) a manifest header value to lines of at most 72 bytes (including \r\n).
/// Returns the complete header block including the trailing \r\n. This is the
/// common helper used for *all* manifest headers (Main-Class, Class-Path,
/// Automatic-Module-Name, etc.).
pub(crate) fn format_manifest_header(name: &str, value: &str) -> String {
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

/// Unfold a raw MANIFEST.MF text by joining continuation lines (those starting
/// with a single space) back onto their preceding header. This is the inverse
/// of format_manifest_header and is required to correctly parse spec-compliant
/// folded manifests (e.g. long Main-Class or Class-Path).
fn unfold_manifest(manifest: &str) -> String {
    let mut result = String::new();
    let mut current = String::new();
    for line in manifest.lines() {
        if line.starts_with(' ') {
            // Continuation: append content after the leading space.
            if !current.is_empty() {
                current.push_str(&line[1..]);
            }
        } else {
            if !current.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&current);
            }
            current = line.to_string();
        }
    }
    if !current.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&current);
    }
    result
}

/// Return the (unfolded) value of a header from a MANIFEST.MF text, or None.
/// Used by all readers (Main-Class in build.rs, Automatic-Module-Name in jpms.rs)
/// so that folded manifests are handled uniformly.
pub(crate) fn get_manifest_header(manifest: &str, name: &str) -> Option<String> {
    let unfolded = unfold_manifest(manifest);
    let prefix = format!("{}:", name);
    for line in unfolded.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            let val = rest.trim().to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Build a complete MANIFEST.MF (as bytes ready for Zip) using the common
/// folding logic for every header. This is the single place that should be
/// used to generate manifest content so that Main-Class, Class-Path,
/// Automatic-Module-Name etc. are always written correctly.
pub(crate) fn build_manifest(
    main_class: Option<&str>,
    class_path: Option<&str>,
    automatic_module_name: Option<&str>,
) -> String {
    let mut m = "Manifest-Version: 1.0\r\n".to_string();
    if let Some(v) = main_class {
        m.push_str(&format_manifest_header("Main-Class", v));
    }
    if let Some(v) = class_path {
        m.push_str(&format_manifest_header("Class-Path", v));
    }
    if let Some(v) = automatic_module_name {
        m.push_str(&format_manifest_header("Automatic-Module-Name", v));
    }
    m.push_str("\r\n");
    m
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

    // Disambiguated target filenames (qualifies group:artifact collisions so no
    // JAR is silently overwritten).  Same mapping the manifest Class-Path uses.
    let names = libs_entry_names(dep_jars);
    let qualified = dep_jars
        .iter()
        .zip(&names)
        .filter(|(src, name)| {
            src.file_name()
                .map(|f| f.to_string_lossy().as_ref() != name.as_str())
                .unwrap_or(false)
        })
        .count();

    for (src, name) in dep_jars.iter().zip(&names) {
        let dst = libs_dir.join(name);

        // Try hardlink first; fall back to copy on any error (cross-device,
        // unsupported filesystem, permissions, etc.).
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dst.display())
            })?;
        }
    }

    crate::parallel::emit(&crate::style::info(
        "Libs",
        &format!("{} JAR(s) → target/libs/", dep_jars.len()),
    ));
    if qualified > 0 {
        crate::parallel::emit(&crate::style::warn(
            "Libs",
            &format!("{qualified} JAR(s) with colliding filenames qualified by group"),
        ));
    }
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
///   • when `automatic_module_name` is `Some`, writes `Automatic-Module-Name`
///     in the manifest (for library JARs that are not yet JPMS modules)
pub(crate) fn write_deterministic_jar(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    main_class: Option<&str>,
    dep_jars: &[PathBuf],
    build_info: Option<&str>,
    automatic_module_name: Option<&str>,
) -> Result<()> {
    let part = staging_path(jar_path);
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create parent for staging file {}",
                parent.display()
            )
        })?;
    }

    // Write the complete JAR to the staging file first. Only on success do we
    // rename it into place. This prevents a mid-write crash from leaving a
    // truncated JAR at the final path with a fresh mtime.
    {
        let file = std::fs::File::create(&part)
            .with_context(|| format!("cannot create {}", part.display()))?;

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

        // Build the full manifest using the single common helper. This ensures
        // Main-Class (and Automatic-Module-Name) are folded exactly like Class-Path
        // and that all writers in the crate produce consistent output.
        let cp_value = if !dep_jars.is_empty() {
            Some(manifest_class_path_value(dep_jars))
        } else {
            None
        };
        let manifest = build_manifest(main_class, cp_value.as_deref(), automatic_module_name);

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
        let mut entries: std::collections::BTreeMap<String, PathBuf> =
            std::collections::BTreeMap::new();

        for (root, label) in [(Some(classes_dir), "classes"), (resources_dir, "resources")] {
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
                // Skip exactly the entries we already wrote ourselves above: the
                // META-INF/ directory entry, MANIFEST.MF (always regenerated),
                // and build-info.properties (only when we generated one).
                // Everything else under META-INF/ — service loader
                // registrations, annotation-processor metadata, native-image
                // configs, Kotlin module files, etc. — is preserved.
                if zip_path == "META-INF/" || zip_path == "META-INF/MANIFEST.MF" {
                    continue;
                }
                if build_info.is_some() && zip_path == "META-INF/build-info.properties" {
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
    } // drop ZipWriter + File for the part before rename

    finalize_staged(&part, jar_path)?;
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
        let result = format_manifest_header("Class-Path", "libs/foo.jar");
        assert_eq!(result, "Class-Path: libs/foo.jar\r\n");
    }

    #[test]
    fn fold_manifest_header_long_value_is_folded() {
        // Build a value that definitely exceeds 72 bytes on the first line.
        let value =
            "libs/aaaa.jar libs/bbbb.jar libs/cccc.jar libs/dddd.jar libs/eeee.jar libs/ffff.jar";
        let result = format_manifest_header("Class-Path", value);
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
            .map(|(i, l)| {
                if i == 0 {
                    l["Class-Path: ".len()..].to_string()
                } else {
                    l[1..].to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(reconstructed, value);
    }

    #[test]
    fn fold_manifest_header_empty_value() {
        let result = format_manifest_header("Class-Path", "");
        assert_eq!(result, "Class-Path: \r\n");
    }

    #[test]
    fn format_manifest_header_long_main_class_is_folded() {
        // A realistically long FQCN that will exceed the first-line capacity.
        let long_mc = "com.example.very.deep.package.with.a.long.name.that.will.definitely.exceed.the.72.byte.limit.Main";
        let result = format_manifest_header("Main-Class", long_mc);
        // Every physical line (incl. \r\n) must be <= 72 bytes.
        for line in result.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(
                line.len() <= 70,
                "Main-Class line exceeds 70 bytes of content: {:?} (len {})",
                line,
                line.len()
            );
        }
        // Round-trip via the common getter (which does unfolding).
        let got = get_manifest_header(&result, "Main-Class").expect("should parse");
        assert_eq!(got, long_mc);
    }

    #[test]
    fn build_manifest_and_get_header_roundtrip_main_class() {
        let mc = "com.example.FooBarBaz";
        let mf = build_manifest(Some(mc), None, None);
        assert!(mf.starts_with("Manifest-Version: 1.0\r\n"));
        assert!(mf.contains("Main-Class:"));
        assert!(mf.ends_with("\r\n\r\n"));
        assert_eq!(get_manifest_header(&mf, "Main-Class"), Some(mc.to_string()));
    }

    #[test]
    fn get_manifest_header_unfolds_continuation_lines() {
        // Simulate a manifest that an external tool (or old Curie) wrote with a folded Main-Class.
        let folded = "Manifest-Version: 1.0\r\nMain-Class: com.example.a.very.long.package.name.that.\r\n was.folded.by.the.writer\r\n\r\n";
        let got = get_manifest_header(folded, "Main-Class").expect("must unfold");
        assert_eq!(
            got,
            "com.example.a.very.long.package.name.that.was.folded.by.the.writer"
        );
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
        write_deterministic_jar(&jar_path, &classes_dir, None, None, &[], build_info, None)
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

    /// Read a single ZIP entry's contents as a UTF-8 string.
    fn zip_entry_content(bytes: &[u8], name: &str) -> String {
        use std::io::{Cursor, Read};
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        content
    }

    /// Helper: write a JAR with a `Foo.class` in `classes_dir` plus the given
    /// `(rel_path, content)` files under `resources_dir`, and return the raw
    /// bytes so we can inspect the ZIP entries.
    fn jar_bytes_with_resources(
        build_info: Option<&str>,
        resource_files: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("Foo.class"), b"\xca\xfe\xba\xbe").unwrap();

        let resources_dir = tmp.path().join("resources");
        for (rel_path, content) in resource_files {
            let full = resources_dir.join(rel_path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, content).unwrap();
        }

        let jar_path = tmp.path().join("out.jar");
        write_deterministic_jar(
            &jar_path,
            &classes_dir,
            Some(&resources_dir),
            None,
            &[],
            build_info,
            None,
        )
        .unwrap();

        std::fs::read(&jar_path).unwrap()
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
        let manifest_pos = names
            .iter()
            .position(|n| n == "META-INF/MANIFEST.MF")
            .unwrap();
        let props_pos = names
            .iter()
            .position(|n| n == "META-INF/build-info.properties")
            .unwrap();
        assert!(
            props_pos > manifest_pos,
            "build-info.properties ({props_pos}) must come after MANIFEST.MF ({manifest_pos})",
        );
    }

    // -----------------------------------------------------------------------
    // META-INF/** resources from classes_dir/resources_dir
    // -----------------------------------------------------------------------

    #[test]
    fn meta_inf_services_resource_is_preserved() {
        let bytes = jar_bytes_with_resources(
            None,
            &[(
                "META-INF/services/java.sql.Driver",
                b"com.example.MyDriver\n",
            )],
        );
        let names = zip_entry_names(&bytes);
        assert!(
            names
                .iter()
                .any(|n| n == "META-INF/services/java.sql.Driver"),
            "META-INF/services/java.sql.Driver must be preserved; entries: {:?}",
            names,
        );
        assert_eq!(
            zip_entry_content(&bytes, "META-INF/services/java.sql.Driver"),
            "com.example.MyDriver\n",
        );
    }

    #[test]
    fn manifest_mf_resource_is_not_duplicated() {
        let bytes = jar_bytes_with_resources(None, &[("META-INF/MANIFEST.MF", b"Custom: yes\r\n")]);
        let names = zip_entry_names(&bytes);
        let count = names
            .iter()
            .filter(|n| *n == "META-INF/MANIFEST.MF")
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one MANIFEST.MF entry; entries: {:?}",
            names
        );
        let manifest = zip_entry_content(&bytes, "META-INF/MANIFEST.MF");
        assert!(
            manifest.contains("Manifest-Version: 1.0"),
            "the generated manifest must win; got: {manifest:?}",
        );
        assert!(
            !manifest.contains("Custom: yes"),
            "the user-provided MANIFEST.MF resource must not leak through; got: {manifest:?}",
        );
    }

    #[test]
    fn build_info_resource_is_not_duplicated_when_generated() {
        let bytes = jar_bytes_with_resources(
            Some("git.commit.id=generated\n"),
            &[(
                "META-INF/build-info.properties",
                b"git.commit.id=user-provided\n",
            )],
        );
        let names = zip_entry_names(&bytes);
        let count = names
            .iter()
            .filter(|n| *n == "META-INF/build-info.properties")
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one build-info.properties entry; entries: {:?}",
            names
        );
        assert_eq!(
            zip_entry_content(&bytes, "META-INF/build-info.properties"),
            "git.commit.id=generated\n",
            "the generated build-info.properties must win",
        );
    }

    #[test]
    fn build_info_resource_preserved_when_not_generated() {
        let bytes = jar_bytes_with_resources(
            None,
            &[(
                "META-INF/build-info.properties",
                b"git.commit.id=user-provided\n",
            )],
        );
        assert_eq!(
            zip_entry_content(&bytes, "META-INF/build-info.properties"),
            "git.commit.id=user-provided\n",
            "with no generated build_info, the user's resource must be preserved",
        );
    }

    #[test]
    fn meta_inf_root_directory_entry_is_not_duplicated() {
        let bytes = jar_bytes_with_resources(
            None,
            &[(
                "META-INF/services/java.sql.Driver",
                b"com.example.MyDriver\n",
            )],
        );
        let names = zip_entry_names(&bytes);
        let count = names.iter().filter(|n| *n == "META-INF/").count();
        assert_eq!(
            count, 1,
            "expected exactly one META-INF/ directory entry; entries: {:?}",
            names
        );
    }

    #[test]
    fn write_deterministic_jar_leaves_no_part_file() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("Foo.class"), b"\xca\xfe\xba\xbe").unwrap();

        let jar_path = tmp.path().join("myapp.jar");
        write_deterministic_jar(&jar_path, &classes_dir, None, None, &[], None, None).unwrap();

        assert!(jar_path.exists(), "final jar must exist");

        // No sibling .part.* for this base name should remain.
        let has_leftover_part = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("myapp.jar.part.")
            });
        assert!(
            !has_leftover_part,
            "no .part.* sibling should be left after successful write"
        );
    }

    // ── libs/ filename disambiguation (bug #19) ──────────────────────────────

    /// Create a real JAR file under a fake `~/.m2/repository` layout and return
    /// its absolute path.
    fn fake_m2_jar(root: &Path, group: &str, artifact: &str, version: &str) -> PathBuf {
        let p = root
            .join("repository")
            .join(group.replace('.', "/"))
            .join(artifact)
            .join(version)
            .join(format!("{artifact}-{version}.jar"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("{group}:{artifact}:{version}").as_bytes()).unwrap();
        p
    }

    fn sorted_dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn group_id_from_repo_path_parses_layout() {
        let p =
            PathBuf::from("/home/u/.m2/repository/javax/inject/javax.inject/1/javax.inject-1.jar");
        assert_eq!(group_id_from_repo_path(&p).as_deref(), Some("javax.inject"));
        let p2 = PathBuf::from("/home/u/.m2/repository/com/google/guava/guava/33.0/guava-33.0.jar");
        assert_eq!(
            group_id_from_repo_path(&p2).as_deref(),
            Some("com.google.guava")
        );
    }

    #[test]
    fn group_id_from_repo_path_none_for_non_repo_path() {
        assert_eq!(
            group_id_from_repo_path(&PathBuf::from("/tmp/whatever/foo-1.0.jar")),
            None
        );
    }

    #[test]
    fn libs_entry_names_leaves_unique_names_unchanged() {
        let jars = vec![
            PathBuf::from("/home/u/.m2/repository/com/example/foo/1.0/foo-1.0.jar"),
            PathBuf::from("/home/u/.m2/repository/com/example/bar/2.0/bar-2.0.jar"),
        ];
        assert_eq!(libs_entry_names(&jars), vec!["foo-1.0.jar", "bar-2.0.jar"]);
    }

    #[test]
    fn libs_entry_names_disambiguates_group_collision() {
        let jars = vec![
            PathBuf::from("/home/u/.m2/repository/javax/inject/javax.inject/1/javax.inject-1.jar"),
            PathBuf::from("/home/u/.m2/repository/com/example/javax.inject/1/javax.inject-1.jar"),
        ];
        let names = libs_entry_names(&jars);
        assert_eq!(names[0], "javax.inject-javax.inject-1.jar");
        assert_eq!(names[1], "com.example-javax.inject-1.jar");
        assert_ne!(names[0], names[1], "colliding names must be made distinct");
    }

    #[test]
    fn populate_libs_dir_keeps_both_colliding_jars() {
        let tmp = tempfile::tempdir().unwrap();
        let a = fake_m2_jar(tmp.path(), "javax.inject", "javax.inject", "1");
        let b = fake_m2_jar(tmp.path(), "com.example", "javax.inject", "1");
        let libs = tmp.path().join("target").join("libs");

        populate_libs_dir(&libs, &[a, b]).unwrap();

        assert_eq!(
            sorted_dir_names(&libs),
            vec![
                "com.example-javax.inject-1.jar",
                "javax.inject-javax.inject-1.jar"
            ],
            "both colliding JARs must be kept under distinct names",
        );
    }

    #[test]
    fn populate_libs_dir_removes_stale_bare_name_on_transition() {
        let tmp = tempfile::tempdir().unwrap();
        let libs = tmp.path().join("target").join("libs");

        // Build 1: a single dep keeps its bare filename.
        let a = fake_m2_jar(tmp.path(), "javax.inject", "javax.inject", "1");
        populate_libs_dir(&libs, std::slice::from_ref(&a)).unwrap();
        assert!(
            libs.join("javax.inject-1.jar").exists(),
            "build 1 writes the bare name"
        );

        // Build 2: a colliding dep is added → both get group-qualified, and the
        // stale bare name from build 1 must be gone (libs/ is wiped each build).
        let b = fake_m2_jar(tmp.path(), "com.example", "javax.inject", "1");
        populate_libs_dir(&libs, &[a, b]).unwrap();

        assert!(
            !libs.join("javax.inject-1.jar").exists(),
            "stale bare name from the previous build must be removed",
        );
        assert_eq!(
            sorted_dir_names(&libs),
            vec![
                "com.example-javax.inject-1.jar",
                "javax.inject-javax.inject-1.jar"
            ],
        );
    }

    #[test]
    fn class_path_lists_both_colliding_jars() {
        let jars = vec![
            PathBuf::from("/home/u/.m2/repository/javax/inject/javax.inject/1/javax.inject-1.jar"),
            PathBuf::from("/home/u/.m2/repository/com/example/javax.inject/1/javax.inject-1.jar"),
        ];
        // The Class-Path entries must match the disambiguated libs/ filenames.
        assert_eq!(
            manifest_class_path_value(&jars),
            "libs/javax.inject-javax.inject-1.jar libs/com.example-javax.inject-1.jar",
        );
    }
}
