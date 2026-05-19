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
    /// Populated from `[test]` (only `junitPlatformVersion` today).
    /// Workspace inheritance is applied by `workspace::inherit_from_workspace`
    /// before any build pipeline reads the value.
    pub test: Test,
    /// Populated from `[kotlin]`.  Workspace inheritance works exactly like
    /// the `[java]` scalar: a member's value wins; when absent the workspace
    /// value (if any) is copied in.
    pub kotlin: Kotlin,
    pub docker: Docker,
    pub build_info: BuildInfo,
    pub dependencies: BTreeMap<String, DependencyValue>,
    pub test_dependencies: BTreeMap<String, DependencyValue>,
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
    /// `[annotation-processors]` — coordinates of processor jars to put on
    /// javac's `-processorpath` during production compilation.  Entries are
    /// resolved through the same Maven resolver as `[dependencies]` and
    /// honour `[bom-imports]` for version-less coordinates.
    pub annotation_processors: BTreeMap<String, AnnotationProcessor>,
    /// `[test-annotation-processors]` — same shape, only added to the
    /// processor path when compiling test sources.
    pub test_annotation_processors: BTreeMap<String, AnnotationProcessor>,
    /// Workspace-inherited counterparts, populated by
    /// `workspace::inherit_from_workspace`.  Member-declared entries take
    /// precedence on a key collision.
    pub inherited_annotation_processors: BTreeMap<String, AnnotationProcessor>,
    pub inherited_test_annotation_processors: BTreeMap<String, AnnotationProcessor>,
    /// `[annotation-processor-options.<prefix>]` — nested table keyed by
    /// processor namespace.  Each inner key/value emits a single
    /// `-A<prefix>.<key>=<value>` to javac.  Examples:
    ///
    /// ```toml
    /// [annotation-processor-options.dagger]
    /// fastInit = "enabled"
    ///
    /// [annotation-processor-options.mapstruct]
    /// suppressGeneratorTimestamp = "true"
    /// ```
    pub annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
    /// Test-only counterpart of [`annotation_processor_options`].
    pub test_annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
    pub inherited_annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
    pub inherited_test_annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
}

/// One entry in `[annotation-processors]` or `[test-annotation-processors]`.
///
/// Two shapes accepted, via serde's untagged enum:
///
/// ```toml
/// # Shorthand: the value is just the version string.
/// "com.google.dagger:dagger-compiler" = "2.50"
///
/// # Detailed: extra knobs.  Today the only knob is on-compile-classpath,
/// # which Lombok needs because its annotation types live in the same jar
/// # as the processor itself.
/// "org.projectlombok:lombok" = { version = "1.18.30", on-compile-classpath = true }
/// ```
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum AnnotationProcessor {
    /// `"key" = "1.0.0"` form — equivalent to detailed with defaults.
    Version(String),
    /// `"key" = { version = "1.0.0", on-compile-classpath = bool }` form.
    Detailed(AnnotationProcessorDetailed),
}

#[derive(Debug, Deserialize, Clone)]
pub struct AnnotationProcessorDetailed {
    pub version: String,
    /// When `true`, the processor jar is added to javac's `-cp` in addition
    /// to `-processorpath`.  Needed for processors whose annotation types
    /// are referenced from user code and live in the same jar as the
    /// processor (Lombok is the canonical case).  Default `false`: most
    /// processors (Dagger, MapStruct, AutoValue, Micronaut) split their
    /// API into a separate jar that the user declares under `[dependencies]`.
    #[serde(default, rename = "on-compile-classpath")]
    pub on_compile_classpath: bool,
}

impl AnnotationProcessor {
    /// Version string as the user wrote it.  `""` means "supply via a BOM".
    pub fn version(&self) -> &str {
        match self {
            AnnotationProcessor::Version(v) => v,
            AnnotationProcessor::Detailed(d) => &d.version,
        }
    }

