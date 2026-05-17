//! Minimal Maven POM parser.
//!
//! Parses only what curie-deps needs:
//!   - `<groupId>`, `<artifactId>`, `<version>` (own + parent)
//!   - `<dependencies>` with scope filtering (compile/runtime only; skip test/provided/optional)
//!   - `<dependencyManagement>` entries, including BOM imports
//!     (`<scope>import</scope>` + `<type>pom</type>`)
//!
//! Property interpolation (`${...}`) is handled for the common `${project.version}`
//! and `${project.parent.version}` patterns.  Full property resolution is left
//! for a future iteration.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

/// A parsed POM with only the fields relevant to dependency resolution.
#[derive(Debug, Default)]
pub struct Pom {
    pub group_id: Option<String>,
    pub artifact_id: Option<String>,
    pub version: Option<String>,
    pub parent: Option<ParentRef>,
    pub properties: HashMap<String, String>,
    pub dependencies: Vec<PomDep>,
    /// Versions declared in `<dependencyManagement>`, keyed by `"group:artifact"`.
    /// Does NOT include BOM imports (`scope=import` + `type=pom`) — those go to
    /// [`bom_imports`].
    pub managed_versions: HashMap<String, String>,
    /// BOM imports found in `<dependencyManagement>`: entries with
    /// `<scope>import</scope>` and `<type>pom</type>`.  These must be fetched
    /// and their managed versions merged into the resolution context.
    pub bom_imports: Vec<BomRef>,
}

#[derive(Debug, Clone)]
pub struct ParentRef {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

/// A BOM referenced via `<scope>import</scope>` + `<type>pom</type>` inside a
/// `<dependencyManagement>` block.  The resolver must fetch this POM and merge
/// its managed versions into the resolution context.
#[derive(Debug, Clone)]
pub struct BomRef {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct PomDep {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    /// The `<type>` element value (e.g. `"pom"` for BOM imports).
    pub type_: Option<String>,
    pub optional: bool,
}

impl PomDep {
    /// Returns `true` when this dependency should be included on the compile classpath.
    pub fn is_compile_scope(&self) -> bool {
        if self.optional {
            return false;
        }
        matches!(self.scope.as_deref(), None | Some("compile") | Some("runtime"))
    }
}

impl Pom {
    /// Effective group ID, falling back to parent group if own is absent.
    pub fn effective_group(&self) -> Option<&str> {
        self.group_id
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.group_id.as_str()))
    }

    /// Effective version, falling back to parent version if own is absent.
    pub fn effective_version(&self) -> Option<&str> {
        self.version
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.version.as_str()))
    }

    /// Resolve `${property}` references against the pom's own properties and
    /// a small set of built-in variables derived from parent/self.
    ///
    /// Iterates until stable (up to 10 passes) to handle chained references
    /// such as `${jackson.version.annotations}` → `${jackson.version}` → `2.17.2`.
    pub fn resolve_value(&self, value: &str) -> String {
        if !value.contains("${") {
            return value.to_string();
        }
        let mut result = value.to_string();

        for _ in 0..10 {
            if !result.contains("${") {
                break;
            }
            let prev = result.clone();

            // Built-in project variables.
            if let Some(v) = self.effective_version() {
                result = result.replace("${project.version}", v);
                result = result.replace("${project.parent.version}", v);
            }

            // User-defined <properties>.
            for (k, v) in &self.properties {
                result = result.replace(&format!("${{{}}}", k), v);
            }

            // No progress — stop to avoid infinite loop.
            if result == prev {
                break;
            }
        }

        result
    }
}

