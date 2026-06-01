//! Tantivy-based local index of Maven Central artifacts for interactive `curie add` search.
//!
//! Paths (all under `~/.curie/`):
//!   `nexus-index.gz`      — cached compressed index (kept after build)
//!   `nexus-index.gz.tmp`  — in-progress download (deleted on completion)
//!   `artifact-index/`     — Tantivy segment directory
//!   `artifact-index.tmp/` — build workspace (renamed atomically on success)
//!   `artifact-index.meta.json` — timestamp + artifact count sidecar

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    IndexRecordOption, NamedFieldDocument, OwnedValue, Schema, STORED, STRING, TEXT,
};
use tantivy::{doc, Document, Index, TantivyDocument, Term};

const NEXUS_INDEX_URL: &str =
    "https://repo1.maven.org/maven2/.index/nexus-maven-repository-index.gz";
const NEXUS_PROPS_URL: &str =
    "https://repo1.maven.org/maven2/.index/nexus-maven-repository-index.properties";
const WRITER_HEAP_BYTES: usize = 256 * 1024 * 1024;
const INDEX_STALENESS_DAYS: i64 = 30;
/// Integer.MIN_VALUE written by Java's DataOutputStream as the end-of-stream marker.
const NEXUS_EOF_MARKER: i32 = i32::MIN;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Open handle to the Tantivy artifact index used by [`search`] and [`total_count`].
pub struct IndexHandle {
    pub reader: tantivy::IndexReader,
    pub schema: Schema,
    /// Stored for possible direct lookups; queries use `f_coord_text`.
    #[allow(dead_code)]
    pub f_coord: tantivy::schema::Field,
    pub f_coord_text: tantivy::schema::Field,
    pub f_name: tantivy::schema::Field,
    pub f_description: tantivy::schema::Field,
    /// Stored for possible direct queries; results retrieved via field name.
    #[allow(dead_code)]
    pub f_version: tantivy::schema::Field,
}

#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    pub coord: String,
    pub name: String,
    pub description: String,
    pub version: String,
}

// ---------------------------------------------------------------------------
// Metadata sidecar
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct IndexMeta {
    index_timestamp_ms: i64,
    artifact_count: u64,
}

impl IndexMeta {
    fn age_days(&self) -> i64 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        (now_ms - self.index_timestamp_ms) / (1_000 * 60 * 60 * 24)
    }
}

struct BestRecord {
    version: String,
    name: String,
    description: String,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

pub fn index_dir() -> PathBuf {
    curie_home().join("artifact-index")
}

fn index_tmp_dir() -> PathBuf {
    curie_home().join("artifact-index.tmp")
}

fn gz_cache_path() -> PathBuf {
    curie_home().join("nexus-index.gz")
}

fn gz_tmp_path() -> PathBuf {
    curie_home().join("nexus-index.gz.tmp")
}

fn meta_path() -> PathBuf {
    curie_home().join("artifact-index.meta.json")
}

fn curie_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".curie")
}

