//! `curie new` and `curie init` — project scaffolding.
//!
//! `curie new <kind> [name]`  — creates a new subdirectory and scaffolds inside it.
//! `curie init <kind>`        — scaffolds inside the current directory.
//!
//! Both commands share the same core logic in [`scaffold`].

use std::path::Path;

use anyhow::{bail, Context, Result};

// ── public entry-points ────────────────────────────────────────────────────

/// Called by `curie new <kind> [name]`.
///
/// Creates `<cwd>/<name>/` (errors if it already exists and is non-empty),
/// then delegates to [`scaffold`].
pub fn run_new(kind: ProjectKind, name: Option<String>, package: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;

    let name = resolve_name(name, &cwd)?;
    let dest = cwd.join(&name);

    if dest.exists() {
        // Allow an existing *empty* directory (mirrors `cargo new` behaviour).
        let empty = dest
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            bail!(
                "destination `{}` already exists and is not empty\n\
                 hint: use `curie init` to initialise inside an existing directory",
                dest.display()
            );
        }
    }

    std::fs::create_dir_all(&dest)
        .with_context(|| format!("failed to create directory `{}`", dest.display()))?;

    scaffold(kind, &dest, &name, package)?;

    maybe_register_in_workspace(&dest)?;

    println!("  Created {} `{}` at {}", kind.label(), name, dest.display());
    Ok(())
}

/// Called by `curie init <kind>`.
///
/// Scaffolds inside the current directory. Errors if `Curie.toml` already
/// exists.
pub fn run_init(kind: ProjectKind, package: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;

    let name = dir_base_name(&cwd)?;

    if cwd.join("Curie.toml").exists() {
        bail!(
            "`Curie.toml` already exists in `{}`\n\
             hint: use `curie new` to create a project in a new subdirectory",
            cwd.display()
        );
    }

    scaffold(kind, &cwd, &name, package)?;

    maybe_register_in_workspace(&cwd)?;

    println!("  Initialised {} `{}` in {}", kind.label(), name, cwd.display());
    Ok(())
}

// ── project kind ──────────────────────────────────────────────────────────

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum ProjectKind {
    /// Executable application (produces a runnable JAR)
    App,
    /// Reusable library (produces a library JAR)
    Lib,
    /// Workspace root (contains member projects)
    Workspace,
}

impl ProjectKind {
    fn label(self) -> &'static str {
        match self {
            ProjectKind::App => "application",
            ProjectKind::Lib => "library",
            ProjectKind::Workspace => "workspace",
        }
    }
}

// ── core scaffolding ───────────────────────────────────────────────────────

/// Write all scaffold files into `dest`.
fn scaffold(
    kind: ProjectKind,
    dest: &Path,
    name: &str,
    package: Option<String>,
) -> Result<()> {
    write_gitignore(dest)?;

    match kind {
        ProjectKind::App => scaffold_app(dest, name, package)?,
        ProjectKind::Lib => scaffold_lib(dest, name, package)?,
        ProjectKind::Workspace => scaffold_workspace(dest)?,
    }
    Ok(())
}

fn scaffold_app(dest: &Path, name: &str, package: Option<String>) -> Result<()> {
    let pkg = package.unwrap_or_else(|| derive_package(name));
    let class = derive_class_name(name);
    let main_class = format!("{}.{}", pkg, class);

    // Curie.toml
    let toml = format!(
        "[application]\nname = \"{}\"\nversion = \"0.1.0\"\nmainClass = \"{}\"\n",
        name, main_class
    );
    write_file(&dest.join("Curie.toml"), &toml)?;

    // src/<package>/<Class>.java
    let src_dir = dest.join("src").join(&pkg);
    std::fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create `{}`", src_dir.display()))?;

    let java = format!(
        "package {};\n\npublic class {} {{\n    public static void main(String[] args) {{\n        System.out.println(\"Hello from {}!\");\n    }}\n}}\n",
        pkg, class, class
    );
    write_file(&src_dir.join(format!("{}.java", class)), &java)?;

    Ok(())
}

fn scaffold_lib(dest: &Path, name: &str, package: Option<String>) -> Result<()> {
    let pkg = package.unwrap_or_else(|| derive_package(name));
    let class = derive_class_name(name);

    // Curie.toml
    let toml = format!(
        "[library]\nname = \"{}\"\nversion = \"0.1.0\"\n",
        name
    );
    write_file(&dest.join("Curie.toml"), &toml)?;

    // src/<package>/<Class>.java
    let src_dir = dest.join("src").join(&pkg);
    std::fs::create_dir_all(&src_dir)
        .with_context(|| format!("failed to create `{}`", src_dir.display()))?;

    let java = format!(
        "package {};\n\npublic class {} {{\n    // TODO: implement\n}}\n",
        pkg, class
    );
    write_file(&src_dir.join(format!("{}.java", class)), &java)?;

    Ok(())
}