/// Parse POM XML from a string, returning a [`Pom`].
pub fn parse(xml: &str) -> Result<Pom> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut pom = Pom::default();

    // tag stack for tracking position
    let mut stack: Vec<String> = Vec::new();

    // scratch buffers for current dependency being parsed
    let mut cur_dep: Option<PomDep> = None;
    // scratch buffer for current dependencyManagement entry
    let mut cur_mgd: Option<PomDep> = None;
    // current text accumulator
    let mut text_buf = String::new();

    loop {
        match reader.read_event().context("XML read error")? {
            Event::Start(e) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();

                if tag == "dependency" && in_direct_dependencies(&stack) {
                    cur_dep = Some(PomDep {
                        group_id: String::new(),
                        artifact_id: String::new(),
                        version: None,
                        scope: None,
                        type_: None,
                        optional: false,
                    });
                }

                if tag == "dependency" && in_dependency_management(&stack) {
                    cur_mgd = Some(PomDep {
                        group_id: String::new(),
                        artifact_id: String::new(),
                        version: None,
                        scope: None,
                        type_: None,
                        optional: false,
                    });
                }

                stack.push(tag);
                text_buf.clear();
            }
            Event::Text(e) => {
                text_buf = e.unescape().unwrap_or_default().to_string();
            }
            Event::End(e) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();

                let path = stack.join("/");

                match path.as_str() {
                    // --- top-level fields ----------------------------------------
                    "project/groupId" => pom.group_id = Some(text_buf.clone()),
                    "project/artifactId" => pom.artifact_id = Some(text_buf.clone()),
                    "project/version" => pom.version = Some(text_buf.clone()),

                    // --- parent --------------------------------------------------
                    "project/parent/groupId" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).group_id = text_buf.clone();
                    }
                    "project/parent/artifactId" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).artifact_id = text_buf.clone();
                    }
                    "project/parent/version" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).version = text_buf.clone();
                    }

                    // --- properties ----------------------------------------------
                    _ if path.starts_with("project/properties/") => {
                        if let Some(key) = path.strip_prefix("project/properties/") {
                            pom.properties.insert(key.to_string(), text_buf.clone());
                        }
                    }

                    // --- dependency fields ---------------------------------------
                    _ if path.ends_with("/dependency/groupId")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.group_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/artifactId")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.artifact_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/version")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.version = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/scope")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.scope = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/optional")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.optional = text_buf.trim() == "true";
                        }
                    }
                    _ if tag == "dependency" && in_direct_dependencies_path(&path) => {
                        if let Some(d) = cur_dep.take() {
                            pom.dependencies.push(d);
                        }
                    }

                    // --- dependencyManagement fields -----------------------------
                    _ if path.ends_with("/dependency/groupId")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.group_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/artifactId")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.artifact_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/version")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.version = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/scope")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.scope = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/type")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.type_ = Some(text_buf.clone());
                        }
                    }
                    _ if tag == "dependency" && path.contains("dependencyManagement") => {
                        if let Some(d) = cur_mgd.take() {
                            let is_bom_import =
                                d.scope.as_deref() == Some("import")
                                && d.type_.as_deref() == Some("pom");
                            if is_bom_import {
                                // Route to bom_imports — the resolver will fetch this
                                // POM and merge its managed versions.
                                if let Some(v) = d.version {
                                    pom.bom_imports.push(BomRef {
                                        group_id: d.group_id,
                                        artifact_id: d.artifact_id,
                                        version: v,
                                    });
                                }
                            } else if let Some(v) = d.version {
                                let key = format!("{}:{}", d.group_id, d.artifact_id);
                                pom.managed_versions.insert(key, v);
                            }
                        }
                    }

                    _ => {}
                }

                stack.pop();
                text_buf.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(pom)
}

/// `stack` currently points *inside* `project/dependencies` (not `dependencyManagement`).
fn in_direct_dependencies(stack: &[String]) -> bool {
    let path = stack.join("/");
    path == "project/dependencies"
        || path.ends_with("/project/dependencies")
}

fn in_dependency_management(stack: &[String]) -> bool {
    let path = stack.join("/");
    path.contains("dependencyManagement") && path.ends_with("/dependencies")
}

