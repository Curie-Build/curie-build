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
    pub groovy: Groovy,
    /// Populated from `[spock]`.  `enabled` is set by [`load`] after a
    /// raw-TOML presence check — absent section = Spock disabled.
    pub spock: Spock,
    /// Populated from `[native-image]`.  The `section_present` flag is set
    /// by [`load`] after a raw-TOML presence check, because an absent section
    /// and a section with all-default values produce the same deserialised
    /// struct — but only the former disables native-image compilation.
    pub native_image: NativeImage,
    pub docker: Docker,
    pub build_info: BuildInfo,
    pub fat_jar: FatJar,
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
    /// `[publish]` — empty/default when the section is absent.  Validated at
    /// publish time, not load time.
    pub publish: PublishConfig,
    /// `[plugin.<name>]` sections — each key activates a plugin binary
    /// named `curie-<key>` on PATH.  The value is the raw TOML tree for
    /// that plugin, passed verbatim as JSON to the plugin on stdin.
    pub plugins: BTreeMap<String, toml::Value>,
    /// Populated from `[maven]`.  Controls `curie maven sync` /
    /// `curie build`'s automatic Maven configuration sync.
    pub maven: MavenConfig,
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
    /// Workspace root: lists member directories but is not itself buildable.
    Workspace(WorkspaceSection),
    /// BOM (Bill of Materials): publishes a POM-only artifact that declares
    /// managed versions for a set of dependencies.  No JAR is produced.
    Bom(Bom),
}

/// Flat shape for serde — every section is `Option`, and [`load`]
/// validates exactly-one-of and converts to [`DescriptorKind`].  Kept
/// private to descriptor.rs; consumers only see the validated
/// [`Descriptor`].
#[derive(Debug, Deserialize)]
struct RawDescriptor {
    application: Option<Application>,
    library: Option<Library>,
    workspace: Option<WorkspaceSection>,
    bom: Option<Bom>,
    #[serde(default)]
    java: Java,
    #[serde(default)]
    docker: Docker,
    #[serde(rename = "build-info", default)]
    build_info: BuildInfo,
    #[serde(rename = "fat-jar", default)]
    fat_jar: FatJar,
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
    #[serde(default)]
    groovy: Groovy,
    #[serde(default)]
    spock: Spock,
    #[serde(rename = "native-image", default)]
    native_image: NativeImage,
    #[serde(default)]
    publish: PublishConfig,
    #[serde(default, rename = "plugin")]
    plugin: BTreeMap<String, toml::Value>,
    #[serde(default)]
    maven: MavenConfig,
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
    /// Maven `groupId` — required only when publishing.  When absent, build
    /// and test paths work normally; `curie publish` errors with a clear
    /// message asking the user to add it.
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
    /// The fully-qualified main class name.  When omitted, curie will scan
    /// production sources and compiled bytecode to detect it automatically.
    #[serde(rename = "mainClass")]
    pub main_class: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Library {
    pub name: String,
    pub version: String,
    /// Maven `groupId` — required only when publishing.  See [`Application::group_id`].
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
}

/// Workspace descriptor section: lists member directories whose own `Curie.toml`
/// files are buildable modules.  Member paths are relative to the workspace
/// `Curie.toml` directory.
#[derive(Debug, Deserialize)]
pub struct WorkspaceSection {
    pub members: Vec<String>,
}

/// BOM (Bill of Materials) descriptor: declares managed dependency versions
/// that consumers can import via `[bom-imports]`.  Produces a POM-only
/// artifact; no JAR, no compilation.
#[derive(Debug, Deserialize)]
pub struct Bom {
    pub name: String,
    pub version: String,
    /// Maven `groupId` — required for publishing.  See [`Application::group_id`].
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Java {
    /// `[java].sourceCompatibility` as the user wrote it, or `None` when
    /// the key was absent.  Use [`Self::effective`] to get the resolved
    /// value — never read this field directly from compile/test paths,
    /// because `None` is meaningful: "inherit from the workspace if any,
    /// else omit `--release` and target the running JDK".
    #[serde(rename = "sourceCompatibility")]
    pub source_compatibility: Option<String>,
    /// When `true`, passes `--enable-preview` to javac and to the java
    /// runtime.  Required for preview features on Java 21–22 (e.g. unnamed
    /// classes and instance main methods).  Not needed on Java 23+ where
    /// those features became standard (JEP 463).
    ///
    /// ```toml
    /// [java]
    /// sourceCompatibility = "21"
    /// enablePreview = true
    /// ```
    /// `None` when `enablePreview` was absent — meaningful, so it can be
    /// distinguished from an explicit `false`: a workspace member that omits
    /// the key inherits the workspace value, whereas `enablePreview = false`
    /// opts out even when the workspace enabled it.  Use
    /// [`Self::preview_enabled`] to read the resolved boolean.
    #[serde(rename = "enablePreview")]
    pub enable_preview: Option<bool>,
}

impl Java {
    /// Resolved `--release` argument for `javac`, or `None` when
    /// `sourceCompatibility` was not set (at this level or any enclosing
    /// workspace).  When `None`, callers must omit `--release` so javac
    /// targets the running JDK's own version.
    ///
    /// Workspace inheritance has already been applied by the time the build
    /// pipeline reads this: if a workspace root sets `sourceCompatibility =
    /// "21"` and a member omits it, the member will have `Some("21")` here.
    pub fn effective(&self) -> Option<&str> {
        self.source_compatibility.as_deref()
    }

