//! Generate Maven POM XML from a Curie descriptor + resolved declared deps.
//!
//! Hand-written formatter — POM 4.0.0 schema is fully known and we control
//! every value, so a small `Write`-based emitter is simpler than pulling in
//! an XML library.

use crate::descriptor::{Descriptor, PublishConfig};
use anyhow::Result;
use curie_deps::Gav;
use std::fmt::Write as _;
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

    let mut out = String::new();
    writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    writeln!(out, "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"").unwrap();
    writeln!(out, "         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"").unwrap();
    writeln!(
        out,
        "         xsi:schemaLocation=\"http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd\">"
    )
    .unwrap();
    writeln!(out, "  <modelVersion>4.0.0</modelVersion>").unwrap();
    writeln!(out, "  <groupId>{}</groupId>", xml_escape(group_id)).unwrap();
    writeln!(out, "  <artifactId>{}</artifactId>", xml_escape(artifact_id)).unwrap();
    writeln!(out, "  <version>{}</version>", xml_escape(version)).unwrap();
    writeln!(out, "  <packaging>jar</packaging>").unwrap();
    writeln!(out, "  <name>{}</name>", xml_escape(artifact_id)).unwrap();

    if let Some(desc_text) = &pub_cfg.description {
        writeln!(out, "  <description>{}</description>", xml_escape(desc_text)).unwrap();
    }
    if let Some(homepage) = &pub_cfg.homepage {
        writeln!(out, "  <url>{}</url>", xml_escape(homepage)).unwrap();
    }

    write_licenses(&mut out, &pub_cfg.licenses);
    write_developers(&mut out, pub_cfg);
    write_scm(&mut out, pub_cfg);
    write_dependencies(&mut out, declared_deps);

    writeln!(out, "</project>").unwrap();
    Ok(out)
}

/// Convenience wrapper: build the POM and write it to `path`.
pub fn write_pom(desc: &Descriptor, declared_deps: &[Gav], path: &Path) -> Result<()> {
    let body = build_pom(desc, declared_deps)?;
    std::fs::write(path, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write POM at {}: {}", path.display(), e))?;
    Ok(())
}

fn write_licenses(out: &mut String, licenses: &[String]) {
    if licenses.is_empty() {
        return;
    }
    writeln!(out, "  <licenses>").unwrap();
    for spdx in licenses {
        let (name, url) = spdx_lookup(spdx);
        writeln!(out, "    <license>").unwrap();
        writeln!(out, "      <name>{}</name>", xml_escape(name)).unwrap();
        if let Some(u) = url {
            writeln!(out, "      <url>{}</url>", xml_escape(u)).unwrap();
        }
        writeln!(out, "    </license>").unwrap();
    }
    writeln!(out, "  </licenses>").unwrap();
}

fn write_developers(out: &mut String, pub_cfg: &PublishConfig) {
    if pub_cfg.developers.is_empty() {
        return;
    }
    writeln!(out, "  <developers>").unwrap();
    for dev in &pub_cfg.developers {
        writeln!(out, "    <developer>").unwrap();
        if let Some(id) = &dev.id {
            writeln!(out, "      <id>{}</id>", xml_escape(id)).unwrap();
        }
        if let Some(name) = &dev.name {
            writeln!(out, "      <name>{}</name>", xml_escape(name)).unwrap();
        }
        if let Some(email) = &dev.email {
            writeln!(out, "      <email>{}</email>", xml_escape(email)).unwrap();
        }
        writeln!(out, "    </developer>").unwrap();
    }
    writeln!(out, "  </developers>").unwrap();
}

fn write_scm(out: &mut String, pub_cfg: &PublishConfig) {
    let Some(scm) = &pub_cfg.scm else { return };
    writeln!(out, "  <scm>").unwrap();
    if let Some(u) = &scm.url {
        writeln!(out, "    <url>{}</url>", xml_escape(u)).unwrap();
    }
    if let Some(c) = &scm.connection {
        writeln!(out, "    <connection>{}</connection>", xml_escape(c)).unwrap();
    }
    if let Some(dc) = &scm.developer_connection {
        writeln!(out, "    <developerConnection>{}</developerConnection>", xml_escape(dc)).unwrap();
    }
    writeln!(out, "  </scm>").unwrap();
}

fn write_dependencies(out: &mut String, deps: &[Gav]) {
    if deps.is_empty() {
        return;
    }
    writeln!(out, "  <dependencies>").unwrap();
    for gav in deps {
        writeln!(out, "    <dependency>").unwrap();
        writeln!(out, "      <groupId>{}</groupId>", xml_escape(&gav.group)).unwrap();
        writeln!(out, "      <artifactId>{}</artifactId>", xml_escape(&gav.artifact)).unwrap();
        writeln!(out, "      <version>{}</version>", xml_escape(&gav.version)).unwrap();
        writeln!(out, "      <scope>compile</scope>").unwrap();
        writeln!(out, "    </dependency>").unwrap();
    }
    writeln!(out, "  </dependencies>").unwrap();
}

/// XML-escape `&`, `<`, `>`, `"`, `'` in a borrowed string.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
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
    use crate::descriptor::{Developer, Scm};

    fn fake_desc(group_id: &str, name: &str, version: &str, pub_cfg: PublishConfig) -> Descriptor {
        use crate::descriptor::*;
        use std::collections::BTreeMap;
        Descriptor {
            kind: DescriptorKind::Library(Library {
                name: name.to_string(),
                version: version.to_string(),
                group_id: Some(group_id.to_string()),
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            native_image: NativeImage::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            dependencies: BTreeMap::new(),
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
            publish: pub_cfg,
        }
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
        use crate::descriptor::*;
        use std::collections::BTreeMap;
        let desc = Descriptor {
            kind: DescriptorKind::Library(Library {
                name: "x".into(),
                version: "1.0".into(),
                group_id: None,
            }),
            java: Java::default(),
            test: Test::default(),
            kotlin: Kotlin::default(),
            groovy: Groovy::default(),
            native_image: NativeImage::default(),
            docker: Docker::default(),
            build_info: BuildInfo::default(),
            dependencies: BTreeMap::new(),
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
        };
        let err = build_pom(&desc, &[]).unwrap_err().to_string();
        assert!(err.contains("groupId"), "got: {err}");
    }
}