    pub fn on_compile_classpath(&self) -> bool {
        match self {
            AnnotationProcessor::Version(_) => false,
            AnnotationProcessor::Detailed(d) => d.on_compile_classpath,
        }
    }
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
    #[serde(rename = "build-info", default)]
    build_info: BuildInfo,
    #[serde(default)]
    dependencies: BTreeMap<String, DependencyValue>,
    #[serde(rename = "test-dependencies", default)]
    test_dependencies: BTreeMap<String, DependencyValue>,
    #[serde(default)]
    repositories: Vec<RepositoryEntry>,
    #[serde(rename = "bom-imports", default)]
    bom_imports: BTreeMap<String, String>,
    #[serde(rename = "test-bom-imports", default)]
    test_bom_imports: BTreeMap<String, String>,
    #[serde(rename = "workspace-dependencies", default)]
    workspace_dependencies: BTreeMap<String, WorkspaceDep>,
    #[serde(rename = "annotation-processors", default)]
    annotation_processors: BTreeMap<String, AnnotationProcessor>,
    #[serde(rename = "test-annotation-processors", default)]
    test_annotation_processors: BTreeMap<String, AnnotationProcessor>,
    #[serde(rename = "annotation-processor-options", default)]
    annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(rename = "test-annotation-processor-options", default)]
    test_annotation_processor_options: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    test: Test,
    #[serde(default)]
    kotlin: Kotlin,
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

/// Default version of the JUnit Platform Console Standalone launcher
/// that Curie downloads (into `~/.m2`) to execute tests.  Users may
/// override it (including at the workspace root) via:
///
/// ```toml
/// [test]
/// junitPlatformVersion = "6.0.3"
/// ```
pub const DEFAULT_JUNIT_PLATFORM_VERSION: &str = "6.0.3";

/// Default Kotlin version used to resolve `kotlin-compiler-embeddable`
/// and `kotlin-stdlib` from Maven Central whenever any `.kt` sources are
/// present.  Override (workspace-inheritable) with:
///
/// ```toml
/// [kotlin]
/// version = "2.1.21"
/// ```
pub const DEFAULT_KOTLIN_VERSION: &str = "2.1.21";

/// Configuration for the `[test]` table (currently only the version of the
/// JUnit Platform Console Standalone runner that Curie itself downloads).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Test {
    /// `junitPlatformVersion` — matches the camelCase style of
    /// `sourceCompatibility`, `mainClass`, `baseImage`, etc.
    #[serde(rename = "junitPlatformVersion", default)]
    pub junit_platform_version: Option<String>,
}

impl Test {
    /// The version string that will be passed to the resolver for the
    /// `junit-platform-console-standalone` artifact.  After
    /// `workspace::inherit_from_workspace`, a member's field already
    /// contains the workspace value when the member omitted the key.
    pub fn junit_platform_version(&self) -> &str {
        self.junit_platform_version
            .as_deref()
            .unwrap_or(DEFAULT_JUNIT_PLATFORM_VERSION)
    }
}

/// Configuration for the `[kotlin]` table (the version of kotlinc + stdlib
/// that Curie downloads when it sees Kotlin sources).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Kotlin {
    /// Simple `version` key inside the `[kotlin]` table.  The table name
    /// makes the meaning unambiguous.
    #[serde(default)]
    pub version: Option<String>,
}

