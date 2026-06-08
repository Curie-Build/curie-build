//! Generate Maven POM XML from a Curie descriptor + resolved declared deps.
//!
//! Uses the `quick-xml` writer for correct XML serialisation (automatic
//! character escaping, well-formed structure).

use crate::descriptor::{Descriptor, DependencyValue, PublishConfig};
use anyhow::Result;
use curie_deps::Gav;
use quick_xml::{
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
    Writer,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// Build a POM XML document for the given descriptor and its declared
/// dependencies (with versions already resolved through any BOMs).
pub fn build_pom(desc: &Descriptor, declared_deps: &[Gav]) -> Result<String> {
    let group_id = desc
        .group_id()
        .ok_or_else(|| anyhow::anyhow!("groupId must be set on [application] or [library] to publish"))?;
    let artifact_id = desc.buildable_name();
    let version = desc.buildable_version();
    let pub_cfg = &desc.publish;

    let mut buf = Vec::new();
    let mut w = Writer::new_with_indent(Cursor::new(&mut buf), b' ', 2);

    // <?xml version="1.0" encoding="UTF-8"?>
    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    // <project ...>
    let mut project = BytesStart::new("project");
    project.push_attribute(("xmlns", "http://maven.apache.org/POM/4.0.0"));
    project.push_attribute(("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"));
    project.push_attribute((
        "xsi:schemaLocation",
        "http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd",
    ));
    w.write_event(Event::Start(project))?;

    text_elem(&mut w, "modelVersion", "4.0.0")?;
    text_elem(&mut w, "groupId", group_id)?;
    text_elem(&mut w, "artifactId", artifact_id)?;
    text_elem(&mut w, "version", version)?;
    text_elem(&mut w, "packaging", "jar")?;
    text_elem(&mut w, "name", artifact_id)?;

    if let Some(desc_text) = &pub_cfg.description {
        text_elem(&mut w, "description", desc_text)?;
    }
    if let Some(homepage) = &pub_cfg.homepage {
        text_elem(&mut w, "url", homepage)?;
    }

    write_licenses(&mut w, &pub_cfg.licenses)?;
    write_developers(&mut w, pub_cfg)?;
    write_scm(&mut w, pub_cfg)?;
    write_dependencies(&mut w, declared_deps)?;

    w.write_event(Event::End(BytesEnd::new("project")))?;

    Ok(String::from_utf8(buf)?)
}

/// Convenience wrapper: build the POM and write it to `path`.
pub fn write_pom(desc: &Descriptor, declared_deps: &[Gav], path: &Path) -> Result<()> {
    let body = build_pom(desc, declared_deps)?;
    std::fs::write(path, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write POM at {}: {}", path.display(), e))?;
    Ok(())
}

/// Build a BOM POM XML document.  The output has `<packaging>pom</packaging>`
/// and a `<dependencyManagement>` block containing both BOM imports and
/// explicitly-versioned managed dependencies.  No `<dependencies>` block.
pub fn build_bom_pom(desc: &Descriptor) -> Result<String> {
    let group_id = desc
        .group_id()
        .ok_or_else(|| anyhow::anyhow!("groupId must be set on [bom] to publish"))?;
    let artifact_id = desc.buildable_name();
    let version = desc.buildable_version();
    let pub_cfg = &desc.publish;

    let mut buf = Vec::new();
    let mut w = Writer::new_with_indent(Cursor::new(&mut buf), b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let mut project = BytesStart::new("project");
    project.push_attribute(("xmlns", "http://maven.apache.org/POM/4.0.0"));
    project.push_attribute(("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"));
    project.push_attribute((
        "xsi:schemaLocation",
        "http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd",
    ));
    w.write_event(Event::Start(project))?;

    text_elem(&mut w, "modelVersion", "4.0.0")?;
    text_elem(&mut w, "groupId", group_id)?;
    text_elem(&mut w, "artifactId", artifact_id)?;
    text_elem(&mut w, "version", version)?;
    text_elem(&mut w, "packaging", "pom")?;
    text_elem(&mut w, "name", artifact_id)?;

    if let Some(desc_text) = &pub_cfg.description {
        text_elem(&mut w, "description", desc_text)?;
    }
    if let Some(homepage) = &pub_cfg.homepage {
        text_elem(&mut w, "url", homepage)?;
    }

    write_licenses(&mut w, &pub_cfg.licenses)?;
    write_developers(&mut w, pub_cfg)?;
    write_scm(&mut w, pub_cfg)?;
    write_dependency_management(&mut w, &desc.bom_imports, &desc.dependencies)?;

    w.write_event(Event::End(BytesEnd::new("project")))?;

    Ok(String::from_utf8(buf)?)
}

/// Convenience wrapper: build the BOM POM and write it to `path`.
pub fn write_bom_pom(desc: &Descriptor, path: &Path) -> Result<()> {
    let body = build_bom_pom(desc)?;
    std::fs::write(path, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write BOM POM at {}: {}", path.display(), e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

type XmlWriter<'a> = Writer<Cursor<&'a mut Vec<u8>>>;

/// Write `<name>text</name>` — `BytesText::new` escapes the content.
fn text_elem(w: &mut XmlWriter<'_>, name: &str, text: &str) -> Result<()> {
    w.create_element(name)
        .write_text_content(BytesText::new(text))?;
    Ok(())
}

fn write_licenses(w: &mut XmlWriter<'_>, licenses: &[String]) -> Result<()> {
    if licenses.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("licenses")))?;
    for spdx in licenses {
        let (name, url) = spdx_lookup(spdx);
        w.write_event(Event::Start(BytesStart::new("license")))?;
        text_elem(w, "name", name)?;
        if let Some(u) = url {
            text_elem(w, "url", u)?;
        }
        w.write_event(Event::End(BytesEnd::new("license")))?;
    }
    w.write_event(Event::End(BytesEnd::new("licenses")))?;
    Ok(())
}

fn write_developers(w: &mut XmlWriter<'_>, pub_cfg: &PublishConfig) -> Result<()> {
    if pub_cfg.developers.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("developers")))?;
    for dev in &pub_cfg.developers {
        w.write_event(Event::Start(BytesStart::new("developer")))?;
        if let Some(id) = &dev.id {
            text_elem(w, "id", id)?;
        }
        if let Some(name) = &dev.name {
            text_elem(w, "name", name)?;
        }
        if let Some(email) = &dev.email {
            text_elem(w, "email", email)?;
        }
        w.write_event(Event::End(BytesEnd::new("developer")))?;
    }
    w.write_event(Event::End(BytesEnd::new("developers")))?;
    Ok(())
}

fn write_scm(w: &mut XmlWriter<'_>, pub_cfg: &PublishConfig) -> Result<()> {
    let Some(scm) = &pub_cfg.scm else { return Ok(()) };
    w.write_event(Event::Start(BytesStart::new("scm")))?;
    if let Some(u) = &scm.url {
        text_elem(w, "url", u)?;
    }
    if let Some(c) = &scm.connection {
        text_elem(w, "connection", c)?;
    }
    if let Some(dc) = &scm.developer_connection {
        text_elem(w, "developerConnection", dc)?;
    }
    w.write_event(Event::End(BytesEnd::new("scm")))?;
    Ok(())
}

fn write_dependencies(w: &mut XmlWriter<'_>, deps: &[Gav]) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("dependencies")))?;
    for gav in deps {
        w.write_event(Event::Start(BytesStart::new("dependency")))?;
        text_elem(w, "groupId", &gav.group)?;
        text_elem(w, "artifactId", &gav.artifact)?;
        text_elem(w, "version", &gav.version)?;
        text_elem(w, "scope", "compile")?;
        w.write_event(Event::End(BytesEnd::new("dependency")))?;
    }
    w.write_event(Event::End(BytesEnd::new("dependencies")))?;
    Ok(())
}

/// Emit a `<dependencyManagement>` block for BOM artifacts.
///
/// `bom_imports` — coordinates imported as BOM; each gets `<type>pom</type><scope>import</scope>`.
/// `managed_deps` — direct managed dependency coordinates with explicit versions.
fn write_dependency_management(
    w: &mut XmlWriter<'_>,
    bom_imports: &BTreeMap<String, String>,
    managed_deps: &BTreeMap<String, DependencyValue>,
) -> Result<()> {
    if bom_imports.is_empty() && managed_deps.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("dependencyManagement")))?;
    w.write_event(Event::Start(BytesStart::new("dependencies")))?;

    for (coord, version) in bom_imports {
        let (group, artifact) = split_coord(coord)?;
        w.write_event(Event::Start(BytesStart::new("dependency")))?;
        text_elem(w, "groupId", group)?;
        text_elem(w, "artifactId", artifact)?;
        text_elem(w, "version", version)?;
        text_elem(w, "type", "pom")?;
        text_elem(w, "scope", "import")?;
        w.write_event(Event::End(BytesEnd::new("dependency")))?;
    }

    for (coord, dep) in managed_deps {
        let (group, artifact) = split_coord(coord)?;
        w.write_event(Event::Start(BytesStart::new("dependency")))?;
        text_elem(w, "groupId", group)?;
        text_elem(w, "artifactId", artifact)?;
        text_elem(w, "version", dep.version())?;
        w.write_event(Event::End(BytesEnd::new("dependency")))?;
    }

    w.write_event(Event::End(BytesEnd::new("dependencies")))?;
    w.write_event(Event::End(BytesEnd::new("dependencyManagement")))?;
    Ok(())
}

