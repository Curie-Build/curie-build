use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// A fully-validated Curie project descriptor.
///
/// The mutually-exclusive `[application]` / `[library]` / `[workspace]`
/// sections are reified as the [`DescriptorKind`] enum: a Descriptor with
/// `kind: DescriptorKind::Application(_)` is statically guaranteed to be
/// an application, no `unreachable!()` branches needed.  Serde-side
/// parsing happens via a private flat-shape struct in [`load`].
#[derive(Debug)]
pub struct Descriptor {
    pub kind: DescriptorKind,
    pub java: Java,
    pub docker: Docker,
    pub dependencies: BTreeMap<String, String>,
    pub test_dependencies: BTreeMap<String, String>,
    pub repositories: Vec<RepositoryEntry>,
    pub bom_imports: BTreeMap<String, String>,
    pub test_bom_imports: BTreeMap<String, String>,
    /// BOMs inherited from the surrounding workspace's `[bom-imports]`,
    /// populated by `workspace::load` during inheritance merge.  Empty in
    /// single-module mode.  Lower priority than the member's own
    /// [`bom_imports`]: in `prod_bom_gavs()` these are emitted first so the
    /// resolver's later-wins semantics let the member override the workspace.
    pub inherited_bom_imports: BTreeMap<String, String>,
    /// Same as [`inherited_bom_imports`] for `[test-bom-imports]`.  Lower
    /// priority than the member's own [`test_bom_imports`].
    pub inherited_test_bom_imports: BTreeMap<String, String>,
    pub workspace_dependencies: BTreeMap<String, WorkspaceDep>,
}

/// Which top-level section the descriptor declares.  Exactly one variant
/// per descriptor — enforced by [`load`] at parse time.
#[derive(Debug)]
pub enum DescriptorKind {
    Application(Application),
    Library(Library),
    /// Workspace root: lists members but is not itself buildable.
    Workspace(Workspace),
}

/// Flat shape for serde — every section is `Option`, and [`load`]
/// validates exactly-one-of and converts to [`DescriptorKind`].  Kept
/// private to descriptor.rs; consumers only see the validated
/// [`Descriptor`].
#[derive(Debug, Deserialize)]
struct RawDescriptor {
    application: Option<Application>,
    library: Option<Library>,
    workspace: Option<Workspace>,
    #[serde(default)]
    java: Java,
    #[serde(default)]
    docker: Docker,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(rename = "test-dependencies", default)]
    test_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    repositories: Vec<RepositoryEntry>,
    #[serde(rename = "bom-imports", default)]
    bom_imports: BTreeMap<String, String>,
    #[serde(rename = "test-bom-imports", default)]
    test_bom_imports: BTreeMap<String, String>,
    #[serde(rename = "workspace-dependencies", default)]
    workspace_dependencies: BTreeMap<String, WorkspaceDep>,
}

/// One entry in `[workspace-dependencies]`.
///
/// Today only `path` is supported.  In future this may grow `features`,
/// optional flags, or scope hints — the struct shape leaves room for that
/// without breaking the table key.
#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceDep {
    pub path: String,
    /// Catch-all so a user who tries `version = "1.0"` (a common Cargo
    /// muscle-memory mistake) gets a precise rejection at load time.
    /// Validated in [`load`]; never read after that.
    #[serde(default)]
    #[allow(dead_code)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Application {
    pub name: String,
    pub version: String,
    /// The fully-qualified main class name.  When omitted, curie will scan
    /// production sources and compiled bytecode to detect it automatically.
    #[serde(rename = "mainClass")]
    pub main_class: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Library {
    pub name: String,
    pub version: String,
}