impl Kotlin {
    /// Effective version passed to the resolver for both the Kotlin
    /// compiler and the stdlib JARs (they are published at the same
    /// version).
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or(DEFAULT_KOTLIN_VERSION)
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

/// Controls generation of `META-INF/build-info.properties` inside the JAR.
///
/// By default (when the `[build-info]` section is absent) Curie generates the
/// file whenever the project directory is inside a Git repository.  Set
/// `enabled = false` to suppress it unconditionally.
///
/// ```toml
/// [build-info]
/// enabled = false
/// ```
#[derive(Debug, Deserialize)]
pub struct BuildInfo {
    /// `true` (default) — generate the file when Git information is available.
    /// `false` — never generate the file.
    #[serde(default = "default_build_info_enabled")]
    pub enabled: bool,
}

fn default_build_info_enabled() -> bool {
    true
}

impl Default for BuildInfo {
    fn default() -> Self {
        BuildInfo { enabled: true }
    }
}

/// An additional Maven-compatible repository declared in `[[repositories]]`.
#[derive(Debug, Deserialize, Clone)]
pub struct RepositoryEntry {
    /// Unique identifier used when deps select this repo via `repository = "id"`.
    pub id: String,
    /// Human-readable display label.  Defaults to [`id`] when absent.
    pub name: Option<String>,
    pub url: String,
}

impl RepositoryEntry {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// One value in `[dependencies]` or `[test-dependencies]`.
///
/// Two shapes accepted, via serde's untagged enum:
///
/// ```toml
/// # Shorthand: the value is just the version string.
/// "com.example:foo" = "1.2.3"
///
/// # Detailed: include an explicit repository id.
/// "net.example:bar" = { version = "2.0.0", repository = "my-repo" }
/// ```
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum DependencyValue {
    /// `"key" = "1.0.0"` shorthand form.
    Version(String),
    /// `"key" = { version = "1.0.0", repository = "id" }` detailed form.
    Detailed(DependencyDetailed),
}

#[derive(Debug, Deserialize, Clone)]
pub struct DependencyDetailed {
    pub version: String,
    /// Id of the repository to fetch this artifact from (must match a
    /// `[[repositories]]` entry's `id`).  When absent, Maven Central is used.
    #[serde(default)]
    pub repository: Option<String>,
}

impl DependencyValue {
    /// Version string as the user wrote it.  `""` means "supply via a BOM".
    pub fn version(&self) -> &str {
        match self {
            DependencyValue::Version(v) => v,
            DependencyValue::Detailed(d) => &d.version,
        }
    }

    /// Repository id override, if present.
    pub fn repository(&self) -> Option<&str> {
        match self {
            DependencyValue::Version(_) => None,
            DependencyValue::Detailed(d) => d.repository.as_deref(),
        }
    }
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

    /// `(group:artifact, version)` pairs for production annotation
    /// processors, in the order the resolver wants: workspace-inherited
    /// first, then member-declared.  On a collision (same coordinate
    /// declared in both), the member-declared one wins — its entry is
    /// later in the returned Vec.
    pub fn ap_pairs(&self) -> Vec<(&str, &str)> {
        ap_pairs_merged(&self.inherited_annotation_processors, &self.annotation_processors)
    }

    /// Same as [`ap_pairs`] for `[test-annotation-processors]`.
    pub fn test_ap_pairs(&self) -> Vec<(&str, &str)> {
        ap_pairs_merged(
            &self.inherited_test_annotation_processors,
            &self.test_annotation_processors,
        )
    }

    /// `group:artifact` strings of AP entries marked
    /// `on-compile-classpath = true`.  These coordinates also need to be
    /// resolved (already done as part of `ap_pairs`) and added to javac's
    /// `-cp` so user code can reference their annotation types.
    ///
    /// Test entries are merged in too: a Lombok-style processor declared
    /// only in `[test-annotation-processors]` should be visible on test
    /// compile's `-cp`.
    pub fn ap_on_compile_classpath_coords(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for map in [&self.inherited_annotation_processors, &self.annotation_processors] {
            for (k, v) in map {
                if v.on_compile_classpath() {
                    out.push(k.as_str());
                }
            }
        }
        out
    }

    /// Same as [`ap_on_compile_classpath_coords`] but covers
    /// test-annotation-processors too.  Used by test compile.
    pub fn test_ap_on_compile_classpath_coords(&self) -> Vec<&str> {
        let mut out = self.ap_on_compile_classpath_coords();
        for map in [
            &self.inherited_test_annotation_processors,
            &self.test_annotation_processors,
        ] {
            for (k, v) in map {
                if v.on_compile_classpath() {
                    out.push(k.as_str());
                }
            }
        }
        out
    }

