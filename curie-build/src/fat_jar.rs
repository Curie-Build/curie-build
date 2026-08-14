//! Fat/uber JAR packaging: merges all dependency classes into a single JAR.
//!
//! Features:
//!   - Deterministic output: sorted entries, fixed timestamps (reproducible builds)
//!   - META-INF/services files are merged (all providers concatenated)
//!   - Package relocations rewrite class-file constant-pool entries and resource paths
//!   - Per-dependency include/exclude via `fatJar = false` on individual deps
//!   - Incremental: only rebuild if any input (classes, resources, deps, Curie.toml) changed

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::jar::build_manifest;

use crate::incremental::{finalize_staged, staging_path};

use crate::descriptor::{self, Relocation};

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC.
fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0).expect("epoch constant is valid")
}

/// Filter dependency JARs according to the global `shadeAll` policy and
/// per-dependency `shade` / `relocations` overrides.
///
/// A declared direct dependency is shaded (its JAR(s) included) when
/// `dep.should_shade(desc.fat_jar.shade_all)` is true.  `relocations` on a
/// dependency force inclusion.  The filename-prefix heuristic (artifact name
/// from the declared "group:artifact" key) is used to map resolved JARs back
/// to direct dependencies for filtering and for per-dep relocation attribution.
pub fn filter_fat_jar_deps(dep_jars: &[PathBuf], desc: &descriptor::Descriptor) -> Vec<PathBuf> {
    let shade_all = desc.fat_jar.shade_all;

    // Direct deps that must not be shaded — match JARs by artifact-version stem
    // (or m2 layout), not a bare artifact-name prefix (bug #23).
    let excluded: Vec<(&str, &str)> = desc
        .dependencies
        .iter()
        .filter(|(_, v)| !v.should_shade(shade_all))
        .map(|(k, v)| (k.as_str(), v.version()))
        .collect();

    dep_jars
        .iter()
        .filter(|jar| {
            !excluded
                .iter()
                .any(|(coord, ver)| jar_matches_direct_dep(jar, coord, ver))
        })
        .cloned()
        .collect()
}

/// Check that per-dependency relocation rules do not target packages that
/// also exist in other bundled dependency JARs.
///
/// If a "from" package declared on a direct dep appears (by ZIP entry prefix)
/// in any other JAR that will be included in the fat JAR, we emit a clear
/// error recommending that the user move the rule(s) to the top-level
/// `[[fat-jar.relocations]]` section.
pub fn check_per_dep_relocation_overlap(
    desc: &descriptor::Descriptor,
    fat_dep_jars: &[PathBuf],
) -> Result<()> {
    let shade_all = desc.fat_jar.shade_all;

    for (coord, v) in &desc.dependencies {
        if !v.should_shade(shade_all) {
            continue;
        }
        let relocs = v.relocations();
        if relocs.is_empty() {
            continue;
        }

        let version = v.version();

        for reloc in relocs {
            let internal_from = reloc.from.replace('.', "/");
            if internal_from.is_empty() {
                continue;
            }

            for jar in fat_dep_jars {
                // Skip JARs that belong to the declaring direct dep itself.
                if jar_matches_direct_dep(jar, coord, version) {
                    continue;
                }

                let fname = jar
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();

                if jar_contains_prefix(jar, &internal_from) {
                    anyhow::bail!(
                        "Package '{}' (from relocation on dependency \"{}\") also appears in another bundled dependency JAR ({}). \
                         Move the relocation rule to the top-level [[fat-jar.relocations]] section so it is applied consistently.",
                        reloc.from,
                        coord,
                        fname
                    );
                }
            }
        }
    }
    Ok(())
}

/// True if `fname` is the Maven file name for `artifact` at `version`:
/// `artifact-version.jar` or `artifact-version-<classifier>.jar`.
///
/// Deliberately does **not** treat `json-path-0.9.jar` as belonging to
/// artifact `json` (bug #23 used a bare `starts_with(artifact)` prefix).
fn jar_file_matches_artifact_version(fname: &str, artifact: &str, version: &str) -> bool {
    if artifact.is_empty() || version.is_empty() {
        return false;
    }
    let stem = format!("{artifact}-{version}");
    if fname == format!("{stem}.jar") {
        return true;
    }
    fname
        .strip_prefix(&format!("{stem}-"))
        .is_some_and(|rest| rest.ends_with(".jar") && rest.len() > ".jar".len())
}

/// Whether `jar` is the resolved file for direct dependency `group:artifact`
/// at `version` (`""` when BOM-managed).
fn jar_matches_direct_dep(jar: &Path, coord: &str, version: &str) -> bool {
    let artifact = coord.split(':').nth(1).unwrap_or(coord);
    let fname = jar
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();

    if jar_file_matches_artifact_version(&fname, artifact, version) {
        // When the path encodes a Maven groupId, require it matches so two
        // groups publishing the same artifact:version are not conflated.
        if let Some((group, _)) = coord.split_once(':') {
            if let Some(path_group) = m2_group_id_from_path(jar) {
                return path_group == group;
            }
        }
        return true;
    }

    // BOM / unknown version: fall back to local-repository layout
    // `…/repository/<group…>/<artifact>/<version>/<file>`.
    m2_path_matches_coord(jar, coord)
}

/// Dotted groupId from a `…/repository/…` JAR path, if the layout matches.
fn m2_group_id_from_path(path: &Path) -> Option<String> {
    let segs = path_segments(path);
    let repo_idx = segs.iter().rposition(|s| s == "repository")?;
    let group_start = repo_idx + 1;
    let group_end = segs.len().checked_sub(3)?; // exclude artifact / version / file
    if group_end <= group_start {
        return None;
    }
    Some(segs[group_start..group_end].join("."))
}

/// True when `jar` sits at `…/repository/<group…>/<artifact>/<ver>/<file>` for `coord`.
fn m2_path_matches_coord(jar: &Path, coord: &str) -> bool {
    let Some((group, artifact)) = coord.split_once(':') else {
        return false;
    };
    let segs = path_segments(jar);
    let Some(repo_idx) = segs.iter().rposition(|s| s == "repository") else {
        return false;
    };
    let group_start = repo_idx + 1;
    let Some(group_end) = segs.len().checked_sub(3) else {
        return false;
    };
    if group_end <= group_start {
        return false;
    }
    let path_group = segs[group_start..group_end].join(".");
    let path_artifact = segs[group_end].as_str();
    path_group == group && path_artifact == artifact
}

fn path_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Return true if the given JAR (opened as a zip) contains any entry whose
/// name starts with the given internal (slash-separated) package prefix.
fn jar_contains_prefix(jar_path: &Path, internal_prefix: &str) -> bool {
    let file = match std::fs::File::open(jar_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return false,
    };
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            let name = entry.name();
            if name.starts_with(internal_prefix) {
                return true;
            }
        }
    }
    false
}

/// All ancestor directory paths of a ZIP entry path, each ending in `/`.
///
/// E.g. `"com/example/App.class"` -> `["com/", "com/example/"]`.
fn path_ancestors(path: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut end = 0;
    while let Some(slash) = path[end..].find('/') {
        end += slash + 1;
        ancestors.push(path[..end].to_string());
    }
    ancestors
}

/// Apply relocations to a ZIP entry path (resource path relocation).
///
/// Converts dot-separated patterns to path separators for matching against
/// ZIP entry paths (which use `/` separators).
pub fn relocate_path(path: &str, relocations: &[Relocation]) -> String {
    let mut result = path.to_string();
    for reloc in relocations {
        let from = reloc.from.replace('.', "/");
        let to = reloc.to.replace('.', "/");
        if result.starts_with(&from) {
            // Check excludes
            if is_excluded(&result, &reloc.excludes) {
                continue;
            }
            result = format!("{}{}", to, &result[from.len()..]);
        }
    }
    result
}