    /// Resolved `--enable-preview` flag (default `false`).  Like
    /// [`Self::effective`], member/workspace inheritance has already been
    /// applied by the time the build pipeline reads this.
    pub fn preview_enabled(&self) -> bool {
        self.enable_preview.unwrap_or(false)
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
    /// When `true`, test runs collect code coverage via JaCoCo and produce
    /// a report under `target/coverage/`.  Can be enabled permanently in
    /// `Curie.toml` or per-invocation with `curie test --coverage`.
    ///
    /// ```toml
    /// [test]
    /// coverage = true
    /// ```
    #[serde(default)]
    pub coverage: Option<bool>,
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

    /// `true` when the user explicitly set `junitPlatformVersion` in
    /// `Curie.toml` (or inherited it from a workspace).  Used by the test
    /// runner to decide whether to override the version for Spock compatibility.
    pub fn junit_platform_version_is_user_set(&self) -> bool {
        self.junit_platform_version.is_some()
    }

    /// Resolved coverage flag (default `false`).  `true` when the user set
    /// `coverage = true` in `[test]` (or inherited it from a workspace).
    pub fn coverage_enabled(&self) -> bool {
        self.coverage.unwrap_or(false)
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

/// Default version of Apache Groovy resolved from Maven Central when `.groovy`
/// sources are present.  Override (workspace-inheritable) with:
///
/// ```toml
/// [groovy]
/// version = "4.0.23"
/// ```
pub const DEFAULT_GROOVY_VERSION: &str = "5.0.6";

/// Configuration for the `[native-image]` table.
///
/// Native image compilation is opt-in: the section must be explicitly present
/// in `Curie.toml` to trigger `native-image` after JAR packaging.  Only
/// meaningful for `[application]` projects.
///
/// ```toml
/// [native-image]
/// # Name of the output binary written to target/ (default: application.name)
/// outputName = "my-app"
///
/// # Path to a directory containing GraalVM reachability-metadata config files
/// # (reflect-config.json, resource-config.json, proxy-config.json, …).
/// # Passed as -H:ConfigurationFileDirectories=<path>.
/// configDir = "src/main/resources/META-INF/native-image"
///
/// # Additional flags forwarded verbatim to native-image (appended last).
/// extraArgs = ["--no-fallback", "-H:+ReportExceptionStackTraces"]
/// ```
///
/// Curie locates the `native-image` executable by checking, in order:
///   1. `$GRAALVM_HOME/bin/native-image`
///   2. `native-image` on `$PATH`
///
/// Install GraalVM from <https://www.graalvm.org/downloads/> or via sdkman.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct NativeImage {
    /// Name of the output binary written to `target/`.
    /// Defaults to the application name (hyphens replaced with hyphens — kept
    /// as-is since native-image accepts hyphens in output names).
    #[serde(rename = "outputName", default)]
    pub output_name: Option<String>,

    /// Path to a directory that contains GraalVM reachability-metadata JSON
    /// files (relative to the project root).  Passed as
    /// `-H:ConfigurationFileDirectories=<abs-path>`.
    #[serde(rename = "configDir", default)]
    pub config_dir: Option<String>,

    /// Extra flags appended verbatim to the `native-image` invocation.
    #[serde(rename = "extraArgs", default)]
    pub extra_args: Vec<String>,

    /// Whether the `[native-image]` section was explicitly present in
    /// `Curie.toml`.  Set by [`load`] after the raw-TOML presence check;
    /// never written by serde.
    #[serde(skip)]
    pub section_present: bool,
}

impl NativeImage {
    /// Resolved output binary name: descriptor override or application name.
    /// `app_name` is the fallback when `outputName` was omitted.
    pub fn resolved_output_name<'a>(&'a self, app_name: &'a str) -> &'a str {
        self.output_name.as_deref().unwrap_or(app_name)
    }
}

/// Configuration for the `[fat-jar]` table.
///
/// When present, Curie produces an uber/fat JAR that merges all dependency
/// classes into a single JAR.
///
/// The `shadeAll` key controls the default policy for whether (direct)
/// dependencies are shaded into the fat JAR:
/// - `shadeAll = true` (default when the section is present) — dependencies
///   are included unless a per-dep `shade = false` or `fatJar = false` (legacy)
///   says otherwise.
/// - `shadeAll = false` — dependencies are excluded unless a per-dep
///   `shade = true` (or `relocations` on the dep, which forces shading) opts in.
///
/// Per-dependency `shade` (or the legacy `fatJar`) and `relocations` act as
/// overrides and can force inclusion even when `shadeAll = false`.
///
/// ```toml
/// [fat-jar]
/// # Enable fat JAR output (default when the section is present).
/// enabled = true
///
/// # Global default: shade (bundle) all dependencies unless overridden per-dep.
/// shadeAll = true
///
/// # Relocate packages to avoid classpath conflicts.
/// [[fat-jar.relocations]]
/// from = "com.google.common"
/// to = "shaded.com.google.common"
/// ```
///
/// Only meaningful for `[application]` and `[library]` projects.
#[derive(Debug, Deserialize, Clone)]
pub struct FatJar {
    /// `true` when fat-JAR packaging is active.  Defaults to `true` when
    /// the `[fat-jar]` section is present, `false` when it is absent.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Global default policy for shading dependencies into the fat JAR.
    /// `true` (default) means dependencies are shaded unless a per-dep
    /// `shade = false` (or legacy `fatJar = false`) excludes them.
    /// `false` means dependencies are excluded unless a per-dep `shade = true`
    /// or `relocations` forces them in.
    #[serde(rename = "shadeAll", default = "default_true")]
    pub shade_all: bool,

    /// Package relocations applied to dependency classes.  Each entry
    /// rewrites every reference to `from` → `to` inside class files and
    /// resource paths (global rules; per-dep rules may also be declared on
    /// individual `[dependencies]` entries).
    #[serde(default)]
    pub relocations: Vec<Relocation>,

    /// Whether the `[fat-jar]` section was explicitly present in
    /// `Curie.toml`.  Set by [`load`]; never written by serde.
    #[serde(skip)]
    pub section_present: bool,
}

impl Default for FatJar {
    fn default() -> Self {
        FatJar {
            enabled: false,
            shade_all: true, // historical "include all unless per-dep says no"
            relocations: vec![],
            section_present: false,
        }
    }
}

/// One relocation entry (used both in `[[fat-jar.relocations]]` and in the
/// `relocations` array on a per-dependency entry).
///
/// Rewrites a package prefix in both class bytecode (constant pool) and
/// resource paths. When declared on a specific dependency the rule applies
/// only to classes/resources originating from that dependency (after an
/// overlap safety check).
///
/// ```toml
/// [[fat-jar.relocations]]
/// from = "com.google.common"
/// to = "shaded.com.google.common"
/// excludes = ["com.google.common.annotations.*"]
///
/// # Or per-dependency:
/// "com.google.guava:guava" = { version = "33", relocations = [
///   { from = "com.google.common", to = "com.example.shaded.com.google.common" }
/// ] }
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct Relocation {
    /// The original package prefix to match (dot-separated).
    #[serde(rename = "from")]
    pub from: String,
    /// The replacement package prefix.
    #[serde(rename = "to")]
    pub to: String,
    /// Patterns to exclude from relocation (glob syntax, optional).
    #[serde(default)]
    pub excludes: Vec<String>,
}

/// Configuration for the `[groovy]` table.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Groovy {
    #[serde(default)]
    pub version: Option<String>,
}

impl Groovy {
    /// Effective version passed to the resolver for the Groovy compiler and
    /// runtime JARs (`org.apache.groovy:groovy:VERSION`).
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or(DEFAULT_GROOVY_VERSION)
    }
}

/// Default `spock-core` version resolved from Maven Central when `[spock]`
/// is present.  The version string includes a Groovy compatibility suffix
/// (e.g. `groovy-4.0`).  Override with:
///
/// ```toml
/// [spock]
/// version = "2.3-groovy-4.0"
/// ```
pub const DEFAULT_SPOCK_VERSION: &str = "2.4-groovy-5.0";

/// Configuration for the `[spock]` table.  The section's mere presence
/// (even with no keys) activates Spock support — `section_present` is set
/// by [`load`] from the raw TOML.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Spock {
    #[serde(default)]
    pub version: Option<String>,
    /// Explicit `enabled = true/false` from `[spock]`.  `None` when the key
    /// was absent → fall back to section presence.  Lets a workspace member
    /// write `enabled = false` to opt out of Spock that the workspace enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `true` when the `[spock]` section appeared in `Curie.toml`.  Set by
    /// [`load`]; never written by serde.
    #[serde(skip)]
    pub section_present: bool,
}

impl Spock {
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or(DEFAULT_SPOCK_VERSION)
    }