/// Workspace descriptor: lists member directories whose own `Curie.toml`
/// files are buildable modules.  Member paths are relative to the workspace
/// `Curie.toml` directory.
#[derive(Debug, Deserialize)]
pub struct Workspace {
    pub members: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Java {
    /// `[java].sourceCompatibility` as the user wrote it, or `None` when
    /// the key was absent.  Use [`Self::effective`] to get the
    /// resolved value (default `"21"`) — never read this field directly
    /// from compile/test paths, because `None` is meaningful: it signals
    /// "inherit from the workspace if any, else use the default".
    #[serde(rename = "sourceCompatibility")]
    pub source_compatibility: Option<String>,
}

impl Java {
    /// Resolved `--release` argument for `javac`.  Workspace inheritance
    /// happens upstream of this call (in `workspace::load`), so by the
    /// time the build pipeline reads it the member's `source_compatibility`
    /// has already been populated with the workspace value if applicable.
    pub fn effective(&self) -> &str {
        self.source_compatibility.as_deref().unwrap_or("21")
    }
}

#[derive(Debug, Deserialize)]
pub struct Docker {
    #[serde(rename = "baseImage", default = "default_base_image")]
    pub base_image: String,
    #[serde(rename = "imageName")]
    pub image_name: Option<String>,
    #[serde(rename = "imageTag")]
    pub image_tag: Option<String>,
    /// Tracks whether the [docker] section was explicitly present in Curie.toml.
    /// Set by Descriptor::load after deserialisation via a raw TOML check.
    #[serde(skip)]
    pub section_present: bool,
}

fn default_base_image() -> String {
    "eclipse-temurin:21-jre-alpine".to_string()
}

impl Default for Docker {
    fn default() -> Self {
        Docker {
            base_image: default_base_image(),
            image_name: None,
            image_tag: None,
            section_present: false,
        }
    }
}

/// An additional Maven-compatible repository declared in `[[repositories]]`.
#[derive(Debug, Deserialize, Clone)]
pub struct RepositoryEntry {
    pub name: String,
    pub url: String,
}

impl Descriptor {
    pub fn is_library(&self) -> bool {
        matches!(self.kind, DescriptorKind::Library(_))
    }

    /// Workspace roots are not themselves buildable — they list member
    /// directories whose own `Curie.toml` files are the buildable modules.
    pub fn is_workspace(&self) -> bool {
        matches!(self.kind, DescriptorKind::Workspace(_))
    }

    /// View the `[application]` section if this descriptor is one.
    pub fn application(&self) -> Option<&Application> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(a),
            _ => None,
        }
    }

    /// View the `[workspace]` section if this descriptor is a workspace root.
    pub fn workspace(&self) -> Option<&Workspace> {
        match &self.kind {
            DescriptorKind::Workspace(w) => Some(w),
            _ => None,
        }
    }

    /// Short human-readable kind for `curie list` output and error messages.
    pub fn kind_label(&self) -> &'static str {
        match &self.kind {
            DescriptorKind::Application(_) => "application",
            DescriptorKind::Library(_) => "library",
            DescriptorKind::Workspace(_) => "workspace",
        }
    }

    /// Project name.  `None` for a workspace root, which has no name of
    /// its own — only its members do.
    pub fn project_name(&self) -> Option<&str> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(&a.name),
            DescriptorKind::Library(l) => Some(&l.name),
            DescriptorKind::Workspace(_) => None,
        }
    }

    /// Project version.  `None` for a workspace root.
    pub fn project_version(&self) -> Option<&str> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(&a.version),
            DescriptorKind::Library(l) => Some(&l.version),
            DescriptorKind::Workspace(_) => None,
        }
    }

    /// Convenience: panic-with-context wrapper around [`project_name`]
    /// for use in build/test/compile paths where the caller knows the
    /// descriptor is buildable (those paths never run on a workspace
    /// root — workspaces are unwrapped to their members by `workspace::*`).
    ///
    /// Prefer matching on `kind` directly where ambiguity is possible.
    pub fn buildable_name(&self) -> &str {
        self.project_name()
            .expect("buildable_name() called on a workspace descriptor")
    }

    /// See [`buildable_name`]; same contract for the version.
    pub fn buildable_version(&self) -> &str {
        self.project_version()
            .expect("buildable_version() called on a workspace descriptor")
    }

    /// Resolved Docker image name: descriptor override or application name.
    /// Only meaningful for application descriptors; the helper falls back
    /// on `project_name()` which is `Some` for any buildable kind.
    pub fn image_name(&self) -> &str {
        self.docker
            .image_name
            .as_deref()
            .or_else(|| self.project_name())
            .expect("image_name() called on a workspace descriptor")
    }

    /// Resolved Docker image tag: descriptor override or application version.
    pub fn image_tag(&self) -> &str {
        self.docker
            .image_tag
            .as_deref()
            .or_else(|| self.project_version())
            .expect("image_tag() called on a workspace descriptor")
    }

    /// Full image reference, e.g. "hello-world:0.1.0".
    pub fn image_ref(&self) -> String {
        format!("{}:{}", self.image_name(), self.image_tag())
    }

    /// Parse `[bom-imports]` into a `Vec<curie_deps::Gav>` for the
    /// resolver, in priority-ascending order (later wins).
    ///
    /// Order:
    ///   1. workspace-inherited prod BOMs (lowest)
    ///   2. member's own prod BOMs (override 1)
    pub fn prod_bom_gavs(&self) -> anyhow::Result<Vec<curie_deps::Gav>> {
        let mut v: Vec<curie_deps::Gav> = self
            .inherited_bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in workspace [bom-imports]")?;
        let own: Vec<curie_deps::Gav> = self
            .bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in [bom-imports]")?;
        v.extend(own);
        Ok(v)
    }

    /// Parse `[bom-imports]` + `[test-bom-imports]` into a merged
    /// `Vec<curie_deps::Gav>` for the test resolver, priority-ascending.
    ///
    /// Order:
    ///   1. workspace-inherited prod BOMs (lowest)
    ///   2. member's own prod BOMs
    ///   3. workspace-inherited test BOMs
    ///   4. member's own test BOMs (highest)
    pub fn test_bom_gavs(&self) -> anyhow::Result<Vec<curie_deps::Gav>> {
        let mut v = self.prod_bom_gavs()?;
        let inherited_test: Vec<curie_deps::Gav> = self
            .inherited_test_bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in workspace [test-bom-imports]")?;
        v.extend(inherited_test);
        let own_test: Vec<curie_deps::Gav> = self
            .test_bom_imports
            .iter()
            .map(|(k, ver)| curie_deps::Gav::from_key_version(k, ver))
            .collect::<anyhow::Result<_>>()
            .context("invalid coordinate in [test-bom-imports]")?;
        v.extend(own_test);
        Ok(v)
    }
}

