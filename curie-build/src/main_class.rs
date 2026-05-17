//! Auto-detection and bytecode validation of an application's `main` class.
//!
//! Two phases:
//!   1. Source heuristic — scan production `.java` files for any of the
//!      Java-21 launch-protocol main-method shapes.
//!   2. Bytecode validation — invoke `javap -p` on each candidate FQCN to
//!      confirm the compiled class actually declares a launchable `main`.

use crate::compile::pkg_prefix_for_src_root;
use crate::jar::classpath_string;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Derive the fully-qualified class name from a `.java` source path, trying
/// each `src_root` in order and using the first successful strip.
///
/// For Maven-style roots (`src/main/java`), the FQCN is the path under the
/// root with separators replaced by dots.
///
/// For flat-package roots (`src/com.example.foo`), the directory name IS the
/// package, so it is prepended to the FQCN.  Example: source
/// `src/com.example.foo/Bar.java` under root `src/com.example.foo` yields
/// FQCN `com.example.foo.Bar`.
///
/// For unnamed/compact source files (no top-level type declaration) the class
/// name equals the file stem.
fn fqcn_from_source(src_roots: &[PathBuf], source: &Path) -> Option<String> {
    for src_root in src_roots {
        if let Ok(rel) = source.strip_prefix(src_root) {
            let without_ext = rel.with_extension("");
            let rel_fqcn = without_ext
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(".");
            if rel_fqcn.is_empty() {
                continue;
            }
            let pkg_prefix = pkg_prefix_for_src_root(src_root);
            let fqcn = if pkg_prefix.is_empty() {
                rel_fqcn
            } else {
                format!("{}.{}", pkg_prefix, rel_fqcn)
            };
            return Some(fqcn);
        }
    }
    None
}

/// Returns `true` when the source text looks like a compact (unnamed-class)
/// source file — heuristically: no top-level `class`, `interface`, `enum`, or
/// `record` keyword outside comments.
fn is_compact_source(text: &str) -> bool {
    // Strip line comments then block comments.
    let no_line: String = text
        .lines()
        .map(|l| if let Some(i) = l.find("//") { &l[..i] } else { l })
        .collect::<Vec<_>>()
        .join("\n");

    let mut stripped = String::with_capacity(no_line.len());
    let mut chars = no_line.chars().peekable();
    let mut in_block = false;
    while let Some(ch) = chars.next() {
        if in_block {
            if ch == '*' && chars.peek() == Some(&'/') { chars.next(); in_block = false; }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next(); in_block = true;
        } else {
            stripped.push(ch);
        }
    }

    for kw in ["class", "interface", "enum", "record"] {
        let mut s = stripped.as_str();
        while let Some(idx) = s.find(kw) {
            let before = if idx == 0 { ' ' } else { s.as_bytes()[idx - 1] as char };
            let after_idx = idx + kw.len();
            let after = if after_idx >= s.len() { ' ' } else { s.as_bytes()[after_idx] as char };
            if !before.is_alphanumeric() && before != '_' && !after.is_alphanumeric() && after != '_' {
                return false;
            }
            s = &s[idx + kw.len()..];
        }
    }
    true
}

/// Returns `true` when the source text contains any recognisable main-method
/// signature under Java 21's flexible launch protocol.
fn source_has_main(text: &str) -> bool {
    // Compact / unnamed class: any `void main` is enough.
    if is_compact_source(text) && text.contains("void main") {
        return true;
    }
    // Static main (patterns 1 & 2).
    let flat = text.replace(['\n', '\r'], " ");
    if flat.contains("static") && flat.contains("void main") {
        return true;
    }
    // Instance main (patterns 3 & 4): any remaining `void main`.
    if flat.contains("void main") {
        return true;
    }
    false
}

/// Returns `true` when `javap` output contains a recognisable main-method
/// signature under Java 21's launch protocol.
fn javap_output_has_main(javap_out: &str) -> bool {
    for line in javap_out.lines() {
        let l = line.trim();
        // static void main(...) — with or without `public`
        if l.contains("static") && l.contains("void main(") {
            return true;
        }
        // instance void main(...) — non-private
        if l.contains("void main(") && !l.contains("private") {
            return true;
        }
    }
    false
}

/// Validate a declared or detected class name against compiled bytecode via
/// `javap`.  Returns `Ok(())` if the class has a launchable main method.
pub fn validate_main_class(
    class_name: &str,
    classes_dir: &Path,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let mut cp_entries = vec![classes_dir.to_path_buf()];
    cp_entries.extend_from_slice(dep_jars);
    let cp = classpath_string(&cp_entries);

    let output = Command::new("javap")
        .arg("-p")
        .arg("-classpath")
        .arg(&cp)
        .arg(class_name)
        .output()
        .context("failed to invoke javap — is a JDK installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "mainClass `{}` was not found in compiled output\n  {}",
            class_name,
            stderr.trim()
        );
    }

    let javap_out = String::from_utf8_lossy(&output.stdout);
    if !javap_output_has_main(&javap_out) {
        anyhow::bail!(
            "mainClass `{}` does not declare a launchable main method\n\
             \n\
             Expected one of:\n\
               public static void main(String[] args)\n\
               static void main()\n\
               void main(String[] args)   (instance, non-private)\n\
               void main()                (instance, non-private)",
            class_name
        );
    }
    Ok(())
}

/// Scan production sources for candidates then validate each against compiled
/// bytecode.  Returns the single detected class name, or an error.
pub fn detect_main_class(
    src_roots: &[PathBuf],
    sources: &[PathBuf],
    classes_dir: &Path,
    dep_jars: &[PathBuf],
) -> Result<String> {
    // Phase 1: fast source heuristic.
    let mut source_candidates: Vec<(String, PathBuf)> = Vec::new();
    for source in sources {
        let text = match std::fs::read_to_string(source) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if source_has_main(&text) {
            if let Some(fqcn) = fqcn_from_source(src_roots, source) {
                source_candidates.push((fqcn, source.clone()));
            }
        }
    }

    if source_candidates.is_empty() {
        anyhow::bail!(
            "no main method found in any production source file\n\
             \n\
             Add a main method to one of your classes, or declare it explicitly:\n\
             \n\
               # Curie.toml\n\
               [application]\n\
               mainClass = \"com.example.YourMainClass\""
        );
    }

    // Phase 2: bytecode validation.
    let mut valid: Vec<String> = Vec::new();
    for (fqcn, _) in &source_candidates {
        if validate_main_class(fqcn, classes_dir, dep_jars).is_ok() {
            valid.push(fqcn.clone());
        }
    }

    match valid.len() {
        0 => anyhow::bail!(
            "no launchable main method found after bytecode inspection\n\
             \n\
             Source candidates that did not pass bytecode validation:\n\
             {}\n\
             \n\
             Declare the main class explicitly in Curie.toml:\n\
             \n\
               [application]\n\
               mainClass = \"com.example.YourMainClass\"",
            source_candidates
                .iter()
                .map(|(n, _)| format!("  {}", n))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        1 => Ok(valid.remove(0)),
        _ => anyhow::bail!(
            "multiple classes with a main method found — declare one explicitly in Curie.toml:\n\
             \n\
             {}\n\
             \n\
               # Curie.toml\n\
               [application]\n\
               mainClass = \"com.example.YourChosenMainClass\"",
            valid
                .iter()
                .map(|n| format!("  {}", n))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}
