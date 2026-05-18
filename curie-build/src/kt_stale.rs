//! Kotlin stale-class detection.
//!
//! Java's stale-class story is driven by the source→class manifest the javac
//! wrapper emits — Curie consults it to delete classes whose source vanished
//! (pre-compile) and to remove classes a still-present source no longer
//! produces (post-compile).
//!
//! `kotlinc` offers no equivalent hook.  Instead we exploit a property of how
//! we drive it: every build that runs kotlinc passes ALL current `.kt`
//! sources, so kotlinc re-emits every class the current Kotlin source set
//! still produces.  That means we can wipe Kotlin-derived classes *before*
//! kotlinc runs and trust kotlinc to put back exactly what's still live.
//!
//! How we identify a "Kotlin-derived" class: every JVM `.class` file may
//! carry an optional `SourceFile` attribute naming the source it came from.
//! `kotlinc` writes that attribute with the `.kt` filename; `javac` writes it
//! with the `.java` filename.  So `SourceFile` ending in `.kt` is the
//! discriminator.  Class files written by `kotlinc` from `.java` sources
//! (the mixed-build phase 1) carry `SourceFile = "Foo.java"` and are left
//! alone — javac will rewrite them in phase 2.
//!
//! # Source-set tracking
//!
//! Deletions are a special case: if the only change between two builds is
//! that a `.kt` file got removed, mtime-based incremental check sees no
//! newer source and returns "up to date" — kotlinc never runs and the
//! orphan class is never wiped.  We compensate by stamping the canonical
//! Kotlin source list under `target/.kt-sources` after every successful
//! compile.  The next build compares; any difference forces a recompile so
//! the wipe gets a chance to run.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::incremental::walk_files;

// ---------------------------------------------------------------------------
// SourceFile attribute extraction
// ---------------------------------------------------------------------------

/// Parse a JVM `.class` file and return the value of its `SourceFile`
/// attribute, if present.  Returns `None` on absent attribute, malformed
/// input, or any read error — callers treat `None` as "don't classify this
/// class as Kotlin-derived" (i.e. leave it alone), which is the safe
/// default.
///
/// We parse just enough of the class file to walk the constant pool and skip
/// past fields/methods to the class-level attribute table.  No bytecode is
/// inspected.
pub fn class_source_file(class_path: &Path) -> Option<String> {
    let bytes = std::fs::read(class_path).ok()?;
    let mut r = Reader::new(&bytes);

    // magic + version
    if r.read_u32()? != 0xCAFEBABE {
        return None;
    }
    let _minor = r.read_u16()?;
    let _major = r.read_u16()?;

    // Constant pool.  Index 0 is reserved; entries are 1..cp_count-1.
    // CONSTANT_Long (5) and CONSTANT_Double (6) occupy two indices.
    //
    // UTF8 entries hold *modified UTF-8* (JVMS §4.4.7), not standard UTF-8 —
    // notably null bytes are encoded as the overlong pair `C0 80`.  Kotlin's
    // `@Metadata` annotation embeds binary data in such entries, so strict
    // UTF-8 decoding would reject them and abort cp parsing before we ever
    // reach the SourceFile attribute.  We sidestep the issue by storing
    // entries as raw byte slices and matching against them byte-wise; only
    // the final result (a source-file name, always ASCII in practice) is
    // converted to a Rust String, and lossily for safety.
    let cp_count = r.read_u16()?;
    let mut utf8: std::collections::HashMap<u16, &[u8]> = std::collections::HashMap::new();
    let mut idx: u16 = 1;
    while idx < cp_count {
        let tag = r.read_u8()?;
        match tag {
            1 => {
                let len = r.read_u16()? as usize;
                let raw = r.read_bytes(len)?;
                utf8.insert(idx, raw);
            }
            3 | 4 => {
                r.skip(4)?;
            }
            5 | 6 => {
                r.skip(8)?;
                // Long/Double take two CP slots — bump idx an extra time.
                idx = idx.checked_add(1)?;
            }
            7 | 8 | 16 | 19 | 20 => {
                r.skip(2)?;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                r.skip(4)?;
            }
            15 => {
                r.skip(3)?;
            }
            _ => return None,
        }
        idx = idx.checked_add(1)?;
    }

    // access_flags, this_class, super_class
    r.skip(6)?;

    // interfaces
    let ifc = r.read_u16()? as usize;
    r.skip(2 * ifc)?;

    // fields[]
    let fields_count = r.read_u16()? as usize;
    for _ in 0..fields_count {
        r.skip(6)?; // access_flags + name_index + descriptor_index
        skip_attributes(&mut r)?;
    }

    // methods[] (same shape as fields)
    let methods_count = r.read_u16()? as usize;
    for _ in 0..methods_count {
        r.skip(6)?;
        skip_attributes(&mut r)?;
    }

    // class-level attributes
    let class_attr_count = r.read_u16()? as usize;
    for _ in 0..class_attr_count {
        let name_idx = r.read_u16()?;
        let alen = r.read_u32()? as usize;
        if utf8.get(&name_idx).copied() == Some(b"SourceFile".as_ref()) {
            // body = sourcefile_index (u2)
            if alen != 2 {
                return None;
            }
            let src_idx = r.read_u16()?;
            let raw = utf8.get(&src_idx).copied()?;
            // Source-file names are ASCII in every Java/Kotlin toolchain
            // we'll meet; lossy decode is fine and avoids choking on the
            // theoretically-possible non-ASCII file name.
            return Some(String::from_utf8_lossy(raw).into_owned());
        }
        r.skip(alen)?;
    }
    None
}

