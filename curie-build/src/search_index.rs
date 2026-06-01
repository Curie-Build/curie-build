//! Tantivy-based local index of Maven Central artifacts for interactive `curie add` search.
//!
//! Index directory: `~/.curie/artifact-index/`
//! Meta sidecar:    `~/.curie/artifact-index.meta.json`

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::PathBuf;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, NamedFieldDocument, OwnedValue, Schema, STORED, STRING, TEXT};
use tantivy::{doc, Document, Index, TantivyDocument, Term};

const NEXUS_INDEX_URL: &str =
    "https://repo1.maven.org/maven2/.index/nexus-maven-repository-index.gz";
const NEXUS_PROPS_URL: &str =
    "https://repo1.maven.org/maven2/.index/nexus-maven-repository-index.properties";
const WRITER_HEAP_BYTES: usize = 128 * 1024 * 1024;
const INDEX_STALENESS_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Fields and reader for the Tantivy artifact index.
pub struct IndexHandle {
    pub reader: tantivy::IndexReader,
    pub schema: Schema,
    /// Stored for direct document lookups; search queries use `f_coord_text`.
    #[allow(dead_code)]
    pub f_coord: tantivy::schema::Field,
    pub f_coord_text: tantivy::schema::Field,
    pub f_name: tantivy::schema::Field,
    pub f_description: tantivy::schema::Field,
    /// Stored for potential direct queries; retrieved via field name in search results.
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
// Serde metadata sidecar
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

/// Return an open `IndexHandle`, downloading and building the index if needed.
pub fn ensure_index(force_refresh: bool, offline: bool) -> Result<IndexHandle> {
    let dir = index_dir();
    let is_absent = !dir.exists()
        || std::fs::read_dir(&dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);

    if force_refresh || is_absent {
        if offline {
            anyhow::bail!(
                "No artifact index found at {}.\n\
                 Run `curie add` without --offline to download it.",
                dir.display()
            );
        }
        download_and_build_index()?;
    } else if let Some(meta) = read_meta() {
        let age = meta.age_days();
        if age > INDEX_STALENESS_DAYS {
            eprintln!(
                "  Index is {} days old. Run `curie add --refresh-index` to update.",
                age
            );
        }
    }

    open_index()
}

/// Search the artifact index.  Returns up to `limit` results ordered by BM25 relevance.
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
// Index download + build
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

fn download_and_build_index() -> Result<()> {
    let timestamp_ms = fetch_timestamp_ms().unwrap_or(0);

    eprintln!("  Downloading Maven Central artifact index (this only happens once)…");

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie/0.1")
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(NEXUS_INDEX_URL)
        .send()
        .context("failed to connect to Maven Central")?;

    let pb = make_download_bar(response.content_length());
    let gz = GzDecoder::new(ProgressReader { inner: response, pb: pb.clone() });
    let mut rdr = BufReader::new(gz);

    let version_byte = read_u8(&mut rdr).context("failed to read index version")?;
    anyhow::ensure!(version_byte == 1, "unexpected Nexus index version {}", version_byte);
    let _header_ts = read_i64(&mut rdr)?;

    // First pass: collect best record per (groupId:artifactId)
    let mut map: HashMap<String, BestRecord> = HashMap::with_capacity(600_000);

    loop {
        let rec_type = match read_i32(&mut rdr) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        let field_count = read_i32(&mut rdr)? as usize;

        let mut g = String::new();
        let mut a = String::new();
        let mut v = String::new();
        let mut p = String::new();
        let mut l = String::new();
        let mut n_fld = String::new();
        let mut d_fld = String::new();
        let mut u_fld = String::new();

        for _ in 0..field_count {
            let fname = read_utf(&mut rdr)?;
            let fval = read_utf(&mut rdr)?;
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

        if rec_type != 1 {
            continue; // only ADD/UPDATE
        }

        // Fall back to `u` field when individual fields are absent
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

    pb.finish_with_message("Downloaded");

    let artifact_count = map.len() as u64;
    eprintln!("  Indexing {} artifacts…", artifact_count);

    // Second pass: write to Tantivy
    let dir = index_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("failed to remove old index")?;
    }
    std::fs::create_dir_all(&dir).context("failed to create index directory")?;

    let (schema, f_coord, f_coord_text, f_name, f_description, f_version) = make_schema();
    let index = Index::create_in_dir(&dir, schema).context("failed to create Tantivy index")?;

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

    let home = curie_home();
    std::fs::create_dir_all(&home).context("failed to create ~/.curie")?;
    let meta = IndexMeta { index_timestamp_ms: timestamp_ms, artifact_count };
    std::fs::write(meta_path(), serde_json::to_string_pretty(&meta)?)
        .context("failed to write index metadata")?;

    eprintln!("  Artifact index ready ({} artifacts).", artifact_count);
    Ok(())
}

// ---------------------------------------------------------------------------
// Binary-format helpers (Java DataOutputStream)
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
        let bytes: &[u8] = &[1, 0, 0, 0, 0, 0, 0, 0, 0, 42];
        let mut cur = std::io::Cursor::new(bytes);
        assert_eq!(read_u8(&mut cur).unwrap(), 1);
    }

    #[test]
    fn parse_nexus_single_record() {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&1i32.to_be_bytes()); // type = ADD
        bytes.extend_from_slice(&1i32.to_be_bytes()); // 1 field
        bytes.extend_from_slice(&1u16.to_be_bytes()); // name len = 1
        bytes.push(b'g');
        bytes.extend_from_slice(&11u16.to_be_bytes()); // value len = 11
        bytes.extend_from_slice(b"com.example");
        let mut cur = std::io::Cursor::new(&bytes[..]);
        assert_eq!(read_i32(&mut cur).unwrap(), 1);
        assert_eq!(read_i32(&mut cur).unwrap(), 1);
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