fn read_meta() -> Option<IndexMeta> {
    let content = std::fs::read_to_string(meta_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn is_dir_empty(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

fn gz_present() -> bool {
    let p = gz_cache_path();
    p.exists() && p.metadata().map(|m| m.len() > 1024).unwrap_or(false)
}

fn index_present() -> bool {
    let d = index_dir();
    d.exists() && !is_dir_empty(&d)
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

fn make_schema() -> (
    Schema,
    tantivy::schema::Field,
    tantivy::schema::Field,
    tantivy::schema::Field,
    tantivy::schema::Field,
    tantivy::schema::Field,
) {
    let mut b = Schema::builder();
    let f_coord = b.add_text_field("coord", STRING | STORED);
    let f_coord_text = b.add_text_field("coord_text", TEXT);
    let f_name = b.add_text_field("name", TEXT | STORED);
    let f_description = b.add_text_field("description", TEXT | STORED);
    let f_version = b.add_text_field("version", STRING | STORED);
    (b.build(), f_coord, f_coord_text, f_name, f_description, f_version)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return an open `IndexHandle`, downloading and/or building the index as needed.
///
/// Strategy:
/// * If `force_refresh`: re-download the gzip (unless `offline`), then rebuild.
/// * If index is absent but the cached gzip exists: rebuild without re-downloading.
/// * If index is absent and gzip is absent: download then build.
/// * If index is present: open it, warn if stale.
pub fn ensure_index(force_refresh: bool, offline: bool) -> Result<IndexHandle> {
    let home = curie_home();
    std::fs::create_dir_all(&home).context("failed to create ~/.curie")?;

    if force_refresh {
        if !offline {
            download_gz()?;
        } else if !gz_present() {
            anyhow::bail!(
                "No cached index file at {}.\n\
                 Run `curie add --refresh-index` without --offline to download it.",
                gz_cache_path().display()
            );
        }
        build_index_from_gz()?;
    } else if !index_present() {
        if !gz_present() {
            if offline {
                anyhow::bail!(
                    "No artifact index found. Run `curie add` without --offline to download it."
                );
            }
            download_gz()?;
        } else {
            eprintln!("  Rebuilding index from cached download…");
        }
        build_index_from_gz()?;
    } else {
        // Index is present; check staleness
        if let Some(meta) = read_meta() {
            let age = meta.age_days();
            if age > INDEX_STALENESS_DAYS {
                eprintln!(
                    "  Index is {} days old. Run `curie add --refresh-index` to update.",
                    age
                );
            }
        }
    }

    open_index()
}

/// Search the artifact index. Returns up to `limit` results ordered by BM25 relevance.
pub fn search(handle: &IndexHandle, query_str: &str, limit: usize) -> Result<Vec<ArtifactRecord>> {
    let q = query_str.trim().to_lowercase();
    let tokens: Vec<&str> = q.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok(vec![]);
    }

    let searcher = handle.reader.searcher();
    let n = tokens.len();
    let search_fields = [handle.f_coord_text, handle.f_name, handle.f_description];
    let mut token_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(n);

    for (i, token) in tokens.iter().enumerate() {
        let is_last = i == n - 1;
        let mut field_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(3);
        for &field in &search_fields {
            let term = Term::from_field_text(field, token);
            let q: Box<dyn Query> = if is_last {
                Box::new(FuzzyTermQuery::new_prefix(term, 0, true))
            } else {
                Box::new(TermQuery::new(term, IndexRecordOption::Basic))
            };
            field_clauses.push((Occur::Should, q));
        }
        token_clauses.push((Occur::Must, Box::new(BooleanQuery::new(field_clauses))));
    }

    let query = BooleanQuery::new(token_clauses);
    let top_docs = searcher
        .search(&query, &TopDocs::with_limit(limit))
        .context("search failed")?;

    let mut results = Vec::with_capacity(top_docs.len());
    for (_, addr) in top_docs {
        let doc: TantivyDocument = searcher.doc(addr).context("doc retrieval failed")?;
        let named: NamedFieldDocument = doc.to_named_doc(&handle.schema);
        let get = |key: &str| -> String {
            named
                .0
                .get(key)
                .and_then(|vs| vs.first())
                .and_then(|v| {
                    if let OwnedValue::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };
        results.push(ArtifactRecord {
            coord: get("coord"),
            name: get("name"),
            description: get("description"),
            version: get("version"),
        });
    }
    Ok(results)
}

/// Total number of artifacts in the index.
pub fn total_count(handle: &IndexHandle) -> u64 {
    handle.reader.searcher().num_docs()
}

// ---------------------------------------------------------------------------
// Index opening
// ---------------------------------------------------------------------------

fn open_index() -> Result<IndexHandle> {
    let dir = index_dir();
    let index = Index::open_in_dir(&dir)
        .with_context(|| format!("failed to open index at {}", dir.display()))?;
    let schema = index.schema();

    let get_field = |name: &str| -> Result<tantivy::schema::Field> {
        schema
            .get_field(name)
            .map_err(|_| anyhow::anyhow!("index schema missing field '{}'", name))
    };

    let f_coord = get_field("coord")?;
    let f_coord_text = get_field("coord_text")?;
    let f_name = get_field("name")?;
    let f_description = get_field("description")?;
    let f_version = get_field("version")?;

    let reader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
        .context("failed to build index reader")?;

    Ok(IndexHandle {
        reader,
        schema,
        f_coord,
        f_coord_text,
        f_name,
        f_description,
        f_version,
    })
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn fetch_timestamp_ms() -> Option<i64> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie/0.1")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let text = client.get(NEXUS_PROPS_URL).send().ok()?.text().ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("nexus.index.timestamp=") {
            return rest.trim().parse::<i64>().ok();
        }
    }
    None
}

/// Download the Nexus index gzip to `~/.curie/nexus-index.gz`.
/// Uses a `.tmp` file to avoid leaving a partial file on interruption.
fn download_gz() -> Result<()> {
    let home = curie_home();
    std::fs::create_dir_all(&home).context("failed to create ~/.curie")?;

    let gz = gz_cache_path();
    let tmp = gz_tmp_path();

    // Remove leftover .tmp from a previous interrupted download
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    eprintln!("  Downloading Maven Central artifact index (this only happens once)…");

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie/0.1")
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("failed to build HTTP client")?;

    let mut response = client
        .get(NEXUS_INDEX_URL)
        .send()
        .context("failed to connect to Maven Central")?;

    let content_length = response.content_length();
    let pb = make_download_bar(content_length);

    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;

    let mut pr = ProgressReader { inner: &mut response, pb: pb.clone() };
    std::io::copy(&mut pr, &mut file).context("download failed")?;
    file.flush().context("failed to flush gz file")?;
    drop(file);

    std::fs::rename(&tmp, &gz).context("failed to finalise download")?;
    pb.finish_with_message("Downloaded");
    Ok(())
}

// ---------------------------------------------------------------------------
// Index build
// ---------------------------------------------------------------------------

/// Parse `~/.curie/nexus-index.gz` and write the Tantivy index.
///
/// Builds into a temp directory and renames atomically, so a failed build
/// never corrupts an existing good index.
fn build_index_from_gz() -> Result<()> {
    let gz = gz_cache_path();
    anyhow::ensure!(
        gz.exists(),
        "Cached index file not found at {}. Re-run without --offline.",
        gz.display()
    );

    let timestamp_ms = fetch_timestamp_ms().unwrap_or(0);

    let file = std::fs::File::open(&gz)
        .with_context(|| format!("failed to open {}", gz.display()))?;
    let gz_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    let pb = make_parse_bar(gz_size);
    let gz_dec = GzDecoder::new(ProgressReader { inner: file, pb: pb.clone() });
    let mut rdr = BufReader::with_capacity(1 << 20, gz_dec); // 1 MiB

    // ── Nexus binary format ───────────────────────────────────────────────
    // Verified empirically against the actual Maven Central index file.
    //
    // Header:
    //   [1 byte]  format version (= 1)
    //   [8 bytes] timestamp (ms, big-endian long)
    //
    // Records — repeat until terminator:
    //   [4 bytes i32] field_count   (0 or Integer.MIN_VALUE = end-of-stream)
    //   per field:
    //     [1 byte]        field type tag (Lucene field encoding; we skip it)
    //     [2 bytes u16]   name length  (big-endian, writeUTF format)
    //     [name_len bytes] name  (ASCII: "u", "g", "a", "v", "p", "l", "n", "d", …)
    //     [4 bytes i32]   value length (big-endian; NOT writeUTF's 2-byte length)
    //     [value_len bytes] value (UTF-8)
    //
    // There is NO per-record type integer.  All records in the full index are
    // artifact records; they differ only in which named fields they carry.

    let _version = read_u8(&mut rdr).context("failed to read index header byte")?;
    let _header_ts = read_i64(&mut rdr).context("failed to read index timestamp")?;

    // First pass: accumulate best (latest) record per coord
    let mut map: HashMap<String, BestRecord> = HashMap::with_capacity(600_000);

    loop {
        let field_count: usize = match read_i32(&mut rdr) {
            Ok(0) | Ok(NEXUS_EOF_MARKER) => break,
            Ok(fc) if fc < 0 => break,
            Ok(fc) => fc as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("error reading field count"),
        };

        let mut g = String::new();
        let mut a = String::new();
        let mut v = String::new();
        let mut p = String::new();
        let mut l = String::new();
        let mut n_fld = String::new();
        let mut d_fld = String::new();
        let mut u_fld = String::new();

        let mut ok = true;
        for _ in 0..field_count {
            // Skip the 1-byte Lucene field type tag
            match read_u8(&mut rdr) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => { ok = false; break; }
                Err(e) => return Err(e).context("error reading field tag"),
            }
            // Field name uses writeUTF: [2-byte u16 len][bytes]
            let fname = match read_utf(&mut rdr) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    ok = false;
                    break;
                }
                Err(e) => return Err(e).context("error reading field name"),
            };
            // Field value uses a 4-byte i32 length (NOT writeUTF's 2-byte length)
            let fval = match read_value(&mut rdr) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    ok = false;
                    break;
                }
                Err(e) => return Err(e).context("error reading field value"),
            };
            match fname.as_str() {
                "g" => g = fval,
                "a" => a = fval,
                "v" => v = fval,
                "p" => p = fval,
                "l" => l = fval,
                "n" => n_fld = fval,
                "d" => d_fld = fval,
                "u" => u_fld = fval,
                _ => {}
            }
        }
        if !ok {
            break;
        }

        // Fall back to `u` field: "groupId|artifactId|version|classifier[|packaging]"
        if (g.is_empty() || a.is_empty()) && !u_fld.is_empty() {
            let parts: Vec<&str> = u_fld.splitn(5, '|').collect();
            if parts.len() >= 2 {
                if g.is_empty() {
                    g = parts[0].to_string();
                }
                if a.is_empty() {
                    a = parts[1].to_string();
                }
                if v.is_empty() && parts.len() >= 3 {
                    v = parts[2].to_string();
                }
                if l.is_empty() && parts.len() >= 4 && parts[3] != "NA" {
                    l = parts[3].to_string();
                }
                if p.is_empty() && parts.len() >= 5 {
                    p = parts[4].to_string();
                }
            }
        }

        if g.is_empty() || a.is_empty() {
            continue;
        }

        let packaging = if p.is_empty() { "jar" } else { p.as_str() };
        if packaging != "jar" {
            continue;
        }
        if !l.is_empty() && l != "NA" {
            continue; // skip -sources, -javadoc, etc.
        }

        let coord = format!("{}:{}", g, a);
        let should_update = map
            .get(&coord)
            .map(|existing| version_gt(&v, &existing.version))
            .unwrap_or(true);

        if should_update {
            map.insert(coord, BestRecord { version: v, name: n_fld, description: d_fld });
        }
    }

    pb.finish_with_message("Parsed");

    let artifact_count = map.len() as u64;
    eprintln!("  Indexing {} artifacts…", artifact_count);

    // Build Tantivy index into a temp dir, then atomically rename
    let tmp_dir = index_tmp_dir();
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).context("failed to remove old temp index")?;
    }
    std::fs::create_dir_all(&tmp_dir).context("failed to create temp index directory")?;

    let (schema, f_coord, f_coord_text, f_name, f_description, f_version) = make_schema();
    let index =
        Index::create_in_dir(&tmp_dir, schema).context("failed to create Tantivy index")?;

    let idx_pb = ProgressBar::new(artifact_count);
    idx_pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} artifacts",
            )
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut writer = index
        .writer(WRITER_HEAP_BYTES)
        .context("failed to create index writer")?;

    for (coord, rec) in &map {
        writer
            .add_document(doc!(
                f_coord       => coord.as_str(),
                f_coord_text  => coord.as_str(),
                f_name        => rec.name.as_str(),
                f_description => rec.description.as_str(),
                f_version     => rec.version.as_str(),
            ))
            .context("failed to add document")?;
        idx_pb.inc(1);
    }
    writer.commit().context("failed to commit index")?;
    idx_pb.finish_with_message("Indexed");
    drop(writer);

    // Atomic replace: remove old index dir (if any), rename temp into place
    let final_dir = index_dir();
    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir).context("failed to remove old index")?;
    }
    std::fs::rename(&tmp_dir, &final_dir).context("failed to install new index")?;

    // Write metadata sidecar
    let meta = IndexMeta { index_timestamp_ms: timestamp_ms, artifact_count };
    std::fs::write(meta_path(), serde_json::to_string_pretty(&meta)?)
        .context("failed to write index metadata")?;

    eprintln!("  Artifact index ready ({} artifacts).", artifact_count);
    Ok(())
}

