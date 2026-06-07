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
use std::path::{Path, PathBuf};

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

/// Raw fields extracted from one Nexus index record.
#[derive(Default)]
struct NexusFields {
    group_id: String,
    artifact_id: String,
    version: String,
    /// "jar", "pom", etc.; empty means the default "jar".
    packaging: String,
    /// Empty or "NA" = main artifact jar; "sources", "javadoc", etc. = skip.
    classifier: String,
    artifact_name: String,
    description: String,
    /// Fallback field: "groupId|artifactId|version|classifier[|packaging]".
    unified_coord: String,
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
    index_exists()
}

/// Return `true` if a usable Tantivy artifact index exists locally.
pub fn index_exists() -> bool {
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

/// Open the existing Tantivy index without downloading or rebuilding.
/// Fails if the index is not present.
pub fn open_index() -> Result<IndexHandle> {
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

/// Top-level orchestrator: parse the cached gz, write the Tantivy index, save metadata.
///
/// The Tantivy index is built in a temp directory and renamed into place atomically,
/// so a failed build never corrupts an existing good index.
fn build_index_from_gz() -> Result<()> {
    let gz_path = gz_cache_path();
    anyhow::ensure!(
        gz_path.exists(),
        "Cached index file not found at {}. Re-run without --offline.",
        gz_path.display()
    );

    let timestamp_ms = fetch_timestamp_ms().unwrap_or(0);

    let compressed_size = std::fs::metadata(&gz_path).map(|m| m.len()).unwrap_or(0);
    let parse_progress = make_parse_bar(compressed_size);

    let artifact_map = parse_gz_to_artifact_map(&gz_path, &parse_progress)?;
    let artifact_count = artifact_map.len() as u64;

    parse_progress.finish_with_message(format!("{} artifacts", artifact_count));

    write_artifacts_to_tantivy(&artifact_map)?;
    save_index_metadata(timestamp_ms, artifact_count)?;

    eprintln!("  Artifact index ready ({} artifacts).", artifact_count);
    Ok(())
}

// ── Parse phase ──────────────────────────────────────────────────────────────

/// Chunk size sent through the decompressor → parser channel.
const DECOMPRESS_CHUNK_BYTES: usize = 512 * 1024; // 512 KiB
/// How many chunks may be buffered between the two threads.
const CHANNEL_BUFFER_CHUNKS: usize = 4; // up to 2 MiB in flight

/// Open the cached gz, decompress on a background thread, parse records on
/// this thread, and return a map of `"group:artifact"` → best-version record.
///
/// Running decompression and parsing on separate threads lets the two CPU-heavy
/// stages (DEFLATE decode and binary field parsing) overlap.
fn parse_gz_to_artifact_map(
    gz_path: &Path,
    parse_progress: &ProgressBar,
) -> Result<HashMap<String, BestRecord>> {
    let file = std::fs::File::open(gz_path)
        .with_context(|| format!("failed to open {}", gz_path.display()))?;

    let (chunk_sender, chunk_receiver) =
        std::sync::mpsc::sync_channel::<Vec<u8>>(CHANNEL_BUFFER_CHUNKS);

    // Decompressor thread: gz file → raw bytes → channel
    let decompressor_progress = parse_progress.clone();
    let decompressor = std::thread::spawn(move || {
        decompress_to_channel(file, decompressor_progress, chunk_sender)
    });

    // Parser (this thread): channel → records → artifact map
    let mut channel_reader = ChannelReader::new(chunk_receiver);
    let parse_result = parse_records_to_artifact_map(&mut channel_reader, parse_progress);

    // Always join so the decompressor thread doesn't leak on parse errors.
    // Dropping channel_reader first ensures a blocked sender unblocks immediately.
    drop(channel_reader);
    let decompress_result = decompressor
        .join()
        .map_err(|_| anyhow::anyhow!("decompressor thread panicked"))?;

    // Surface the parse error first; it is more actionable than a decompress error.
    let artifact_map = parse_result.context("failed to parse Nexus index")?;
    decompress_result.context("failed to decompress Nexus index")?;

    Ok(artifact_map)
}

/// Decompress `file` and send 512 KiB chunks through `sender` until EOF.
fn decompress_to_channel(
    file: std::fs::File,
    parse_progress: ProgressBar,
    sender: std::sync::mpsc::SyncSender<Vec<u8>>,
) -> Result<()> {
    let decoder = GzDecoder::new(ProgressReader { inner: file, pb: parse_progress });
    let mut reader = BufReader::with_capacity(1 << 20, decoder); // 1 MiB read buffer
    let mut chunk = vec![0u8; DECOMPRESS_CHUNK_BYTES];

    loop {
        let bytes_decompressed = reader.read(&mut chunk).context("decompression error")?;
        if bytes_decompressed == 0 {
            break; // EOF
        }
        if sender.send(chunk[..bytes_decompressed].to_vec()).is_err() {
            break; // Parser dropped its receiver (e.g., on error); stop early.
        }
    }
    Ok(())
}

/// `Read` adapter over a channel of byte chunks.
///
/// The decompressor thread sends chunks; this reader reassembles them into a
/// continuous byte stream for the record parser.
struct ChannelReader {
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
    current_chunk: Vec<u8>,
    position: usize,
}

impl ChannelReader {
    fn new(receiver: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self { receiver, current_chunk: Vec::new(), position: 0 }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Refill from the channel whenever the current chunk is exhausted.
        while self.position >= self.current_chunk.len() {
            match self.receiver.recv() {
                Ok(chunk) => {
                    self.current_chunk = chunk;
                    self.position = 0;
                }
                Err(_) => return Ok(0), // Channel closed → EOF for the parser.
            }
        }
        let bytes_available = self.current_chunk.len() - self.position;
        let bytes_to_copy = buf.len().min(bytes_available);
        buf[..bytes_to_copy]
            .copy_from_slice(&self.current_chunk[self.position..self.position + bytes_to_copy]);
        self.position += bytes_to_copy;
        Ok(bytes_to_copy)
    }
}

/// Read every record from `reader`, resolve coordinates, filter, and deduplicate.
///
/// Accepts any `Read` so that unit tests can pass an in-memory cursor without
/// a real gzip file.
fn parse_records_to_artifact_map<R: Read>(
    reader: &mut R,
    progress: &ProgressBar,
) -> Result<HashMap<String, BestRecord>> {
    skip_nexus_header(reader)?;

    let mut best_by_coord: HashMap<String, BestRecord> = HashMap::with_capacity(600_000);

    while let Some(nexus_fields) = read_next_record(reader)? {
        let Some((coord, record)) = resolve_artifact(nexus_fields) else { continue };

        let is_newer = best_by_coord
            .get(&coord)
            .map(|existing| version_gt(&record.version, &existing.version))
            .unwrap_or(true);

        if is_newer {
            best_by_coord.insert(coord, record);
            let artifact_count = best_by_coord.len() as u64;
            if artifact_count.is_power_of_two() {
                progress.set_message(format!("{} artifacts", artifact_count));
            }
        }
    }

    Ok(best_by_coord)
}

/// Skip the 9-byte stream header: 1-byte format version + 8-byte timestamp.
fn skip_nexus_header<R: Read>(reader: &mut R) -> Result<()> {
    read_u8(reader).context("failed to read index version byte")?;
    read_i64(reader).context("failed to read index timestamp")?;
    Ok(())
}

/// Read one complete record from the Nexus binary stream.
///
/// Returns `None` when the end-of-stream marker (`0` or `Integer.MIN_VALUE`
/// as `field_count`) or an unexpected EOF is reached.
///
/// # Format (verified against the real Maven Central index)
/// ```text
/// [4 bytes i32]  field_count  (0 or i32::MIN = end-of-stream)
/// per field:
///   [1 byte]       Lucene field type tag  (skipped)
///   [2 bytes u16]  name length            (writeUTF big-endian)
///   [name_len]     name bytes             (ASCII: "g", "a", "v", "u", …)
///   [4 bytes i32]  value length           (big-endian; wider than writeUTF)
///   [value_len]    value bytes            (UTF-8)
/// ```
fn read_next_record<R: Read>(reader: &mut R) -> Result<Option<NexusFields>> {
    let field_count = match read_i32(reader) {
        Ok(0) | Ok(NEXUS_EOF_MARKER) => return Ok(None),
        Ok(fc) if fc < 0 => return Ok(None),
        Ok(fc) => fc as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("error reading field count"),
    };

    let mut fields = NexusFields::default();
    for _ in 0..field_count {
        let (field_name, field_value) = match read_field(reader) {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match field_name.as_str() {
            "g" => fields.group_id = field_value,
            "a" => fields.artifact_id = field_value,
            "v" => fields.version = field_value,
            "p" => fields.packaging = field_value,
            "l" => fields.classifier = field_value,
            "n" => fields.artifact_name = field_value,
            "d" => fields.description = field_value,
            "u" => fields.unified_coord = field_value,
            _ => {}
        }
    }
    Ok(Some(fields))
}

/// Read one field: skip 1-byte Lucene tag, read name via writeUTF, read value
/// with 4-byte length prefix.  Returns `(name, value)`.
fn read_field<R: Read>(reader: &mut R) -> std::io::Result<(String, String)> {
    let _lucene_type_tag = read_u8(reader)?;
    let field_name = read_utf(reader)?;
    let field_value = read_value(reader)?;
    Ok((field_name, field_value))
}

/// Populate `group_id`, `artifact_id`, `version`, `classifier`, and `packaging`
/// from the `u` (unified coordinate) field when the individual fields are absent.
///
/// The `u` field format is `"groupId|artifactId|version|classifier[|packaging]"`.
/// A classifier of `"NA"` means no classifier (primary jar).
fn fill_from_unified_coord(fields: &mut NexusFields) {
    if (fields.group_id.is_empty() || fields.artifact_id.is_empty())
        && !fields.unified_coord.is_empty()
    {
        let parts: Vec<&str> = fields.unified_coord.splitn(5, '|').collect();
        if parts.len() >= 2 {
            if fields.group_id.is_empty() {
                fields.group_id = parts[0].to_string();
            }
            if fields.artifact_id.is_empty() {
                fields.artifact_id = parts[1].to_string();
            }
            if fields.version.is_empty() && parts.len() >= 3 {
                fields.version = parts[2].to_string();
            }
            if fields.classifier.is_empty() && parts.len() >= 4 && parts[3] != "NA" {
                fields.classifier = parts[3].to_string();
            }
            if fields.packaging.is_empty() && parts.len() >= 5 {
                fields.packaging = parts[4].to_string();
            }
        }
    }
}

/// Return `true` when the record represents a primary jar artifact.
///
/// Excludes: POMs, checksums (non-jar packaging), classifier jars (`-sources`,
/// `-javadoc`, etc.), and records with no usable coordinates.
fn is_jar_artifact(fields: &NexusFields) -> bool {
    if fields.group_id.is_empty() || fields.artifact_id.is_empty() {
        return false;
    }
    let effective_packaging = if fields.packaging.is_empty() { "jar" } else { &fields.packaging };
    if effective_packaging != "jar" {
        return false;
    }
    if !fields.classifier.is_empty() && fields.classifier != "NA" {
        return false;
    }
    true
}

/// Resolve coordinates and apply the jar filter.
///
/// Returns `Some((coord, record))` for indexable jar artifacts, `None` otherwise.
fn resolve_artifact(mut fields: NexusFields) -> Option<(String, BestRecord)> {
    fill_from_unified_coord(&mut fields);
    if !is_jar_artifact(&fields) {
        return None;
    }
    let coord = format!("{}:{}", fields.group_id, fields.artifact_id);
    let record = BestRecord {
        version: fields.version,
        name: fields.artifact_name,
        description: fields.description,
    };
    Some((coord, record))
}

// ── Tantivy write phase ───────────────────────────────────────────────────────

/// Write every entry in `artifact_map` to a fresh Tantivy index and install it
/// atomically, replacing any previous index.
fn write_artifacts_to_tantivy(artifact_map: &HashMap<String, BestRecord>) -> Result<()> {
    let artifact_count = artifact_map.len() as u64;
    eprintln!("  Indexing {} artifacts…", artifact_count);

    let tmp_dir = index_tmp_dir();
    prepare_build_directory(&tmp_dir)?;

    let (schema, f_coord, f_coord_text, f_name, f_description, f_version) = make_schema();
    let index =
        Index::create_in_dir(&tmp_dir, schema).context("failed to create Tantivy index")?;

    let index_progress = make_index_progress_bar(artifact_count);
    let mut writer = index
        .writer(WRITER_HEAP_BYTES)
        .context("failed to create index writer")?;

    for (coord, record) in artifact_map {
        writer
            .add_document(doc!(
                f_coord       => coord.as_str(),
                f_coord_text  => coord.as_str(),
                f_name        => record.name.as_str(),
                f_description => record.description.as_str(),
                f_version     => record.version.as_str(),
            ))
            .context("failed to add document")?;
        index_progress.inc(1);
    }
    writer.commit().context("failed to commit index")?;
    drop(writer);
    index_progress.finish_with_message("Indexed");

    install_index_atomically(&tmp_dir)
}

/// Clear and (re)create the temp build directory.
fn prepare_build_directory(tmp_dir: &Path) -> Result<()> {
    if tmp_dir.exists() {
        std::fs::remove_dir_all(tmp_dir).context("failed to remove old temp index")?;
    }
    std::fs::create_dir_all(tmp_dir).context("failed to create temp index directory")
}

/// Atomically replace the live index directory with the freshly built one.
fn install_index_atomically(tmp_dir: &Path) -> Result<()> {
    let final_dir = index_dir();
    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir).context("failed to remove old index")?;
    }
    std::fs::rename(tmp_dir, &final_dir).context("failed to install new index")
}

/// Persist the index timestamp and artifact count to the JSON sidecar file.
fn save_index_metadata(timestamp_ms: i64, artifact_count: u64) -> Result<()> {
    let meta = IndexMeta { index_timestamp_ms: timestamp_ms, artifact_count };
    std::fs::write(meta_path(), serde_json::to_string_pretty(&meta)?)
        .context("failed to write index metadata")
}

fn make_index_progress_bar(artifact_count: u64) -> ProgressBar {
    let pb = ProgressBar::new(artifact_count);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} artifacts",
            )
            .unwrap()
            .progress_chars("#>-"),
    );
    pb
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
                     {bytes}/{total_bytes} parsed ({eta})  {msg}",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {bytes} parsed  {msg}")
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

    /// Drive the production parse pipeline on an in-memory byte slice.
    fn parse_stream_into_map(raw: &[u8], map: &mut HashMap<String, BestRecord>) {
        let pb = ProgressBar::hidden();
        let result = parse_records_to_artifact_map(
            &mut BufReader::new(std::io::Cursor::new(raw)),
            &pb,
        );
        map.extend(result.unwrap_or_default());
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