fn in_direct_dependencies_path(path: &str) -> bool {
    // must contain /dependencies/ but NOT /dependencyManagement/
    path.contains("/dependencies/") && !path.contains("dependencyManagement")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_POM: &str = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>myapp</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.2.0-jre</version>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#;

    #[test]
    fn parse_basic() {
        let pom = parse(SIMPLE_POM).unwrap();
        assert_eq!(pom.group_id.as_deref(), Some("com.example"));
        assert_eq!(pom.artifact_id.as_deref(), Some("myapp"));
        assert_eq!(pom.version.as_deref(), Some("1.0.0"));
        assert_eq!(pom.dependencies.len(), 2);
    }

    #[test]
    fn compile_scope_filter() {
        let pom = parse(SIMPLE_POM).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert_eq!(compile.len(), 1);
        assert_eq!(compile[0].artifact_id, "guava");
    }

    // --- parsing: parent --------------------------------------------------------

    #[test]
    fn parse_parent() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <artifactId>child</artifactId>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>parent-pom</artifactId>
    <version>2.0.0</version>
  </parent>
</project>"#;
        let pom = parse(xml).unwrap();
        let parent = pom.parent.as_ref().expect("parent should be present");
        assert_eq!(parent.group_id, "com.example");
        assert_eq!(parent.artifact_id, "parent-pom");
        assert_eq!(parent.version, "2.0.0");
    }

    // --- parsing: properties ----------------------------------------------------

    #[test]
    fn parse_properties() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>myapp</artifactId>
  <version>1.0.0</version>
  <properties>
    <my.lib.version>3.5.1</my.lib.version>
    <encoding>UTF-8</encoding>
  </properties>
</project>"#;
        let pom = parse(xml).unwrap();
        assert_eq!(pom.properties.get("my.lib.version").map(String::as_str), Some("3.5.1"));
        assert_eq!(pom.properties.get("encoding").map(String::as_str), Some("UTF-8"));
    }

    // --- parsing: dependencyManagement ------------------------------------------

    #[test]
    fn parse_dependency_management() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>bom</artifactId>
  <version>1.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.slf4j</groupId>
        <artifactId>slf4j-api</artifactId>
        <version>2.0.9</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;
        let pom = parse(xml).unwrap();
        assert_eq!(
            pom.managed_versions.get("org.slf4j:slf4j-api").map(String::as_str),
            Some("2.0.9")
        );
        // dependencyManagement entries are NOT in pom.dependencies
        assert!(pom.dependencies.is_empty());
        // No BOM imports
        assert!(pom.bom_imports.is_empty());
    }

    #[test]
    fn parse_bom_import_goes_to_bom_imports_not_managed_versions() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>parent</artifactId>
  <version>1.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.fasterxml.jackson</groupId>
        <artifactId>jackson-bom</artifactId>
        <version>2.17.2</version>
        <type>pom</type>
        <scope>import</scope>
      </dependency>
      <dependency>
        <groupId>org.slf4j</groupId>
        <artifactId>slf4j-api</artifactId>
        <version>2.0.9</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;
        let pom = parse(xml).unwrap();
        // BOM import goes to bom_imports, not managed_versions
        assert_eq!(pom.bom_imports.len(), 1);
        let bom = &pom.bom_imports[0];
        assert_eq!(bom.group_id, "com.fasterxml.jackson");
        assert_eq!(bom.artifact_id, "jackson-bom");
        assert_eq!(bom.version, "2.17.2");
        assert!(!pom.managed_versions.contains_key("com.fasterxml.jackson:jackson-bom"));
        // Regular managed dep is still captured
        assert_eq!(
            pom.managed_versions.get("org.slf4j:slf4j-api").map(String::as_str),
            Some("2.0.9")
        );
    }

    #[test]
    fn parse_managed_dep_with_scope_not_treated_as_bom_import() {
        // A managed dep with scope=compile should NOT go to bom_imports
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>parent</artifactId>
  <version>1.0.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.example</groupId>
        <artifactId>some-lib</artifactId>
        <version>3.0.0</version>
        <scope>compile</scope>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>"#;
        let pom = parse(xml).unwrap();
        assert!(pom.bom_imports.is_empty());
        assert_eq!(
            pom.managed_versions.get("org.example:some-lib").map(String::as_str),
            Some("3.0.0")
        );
    }

    // --- scope / optional filtering --------------------------------------------

    #[test]
    fn parse_optional_dep_excluded() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId><artifactId>x</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>org.example</groupId>
      <artifactId>optional-lib</artifactId>
      <version>1.0</version>
      <optional>true</optional>
    </dependency>
  </dependencies>
