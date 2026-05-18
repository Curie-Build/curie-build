//! Build script: compiles `javac-wrapper/Wrapper.java` into a JAR and
//! emits its sha256 digest as a Rust `const` for cache-key versioning.
//!
//! The wrapper itself is then embedded into the Curie binary via
//! `include_bytes!` in `src/wrapper.rs` so end users never need to
//! bootstrap anything at runtime.
//!
//! Building Curie requires `javac` and `jar` in $PATH — both are part of
//! every JDK distribution Curie targets users to install anyway, so
//! making them a build-time prerequisite costs nothing.

use sha2::{Digest, Sha256};
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=javac-wrapper/Wrapper.java");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let class_dir = out_dir.join("wrapper-classes");
    let manifest_path = out_dir.join("wrapper-manifest.txt");
    let jar_path = out_dir.join("wrapper.jar");

    std::fs::create_dir_all(&class_dir).expect("failed to create wrapper-classes/");

    // --- compile Wrapper.java -----------------------------------------------
    // `--release 21` matches the runtime baseline Curie targets.  Anyone
    // running Curie must have at least a JDK-21 `java` on PATH (the same
    // version they use to compile their projects).
    let status = Command::new("javac")
        .arg("--release").arg("21")
        .arg("-d").arg(&class_dir)
        .arg("javac-wrapper/Wrapper.java")
        .status()
        .unwrap_or_else(|e| panic!("building Curie requires javac on PATH ({e})"));
    assert!(status.success(), "javac failed compiling javac-wrapper/Wrapper.java");

    // --- write JAR manifest with Add-Exports + Add-Opens --------------------
    // Add-Exports / Add-Opens as JAR manifest attributes (JEP 261) means
    // callers of `java -jar wrapper.jar` don't have to pass --add-exports /
    // --add-opens themselves.  Add-Exports is mandatory for our own use of
    // com.sun.source.util.TaskListener.  Add-Opens covers Lombok, which
    // requires reflective access to nine internal javac packages on JDK 16+
    // to do its tree-rewriting magic.  Harmless for non-Lombok builds —
    // an --add-opens to a package that's never reflected into is a no-op.
    // JAR manifest spec: lines max 72 bytes including \r\n; continuation
    // lines start with a single space which is NOT part of the value.
    // Add-Opens needs ten module/package entries for Lombok → put each on
    // its own continuation line so we stay under the byte limit.
    // Single-line Add-Opens — the `jar` tool will fold lines >72 bytes
    // into continuation lines on its own.  Writing it as multiple lines
    // ourselves with leading-space continuation markers is wrong: the
    // marker space is consumed by parsing, and the values would
    // concatenate without separators on the receiving side.
    let add_opens_packages = [
        "com.sun.tools.javac.code",
        "com.sun.tools.javac.comp",
        "com.sun.tools.javac.file",
        "com.sun.tools.javac.jvm",
        "com.sun.tools.javac.main",
        "com.sun.tools.javac.model",
        "com.sun.tools.javac.parser",
        "com.sun.tools.javac.processing",
        "com.sun.tools.javac.tree",
        "com.sun.tools.javac.util",
    ];
    let add_opens: String = add_opens_packages
        .iter()
        .map(|p| format!("jdk.compiler/{}", p))
        .collect::<Vec<_>>()
        .join(" ");
    let manifest = format!(
        "Manifest-Version: 1.0\n\
         Main-Class: dev.curie.javac.Wrapper\n\
         Add-Exports: jdk.compiler/com.sun.source.util jdk.compiler/com.sun.source.tree\n\
         Add-Opens: {}\n",
        add_opens,
    );
    std::fs::write(&manifest_path, &manifest).expect("failed to write JAR manifest");

    // --- package into wrapper.jar -------------------------------------------
    let status = Command::new("jar")
        .arg("cfm").arg(&jar_path).arg(&manifest_path)
        .arg("-C").arg(&class_dir).arg(".")
        .status()
        .unwrap_or_else(|e| panic!("building Curie requires `jar` on PATH ({e})"));
    assert!(status.success(), "jar failed packaging wrapper.jar");

    // --- emit sha8 const for runtime cache invalidation ---------------------
    let bytes = std::fs::read(&jar_path).expect("failed to read wrapper.jar");
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    let sha8 = &hex[..8];

    let sha_rs = out_dir.join("wrapper_sha8.rs");
    std::fs::write(
        &sha_rs,
        format!(
            "/// First 8 hex chars of sha256(wrapper.jar).  Used by\n\
             /// [`crate::wrapper::ensure`] to invalidate cached extractions\n\
             /// when the wrapper changes (rebuilt Curie → new bytes → new key).\n\
             pub const WRAPPER_JAR_SHA8: &str = \"{sha8}\";\n",
        ),
    )
    .expect("failed to write wrapper_sha8.rs");
}