pub fn load(project_root: &Path) -> Result<Descriptor> {
    let path = project_root.join("Curie.toml");

    if !path.exists() {
        bail!(
            "no Curie.toml found in {}",
            project_root.display()
        );
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Detect which top-level sections are explicitly present via a raw
    // first-pass parse.  We can't infer this from the deserialised
    // RawDescriptor alone because `[docker]` with no fields would still
    // populate a default Docker struct — but its absence in the user's
    // file is meaningful (Docker is off unless [docker] OR a project
    // root Dockerfile exists).
    let raw: toml::Value = toml::from_str(&content)
        .map_err(|e| format_parse_error(e, &content, &path))?;
    let table = raw.as_table();
    let docker_section_present = table.map(|t| t.contains_key("docker")).unwrap_or(false);

    let parsed: RawDescriptor = toml::from_str(&content)
        .map_err(|e| format_parse_error(e, &content, &path))?;

    // Exactly one of [application] / [library] / [workspace] — enforced
    // both as a count check (for the diagnostic message) and by reifying
    // the kind into the DescriptorKind enum.
    let kind = match (parsed.application, parsed.library, parsed.workspace) {
        (Some(a), None, None) => DescriptorKind::Application(a),
        (None, Some(l), None) => DescriptorKind::Library(l),
        (None, None, Some(w)) => DescriptorKind::Workspace(w),
        (None, None, None) => bail!(
            "Curie.toml must contain one of [application], [library], or [workspace]"
        ),
        _ => bail!(
            "Curie.toml must contain only one of [application], [library], or [workspace]"
        ),
    };

    let mut docker = parsed.docker;
    docker.section_present = docker_section_present;

    let descriptor = Descriptor {
        kind,
        java: parsed.java,
        docker,
        dependencies: parsed.dependencies,
        test_dependencies: parsed.test_dependencies,
        repositories: parsed.repositories,
        bom_imports: parsed.bom_imports,
        test_bom_imports: parsed.test_bom_imports,
        inherited_bom_imports: BTreeMap::new(),
        inherited_test_bom_imports: BTreeMap::new(),
        workspace_dependencies: parsed.workspace_dependencies,
    };

    // Workspace-only restrictions: they describe member layout, not
    // build inputs of their own.  These checks need the now-built
    // `descriptor` because that's where the deserialised collections live.
    if descriptor.is_workspace() {
        if !descriptor.dependencies.is_empty() {
            bail!("workspace Curie.toml must not declare [dependencies] — declare them in each member");
        }
        if !descriptor.test_dependencies.is_empty() {
            bail!("workspace Curie.toml must not declare [test-dependencies] — declare them in each member");
        }
        if !descriptor.workspace_dependencies.is_empty() {
            bail!("workspace Curie.toml must not declare [workspace-dependencies] — declare them on each member");
        }
        if docker_section_present {
            bail!("workspace Curie.toml must not declare [docker] — declare it on each application member");
        }
    }

    // [workspace-dependencies] entries must be version-less.  The
    // depended-on member's own version is authoritative; declaring one
    // here is almost certainly Cargo muscle-memory and would silently
    // mask a version mismatch.
    for (label, dep) in &descriptor.workspace_dependencies {
        if dep.version.is_some() {
            bail!(
                "workspace-dependency \"{}\" must not declare a version — \
                 the depended-on member's own version is used.  Remove the \
                 `version` key from [workspace-dependencies.{}].",
                label, label,
            );
        }
        if dep.path.trim().is_empty() {
            bail!("workspace-dependency \"{}\" has an empty `path`", label);
        }
    }

    if descriptor.is_library() && docker_section_present {
        bail!(
            "library projects do not support Docker: remove the [docker] section from Curie.toml"
        );
    }

    Ok(descriptor)
}

/// Returns true when Docker support is active:
/// either a [docker] section exists in Curie.toml (non-default base image or
/// explicit name/tag counts as intentional) OR a Dockerfile is present at the
/// project root.
pub fn docker_enabled(project_root: &Path, desc: &Descriptor) -> bool {
    desc.docker.section_present || project_root.join("Dockerfile").exists()
}

// ---------------------------------------------------------------------------
// Parse error formatting
// ---------------------------------------------------------------------------

/// Reformat a `toml::de::Error` into a contextual error with:
///   • a `failed to parse <path>` header
///   • the TOML source line with a caret pointing at the problem
///   • an optional actionable hint for common mistakes
///
/// `toml 0.8` already produces a multi-line display in the form:
///
///   TOML parse error at line N, column M
///     |
///   N | <source line>
///     | ^^^^^^^^^^^^
///   <message>
///
/// We keep that display but swap the generic first line for one that names
/// the file, and append a hint where the error message matches a known
/// pattern.
fn format_parse_error(err: toml::de::Error, _source: &str, path: &Path) -> anyhow::Error {
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    // toml's Display always starts with "TOML parse error at line N, column M".
    // Replace that prefix with one that names the file, keeping the rest of
    // the contextual block (the source line + caret) unchanged.
    let raw_display = err.to_string();
    let contextual = if let Some(rest) = raw_display.strip_prefix("TOML parse error at ") {
        // `rest` is now "line N, column M\n  |\nN | <src>\n  | ^^^^\n<msg>"
        // Reformat as "  --> <file>:N:M\n   |\n ..."
        let reformatted = rest
            .replacen("line ", "", 1)
            .replacen(", column ", ":", 1);
        format!(
            "failed to parse {}\n\n  --> {}:{}",
            path.display(),
            file_name,
            reformatted
        )
    } else {
        format!("failed to parse {}\n\n{}", path.display(), raw_display)
    };

    // Extract the bare error message (last non-empty line of toml's output).
    let message = raw_display
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();

    // Append a hint for known, actionable error patterns.
    let hint = hint_for(message, &file_name);

    let full = if let Some(h) = hint {
        format!("{}\n\n  hint: {}", contextual, h)
    } else {
        contextual
    };

    anyhow::anyhow!("{}", full)
}

/// Return a hint string for well-known error messages, or `None` if the
/// error is already self-explanatory from the caret context alone.
fn hint_for(message: &str, _file_name: &str) -> Option<String> {
    // missing field `name` or `version` — could be in [application] or [library]
    if message.contains("missing field") && message.contains("name") {
        return Some(
            "both [application] and [library] require a `name` field.".to_string(),
        );
    }
    if message.contains("missing field") && message.contains("version") {
        return Some(
            "both [application] and [library] require a `version` field.".to_string(),
        );
    }

    // unknown field — suggest checking for typos
    if message.contains("unknown field") {
        return Some(
            "check for typos in field names; see the README for all supported fields.".to_string(),
        );
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `content` as Curie.toml under a fresh tempdir and call `load`.
    fn load_str(content: &str) -> Result<Descriptor> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Curie.toml"), content).unwrap();
        load(dir.path())
    }

    #[test]
    fn parse_workspace_with_members() {
        let toml = r#"
[workspace]
members = ["a", "b", "nested/c"]
"#;
        let d = load_str(toml).unwrap();
        assert!(d.is_workspace());
        assert_eq!(d.kind_label(), "workspace");
        let ws = d.workspace().expect("workspace section present");
        assert_eq!(ws.members, vec!["a", "b", "nested/c"]);
        // Workspaces have no project-level name or version.
        assert_eq!(d.project_name(), None);
        assert_eq!(d.project_version(), None);
    }

    #[test]
    fn parse_application_still_works() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.is_workspace());
        assert_eq!(d.kind_label(), "application");
        assert_eq!(d.project_name(), Some("x"));
        assert_eq!(d.project_version(), Some("1.0"));
        assert!(d.application().is_some());
    }

    #[test]
    fn workspace_with_application_is_rejected() {
        let toml = r#"
[workspace]
members = ["a"]
[application]
name = "x"
version = "1.0"
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("only one"), "got: {err}");
    }

    #[test]
    fn workspace_with_library_is_rejected() {
        let toml = r#"
[workspace]
members = ["a"]
[library]
name = "x"
version = "1.0"
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("only one"), "got: {err}");
    }

    #[test]
    fn workspace_with_dependencies_is_rejected() {
        let toml = r#"
[workspace]
members = ["a"]
[dependencies]
"com.example:foo" = "1.0"
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("[dependencies]"), "got: {err}");
    }

    #[test]
    fn workspace_with_docker_is_rejected() {
        let toml = r#"
[workspace]
members = ["a"]
[docker]
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("[docker]"), "got: {err}");
    }

    #[test]
    fn workspace_allows_shared_java_and_repositories() {
        // These are inheritable config; workspace may carry them.
        let toml = r#"
[workspace]
members = ["a"]
[java]
sourceCompatibility = "17"
[[repositories]]
name = "Nexus"
url = "https://example.com/m2"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.java.effective(), "17");
        assert_eq!(d.repositories.len(), 1);
    }

    #[test]
    fn empty_descriptor_is_rejected() {
        let err = load_str("").unwrap_err().to_string();
        assert!(err.contains("must contain one of"), "got: {err}");
    }

    // -- workspace-dependencies ---------------------------------------------

    #[test]
    fn parse_workspace_dependencies_path_only() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"
[workspace-dependencies]
core = { path = "../core" }
data = { path = "../sibling/data" }
"#;
        let d = load_str(toml).unwrap();
        let core = d.workspace_dependencies.get("core").unwrap();
        assert_eq!(core.path, "../core");
        assert!(core.version.is_none());
        assert_eq!(d.workspace_dependencies.len(), 2);
    }

    #[test]
    fn workspace_dependency_with_version_is_rejected() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"
[workspace-dependencies]
core = { path = "../core", version = "1.0" }
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("must not declare a version"), "got: {err}");
        assert!(err.contains("core"), "got: {err}");
    }

    #[test]
    fn workspace_dependency_with_empty_path_is_rejected() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"
[workspace-dependencies]
core = { path = "" }
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("empty `path`"), "got: {err}");
    }

    #[test]
    fn workspace_root_with_workspace_dependencies_is_rejected() {
        let toml = r#"
[workspace]
members = ["a"]
[workspace-dependencies]
core = { path = "../core" }
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("[workspace-dependencies]"), "got: {err}");
    }
}
