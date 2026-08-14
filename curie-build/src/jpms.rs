//! Java Platform Module System (JPMS) support.
//!
//! This module handles everything needed to decide how to split a dependency
//! list between `--module-path` and `-classpath` when the project contains a
//! `module-info.java`.
//!
//! Three distinct identities a JAR can carry:
//!
//! * **Explicit module** — the JAR bundles a compiled `module-info.class`
//!   whose constant pool contains a CONSTANT_Module entry naming the module.
//! * **Automatic module (manifest)** — no `module-info.class`, but the
//!   `MANIFEST.MF` entry `Automatic-Module-Name` is present.  The JDK uses
//!   that value verbatim as the module name.
//! * **Automatic module (filename)** — no `module-info.class` and no manifest
//!   entry.  The JDK derives the module name from the JAR filename with the
//!   algorithm implemented in [`derive_automatic_module_name`].
//! * **Underivable** — the filename is so short or malformed that the
//!   algorithm yields nothing usable.
//!
//! # Caching
//!
//! Reading JARs from `~/.m2` on every build would be slow.  Module identities
//! are cached in `<target_dir>/.jpms-modules.toml` and recomputed only when
//! the JAR's mtime changes.

use crate::jar::get_manifest_header;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use zip::ZipArchive;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A `requires` directive parsed from `module-info.java`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requires {
    pub module_name: String,
    pub is_static: bool,
    pub is_transitive: bool,
}

/// Parsed representation of a `module-info.java` source file.
#[derive(Debug, Clone)]
pub struct ParsedModuleInfo {
    pub module_name: String,
    pub requires: Vec<Requires>,
}

/// How a JAR declares its module identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleIdentity {
    /// Explicit module: compiled `module-info.class` found inside the JAR.
    Explicit(String),
    /// Automatic module named via `Automatic-Module-Name` in `MANIFEST.MF`.
    AutomaticManifest(String),
    /// Automatic module whose name was derived from the JAR filename.
    AutomaticFilename(String),
    /// No reliable module name can be determined (very short/invalid filename,
    /// or contains only version-suffix characters after stripping).
    Underivable,
}

impl ModuleIdentity {
    /// The effective module name, if any.
    pub fn module_name(&self) -> Option<&str> {
        match self {
            ModuleIdentity::Explicit(n)
            | ModuleIdentity::AutomaticManifest(n)
            | ModuleIdentity::AutomaticFilename(n) => Some(n),
            ModuleIdentity::Underivable => None,
        }
    }
}

/// Result of splitting dependency JARs between `--module-path` and `-cp`.
#[derive(Debug, Default)]
pub struct ModuleSplit {
    /// JARs that must be placed on `--module-path` (explicit or automatic
    /// modules that are explicitly `requires`d by the project's
    /// `module-info.java`).
    pub module_path: Vec<PathBuf>,
    /// JARs that remain on the traditional class-path.
    pub classpath: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// On-disk cache types
// ---------------------------------------------------------------------------

/// Serialisable record of one JAR's cached module identity.
#[derive(Debug, Serialize, Deserialize)]
struct CachedIdentity {
    /// JAR mtime in seconds since the Unix epoch at cache time, or `None` when
    /// mtime could not be determined.  Entries with `None` never produce a
    /// cache hit — unknown mtime is always treated as stale.
    mtime_secs: Option<u64>,
    /// One of: `"explicit"`, `"automatic-manifest"`, `"automatic-filename"`,
    /// `"underivable"`.
    kind: String,
    /// Module name — present for all kinds except `"underivable"`.
    name: Option<String>,
}

/// Top-level structure of `.jpms-modules.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ModuleCache {
    /// Key is the absolute JAR path as a string.
    #[serde(default)]
    entries: BTreeMap<String, CachedIdentity>,
}