    /// Flatten the nested production-AP options into the `<prefix>.<key> = <value>`
    /// list javac wants on `-A`.  Inherited options come first; member
    /// entries override per (prefix, key).
    pub fn flat_ap_options(&self) -> Vec<(String, String)> {
        flatten_ap_options(
            &self.inherited_annotation_processor_options,
            &self.annotation_processor_options,
        )
    }

    /// Same as [`flat_ap_options`] for test-compile.  Test options layer
    /// on top of production options (a test-only override beats both).
    pub fn flat_test_ap_options(&self) -> Vec<(String, String)> {
        let mut merged = self.flat_ap_options();
        let test = flatten_ap_options(
            &self.inherited_test_annotation_processor_options,
            &self.test_annotation_processor_options,
        );
        // Test entries with the same `prefix.key` override production.
        for (k, v) in test {
            if let Some(existing) = merged.iter_mut().find(|(ek, _)| ek == &k) {
                existing.1 = v;
            } else {
                merged.push((k, v));
            }
        }
        merged
    }
}

/// Concatenate two AP maps in inherited-then-own order.  When the same
/// coordinate appears in both, the own-map entry is emitted (the
/// inherited one is dropped) so callers see exactly one resolve target.
fn ap_pairs_merged<'a>(
    inherited: &'a BTreeMap<String, AnnotationProcessor>,
    own: &'a BTreeMap<String, AnnotationProcessor>,
) -> Vec<(&'a str, &'a str)> {
    let mut out: Vec<(&'a str, &'a str)> = Vec::with_capacity(inherited.len() + own.len());
    for (k, v) in inherited {
        if !own.contains_key(k) {
            out.push((k.as_str(), v.version()));
        }
    }
    for (k, v) in own {
        out.push((k.as_str(), v.version()));
    }
    out
}

/// Two-pass merge of nested option tables, then flatten to
/// `("prefix.key", "value")` pairs ready for `-A`.
fn flatten_ap_options(
    inherited: &BTreeMap<String, BTreeMap<String, String>>,
    own: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, BTreeMap<String, String>> = inherited.clone();
    for (prefix, inner) in own {
        let dst = merged.entry(prefix.clone()).or_default();
        for (k, v) in inner {
            dst.insert(k.clone(), v.clone());
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (prefix, inner) in &merged {
        for (k, v) in inner {
            out.push((format!("{}.{}", prefix, k), v.clone()));
        }
    }
    out
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
        test: parsed.test,
        kotlin: parsed.kotlin,
        docker,
        build_info: parsed.build_info,
        dependencies: parsed.dependencies,
        test_dependencies: parsed.test_dependencies,
        repositories: parsed.repositories,
        bom_imports: parsed.bom_imports,
        test_bom_imports: parsed.test_bom_imports,
        inherited_bom_imports: BTreeMap::new(),
        inherited_test_bom_imports: BTreeMap::new(),
        workspace_dependencies: parsed.workspace_dependencies,
        annotation_processors: parsed.annotation_processors,
        test_annotation_processors: parsed.test_annotation_processors,
        inherited_annotation_processors: BTreeMap::new(),
        inherited_test_annotation_processors: BTreeMap::new(),
        annotation_processor_options: parsed.annotation_processor_options,
        test_annotation_processor_options: parsed.test_annotation_processor_options,
        inherited_annotation_processor_options: BTreeMap::new(),
        inherited_test_annotation_processor_options: BTreeMap::new(),
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

    validate_dep_repo_refs(&descriptor)?;

    Ok(descriptor)
}

/// Validate that every `repository = "id"` reference in `[dependencies]` and
/// `[test-dependencies]` names a repository declared in `[[repositories]]`.
///
/// Called once at the end of single-module [`load`] and again after workspace
/// inheritance so workspace-level repos are visible.
pub fn validate_dep_repo_refs(desc: &Descriptor) -> Result<()> {
    let known_ids: std::collections::HashSet<&str> =
        desc.repositories.iter().map(|r| r.id.as_str()).collect();

    for (coord, dep) in &desc.dependencies {
        if let Some(repo_id) = dep.repository() {
            if !known_ids.contains(repo_id) {
                bail!(
                    "dependency \"{}\" references unknown repository \"{}\"; \
                     declare it with [[repositories]]",
                    coord, repo_id
                );
            }
        }
    }
    for (coord, dep) in &desc.test_dependencies {
        if let Some(repo_id) = dep.repository() {
            if !known_ids.contains(repo_id) {
                bail!(
                    "test-dependency \"{}\" references unknown repository \"{}\"; \
                     declare it with [[repositories]]",
                    coord, repo_id
                );
            }
        }
    }
    Ok(())
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
id = "nexus"
url = "https://example.com/m2"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.java.effective(), "17");
        assert_eq!(d.repositories.len(), 1);
        assert_eq!(d.repositories[0].id, "nexus");
    }

    #[test]
    fn empty_descriptor_is_rejected() {
        let err = load_str("").unwrap_err().to_string();
        assert!(err.contains("must contain one of"), "got: {err}");
    }

    // -- build-info ----------------------------------------------------------

    #[test]
    fn build_info_enabled_by_default() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.build_info.enabled, "build-info must be enabled by default");
    }

    #[test]
    fn build_info_can_be_disabled() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[build-info]