    /// Resolved enabled state: an explicit `enabled` key wins, otherwise the
    /// mere presence of the `[spock]` section activates Spock.
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(self.section_present)
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

/// Configuration for the `[maven]` table — controls `curie maven sync`'s
/// behaviour and `curie build`'s automatic Maven configuration sync.
///
/// ```toml
/// [maven]
/// # Regenerate pom.xml automatically at the start of every `curie build`.
/// sync = true
///
/// # Escape hatch: pin the fully-resolved transitive dependency closure into
/// # <dependencyManagement> so Maven's version mediation cannot diverge from
/// # Curie's resolver. Default: false — Curie's resolver is intended to
/// # match Maven's algorithm exactly.
/// pinTransitive = false
/// ```
#[derive(Debug, Deserialize, Default, Clone)]
pub struct MavenConfig {
    /// `true` — `curie build` regenerates `pom.xml` (and, in a workspace,
    /// the aggregator POM) before compiling.  `None`/absent means disabled;
    /// workspace-inheritable like `[test]`/`[kotlin]`.
    #[serde(default)]
    pub sync: Option<bool>,
    /// `true` — emit the fully-resolved transitive dependency closure into
    /// `<dependencyManagement>` so Maven's mediation cannot diverge from
    /// Curie's resolver.  `None`/absent behaves as `false`.
    #[serde(rename = "pinTransitive", default)]
    pub pin_transitive: Option<bool>,
}

impl MavenConfig {
    /// Resolved `sync` flag (default `false`).
    pub fn sync_enabled(&self) -> bool {
        self.sync.unwrap_or(false)
    }

    /// Resolved `pinTransitive` flag (default `false`).
    pub fn pin_transitive_enabled(&self) -> bool {
        self.pin_transitive.unwrap_or(false)
    }
}

/// `[publish]` — settings for `curie publish`.
///
/// All POM-metadata fields are optional in the type so that descriptors
/// without a `[publish]` section parse fine; they are validated at publish
/// time by `publish::validate_for_publish`.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct PublishConfig {
    /// Named repository id from `[[repositories]]` to publish to.
    /// Mutually exclusive with [`url`].
    pub repository: Option<String>,
    /// Inline target URL.  Mutually exclusive with [`repository`].
    pub url: Option<String>,

    /// Default: GPG-sign every artifact.  Maven Central requires this.
    #[serde(default = "default_true")]
    pub sign: bool,
    /// Default: build a javadoc jar.  Maven Central requires this.
    #[serde(default = "default_true")]
    pub javadoc: bool,

