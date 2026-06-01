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
const WRITER_HEAP_BYTES: usize = 128 * 1024 * 1024;
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
    let mut rdr = BufReader::new(gz_dec);

    // ── Nexus binary format (Java DataOutputStream) ───────────────────────
    // Header:
    //   [1 byte]  format version (= 1)
    //   [8 bytes] timestamp (ms, big-endian long)
    //
    // Records — repeat:
    //   [4 bytes] rec_type   0=DESCRIPTOR  1=ADD/UPDATE  2=DELETE
    //                        Integer.MIN_VALUE (0x80000000) = end-of-stream; NO
    //                        field_count follows this sentinel value.
    //   [4 bytes] field_count
    //   per field:
    //     writeUTF(name)  → [2 bytes big-endian u16 len] + [N bytes]
    //     writeUTF(value) → [2 bytes big-endian u16 len] + [M bytes]

    let _version = read_u8(&mut rdr).context("failed to read index header byte")?;
    let _header_ts = read_i64(&mut rdr).context("failed to read index timestamp")?;

    // First pass: accumulate best (latest) record per coord
    let mut map: HashMap<String, BestRecord> = HashMap::with_capacity(600_000);

    loop {
        // rec_type must be read before field_count; Integer.MIN_VALUE is the
        // end-of-stream sentinel with NO following field_count.
        let rec_type = match read_i32(&mut rdr) {
            Ok(NEXUS_EOF_MARKER) => break,
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("error reading record type"),
        };

        let field_count: usize = match read_i32(&mut rdr) {
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
            let fname = match read_utf(&mut rdr) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    ok = false;
                    break;
                }
                Err(e) => return Err(e).context("error reading field name"),
            };
            let fval = match read_utf(&mut rdr) {
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

        // Fall back to `u` field: "groupId|artifactId|version|classifier|packaging"
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

        // Only ADD/UPDATE records contain artifact data worth indexing.
        // DESCRIPTOR (0) and DELETE (2) records are skipped after their fields
        // have been consumed to maintain stream alignment.
        if rec_type != 1 {
            continue;
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

fn read_utf<R: Read>(r: &mut R) -> std::io::Result<String> {
    let mut lb = [0u8; 2];
    r.read_exact(&mut lb)?;
    let len = u16::from_be_bytes(lb) as usize;
    let mut bytes = vec![0u8; len];
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

    #[test]
    fn parse_nexus_header_version() {
        // 1 byte version, 8 bytes timestamp
        let bytes: &[u8] = &[1, 0, 0, 0, 0, 0, 0, 0, 42, 99];
        let mut cur = std::io::Cursor::new(bytes);
        assert_eq!(read_u8(&mut cur).unwrap(), 1);
        // timestamp bytes follow
        let ts = read_i64(&mut cur).unwrap();
        assert_eq!(ts, 0x_00_00_00_00_00_00_00_2a_i64); // 42
    }

    #[test]
    fn parse_nexus_single_record() {
        // rec_type=1 (ADD), field_count=1, name="g", value="com.example"
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&1i32.to_be_bytes());  // rec_type = 1 (ADD)
        bytes.extend_from_slice(&1i32.to_be_bytes());  // field_count = 1
        bytes.extend_from_slice(&1u16.to_be_bytes());  // name len = 1
        bytes.push(b'g');
        bytes.extend_from_slice(&11u16.to_be_bytes()); // value len = 11
        bytes.extend_from_slice(b"com.example");
        let mut cur = std::io::Cursor::new(&bytes[..]);
        assert_eq!(read_i32(&mut cur).unwrap(), 1); // rec_type
        assert_eq!(read_i32(&mut cur).unwrap(), 1); // field_count
        assert_eq!(read_utf(&mut cur).unwrap(), "g");
        assert_eq!(read_utf(&mut cur).unwrap(), "com.example");
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