/// Split `"group:artifact"` into `("group", "artifact")`.
fn split_coord(coord: &str) -> Result<(&str, &str)> {
    let mut parts = coord.splitn(2, ':');
    let group = parts.next().ok_or_else(|| anyhow::anyhow!("invalid coordinate: {}", coord))?;
    let artifact = parts.next().ok_or_else(|| anyhow::anyhow!("invalid coordinate (missing ':'): {}", coord))?;
    Ok((group, artifact))
}

/// SPDX-id → `(<name>, Some(<url>))` for common licenses.  Unknown ids
/// return `(spdx_id, None)` so the SPDX id still becomes the `<name>`.
fn spdx_lookup(spdx: &str) -> (&str, Option<&str>) {
    match spdx {
        "Apache-2.0" => ("Apache License 2.0", Some("https://www.apache.org/licenses/LICENSE-2.0.txt")),
        "MIT" => ("MIT License", Some("https://opensource.org/licenses/MIT")),
        "BSD-2-Clause" => ("BSD 2-Clause License", Some("https://opensource.org/licenses/BSD-2-Clause")),
        "BSD-3-Clause" => ("BSD 3-Clause License", Some("https://opensource.org/licenses/BSD-3-Clause")),
        "MPL-2.0" => ("Mozilla Public License 2.0", Some("https://www.mozilla.org/en-US/MPL/2.0/")),
        "GPL-2.0" => ("GNU General Public License v2.0", Some("https://www.gnu.org/licenses/old-licenses/gpl-2.0.txt")),
        "GPL-3.0" => ("GNU General Public License v3.0", Some("https://www.gnu.org/licenses/gpl-3.0.txt")),
        "LGPL-2.1" => ("GNU Lesser General Public License v2.1", Some("https://www.gnu.org/licenses/old-licenses/lgpl-2.1.txt")),
        "LGPL-3.0" => ("GNU Lesser General Public License v3.0", Some("https://www.gnu.org/licenses/lgpl-3.0.txt")),
        "ISC" => ("ISC License", Some("https://opensource.org/licenses/ISC")),
        "Unlicense" => ("The Unlicense", Some("https://unlicense.org/")),
        other => (other, None),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{fake_bom_desc, fake_library_desc, Developer, Scm};

    fn fake_desc(group_id: &str, name: &str, version: &str, pub_cfg: PublishConfig) -> Descriptor {
        fake_library_desc(Some(group_id), name, version, pub_cfg)
    }

    #[test]
    fn pom_minimal_application() {
        let desc = fake_desc("com.example", "my-lib", "1.0.0", PublishConfig::default());
        let xml = build_pom(&desc, &[]).unwrap();
        assert!(xml.contains("<groupId>com.example</groupId>"));
        assert!(xml.contains("<artifactId>my-lib</artifactId>"));
        assert!(xml.contains("<version>1.0.0</version>"));
        assert!(xml.contains("<packaging>jar</packaging>"));
    }

    #[test]
    fn pom_with_full_metadata() {
        let pub_cfg = PublishConfig {
            description: Some("Test lib".into()),
            homepage: Some("https://example.com".into()),
            licenses: vec!["Apache-2.0".into()],
            developers: vec![Developer {
                id: Some("alice".into()),
                name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
            }],
            scm: Some(Scm {
                url: Some("https://github.com/x/y".into()),
                connection: Some("scm:git:git@github.com:x/y.git".into()),
                developer_connection: None,
            }),
            ..PublishConfig::default()
        };
        let desc = fake_desc("com.example", "my-lib", "1.0.0", pub_cfg);
        let xml = build_pom(&desc, &[]).unwrap();
        assert!(xml.contains("<description>Test lib</description>"));
        assert!(xml.contains("<url>https://example.com</url>"));
        assert!(xml.contains("<name>Apache License 2.0</name>"));
        assert!(xml.contains("<id>alice</id>"));
        assert!(xml.contains("<connection>scm:git:git@github.com:x/y.git</connection>"));
    }

    #[test]
    fn pom_xml_escapes_user_strings() {
        let pub_cfg = PublishConfig {
            description: Some("a & b <c> \"d\"".into()),
            ..PublishConfig::default()
        };
        let desc = fake_desc("com.example", "my-lib", "1.0.0", pub_cfg);
        let xml = build_pom(&desc, &[]).unwrap();
        assert!(xml.contains("a &amp; b &lt;c&gt; &quot;d&quot;"));
        assert!(!xml.contains("a & b <c>"));
    }

    #[test]
    fn pom_emits_dependencies_in_provided_order() {
        let desc = fake_desc("com.example", "my-lib", "1.0.0", PublishConfig::default());
        let deps = vec![
            Gav::from_key_version("com.fasterxml.jackson.core:jackson-databind", "2.17.2").unwrap(),
            Gav::from_key_version("com.google.guava:guava", "33.2.0-jre").unwrap(),
        ];
        let xml = build_pom(&desc, &deps).unwrap();
        // Both deps present.
        let p1 = xml.find("jackson-databind").unwrap();
        let p2 = xml.find("guava").unwrap();
        // First in declaration order.
        assert!(p1 < p2);
        // Compile scope emitted.
        assert!(xml.contains("<scope>compile</scope>"));
    }

    #[test]
    fn spdx_lookup_known_apache_2_0() {
        let (name, url) = spdx_lookup("Apache-2.0");
        assert_eq!(name, "Apache License 2.0");
        assert!(url.unwrap().contains("LICENSE-2.0"));
    }

    #[test]
    fn spdx_lookup_unknown_falls_back_to_id_only() {
        let (name, url) = spdx_lookup("My-Custom-1.0");
        assert_eq!(name, "My-Custom-1.0");
        assert!(url.is_none());
    }

    #[test]
    fn pom_errors_when_group_id_missing() {
        let desc = fake_library_desc(None, "x", "1.0", PublishConfig::default());
        let err = build_pom(&desc, &[]).unwrap_err().to_string();
        assert!(err.contains("groupId"), "got: {err}");
    }

    // -- BOM POM tests -------------------------------------------------------

    #[test]
    fn build_bom_pom_basic() {
        let desc = fake_bom_desc(
            Some("com.example"),
            "my-platform",
            "1.0.0",
            BTreeMap::new(),
            BTreeMap::new(),
            PublishConfig::default(),
        );
        let xml = build_bom_pom(&desc).unwrap();
        assert!(xml.contains("<groupId>com.example</groupId>"));
        assert!(xml.contains("<artifactId>my-platform</artifactId>"));
        assert!(xml.contains("<version>1.0.0</version>"));
        assert!(xml.contains("<packaging>pom</packaging>"));
    }

    #[test]
    fn build_bom_pom_bom_import_has_pom_scope() {
        let mut bom_imports = BTreeMap::new();
        bom_imports.insert("io.micronaut:micronaut-bom".to_string(), "4.3.2".to_string());
        let desc = fake_bom_desc(
            Some("com.example"),
            "my-platform",
            "1.0.0",
            BTreeMap::new(),
            bom_imports,
            PublishConfig::default(),
        );
        let xml = build_bom_pom(&desc).unwrap();
        assert!(xml.contains("<dependencyManagement>"));
        assert!(xml.contains("<type>pom</type>"));
        assert!(xml.contains("<scope>import</scope>"));
        assert!(xml.contains("<artifactId>micronaut-bom</artifactId>"));
    }

    #[test]
    fn build_bom_pom_managed_dep_has_version() {
        let mut deps = BTreeMap::new();
        deps.insert(
            "com.google.guava:guava".to_string(),
            DependencyValue::Version("33.0.0-jre".to_string()),
        );
        let desc = fake_bom_desc(
            Some("com.example"),
            "my-platform",
            "1.0.0",
            deps,
            BTreeMap::new(),
            PublishConfig::default(),
        );
        let xml = build_bom_pom(&desc).unwrap();
        assert!(xml.contains("<dependencyManagement>"));
        assert!(xml.contains("<artifactId>guava</artifactId>"));
        assert!(xml.contains("<version>33.0.0-jre</version>"));
    }

    #[test]
    fn build_bom_pom_no_plain_dependencies_block() {
        let mut deps = BTreeMap::new();
        deps.insert(
            "org.slf4j:slf4j-api".to_string(),
            DependencyValue::Version("2.0.12".to_string()),
        );
        let desc = fake_bom_desc(
            Some("com.example"),
            "my-platform",
            "1.0.0",
            deps,
            BTreeMap::new(),
            PublishConfig::default(),
        );
        let xml = build_bom_pom(&desc).unwrap();
        // <dependencies> must only appear inside <dependencyManagement>
        let first_dep_management = xml.find("<dependencyManagement>").unwrap();
        let first_dependencies = xml.find("<dependencies>").unwrap();
        assert!(
            first_dependencies > first_dep_management,
            "<dependencies> must be inside <dependencyManagement>, not before it"
        );
        // No standalone <dependencies> outside <dependencyManagement>
        let after_mgmt = &xml[first_dep_management..];
        assert!(
            !xml[..first_dep_management].contains("<dependencies>"),
            "no <dependencies> block should appear before <dependencyManagement>"
        );
        let _ = after_mgmt; // used in assertion above
    }

    #[test]
    fn build_bom_pom_errors_when_group_id_missing() {
        let desc = fake_bom_desc(None, "x", "1.0", BTreeMap::new(), BTreeMap::new(), PublishConfig::default());
        let err = build_bom_pom(&desc).unwrap_err().to_string();
        assert!(err.contains("groupId"), "got: {err}");
    }
}