fn skip_attributes(r: &mut Reader) -> Option<()> {
    let n = r.read_u16()? as usize;
    for _ in 0..n {
        r.skip(2)?; // name_index
        let alen = r.read_u32()? as usize;
        r.skip(alen)?;
    }
    Some(())
}

/// Tiny defensive byte reader.  Every operation returns `None` on overrun so
/// the surrounding parser can short-circuit out of malformed input without
/// panicking.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn read_u8(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn read_u16(&mut self) -> Option<u16> {
        let hi = self.read_u8()? as u16;
        let lo = self.read_u8()? as u16;
        Some((hi << 8) | lo)
    }
    fn read_u32(&mut self) -> Option<u32> {
        let a = self.read_u8()? as u32;
        let b = self.read_u8()? as u32;
        let c = self.read_u8()? as u32;
        let d = self.read_u8()? as u32;
        Some((a << 24) | (b << 16) | (c << 8) | d)
    }
    fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n)?;
        if end > self.bytes.len() {
            return None;
        }
        self.pos = end;
        Some(())
    }
}

// ---------------------------------------------------------------------------
// Wipe + source-set tracking
// ---------------------------------------------------------------------------

/// Delete every `.class` file under `classes_dir` whose `SourceFile`
/// attribute ends with `.kt`.  Returns the absolute paths of the files
/// removed so the caller can tell, post-kotlinc, which ones weren't
/// re-emitted — those are the true orphans worth reporting.
///
/// Called just before invoking `kotlinc` so the compiler starts from a clean
/// slate — anything still produced will be re-emitted, anything no longer
/// produced will stay gone.
pub fn wipe_kotlin_derived_classes(classes_dir: &Path) -> Result<Vec<PathBuf>> {
    if !classes_dir.exists() {
        return Ok(Vec::new());
    }
    let mut removed = Vec::new();
    for entry in walk_files(classes_dir) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("class") {
            continue;
        }
        if let Some(src) = class_source_file(p) {
            if src.ends_with(".kt") {
                std::fs::remove_file(p)
                    .with_context(|| format!("failed to remove stale {}", p.display()))?;
                removed.push(p.to_path_buf());
            }
        }
    }
    Ok(removed)
}

/// Path of the stamp file recording the previous build's Kotlin source set.
pub fn kt_sources_stamp_path(target_dir: &Path) -> PathBuf {
    target_dir.join(".kt-sources")
}

/// Load the previous build's Kotlin source list, or `None` when the stamp
/// is missing (first build after clean, or no Kotlin sources previously).
pub fn load_kt_sources(target_dir: &Path) -> Option<BTreeSet<String>> {
    let path = kt_sources_stamp_path(target_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    )
}

