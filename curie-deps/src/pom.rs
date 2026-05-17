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

/// Where we are inside the POM document.
///
/// One state per container we care about, plus `Root` for "before `<project>`".
/// Unrecognized child elements stay in the parent's context — that matches the
/// behaviour of the previous `path.contains(...)` dispatch and is good enough
/// for real-world POMs, which don't put `<dependencies>` or `<dependency>` in
/// custom wrapper elements.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Ctx {
    Root,
    Project,
    Parent,
    Properties,
    Dependencies,
    Dependency,
    Exclusions,
    Exclusion,
    DepMgmt,
    DepMgmtDependencies,
    ManagedDependency,
    ManagedExclusions,
    ManagedExclusion,
}

/// Transition function: given the current container and a child tag, return
/// the child's container.  Unrecognized tags inherit the parent's context so
/// nested leaf elements (like `<groupId>` inside `<project>`) still dispatch
/// against the right parent on End.
fn next_ctx(parent: Ctx, tag: &str) -> Ctx {
    match (parent, tag) {
        (Ctx::Root, "project") => Ctx::Project,
        (Ctx::Project, "parent") => Ctx::Parent,
        (Ctx::Project, "properties") => Ctx::Properties,
        (Ctx::Project, "dependencies") => Ctx::Dependencies,
        (Ctx::Project, "dependencyManagement") => Ctx::DepMgmt,
        (Ctx::Dependencies, "dependency") => Ctx::Dependency,
        (Ctx::Dependency, "exclusions") => Ctx::Exclusions,
        (Ctx::Exclusions, "exclusion") => Ctx::Exclusion,
        (Ctx::DepMgmt, "dependencies") => Ctx::DepMgmtDependencies,
        (Ctx::DepMgmtDependencies, "dependency") => Ctx::ManagedDependency,
        (Ctx::ManagedDependency, "exclusions") => Ctx::ManagedExclusions,
        (Ctx::ManagedExclusions, "exclusion") => Ctx::ManagedExclusion,
        _ => parent,
    }
}

/// Apply a known dependency-field value (`groupId`, `version`, …) to the
/// scratch `PomDep` being assembled inside `<dependency>` / managed
/// `<dependency>`.  Unknown fields are silently ignored, matching POM
/// schema permissiveness.
fn assign_dep_field(dep: &mut PomDep, field: &str, text: &str) {
    match field {
        "groupId" => dep.group_id = text.to_string(),
        "artifactId" => dep.artifact_id = text.to_string(),
        "version" => dep.version = Some(text.to_string()),
        "scope" => dep.scope = Some(text.to_string()),
        "type" => dep.type_ = Some(text.to_string()),
        "optional" => dep.optional = text.trim() == "true",
        _ => {}
    }
}

/// Mutator that lazily creates a `ParentRef` so each of the three nested
/// fields (`groupId` / `artifactId` / `version`) can assign without
/// repeating the `get_or_insert_with` boilerplate.
fn parent_ref_mut(p: &mut Option<ParentRef>) -> &mut ParentRef {
    p.get_or_insert_with(|| ParentRef {
        group_id: String::new(),
        artifact_id: String::new(),
        version: String::new(),
    })
}