/// Apply relocations to class-file bytecode constant pool.
///
/// Parses the constant pool and rewrites only the contents of UTF-8 entries,
/// correctly updating their length prefixes. Both internal form
/// (`com/google/common`) and dotted form (`com.google.common`) are handled
/// inside descriptors, signatures, and class names. Excludes are respected.
///
/// Returns `Err` when the buffer is not a well-formed classfile, or when a
/// relocation would make a `CONSTANT_Utf8` entry longer than `u16::MAX` bytes
/// (the class-file format limit). Callers must fail the fat-JAR build rather
/// than shipping corrupt or unrelocated bytecode.
///
/// Callers should not invoke this for `module-info.class` (including
/// multi-release `META-INF/versions/*/module-info.class`); those entries are
/// copied through unchanged — see [`is_module_info_class_entry`].
pub fn relocate_class_bytes(data: &[u8], relocations: &[Relocation]) -> Result<Vec<u8>> {
    if relocations.is_empty() || data.len() < 10 {
        return Ok(data.to_vec());
    }
    rewrite_class_with_relocations(data, relocations)
}

/// True for `module-info.class` and multi-release variants
/// (`META-INF/versions/9/module-info.class`, …). Fat JARs keep these bytes
/// as-is: the rewriter only understands a subset of CP tags and must not
/// fail the build on module descriptors (regression after bug #24).
fn is_module_info_class_entry(zip_path: &str) -> bool {
    zip_path == "module-info.class" || zip_path.ends_with("/module-info.class")
}

/// Whether a ZIP entry should have package relocation applied to its bytecode.
fn should_relocate_class_bytes(zip_path: &str) -> bool {
    zip_path.ends_with(".class") && !is_module_info_class_entry(zip_path)
}

/// Rewrite a classfile buffer, applying package relocations only to UTF-8
/// constant pool entries while preserving all structure and indices.
fn rewrite_class_with_relocations(data: &[u8], relocations: &[Relocation]) -> Result<Vec<u8>> {
    // Header: magic (4) + minor (2) + major (2) + cp_count (2)
    if &data[0..4] != b"\xCA\xFE\xBA\xBE" {
        anyhow::bail!("not a classfile");
    }
    let cp_count = u16::from_be_bytes([data[8], data[9]]) as usize;

    let mut out = Vec::with_capacity(data.len() + 512);
    out.extend_from_slice(&data[0..10]);

    let mut i = 10usize; // current read position in input
    let mut slot: usize = 1; // 1-based constant pool slot counter

    while slot < cp_count {
        if i >= data.len() {
            anyhow::bail!("truncated classfile in constant pool");
        }
        let tag = data[i];
        out.push(tag);
        i += 1;

        match tag {
            1 => {
                // UTF-8: u16 length + length bytes of (modified) UTF-8
                if i + 2 > data.len() {
                    anyhow::bail!("truncated UTF-8 length");
                }
                let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
                if i + 2 + len > data.len() {
                    anyhow::bail!("truncated UTF-8 content");
                }
                let content = &data[i + 2..i + 2 + len];
                let new_content = apply_relocations_to_utf8_content(content, relocations);
                let new_len = new_content.len();
                // JVMS §4.4.7: CONSTANT_Utf8 length is a u16 — never truncate.
                let new_len_u16 = u16::try_from(new_len).map_err(|_| {
                    anyhow::anyhow!(
                        "relocated CONSTANT_Utf8 entry would be {new_len} bytes, \
                         but the class-file format allows at most {}",
                        u16::MAX
                    )
                })?;
                out.extend_from_slice(&new_len_u16.to_be_bytes());
                out.extend_from_slice(&new_content);
                i += 2 + len;
            }
            3 | 4 | 9 | 10 | 11 | 12 | 18 | 19 | 20 => {
                // 4 bytes of payload after tag
                if i + 4 > data.len() {
                    anyhow::bail!("truncated cp entry");
                }
                out.extend_from_slice(&data[i..i + 4]);
                i += 4;
            }
            5 | 6 => {
                // Long / Double: 8 bytes payload, takes two slots in cp_count
                if i + 8 > data.len() {
                    anyhow::bail!("truncated long/double");
                }
                out.extend_from_slice(&data[i..i + 8]);
                i += 8;
                slot += 1; // skip the phantom second slot
            }
            7 | 8 | 16 => {
                // Class, String, MethodType: u16 index
                if i + 2 > data.len() {
                    anyhow::bail!("truncated cp entry");
                }
                out.extend_from_slice(&data[i..i + 2]);
                i += 2;
            }
            15 => {
                // MethodHandle: u1 kind + u2 index
                if i + 3 > data.len() {
                    anyhow::bail!("truncated MethodHandle");
                }
                out.extend_from_slice(&data[i..i + 3]);
                i += 3;
            }
            17 => {
                // Dynamic: u16 bootstrap + u16 name_and_type
                if i + 4 > data.len() {
                    anyhow::bail!("truncated Dynamic");
                }
                out.extend_from_slice(&data[i..i + 4]);
                i += 4;
            }
            _ => {
                anyhow::bail!("unknown constant pool tag {}", tag);
            }
        }
        slot += 1;
    }

    // Copy everything after the constant pool verbatim (access flags onward).
    if i <= data.len() {
        out.extend_from_slice(&data[i..]);
    }
    Ok(out)
}

/// Apply all relocations to the decoded content of one UTF-8 constant pool entry.
/// Replacements are performed for both slash-separated and dot-separated forms
/// and may occur anywhere inside the string (class names, descriptors, signatures).
fn apply_relocations_to_utf8_content(content: &[u8], relocations: &[Relocation]) -> Vec<u8> {
    let Ok(s) = std::str::from_utf8(content) else {
        return content.to_vec();
    };
    let mut result = s.to_string();
    for reloc in relocations {
        let from_slash = reloc.from.replace('.', "/");
        let to_slash = reloc.to.replace('.', "/");
        result = replace_pattern_occurrences(&result, &from_slash, &to_slash, &reloc.excludes);

        let from_dot = reloc.from.clone();
        let to_dot = reloc.to.clone();
        result = replace_pattern_occurrences(&result, &from_dot, &to_dot, &reloc.excludes);
    }
    result.into_bytes()
}

