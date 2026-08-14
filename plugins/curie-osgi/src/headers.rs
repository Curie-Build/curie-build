//! MANIFEST.MF folding, unfolding, and OSGi header assembly.

use crate::config::{self, OsgiConfig};
use anyhow::Result;
use curie_plugin::PluginContext;
use std::collections::{BTreeMap, BTreeSet};

/// Assemble the OSGi headers that will be merged into the JAR manifest.
pub fn build(
    cfg: &OsgiConfig,
    ctx: &PluginContext,
    packages: &BTreeSet<String>,
    referenced_packages: &BTreeSet<String>,
    min_class_major: Option<u16>,
) -> Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    headers.insert("Bundle-ManifestVersion".into(), "2".into());
    headers.insert(
        "Bundle-SymbolicName".into(),
        config::symbolic_name(cfg, ctx),
    );
    let version = config::bundle_version(cfg, ctx);
    headers.insert("Bundle-Version".into(), version.clone());
    if let Some(name) = &cfg.bundle_name {
        headers.insert("Bundle-Name".into(), name.clone());
    }
    if let Some(act) = &cfg.activator {
        headers.insert("Bundle-Activator".into(), act.clone());
    }
    if let Some(exp) = compute_exports(cfg, packages) {
        headers.insert(
            "Export-Package".into(),
            annotate_export_versions(&exp, &version),
        );
    }
    if let Some(imp) = compute_imports(cfg, packages, referenced_packages) {
        headers.insert("Import-Package".into(), imp);
    }
    if let Some(ee) = require_capability_ee(min_class_major) {
        headers.insert("Require-Capability".into(), ee);
    }
    for (k, v) in &cfg.headers {
        headers.insert(k.clone(), v.clone());
    }
    Ok(headers)
}

/// Map a class-file major version to the Java SE version string used in
/// `osgi.ee` filters (`1.8`, `11`, `21`, …).
pub fn java_se_from_class_major(major: u16) -> Option<String> {
    match major {
        45 => Some("1.1".into()),
        46 => Some("1.2".into()),
        47 => Some("1.3".into()),
        48 => Some("1.4".into()),
        49 => Some("1.5".into()),
        50 => Some("1.6".into()),
        51 => Some("1.7".into()),
        52 => Some("1.8".into()),
        m if m >= 53 => Some((m - 44).to_string()),
        _ => None,
    }
}

/// `Require-Capability` clause for the OSGi execution environment, matching
/// bnd's default (`osgi.ee` / JavaSE). Uses the oldest class file in the JAR
/// so the filter is the minimum Java the bytes actually need.
pub fn require_capability_ee(min_class_major: Option<u16>) -> Option<String> {
    let se = java_se_from_class_major(min_class_major?)?;
    Some(format!(
        "osgi.ee;filter:=\"(&(osgi.ee=JavaSE)(version={se}))\""
    ))
}

pub use crate::classfile::class_file_major;

/// Append `;version="<bundle>"` to each Export-Package clause that does not
/// already set `version=`. Matches bnd / maven-bundle-plugin defaults.
pub fn annotate_export_versions(spec: &str, version: &str) -> String {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|clause| {
            if clause.contains("version=") {
                clause.to_string()
            } else {
                format!("{clause};version=\"{version}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Import-Package: user override, or referenced packages that are not
/// contained in this JAR. Order is alphabetical (BTreeSet) so the header
/// is stable and matches bnd for the same set.
pub fn compute_imports(
    cfg: &OsgiConfig,
    local: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
) -> Option<String> {
    if let Some(imp) = &cfg.import_package {
        return Some(imp.clone());
    }
    let imports: Vec<String> = referenced
        .iter()
        .filter(|p| !local.contains(*p))
        .cloned()
        .collect();
    if imports.is_empty() {
        None
    } else {
        Some(imports.join(","))
    }
}

pub fn compute_exports(cfg: &OsgiConfig, packages: &BTreeSet<String>) -> Option<String> {
    if let Some(exp) = &cfg.export_package {
        return Some(exp.clone());
    }
    let private = parse_package_patterns(cfg.private_package.as_deref());
    let exported: Vec<String> = packages
        .iter()
        .filter(|p| !matches_any(p, &private))
        .cloned()
        .collect();
    if exported.is_empty() {
        None
    } else {
        Some(exported.join(","))
    }
}

pub fn parse_package_patterns(spec: Option<&str>) -> Vec<String> {
    spec.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn matches_any(package: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| match_package(package, pat))
}

fn match_package(package: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        package == prefix || package.starts_with(&format!("{prefix}."))
    } else {
        package == pattern
    }
}

/// Packages that contain at least one `.class` entry, excluding `META-INF`
/// and the unnamed package.
pub fn packages_from_entries<'a>(names: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut pkgs = BTreeSet::new();
    for name in names {
        if !name.ends_with(".class") || name == "module-info.class" {
            continue;
        }
        if name.starts_with("META-INF/") {
            continue;
        }
        if let Some(slash) = name.rfind('/') {
            pkgs.insert(name[..slash].replace('/', "."));
        }
    }
    pkgs
}

/// Parse a MANIFEST.MF into an ordered list of (name, value) headers.
/// Continuation lines are unfolded. Duplicate names keep the last value.
pub fn parse_manifest(text: &str) -> Vec<(String, String)> {
    let unfolded = unfold_manifest(text);
    let mut out = Vec::new();
    for line in unfolded.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if let Some(existing) = out.iter_mut().find(|(n, _)| n == &name) {
                existing.1 = value;
            } else {
                out.push((name, value));
            }
        }
    }
    out
}