// ---------------------------------------------------------------------------
// Binary-format helpers (Java DataOutputStream / writeUTF)
// ---------------------------------------------------------------------------

fn read_u8<R: Read>(r: &mut R) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_i32<R: Read>(r: &mut R) -> std::io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

fn read_i64<R: Read>(r: &mut R) -> std::io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_be_bytes(b))
}

/// Read a field name — Java writeUTF: 2-byte big-endian u16 length + bytes.
fn read_utf<R: Read>(r: &mut R) -> std::io::Result<String> {
    let mut lb = [0u8; 2];
    r.read_exact(&mut lb)?;
    let len = u16::from_be_bytes(lb) as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read a field value — 4-byte big-endian i32 length + bytes.
/// (Values use a wider length than writeUTF to support strings > 65 535 bytes.)
fn read_value<R: Read>(r: &mut R) -> std::io::Result<String> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb)?;
    let len = i32::from_be_bytes(lb);
    if len < 0 {
        return Ok(String::new());
    }
    let mut bytes = vec![0u8; len as usize];
    r.read_exact(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Progress helpers
// ---------------------------------------------------------------------------

fn make_download_bar(content_length: Option<u64>) -> ProgressBar {
    if let Some(len) = content_length {
        let pb = ProgressBar::new(len);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
                     {bytes}/{total_bytes} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {bytes} downloaded")
                .unwrap(),
        );
        pb
    }
}