/// Replace all non-overlapping occurrences of `from` inside `s` with `to`,
/// skipping any match whose candidate path (from the match site) is excluded.
fn replace_pattern_occurrences(s: &str, from: &str, to: &str, excludes: &[String]) -> String {
    if from.is_empty() || !s.contains(from) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + (to.len().saturating_sub(from.len())) * 4);
    let mut i = 0usize;
    while i < s.len() {
        if let Some(pos) = s[i..].find(from) {
            let abs = i + pos;
            // Determine a candidate path segment starting at this occurrence
            // for the purpose of exclude matching. Stop at common descriptor terminators.
            let rest = &s[abs..];
            let mut end = rest.len();
            for (idx, ch) in rest.char_indices() {
                if !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '/' || ch == '_' || ch == '$')
                {
                    end = idx;
                    break;
                }
            }
            let candidate = &rest[..end];
            let candidate_for_excl = candidate.replace('.', "/");
            if is_excluded(&candidate_for_excl, excludes) {
                // Keep original bytes for this occurrence
                out.push_str(&s[i..abs + from.len()]);
                i = abs + from.len();
            } else {
                out.push_str(&s[i..abs]);
                out.push_str(to);
                i = abs + from.len();
            }
        } else {
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

/// Simple byte-level find-and-replace (kept for unit tests of the helper and
/// possible future internal use). Handles length-changing replacements.
#[cfg(test)]
fn byte_replace(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || data.len() < from.len() {
        return data.to_vec();
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + from.len() <= data.len() && &data[i..i + from.len()] == from {
            result.extend_from_slice(to);
            i += from.len();
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}

/// Check whether a path matches any exclusion pattern (simple glob: `*` at end).
fn is_excluded(path: &str, excludes: &[String]) -> bool {
    for pattern in excludes {
        let pat_path = pattern.replace('.', "/");
        if pat_path.ends_with('*') {
            let prefix = &pat_path[..pat_path.len() - 1];
            if path.starts_with(prefix) {
                return true;
            }
        } else if path == pat_path {
            return true;
        }
    }
    false
}

/// Merge all META-INF/services files from multiple JARs.
///
/// Returns a map of service file name → merged content (all provider lines
/// concatenated, deduplicated, sorted for determinism).
fn merge_services(
    dep_jars: &[PathBuf],
    relocations: &[Relocation],
) -> Result<BTreeMap<String, String>> {
    let mut services: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for jar_path in dep_jars {
        let file = std::fs::File::open(jar_path)
            .with_context(|| format!("failed to open dep JAR: {}", jar_path.display()))?;
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(_) => continue, // skip non-ZIP files gracefully
        };

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            if name.starts_with("META-INF/services/") && !entry.is_dir() {
                // Fail closed on I/O or non-UTF-8: partial reads used to be merged
                // into META-INF/services and corrupt ServiceLoader descriptors (bug #33).
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).with_context(|| {
                    format!(
                        "failed to read service descriptor '{}' from {}",
                        name,
                        jar_path.display()
                    )
                })?;
                let content = std::str::from_utf8(&bytes).with_context(|| {
                    format!(
                        "service descriptor '{}' in {} is not valid UTF-8",
                        name,
                        jar_path.display()
                    )
                })?;
                let service_name = &name["META-INF/services/".len()..];
                let relocated_service = relocate_service_name(service_name, relocations);
                let providers = services.entry(relocated_service).or_default();
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        let relocated_line = relocate_dotted_name(line, relocations);
                        if !providers.contains(&relocated_line) {
                            providers.push(relocated_line);
                        }
                    }
                }
            }
        }
    }

    // Sort providers for determinism
    let mut result = BTreeMap::new();
    for (service, mut providers) in services {
        providers.sort();
        providers.dedup();
        result.insert(service, providers.join("\n") + "\n");
    }
    Ok(result)
}

/// Apply relocations to a dot-separated name (used for service provider class names).
fn relocate_dotted_name(name: &str, relocations: &[Relocation]) -> String {
    let mut result = name.to_string();
    for reloc in relocations {
        if result.starts_with(&reloc.from) {
            if !is_excluded(&result.replace('.', "/"), &reloc.excludes) {
                result = format!("{}{}", reloc.to, &result[reloc.from.len()..]);
            }
        }
    }
    result
}

/// Apply relocations to a service file name (the filename under META-INF/services/).
fn relocate_service_name(name: &str, relocations: &[Relocation]) -> String {
    relocate_dotted_name(name, relocations)
}

/// Write a deterministic fat/uber JAR that merges the project's own classes
/// and resources with all dependency JARs.
///
/// Properties:
///   - Entries sorted lexicographically (deterministic)
///   - All timestamps set to epoch (reproducible builds)
///   - MANIFEST.MF written first per JAR spec
///   - META-INF/services files from all deps merged
///   - Package relocations applied to class bytecode and resource paths
///   - Duplicate entries resolved: project's own classes win over deps
///   - Directory entries are derived from the ancestor paths of real files
///     (mirroring maven-shade-plugin), not copied from source dirs/JARs —
///     this avoids "empty" placeholder directories such as multi-release
///     module-info package stubs
pub fn write_fat_jar(
    jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    main_class: Option<&str>,
    dep_jars: &[PathBuf],
    build_info: Option<&str>,
    relocations: &[Relocation],
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

        // Use the common builder so Main-Class is folded when long and we have
        // a single implementation for all manifest content in the crate.
        let manifest = build_manifest(main_class, None, None);

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

        // --- Merge META-INF/services from all dependency JARs -------------------
        let merged_services = merge_services(dep_jars, relocations)?;

        // --- Collect project's own classes/resources into entries ----------------
        // Also collect from resources_dir merged into the project's own services.
        let mut project_services: BTreeMap<String, String> = BTreeMap::new();
        let mut entries: BTreeMap<String, EntrySource> = BTreeMap::new();

        for (root, label) in [(Some(classes_dir), "classes"), (resources_dir, "resources")] {
            let Some(root) = root else { continue };
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                let zip_path = entry
                    .path()
                    .strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                if zip_path.is_empty() || zip_path == "META-INF/MANIFEST.MF" {
                    continue;
                }
                if build_info.is_some() && zip_path == "META-INF/build-info.properties" {
                    continue;
                }
                // Fat JARs run as the unnamed module; module-info.class would describe a broken module.
                if zip_path == "module-info.class" {
                    continue;
                }

                // Handle project's own META-INF/services — collect for merging
                if zip_path.starts_with("META-INF/services/") {
                    let service_name = &zip_path["META-INF/services/".len()..];
                    let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
                    let relocated_service = relocate_service_name(service_name, relocations);
                    let existing = project_services.entry(relocated_service).or_default();
                    for line in content.lines() {
                        let line = line.trim();
                        if !line.is_empty() && !line.starts_with('#') {
                            let relocated_line = relocate_dotted_name(line, relocations);
                            if !existing.contains(&relocated_line) {
                                if !existing.is_empty() {
                                    existing.push('\n');
                                }
                                existing.push_str(&relocated_line);
                            }
                        }
                    }
                    continue;
                }

                // Class files take precedence; skip if already inserted from classes.
                if label == "resources" && entries.contains_key(&zip_path) {
                    continue;
                }

                let relocated_path = relocate_path(&zip_path, relocations);
                entries.insert(relocated_path, EntrySource::File(entry.into_path()));
            }
        }

        // --- Collect entries from dependency JARs --------------------------------
        for jar_dep_path in dep_jars {
            let file = std::fs::File::open(jar_dep_path)
                .with_context(|| format!("failed to open dep JAR: {}", jar_dep_path.display()))?;
            let mut archive = match zip::ZipArchive::new(file) {
                Ok(a) => a,
                Err(_) => continue,
            };

            for i in 0..archive.len() {
                let mut entry = match archive.by_index(i) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.is_dir() {
                    continue;
                }
                let name = entry.name().to_string();

                // Skip META-INF that we handle ourselves
                if name == "META-INF/MANIFEST.MF" {
                    continue;
                }
                // Skip META-INF/services — handled via merge_services above
                if name.starts_with("META-INF/services/") {
                    continue;
                }
                // Skip signature files that would invalidate the fat JAR
                if name.starts_with("META-INF/")
                    && (name.ends_with(".SF")
                        || name.ends_with(".DSA")
                        || name.ends_with(".RSA")
                        || name.ends_with(".EC"))
                {
                    continue;
                }
                // Skip a top-level module-info.class from deps: it describes that
                // dependency's own module, not the shaded application. Nested
                // multi-release entries (META-INF/versions/*/module-info.class)
                // are kept, matching maven-shade-plugin's default behaviour.
                if name == "module-info.class" {
                    continue;
                }
                if build_info.is_some() && name == "META-INF/build-info.properties" {
                    continue;
                }

                // Project's own entries win over deps
                let relocated_name = relocate_path(&name, relocations);
                if entries.contains_key(&relocated_name) {
                    continue;
                }

                let mut data = Vec::new();
                let _ = entry.read_to_end(&mut data);

                // Apply package relocations to ordinary class files only — never to
                // module-info.class (incl. multi-release), which we copy through.
                let data = if should_relocate_class_bytes(&relocated_name) {
                    relocate_class_bytes(&data, relocations)
                        .with_context(|| format!("relocating class entry '{relocated_name}'"))?
                } else {
                    data
                };
                entries.insert(relocated_name, EntrySource::Bytes(data));
            }
        }

        // --- Merge META-INF/services: project + deps ----------------------------
        let mut all_services: BTreeMap<String, String> = merged_services;
        for (service, project_content) in &project_services {
            let entry = all_services.entry(service.clone()).or_default();
            // Prepend project's providers (they come first)
            if entry.is_empty() {
                *entry = format!("{}\n", project_content);
            } else {
                // Merge: project providers first, then dep providers
                let mut combined: Vec<String> = Vec::new();
                for line in project_content.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        combined.push(line.to_string());
                    }
                }
                for line in entry.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !combined.contains(&line.to_string()) {
                        combined.push(line.to_string());
                    }
                }
                combined.sort();
                combined.dedup();
                *entry = combined.join("\n") + "\n";
            }
        }

        // Add merged services as entries (their META-INF/services/ ancestor
        // directory is derived below along with every other entry's ancestors).
        for (service_name, content) in &all_services {
            let key = format!("META-INF/services/{}", service_name);
            entries.insert(key, EntrySource::Bytes(content.as_bytes().to_vec()));
        }

        // --- Derive directory entries from file ancestor paths ------------------
        // `META-INF/` is excluded since it was already written above.
        let dir_entries: BTreeSet<String> = entries
            .keys()
            .flat_map(|path| path_ancestors(path))
            .filter(|dir| dir != "META-INF/")
            .collect();
        for dir in dir_entries {
            entries.entry(dir).or_insert(EntrySource::Dir);
        }

        // --- Write all entries sorted lexicographically -------------------------
        for (zip_path, source) in &entries {
            match source {
                EntrySource::Dir => {
                    zip.start_file(zip_path.as_str(), dir_options)
                        .with_context(|| format!("failed to write directory entry {}", zip_path))?;
                }
                EntrySource::File(fs_path) => {
                    let data = std::fs::read(fs_path)
                        .with_context(|| format!("failed to read {}", fs_path.display()))?;
                    // Apply package relocations to ordinary class files only.
                    let data = if should_relocate_class_bytes(zip_path) {
                        relocate_class_bytes(&data, relocations)
                            .with_context(|| format!("relocating class entry '{zip_path}'"))?
                    } else {
                        data
                    };
                    zip.start_file(zip_path.as_str(), options)
                        .with_context(|| format!("failed to start entry {}", zip_path))?;
                    zip.write_all(&data)
                        .with_context(|| format!("failed to write entry {}", zip_path))?;
                }
                EntrySource::Bytes(data) => {
                    zip.start_file(zip_path.as_str(), options)
                        .with_context(|| format!("failed to start entry {}", zip_path))?;
                    zip.write_all(data)
                        .with_context(|| format!("failed to write entry {}", zip_path))?;
                }
            }
        }

        zip.finish().context("failed to finalise fat JAR")?;
    } // drop writer + file for the part

    finalize_staged(&part, jar_path)?;
    Ok(())
}