    pub description: Option<String>,
    /// Project homepage for the POM `<url>` element.  Named `homepage` to
    /// disambiguate from the `url` field above (which is the publish target).
    pub homepage: Option<String>,
    #[serde(default)]
    pub licenses: Vec<String>,
    #[serde(default)]
    pub developers: Vec<Developer>,
    pub scm: Option<Scm>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Developer {
    pub id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Scm {
    pub url: Option<String>,
    pub connection: Option<String>,
    #[serde(rename = "developerConnection")]
    pub developer_connection: Option<String>,
}

fn default_true() -> bool {
    true
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
/// # Detailed: extra knobs.
/// "net.example:bar" = { version = "2.0.0", repository = "my-repo" }
///
/// # Mark as a Java agent — Curie adds -javaagent:<jar> to the JVM at runtime.
/// "org.mockito:mockito-core" = { version = "", javaAgent = true }
/// ```
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum DependencyValue {
    /// `"key" = "1.0.0"` shorthand form.
    Version(String),
    /// `"key" = { version = "1.0.0", ... }` detailed form.
    Detailed(DependencyDetailed),
}

#[derive(Debug, Deserialize, Clone)]
pub struct DependencyDetailed {
    pub version: String,
    /// Id of the repository to fetch this artifact from (must match a
    /// `[[repositories]]` entry's `id`).  When absent, Maven Central is used.
    #[serde(default)]
    pub repository: Option<String>,
    /// When `true`, the JAR is also passed as `-javaagent:<jar>` to the JVM
    /// that runs the tests (for `[test-dependencies]`) or the application (for
    /// `[dependencies]`).  Use this for Mockito on JDK 21+ and similar agents.
    #[serde(default, rename = "javaAgent")]
    pub java_agent: bool,
    /// Transitive dependencies to exclude.  Each entry is a
    /// `"group:artifact"` string.  These are propagated transitively:
    /// any transitive dependency matching an exclusion is omitted from
    /// the resolved closure.
    ///
    /// ```toml
    /// "org.apache.pdfbox:pdfbox" = { version = "3.0.7", exclusions = ["org.bouncycastle:bcprov-jdk18on"] }
    /// ```
    #[serde(default)]
    pub exclusions: Vec<String>,
    /// Per-dependency control for shading (including) this dependency into
    /// the fat/uber JAR.
    ///
    /// - `None` (absent) → follow the global `[fat-jar].shadeAll` default.
    /// - `Some(true)` / `shade = true` → always shade (include) this dep.
    /// - `Some(false)` / `shade = false` → never shade this dep.
    ///
    /// Declaring a non-empty `relocations` array on the dependency also
    /// forces shading (implies `shade = true`).
    ///
    /// ```toml
    /// "org.example:logging-api" = { version = "1.0", shade = false }
    /// "com.google.guava:guava" = { version = "33", shade = true, relocations = [
    ///   { from = "com.google.common", to = "com.example.shaded.com.google.common" }
    /// ]}
    /// ```
    #[serde(default, rename = "shade")]
    pub shade: Option<bool>,

    /// Per-dependency package relocations. When non-empty this dependency
    /// is always shaded into the fat JAR (the relocation rules are applied
    /// only to classes/resources coming from this dependency, after an
    /// overlap safety check against other bundled deps).
    ///
    /// ```toml
    /// "com.google.guava:guava" = { version = "33.2.1-jre", relocations = [
    ///   { from = "com.google.common", to = "com.example.fatjar.shaded.com.google.common" },
    ///   { from = "com.google.thirdparty", to = "com.example.fatjar.shaded.com.google.thirdparty" }
    /// ]}
    /// ```
    #[serde(default)]
    pub relocations: Vec<Relocation>,
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

    /// `true` when `javaAgent = true` is set in the detailed form.
    pub fn java_agent(&self) -> bool {
        match self {
            DependencyValue::Version(_) => false,
            DependencyValue::Detailed(d) => d.java_agent,
        }
    }

    /// Exclusion strings declared on this dependency.  Returns references
    /// to the original strings for zero-copy threading into `DepEntry`.
    pub fn exclusions(&self) -> Vec<&str> {
        match self {
            DependencyValue::Version(_) => vec![],
            DependencyValue::Detailed(d) => d.exclusions.iter().map(|s| s.as_str()).collect(),
        }
    }

    /// Per-dependency fat-JAR include/exclude override (legacy name).
    /// Prefer `shade` for new code. `None` → follow the global `shadeAll` default.
    pub fn fat_jar_include(&self) -> Option<bool> {
        match self {
            DependencyValue::Version(_) => None,
            DependencyValue::Detailed(d) => d.shade, // the underlying storage is now `shade`
        }
    }

    /// Per-dependency shade override (`shade = true/false`).
    /// `None` means "follow the global `[fat-jar].shadeAll` default".
    pub fn shade(&self) -> Option<bool> {
        match self {
            DependencyValue::Version(_) => None,
            DependencyValue::Detailed(d) => d.shade,
        }
    }

    /// Per-dependency relocation rules (empty when none declared on this dep).
    pub fn relocations(&self) -> &[Relocation] {
        match self {
            DependencyValue::Version(_) => &[],
            DependencyValue::Detailed(d) => &d.relocations,
        }
    }

    /// Whether this dependency should be shaded (included) into the fat JAR,
    /// given the global `shadeAll` policy from `[fat-jar]`.
    ///
    /// Precedence (highest first):
    /// - Non-empty `relocations` on the dep → true (forces shading)
    /// - Explicit `shade = Some(b)` (or legacy `fatJar`) → b
    /// - Otherwise the `shade_all` global default
    pub fn should_shade(&self, shade_all: bool) -> bool {
        if !self.relocations().is_empty() {
            return true;
        }
        match self.shade() {
            Some(b) => b,
            None => shade_all,
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

    /// BOM projects produce a POM-only artifact with no JAR.
    pub fn is_bom(&self) -> bool {
        matches!(self.kind, DescriptorKind::Bom(_))
    }

    /// View the `[application]` section if this descriptor is one.
    pub fn application(&self) -> Option<&Application> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(a),
            _ => None,
        }
    }

    /// View the `[workspace]` section if this descriptor is a workspace root.
    pub fn workspace(&self) -> Option<&WorkspaceSection> {
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
            DescriptorKind::Bom(_) => "bom",
        }
    }

    /// Project name.  `None` for a workspace root, which has no name of
    /// its own — only its members do.
    pub fn project_name(&self) -> Option<&str> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(&a.name),
            DescriptorKind::Library(l) => Some(&l.name),
            DescriptorKind::Workspace(_) => None,
            DescriptorKind::Bom(b) => Some(&b.name),
        }
    }