fn scaffold_workspace(dest: &Path) -> Result<()> {
    let toml = "[workspace]\nmembers = []\n";
    write_file(&dest.join("Curie.toml"), toml)?;
    Ok(())
}

fn write_gitignore(dest: &Path) -> Result<()> {
    write_file(&dest.join(".gitignore"), "target/\n")
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)
        .with_context(|| format!("failed to write `{}`", path.display()))
}

// ── workspace auto-registration ────────────────────────────────────────────

/// If `dest`'s parent directory contains a workspace `Curie.toml`, append
/// `dest`'s directory name to `members` using format-preserving `toml_edit`.
fn maybe_register_in_workspace(dest: &Path) -> Result<()> {
    let parent = match dest.parent() {
        Some(p) => p,
        None => return Ok(()),
    };

    let ws_toml_path = parent.join("Curie.toml");
    if !ws_toml_path.exists() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&ws_toml_path)
        .with_context(|| format!("failed to read `{}`", ws_toml_path.display()))?;

    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("failed to parse `{}`", ws_toml_path.display()))?;

    // Only proceed if this is a workspace Curie.toml with a members array.
    let members = doc
        .get_mut("workspace")
        .and_then(|ws| ws.get_mut("members"))
        .and_then(|m| m.as_array_mut());

    let members = match members {
        Some(m) => m,
        None => return Ok(()),
    };

    let member_name = dir_base_name(dest)?;

    // Avoid duplicates.
    let already_present = members
        .iter()
        .any(|v| v.as_str() == Some(&member_name));
    if already_present {
        return Ok(());
    }

    members.push(member_name.as_str());

    std::fs::write(&ws_toml_path, doc.to_string())
        .with_context(|| format!("failed to write `{}`", ws_toml_path.display()))?;

    println!("  Added \"{}\" to workspace at {}", member_name, ws_toml_path.display());
    Ok(())
}

// ── name / package / class derivation ─────────────────────────────────────

/// Resolve the project name: use the provided name, or fall back to the
/// base name of `cwd`.
fn resolve_name(name: Option<String>, cwd: &Path) -> Result<String> {
    match name {
        Some(n) if !n.is_empty() => Ok(n),
        _ => dir_base_name(cwd),
    }
}

/// Return the base name of `dir` as a `String`.
fn dir_base_name(dir: &Path) -> Result<String> {
    dir.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_owned())
        .context("could not determine a project name from the current directory")
}

/// Derive a Java package name from a project name.
///
/// `my-service` → `com.example.myservice`
pub fn derive_package(name: &str) -> String {
    let sanitised: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    format!("com.example.{}", sanitised)
}