/// Source for a ZIP entry in the fat JAR.
enum EntrySource {
    /// A directory entry (no data).
    Dir,
    /// Read from a filesystem path (project's own classes/resources).
    File(PathBuf),
    /// In-memory bytes (extracted from a dep JAR, possibly relocated).
    Bytes(Vec<u8>),
}

/// Returns `true` when the fat JAR needs to be rebuilt.
///
/// Checks the fat JAR's mtime against all inputs: classes, resources,
/// dependency JARs, and `Curie.toml`.
pub fn needs_rebuild(
    fat_jar_path: &Path,
    classes_dir: &Path,
    resources_dir: Option<&Path>,
    dep_jars: &[PathBuf],
    toml_path: &Path,
) -> bool {
    let mut inputs = crate::incremental::Inputs::new();
    inputs
        .add_dir(classes_dir)
        .add_dir_opt(resources_dir)
        .add_paths(dep_jars)
        .add_file(toml_path);
    !crate::incremental::Stamp::of(fat_jar_path).covers(&inputs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- byte_replace --------------------------------------------------------

    #[test]
    fn byte_replace_simple() {
        let data = b"hello world hello";
        let result = byte_replace(data, b"hello", b"hi");
        assert_eq!(result, b"hi world hi");
    }

    #[test]
    fn byte_replace_no_match() {
        let data = b"hello world";
        let result = byte_replace(data, b"xyz", b"abc");
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn byte_replace_empty_from() {
        let data = b"hello";
        let result = byte_replace(data, b"", b"x");
        assert_eq!(result, b"hello");
    }

    #[test]
    fn byte_replace_longer_replacement() {
        let data = b"a.b.c";
        let result = byte_replace(data, b"a", b"xyz");
        assert_eq!(result, b"xyz.b.c");
    }

    // --- relocate_path -------------------------------------------------------

    #[test]
    fn relocate_path_no_relocations() {
        let result = relocate_path("com/google/common/Foo.class", &[]);
        assert_eq!(result, "com/google/common/Foo.class");
    }

    #[test]
    fn relocate_path_matches() {
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];
        let result = relocate_path("com/google/common/collect/ImmutableList.class", &relocs);
        assert_eq!(
            result,
            "shaded/com/google/common/collect/ImmutableList.class"
        );
    }

    #[test]
    fn relocate_path_no_match() {
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];
        let result = relocate_path("org/example/Foo.class", &relocs);
        assert_eq!(result, "org/example/Foo.class");
    }

    #[test]
    fn relocate_path_with_exclude() {
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec!["com.google.common.annotations.*".into()],
        }];
        // Excluded path should not be relocated
        let result = relocate_path("com/google/common/annotations/Nullable.class", &relocs);
        assert_eq!(result, "com/google/common/annotations/Nullable.class");
        // Non-excluded path should be relocated
        let result2 = relocate_path("com/google/common/collect/ImmutableList.class", &relocs);
        assert_eq!(
            result2,
            "shaded/com/google/common/collect/ImmutableList.class"
        );
    }

    // --- relocate_class_bytes -----------------------------------------------

    /// Construct a minimal classfile containing the provided UTF-8 constant
    /// pool entries (in addition to a tiny set of bootstrap entries for
    /// java/lang/Object and the class itself). Returns the bytes and the
    /// 1-based indices of the inserted extra UTF-8 entries.
    fn make_minimal_class_with_utf8s(extra_utf8s: &[&str]) -> (Vec<u8>, Vec<u16>) {
        let mut cp_entries: Vec<Vec<u8>> = Vec::new();

        // Slot 1: UTF8 "java/lang/Object"
        cp_entries.push(encode_utf8_cp("java/lang/Object"));
        // Slot 2: Class #1
        cp_entries.push(encode_class_cp(1));
        // Slot 3: UTF8 for our synthetic class name
        cp_entries.push(encode_utf8_cp("com/example/TestRelocated"));
        // Slot 4: Class #3  (this class)
        cp_entries.push(encode_class_cp(3));

        let mut extra_indices = Vec::new();
        let mut next_slot: u16 = 5;
        for s in extra_utf8s {
            extra_indices.push(next_slot);
            cp_entries.push(encode_utf8_cp(s));
            next_slot += 1;
        }

        // Header constant_pool_count is one greater than the highest index.
        let num_logical_slots = 4 + extra_utf8s.len();
        let cp_count = (num_logical_slots as u16) + 1;

        let mut out = Vec::new();
        // magic
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        // minor, major
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&65u16.to_be_bytes()); // Java 21
                                                     // cp_count (number of slots)
        out.extend_from_slice(&cp_count.to_be_bytes());

        // Write the cp entries we built (each already includes its tag)
        for e in &cp_entries {
            out.extend_from_slice(e);
        }

        // Minimal body: public super class, this_class -> #4, super -> #2
        out.extend_from_slice(&0x0021u16.to_be_bytes()); // access
        out.extend_from_slice(&4u16.to_be_bytes()); // this_class
        out.extend_from_slice(&2u16.to_be_bytes()); // super_class
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        out.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        out.extend_from_slice(&0u16.to_be_bytes()); // methods_count
        out.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

        (out, extra_indices)
    }

    fn encode_utf8_cp(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        let mut v = Vec::new();
        v.push(1u8); // tag UTF8
        v.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        v.extend_from_slice(bytes);
        v
    }

    fn encode_class_cp(name_index: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(7u8); // tag Class
        v.extend_from_slice(&name_index.to_be_bytes());
        v
    }

    fn parse_first_utf8_after_bootstrap(class_bytes: &[u8], extra_idx: u16) -> Option<Vec<u8>> {
        // Walk the CP of a classfile produced by make_minimal... and return the
        // content of the UTF8 at the given extra index.
        if class_bytes.len() < 10 {
            return None;
        }
        if &class_bytes[0..4] != b"\xCA\xFE\xBA\xBE" {
            return None;
        }
        let cp_count = u16::from_be_bytes([class_bytes[8], class_bytes[9]]) as usize;
        let mut i = 10usize;
        let mut slot: usize = 1;
        while slot < cp_count {
            if i >= class_bytes.len() {
                return None;
            }
            let tag = class_bytes[i];
            i += 1;
            if tag == 1 {
                if i + 2 > class_bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([class_bytes[i], class_bytes[i + 1]]) as usize;
                let content = class_bytes[i + 2..i + 2 + len].to_vec();
                if slot == extra_idx as usize {
                    return Some(content);
                }
                i += 2 + len;
            } else if tag == 7 || tag == 8 || tag == 16 {
                i += 2;
            } else if tag == 15 {
                i += 3;
            } else if tag == 17 {
                i += 4;
            } else if tag == 5 || tag == 6 {
                i += 8;
                slot += 1;
            } else {
                i += 4; // 3,4,9,10,11,12,18,19,20
            }
            slot += 1;
        }
        None
    }

    #[test]
    fn relocate_class_bytes_no_relocations() {
        let (data, _) = make_minimal_class_with_utf8s(&["com/google/common/Foo"]);
        let result = relocate_class_bytes(&data, &[]).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn relocate_class_bytes_updates_length_prefix_for_longer_name() {
        let pattern = "com/google/common/collect/ImmutableList";
        let (data, indices) = make_minimal_class_with_utf8s(&[pattern]);
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "com.example.fatjar.shaded.com.google.common".into(),
            excludes: vec![],
        }];
        let result = relocate_class_bytes(&data, &relocs).unwrap();
        // Must still be recognized as a classfile and be re-parsable
        assert_eq!(&result[0..4], b"\xCA\xFE\xBA\xBE");
        let new_content =
            parse_first_utf8_after_bootstrap(&result, indices[0]).expect("utf8 present");
        let expected = "com/example/fatjar/shaded/com/google/common/collect/ImmutableList";
        assert_eq!(new_content, expected.as_bytes());
        // Length prefix must have been updated (original len 39 -> new longer)
        assert!(new_content.len() > pattern.len());
    }

    #[test]
    fn relocate_class_bytes_replaces_inside_descriptors() {
        // A realistic descriptor fragment as it would appear in a NameAndType or signature
        let desc = "(Ljava/lang/Object;)Lcom/google/common/collect/ImmutableList;";
        let (data, indices) = make_minimal_class_with_utf8s(&[desc]);
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];
        let result = relocate_class_bytes(&data, &relocs).unwrap();
        let new_content = parse_first_utf8_after_bootstrap(&result, indices[0]).unwrap();
        assert!(std::str::from_utf8(&new_content)
            .unwrap()
            .contains("shaded/com/google/common/collect/ImmutableList"));
    }

    #[test]
    fn relocate_class_bytes_respects_excludes() {
        let target = "com/google/common/annotations/Nullable";
        let (data, indices) = make_minimal_class_with_utf8s(&[target]);
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec!["com.google.common.annotations.*".into()],
        }];
        let result = relocate_class_bytes(&data, &relocs).unwrap();
        let new_content = parse_first_utf8_after_bootstrap(&result, indices[0]).unwrap();
        // Should remain unchanged because of the exclude
        assert_eq!(new_content, target.as_bytes());
    }

    #[test]
    fn relocate_class_bytes_replaces_dotted_form() {
        // Some UTF8 entries (e.g. in annotations or legacy) can contain dotted names
        let dotted = "com.google.common.base.Preconditions";
        let (data, indices) = make_minimal_class_with_utf8s(&[dotted]);
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];
        let result = relocate_class_bytes(&data, &relocs).unwrap();
        let new_content = parse_first_utf8_after_bootstrap(&result, indices[0]).unwrap();
        assert_eq!(
            std::str::from_utf8(&new_content).unwrap(),
            "shaded.com.google.common.base.Preconditions"
        );
    }

    #[test]
    fn relocate_class_bytes_errors_when_utf8_exceeds_u16_max() {
        // Use a token that does not appear in bootstrap CP strings
        // (e.g. "java/lang/Object", "com/example/TestRelocated").
        let (data, _) = make_minimal_class_with_utf8s(&["Q"]);
        let to = "Z".repeat(u16::MAX as usize + 1);
        let relocs = vec![Relocation {
            from: "Q".into(),
            to,
            excludes: vec![],
        }];
        let err = relocate_class_bytes(&data, &relocs).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("CONSTANT_Utf8") && msg.contains(&u16::MAX.to_string()),
            "expected limit error, got: {msg}"
        );
        // Must not silently return the original (unrelocated) class bytes.
        assert!(relocate_class_bytes(&data, &relocs).is_err());
    }

    #[test]
    fn relocate_class_bytes_allows_utf8_at_exactly_u16_max() {
        // Content "Q" (1 byte) → to of length 65535 keeps the entry at the limit.
        let (data, indices) = make_minimal_class_with_utf8s(&["Q"]);
        let to = "Z".repeat(u16::MAX as usize);
        let relocs = vec![Relocation {
            from: "Q".into(),
            to: to.clone(),
            excludes: vec![],
        }];
        let result = relocate_class_bytes(&data, &relocs).unwrap();
        assert_eq!(&result[0..4], b"\xCA\xFE\xBA\xBE");
        let new_content = parse_first_utf8_after_bootstrap(&result, indices[0]).unwrap();
        assert_eq!(new_content.len(), u16::MAX as usize);
        assert_eq!(new_content, to.as_bytes());
    }

    #[test]
    fn relocate_class_bytes_rejects_non_classfile() {
        let err = relocate_class_bytes(
            b"not-a-classfile!!!!",
            &[Relocation {
                from: "a".into(),
                to: "b".into(),
                excludes: vec![],
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a classfile"), "got: {err}");
    }

    // --- is_excluded ---------------------------------------------------------

    #[test]
    fn is_excluded_no_excludes() {
        assert!(!is_excluded("com/example/Foo", &[]));
    }

    #[test]
    fn is_excluded_glob_matches() {
        let excludes = vec!["com.example.api.*".to_string()];
        assert!(is_excluded("com/example/api/Foo", &excludes));
        assert!(!is_excluded("com/example/impl/Bar", &excludes));
    }

    #[test]
    fn is_excluded_exact_match() {
        let excludes = vec!["com.example.Foo".to_string()];
        assert!(is_excluded("com/example/Foo", &excludes));
        assert!(!is_excluded("com/example/Bar", &excludes));
    }

    // --- merge_services ------------------------------------------------------

    #[test]
    fn merge_services_from_multiple_jars() {
        let tmp = tempfile::tempdir().unwrap();

        // Create two dep JARs with overlapping services
        let jar1 = create_test_jar(
            tmp.path(),
            "dep1.jar",
            &[("META-INF/services/java.sql.Driver", b"com.db1.Driver\n")],
        );
        let jar2 = create_test_jar(
            tmp.path(),
            "dep2.jar",
            &[("META-INF/services/java.sql.Driver", b"com.db2.Driver\n")],
        );

        let merged = merge_services(&[jar1, jar2], &[]).unwrap();
        let content = merged.get("java.sql.Driver").expect("service should exist");
        assert!(content.contains("com.db1.Driver"));
        assert!(content.contains("com.db2.Driver"));
    }

    #[test]
    fn merge_services_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();

        let jar1 = create_test_jar(
            tmp.path(),
            "dep1.jar",
            &[("META-INF/services/java.sql.Driver", b"com.db1.Driver\n")],
        );
        let jar2 = create_test_jar(
            tmp.path(),
            "dep2.jar",
            &[("META-INF/services/java.sql.Driver", b"com.db1.Driver\n")],
        );

        let merged = merge_services(&[jar1, jar2], &[]).unwrap();
        let content = merged.get("java.sql.Driver").unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim() == "com.db1.Driver")
            .count();
        assert_eq!(count, 1, "duplicates should be removed");
    }

    #[test]
    fn merge_services_with_relocations() {
        let tmp = tempfile::tempdir().unwrap();

        let jar1 = create_test_jar(
            tmp.path(),
            "dep1.jar",
            &[(
                "META-INF/services/com.google.inject.Module",
                b"com.google.inject.BuiltinModule\n",
            )],
        );

        let relocs = vec![Relocation {
            from: "com.google.inject".into(),
            to: "shaded.com.google.inject".into(),
            excludes: vec![],
        }];

        let merged = merge_services(&[jar1], &relocs).unwrap();
        // Service file name should be relocated
        assert!(merged.contains_key("shaded.com.google.inject.Module"));
        // Provider class name should be relocated
        let content = merged.get("shaded.com.google.inject.Module").unwrap();
        assert!(content.contains("shaded.com.google.inject.BuiltinModule"));
    }

    #[test]
    fn merge_services_errors_on_non_utf8_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = create_test_jar(
            tmp.path(),
            "bad-services.jar",
            &[(
                "META-INF/services/com.example.Spi",
                b"com.ok.Provider\n\xff\xfe\n",
            )],
        );

        let err = merge_services(&[jar], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("META-INF/services/com.example.Spi")
                && (msg.contains("UTF-8") || msg.contains("utf-8")),
            "expected UTF-8 error naming the entry, got: {msg}"
        );
    }

    #[test]
    fn merge_services_allows_empty_utf8_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = create_test_jar(
            tmp.path(),
            "empty-services.jar",
            &[("META-INF/services/com.example.Empty", b"")],
        );
        let merged = merge_services(&[jar], &[]).unwrap();
        // Empty file yields an empty provider list → one trailing newline entry.
        assert_eq!(
            merged.get("com.example.Empty").map(String::as_str),
            Some("\n")
        );
    }

    // --- write_fat_jar (integration) ----------------------------------------

    #[test]
    fn fat_jar_contains_project_and_dep_classes() {
        let tmp = tempfile::tempdir().unwrap();

        // Project classes
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(classes_dir.join("com/example")).unwrap();
        std::fs::write(
            classes_dir.join("com/example/App.class"),
            b"\xca\xfe\xba\xbe",
        )
        .unwrap();

        // Dep JAR with its own class
        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[("org/lib/Util.class", b"\xca\xfe\xba\xbe")],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            None,
            Some("com.example.App"),
            &[dep_jar],
            None,
            &[],
        )
        .unwrap();

        let names = zip_entry_names(&std::fs::read(&fat_path).unwrap());
        assert!(names.contains(&"com/example/App.class".to_string()));
        assert!(names.contains(&"org/lib/Util.class".to_string()));
    }

    #[test]
    fn fat_jar_project_classes_win_over_dep_classes() {
        let tmp = tempfile::tempdir().unwrap();

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(classes_dir.join("com/example")).unwrap();
        std::fs::write(
            classes_dir.join("com/example/Conflict.class"),
            b"PROJECT_VERSION",
        )
        .unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[("com/example/Conflict.class", b"DEP_VERSION")],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(&fat_path, &classes_dir, None, None, &[dep_jar], None, &[]).unwrap();

        let content = zip_entry_content(
            &std::fs::read(&fat_path).unwrap(),
            "com/example/Conflict.class",
        );
        assert_eq!(content.as_bytes(), b"PROJECT_VERSION");
    }

    #[test]
    fn fat_jar_has_no_class_path_header() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("Main.class"), b"\xca\xfe\xba\xbe").unwrap();

        let dep_jar = create_test_jar(tmp.path(), "dep.jar", &[("Dep.class", b"\xca\xfe\xba\xbe")]);

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            None,
            Some("Main"),
            &[dep_jar],
            None,
            &[],
        )
        .unwrap();

        let bytes = std::fs::read(&fat_path).unwrap();
        let manifest = zip_entry_content(&bytes, "META-INF/MANIFEST.MF");
        assert!(
            !manifest.contains("Class-Path"),
            "fat JAR must not have Class-Path header"
        );
        assert!(manifest.contains("Main-Class: Main"));
    }

    #[test]
    fn fat_jar_skips_signature_files() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[
                ("org/lib/Lib.class", b"\xca\xfe\xba\xbe"),
                ("META-INF/BCRYPT.SF", b"signature"),
                ("META-INF/BCRYPT.DSA", b"signature"),
                ("META-INF/BCRYPT.RSA", b"signature"),
            ],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(&fat_path, &classes_dir, None, None, &[dep_jar], None, &[]).unwrap();

        let names = zip_entry_names(&std::fs::read(&fat_path).unwrap());
        assert!(!names
            .iter()
            .any(|n| n.ends_with(".SF") || n.ends_with(".DSA") || n.ends_with(".RSA")));
        assert!(names.contains(&"org/lib/Lib.class".to_string()));
    }

    #[test]
    fn fat_jar_skips_module_info() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[
                ("module-info.class", b"\xca\xfe"),
                ("org/lib/Lib.class", b"\xca\xfe\xba\xbe"),
            ],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(&fat_path, &classes_dir, None, None, &[dep_jar], None, &[]).unwrap();

        let names = zip_entry_names(&std::fs::read(&fat_path).unwrap());
        assert!(!names.contains(&"module-info.class".to_string()));
        assert!(names.contains(&"org/lib/Lib.class".to_string()));
    }

    /// Minimal classfile whose constant pool uses unsupported tag 46 — mirrors
    /// real multi-release `module-info.class` entries that trip the rewriter
    /// (see fat-jar-demo regression after bug #24).
    fn classfile_with_unknown_cp_tag_46() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&65u16.to_be_bytes()); // major (Java 21)
        out.extend_from_slice(&2u16.to_be_bytes()); // constant_pool_count
        out.push(46u8); // unknown tag at slot 1
        out.extend_from_slice(&[0u8; 16]); // padding so the buffer is non-trivial
        out
    }

    /// Regression for examples/fat-jar-demo after bug #24: multi-release
    /// `META-INF/versions/*/module-info.class` must be kept as-is under
    /// package relocation, not run through `relocate_class_bytes` (which fails
    /// on unknown constant-pool tags such as 46).
    #[test]
    fn fat_jar_keeps_multi_release_module_info_under_relocations() {
        let module_info = classfile_with_unknown_cp_tag_46();
        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];
        // Preconditions: the rewriter cannot handle this classfile.
        let rewrite_err = relocate_class_bytes(&module_info, &relocs).unwrap_err();
        assert!(
            rewrite_err
                .to_string()
                .contains("unknown constant pool tag"),
            "got: {rewrite_err}"
        );

        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let mr_path = "META-INF/versions/9/module-info.class";
        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[
                (mr_path, module_info.as_slice()),
                (
                    "com/google/common/collect/ImmutableList.class",
                    b"\xca\xfe\xba\xbe",
                ),
            ],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            None,
            None,
            &[dep_jar],
            None,
            &relocs,
        )
        .expect("fat JAR must succeed with multi-release module-info under relocations");

        let bytes = std::fs::read(&fat_path).unwrap();
        let names = zip_entry_names(&bytes);
        assert!(
            names.iter().any(|n| n == mr_path),
            "multi-release module-info must be kept; names={names:?}"
        );
        assert_eq!(
            zip_entry_content_bytes(&bytes, mr_path),
            module_info,
            "module-info bytes must be copied through unchanged"
        );
        assert!(names.contains(&"shaded/com/google/common/collect/ImmutableList.class".to_string()));
    }

    #[test]
    fn fat_jar_merges_services_with_project() {
        let tmp = tempfile::tempdir().unwrap();

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let resources_dir = tmp.path().join("resources");
        std::fs::create_dir_all(resources_dir.join("META-INF/services")).unwrap();
        std::fs::write(
            resources_dir.join("META-INF/services/javax.sql.DataSource"),
            "com.myapp.MyDataSource\n",
        )
        .unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[(
                "META-INF/services/javax.sql.DataSource",
                b"com.lib.LibDataSource\n",
            )],
        );

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            Some(&resources_dir),
            None,
            &[dep_jar],
            None,
            &[],
        )
        .unwrap();

        let bytes = std::fs::read(&fat_path).unwrap();
        let content = zip_entry_content(&bytes, "META-INF/services/javax.sql.DataSource");
        assert!(content.contains("com.myapp.MyDataSource"));
        assert!(content.contains("com.lib.LibDataSource"));
    }

    #[test]
    fn fat_jar_with_relocations() {
        let tmp = tempfile::tempdir().unwrap();

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[
                (
                    "com/google/common/collect/ImmutableList.class",
                    b"\xca\xfe\xba\xbe",
                ),
                (
                    "com/google/common/base/Preconditions.class",
                    b"\xca\xfe\xba\xbe",
                ),
            ],
        );

        let relocs = vec![Relocation {
            from: "com.google.common".into(),
            to: "shaded.com.google.common".into(),
            excludes: vec![],
        }];

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            None,
            None,
            &[dep_jar],
            None,
            &relocs,
        )
        .unwrap();

        let names = zip_entry_names(&std::fs::read(&fat_path).unwrap());
        assert!(names.contains(&"shaded/com/google/common/collect/ImmutableList.class".to_string()));
        assert!(names.contains(&"shaded/com/google/common/base/Preconditions.class".to_string()));
        // Original unrelocated path should NOT exist
        assert!(!names.contains(&"com/google/common/collect/ImmutableList.class".to_string()));
    }

    #[test]
    fn fat_jar_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(classes_dir.join("com/example")).unwrap();
        std::fs::write(
            classes_dir.join("com/example/App.class"),
            b"\xca\xfe\xba\xbe",
        )
        .unwrap();

        let dep_jar = create_test_jar(
            tmp.path(),
            "dep.jar",
            &[
                ("org/lib/A.class", b"\xca\xfe\xba\xbe"),
                ("org/lib/B.class", b"\xca\xfe\xba\xbe"),
            ],
        );

        // Build twice
        let fat1 = tmp.path().join("fat1.jar");
        let fat2 = tmp.path().join("fat2.jar");

        write_fat_jar(
            &fat1,
            &classes_dir,
            None,
            Some("com.example.App"),
            &[dep_jar.clone()],
            None,
            &[],
        )
        .unwrap();
        write_fat_jar(
            &fat2,
            &classes_dir,
            None,
            Some("com.example.App"),
            &[dep_jar],
            None,
            &[],
        )
        .unwrap();

        let bytes1 = std::fs::read(&fat1).unwrap();
        let bytes2 = std::fs::read(&fat2).unwrap();
        assert_eq!(
            bytes1, bytes2,
            "fat JAR must be deterministic (identical inputs → identical output)"
        );
    }

    #[test]
    fn fat_jar_with_build_info() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            None,
            None,
            &[],
            Some("git.commit.id=abc123\n"),
            &[],
        )
        .unwrap();

        let bytes = std::fs::read(&fat_path).unwrap();
        let names = zip_entry_names(&bytes);
        assert!(names.contains(&"META-INF/build-info.properties".to_string()));
        let content = zip_entry_content(&bytes, "META-INF/build-info.properties");
        assert_eq!(content, "git.commit.id=abc123\n");
    }

    #[test]
    fn fat_jar_resources_included() {
        let tmp = tempfile::tempdir().unwrap();
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"\xca\xfe\xba\xbe").unwrap();

        let resources_dir = tmp.path().join("resources");
        std::fs::create_dir_all(&resources_dir).unwrap();
        std::fs::write(resources_dir.join("application.properties"), b"key=value\n").unwrap();

        let fat_path = tmp.path().join("fat.jar");
        write_fat_jar(
            &fat_path,
            &classes_dir,
            Some(&resources_dir),
            None,
            &[],
            None,
            &[],
        )
        .unwrap();

        let bytes = std::fs::read(&fat_path).unwrap();
        let content = zip_entry_content(&bytes, "application.properties");
        assert_eq!(content, "key=value\n");
    }

    // --- needs_rebuild -------------------------------------------------------

    #[test]
    fn needs_rebuild_no_fat_jar() {
        let tmp = tempfile::tempdir().unwrap();
        let fat_jar = tmp.path().join("fat.jar"); // does not exist
        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        std::fs::write(classes_dir.join("App.class"), b"bytecode").unwrap();
        let toml = tmp.path().join("Curie.toml");
        std::fs::write(&toml, b"[application]").unwrap();
        assert!(needs_rebuild(&fat_jar, &classes_dir, None, &[], &toml));
    }

    #[test]
    fn needs_rebuild_false_when_up_to_date() {
        use std::time::{Duration, SystemTime};
        let tmp = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        let class_file = classes_dir.join("App.class");
        std::fs::write(&class_file, b"bytecode").unwrap();
        filetime::set_file_mtime(&class_file, filetime::FileTime::from_system_time(base)).unwrap();

        let toml = tmp.path().join("Curie.toml");
        std::fs::write(&toml, b"[application]").unwrap();
        filetime::set_file_mtime(&toml, filetime::FileTime::from_system_time(base)).unwrap();

        let fat_jar = tmp.path().join("fat.jar");
        std::fs::write(&fat_jar, b"jar").unwrap();
        filetime::set_file_mtime(
            &fat_jar,
            filetime::FileTime::from_system_time(base + Duration::from_secs(5)),
        )
        .unwrap();

        assert!(!needs_rebuild(&fat_jar, &classes_dir, None, &[], &toml));
    }

    #[test]
    fn needs_rebuild_true_when_dep_newer() {
        use std::time::{Duration, SystemTime};
        let tmp = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);

        let classes_dir = tmp.path().join("classes");
        std::fs::create_dir_all(&classes_dir).unwrap();
        let class_file = classes_dir.join("App.class");
        std::fs::write(&class_file, b"bytecode").unwrap();
        filetime::set_file_mtime(&class_file, filetime::FileTime::from_system_time(base)).unwrap();

        let toml = tmp.path().join("Curie.toml");
        std::fs::write(&toml, b"[application]").unwrap();
        filetime::set_file_mtime(&toml, filetime::FileTime::from_system_time(base)).unwrap();

        let fat_jar = tmp.path().join("fat.jar");
        std::fs::write(&fat_jar, b"jar").unwrap();
        filetime::set_file_mtime(
            &fat_jar,
            filetime::FileTime::from_system_time(base + Duration::from_secs(5)),
        )
        .unwrap();

        // A dep JAR that's newer than the fat JAR
        let dep = tmp.path().join("dep.jar");
        std::fs::write(&dep, b"dep").unwrap();
        filetime::set_file_mtime(
            &dep,
            filetime::FileTime::from_system_time(base + Duration::from_secs(10)),
        )
        .unwrap();

        assert!(needs_rebuild(&fat_jar, &classes_dir, None, &[dep], &toml));
    }

    // --- filter_fat_jar_deps -------------------------------------------------

    #[test]
    fn filter_excludes_deps_with_fat_jar_false() {
        use crate::descriptor::*;
        use std::collections::BTreeMap;

        let mut deps = BTreeMap::new();
        deps.insert(
            "org.example:included-lib".to_string(),
            DependencyValue::Version("1.0".to_string()),
        );
        deps.insert(
            "org.example:excluded-lib".to_string(),
            DependencyValue::Detailed(DependencyDetailed {
                version: "1.0".to_string(),
                repository: None,
                java_agent: false,
                exclusions: vec![],
                shade: Some(false),
                relocations: vec![],
                allow_version_conflict: false,
            }),
        );

        let desc = Descriptor {
            kind: DescriptorKind::Application(Application {
                name: "test".to_string(),
                version: "1.0".to_string(),
                group_id: None,
                main_class: None,
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            spock: Spock::default(),
            native_image: NativeImage::default(),
            jlink: Jlink::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            fat_jar: FatJar::default(),
            dependencies: deps,
            test_dependencies: BTreeMap::new(),
            repositories: vec![],
            bom_imports: BTreeMap::new(),
            test_bom_imports: BTreeMap::new(),
            inherited_bom_imports: BTreeMap::new(),
            inherited_test_bom_imports: BTreeMap::new(),
            workspace_dependencies: BTreeMap::new(),
            annotation_processors: BTreeMap::new(),
            test_annotation_processors: BTreeMap::new(),
            inherited_annotation_processors: BTreeMap::new(),
            inherited_test_annotation_processors: BTreeMap::new(),
            annotation_processor_options: BTreeMap::new(),
            test_annotation_processor_options: BTreeMap::new(),
            inherited_annotation_processor_options: BTreeMap::new(),
            inherited_test_annotation_processor_options: BTreeMap::new(),
            publish: PublishConfig::default(),
            plugins: BTreeMap::new(),
            maven: MavenConfig::default(),
            modules: crate::descriptor::ModulesConfig::default(),
            resources: crate::descriptor::Resources::default(),
            test_resources: crate::descriptor::Resources::default(),
        };

        let all_jars = vec![
            PathBuf::from("/m2/included-lib-1.0.jar"),
            PathBuf::from("/m2/excluded-lib-1.0.jar"),
            PathBuf::from("/m2/transitive-1.0.jar"),
        ];

        let filtered = filter_fat_jar_deps(&all_jars, &desc);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains(&PathBuf::from("/m2/included-lib-1.0.jar")));
        assert!(filtered.contains(&PathBuf::from("/m2/transitive-1.0.jar")));
        assert!(!filtered.contains(&PathBuf::from("/m2/excluded-lib-1.0.jar")));
    }

    /// Regression for bug #23: excluding `com.example:json` must not drop
    /// `json-path-*.jar` / `json-smart-*.jar` just because they share a prefix.
    #[test]
    fn filter_does_not_exclude_jars_with_longer_artifact_prefix() {
        use crate::descriptor::*;
        use std::collections::BTreeMap;

        let mut deps = BTreeMap::new();
        deps.insert(
            "com.example:json".to_string(),
            DependencyValue::Detailed(DependencyDetailed {
                version: "1.0".to_string(),
                repository: None,
                java_agent: false,
                exclusions: vec![],
                shade: Some(false),
                relocations: vec![],
                allow_version_conflict: false,
            }),
        );

        let desc = Descriptor {
            kind: DescriptorKind::Application(Application {
                name: "test".to_string(),
                version: "1.0".to_string(),
                group_id: None,
                main_class: None,
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            spock: Spock::default(),
            native_image: NativeImage::default(),
            jlink: Jlink::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            fat_jar: FatJar {
                enabled: true,
                shade_all: true,
                ..FatJar::default()
            },
            dependencies: deps,
            test_dependencies: BTreeMap::new(),
            repositories: vec![],
            bom_imports: BTreeMap::new(),
            test_bom_imports: BTreeMap::new(),
            inherited_bom_imports: BTreeMap::new(),
            inherited_test_bom_imports: BTreeMap::new(),
            workspace_dependencies: BTreeMap::new(),
            annotation_processors: BTreeMap::new(),
            test_annotation_processors: BTreeMap::new(),
            inherited_annotation_processors: BTreeMap::new(),
            inherited_test_annotation_processors: BTreeMap::new(),
            annotation_processor_options: BTreeMap::new(),
            test_annotation_processor_options: BTreeMap::new(),
            inherited_annotation_processor_options: BTreeMap::new(),
            inherited_test_annotation_processor_options: BTreeMap::new(),
            publish: PublishConfig::default(),
            plugins: BTreeMap::new(),
            maven: MavenConfig::default(),
            modules: crate::descriptor::ModulesConfig::default(),
            resources: crate::descriptor::Resources::default(),
            test_resources: crate::descriptor::Resources::default(),
        };

        let all_jars = vec![
            PathBuf::from("/m2/json-1.0.jar"),
            PathBuf::from("/m2/json-path-0.9.jar"),
            PathBuf::from("/m2/json-smart-2.4.jar"),
            PathBuf::from("/m2/json-1.0-tests.jar"), // classifier of excluded dep
        ];

        let filtered = filter_fat_jar_deps(&all_jars, &desc);
        assert!(
            !filtered.iter().any(|p| p.ends_with("json-1.0.jar")),
            "exact excluded artifact-version must be dropped"
        );
        assert!(
            !filtered.iter().any(|p| p.ends_with("json-1.0-tests.jar")),
            "classifier JAR of excluded dep must be dropped"
        );
        assert!(
            filtered.iter().any(|p| p.ends_with("json-path-0.9.jar")),
            "json-path must not be treated as artifact json"
        );
        assert!(
            filtered.iter().any(|p| p.ends_with("json-smart-2.4.jar")),
            "json-smart must not be treated as artifact json"
        );
    }

    #[test]
    fn jar_file_matches_artifact_version_boundaries() {
        assert!(jar_file_matches_artifact_version(
            "json-1.0.jar",
            "json",
            "1.0"
        ));
        assert!(jar_file_matches_artifact_version(
            "json-1.0-tests.jar",
            "json",
            "1.0"
        ));
        assert!(!jar_file_matches_artifact_version(
            "json-path-0.9.jar",
            "json",
            "1.0"
        ));
        assert!(!jar_file_matches_artifact_version(
            "json-1.0.jar",
            "json",
            "2.0"
        ));
        assert!(!jar_file_matches_artifact_version(
            "json-1.0.jar",
            "json",
            ""
        ));
    }

    #[test]
    fn jar_matches_direct_dep_via_m2_layout_when_version_empty() {
        let jar = PathBuf::from("/home/u/.m2/repository/com/example/json/1.0/json-1.0.jar");
        assert!(jar_matches_direct_dep(&jar, "com.example:json", ""));
        assert!(!jar_matches_direct_dep(&jar, "com.example:json-path", ""));
        let other =
            PathBuf::from("/home/u/.m2/repository/com/example/json-path/0.9/json-path-0.9.jar");
        assert!(!jar_matches_direct_dep(&other, "com.example:json", ""));
        assert!(jar_matches_direct_dep(&other, "com.example:json-path", ""));
    }

    // --- test helpers --------------------------------------------------------

    fn create_test_jar(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(epoch());

        for (entry_name, data) in entries {
            zip.start_file(*entry_name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
        use std::io::Cursor;
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_owned())
            .collect()
    }

    fn zip_entry_content(bytes: &[u8], name: &str) -> String {
        use std::io::Cursor;
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        content
    }

    fn zip_entry_content_bytes(bytes: &[u8], name: &str) -> Vec<u8> {
        use std::io::Cursor;
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        content
    }
}
