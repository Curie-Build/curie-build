//! Build `*-sources.jar` files — reproducible JARs containing the project's
//! Java/Kotlin source files at their package-relative paths.
//!
//! Source-root handling mirrors the compiler:
//!   * **Maven-layout** (`src/main/java`, `src/main/kotlin`): entries land at
//!     their path relative to the root (e.g. `src/main/java/com/foo/Bar.java`
//!     → `com/foo/Bar.java` in the jar).
//!   * **Flat-package** (`src/com.example.foo/`): the dot-named directory IS
//!     the package, so files inside it become `com/example/foo/<filename>` in
//!     the jar (matching what `javac` does at compile time).

use crate::compile::pkg_prefix_for_src_root;
use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0).expect("epoch constant is valid")
}

/// Write a deterministic `*-sources.jar` containing every `.java`/`.kt` source
/// file under `src_roots`, plus any files under `resources_dir` (Maven
/// convention).  Entries are sorted, timestamped to EPOCH, deflated.
pub fn write_sources_jar(
    jar_path: &Path,
    src_roots: &[PathBuf],
    resources_dir: Option<&Path>,
) -> Result<()> {
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("cannot create {}", jar_path.display()))?;
    let mut zip = ZipWriter::new(file);

    let file_opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(epoch())
        .unix_permissions(0o644);

    let dir_opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(epoch())
        .unix_permissions(0o755);

    // MANIFEST.MF (must be first).
    zip.start_file("META-INF/", dir_opts)
        .context("failed to write META-INF/ entry")?;
    zip.start_file("META-INF/MANIFEST.MF", file_opts)
        .context("failed to start MANIFEST.MF entry")?;
    zip.write_all(b"Manifest-Version: 1.0\r\n\r\n")
        .context("failed to write MANIFEST.MF")?;

    // Collect (entry_path → fs_path) from all source roots and the resources dir.
    let mut entries: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();

    for root in src_roots {
        let pkg_prefix_dotted = pkg_prefix_for_src_root(root);
        // Convert "com.example.foo" → "com/example/foo" for jar paths.
        let pkg_prefix_slashed = pkg_prefix_dotted.replace('.', "/");

        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let fs_path = entry.path().to_path_buf();
            // Only Java + Kotlin sources.
            let is_source = matches!(
                fs_path.extension().and_then(|s| s.to_str()),
                Some("java") | Some("kt") | Some("groovy")
            );
            if !is_source {
                continue;
            }
            let rel = match fs_path.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let entry_path = if pkg_prefix_slashed.is_empty() {
                rel
            } else {
                format!("{}/{}", pkg_prefix_slashed, rel)
            };
            entries.insert(entry_path, fs_path);
        }
    }

    if let Some(res_root) = resources_dir {
        for entry in WalkDir::new(res_root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let fs_path = entry.path().to_path_buf();
            if let Ok(rel) = fs_path.strip_prefix(res_root) {
                let entry_path = rel.to_string_lossy().replace('\\', "/");
                // Resources don't overwrite sources with the same path.
                entries.entry(entry_path).or_insert(fs_path);
            }
        }
    }

    // Add intermediate dir entries to keep the jar layout tidy (matches Maven).
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry_path in entries.keys() {
        let parts: Vec<&str> = entry_path.split('/').collect();
        for i in 1..parts.len() {
            dirs.insert(format!("{}/", parts[..i].join("/")));
        }
    }
    for d in &dirs {
        if d == "META-INF/" {
            continue;
        }
        zip.start_file(d, dir_opts)
            .with_context(|| format!("failed to write dir entry {d}"))?;
    }

    for (entry_path, fs_path) in &entries {
        let bytes = std::fs::read(fs_path)
            .with_context(|| format!("failed to read source {}", fs_path.display()))?;
        zip.start_file(entry_path.as_str(), file_opts)
            .with_context(|| format!("failed to start jar entry {entry_path}"))?;
        zip.write_all(&bytes)
            .with_context(|| format!("failed to write jar entry {entry_path}"))?;
    }

    zip.finish().context("failed to finalize sources jar")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// List jar entries (non-directory) — used by assertions below.
    fn list_jar_entries(jar_path: &Path) -> Vec<String> {
        let f = std::fs::File::open(jar_path).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut names = Vec::new();
        for i in 0..zip.len() {
            let entry = zip.by_index(i).unwrap();
            if !entry.is_dir() {
                names.push(entry.name().to_string());
            }
        }
        names.sort();
        names
    }

    fn read_jar_entry(jar_path: &Path, name: &str) -> Option<Vec<u8>> {
        let f = std::fs::File::open(jar_path).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let mut e = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).unwrap();
        Some(buf)
    }

    #[test]
    fn sources_jar_includes_maven_layout_java() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(src.join("com").join("foo")).unwrap();
        std::fs::write(src.join("com").join("foo").join("Bar.java"), b"package com.foo; class Bar {}").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], None).unwrap();

        let entries = list_jar_entries(&jar);
        assert!(entries.contains(&"com/foo/Bar.java".to_string()), "got: {entries:?}");
    }

    #[test]
    fn sources_jar_includes_flat_package_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("com.example.foo");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Hello.java"), b"package com.example.foo; class Hello {}").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], None).unwrap();

        let entries = list_jar_entries(&jar);
        assert!(
            entries.contains(&"com/example/foo/Hello.java".to_string()),
            "flat-package file must land at com/example/foo/Hello.java; got: {entries:?}",
        );
    }

    #[test]
    fn sources_jar_includes_kotlin_sources() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("main").join("kotlin");
        std::fs::create_dir_all(src.join("com").join("foo")).unwrap();
        std::fs::write(src.join("com").join("foo").join("Baz.kt"), b"package com.foo; fun main() {}").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], None).unwrap();

        let entries = list_jar_entries(&jar);
        assert!(entries.contains(&"com/foo/Baz.kt".to_string()), "got: {entries:?}");
    }

    #[test]
    fn sources_jar_includes_resources() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(src.join("p")).unwrap();
        std::fs::write(src.join("p").join("X.java"), b"package p; class X {}").unwrap();

        let res = dir.path().join("src").join("main").join("resources");
        std::fs::create_dir_all(&res).unwrap();
        std::fs::write(res.join("config.properties"), b"k=v\n").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], Some(&res)).unwrap();

        let entries = list_jar_entries(&jar);
        assert!(entries.contains(&"config.properties".to_string()), "got: {entries:?}");
    }

    #[test]
    fn sources_jar_includes_groovy_sources() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("com.example");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Greeter.groovy"), b"package com.example; class Greeter {}").unwrap();
        std::fs::write(src.join("Helper.java"),    b"package com.example; class Helper {}").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], None).unwrap();

        let entries = list_jar_entries(&jar);
        assert!(
            entries.contains(&"com/example/Greeter.groovy".to_string()),
            "Groovy source must land at com/example/Greeter.groovy; got: {entries:?}",
        );
        assert!(
            entries.contains(&"com/example/Helper.java".to_string()),
            "Java source must also be present; got: {entries:?}",
        );
    }

    #[test]
    fn sources_jar_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(src.join("p")).unwrap();
        std::fs::write(src.join("p").join("X.java"), b"package p; class X {}").unwrap();
        std::fs::write(src.join("p").join("Y.java"), b"package p; class Y {}").unwrap();

        let a = dir.path().join("a.jar");
        let b = dir.path().join("b.jar");
        write_sources_jar(&a, &[src.clone()], None).unwrap();
        write_sources_jar(&b, &[src], None).unwrap();

        let bytes_a = std::fs::read(&a).unwrap();
        let bytes_b = std::fs::read(&b).unwrap();
        assert_eq!(bytes_a, bytes_b, "two runs must produce byte-identical jars");
    }

    #[test]
    fn sources_jar_has_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Empty.java"), b"class Empty {}").unwrap();

        let jar = dir.path().join("out.jar");
        write_sources_jar(&jar, &[src], None).unwrap();

        let manifest = read_jar_entry(&jar, "META-INF/MANIFEST.MF").unwrap();
        assert!(String::from_utf8_lossy(&manifest).contains("Manifest-Version: 1.0"));
    }
}