enabled = false
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.build_info.enabled);
    }

    #[test]
    fn build_info_explicitly_enabled() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[build-info]
enabled = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.build_info.enabled);
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

    // -- annotation-processors ----------------------------------------------

    #[test]
    fn parse_annotation_processors_both_forms() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processors]
"com.google.dagger:dagger-compiler" = "2.50"
"org.projectlombok:lombok" = { version = "1.18.30", on-compile-classpath = true }
"#;
        let d = load_str(toml).unwrap();
        let dagger = d.annotation_processors.get("com.google.dagger:dagger-compiler").unwrap();
        assert_eq!(dagger.version(), "2.50");
        assert!(!dagger.on_compile_classpath());

        let lombok = d.annotation_processors.get("org.projectlombok:lombok").unwrap();
        assert_eq!(lombok.version(), "1.18.30");
        assert!(lombok.on_compile_classpath());
    }

    #[test]
    fn ap_pairs_returns_inherited_then_own() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processors]
"own:proc" = "2.0"
"#;
        let mut d = load_str(toml).unwrap();
        // Simulate inheritance — what workspace::inherit_from_workspace
        // would do at member-load time.
        d.inherited_annotation_processors.insert(
            "ws:proc".into(),
            AnnotationProcessor::Version("1.0".into()),
        );
        let pairs = d.ap_pairs();
        assert_eq!(
            pairs,
            vec![("ws:proc", "1.0"), ("own:proc", "2.0")],
            "inherited entries should come first so own can override on collision",
        );
    }

    #[test]
    fn ap_pairs_own_overrides_inherited_on_same_coord() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processors]
"shared:proc" = "2.0"
"#;
        let mut d = load_str(toml).unwrap();
        d.inherited_annotation_processors.insert(
            "shared:proc".into(),
            AnnotationProcessor::Version("1.0".into()),
        );
        let pairs = d.ap_pairs();
        // Inherited entry is dropped because member redeclared it.
        assert_eq!(pairs, vec![("shared:proc", "2.0")]);
    }

    #[test]
    fn test_ap_pairs_uses_test_table_only() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processors]
"prod:proc" = "1.0"

[test-annotation-processors]
"test:proc" = "2.0"
"#;
        let d = load_str(toml).unwrap();
        // Prod path: only "prod:proc"
        assert_eq!(d.ap_pairs(), vec![("prod:proc", "1.0")]);
        // Test path: only "test:proc" (test_ap_pairs is just the test table;
        // compile.rs/test.rs concatenates the two when invoking javac).
        assert_eq!(d.test_ap_pairs(), vec![("test:proc", "2.0")]);
    }

    #[test]
    fn on_compile_classpath_coords_listed() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processors]