    /// Maven `groupId`.  `None` for a workspace root and when the buildable
    /// section omitted the key.  `publish::validate_for_publish` errors on
    /// `None` for buildable projects.
    pub fn group_id(&self) -> Option<&str> {
        match &self.kind {
            DescriptorKind::Application(a) => a.group_id.as_deref(),
            DescriptorKind::Library(l) => l.group_id.as_deref(),
            DescriptorKind::Workspace(_) => None,
            DescriptorKind::Bom(b) => b.group_id.as_deref(),
        }
    }

    /// Project version.  `None` for a workspace root.
    pub fn project_version(&self) -> Option<&str> {
        match &self.kind {
            DescriptorKind::Application(a) => Some(&a.version),
            DescriptorKind::Library(l) => Some(&l.version),
            DescriptorKind::Workspace(_) => None,
            DescriptorKind::Bom(b) => Some(&b.version),
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

    /// Coordinates of production `[dependencies]` entries marked `javaAgent = true`.
    /// The returned strings are the TOML keys (`"group:artifact"`).
    pub fn dep_java_agent_coords(&self) -> Vec<&str> {
        self.dependencies
            .iter()
            .filter(|(_, v)| v.java_agent())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Coordinates of test-scoped deps marked `javaAgent = true`.
    /// Includes both production deps (agents visible on the test classpath too)
    /// and test-only deps from `[test-dependencies]`.
    pub fn test_dep_java_agent_coords(&self) -> Vec<&str> {
        let mut out = self.dep_java_agent_coords();
        out.extend(
            self.test_dependencies
                .iter()
                .filter(|(_, v)| v.java_agent())
                .map(|(k, _)| k.as_str()),
        );
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

    // Exactly one of [application] / [library] / [workspace] / [bom] — enforced
    // both as a count check (for the diagnostic message) and by reifying
    // the kind into the DescriptorKind enum.
    let kind = match (parsed.application, parsed.library, parsed.workspace, parsed.bom) {
        (Some(a), None, None, None) => DescriptorKind::Application(a),
        (None, Some(l), None, None) => DescriptorKind::Library(l),
        (None, None, Some(w), None) => DescriptorKind::Workspace(w),
        (None, None, None, Some(b)) => DescriptorKind::Bom(b),
        (None, None, None, None) => bail!(
            "Curie.toml must contain one of [application], [library], [workspace], or [bom]"
        ),
        _ => bail!(
            "Curie.toml must contain only one of [application], [library], [workspace], or [bom]"
        ),
    };

    let mut docker = parsed.docker;
    docker.section_present = docker_section_present;

    let native_image_section_present = table.map(|t| t.contains_key("native-image")).unwrap_or(false);
    let mut native_image = parsed.native_image;
    native_image.section_present = native_image_section_present;

    let fat_jar_section_present = table.map(|t| t.contains_key("fat-jar")).unwrap_or(false);
    let mut fat_jar = parsed.fat_jar;
    fat_jar.section_present = fat_jar_section_present;

    let spock_section_present = table.map(|t| t.contains_key("spock")).unwrap_or(false);
    let mut spock = parsed.spock;
    spock.section_present = spock_section_present;

    let descriptor = Descriptor {
        kind,
        java: parsed.java,
        test: parsed.test,
        kotlin: parsed.kotlin,
        groovy: parsed.groovy,
        spock,
        native_image,
        docker,
        build_info: parsed.build_info,
        fat_jar,
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
        publish: parsed.publish,
        plugins: parsed.plugin,
        maven: parsed.maven,
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

    if descriptor.is_library() && native_image_section_present {
        bail!(
            "library projects do not support native-image compilation: \
             remove the [native-image] section from Curie.toml"
        );
    }

    if descriptor.is_bom() {
        validate_bom_restrictions(&descriptor, docker_section_present, native_image_section_present,
            table.map(|t| t.contains_key("test")).unwrap_or(false),
            table.map(|t| t.contains_key("test-dependencies")).unwrap_or(false),
            table.map(|t| t.contains_key("test-bom-imports")).unwrap_or(false),
            table.map(|t| t.contains_key("annotation-processors")).unwrap_or(false),
            table.map(|t| t.contains_key("test-annotation-processors")).unwrap_or(false),
        )?;
    }

    validate_dep_repo_refs(&descriptor)?;

    Ok(descriptor)
}

/// Enforce restrictions that apply exclusively to BOM projects.
#[allow(clippy::too_many_arguments)]
fn validate_bom_restrictions(
    desc: &Descriptor,
    docker_present: bool,
    native_image_present: bool,
    test_present: bool,
    test_deps_present: bool,
    test_bom_imports_present: bool,
    annotation_processors_present: bool,
    test_annotation_processors_present: bool,
) -> Result<()> {
    if docker_present {
        bail!("BOM projects do not support Docker: remove the [docker] section from Curie.toml");
    }
    if native_image_present {
        bail!("BOM projects do not support native-image compilation: remove the [native-image] section from Curie.toml");
    }
    if test_present {
        bail!("BOM projects must not declare a [test] section");
    }
    if test_deps_present {
        bail!("BOM projects must not declare [test-dependencies]");
    }
    if test_bom_imports_present {
        bail!("BOM projects must not declare [test-bom-imports]");
    }
    if annotation_processors_present {
        bail!("BOM projects must not declare [annotation-processors]");
    }
    if test_annotation_processors_present {
        bail!("BOM projects must not declare [test-annotation-processors]");
    }
    for (coord, dep) in &desc.dependencies {
        if dep.version().is_empty() {
            bail!(
                "BOM dependency \"{}\" must have an explicit version; \
                 BOM-delegated versions (\"\") are not allowed in [bom] projects",
                coord
            );
        }
    }
    Ok(())
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

/// Native-image compilation is enabled when the `[native-image]` section is
/// explicitly present in `Curie.toml`.  Unlike Docker, there is no implicit
/// trigger (no Dockerfile analogue); the section must always be declared.
pub fn native_image_enabled(desc: &Descriptor) -> bool {
    desc.native_image.section_present
}

/// Fat-JAR packaging is enabled when the `[fat-jar]` section is present and
/// `enabled` is `true` (the default when the section is present).
pub fn fat_jar_enabled(desc: &Descriptor) -> bool {
    desc.fat_jar.section_present && desc.fat_jar.enabled
}

// ---------------------------------------------------------------------------
// Parse error formatting
// ---------------------------------------------------------------------------

fn format_parse_error(err: toml::de::Error, _source: &str, path: &Path) -> anyhow::Error {
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    let raw_display = err.to_string();
    let contextual = if let Some(rest) = raw_display.strip_prefix("TOML parse error at ") {
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

    let message = raw_display
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();

    let hint = hint_for(message, &file_name);

    let full = if let Some(h) = hint {
        format!("{}\n\n  hint: {}", contextual, h)
    } else {
        contextual
    };

    anyhow::anyhow!("{}", full)
}

fn hint_for(message: &str, _file_name: &str) -> Option<String> {
    if message.contains("missing field") && message.contains("name") {
        return Some(
            "[application], [library], and [bom] all require a `name` field.".to_string(),
        );
    }
    if message.contains("missing field") && message.contains("version") {
        return Some(
            "[application], [library], and [bom] all require a `version` field.".to_string(),
        );
    }
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
        assert_eq!(d.java.effective(), Some("17"));
        assert_eq!(d.repositories.len(), 1);
        assert_eq!(d.repositories[0].id, "nexus");
    }

    #[test]
    fn empty_descriptor_is_rejected() {
        let err = load_str("").unwrap_err().to_string();
        assert!(err.contains("must contain one of"), "got: {err}");
    }

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
        assert_eq!(d.ap_pairs(), vec![("prod:proc", "1.0")]);
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
        let mut ws_dagger = BTreeMap::new();
        ws_dagger.insert("fastInit".to_string(), "disabled".to_string());
        ws_dagger.insert("formatGeneratedSource".to_string(), "disabled".to_string());
        d.inherited_annotation_processor_options.insert("dagger".to_string(), ws_dagger);

        let flat = d.flat_ap_options();
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
        assert_eq!(
            d.flat_ap_options(),
            vec![("dagger.fastInit".to_string(), "enabled".to_string())],
        );
        assert_eq!(
            d.flat_test_ap_options(),
            vec![("dagger.fastInit".to_string(), "disabled".to_string())],
        );
    }

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
    fn parse_dependency_detailed_form_with_exclusions() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"org.apache.pdfbox:pdfbox" = { version = "3.0.7", exclusions = ["org.bouncycastle:bcprov-jdk18on", "org.bouncycastle:bcmail-jdk18on"] }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("org.apache.pdfbox:pdfbox").unwrap();
        assert_eq!(v.version(), "3.0.7");
        assert_eq!(v.exclusions(), vec!["org.bouncycastle:bcprov-jdk18on", "org.bouncycastle:bcmail-jdk18on"]);
    }

    #[test]
    fn parse_dependency_shorthand_has_no_exclusions() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = "1.2.3"
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:foo").unwrap();
        assert!(v.exclusions().is_empty());
    }

    #[test]
    fn parse_dependency_detailed_form_without_exclusions_has_empty_vec() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = { version = "2.0.0" }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:foo").unwrap();
        assert!(v.exclusions().is_empty());
    }

    #[test]
    fn parse_dependency_wildcard_exclusion() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
[dependencies]
"com.example:foo" = { version = "1.0", exclusions = ["*:*"] }
"#;
        let d = load_str(toml).unwrap();
        let v = d.dependencies.get("com.example:foo").unwrap();
        assert_eq!(v.exclusions(), vec!["*:*"]);
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

    #[test]
    fn spock_section_absent_is_disabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.spock.enabled(), "absent [spock] must leave enabled = false");
    }