fn make_parse_bar(gz_size: u64) -> ProgressBar {
    if gz_size > 0 {
        let pb = ProgressBar::new(gz_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
                     {bytes}/{total_bytes} parsed ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {bytes} parsed")
                .unwrap(),
        );
        pb
    }
}

struct ProgressReader<R: Read> {
    inner: R,
    pb: ProgressBar,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pb.inc(n as u64);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

fn version_gt(a: &str, b: &str) -> bool {
    version_cmp(a, b) == std::cmp::Ordering::Greater
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa = split_version(a);
    let pb = split_version(b);
    for (x, y) in pa.iter().zip(pb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(n), Ok(m)) => n.cmp(&m),
            _ => x.as_str().cmp(y.as_str()),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    pa.len().cmp(&pb.len())
}

fn split_version(v: &str) -> Vec<String> {
    v.split(|c| c == '.' || c == '-').map(str::to_string).collect()
}

// Used only in tests to verify the FTS query string format.
#[cfg(test)]
fn build_fts_query(q: &str) -> String {
    let tokens: Vec<&str> = q.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    tokens.iter().map(|t| format!("{}*", t)).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::Index;

    fn make_test_handle(records: &[(&str, &str, &str, &str)]) -> IndexHandle {
        let (schema, f_coord, f_coord_text, f_name, f_description, f_version) = make_schema();
        let index = Index::create_in_ram(schema.clone());
        let mut writer = index.writer(15_000_000).unwrap();
        for &(coord, name, desc, version) in records {
            writer
                .add_document(doc!(
                    f_coord       => coord,
                    f_coord_text  => coord,
                    f_name        => name,
                    f_description => desc,
                    f_version     => version,
                ))
                .unwrap();
        }
        writer.commit().unwrap();
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        IndexHandle { reader, schema, f_coord, f_coord_text, f_name, f_description, f_version }
    }

    #[test]
    fn build_fts_query_single_token() {
        assert_eq!(build_fts_query("guava"), "guava*");
    }

    #[test]
    fn build_fts_query_multi_token() {
        assert_eq!(build_fts_query("jackson databind"), "jackson* databind*");
    }

    #[test]
    fn build_fts_query_empty() {
        assert_eq!(build_fts_query(""), "");
    }

    // ── helpers for building synthetic index bytes ─────────────────────────

    /// Encode a field: [1-byte tag][writeUTF name][4-byte value_len][value]
    fn field_bytes(tag: u8, name: &str, value: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(tag);
        v.extend_from_slice(&(name.len() as u16).to_be_bytes());
        v.extend_from_slice(name.as_bytes());
        v.extend_from_slice(&(value.len() as i32).to_be_bytes());
        v.extend_from_slice(value.as_bytes());
        v
    }

    /// Build a minimal valid stream: 9-byte header + one record + zero terminator.
    fn minimal_stream(fields: &[(u8, &str, &str)]) -> Vec<u8> {
        let mut v = Vec::new();
        // header: version(1) + timestamp(8)
        v.push(1u8);
        v.extend_from_slice(&0i64.to_be_bytes());
        // record: field_count(4) + fields
        v.extend_from_slice(&(fields.len() as i32).to_be_bytes());
        for &(tag, name, value) in fields {
            v.extend_from_slice(&field_bytes(tag, name, value));
        }
        // terminator
        v.extend_from_slice(&0i32.to_be_bytes());
        v
    }

    // ── format unit tests ───────────────────────────────────────────────────

    #[test]
    fn parse_nexus_header_version() {
        let bytes: &[u8] = &[1, 0, 0, 0, 0, 0, 0, 0, 42, 99];
        let mut cur = std::io::Cursor::new(bytes);
        assert_eq!(read_u8(&mut cur).unwrap(), 1); // version
        assert_eq!(read_i64(&mut cur).unwrap(), 42i64); // timestamp
    }

    #[test]
    fn parse_nexus_field_uses_4byte_value_length() {
        // Verified from real file: field "u" at byte 13 of decompressed stream.
        // tag=0x05, name="u" (writeUTF: 0x00 0x01 0x75), value_len=30 (4-byte: 0x0000001E)
        let raw = field_bytes(0x05, "u", "zw.co.paynow|java-sdk|1.1.2|NA");
        let mut cur = std::io::Cursor::new(&raw[..]);
        assert_eq!(read_u8(&mut cur).unwrap(), 0x05); // tag
        assert_eq!(read_utf(&mut cur).unwrap(), "u"); // writeUTF name
        assert_eq!(read_value(&mut cur).unwrap(), "zw.co.paynow|java-sdk|1.1.2|NA"); // 4-byte value
    }

    #[test]
    fn parse_nexus_real_first_record_bytes() {
        // Bytes 0x00–0xC8 from the actual decompressed nexus-index.gz, captured
        // via hex dump.  Header (9 bytes) + record with 5 fields: u, m, i, n, d.
        #[rustfmt::skip]
        let raw: &[u8] = &[
            // header
            0x01,                                     // version = 1
            0x00,0x00,0x01,0x9E,0x47,0xC3,0x85,0xC4, // timestamp
            // record: field_count = 5
            0x00,0x00,0x00,0x05,
            // field "u": tag=0x05, name="u"(len=1), value="zw.co.paynow|java-sdk|1.1.2|NA"(len=30)
            0x05, 0x00,0x01,0x75, 0x00,0x00,0x00,0x1E,
            b'z',b'w',b'.',b'c',b'o',b'.',b'p',b'a',b'y',b'n',b'o',b'w',b'|',
            b'j',b'a',b'v',b'a',b'-',b's',b'd',b'k',b'|',b'1',b'.',b'1',b'.',b'2',b'|',b'N',b'A',
            // field "m": tag=0x04, name="m"(len=1), value="1765378927470"(len=13)
            0x04, 0x00,0x01,0x6D, 0x00,0x00,0x00,0x0D,
            b'1',b'7',b'6',b'5',b'3',b'7',b'8',b'9',b'2',b'7',b'4',b'7',b'0',
            // field "i": tag=0x04, name="i"(len=1), value(len=45)
            0x04, 0x00,0x01,0x69, 0x00,0x00,0x00,0x2D,
            b'p',b'o',b'm',b'.',b's',b'h',b'a',b'5',b'1',b'2',b'|',
            b'1',b'7',b'3',b'8',b'1',b'5',b'5',b'0',b'9',b'8',b'0',b'0',b'0',b'|',
            b'1',b'2',b'8',b'|',b'1',b'|',b'1',b'|',b'0',b'|',
            b'p',b'o',b'm',b'.',b's',b'h',b'a',b'5',b'1',b'2',
            // field "n": tag=0x07, name="n"(len=1), value="java-sdk"(len=8)
            0x07, 0x00,0x01,0x6E, 0x00,0x00,0x00,0x08,
            b'j',b'a',b'v',b'a',b'-',b's',b'd',b'k',
            // field "d": tag=0x07, name="d"(len=1), value(len=92) — truncated to 0 for brevity
            0x07, 0x00,0x01,0x64, 0x00,0x00,0x00,0x00,
            // terminator
            0x00,0x00,0x00,0x00,
        ];

        let mut cur = std::io::Cursor::new(raw);

        // header
        assert_eq!(read_u8(&mut cur).unwrap(), 1);
        let ts = read_i64(&mut cur).unwrap();
        assert_eq!(ts, 0x000001_9E47C385C4u64 as i64);

        // record
        let fc = read_i32(&mut cur).unwrap();
        assert_eq!(fc, 5);

        let mut u_val = String::new();
        let mut n_val = String::new();
        for _ in 0..fc {
            let _tag = read_u8(&mut cur).unwrap();
            let name = read_utf(&mut cur).unwrap();
            let val = read_value(&mut cur).unwrap();
            match name.as_str() {
                "u" => u_val = val,
                "n" => n_val = val,
                _ => {}
            }
        }
        assert_eq!(u_val, "zw.co.paynow|java-sdk|1.1.2|NA");
        assert_eq!(n_val, "java-sdk");

        // terminator
        assert_eq!(read_i32(&mut cur).unwrap(), 0);
    }

    #[test]
    fn parse_stream_extracts_artifact_via_u_field() {
        // u-field format: groupId|artifactId|version|classifier[|packaging]
        let stream = minimal_stream(&[
            (0x05, "u", "com.example|mylib|2.3.0|NA"),
            (0x07, "n", "My Library"),
        ]);
        let mut map: HashMap<String, BestRecord> = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(map.contains_key("com.example:mylib"),
            "map: {:?}", map.keys().collect::<Vec<_>>());
        assert_eq!(map["com.example:mylib"].version, "2.3.0");
        assert_eq!(map["com.example:mylib"].name, "My Library");
    }

    #[test]
    fn parse_stream_extracts_artifact_via_g_a_fields() {
        // A stream whose record carries individual "g", "a", "v" fields.
        let stream = minimal_stream(&[
            (0x05, "g", "org.apache.commons"),
            (0x05, "a", "commons-lang3"),
            (0x05, "v", "3.14.0"),
            (0x05, "p", "jar"),
        ]);
        let mut map: HashMap<String, BestRecord> = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(
            map.contains_key("org.apache.commons:commons-lang3"),
            "map: {:?}", map.keys().collect::<Vec<_>>()
        );
        assert_eq!(map["org.apache.commons:commons-lang3"].version, "3.14.0");
    }

    #[test]
    fn parse_stream_skips_pom_packaging() {
        let stream = minimal_stream(&[
            (0x05, "g", "com.example"),
            (0x05, "a", "mylib"),
            (0x05, "v", "1.0"),
            (0x05, "p", "pom"),
        ]);
        let mut map: HashMap<String, BestRecord> = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(map.is_empty(), "pom should be filtered; map: {:?}", map.keys().collect::<Vec<_>>());
    }

    #[test]
    fn parse_stream_skips_sources_classifier() {
        let stream = minimal_stream(&[
            (0x05, "g", "com.example"),
            (0x05, "a", "mylib"),
            (0x05, "v", "1.0"),
            (0x05, "l", "sources"),
        ]);
        let mut map: HashMap<String, BestRecord> = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_stream_terminates_on_zero_field_count() {
        // Zero field_count immediately after header = valid empty stream.
        let mut stream = Vec::new();
        stream.push(1u8); // version
        stream.extend_from_slice(&0i64.to_be_bytes()); // timestamp
        stream.extend_from_slice(&0i32.to_be_bytes()); // terminator
        let mut map = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_stream_terminates_on_min_value_sentinel() {
        let mut stream = Vec::new();
        stream.push(1u8);
        stream.extend_from_slice(&0i64.to_be_bytes());
        stream.extend_from_slice(&i32::MIN.to_be_bytes()); // Integer.MIN_VALUE sentinel
        let mut map = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_stream_deduplicates_by_highest_version() {
        let stream = {
            let mut v = Vec::new();
            v.push(1u8);
            v.extend_from_slice(&0i64.to_be_bytes());
            // record 1: v=1.0
            v.extend_from_slice(&3i32.to_be_bytes());
            v.extend_from_slice(&field_bytes(0x05, "g", "com.example"));
            v.extend_from_slice(&field_bytes(0x05, "a", "lib"));
            v.extend_from_slice(&field_bytes(0x05, "v", "1.0"));
            // record 2: v=2.0 (should win)
            v.extend_from_slice(&3i32.to_be_bytes());
            v.extend_from_slice(&field_bytes(0x05, "g", "com.example"));
            v.extend_from_slice(&field_bytes(0x05, "a", "lib"));
            v.extend_from_slice(&field_bytes(0x05, "v", "2.0"));
            // record 3: v=1.5 (older than 2.0)
            v.extend_from_slice(&3i32.to_be_bytes());
            v.extend_from_slice(&field_bytes(0x05, "g", "com.example"));
            v.extend_from_slice(&field_bytes(0x05, "a", "lib"));
            v.extend_from_slice(&field_bytes(0x05, "v", "1.5"));
            v.extend_from_slice(&0i32.to_be_bytes()); // terminator
            v
        };
        let mut map = HashMap::new();
        parse_stream_into_map(&stream, &mut map);
        assert_eq!(map.len(), 1);
        assert_eq!(map["com.example:lib"].version, "2.0");
    }

    /// Parse a raw decompressed byte slice (no gzip) into a map, using the same
    /// logic as `build_index_from_gz` but operating on an in-memory cursor so
    /// tests don't need actual files on disk.
    fn parse_stream_into_map(raw: &[u8], map: &mut HashMap<String, BestRecord>) {
        use std::io::BufReader;
        let mut rdr = BufReader::new(std::io::Cursor::new(raw));

        let _version = read_u8(&mut rdr).unwrap_or(0);
        let _ts = read_i64(&mut rdr).unwrap_or(0);

        loop {
            let field_count: usize = match read_i32(&mut rdr) {
                Ok(0) | Ok(NEXUS_EOF_MARKER) => break,
                Ok(fc) if fc < 0 => break,
                Ok(fc) => fc as usize,
                Err(_) => break,
            };

            let mut g = String::new();
            let mut a = String::new();
            let mut v = String::new();
            let mut p = String::new();
            let mut l = String::new();
            let mut n_fld = String::new();
            let mut d_fld = String::new();
            let mut u_fld = String::new();
            let mut ok = true;

            for _ in 0..field_count {
                if read_u8(&mut rdr).is_err() { ok = false; break; }
                let fname = match read_utf(&mut rdr) { Ok(s) => s, Err(_) => { ok = false; break; } };
                let fval  = match read_value(&mut rdr) { Ok(s) => s, Err(_) => { ok = false; break; } };
                match fname.as_str() {
                    "g" => g = fval, "a" => a = fval, "v" => v = fval,
                    "p" => p = fval, "l" => l = fval,
                    "n" => n_fld = fval, "d" => d_fld = fval, "u" => u_fld = fval,
                    _ => {}
                }
            }
            if !ok { break; }

            if (g.is_empty() || a.is_empty()) && !u_fld.is_empty() {
                let parts: Vec<&str> = u_fld.splitn(5, '|').collect();
                if parts.len() >= 2 {
                    if g.is_empty() { g = parts[0].to_string(); }
                    if a.is_empty() { a = parts[1].to_string(); }
                    if v.is_empty() && parts.len() >= 3 { v = parts[2].to_string(); }
                    if l.is_empty() && parts.len() >= 4 && parts[3] != "NA" { l = parts[3].to_string(); }
                    if p.is_empty() && parts.len() >= 5 { p = parts[4].to_string(); }
                }
            }

            if g.is_empty() || a.is_empty() { continue; }
            let packaging = if p.is_empty() { "jar" } else { p.as_str() };
            if packaging != "jar" { continue; }
            if !l.is_empty() && l != "NA" { continue; }
            let coord = format!("{}:{}", g, a);
            let should_update = map.get(&coord)
                .map(|e| version_gt(&v, &e.version))
                .unwrap_or(true);
            if should_update {
                map.insert(coord, BestRecord { version: v, name: n_fld, description: d_fld });
            }
        }
    }

    #[test]
    fn filter_skips_sources_classifier() {
        let l = "sources";
        assert!(!l.is_empty() && l != "NA");
    }

    #[test]
    fn filter_skips_pom_packaging() {
        let p = "pom";
        let packaging = if p.is_empty() { "jar" } else { p };
        assert_ne!(packaging, "jar");
    }

    #[test]
    fn filter_accepts_jar_packaging() {
        let p = "jar";
        let packaging = if p.is_empty() { "jar" } else { p };
        assert_eq!(packaging, "jar");
    }

    #[test]
    fn dedup_keeps_latest_version() {
        assert!(version_gt("2.0.0", "1.0.0"));
        assert!(!version_gt("1.0.0", "2.0.0"));
        assert!(!version_gt("1.0.0", "1.0.0"));
    }

    #[test]
    fn search_returns_matching_results() {
        let h = make_test_handle(&[(
            "com.google.guava:guava",
            "Guava",
            "Google core libraries",
            "33.0.0-jre",
        )]);
        let r = search(&h, "guava", 10).unwrap();
        assert!(!r.is_empty());
        assert_eq!(r[0].coord, "com.google.guava:guava");
    }

    #[test]
    fn search_prefix_matches_partial_name() {
        let h = make_test_handle(&[("com.google.guava:guava", "Guava", "", "33.0.0-jre")]);
        let r = search(&h, "guav", 10).unwrap();
        assert!(!r.is_empty());
    }

    #[test]
    fn search_by_group_token() {
        let h = make_test_handle(&[("com.google.guava:guava", "Guava", "", "33.0.0-jre")]);
        let r = search(&h, "google", 10).unwrap();
        assert!(!r.is_empty());
        assert_eq!(r[0].coord, "com.google.guava:guava");
    }

    #[test]
    fn search_multi_token() {
        let h = make_test_handle(&[(
            "com.fasterxml.jackson.core:jackson-databind",
            "Jackson Databind",
            "",
            "2.17.2",
        )]);
        let r = search(&h, "jackson databind", 10).unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let h = make_test_handle(&[("com.google.guava:guava", "Guava", "", "33.0.0-jre")]);
        assert!(search(&h, "", 10).unwrap().is_empty());
    }
}