"org.projectlombok:lombok" = { version = "1.18.30", on-compile-classpath = true }
"com.google.dagger:dagger-compiler" = "2.50"
"#;
        let d = load_str(toml).unwrap();
        let on_cp = d.ap_on_compile_classpath_coords();
        assert_eq!(on_cp, vec!["org.projectlombok:lombok"]);
    }

    // -- annotation-processor-options (nested form) ------------------------

    #[test]
    fn parse_nested_ap_options_emits_dotted_flags() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processor-options.dagger]
fastInit = "enabled"
formatGeneratedSource = "disabled"

[annotation-processor-options.mapstruct]
suppressGeneratorTimestamp = "true"
"#;
        let d = load_str(toml).unwrap();
        let flat = d.flat_ap_options();
        // BTreeMap iteration is sorted, so flat is stable.
        assert_eq!(
            flat,
            vec![
                ("dagger.fastInit".to_string(), "enabled".to_string()),
                ("dagger.formatGeneratedSource".to_string(), "disabled".to_string()),
                ("mapstruct.suppressGeneratorTimestamp".to_string(), "true".to_string()),
            ],
        );
    }

    #[test]
    fn ap_options_inheritance_member_overrides_per_key() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processor-options.dagger]
fastInit = "enabled"
"#;
        let mut d = load_str(toml).unwrap();
        // Simulate workspace-inherited options: a different `fastInit`
        // value PLUS a sibling key the member doesn't redeclare.
        let mut ws_dagger = BTreeMap::new();
        ws_dagger.insert("fastInit".to_string(), "disabled".to_string());
        ws_dagger.insert("formatGeneratedSource".to_string(), "disabled".to_string());
        d.inherited_annotation_processor_options.insert("dagger".to_string(), ws_dagger);

        let flat = d.flat_ap_options();
        // Member's `fastInit = enabled` wins over workspace's `disabled`.
        // Workspace's `formatGeneratedSource = disabled` survives because
        // the member didn't redeclare it.
        assert_eq!(
            flat,
            vec![
                ("dagger.fastInit".to_string(), "enabled".to_string()),
                ("dagger.formatGeneratedSource".to_string(), "disabled".to_string()),
            ],
        );
    }

    #[test]
    fn flat_test_ap_options_layers_test_on_top_of_prod() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
mainClass = "X"

[annotation-processor-options.dagger]
fastInit = "enabled"