/// Write the current Kotlin source list to the stamp file.  Pass the
/// canonical (absolute, symlink-resolved) source paths so future builds
/// compare apples-to-apples regardless of how the user invokes Curie.
pub fn write_kt_sources(target_dir: &Path, sources: &BTreeSet<String>) -> Result<()> {
    let path = kt_sources_stamp_path(target_dir);
    let body = {
        let mut s = String::with_capacity(sources.iter().map(|p| p.len() + 1).sum());
        for p in sources {
            s.push_str(p);
            s.push('\n');
        }
        s
    };
    std::fs::write(&path, body)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Canonicalise a slice of Kotlin source paths into the stamp's
/// comparison set, dropping any that fail to canonicalise (which would only
/// happen if the source vanished between source discovery and this call).
pub fn canonical_kt_set(sources: &[PathBuf]) -> BTreeSet<String> {
    sources
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // -- handcrafted class file ------------------------------------------------
    //
    // The smallest "valid enough" class to exercise the parser: a magic
    // header, two UTF8 entries (one for the attribute name "SourceFile" and
    // one for the source filename), then a single class-level SourceFile
    // attribute pointing at the second UTF8.  All the in-between counts are
    // zero so there are no fields/methods/etc. to skip.

    fn build_minimal_class(source_name: &str) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();

        // magic + version (any plausible major/minor)
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8-ish; arbitrary)

        // constant pool: 3 entries (cp_count = 4)
        //   #1 = Utf8 "SourceFile"
        //   #2 = Utf8 source_name
        //   #3 = Class (any name) — included so this_class can point at it
        //
        // Tag 1: u8 tag, u2 length, bytes
        out.extend_from_slice(&4u16.to_be_bytes());

        let sf_bytes = b"SourceFile";
        out.push(1);
        out.extend_from_slice(&(sf_bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(sf_bytes);

        let sn = source_name.as_bytes();
        out.push(1);
        out.extend_from_slice(&(sn.len() as u16).to_be_bytes());
        out.extend_from_slice(sn);

        // Tag 7 Class: name_index → #1 (re-use any utf8; doesn't matter)
        out.push(7);
        out.extend_from_slice(&1u16.to_be_bytes());

        // access_flags, this_class, super_class
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes()); // this_class → cp #3
        out.extend_from_slice(&0u16.to_be_bytes()); // super_class (0 = none)

        // interfaces_count, fields_count, methods_count, attributes_count
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());

        // single SourceFile attribute
        //   name_index → #1 ("SourceFile")
        //   length     = 2
        //   body       = u2 sourcefile_index → #2
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&2u16.to_be_bytes());

        out
    }

    fn write_class(path: &Path, source_name: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&build_minimal_class(source_name)).unwrap();
    }

    /// Like `build_minimal_class` but inserts a CONSTANT_Utf8 entry whose
    /// bytes contain `C0 80` — the JVM's *modified UTF-8* encoding for a NUL
    /// byte, which standard `std::str::from_utf8` rejects.  Real Kotlin
    /// `@Metadata` annotations stash binary data in such entries; the parser
    /// must keep walking the constant pool instead of bailing on the bad
    /// codepoint.
    fn build_class_with_modified_utf8(source_name: &str) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&52u16.to_be_bytes());

        // CP: 4 entries (cp_count = 5)
        //   #1 = Utf8 "SourceFile"
        //   #2 = Utf8 modified-UTF-8 blob (forces the parser to swallow C0 80)
        //   #3 = Utf8 source_name
        //   #4 = Class
        out.extend_from_slice(&5u16.to_be_bytes());

        let sf = b"SourceFile";
        out.push(1);
        out.extend_from_slice(&(sf.len() as u16).to_be_bytes());
        out.extend_from_slice(sf);

        // Modified-UTF-8 entry: ASCII + (C0 80) + ASCII.
        let blob: &[u8] = &[b'a', 0xC0, 0x80, b'b'];
        out.push(1);
        out.extend_from_slice(&(blob.len() as u16).to_be_bytes());
        out.extend_from_slice(blob);

        let sn = source_name.as_bytes();
        out.push(1);
        out.extend_from_slice(&(sn.len() as u16).to_be_bytes());
        out.extend_from_slice(sn);

        out.push(7);
        out.extend_from_slice(&1u16.to_be_bytes());

        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes()); // this_class → #4
        out.extend_from_slice(&0u16.to_be_bytes());

        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());

        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes()); // sourcefile → #3

        out
    }

    #[test]
    fn parses_through_modified_utf8_entries() {
        // Regression: Kotlin's @Metadata annotation puts binary data in CP
        // UTF8 entries using the JVM's modified UTF-8.  An earlier version
        // of this parser tried to decode every UTF8 entry as standard UTF-8
        // and bailed on the C0 80 NUL — meaning no Kotlin class file was
        // ever recognised as Kotlin-derived and the wipe ran but cleaned
        // nothing.
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("Mod.class");
        std::fs::write(&c, &build_class_with_modified_utf8("Greeting.kt")).unwrap();
        assert_eq!(class_source_file(&c).as_deref(), Some("Greeting.kt"));
    }

    #[test]
    fn extracts_source_file_for_kotlin() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("Foo.class");
        write_class(&c, "Foo.kt");
        assert_eq!(class_source_file(&c).as_deref(), Some("Foo.kt"));
    }

    #[test]
    fn extracts_source_file_for_java() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("Foo.class");
        write_class(&c, "Foo.java");
        assert_eq!(class_source_file(&c).as_deref(), Some("Foo.java"));
    }

    #[test]
    fn missing_file_returns_none() {
        assert!(class_source_file(Path::new("/nonexistent.class")).is_none());
    }

    #[test]
    fn malformed_file_returns_none_safely() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("garbage.class");
        std::fs::write(&c, b"not a class file at all").unwrap();
        // Must not panic and must return None on bad magic.
        assert!(class_source_file(&c).is_none());
    }

    #[test]
    fn truncated_file_returns_none_safely() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("short.class");
        // Just the magic, then EOF.
        std::fs::write(&c, &0xCAFEBABEu32.to_be_bytes()).unwrap();
        assert!(class_source_file(&c).is_none());
    }

    // -- wipe ----------------------------------------------------------------

    #[test]
    fn wipe_removes_only_kotlin_derived_classes() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");

        let kt_class = classes.join("com").join("FooKt.class");
        let kt_extra = classes.join("com").join("Bar.class");
        let java_class = classes.join("com").join("Baz.class");
        write_class(&kt_class, "Foo.kt");
        write_class(&kt_extra, "Foo.kt"); // companion class from same .kt
        write_class(&java_class, "Baz.java");

        let removed = wipe_kotlin_derived_classes(&classes).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!kt_class.exists());
        assert!(!kt_extra.exists());
        assert!(java_class.exists(), "Java-derived class must survive wipe");
    }

    #[test]
    fn wipe_no_classes_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(wipe_kotlin_derived_classes(&dir.path().join("ghost"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn wipe_ignores_non_class_files() {
        let dir = tempfile::tempdir().unwrap();
        let classes = dir.path().join("classes");
        std::fs::create_dir_all(&classes).unwrap();
        // A resource file (e.g. META-INF/main.kotlin_module) must be left alone
        // even though kotlinc wrote it.
        let res = classes.join("META-INF").join("main.kotlin_module");
        std::fs::create_dir_all(res.parent().unwrap()).unwrap();
        std::fs::write(&res, b"resource").unwrap();
        assert!(wipe_kotlin_derived_classes(&classes).unwrap().is_empty());
        assert!(res.exists());
    }

    // -- source-set tracking -------------------------------------------------

    #[test]
    fn kt_sources_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut set = BTreeSet::new();
        set.insert("/a/Foo.kt".to_string());
        set.insert("/a/Bar.kt".to_string());
        write_kt_sources(dir.path(), &set).unwrap();
        let read = load_kt_sources(dir.path()).unwrap();
        assert_eq!(read, set);
    }

    #[test]
    fn kt_sources_load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_kt_sources(dir.path()).is_none());
    }

    #[test]
    fn kt_sources_ignores_blank_lines_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            kt_sources_stamp_path(dir.path()),
            "\n  /a/Foo.kt  \n\n/a/Bar.kt\n",
        )
        .unwrap();
        let read = load_kt_sources(dir.path()).unwrap();
        let mut expected = BTreeSet::new();
        expected.insert("/a/Foo.kt".to_string());
        expected.insert("/a/Bar.kt".to_string());
        assert_eq!(read, expected);
    }
}