    #[test]
    fn spock_section_present_but_enabled_false_is_disabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[spock]
enabled = false
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.spock.enabled(), "explicit enabled=false must override section presence");
    }

    #[test]
    fn spock_section_present_is_enabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[spock]
"#;
        let d = load_str(toml).unwrap();
        assert!(d.spock.enabled(), "[spock] present must set enabled = true");
        assert_eq!(d.spock.version(), crate::descriptor::DEFAULT_SPOCK_VERSION);
    }

    #[test]
    fn spock_version_can_be_set() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[spock]
version = "2.4-groovy-4.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.spock.enabled());
        assert_eq!(d.spock.version(), "2.4-groovy-4.0");
    }

    #[test]
    fn groovy_version_defaults_to_constant() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.groovy.version(), crate::descriptor::DEFAULT_GROOVY_VERSION);
    }

    #[test]
    fn groovy_version_can_be_set() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[groovy]
version = "3.0.22"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.groovy.version(), "3.0.22");
    }

    #[test]
    fn enable_preview_defaults_to_false() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.java.preview_enabled(), "enablePreview must default to false");
        assert!(d.java.enable_preview.is_none(), "absent key must stay None for inheritance");
    }

    #[test]
    fn enable_preview_explicit_false_is_distinguished_from_absent() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[java]
