use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Descriptor {
    pub application: Option<Application>,
    pub library: Option<Library>,
    #[serde(default)]
    pub java: Java,
    #[serde(default)]
    pub docker: Docker,
    /// `[dependencies]` table: keys are `"group:artifact"`, values are version strings.
    /// An empty version string (`""`) means the version is supplied by a BOM in
    /// `[bom-imports]`.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// `[test-dependencies]` table: test-scoped deps not included in the production JAR.
    /// An empty version string (`""`) means the version is supplied by a BOM in
    /// `[bom-imports]` or `[test-bom-imports]`.
    #[serde(rename = "test-dependencies", default)]
    pub test_dependencies: BTreeMap<String, String>,
    /// `[[repositories]]` array for additional Maven repositories.
    #[serde(default)]
    pub repositories: Vec<RepositoryEntry>,
    /// `[bom-imports]` table: BOMs whose `<dependencyManagement>` sections provide
    /// version constraints for `[dependencies]` and `[test-dependencies]`.
    /// Keys are `"group:artifact"`, values are version strings.
    /// Later entries win when two BOMs manage the same artifact.
    #[serde(rename = "bom-imports", default)]
    pub bom_imports: BTreeMap<String, String>,
    /// `[test-bom-imports]` table: BOMs that additionally apply to test dependencies
    /// only.  Combined with `[bom-imports]` during test dependency resolution, with
    /// `[test-bom-imports]` taking higher priority.
    /// Keys are `"group:artifact"`, values are version strings.
    #[serde(rename = "test-bom-imports", default)]
    pub test_bom_imports: BTreeMap<String, String>,
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

#[derive(Debug, Deserialize)]
pub struct Java {
    #[serde(rename = "sourceCompatibility", default = "default_source_compat")]
    pub source_compatibility: String,
}

fn default_source_compat() -> String {
    "21".to_string()
}

impl Default for Java {
    fn default() -> Self {
        Java {
            source_compatibility: default_source_compat(),
        }
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
    /// Returns true when this is a library project (has `[library]` section).
    pub fn is_library(&self) -> bool {
        self.library.is_some()
    }

    /// Project name regardless of kind.
    pub fn project_name(&self) -> &str {
        if let Some(lib) = &self.library {
            &lib.name
        } else if let Some(app) = &self.application {
            &app.name
        } else {
            unreachable!("descriptor validation guarantees one of library/application is set")
        }
    }

    /// Project version regardless of kind.
    pub fn project_version(&self) -> &str {
        if let Some(lib) = &self.library {
            &lib.version
        } else if let Some(app) = &self.application {
            &app.version
        } else {
            unreachable!("descriptor validation guarantees one of library/application is set")
        }
    }

    /// Resolved Docker image name: descriptor override or application name.
    pub fn image_name(&self) -> &str {
        self.docker
            .image_name
            .as_deref()
            .unwrap_or_else(|| self.project_name())
    }

    /// Resolved Docker image tag: descriptor override or application version.
    pub fn image_tag(&self) -> &str {
        self.docker
            .image_tag
            .as_deref()
            .unwrap_or_else(|| self.project_version())
    }

    /// Full image reference, e.g. "hello-world:0.1.0".
    pub fn image_ref(&self) -> String {
        format!("{}:{}", self.image_name(), self.image_tag())
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

    // Parse raw table first so we can detect optional section presence.
    let raw: toml::Value = toml::from_str(&content)
        .map_err(|e| format_parse_error(e, &content, &path))?;

    let docker_section_present = raw
        .as_table()
        .map(|t| t.contains_key("docker"))
        .unwrap_or(false);

    let library_section_present = raw
        .as_table()
        .map(|t| t.contains_key("library"))
        .unwrap_or(false);

    let application_section_present = raw
        .as_table()
        .map(|t| t.contains_key("application"))
        .unwrap_or(false);

    // Deserialize directly from the source string so that toml retains span
    // information and can produce contextual line/column error messages.
    let mut descriptor: Descriptor = toml::from_str(&content)
        .map_err(|e| format_parse_error(e, &content, &path))?;

    descriptor.docker.section_present = docker_section_present;

    // Validate: exactly one of [application] or [library] must be present.
    match (application_section_present, library_section_present) {
        (false, false) => bail!(
            "Curie.toml must contain either an [application] or [library] section"
        ),
        (true, true) => bail!(
            "Curie.toml must not contain both [application] and [library] sections"
        ),
        _ => {}
    }

    // Validate: library projects cannot use Docker.
    if library_section_present && docker_section_present {
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
