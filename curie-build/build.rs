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

    // --- write JAR manifest with Add-Exports --------------------------------
    // Add-Exports as a JAR manifest attribute (JEP 261) means callers of
    // `java -jar wrapper.jar` don't have to pass --add-exports themselves.
    std::fs::write(
        &manifest_path,
        "Manifest-Version: 1.0\n\
         Main-Class: dev.curie.javac.Wrapper\n\
         Add-Exports: jdk.compiler/com.sun.source.util jdk.compiler/com.sun.source.tree\n",
    )
    .expect("failed to write JAR manifest");

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