impl ModuleCache {
    fn load(cache_path: &Path) -> Self {
        if !cache_path.exists() {
            return Self::default();
        }
        std::fs::read_to_string(cache_path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, cache_path: &Path) -> Result<()> {
        let content = toml::to_string(self).context("failed to serialise module cache")?;
        std::fs::write(cache_path, content)
            .with_context(|| format!("failed to write module cache {}", cache_path.display()))
    }

    /// Return a cached identity only when both the stored and current mtimes
    /// are known (`Some`) and equal.  `None` current mtime (stat failure) is
    /// always a miss so a replaced JAR is re-inspected.
    fn get(&self, jar_path: &Path, current_mtime: Option<u64>) -> Option<ModuleIdentity> {
        let key = jar_path.to_string_lossy();
        let entry = self.entries.get(key.as_ref())?;
        match (entry.mtime_secs, current_mtime) {
            (Some(stored), Some(current)) if stored == current => Some(decode_identity(entry)),
            _ => None,
        }
    }

    fn insert(&mut self, jar_path: &Path, mtime_secs: Option<u64>, identity: &ModuleIdentity) {
        let (kind, name) = encode_identity(identity);
        let key = jar_path.to_string_lossy().into_owned();
        self.entries.insert(
            key,
            CachedIdentity {
                mtime_secs,
                kind,
                name,
            },
        );
    }
}

fn encode_identity(identity: &ModuleIdentity) -> (String, Option<String>) {
    match identity {
        ModuleIdentity::Explicit(n) => ("explicit".into(), Some(n.clone())),
        ModuleIdentity::AutomaticManifest(n) => ("automatic-manifest".into(), Some(n.clone())),
        ModuleIdentity::AutomaticFilename(n) => ("automatic-filename".into(), Some(n.clone())),
        ModuleIdentity::Underivable => ("underivable".into(), None),
    }
}

fn decode_identity(entry: &CachedIdentity) -> ModuleIdentity {
    match entry.kind.as_str() {
        "explicit" => ModuleIdentity::Explicit(entry.name.clone().unwrap_or_default()),
        "automatic-manifest" => {
            ModuleIdentity::AutomaticManifest(entry.name.clone().unwrap_or_default())
        }
        "automatic-filename" => {
            ModuleIdentity::AutomaticFilename(entry.name.clone().unwrap_or_default())
        }
        _ => ModuleIdentity::Underivable,
    }
}

// ---------------------------------------------------------------------------
// module-info.java parser
// ---------------------------------------------------------------------------

/// Parse a `module-info.java` source text and return the module name plus
/// every `requires` directive.
///
/// Comments (both `// …` and `/* … */`) are stripped before parsing so that
/// identifiers inside comments never confuse the simple keyword scanner.
///
/// The parser is intentionally minimal — it only needs to handle valid source
/// that `javac` would accept — and does not attempt full Java grammar.
pub fn parse_module_info_java(content: &str) -> Result<ParsedModuleInfo> {
    let stripped = strip_java_comments(content);
    let module_name = extract_module_name(&stripped)?;
    let requires = extract_requires_directives(&stripped);
    Ok(ParsedModuleInfo {
        module_name,
        requires,
    })
}

/// Remove `// …` line comments and `/* … */` block comments from Java source.
fn strip_java_comments(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Skip to end of line.
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Skip block comment.
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2; // consume `*/`
        } else if bytes[i] == b'"' {
            // String literal — copy verbatim so keyword scanning doesn't see
            // `requires` inside string content (unusual in module-info, but safe).
            result.push('"');
            i += 1;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    result.push('\\');
                    i += 1;
                }
                if i < len {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < len {
                result.push('"');
                i += 1;
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Extract the module name from comment-stripped `module-info.java` text.
///
/// Handles `open module foo.bar { … }` and `module foo.bar { … }`.
fn extract_module_name(source: &str) -> Result<String> {
    let tokens = tokenise(source);
    let mut iter = tokens.iter().peekable();
    while let Some(tok) = iter.next() {
        // Skip `open` keyword that may precede `module`, then fall through
        // to the `module` check on the next iteration.
        if tok == "open" {
            continue;
        }
        if tok == "module" {
            if let Some(name) = iter.next() {
                // Skip over punctuation tokens like `{` that might be caught
                // if the module keyword appears at an unusual position.
                if name != "{" && name != ";" {
                    return Ok(name.clone());
                }
            }
        }
    }
    bail!("could not find module name in module-info.java")
}

/// Extract all `requires` directives from comment-stripped source.
fn extract_requires_directives(source: &str) -> Vec<Requires> {
    let tokens = tokenise(source);
    let mut requires_list = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        if tokens[i] == "requires" {
            let mut is_static = false;
            let mut is_transitive = false;

            // Consume optional modifiers.
            loop {
                if i + 1 < tokens.len() && tokens[i + 1] == "static" {
                    is_static = true;
                    i += 1;
                } else if i + 1 < tokens.len() && tokens[i + 1] == "transitive" {
                    is_transitive = true;
                    i += 1;
                } else {
                    break;
                }
            }

            // Next token is the required module name.
            if i + 1 < tokens.len() {
                let module_name = tokens[i + 1].clone();
                // Skip the semicolon or anything that isn't an identifier.
                if !module_name.is_empty() && module_name != ";" {
                    requires_list.push(Requires {
                        module_name,
                        is_static,
                        is_transitive,
                    });
                }
                i += 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    requires_list
}

/// Split `source` into identifier/keyword tokens, skipping punctuation.
///
/// Returns only sequences of characters that are valid in Java module names
/// (letters, digits, dots, underscores, hyphens).  Semicolons and braces are
/// included as single-character tokens so the directive parser can detect the
/// end of a name.
fn tokenise(source: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in source.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => current.push(ch),
            ';' | '{' | '}' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            _ => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Automatic module name derivation (JDK algorithm)
// ---------------------------------------------------------------------------

/// Derive an automatic module name from a JAR filename, using the same
/// algorithm that the JDK's module system applies.
///
/// Steps (from the JDK spec):
/// 1. Strip the `.jar` extension.
/// 2. Strip the version suffix: the last hyphen followed by a digit (and
///    everything after it).
/// 3. Replace every sequence of non-alphanumeric characters with a single
///    dot.
/// 4. Strip leading and trailing dots.
/// 5. Collapse consecutive dots into one.
///
/// Returns `None` if the result would be empty.
pub fn derive_automatic_module_name(filename: &str) -> Option<String> {
    let without_jar = strip_jar_extension(filename);
    let without_version = strip_version_suffix(without_jar);
    let with_dots = replace_non_alphanumeric_with_dot(without_version);
    let collapsed = collapse_and_trim_dots(&with_dots);

    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// Remove the `.jar` suffix (case-insensitive) from a filename.
fn strip_jar_extension(filename: &str) -> &str {
    if filename.to_ascii_lowercase().ends_with(".jar") {
        &filename[..filename.len() - 4]
    } else {
        filename
    }
}

/// Strip the version suffix: the last occurrence of `-<digit>` and everything
/// that follows it.  For example, `guava-32.1.3-jre` → `guava`.
fn strip_version_suffix(name: &str) -> &str {
    // Walk backwards looking for `- <digit>`.
    let bytes = name.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i].is_ascii_digit() && bytes[i - 1] == b'-' {
            return &name[..i - 1];
        }
    }
    name
}

/// Replace every run of characters that are not ASCII alphanumeric with a
/// single `.`.
fn replace_non_alphanumeric_with_dot(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut in_non_alnum = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            in_non_alnum = false;
            result.push(ch);
        } else {
            if !in_non_alnum {
                result.push('.');
            }
            in_non_alnum = true;
        }
    }
    result
}

/// Collapse consecutive dots into one and strip leading/trailing dots.
fn collapse_and_trim_dots(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut prev_dot = false;

    for ch in name.chars() {
        if ch == '.' {
            if !prev_dot && !result.is_empty() {
                result.push('.');
            }
            prev_dot = true;
        } else {
            prev_dot = false;
            result.push(ch);
        }
    }
    // Trim trailing dot.
    if result.ends_with('.') {
        result.pop();
    }
    result
}

// ---------------------------------------------------------------------------
// Binary class file — read module name from constant pool
// ---------------------------------------------------------------------------

/// Attempt to read the module name from a compiled `module-info.class` binary.
///
/// The class file constant pool may contain entries with tag **19**
/// (`CONSTANT_Module`), each of which points to a `CONSTANT_Utf8` entry.
/// The first `CONSTANT_Module` entry encountered is taken as the module name.
///
/// Returns `None` if the file is not a valid class file or contains no
/// `CONSTANT_Module` entry.
pub fn read_module_info_class(data: &[u8]) -> Option<String> {
    // Class file magic: 0xCAFEBABE
    if data.len() < 8 || &data[0..4] != b"\xCA\xFE\xBA\xBE" {
        return None;
    }

    let (pool, _) = parse_constant_pool(data)?;
    find_module_name_in_pool(&pool)
}

/// Read the list of module names from `requires` directives in a `module-info.class`.
///
/// Used by the BFS transitive expansion in [`compute_module_path_split`] to pull
/// the deps of deps onto `--module-path`.  Returns an empty vec on any parse
/// error or if the data is not a module-info class.
pub fn read_module_requires_from_class(data: &[u8]) -> Vec<String> {
    read_module_requires_inner(data).unwrap_or_default()
}

fn read_module_requires_inner(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 8 || &data[0..4] != b"\xCA\xFE\xBA\xBE" {
        return None;
    }
    let (pool, mut offset) = parse_constant_pool(data)?;

    // Skip access_flags(2) + this_class(2) + super_class(2).
    offset += 6;

    // interfaces_count (u2) — 0 for module-info.class.
    let interfaces_count = read_u16(data, offset)? as usize;
    offset += 2 + interfaces_count * 2;

    // fields_count — 0 for module-info.class.
    let fields_count = read_u16(data, offset)? as usize;
    offset += 2;
    for _ in 0..fields_count {
        offset = skip_member(data, offset)?;
    }

    // methods_count — 0 for module-info.class.
    let methods_count = read_u16(data, offset)? as usize;
    offset += 2;
    for _ in 0..methods_count {
        offset = skip_member(data, offset)?;
    }

    // Attributes: find the "Module" attribute.
    let attr_count = read_u16(data, offset)? as usize;
    offset += 2;
    for _ in 0..attr_count {
        let name_idx = read_u16(data, offset)? as usize;
        offset += 2;
        let attr_len = read_u32(data, offset)? as usize;
        offset += 4;
        let attr_start = offset;
        offset += attr_len;

        let is_module_attr =
            matches!(pool.get(name_idx), Some(ConstantPoolEntry::Utf8(s)) if s == "Module");
        if is_module_attr {
            return parse_module_attr_requires(&pool, data, attr_start);
        }
    }
    None
}

/// Parse the `requires` entries from a Module attribute body.
///
/// Module attribute layout (JVMS §4.7.25):
/// ```text
/// u2  module_name_index
/// u2  module_flags
/// u2  module_version_index
/// u2  requires_count
/// { u2 requires_index; u2 requires_flags; u2 requires_version_index }[requires_count]
/// ...
/// ```
fn parse_module_attr_requires(
    pool: &[ConstantPoolEntry],
    data: &[u8],
    start: usize,
) -> Option<Vec<String>> {
    // Skip module_name_index(2) + module_flags(2) + module_version_index(2).
    let mut offset = start + 6;
    let requires_count = read_u16(data, offset)? as usize;
    offset += 2;

    let mut result = Vec::new();
    for _ in 0..requires_count {
        let req_idx = read_u16(data, offset)? as usize;
        offset += 6; // requires_index(2) + requires_flags(2) + requires_version_index(2)
        if let Some(ConstantPoolEntry::Module { name_index }) = pool.get(req_idx) {
            if let Some(ConstantPoolEntry::Utf8(s)) = pool.get(*name_index as usize) {
                result.push(s.replace('/', "."));
            }
        }
    }
    Some(result)
}

/// Skip over a single field_info or method_info and return the new offset.
fn skip_member(data: &[u8], mut offset: usize) -> Option<usize> {
    // access_flags(2) + name_index(2) + descriptor_index(2)
    offset += 6;
    let attr_count = read_u16(data, offset)? as usize;
    offset += 2;
    for _ in 0..attr_count {
        // attribute_name_index(2) + attribute_length(4)
        offset += 2;
        let attr_len = read_u32(data, offset)? as usize;
        offset += 4 + attr_len;
    }
    Some(offset)
}

/// A minimal representation of a constant pool entry.
#[derive(Debug)]
enum ConstantPoolEntry {
    Utf8(String),
    /// Tag 19 — CONSTANT_Module — contains a name_index into the pool.
    Module {
        name_index: u16,
    },
    /// All other entries that take exactly 4 bytes in the pool item body.
    FourBytes,
    /// Long/Double: takes two slots.
    EightBytesWide,
    /// Entries we don't need to decode but that carry a u16 index.
    TwoIndexes,
    /// Entries that carry exactly one u16 index.
    OneIndex,
    /// Entries that carry one u8.
    OneByte,
    /// Placeholder for the second slot of Long/Double.
    Placeholder,
}

/// Parse the constant pool from a class file byte slice.
///
/// Returns `(pool, offset_after_pool)` or `None` on any parse error.
fn parse_constant_pool(data: &[u8]) -> Option<(Vec<ConstantPoolEntry>, usize)> {
    // Offset 8: constant_pool_count (u16).
    if data.len() < 10 {
        return None;
    }
    let pool_count = u16::from_be_bytes([data[8], data[9]]) as usize;
    let mut pool: Vec<ConstantPoolEntry> = Vec::with_capacity(pool_count);
    // Index 0 is not used; the pool is 1-indexed.
    pool.push(ConstantPoolEntry::Placeholder);

    let mut offset = 10usize;

    for _ in 1..pool_count {
        if offset >= data.len() {
            return None;
        }
        let tag = data[offset];
        offset += 1;

        let entry = match tag {
            1 => {
                // CONSTANT_Utf8: u16 length + bytes.
                let len = read_u16(data, offset)? as usize;
                offset += 2;
                if offset + len > data.len() {
                    return None;
                }
                let s = String::from_utf8_lossy(&data[offset..offset + len]).into_owned();
                offset += len;
                ConstantPoolEntry::Utf8(s)
            }
            3 | 4 => {
                // Integer / Float: 4 bytes.
                offset += 4;
                ConstantPoolEntry::FourBytes
            }
            5 | 6 => {
                // Long / Double: 8 bytes, occupies two pool slots.
                offset += 8;
                pool.push(ConstantPoolEntry::Placeholder);
                ConstantPoolEntry::EightBytesWide
            }
            7 | 8 | 16 | 19 | 20 => {
                // Class / String / MethodType / Module / Package: one u16 name_index.
                let name_index = read_u16(data, offset)?;
                offset += 2;
                if tag == 19 {
                    ConstantPoolEntry::Module { name_index }
                } else {
                    ConstantPoolEntry::OneIndex
                }
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                // Fieldref / Methodref / InterfaceMethodref / NameAndType /
                // Dynamic / InvokeDynamic: two u16 indexes.
                offset += 4;
                ConstantPoolEntry::TwoIndexes
            }
            15 => {
                // MethodHandle: one u8 + one u16.
                offset += 3;
                ConstantPoolEntry::OneByte
            }
            _ => {
                // Unknown tag — cannot continue safely.
                return None;
            }
        };
        pool.push(entry);
    }

    Some((pool, offset))
}

/// Read a big-endian u16 from `data` at `offset`.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 1 >= data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

/// Read a big-endian u32 from `data` at `offset`.
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 3 >= data.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

/// Find the first CONSTANT_Module entry in the pool and return its UTF-8 name.
fn find_module_name_in_pool(pool: &[ConstantPoolEntry]) -> Option<String> {
    for entry in pool {
        if let ConstantPoolEntry::Module { name_index } = entry {
            let idx = *name_index as usize;
            if let Some(ConstantPoolEntry::Utf8(s)) = pool.get(idx) {
                // Module names in the class file use `/` — convert to dot form.
                return Some(s.replace('/', "."));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JAR module identity
// ---------------------------------------------------------------------------

/// Determine the module identity of a single JAR file.
///
/// Checks (in priority order):
/// 1. `module-info.class` in the root of the JAR (explicit module).
/// 2. `Automatic-Module-Name` header in `META-INF/MANIFEST.MF`.
/// 3. Filename derivation.
pub fn read_jar_module_identity(jar_path: &Path) -> Result<ModuleIdentity> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("cannot open JAR {}", jar_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("cannot read ZIP archive {}", jar_path.display()))?;

    // 1. Check for explicit module-info.class.
    if let Some(name) = try_read_explicit_module(&mut archive)? {
        return Ok(ModuleIdentity::Explicit(name));
    }

    // 2. Check MANIFEST.MF for Automatic-Module-Name.
    if let Some(name) = try_read_manifest_module_name(&mut archive)? {
        return Ok(ModuleIdentity::AutomaticManifest(name));
    }

    // 3. Derive from filename.
    let filename = jar_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match derive_automatic_module_name(&filename) {
        Some(name) => Ok(ModuleIdentity::AutomaticFilename(name)),
        None => Ok(ModuleIdentity::Underivable),
    }
}

/// Extract `module-info.class` bytes from a ZIP archive.
///
/// Checks the root first, then multi-release paths (`META-INF/versions/<N>/module-info.class`)
/// in ascending version order.  Many libraries (e.g. jackson-databind) ship as multi-release
/// JARs where the root entry is absent and the descriptor lives under `META-INF/versions/9/`.
fn read_module_info_bytes(archive: &mut ZipArchive<std::fs::File>) -> Result<Option<Vec<u8>>> {
    // 1. Check root first.
    match archive.by_name("module-info.class") {
        Ok(mut entry) => {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .context("failed to read module-info.class")?;
            return Ok(Some(buf));
        }
        Err(zip::result::ZipError::FileNotFound) => {}
        Err(e) => return Err(e).context("error reading module-info.class from JAR"),
    };

    // 2. Scan multi-release versioned directories.
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_owned()))
        .collect();

    let mut versioned: Vec<(u32, String)> = names
        .into_iter()
        .filter_map(|name| {
            let rest = name.strip_prefix("META-INF/versions/")?;
            let slash = rest.find('/')?;
            let ver: u32 = rest[..slash].parse().ok()?;
            if &rest[slash + 1..] == "module-info.class" {
                Some((ver, name))
            } else {
                None
            }
        })
        .collect();
    versioned.sort_by_key(|(v, _)| *v);

    for (_ver, path) in versioned {
        match archive.by_name(&path) {
            Ok(mut entry) => {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .with_context(|| format!("failed to read {path}"))?;
                return Ok(Some(buf));
            }
            Err(_) => continue,
        }
    }

    Ok(None)
}

/// Try to read `module-info.class` from a ZIP archive and return the module name.
fn try_read_explicit_module(archive: &mut ZipArchive<std::fs::File>) -> Result<Option<String>> {
    match read_module_info_bytes(archive)? {
        Some(data) => Ok(read_module_info_class(&data)),
        None => Ok(None),
    }
}

/// Read the `requires` list from the `module-info.class` in a JAR.
///
/// Used during the BFS transitive expansion in [`compute_module_path_split`] to
/// pull the module-level dependencies of a directly required module onto the
/// `--module-path` as well.  Returns an empty vec for non-modular JARs or on
/// parse error.
pub fn read_jar_module_requires(jar_path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(jar_path)
        .with_context(|| format!("cannot open JAR {}", jar_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("cannot read ZIP archive {}", jar_path.display()))?;
    Ok(read_module_info_bytes(&mut archive)?
        .map(|d| read_module_requires_from_class(&d))
        .unwrap_or_default())
}

/// Try to read `Automatic-Module-Name` from `META-INF/MANIFEST.MF`.
fn try_read_manifest_module_name(
    archive: &mut ZipArchive<std::fs::File>,
) -> Result<Option<String>> {
    let manifest_text = match archive.by_name("META-INF/MANIFEST.MF") {
        Ok(mut entry) => {
            let mut buf = String::new();
            entry
                .read_to_string(&mut buf)
                .context("failed to read MANIFEST.MF")?;
            buf
        }
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e).context("error reading MANIFEST.MF from JAR"),
    };

    Ok(parse_automatic_module_name_from_manifest(&manifest_text))
}

/// Parse the value of `Automatic-Module-Name` from MANIFEST.MF text.
///
/// Delegates to the crate-wide `get_manifest_header` so that folded
/// (continuation-line) headers are handled correctly for all manifest
/// attributes, not just Class-Path.
fn parse_automatic_module_name_from_manifest(manifest: &str) -> Option<String> {
    get_manifest_header(manifest, "Automatic-Module-Name")
}

// ---------------------------------------------------------------------------
// Project-level helpers
// ---------------------------------------------------------------------------

/// Search for `module-info.java` under the given source roots.
///
/// Returns the path to the first one found, or `None` if the project does not
/// use JPMS.
pub fn find_module_info_java(src_roots: &[PathBuf]) -> Option<PathBuf> {
    for root in src_roots {
        let candidate = root.join("module-info.java");
        if candidate.exists() {
            return Some(candidate);
        }
        // Also search recursively under the root (Maven layout places
        // module-info.java directly in src/main/java/, not in a package dir).
        if let Ok(walker) = std::fs::read_dir(root) {
            for entry in walker.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file()
                    && path
                        .file_name()
                        .map(|n| n == "module-info.java")
                        .unwrap_or(false)
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Module path / classpath split
// ---------------------------------------------------------------------------

/// Compute which dependency JARs belong on `--module-path` vs `-cp`.
///
/// Starting from the project's own `requires` directives, performs a BFS over
/// the dependency module graph: when an explicit-module JAR is placed on the
/// module-path its own `requires` are read from the binary descriptor and added
/// to the queue.  Platform modules (`java.*`, `jdk.*`) are always available and
/// are skipped.
///
/// The module-identity cache in `<target_dir>/.jpms-modules.toml` avoids
/// re-reading JARs on every build.
pub fn compute_module_path_split(
    module_info: &ParsedModuleInfo,
    dep_jars: &[PathBuf],
    target_dir: &Path,
) -> Result<ModuleSplit> {
    let cache_path = target_dir.join(".jpms-modules.toml");
    let mut cache = ModuleCache::load(&cache_path);

    // Resolve identity for every dep JAR up-front.
    let identities: Vec<(PathBuf, ModuleIdentity)> = dep_jars
        .iter()
        .map(|jar| resolve_jar_identity(jar, &mut cache).map(|id| (jar.clone(), id)))
        .collect::<Result<_>>()?;

    let _ = cache.save(&cache_path);

    // Build a name → index map for fast lookup.
    let mut name_to_idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, (_, identity)) in identities.iter().enumerate() {
        if let Some(name) = identity.module_name() {
            name_to_idx.insert(name, i);
        }
    }

    // BFS: start from the project's direct requires and expand transitively.
    let mut on_module_path = vec![false; identities.len()];
    let mut queue: std::collections::VecDeque<String> = module_info
        .requires
        .iter()
        .map(|r| r.module_name.clone())
        .collect();

    while let Some(req_name) = queue.pop_front() {
        // Platform modules are always on the module path — no dep JAR needed.
        if is_platform_module(&req_name) {
            continue;
        }
        let Some(&idx) = name_to_idx.get(req_name.as_str()) else {
            continue;
        };
        if on_module_path[idx] {
            continue;
        }
        on_module_path[idx] = true;

        // For explicit modules expand their requires into the queue so that
        // javac can resolve the full module graph.
        if matches!(&identities[idx].1, ModuleIdentity::Explicit(_)) {
            let requires = read_jar_module_requires(&identities[idx].0).unwrap_or_default();
            for r in requires {
                if !is_platform_module(&r) {
                    queue.push_back(r);
                }
            }
        }
    }

    let mut split = ModuleSplit::default();
    for (i, (jar, _)) in identities.into_iter().enumerate() {
        if on_module_path[i] {
            split.module_path.push(jar);
        } else {
            split.classpath.push(jar);
        }
    }
    Ok(split)
}

/// Returns `true` for `java.*` and `jdk.*` module names that are always
/// provided by the running JDK and never correspond to a dep JAR.
fn is_platform_module(name: &str) -> bool {
    name.starts_with("java.") || name.starts_with("jdk.")
}

/// Resolve the identity of a JAR, using the cache when available.
fn resolve_jar_identity(jar: &Path, cache: &mut ModuleCache) -> Result<ModuleIdentity> {
    let mtime_secs = jar_mtime_secs(jar);
    if let Some(cached) = cache.get(jar, mtime_secs) {
        return Ok(cached);
    }
    let identity = read_jar_module_identity(jar)?;
    // Only cache when mtime is known.  Inserting `None` would never hit
    // usefully, and historically storing 0 for "error" froze stale identities
    // when subsequent stats also failed (bug #36).
    if mtime_secs.is_some() {
        cache.insert(jar, mtime_secs, &identity);
    }
    Ok(identity)
}

/// Return the JAR mtime as seconds since the Unix epoch, or `None` if it
/// cannot be determined (missing file, unsupported `modified()`, etc.).
fn jar_mtime_secs(jar: &Path) -> Option<u64> {
    std::fs::metadata(jar)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_module_info_java
    // -----------------------------------------------------------------------

    #[test]
    fn parse_simple_module() {
        let src = r#"
            module com.example.app {
                requires java.base;
                requires com.example.lib;
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.app");
        assert_eq!(info.requires.len(), 2);
        assert_eq!(info.requires[0].module_name, "java.base");
        assert!(!info.requires[0].is_static);
        assert!(!info.requires[0].is_transitive);
        assert_eq!(info.requires[1].module_name, "com.example.lib");
    }

    #[test]
    fn parse_open_module() {
        let src = r#"
            open module com.example.open {
                requires transitive com.example.api;
                requires static com.example.optional;
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.open");
        assert_eq!(info.requires.len(), 2);
        assert!(info.requires[0].is_transitive);
        assert!(!info.requires[0].is_static);
        assert!(info.requires[1].is_static);
        assert!(!info.requires[1].is_transitive);
    }

    #[test]
    fn parse_module_with_line_comments() {
        let src = r#"
            // This is a comment
            module com.example.commented {
                // Another comment
                requires java.logging; // inline comment
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.commented");
        assert_eq!(info.requires.len(), 1);
        assert_eq!(info.requires[0].module_name, "java.logging");
    }

    #[test]
    fn parse_module_with_block_comments() {
        let src = r#"
            /* File header */
            module /* unusual but valid */ com.example.block {
                requires /* required */ java.sql;
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.block");
        assert_eq!(info.requires.len(), 1);
        assert_eq!(info.requires[0].module_name, "java.sql");
    }

    #[test]
    fn parse_module_with_exports_and_opens() {
        let src = r#"
            module com.example.full {
                requires java.base;
                exports com.example.full.api;
                opens com.example.full.impl;
                provides com.example.spi.Foo with com.example.full.FooImpl;
                uses com.example.spi.Bar;
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.full");
        assert_eq!(info.requires.len(), 1);
        assert_eq!(info.requires[0].module_name, "java.base");
    }

    #[test]
    fn parse_module_no_requires() {
        let src = "module com.example.standalone {}";
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.module_name, "com.example.standalone");
        assert!(info.requires.is_empty());
    }

    #[test]
    fn parse_module_missing_keyword_returns_error() {
        let result = parse_module_info_java("class Foo {}");
        assert!(result.is_err());
    }

    #[test]
    fn parse_static_transitive_combined() {
        // `requires static transitive` is unusual but syntactically legal.
        let src = r#"
            module m {
                requires static transitive some.module;
            }
        "#;
        let info = parse_module_info_java(src).unwrap();
        assert_eq!(info.requires.len(), 1);
        assert!(info.requires[0].is_static);
        assert!(info.requires[0].is_transitive);
    }

    // -----------------------------------------------------------------------
    // derive_automatic_module_name
    // -----------------------------------------------------------------------

    #[test]
    fn derive_simple_name() {
        assert_eq!(
            derive_automatic_module_name("guava-32.1.3-jre.jar"),
            Some("guava".to_string())
        );
    }

    #[test]
    fn derive_no_version_suffix() {
        assert_eq!(
            derive_automatic_module_name("mylib.jar"),
            Some("mylib".to_string())
        );
    }

    #[test]
    fn derive_replaces_hyphens_with_dots() {
        assert_eq!(
            derive_automatic_module_name("my-library.jar"),
            Some("my.library".to_string())
        );
    }

    #[test]
    fn derive_strips_version_and_normalises() {
        // jackson-databind-2.15.3.jar → jackson.databind
        assert_eq!(
            derive_automatic_module_name("jackson-databind-2.15.3.jar"),
            Some("jackson.databind".to_string())
        );
    }

    #[test]
    fn derive_handles_classifier() {
        // slf4j-api-2.0.9.jar → slf4j.api
        assert_eq!(
            derive_automatic_module_name("slf4j-api-2.0.9.jar"),
            Some("slf4j.api".to_string())
        );
    }

    #[test]
    fn derive_collapses_consecutive_dots() {
        // some...lib.jar → some.lib
        assert_eq!(
            derive_automatic_module_name("some...lib.jar"),
            Some("some.lib".to_string())
        );
    }

    #[test]
    fn derive_strips_leading_and_trailing_dots() {
        // .hidden-1.0.jar → hidden  (leading dot stripped from filename stem)
        let result = derive_automatic_module_name(".hidden-1.0.jar").unwrap();
        assert!(!result.starts_with('.'));
        assert!(!result.ends_with('.'));
    }

    #[test]
    fn derive_returns_none_for_empty_result() {
        // A filename that reduces to nothing should return None.
        assert_eq!(derive_automatic_module_name(".jar"), None);
    }

    #[test]
    fn derive_plain_alphanumeric_unchanged() {
        assert_eq!(
            derive_automatic_module_name("commons.jar"),
            Some("commons".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // strip_java_comments
    // -----------------------------------------------------------------------

    #[test]
    fn strip_line_comment() {
        let input = "module foo { // comment\n}";
        let result = strip_java_comments(input);
        assert!(!result.contains("comment"));
        assert!(result.contains("module foo"));
    }

    #[test]
    fn strip_block_comment() {
        let input = "module /* block */ foo {}";
        let result = strip_java_comments(input);
        assert!(!result.contains("block"));
        assert!(result.contains("foo"));
    }

    #[test]
    fn strip_nested_adjacent_block_comment() {
        let input = "/* a *//* b */ module foo {}";
        let result = strip_java_comments(input);
        assert!(result.contains("module foo"));
        assert!(!result.contains("/* b */"));
    }

    // -----------------------------------------------------------------------
    // read_module_info_class (binary class file)
    // -----------------------------------------------------------------------

    /// Build a minimal synthetic `module-info.class` that contains:
    /// - Magic: 0xCAFEBABE
    /// - Minor/Major version (unused by our parser)
    /// - A constant pool with one Utf8 entry and one Module entry
    fn make_synthetic_module_class(module_name: &str) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();

        // Magic
        bytes.extend_from_slice(b"\xCA\xFE\xBA\xBE");
        // Minor version
        bytes.extend_from_slice(&0u16.to_be_bytes());
        // Major version (Java 9 = 53)
        bytes.extend_from_slice(&53u16.to_be_bytes());

        // Constant pool count = 3 (pool indices 1 and 2 used)
        // Index 1: Utf8 (the module name)
        // Index 2: Module pointing to index 1
        bytes.extend_from_slice(&3u16.to_be_bytes());

        // Entry 1: CONSTANT_Utf8 (tag = 1)
        let name_bytes = module_name.as_bytes();
        bytes.push(1u8);
        bytes.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        bytes.extend_from_slice(name_bytes);

        // Entry 2: CONSTANT_Module (tag = 19), name_index = 1
        bytes.push(19u8);
        bytes.extend_from_slice(&1u16.to_be_bytes());

        bytes
    }

    #[test]
    fn read_module_info_class_finds_module_name() {
        let data = make_synthetic_module_class("com.example.mymodule");
        let name = read_module_info_class(&data);
        assert_eq!(name, Some("com.example.mymodule".to_string()));
    }

    #[test]
    fn read_module_info_class_invalid_magic_returns_none() {
        let data = b"\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(read_module_info_class(data), None);
    }

    #[test]
    fn read_module_info_class_too_short_returns_none() {
        assert_eq!(read_module_info_class(b"\xCA\xFE"), None);
    }

    #[test]
    fn read_module_info_class_no_module_entry_returns_none() {
        // Valid class file header but pool has no CONSTANT_Module entry.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"\xCA\xFE\xBA\xBE");
        bytes.extend_from_slice(&0u16.to_be_bytes()); // minor
        bytes.extend_from_slice(&53u16.to_be_bytes()); // major
        bytes.extend_from_slice(&2u16.to_be_bytes()); // pool count = 2 (one entry)
                                                      // Entry 1: CONSTANT_Utf8 "hello"
        bytes.push(1u8);
        bytes.extend_from_slice(&5u16.to_be_bytes());
        bytes.extend_from_slice(b"hello");
        assert_eq!(read_module_info_class(&bytes), None);
    }

    // -----------------------------------------------------------------------
    // parse_automatic_module_name_from_manifest
    // -----------------------------------------------------------------------

    #[test]
    fn manifest_with_automatic_module_name() {
        let manifest = "Manifest-Version: 1.0\nAutomatic-Module-Name: com.example.lib\n";
        assert_eq!(
            parse_automatic_module_name_from_manifest(manifest),
            Some("com.example.lib".to_string())
        );
    }

    #[test]
    fn manifest_without_automatic_module_name() {
        let manifest = "Manifest-Version: 1.0\nMain-Class: com.example.Main\n";
        assert_eq!(parse_automatic_module_name_from_manifest(manifest), None);
    }

    #[test]
    fn manifest_empty() {
        assert_eq!(parse_automatic_module_name_from_manifest(""), None);
    }

    // -----------------------------------------------------------------------
    // find_module_info_java
    // -----------------------------------------------------------------------

    #[test]
    fn find_module_info_java_in_maven_root() {
        let dir = tempfile::tempdir().unwrap();
        let java_src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(&java_src).unwrap();
        std::fs::write(java_src.join("module-info.java"), b"module com.example {}").unwrap();

        let roots = vec![java_src];
        let found = find_module_info_java(&roots);
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("module-info.java"));
    }

    #[test]
    fn find_module_info_java_absent() {
        let dir = tempfile::tempdir().unwrap();
        let java_src = dir.path().join("src").join("main").join("java");
        std::fs::create_dir_all(&java_src).unwrap();

        let roots = vec![java_src];
        assert!(find_module_info_java(&roots).is_none());
    }

    #[test]
    fn find_module_info_java_empty_roots() {
        assert!(find_module_info_java(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // compute_module_path_split
    // -----------------------------------------------------------------------

    /// Build a minimal JAR containing only a MANIFEST.MF with Automatic-Module-Name.
    fn make_jar_with_manifest_module_name(path: &Path, module_name: &str) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        write!(
            zip,
            "Manifest-Version: 1.0\nAutomatic-Module-Name: {}\n",
            module_name
        )
        .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn split_puts_required_jar_on_module_path() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let jar_path = dir.path().join("mylib-1.0.jar");
        make_jar_with_manifest_module_name(&jar_path, "com.example.mylib");

        let module_info = ParsedModuleInfo {
            module_name: "com.example.app".to_string(),
            requires: vec![Requires {
                module_name: "com.example.mylib".to_string(),
                is_static: false,
                is_transitive: false,
            }],
        };

        let split =
            compute_module_path_split(&module_info, std::slice::from_ref(&jar_path), &target_dir)
                .unwrap();
        assert_eq!(split.module_path, vec![jar_path]);
        assert!(split.classpath.is_empty());
    }

    #[test]
    fn split_puts_unrequired_jar_on_classpath() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let jar_path = dir.path().join("otherlib-2.0.jar");
        make_jar_with_manifest_module_name(&jar_path, "com.example.otherlib");

        let module_info = ParsedModuleInfo {
            module_name: "com.example.app".to_string(),
            requires: vec![], // does NOT require com.example.otherlib
        };

        let split =
            compute_module_path_split(&module_info, std::slice::from_ref(&jar_path), &target_dir)
                .unwrap();
        assert!(split.module_path.is_empty());
        assert_eq!(split.classpath, vec![jar_path]);
    }

    #[test]
    fn split_mixes_module_path_and_classpath() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let jar_a = dir.path().join("a-1.jar");
        let jar_b = dir.path().join("b-1.jar");
        make_jar_with_manifest_module_name(&jar_a, "com.example.a");
        make_jar_with_manifest_module_name(&jar_b, "com.example.b");

        let module_info = ParsedModuleInfo {
            module_name: "com.example.app".to_string(),
            requires: vec![Requires {
                module_name: "com.example.a".to_string(),
                is_static: false,
                is_transitive: false,
            }],
        };

        let split =
            compute_module_path_split(&module_info, &[jar_a.clone(), jar_b.clone()], &target_dir)
                .unwrap();
        assert_eq!(split.module_path, vec![jar_a]);
        assert_eq!(split.classpath, vec![jar_b]);
    }

    // -----------------------------------------------------------------------
    // ModuleCache round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn module_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join(".jpms-modules.toml");

        let jar = dir.path().join("lib.jar");
        std::fs::write(&jar, b"").unwrap(); // create file so mtime exists

        let mtime = jar_mtime_secs(&jar);
        assert!(mtime.is_some(), "temp file should have an mtime");
        let identity = ModuleIdentity::AutomaticManifest("my.module".to_string());

        let mut cache = ModuleCache::default();
        cache.insert(&jar, mtime, &identity);
        cache.save(&cache_path).unwrap();

        let loaded = ModuleCache::load(&cache_path);
        let retrieved = loaded.get(&jar, mtime).unwrap();
        assert_eq!(retrieved, identity);
    }

    #[test]
    fn module_cache_miss_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join(".jpms-modules.toml");
        let jar = dir.path().join("lib.jar");

        let mut cache = ModuleCache::default();
        cache.insert(&jar, Some(100), &ModuleIdentity::Underivable);
        cache.save(&cache_path).unwrap();

        let loaded = ModuleCache::load(&cache_path);
        // Different mtime → cache miss.
        assert!(loaded.get(&jar, Some(200)).is_none());
    }

    #[test]
    fn module_cache_miss_when_current_mtime_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("lib.jar");

        let mut cache = ModuleCache::default();
        cache.insert(&jar, Some(100), &ModuleIdentity::Underivable);

        // Stat failure → None current mtime must never hit, even if an entry exists.
        assert!(cache.get(&jar, None).is_none());
    }

    #[test]
    fn module_cache_miss_when_stored_mtime_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("lib.jar");

        let mut cache = ModuleCache::default();
        cache.insert(&jar, None, &ModuleIdentity::Underivable);

        // None stored never matches a known current mtime (or another None).
        assert!(cache.get(&jar, Some(100)).is_none());
        assert!(cache.get(&jar, None).is_none());
    }

    #[test]
    fn module_cache_hit_with_epoch_mtime() {
        // Some(0) is a real fingerprint (1970-01-01), not an error sentinel.
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("lib.jar");

        let mut cache = ModuleCache::default();
        let identity = ModuleIdentity::AutomaticFilename("epoch.mod".into());
        cache.insert(&jar, Some(0), &identity);
        assert_eq!(cache.get(&jar, Some(0)), Some(identity));
    }

    #[test]
    fn module_cache_missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("nonexistent.toml");
        let cache = ModuleCache::load(&cache_path);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn jar_mtime_secs_none_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.jar");
        assert_eq!(jar_mtime_secs(&missing), None);
    }

    // -----------------------------------------------------------------------
    // is_platform_module
    // -----------------------------------------------------------------------

    #[test]
    fn java_modules_are_platform() {
        assert!(is_platform_module("java.base"));
        assert!(is_platform_module("java.sql"));
        assert!(is_platform_module("java.desktop"));
        assert!(!is_platform_module("com.example.lib"));
        assert!(!is_platform_module("com.fasterxml.jackson.core"));
    }

    #[test]
    fn jdk_modules_are_platform() {
        assert!(is_platform_module("jdk.incubator.vector"));
        assert!(!is_platform_module("jdkfoo.something"));
    }

    // -----------------------------------------------------------------------
    // read_jar_module_requires (integration — skipped when JAR not cached)
    // -----------------------------------------------------------------------

    #[test]
    fn jackson_databind_requires_core_and_annotations() {
        let jar = std::path::Path::new(
            "/home/sentinel/.m2/repository/com/fasterxml/jackson/core/\
             jackson-databind/2.15.2/jackson-databind-2.15.2.jar",
        );
        if !jar.exists() {
            return;
        }
        let requires = read_jar_module_requires(jar).unwrap();
        let names: std::collections::HashSet<&str> = requires.iter().map(|s| s.as_str()).collect();
        assert!(
            names.contains("com.fasterxml.jackson.core"),
            "jackson-databind should require jackson-core; got: {requires:?}"
        );
        assert!(
            names.contains("com.fasterxml.jackson.annotation"),
            "jackson-databind should require jackson-annotations; got: {requires:?}"
        );
    }

    /// Integration test: BFS expansion — requiring only jackson-databind should
    /// pull jackson-core and jackson-annotations onto the module-path too.
    #[test]
    fn bfs_split_expands_transitive_explicit_requires() {
        let m2 = std::path::Path::new("/home/sentinel/.m2/repository/com/fasterxml/jackson");
        let databind = m2.join("core/jackson-databind/2.15.2/jackson-databind-2.15.2.jar");
        let core = m2.join("core/jackson-core/2.15.2/jackson-core-2.15.2.jar");
        let annotations = m2.join("core/jackson-annotations/2.15.2/jackson-annotations-2.15.2.jar");
        if !databind.exists() || !core.exists() || !annotations.exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let module_info = ParsedModuleInfo {
            module_name: "com.example.greeter".to_string(),
            requires: vec![Requires {
                module_name: "com.fasterxml.jackson.databind".to_string(),
                is_static: false,
                is_transitive: false,
            }],
        };
        let dep_jars = vec![databind.clone(), core.clone(), annotations.clone()];
        let split = compute_module_path_split(&module_info, &dep_jars, &target_dir).unwrap();

        let mp: std::collections::HashSet<&std::path::Path> =
            split.module_path.iter().map(|p| p.as_path()).collect();
        assert!(
            mp.contains(databind.as_path()),
            "databind must be on module-path"
        );
        assert!(
            mp.contains(core.as_path()),
            "jackson-core must be on module-path (transitive)"
        );
        assert!(
            mp.contains(annotations.as_path()),
            "jackson-annotations must be on module-path (transitive)"
        );
        assert!(
            split.classpath.is_empty(),
            "all three JARs should be on module-path"
        );
    }

    /// Integration test: verify that a real multi-release JAR (jackson-databind)
    /// resolves to an Explicit module. Skipped when the JAR is not cached locally.
    #[test]
    fn multi_release_jar_reads_explicit_module_name() {
        let jar = std::path::Path::new(
            "/home/sentinel/.m2/repository/com/fasterxml/jackson/core/\
             jackson-databind/2.15.2/jackson-databind-2.15.2.jar",
        );
        if !jar.exists() {
            return; // CI may not have the JAR; skip gracefully.
        }
        let identity = read_jar_module_identity(jar).unwrap();
        match &identity {
            ModuleIdentity::Explicit(name) => {
                assert_eq!(
                    name, "com.fasterxml.jackson.databind",
                    "unexpected module name: {name}"
                );
            }
            other => panic!("expected Explicit module, got {other:?}"),
        }
    }
}