/// Merge `extra` headers into an existing manifest. Extra keys replace
/// matching names; `Manifest-Version` is kept first if present (or added).
pub fn merge_manifest(existing: &str, extra: &BTreeMap<String, String>) -> String {
    let mut headers = parse_manifest(existing);
    if !headers.iter().any(|(n, _)| n == "Manifest-Version") {
        headers.insert(0, ("Manifest-Version".into(), "1.0".into()));
    }
    for (name, value) in extra {
        if let Some(existing) = headers.iter_mut().find(|(n, _)| n == name) {
            existing.1 = value.clone();
        } else {
            headers.push((name.clone(), value.clone()));
        }
    }
    let mut out = String::new();
    for (name, value) in &headers {
        out.push_str(&format_manifest_header(name, value));
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
pub fn header_value(manifest: &str, name: &str) -> Option<String> {
    parse_manifest(manifest)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

fn unfold_manifest(manifest: &str) -> String {
    let mut result = String::new();
    let mut current = String::new();
    for line in manifest.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            current.push_str(rest);
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

/// Fold a header to 72-byte lines per the JAR spec.
fn format_manifest_header(name: &str, value: &str) -> String {
    let first_capacity = 72usize.saturating_sub(name.len() + 2 + 2);
    let cont_capacity = 69usize;
    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut pos = 0usize;
    let mut first = true;
    while pos < bytes.len() {
        let capacity = if first { first_capacity } else { cont_capacity };
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
            out.push(' ');
        }
        out.push_str(chunk);
        out.push_str("\r\n");
        pos = end;
    }
    if first {
        out.push_str(name);
        out.push_str(": \r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_se_from_class_major_maps_known_versions() {
        assert_eq!(java_se_from_class_major(52).as_deref(), Some("1.8"));
        assert_eq!(java_se_from_class_major(65).as_deref(), Some("21"));
        assert_eq!(java_se_from_class_major(70).as_deref(), Some("26"));
        assert_eq!(java_se_from_class_major(20), None);
    }

    #[test]
    fn require_capability_ee_matches_bnd() {
        assert_eq!(
            require_capability_ee(Some(52)).as_deref(),
            Some("osgi.ee;filter:=\"(&(osgi.ee=JavaSE)(version=1.8))\"")
        );
        assert_eq!(
            require_capability_ee(Some(65)).as_deref(),
            Some("osgi.ee;filter:=\"(&(osgi.ee=JavaSE)(version=21))\"")
        );
        assert!(require_capability_ee(None).is_none());
    }

    #[test]
    fn class_file_major_reads_cafe_babe() {
        let mut bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 65];
        bytes.extend_from_slice(&[0u8; 8]);
        assert_eq!(class_file_major(&bytes), Some(65));
        assert_eq!(class_file_major(b"not-a-class"), None);
    }

    #[test]
    fn compute_imports_skips_local_packages() {
        let cfg = OsgiConfig::default();
        let local = BTreeSet::from(["com.example.osgi".to_string()]);
        let referenced = BTreeSet::from([
            "com.example.osgi".to_string(),
            "java.lang".to_string(),
            "java.lang.invoke".to_string(),
        ]);
        assert_eq!(
            compute_imports(&cfg, &local, &referenced).as_deref(),
            Some("java.lang,java.lang.invoke")
        );
    }

    #[test]
    fn build_emits_require_capability_from_class_major() {
        let cfg = OsgiConfig::default();
        let ctx = PluginContext {
            artifact_id: "greeter".into(),
            version: "1.0.0".into(),
            ..Default::default()
        };
        let headers = build(&cfg, &ctx, &BTreeSet::new(), &BTreeSet::new(), Some(65)).unwrap();
        assert_eq!(
            headers.get("Require-Capability").map(String::as_str),
            Some("osgi.ee;filter:=\"(&(osgi.ee=JavaSE)(version=21))\"")
        );
    }

    #[test]
    fn packages_from_class_entries() {
        let names = [
            "com/example/Foo.class",
            "com/example/Foo$Bar.class",
            "com/example/util/Util.class",
            "META-INF/MANIFEST.MF",
            "module-info.class",
            "README.txt",
        ];
        let pkgs = packages_from_entries(names.into_iter());
        assert_eq!(
            pkgs,
            BTreeSet::from(["com.example".to_string(), "com.example.util".to_string()])
        );
    }

    #[test]
    fn private_package_wildcard_filters_exports() {
        let cfg = OsgiConfig {
            private_package: Some("com.example.internal.*".into()),
            ..Default::default()
        };
        let pkgs = BTreeSet::from([
            "com.example.api".into(),
            "com.example.internal".into(),
            "com.example.internal.impl".into(),
        ]);
        let exp = compute_exports(&cfg, &pkgs).unwrap();
        assert_eq!(exp, "com.example.api");
    }

    #[test]
    fn annotate_export_versions_adds_missing_version() {
        assert_eq!(
            annotate_export_versions("com.example.osgi", "1.0.0"),
            "com.example.osgi;version=\"1.0.0\""
        );
        assert_eq!(
            annotate_export_versions("com.a,com.b", "2.0.0"),
            "com.a;version=\"2.0.0\",com.b;version=\"2.0.0\""
        );
    }

    #[test]
    fn annotate_export_versions_keeps_explicit_version() {
        assert_eq!(
            annotate_export_versions("com.example.api;version=2.0", "1.0.0"),
            "com.example.api;version=2.0"
        );
    }

    #[test]
    fn build_annotates_export_package_with_bundle_version() {
        let cfg = OsgiConfig {
            export_package: Some("com.example.osgi".into()),
            ..Default::default()
        };
        let ctx = PluginContext {
            version: "1.0.0".into(),
            ..Default::default()
        };
        let headers = build(&cfg, &ctx, &BTreeSet::new(), &BTreeSet::new(), None).unwrap();
        assert_eq!(
            headers.get("Export-Package").map(String::as_str),
            Some("com.example.osgi;version=\"1.0.0\"")
        );
    }

    #[test]
    fn explicit_export_package_wins() {
        let cfg = OsgiConfig {
            export_package: Some("com.example.api;version=1.0".into()),
            ..Default::default()
        };
        let pkgs = BTreeSet::from(["com.example.api".into(), "com.example.impl".into()]);
        assert_eq!(
            compute_exports(&cfg, &pkgs).as_deref(),
            Some("com.example.api;version=1.0")
        );
    }

    #[test]
    fn merge_preserves_main_class_and_adds_osgi() {
        let existing = "Manifest-Version: 1.0\r\nMain-Class: com.example.App\r\n\r\n";
        let mut extra = BTreeMap::new();
        extra.insert("Bundle-SymbolicName".into(), "com.example.app".into());
        extra.insert("Bundle-Version".into(), "1.0.0".into());
        let merged = merge_manifest(existing, &extra);
        assert_eq!(
            header_value(&merged, "Main-Class").as_deref(),
            Some("com.example.App")
        );
        assert_eq!(
            header_value(&merged, "Bundle-SymbolicName").as_deref(),
            Some("com.example.app")
        );
        assert_eq!(
            header_value(&merged, "Manifest-Version").as_deref(),
            Some("1.0")
        );
    }

    #[test]
    fn fold_and_unfold_long_header() {
        let long = "a".repeat(90);
        let folded = format_manifest_header("Export-Package", &long);
        assert!(folded.lines().count() >= 2);
        let parsed = parse_manifest(&folded);
        assert_eq!(parsed[0].1, long);
    }
}