enablePreview = false
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.java.preview_enabled());
        assert_eq!(d.java.enable_preview, Some(false), "explicit false must be Some(false), not None");
    }

    #[test]
    fn enable_preview_can_be_set_true() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[java]
sourceCompatibility = "21"
enablePreview = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.java.preview_enabled());
        assert_eq!(d.java.effective(), Some("21"));
    }

    #[test]
    fn native_image_absent_means_disabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.native_image.section_present);
        assert!(!native_image_enabled(&d));
    }

    #[test]
    fn native_image_section_present_enables_it() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[native-image]
"#;
        let d = load_str(toml).unwrap();
        assert!(d.native_image.section_present);
        assert!(native_image_enabled(&d));
    }

    #[test]
    fn native_image_output_name_defaults_to_app_name() {
        let toml = r#"
[application]
name = "my-app"
version = "0.1"
mainClass = "X"

[native-image]
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.native_image.resolved_output_name("my-app"), "my-app");
    }

    #[test]
    fn native_image_output_name_override() {
        let toml = r#"
[application]
name = "my-app"
version = "0.1"
mainClass = "X"

[native-image]
outputName = "my-binary"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.native_image.output_name.as_deref(), Some("my-binary"));
        assert_eq!(d.native_image.resolved_output_name("my-app"), "my-binary");
    }

    #[test]
    fn native_image_config_dir_parsed() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[native-image]
configDir = "src/main/resources/META-INF/native-image"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(
            d.native_image.config_dir.as_deref(),
            Some("src/main/resources/META-INF/native-image")
        );
    }

    #[test]
    fn native_image_extra_args_parsed() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[native-image]
extraArgs = ["--no-fallback", "-H:+ReportExceptionStackTraces"]
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(
            d.native_image.extra_args,
            vec!["--no-fallback", "-H:+ReportExceptionStackTraces"]
        );
    }

    #[test]
    fn native_image_on_library_is_rejected() {
        let toml = r#"
[library]
name = "x"
version = "0.1"

[native-image]
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("library") && err.contains("native-image"), "got: {err}");
    }

    #[test]
    fn parse_bom_section() {
        let toml = r#"
[bom]
name = "my-platform"
version = "1.0.0"
groupId = "com.example"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.is_bom(), "should be recognised as a BOM project");
        assert_eq!(d.kind_label(), "bom");
        assert_eq!(d.project_name(), Some("my-platform"));
        assert_eq!(d.project_version(), Some("1.0.0"));
        assert_eq!(d.group_id(), Some("com.example"));
    }

    #[test]
    fn bom_with_docker_is_rejected() {
        let toml = r#"
[bom]
name = "x"
version = "0.1"
[docker]
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("BOM") && err.contains("docker"), "got: {err}");
    }

    #[test]
    fn bom_with_test_dependencies_is_rejected() {
        let toml = r#"
[bom]
name = "x"
version = "0.1"
[test-dependencies]
"com.example:foo" = "1.0"
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("BOM") && err.contains("test-dependencies"), "got: {err}");
    }

    #[test]
    fn bom_dep_without_explicit_version_is_rejected() {
        let toml = r#"
[bom]
name = "x"
version = "0.1"
[dependencies]
"com.example:foo" = ""
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("explicit version"), "got: {err}");
        assert!(err.contains("com.example:foo"), "got: {err}");
    }

    #[test]
    fn bom_with_two_sections_is_rejected() {
        let toml = r#"
[bom]
name = "x"
version = "0.1"
[library]
name = "y"
version = "0.1"
"#;
        let err = load_str(toml).unwrap_err().to_string();
        assert!(err.contains("only one"), "got: {err}");
    }

    #[test]
    fn bom_with_explicit_deps_is_accepted() {
        let toml = r#"
[bom]
name = "my-platform"
version = "1.0.0"
groupId = "com.example"
[dependencies]
"com.google.guava:guava" = "33.0.0-jre"
[bom-imports]
"io.micronaut:micronaut-bom" = "4.3.2"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.is_bom());
        assert_eq!(d.dependencies.len(), 1);
        assert_eq!(d.bom_imports.len(), 1);
    }

    // ── plugin section ────────────────────────────────────────────────────

    #[test]
    fn plugin_section_absent_gives_empty_map() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(d.plugins.is_empty());
    }

    #[test]
    fn plugin_simple_section_parsed() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[plugin.protobuf]
version   = "3.25.0"
sourceDir = "proto"
"#;
        let d = load_str(toml).unwrap();
        assert_eq!(d.plugins.len(), 1);
        let proto = d.plugins.get("protobuf").expect("protobuf plugin present");
        let table = proto.as_table().expect("protobuf config is a table");
        assert_eq!(
            table.get("version").and_then(|v| v.as_str()),
            Some("3.25.0")
        );
    }

    #[test]
    fn plugin_nested_table_preserved() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[plugin.foo.bar]
key = "value"
"#;
        let d = load_str(toml).unwrap();
        let foo = d.plugins.get("foo").expect("foo plugin present");
        let bar = foo
            .get("bar")
            .and_then(|v| v.as_table())
            .expect("nested bar table");
        assert_eq!(bar.get("key").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn plugin_array_of_tables_preserved() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[[plugin.foo.items]]
path = "vendor/a"

[[plugin.foo.items]]
path = "vendor/b"
"#;
        let d = load_str(toml).unwrap();
        let foo = d.plugins.get("foo").expect("foo plugin present");
        let items = foo
            .get("items")
            .and_then(|v| v.as_array())
            .expect("items is an array");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].get("path").and_then(|v| v.as_str()),
            Some("vendor/a")
        );
    }

    // ── coverage ──────────────────────────────────────────────────────────

    #[test]
    fn coverage_defaults_to_false() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.test.coverage_enabled(), "coverage must default to false");
        assert!(d.test.coverage.is_none(), "absent key must stay None for inheritance");
    }

    #[test]
    fn coverage_can_be_enabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[test]