/// Parse POM XML from a string, returning a [`Pom`].
///
/// Dispatch is driven by a small state machine ([`Ctx`]) instead of joined
/// path strings, so the parser is allocation-free per event and the
/// dispatch table is exhaustive enough that the compiler will flag a stale
/// branch if a context is renamed.
pub fn parse(xml: &str) -> Result<Pom> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut pom = Pom::default();

    // `stack[i]` is the Ctx we entered at depth `i+1`.  `stack.last()` is
    // the "current container".
    let mut stack: Vec<Ctx> = Vec::new();
    let mut cur_dep: Option<PomDep> = None;
    let mut cur_mgd: Option<PomDep> = None;
    let mut text_buf = String::new();

    loop {
        match reader.read_event().context("XML read error")? {
            Event::Start(e) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())
                    .context("invalid UTF-8 in POM tag name")?
                    .to_string();
                let parent_ctx = stack.last().copied().unwrap_or(Ctx::Root);
                let new_ctx = next_ctx(parent_ctx, &tag);

                // Only initialize scratch buffers when we're entering a new
                // *container* — not when an unrecognized leaf inherits the
                // parent's context (which would also appear as new_ctx ==
                // Dependency/ManagedDependency but with parent_ctx ==
                // new_ctx).
                if new_ctx == Ctx::Dependency && parent_ctx != Ctx::Dependency {
                    cur_dep = Some(PomDep::empty());
                } else if new_ctx == Ctx::ManagedDependency && parent_ctx != Ctx::ManagedDependency {
                    cur_mgd = Some(PomDep::empty());
                }

                stack.push(new_ctx);
                text_buf.clear();
            }
            Event::Text(e) => {
                // Append rather than replace so values split across multiple
                // Text events (e.g. with an embedded CDATA section) survive.
                let s = e.unescape().context("invalid XML escape in POM")?;
                text_buf.push_str(&s);
            }
            Event::End(e) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())
                    .context("invalid UTF-8 in POM tag name")?
                    .to_string();
                let leaving_ctx = stack.pop().unwrap_or(Ctx::Root);
                let parent_ctx = stack.last().copied().unwrap_or(Ctx::Root);

                // Leaf assignments: we're closing a value tag whose parent
                // is a recognized container.
                match (parent_ctx, tag.as_str()) {
                    (Ctx::Project, "groupId") => pom.group_id = Some(text_buf.clone()),
                    (Ctx::Project, "artifactId") => pom.artifact_id = Some(text_buf.clone()),
                    (Ctx::Project, "version") => pom.version = Some(text_buf.clone()),

                    (Ctx::Parent, "groupId") => parent_ref_mut(&mut pom.parent).group_id = text_buf.clone(),
                    (Ctx::Parent, "artifactId") => parent_ref_mut(&mut pom.parent).artifact_id = text_buf.clone(),
                    (Ctx::Parent, "version") => parent_ref_mut(&mut pom.parent).version = text_buf.clone(),

                    (Ctx::Properties, key) => {
                        pom.properties.insert(key.to_string(), text_buf.clone());
                    }

                    (Ctx::Dependency, field) => {
                        if let Some(d) = cur_dep.as_mut() {
                            assign_dep_field(d, field, &text_buf);
                        }
                    }
                    (Ctx::ManagedDependency, field) => {
                        if let Some(d) = cur_mgd.as_mut() {
                            assign_dep_field(d, field, &text_buf);
                        }
                    }
                    _ => {}
                }

                // Container finalization: closing the actual `<dependency>`
                // element (rather than a leaf inside it) pushes its scratch
                // struct into the right list on `pom`.
                //
                // We distinguish via (leaving_ctx, parent_ctx): a leaf inside
                // a Dependency leaves Ctx=Dependency and returns to Ctx=Dependency
                // (unrecognized child inherits parent ctx).  The real container
                // close leaves Ctx=Dependency and returns to Ctx=Dependencies.
                match (leaving_ctx, parent_ctx) {
                    (Ctx::Dependency, Ctx::Dependencies) => {
                        if let Some(d) = cur_dep.take() {
                            pom.dependencies.push(d);
                        }
                    }
                    (Ctx::ManagedDependency, Ctx::DepMgmtDependencies) => {
                        if let Some(d) = cur_mgd.take() {
                            finalize_managed_dep(d, &mut pom);
                        }
                    }
                    _ => {}
                }

                text_buf.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(pom)
}

/// Route a completed `<dependencyManagement>/<dependency>` entry to the
/// right field on `pom`: `bom_imports` when scope=import + type=pom,
/// otherwise `managed_versions` (keyed `group:artifact`).
fn finalize_managed_dep(d: PomDep, pom: &mut Pom) {
    let is_bom_import =
        d.scope.as_deref() == Some("import") && d.type_.as_deref() == Some("pom");
    if is_bom_import {
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

impl PomDep {
    fn empty() -> Self {
        PomDep {
            group_id: String::new(),
            artifact_id: String::new(),
            version: None,
            scope: None,
            type_: None,
            optional: false,
        }
    }
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

    // --- exclusions do not corrupt groupId/artifactId --------------------------

    #[test]
    fn exclusions_do_not_overwrite_dep_fields() {
        // Regression test: <exclusions><exclusion><groupId> was incorrectly
        // overwriting the parent dependency's groupId/artifactId because
        // those tags were dispatched against the Dependency context.
        let xml = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId><artifactId>x</artifactId><version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>org.codehaus.woodstox</groupId>
      <artifactId>stax2-api</artifactId>
      <version>4.2.2</version>
      <exclusions>
        <exclusion>
          <groupId>javax.xml.stream</groupId>
          <artifactId>stax-api</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>"#;
        let pom = parse(xml).unwrap();
        assert_eq!(pom.dependencies.len(), 1);
        let dep = &pom.dependencies[0];
        assert_eq!(dep.group_id, "org.codehaus.woodstox", "exclusion groupId must not overwrite dep groupId");
        assert_eq!(dep.artifact_id, "stax2-api", "exclusion artifactId must not overwrite dep artifactId");
        assert_eq!(dep.version.as_deref(), Some("4.2.2"));
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