</project>"#;
        let pom = parse(xml).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert!(compile.is_empty(), "optional dep should be excluded from compile scope");
    }

    #[test]
    fn parse_provided_scope_excluded() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId><artifactId>x</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>javax.servlet</groupId>
      <artifactId>servlet-api</artifactId>
      <version>2.5</version>
      <scope>provided</scope>
    </dependency>
  </dependencies>
</project>"#;
        let pom = parse(xml).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert!(compile.is_empty(), "provided-scope dep should be excluded");
    }

    #[test]
    fn parse_runtime_scope_included() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId><artifactId>x</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>org.postgresql</groupId>
      <artifactId>postgresql</artifactId>
      <version>42.7.3</version>
      <scope>runtime</scope>
    </dependency>
  </dependencies>
</project>"#;
        let pom = parse(xml).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert_eq!(compile.len(), 1, "runtime-scope dep should be included");
        assert_eq!(compile[0].artifact_id, "postgresql");
    }

    #[test]
    fn parse_test_scope_excluded() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId><artifactId>x</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>org.junit.jupiter</groupId>
      <artifactId>junit-jupiter</artifactId>
      <version>5.10.0</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#;
        let pom = parse(xml).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert!(compile.is_empty(), "test-scope dep should be excluded");
    }

    // --- resolve_value ----------------------------------------------------------

    #[test]
    fn resolve_project_version() {
        let mut pom = Pom::default();
        pom.version = Some("4.2.0".to_string());
        assert_eq!(pom.resolve_value("${project.version}"), "4.2.0");
    }

    #[test]
    fn resolve_project_parent_version() {
        let mut pom = Pom::default();
        // own version absent; falls back to parent version
        pom.parent = Some(ParentRef {
            group_id: "com.example".to_string(),
            artifact_id: "parent".to_string(),
            version: "3.1.0".to_string(),
        });
        assert_eq!(pom.resolve_value("${project.parent.version}"), "3.1.0");
    }

    #[test]
    fn resolve_custom_property() {
        let mut pom = Pom::default();
        pom.version = Some("1.0.0".to_string());
        pom.properties.insert("jackson.version".to_string(), "2.17.2".to_string());
        assert_eq!(pom.resolve_value("${jackson.version}"), "2.17.2");
    }

    #[test]
    fn resolve_chained_property() {
        // Mirrors the real jackson-bom chain:
        //   ${jackson.version.annotations} -> ${jackson.version} -> 2.17.2
        let mut pom = Pom::default();
        pom.properties.insert(
            "jackson.version".to_string(),
            "2.17.2".to_string(),
        );
        pom.properties.insert(
            "jackson.version.annotations".to_string(),
            "${jackson.version}".to_string(),
        );
        assert_eq!(pom.resolve_value("${jackson.version.annotations}"), "2.17.2");
    }

    #[test]
    fn resolve_unknown_property_unchanged() {
        let pom = Pom::default();
        // Should return the placeholder as-is, not panic.
        assert_eq!(pom.resolve_value("${totally.unknown}"), "${totally.unknown}");
    }

    // --- effective_group / effective_version ------------------------------------

    #[test]
    fn effective_group_own() {
        let mut pom = Pom::default();
        pom.group_id = Some("com.example".to_string());
        assert_eq!(pom.effective_group(), Some("com.example"));
    }

    #[test]
    fn effective_group_fallback_to_parent() {
        let mut pom = Pom::default();
        pom.group_id = None;
        pom.parent = Some(ParentRef {
            group_id: "com.parent".to_string(),
            artifact_id: "parent".to_string(),
            version: "1.0".to_string(),
        });
        assert_eq!(pom.effective_group(), Some("com.parent"));
    }

    #[test]
    fn effective_version_own() {
        let mut pom = Pom::default();
        pom.version = Some("5.0.0".to_string());
        assert_eq!(pom.effective_version(), Some("5.0.0"));
    }

    #[test]
    fn effective_version_fallback_to_parent() {
        let mut pom = Pom::default();
        pom.version = None;
        pom.parent = Some(ParentRef {
            group_id: "com.example".to_string(),
            artifact_id: "parent".to_string(),
            version: "9.9.9".to_string(),
        });
        assert_eq!(pom.effective_version(), Some("9.9.9"));
    }
}
