//! Rewrite a project JAR as an OSGi bundle by merging headers into MANIFEST.MF.

use crate::config::Envelope;
use crate::headers;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

pub fn run(project_root: &Path, env: &Envelope) -> Result<()> {
    let ctx = env
        .context
        .as_ref()
        .context("curie-build did not provide a plugin context")?;
    let jar = ctx.jar.as_ref().context("plugin context has no jar path")?;
    let jar = if jar.is_absolute() {
        jar.clone()
    } else {
        project_root.join(jar)
    };
    if !jar.exists() {
        bail!("project JAR does not exist: {}", jar.display());
    }

    let scan = scan_jar(&jar)?;
    let extra = headers::build(
        &env.config,
        ctx,
        &scan.packages,
        &scan.referenced_packages,
        scan.min_class_major,
    )?;
    wrap_in_place(&jar, &extra)?;
    eprintln!(
        "OSGi bundle {} ({})",
        jar.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default(),
        extra
            .get("Bundle-SymbolicName")
            .map(String::as_str)
            .unwrap_or("?")
    );
    Ok(())
}

pub struct JarScan {
    pub packages: std::collections::BTreeSet<String>,
    pub referenced_packages: std::collections::BTreeSet<String>,
    pub min_class_major: Option<u16>,
}

pub fn scan_jar(jar: &Path) -> Result<JarScan> {
    let bytes = fs::read(jar).with_context(|| format!("failed to read {}", jar.display()))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .with_context(|| format!("not a JAR: {}", jar.display()))?;
    let mut names = Vec::new();
    let mut referenced_packages = std::collections::BTreeSet::new();
    let mut min_class_major: Option<u16> = None;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        if name.ends_with(".class") {
            let mut class_bytes = Vec::new();
            file.read_to_end(&mut class_bytes)?;
            if let Some(major) = headers::class_file_major(&class_bytes) {
                min_class_major = Some(match min_class_major {
                    Some(prev) => prev.min(major),
                    None => major,
                });
            }
            referenced_packages.extend(crate::classfile::referenced_packages(&class_bytes));
        }
        names.push(name);
    }
    Ok(JarScan {
        packages: headers::packages_from_entries(names.iter().map(String::as_str)),
        referenced_packages,
        min_class_major,
    })
}

pub fn scan_packages(jar: &Path) -> Result<std::collections::BTreeSet<String>> {
    Ok(scan_jar(jar)?.packages)
}

pub fn wrap_in_place(jar: &Path, extra: &BTreeMap<String, String>) -> Result<()> {
    let parent = jar.parent().unwrap_or(Path::new("."));
    let staging = parent.join(format!(
        ".{}.osgi-wrap",
        jar.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
    ));
    wrap_jar(jar, &staging, extra)?;
    fs::rename(&staging, jar).with_context(|| format!("failed to replace {}", jar.display()))?;
    Ok(())
}

pub fn wrap_jar(input: &Path, output: &Path, extra: &BTreeMap<String, String>) -> Result<()> {
    let bytes = fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice()))
        .with_context(|| format!("not a JAR: {}", input.display()))?;

    let mut existing_manifest = String::new();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        if name == "META-INF/MANIFEST.MF" {
            file.read_to_string(&mut existing_manifest)
                .context("failed to read MANIFEST.MF")?;
            continue;
        }
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .with_context(|| format!("failed to read JAR entry {name}"))?;
        entries.push((name, data));
    }

    let manifest = headers::merge_manifest(&existing_manifest, extra);
    write_jar(output, &manifest, &entries)
}

fn write_jar(path: &Path, manifest: &str, entries: &[(String, Vec<u8>)]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file =
        fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let file_opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(epoch())
        .unix_permissions(0o644);
    let dir_opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(epoch())
        .unix_permissions(0o755);

    zip.start_file("META-INF/", dir_opts)
        .context("failed to write META-INF/")?;
    zip.start_file("META-INF/MANIFEST.MF", file_opts)
        .context("failed to start MANIFEST.MF")?;
    zip.write_all(manifest.as_bytes())?;

    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, data) in &sorted {
        zip.start_file(name.as_str(), file_opts)
            .with_context(|| format!("failed to write {name}"))?;
        zip.write_all(data)?;
    }
    zip.finish().context("failed to finalize bundle JAR")?;
    Ok(())
}

fn epoch() -> DateTime {
    DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0).expect("epoch is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn write_plain_jar(path: &Path) {
        let entries = vec![
            ("com/example/Greeter.class".into(), b"classbytes".to_vec()),
            ("com/example/internal/Hidden.class".into(), b"hid".to_vec()),
        ];
        let manifest = "Manifest-Version: 1.0\r\nMain-Class: com.example.Greeter\r\n\r\n";
        write_jar(path, manifest, &entries).unwrap();
    }

    #[test]
    fn wrap_adds_osgi_headers_and_keeps_entries() {
        let tmp = TempDir::new().unwrap();
        let jar = tmp.path().join("lib.jar");
        write_plain_jar(&jar);

        let mut extra = BTreeMap::new();
        extra.insert("Bundle-ManifestVersion".into(), "2".into());
        extra.insert("Bundle-SymbolicName".into(), "com.example.greeter".into());
        extra.insert("Bundle-Version".into(), "1.0.0".into());
        extra.insert("Export-Package".into(), "com.example".into());

        wrap_in_place(&jar, &extra).unwrap();

        let bytes = fs::read(&jar).unwrap();
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut mf = String::new();
        archive
            .by_name("META-INF/MANIFEST.MF")
            .unwrap()
            .read_to_string(&mut mf)
            .unwrap();
        assert_eq!(
            headers::header_value(&mf, "Bundle-SymbolicName").as_deref(),
            Some("com.example.greeter")
        );
        assert_eq!(
            headers::header_value(&mf, "Main-Class").as_deref(),
            Some("com.example.Greeter")
        );
        assert!(archive.by_name("com/example/Greeter.class").is_ok());
    }

    #[test]
    fn scan_packages_finds_class_packages() {
        let tmp = TempDir::new().unwrap();
        let jar = tmp.path().join("lib.jar");
        write_plain_jar(&jar);
        let pkgs = scan_packages(&jar).unwrap();
        assert!(pkgs.contains("com.example"));
        assert!(pkgs.contains("com.example.internal"));
    }

    fn class_stub(major: u16) -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00];
        b.extend_from_slice(&major.to_be_bytes());
        b.extend_from_slice(&[0u8; 16]);
        b
    }

    #[test]
    fn scan_jar_tracks_oldest_class_major() {
        let tmp = TempDir::new().unwrap();
        let jar = tmp.path().join("lib.jar");
        write_jar(
            &jar,
            "Manifest-Version: 1.0\r\n\r\n",
            &[
                ("com/example/A.class".into(), class_stub(65)),
                ("com/example/B.class".into(), class_stub(52)),
            ],
        )
        .unwrap();
        let scan = scan_jar(&jar).unwrap();
        assert_eq!(scan.min_class_major, Some(52));
        assert!(scan.packages.contains("com.example"));
    }
}