coverage = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.test.coverage_enabled());
        assert_eq!(d.test.coverage, Some(true));
    }

    #[test]
    fn coverage_explicit_false_is_distinct_from_absent() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[test]
coverage = false
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.test.coverage_enabled());
        assert_eq!(d.test.coverage, Some(false), "explicit false must be Some(false), not None");
    }

    #[test]
    fn coverage_coexists_with_junit_version() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[test]
junitPlatformVersion = "6.0.3"
coverage = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.test.coverage_enabled());
        assert_eq!(d.test.junit_platform_version(), "6.0.3");
    }

    // -- fat-jar -----------------------------------------------------------------

    #[test]
    fn fat_jar_disabled_by_default() {
        let toml = r#"
[application]
name = "x"
version = "1.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(!fat_jar_enabled(&d));
        assert!(!d.fat_jar.section_present);
    }

    #[test]
    fn fat_jar_enabled_when_section_present() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[fat-jar]
"#;
        let d = load_str(toml).unwrap();
        assert!(fat_jar_enabled(&d));
        assert!(d.fat_jar.section_present);
        assert!(d.fat_jar.enabled);
    }

    #[test]
    fn fat_jar_can_be_disabled_explicitly() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[fat-jar]
enabled = false
"#;
        let d = load_str(toml).unwrap();
        assert!(!fat_jar_enabled(&d));
        assert!(d.fat_jar.section_present);
        assert!(!d.fat_jar.enabled);
    }

    #[test]
    fn fat_jar_parses_relocations() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[fat-jar]
shadeAll = true

[[fat-jar.relocations]]
from = "com.google.common"
to = "shaded.com.google.common"

[[fat-jar.relocations]]
from = "com.google.thirdparty"
to = "shaded.com.google.thirdparty"
excludes = ["com.google.thirdparty.publicsuffix.*"]
"#;
        let d = load_str(toml).unwrap();
        assert!(fat_jar_enabled(&d));
        assert!(d.fat_jar.shade_all);
        assert_eq!(d.fat_jar.relocations.len(), 2);
        assert_eq!(d.fat_jar.relocations[0].from, "com.google.common");
        assert_eq!(d.fat_jar.relocations[0].to, "shaded.com.google.common");
        assert!(d.fat_jar.relocations[0].excludes.is_empty());
        assert_eq!(d.fat_jar.relocations[1].from, "com.google.thirdparty");
        assert_eq!(d.fat_jar.relocations[1].excludes.len(), 1);
    }

    #[test]
    fn fat_jar_on_library_is_accepted() {
        let toml = r#"
[library]
name = "x"
version = "1.0"

[fat-jar]
"#;
        let d = load_str(toml).unwrap();
        assert!(fat_jar_enabled(&d));
    }

    #[test]
    fn fat_jar_dep_include_exclude() {
        let toml = r#"
[application]
name = "x"
version = "1.0"

[fat-jar]
shadeAll = true

[dependencies]
"com.example:included" = "1.0"
"com.example:excluded" = { version = "1.0", shade = false }
"com.example:force-included" = { version = "1.0", shade = true }
"#;
        let d = load_str(toml).unwrap();
        let included = d.dependencies.get("com.example:included").unwrap();
        assert_eq!(included.shade(), None);
        assert_eq!(included.fat_jar_include(), None); // legacy accessor still works
        let excluded = d.dependencies.get("com.example:excluded").unwrap();
        assert_eq!(excluded.shade(), Some(false));
        assert_eq!(excluded.fat_jar_include(), Some(false));
        let forced = d.dependencies.get("com.example:force-included").unwrap();
        assert_eq!(forced.shade(), Some(true));
        assert_eq!(forced.fat_jar_include(), Some(true));
    }

    #[test]
    fn fat_jar_dep_shade_all_and_relocations_force() {
        // shadeAll = false + per-dep shade/relocations
        let toml = r#"
[application]
name = "x"
version = "1.0"

[fat-jar]
shadeAll = false

[dependencies]
"com.example:by-shade" = { version = "1.0", shade = true }
"com.example:by-reloc" = { version = "1.0", relocations = [ { from = "com.foo", to = "shaded.com.foo" } ] }
"com.example:not-shaded" = "1.0"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.fat_jar.shade_all);
        assert!(d.dependencies.get("com.example:by-shade").unwrap().should_shade(false));
        assert!(d.dependencies.get("com.example:by-reloc").unwrap().should_shade(false));
        assert!(!d.dependencies.get("com.example:not-shaded").unwrap().should_shade(false));
    }

    #[test]
    fn maven_section_absent_disables_sync_and_pin_transitive() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"
"#;
        let d = load_str(toml).unwrap();
        assert!(!d.maven.sync_enabled(), "absent [maven] must leave sync = false");
        assert!(
            !d.maven.pin_transitive_enabled(),
            "absent [maven] must leave pinTransitive = false"
        );
    }

    #[test]
    fn maven_sync_can_be_enabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[maven]
sync = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.maven.sync_enabled());
        assert!(!d.maven.pin_transitive_enabled());
    }

    #[test]
    fn maven_pin_transitive_can_be_enabled() {
        let toml = r#"
[application]
name = "x"
version = "0.1"
mainClass = "X"

[maven]
sync = true
pinTransitive = true
"#;
        let d = load_str(toml).unwrap();
        assert!(d.maven.sync_enabled());
        assert!(d.maven.pin_transitive_enabled());
    }
}