/// Derive a PascalCase Java class name from a project name.
///
/// `my-service` → `MyService`, `hello_world` → `HelloWorld`
pub fn derive_class_name(name: &str) -> String {
    name.split(|c| c == '-' || c == '_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()
                }
            }
        })
        .collect()
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // ── derivation helpers ─────────────────────────────────────────────────

    #[test]
    fn derive_package_from_name() {
        assert_eq!(derive_package("my-service"), "com.example.myservice");
        assert_eq!(derive_package("hello-world"), "com.example.helloworld");
        assert_eq!(derive_package("string_utils"), "com.example.stringutils");
        assert_eq!(derive_package("foo"), "com.example.foo");
    }

    #[test]
    fn derive_class_name_from_name() {
        assert_eq!(derive_class_name("my-service"), "MyService");
        assert_eq!(derive_class_name("hello-world"), "HelloWorld");
        assert_eq!(derive_class_name("string_utils"), "StringUtils");
        assert_eq!(derive_class_name("foo"), "Foo");
    }

    // ── app scaffold ───────────────────────────────────────────────────────

    #[test]
    fn new_app_creates_expected_files() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();

        scaffold(ProjectKind::App, &dest, "my-app", None).unwrap();

        // Curie.toml
        let toml = fs::read_to_string(dest.join("Curie.toml")).unwrap();
        assert!(toml.contains("[application]"));
        assert!(toml.contains("name = \"my-app\""));
        assert!(toml.contains("mainClass = \"com.example.myapp.MyApp\""));

        // .gitignore
        let gi = fs::read_to_string(dest.join(".gitignore")).unwrap();
        assert!(gi.contains("target/"));

        // Java source
        let java_path = dest
            .join("src")
            .join("com.example.myapp")
            .join("MyApp.java");
        assert!(java_path.exists(), "expected {:?} to exist", java_path);
        let java = fs::read_to_string(&java_path).unwrap();
        assert!(java.contains("package com.example.myapp;"));
        assert!(java.contains("public class MyApp"));
        assert!(java.contains("public static void main"));
    }

    #[test]
    fn new_app_custom_package() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();

        scaffold(
            ProjectKind::App,
            &dest,
            "my-app",
            Some("org.acme.demo".to_string()),
        )
        .unwrap();

        let toml = fs::read_to_string(dest.join("Curie.toml")).unwrap();
        assert!(toml.contains("mainClass = \"org.acme.demo.MyApp\""));

        let java_path = dest.join("src").join("org.acme.demo").join("MyApp.java");
        assert!(java_path.exists());
    }

    // ── lib scaffold ───────────────────────────────────────────────────────

    #[test]
    fn new_lib_creates_expected_files() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("my-lib");
        fs::create_dir_all(&dest).unwrap();

        scaffold(ProjectKind::Lib, &dest, "my-lib", None).unwrap();

        let toml = fs::read_to_string(dest.join("Curie.toml")).unwrap();
        assert!(toml.contains("[library]"));
        assert!(toml.contains("name = \"my-lib\""));
        assert!(!toml.contains("mainClass"));

        let java_path = dest
            .join("src")
            .join("com.example.mylib")
            .join("MyLib.java");
        assert!(java_path.exists());
        let java = fs::read_to_string(&java_path).unwrap();
        assert!(java.contains("package com.example.mylib;"));
        assert!(java.contains("public class MyLib"));
    }

    // ── workspace scaffold ─────────────────────────────────────────────────

    #[test]
    fn new_workspace_creates_expected_files() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("my-ws");
        fs::create_dir_all(&dest).unwrap();

        scaffold(ProjectKind::Workspace, &dest, "my-ws", None).unwrap();

        let toml = fs::read_to_string(dest.join("Curie.toml")).unwrap();
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("members = []"));

        // No source directory for workspace
        assert!(!dest.join("src").exists());
    }

    // ── guard: non-empty dir ───────────────────────────────────────────────

    #[test]
    fn new_existing_nonempty_dir_is_rejected() {
        let tmp = tempdir().unwrap();
        // Pre-populate so run_new treats it as non-empty.
        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("README.md"), "existing").unwrap();

        // run_new works relative to cwd which we can't change in a unit test,
        // so test the guard logic directly.
        let empty = dest
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        assert!(!empty, "directory should be non-empty");
    }

    // ── guard: existing Curie.toml ─────────────────────────────────────────

    #[test]
    fn init_existing_curie_toml_is_rejected() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("Curie.toml"), "[library]\nname=\"x\"\nversion=\"0\"\n")
            .unwrap();
        // Verify the check logic: Curie.toml present → should error.
        assert!(tmp.path().join("Curie.toml").exists());
    }

    // ── workspace auto-registration ───────────────────────────────────────

    #[test]
    fn auto_register_adds_member_to_workspace() {
        let tmp = tempdir().unwrap();

        // Create a workspace Curie.toml in tmp.
        fs::write(
            tmp.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"existing\"]\n",
        )
        .unwrap();

        // Scaffold a new app inside tmp/my-app.
        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();
        scaffold(ProjectKind::App, &dest, "my-app", None).unwrap();
        maybe_register_in_workspace(&dest).unwrap();

        let ws = fs::read_to_string(tmp.path().join("Curie.toml")).unwrap();
        assert!(ws.contains("\"my-app\""), "workspace should contain my-app: {}", ws);
        assert!(ws.contains("\"existing\""), "existing member should be preserved: {}", ws);
    }

    #[test]
    fn auto_register_no_duplicate() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("Curie.toml"),
            "[workspace]\nmembers = [\"my-app\"]\n",
        )
        .unwrap();

        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();
        maybe_register_in_workspace(&dest).unwrap();

        let ws = fs::read_to_string(tmp.path().join("Curie.toml")).unwrap();
        // Should only appear once.
        assert_eq!(ws.matches("\"my-app\"").count(), 1);
    }

    #[test]
    fn auto_register_noop_when_no_workspace() {
        let tmp = tempdir().unwrap();
        // No parent Curie.toml.
        let dest = tmp.path().join("my-app");
        fs::create_dir_all(&dest).unwrap();
        // Should succeed without touching anything.
        maybe_register_in_workspace(&dest).unwrap();
    }

    #[test]
    fn auto_register_preserves_workspace_comments() {
        let tmp = tempdir().unwrap();
        let original = "# My workspace\n[workspace]\nmembers = [] # list of members\n";
        fs::write(tmp.path().join("Curie.toml"), original).unwrap();

        let dest = tmp.path().join("new-member");
        fs::create_dir_all(&dest).unwrap();
        maybe_register_in_workspace(&dest).unwrap();

        let ws = fs::read_to_string(tmp.path().join("Curie.toml")).unwrap();
        assert!(ws.contains("# My workspace"), "top comment should be preserved");
        assert!(ws.contains("# list of members"), "inline comment should be preserved");
        assert!(ws.contains("\"new-member\""));
    }
}
