//! Auto-detection and bytecode validation of an application's `main` class.
//!
//! Two phases:
//!   1. Source heuristic — scan production `.java` and `.kt` files for any of
//!      the recognised main-method shapes.
//!   2. Bytecode validation — invoke `javap -p` on each candidate FQCN to
//!      confirm the compiled class actually declares a launchable `main`.
//!
//! ## Kotlin naming convention
//! The Kotlin compiler places top-level functions from `Foo.kt` into a class
//! named `FooKt`.  A `fun main()` at the top level of `Hello.kt` in package
//! `com.example` therefore ends up as `com.example.HelloKt`.  The heuristic
//! below derives this name and then confirms it via `javap`.

use crate::compile::pkg_prefix_for_src_root;
use crate::jar::classpath_string;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// FQCN derivation
// ---------------------------------------------------------------------------

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
fn fqcn_from_java_source(src_roots: &[PathBuf], source: &Path) -> Option<String> {
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

/// Derive the fully-qualified class name for a `.kt` source file.
///
/// Kotlin compiles top-level functions from `Foo.kt` into a JVM class named
/// `FooKt`.  The package is derived the same way as for Java sources.
///
/// Example: `src/main/kotlin/com/example/Hello.kt`
///   → stem `Hello` + `Kt` suffix → FQCN `com.example.HelloKt`
fn fqcn_from_kotlin_source(src_roots: &[PathBuf], source: &Path) -> Option<String> {
    // Try each src_root (including kotlin-specific ones like src/main/kotlin).
    for src_root in src_roots {
        if let Ok(rel) = source.strip_prefix(src_root) {
            let without_ext = rel.with_extension("");
            let mut parts: Vec<String> = without_ext
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if parts.is_empty() {
                continue;
            }
            // Append "Kt" suffix to the file-stem (last component).
            let last = parts.len() - 1;
            parts[last].push_str("Kt");

            let rel_fqcn = parts.join(".");
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

// ---------------------------------------------------------------------------
// Source heuristics
// ---------------------------------------------------------------------------

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

/// Returns `true` when the Java source text contains any recognisable
/// main-method signature under Java 21's flexible launch protocol.
fn java_source_has_main(text: &str) -> bool {
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

/// Returns `true` when the Kotlin source text contains a top-level `fun main`
/// entry point (with or without arguments, with or without suspend).
///
/// "Top-level" is approximated by tracking brace depth line by line:
/// `fun main` is top-level when the brace depth at the start of that line is 0
/// (i.e. not inside any class/object/function body).
fn kotlin_source_has_main(text: &str) -> bool {
    let mut depth: i32 = 0;

    for raw_line in text.lines() {
        // Strip inline line comment before processing.
        let line = if let Some(i) = raw_line.find("//") {
            &raw_line[..i]
        } else {
            raw_line
        };

        let trimmed = line.trim_start();

        // Check for top-level `fun main` / `suspend fun main` at current depth.
        if depth == 0
            && (trimmed.starts_with("fun main(") || trimmed.starts_with("suspend fun main("))
        {
            return true;
        }

        // Update depth based on braces in this line.
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => { depth -= 1; if depth < 0 { depth = 0; } }
                _ => {}
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Bytecode validation
// ---------------------------------------------------------------------------

/// Returns `true` when `javap` output contains a recognisable main-method
/// signature under Java 21's launch protocol or Kotlin's compiled output.
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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
///
/// Both `.java` and `.kt` files are scanned.  Kotlin top-level `fun main`
/// functions are mapped to the `<FileNameKt>` JVM class that the Kotlin
/// compiler generates.
pub fn detect_main_class(
    src_roots: &[PathBuf],
    sources: &[PathBuf],
    classes_dir: &Path,
    dep_jars: &[PathBuf],
) -> Result<String> {
    // Phase 1: fast source heuristic — collect (fqcn, source_path) candidates.
    let mut source_candidates: Vec<(String, PathBuf)> = Vec::new();
    for source in sources {
        let text = match std::fs::read_to_string(source) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "java" => {
                if java_source_has_main(&text) {
                    if let Some(fqcn) = fqcn_from_java_source(src_roots, source) {
                        source_candidates.push((fqcn, source.clone()));
                    }
                }
            }
            "kt" => {
                if kotlin_source_has_main(&text) {
                    if let Some(fqcn) = fqcn_from_kotlin_source(src_roots, source) {
                        source_candidates.push((fqcn, source.clone()));
                    }
                }
            }
            _ => {}
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- kotlin_source_has_main ---

    #[test]
    fn kotlin_main_no_args() {
        assert!(kotlin_source_has_main("fun main() {\n    println(\"hi\")\n}\n"));
    }

    #[test]
    fn kotlin_main_with_args() {
        assert!(kotlin_source_has_main(
            "fun main(args: Array<String>) {\n    println(args[0])\n}\n"
        ));
    }

    #[test]
    fn kotlin_suspend_main() {
        assert!(kotlin_source_has_main("suspend fun main() {\n}\n"));
    }

    #[test]
    fn kotlin_main_inside_class_not_matched() {
        // Indented inside a class body — should NOT be treated as top-level.
        let src = "class Foo {\n    fun main() {\n    }\n}\n";
        assert!(!kotlin_source_has_main(src));
    }

    #[test]
    fn kotlin_no_main() {
        assert!(!kotlin_source_has_main("fun helper() {}\n"));
    }

    #[test]
    fn kotlin_main_in_comment_not_matched() {
        let src = "// fun main() — example\nfun helper() {}\n";
        assert!(!kotlin_source_has_main(src));
    }

    // --- fqcn_from_kotlin_source ---

    #[test]
    fn kotlin_fqcn_maven_layout() {
        let root = PathBuf::from("/proj/src/main/kotlin");
        let src = PathBuf::from("/proj/src/main/kotlin/com/example/Hello.kt");
        let fqcn = fqcn_from_kotlin_source(&[root], &src);
        assert_eq!(fqcn, Some("com.example.HelloKt".to_string()));
    }

    #[test]
    fn kotlin_fqcn_default_package() {
        let root = PathBuf::from("/proj/src/main/kotlin");
        let src = PathBuf::from("/proj/src/main/kotlin/App.kt");
        let fqcn = fqcn_from_kotlin_source(&[root], &src);
        assert_eq!(fqcn, Some("AppKt".to_string()));
    }

    #[test]
    fn kotlin_fqcn_multiple_roots_picks_correct() {
        let roots = vec![
            PathBuf::from("/proj/src/main/java"),
            PathBuf::from("/proj/src/main/kotlin"),
        ];
        let src = PathBuf::from("/proj/src/main/kotlin/com/example/Main.kt");
        let fqcn = fqcn_from_kotlin_source(&roots, &src);
        assert_eq!(fqcn, Some("com.example.MainKt".to_string()));
    }

    // --- java_source_has_main (renamed from source_has_main) ---

    #[test]
    fn java_main_static() {
        assert!(java_source_has_main(
            "public class App { public static void main(String[] args) {} }"
        ));
    }

    #[test]
    fn java_no_main() {
        assert!(!java_source_has_main("public class Lib { public void run() {} }"));
    }

    // --- javap_output_has_main ---

    #[test]
    fn javap_static_main_detected() {
        let out = "public static void main(java.lang.String[]);";
        assert!(javap_output_has_main(out));
    }

    #[test]
    fn javap_kotlin_compiled_main_detected() {
        // Kotlin top-level fun main compiles to:
        //   public static final void main(java.lang.String[]);
        let out = "public static final void main(java.lang.String[]);";
        assert!(javap_output_has_main(out));
    }

    #[test]
    fn javap_no_main() {
        let out = "public void helper();\npublic java.lang.String getName();";
        assert!(!javap_output_has_main(out));
    }
}