[test-annotation-processor-options.dagger]
fastInit = "disabled"
"#;
        let d = load_str(toml).unwrap();
        // Production path: just fastInit=enabled.
        assert_eq!(
            d.flat_ap_options(),
            vec![("dagger.fastInit".to_string(), "enabled".to_string())],
        );
        // Test path: production options layered first, then test ones
        // override per (prefix, key).
        assert_eq!(
            d.flat_test_ap_options(),
            vec![("dagger.fastInit".to_string(), "disabled".to_string())],
        );
    }

    // -- [test] / [kotlin] tool version configuration -----------------------

    #[test]
    fn workspace_may_declare_test_and_kotlin_versions() {
        let toml = r#"
[workspace]
members = ["a"]

[test]
junitPlatformVersion = "6.0.3"

[kotlin]
version = "2.1.21"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.is_workspace());
        assert_eq!(d.test.junit_platform_version(), "6.0.3");
        assert_eq!(d.kotlin.version(), "2.1.21");
    }

    #[test]
    fn test_and_kotlin_versions_inherit_from_workspace_when_omitted() {
        // Member has no [test] or [kotlin] — must pick up workspace values.
        let toml = r#"
[workspace]
members = ["member"]

[test]
junitPlatformVersion = "6.1.0"

[kotlin]
version = "2.2.0"
"#;
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path();
        std::fs::write(ws_path.join("Curie.toml"), toml).unwrap();
        std::fs::create_dir(ws_path.join("member")).unwrap();
        let member_toml = r#"
[application]
name = "member"
version = "0.0.0"
mainClass = "M"
"#;
        std::fs::write(ws_path.join("member").join("Curie.toml"), member_toml).unwrap();

        // Use the real workspace loading path (not the single-file load_str)
        // so inherit_from_workspace runs.
        let ws = crate::workspace::load(ws_path).unwrap();
        let member_desc = &ws.members[0].descriptor;
        assert_eq!(member_desc.test.junit_platform_version(), "6.1.0");
        assert_eq!(member_desc.kotlin.version(), "2.2.0");
    }

    #[test]
    fn member_version_overrides_workspace_version() {
        let toml = r#"
[workspace]
members = ["m"]

[test]
junitPlatformVersion = "6.0.3"

[kotlin]
version = "2.1.21"
"#;
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path();
        std::fs::write(ws_path.join("Curie.toml"), toml).unwrap();
        std::fs::create_dir(ws_path.join("m")).unwrap();
        let member_toml = r#"
[application]
name = "m"
version = "0.0.0"
mainClass = "M"

[test]
junitPlatformVersion = "6.5.0"

[kotlin]
version = "1.9.25"
"#;
        std::fs::write(ws_path.join("m").join("Curie.toml"), member_toml).unwrap();

        let ws = crate::workspace::load(ws_path).unwrap();
        let m = &ws.members[0].descriptor;
        assert_eq!(m.test.junit_platform_version(), "6.5.0");
        assert_eq!(m.kotlin.version(), "1.9.25");
    }

    #[test]
    fn tool_versions_fall_back_to_defaults_when_absent() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.test.junit_platform_version(), crate::descriptor::DEFAULT_JUNIT_PLATFORM_VERSION);
        assert_eq!(d.kotlin.version(), crate::descriptor::DEFAULT_KOTLIN_VERSION);
    }

    // -- DependencyValue / RepositoryEntry ----------------------------------------

    #[test]
    fn parse_dependency_shorthand_form() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = "1.2.3"
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:foo").unwrap();
        assert_eq!(v.version(), "1.2.3");
        assert_eq!(v.repository(), None);
    }

    #[test]
    fn parse_dependency_detailed_form_without_repo() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = { version = "2.0.0" }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:foo").unwrap();
        assert_eq!(v.version(), "2.0.0");
        assert_eq!(v.repository(), None);
    }

    #[test]
    fn parse_dependency_detailed_form_with_repo() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[[repositories]]
id = "my-repo"
url = "https://repo.example.com/m2"
[dependencies]
"com.example:bar" = { version = "3.0.0", repository = "my-repo" }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:bar").unwrap();
        assert_eq!(v.version(), "3.0.0");
        assert_eq!(v.repository(), Some("my-repo"));
    }

    #[test]
    fn dep_with_unknown_repo_id_is_rejected() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = { version = "1.0", repository = "does-not-exist" }
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("does-not-exist"), "expected unknown-repo error, got: {err}");
        assert!(err.contains("[[repositories]]"), "should hint about [[repositories]], got: {err}");
    }

    #[test]
    fn dep_with_known_repo_id_is_accepted() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[[repositories]]
id = "shibboleth"
url = "https://build.shibboleth.net/nexus/content/repositories/releases/"
[dependencies]
"net.shibboleth.oidc:oidc-common-crypto-api" = { version = "3.3.0", repository = "shibboleth" }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("net.shibboleth.oidc:oidc-common-crypto-api").unwrap();
        assert_eq!(v.version(), "3.3.0");
        assert_eq!(v.repository(), Some("shibboleth"));
    }

    #[test]
    fn test_dep_with_unknown_repo_id_is_rejected() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[test-dependencies]
"com.example:foo" = { version = "1.0", repository = "ghost" }
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("ghost"), "expected unknown-repo error, got: {err}");
    }

    #[test]
    fn repository_entry_display_name_defaults_to_id() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[[repositories]]
id = "shibboleth"
url = "https://example.com"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.repositories[0].display_name(), "shibboleth");
    }

    #[test]
    fn repository_entry_display_name_uses_name_when_set() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[[repositories]]
id = "shibboleth"
name = "Shibboleth Releases"
url = "https://example.com"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.repositories[0].id, "shibboleth");
        assert_eq!(d.repositories[0].display_name(), "Shibboleth Releases");
    }
}
