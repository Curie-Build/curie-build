//! Generic plugin framework.
//!
//! Protocol (two-phase, JSON over stdin/stdout):
//!
//! 1. `curie-<name> manifest --project <dir>`
//!    stdin:  envelope JSON  (curie_version + config)
//!    stdout: manifest JSON  (types, inputs, outputs, artifacts)
//!
//! 2. `curie-<name> generate-sources --project <dir> [--offline]`
//!    stdin:  envelope JSON  (curie_version + config + artifacts map)
//!    stdout: (not parsed)
//!    stderr: progress, visible to the user

use anyhow::{Context, Result};
use curie_deps::repo::Repository;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::UNIX_EPOCH;

// ── Manifest types (re-exported from curie-plugin) ────────────────────────────

pub use curie_plugin::Artifact as PluginArtifact;
pub use curie_plugin::Manifest as PluginManifest;

// ── Stamp types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Stamp {
    dir_mtimes: Vec<MtimeEntry>,
    file_mtimes: Vec<MtimeEntry>,
    /// SHA-256 of the config envelope + plugin manifest version.  Empty in
    /// stamps written before this field existed, which forces one regeneration.
    #[serde(default)]
    config_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MtimeEntry {
    path: PathBuf,
    mtime_ns: u128,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawn the child with piped stdin and return the child plus a thread that
/// performs the write (and drops the handle to send EOF). This prevents
/// blocking the main thread on large envelopes and allows us to wait for the
/// child even if the write fails (e.g. EPIPE when child exited early).
fn spawn_with_stdin_thread(
    mut cmd: std::process::Command,
    data: &[u8],
) -> Result<(std::process::Child, thread::JoinHandle<io::Result<()>>)> {
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {}", cmd.get_program().to_string_lossy()))?;
    let mut stdin = child
        .stdin
        .take()
        .expect("stdin was requested as piped");
    let data = data.to_vec();
    let handle = thread::spawn(move || {
        let res = stdin.write_all(&data);
        drop(stdin); // send EOF to child
        res
    });
    Ok((child, handle))
}

/// Given the result of the stdin write thread and the child's exit status,
/// decide what error (if any) to surface. We prefer the child's status when
/// it exited unsuccessfully (the real error is typically on the inherited
/// stderr that the user already saw).
fn check_write_result_vs_status(
    write_res: io::Result<()>,
    status: std::process::ExitStatus,
    bin_name: &str,
) -> Result<()> {
    if let Err(e) = write_res {
        if status.success() {
            // Write failed even though child reported success — unusual, surface it.
            return Err(e).with_context(|| format!("failed to write stdin to {bin_name}"));
        }
        // Child exited with failure (EPIPE or other write error); report child's status.
        anyhow::bail!(
            "{bin_name} exited with status {:?}",
            status.code()
        );
    }
    if !status.success() {
        anyhow::bail!(
            "{bin_name} exited with status {:?}",
            status.code()
        );
    }
    Ok(())
}

/// Invoke `curie-<name> manifest`, returning the parsed manifest.
pub fn fetch_manifest(
    name: &str,
    envelope_json: &str,
    project_root: &Path,
) -> Result<PluginManifest> {
    let bin_name = format!("curie-{name}");
    let bin = which::which(&bin_name)
        .with_context(|| format!("{bin_name} not found on PATH (required by [plugin.{name}])"))?;

    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["manifest", "--project"])
        .arg(project_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let (child, write_th) =
        spawn_with_stdin_thread(cmd, envelope_json.as_bytes())?;

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {bin_name}"))?;

    let write_res = write_th.join().expect("stdin writer thread panicked");
    check_write_result_vs_status(write_res, output.status, &bin_name)?;

    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{bin_name} manifest produced invalid JSON on stdout"))
}

/// Return true when all inputs recorded in the stamp are still unchanged.
///
/// `config_hash` is the current hash of the config envelope + plugin version
/// (see [`config_hash`]); a mismatch means the plugin config or version changed
/// and generation must re-run even when no input file did.
pub fn is_up_to_date(
    manifest: &PluginManifest,
    stamp_path: &Path,
    project_root: &Path,
    config_hash: &str,
) -> bool {
    let stamp = match read_stamp(stamp_path) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Config envelope or plugin version changed → stale even if files didn't.
    if stamp.config_hash != config_hash {
        return false;
    }

    // Verify directory mtimes (detects added / removed files).
    for entry in &stamp.dir_mtimes {
        let current = mtime_ns(&project_root.join(&entry.path));
        if current != entry.mtime_ns {
            return false;
        }
    }

    // Verify individual file mtimes.
    for entry in &stamp.file_mtimes {
        let current = mtime_ns(&project_root.join(&entry.path));
        if current != entry.mtime_ns {
            return false;
        }
    }

    // Also verify that the set of input dirs/files the manifest declares
    // matches what was recorded (detects config changes).
    let stamp_dir_paths: Vec<&Path> = stamp.dir_mtimes.iter().map(|e| e.path.as_path()).collect();
    let manifest_dir_paths: Vec<&Path> = manifest.inputs.dirs.iter().map(|p| p.as_path()).collect();
    if stamp_dir_paths != manifest_dir_paths {
        return false;
    }

    true
}

/// Download all artifacts declared in the manifest; return a map from `id` to
/// local filesystem path.  Cache hits never touch the network.
pub fn download_artifacts(
    artifacts: &[PluginArtifact],
    repos: &[Repository],
    offline: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut resolved = BTreeMap::new();
    for art in artifacts {
        let path = download_artifact(art, repos, offline)?;
        resolved.insert(art.id.clone(), path);
    }
    Ok(resolved)
}

/// Invoke `curie-<name> generate-sources`, passing the envelope + artifact paths on stdin.
pub fn generate_sources(
    name: &str,
    config_envelope: &str,
    resolved: &BTreeMap<String, PathBuf>,
    project_root: &Path,
    offline: bool,
) -> Result<()> {
    let bin_name = format!("curie-{name}");
    let bin = which::which(&bin_name)
        .with_context(|| format!("{bin_name} not found on PATH"))?;

    // Merge the config envelope with the resolved artifact paths.
    let mut envelope: serde_json::Value = serde_json::from_str(config_envelope)
        .context("internal: failed to parse config envelope")?;
    let artifacts_json: serde_json::Value = resolved
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.display().to_string())))
        .collect::<serde_json::Map<_, _>>()
        .into();
    envelope["artifacts"] = artifacts_json;
    let run_json = serde_json::to_string(&envelope).context("internal: failed to serialize run envelope")?;

    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["generate-sources", "--project"])
        .arg(project_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if offline {
        cmd.arg("--offline");
    }

    let (mut child, write_th) = spawn_with_stdin_thread(cmd, run_json.as_bytes())?;

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {bin_name}"))?;

    let write_res = write_th.join().expect("stdin writer thread panicked");
    check_write_result_vs_status(write_res, status, &bin_name)?;

    Ok(())
}

/// Hash the inputs that should trigger regeneration when changed: the full
/// config envelope (`curie_version` + the `[plugin.<name>]` config) plus the
/// plugin's own manifest version.  Returned as a hex SHA-256 string.
pub fn config_hash(envelope_json: &str, manifest_version: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(manifest_version.as_bytes());
    h.update(b"\n");
    h.update(envelope_json.as_bytes());
    hex::encode(h.finalize())
}

/// Write a fresh stamp file reflecting current input mtimes from the manifest
/// plus the current config/version hash.
pub fn write_stamp(
    manifest: &PluginManifest,
    stamp_path: &Path,
    project_root: &Path,
    config_hash: &str,
) -> Result<()> {
    let dir_mtimes = manifest
        .inputs
        .dirs
        .iter()
        .map(|d| {
            let abs = project_root.join(d);
            MtimeEntry { path: d.clone(), mtime_ns: mtime_ns(&abs) }
        })
        .collect();

    let file_mtimes = collect_input_files(manifest, project_root)
        .into_iter()
        .map(|abs| {
            let rel = abs.strip_prefix(project_root).unwrap_or(&abs).to_path_buf();
            MtimeEntry { path: rel, mtime_ns: mtime_ns(&abs) }
        })
        .collect();

    let stamp = Stamp { dir_mtimes, file_mtimes, config_hash: config_hash.to_string() };

    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent).context("failed to create .curie-plugins dir")?;
    }
    let json = serde_json::to_string_pretty(&stamp).context("failed to serialize stamp")?;
    std::fs::write(stamp_path, json).context("failed to write stamp file")?;
    Ok(())
}

/// Path of the output-set stamp for `plugin_name` under `plugins_dir`
/// (`target/.curie-plugins/<name>.output-set`).
pub fn plugin_output_set_stamp_path(plugins_dir: &Path, plugin_name: &str) -> PathBuf {
    plugins_dir.join(format!("{plugin_name}.output-set"))
}

/// Canonical set of every file currently present in the plugin's declared output
/// directories.  Files that no longer exist on disk are dropped automatically
/// (canonicalize fails for missing paths), so the result always reflects what is
/// actually on disk at call time.
pub fn current_plugin_output_set(
    manifest: &PluginManifest,
    project_root: &Path,
) -> BTreeSet<String> {
    let files: Vec<PathBuf> = manifest
        .outputs
        .source_dirs
        .iter()
        .flat_map(|d| {
            let abs_dir = project_root.join(d);
            crate::incremental::walk_files(&abs_dir)
                .map(|e| e.into_path())
                .collect::<Vec<_>>()
        })
        .collect();
    crate::incremental::canonical_source_set(&files)
}

/// Delete every file that was generated on the previous plugin run (`prev`) but
/// is absent from the current run's output (`current`).  Returns the paths that
/// were successfully deleted, for progress logging.  Files that fail to remove
/// (e.g. already gone) are silently skipped.
pub fn wipe_orphaned_plugin_outputs(
    prev: &BTreeSet<String>,
    current: &BTreeSet<String>,
) -> Vec<PathBuf> {
    prev.difference(current)
        .filter_map(|p| {
            let path = PathBuf::from(p);
            std::fs::remove_file(&path).ok().map(|_| path)
        })
        .collect()
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn read_stamp(path: &Path) -> Result<Stamp> {
    let content = std::fs::read_to_string(path).context("stamp not found")?;
    serde_json::from_str(&content).context("invalid stamp JSON")
}

fn mtime_ns(path: &Path) -> u128 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Walk manifest input dirs and collect matching files.
fn collect_input_files(manifest: &PluginManifest, project_root: &Path) -> Vec<PathBuf> {
    let regex = manifest.inputs.file_regex.as_deref().and_then(|r| {
        regex::Regex::new(r).ok()
    });

    let mut files: Vec<PathBuf> = manifest
        .inputs
        .files
        .iter()
        .map(|f| project_root.join(f))
        .collect();

    for dir in &manifest.inputs.dirs {
        let abs_dir = project_root.join(dir);
        if let Ok(walker) = walkdir::WalkDir::new(&abs_dir)
            .min_depth(1)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
        {
            for entry in walker {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(&abs_dir)
                    .unwrap_or(path)
                    .to_string_lossy();
                if let Some(re) = &regex {
                    if !re.is_match(&rel) {
                        continue;
                    }
                }
                files.push(path.to_path_buf());
            }
        }
    }

    files.sort();
    files
}

fn download_artifact(art: &PluginArtifact, repos: &[Repository], offline: bool) -> Result<PathBuf> {
    // Delegate to the hardened resolver path.  This gives us:
    // - sidecar persistence + re-verification on cache hits
    // - atomic writes via unique staging files
    // - proper HTTP client (timeout + UA)
    // - respect for mirrors and configured repositories
    // - in-process deduplication (from the bug #2 gate)
    let key = format!("{}:{}", art.group, art.artifact);
    let mut gav = curie_deps::Gav::from_key_version_classifier(
        &key,
        &art.version,
        art.classifier.as_deref(),
    )?;
    gav.extension = Some(art.extension.clone());

    let path = curie_deps::fetch_artifact_file(
        &art.group,
        &art.artifact,
        &art.version,
        art.classifier.as_deref(),
        &art.extension,
        repos,
        offline,
    )?;

    if art.executable {
        set_executable(&path)?;
    }
    Ok(path)
}

#[cfg(test)]
fn artifact_cache_path(art: &PluginArtifact) -> Result<PathBuf> {
    // Delegate to Gav so we get the common validation (bug #21) and single
    // source of truth for local repository layout.
    let key = format!("{}:{}", art.group, art.artifact);
    let mut gav = curie_deps::Gav::from_key_version_classifier_extension(
        &key,
        &art.version,
        art.classifier.as_deref(),
        &art.extension,
    )?;
    // Note: extension may be set already by the from_ call.
    Ok(gav.local_repository_path()?)
}

#[cfg(test)]
fn artifact_relative_path(art: &PluginArtifact) -> String {
    // Delegate to Gav (gets validation for free).
    let key = format!("{}:{}", art.group, art.artifact);
    let mut gav = curie_deps::Gav::from_key_version_classifier_extension(
        &key,
        &art.version,
        art.classifier.as_deref(),
        &art.extension,
    )
    .expect("test artifact must be valid");
    gav.relative_path()
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn fake_art() -> PluginArtifact {
        PluginArtifact {
            id: "testlib".into(),
            group: "com.example".into(),
            artifact: "testlib".into(),
            version: "1.0.0".into(),
            classifier: None,
            extension: "jar".into(),
            executable: false,
        }
    }

    fn nonexistent_art() -> PluginArtifact {
        // Coordinate that will never exist in a real ~/.m2 cache.
        PluginArtifact {
            id: "x".into(),
            group: "com.example.curie-test".into(),
            artifact: "nonexistent-test-artifact".into(),
            version: "0.0.0-TEST-ONLY".into(),
            classifier: None,
            extension: "jar".into(),
            executable: false,
        }
    }

    fn test_repo(url: &str) -> Repository {
        Repository { id: "test".into(), name: "Test".into(), url: url.to_string() }
    }

    fn sha1_hex(data: &[u8]) -> String {
        use sha1::Digest as _;
        hex::encode(sha1::Sha1::digest(data))
    }

    fn sample_manifest() -> PluginManifest {
        serde_json::from_str(r#"{
            "name": "test",
            "description": "test plugin",
            "version": "0.1.0",
            "types": ["source-generator"],
            "inputs": {
                "dirs": ["proto"],
                "file_regex": "\\.proto$"
            },
            "outputs": { "source_dirs": ["target/generated-sources/protobuf"] },
            "artifacts": []
        }"#).unwrap()
    }

    /// Fixed config hash used by the input-tracking tests, which only care
    /// about file/dir staleness and keep the config constant.
    const TEST_HASH: &str = "test-config-hash";

    fn write_stamp_at(dir: &Path, manifest: &PluginManifest) {
        let stamp_path = dir.join("stamp.json");
        write_stamp(manifest, &stamp_path, dir, TEST_HASH).unwrap();
    }

    #[test]
    fn stale_when_stamp_absent() {
        let tmp = TempDir::new().unwrap();
        let manifest = sample_manifest();
        fs::create_dir_all(tmp.path().join("proto")).unwrap();
        let stamp = tmp.path().join("nonexistent.stamp");
        assert!(!is_up_to_date(&manifest, &stamp, tmp.path(), TEST_HASH));
    }

    #[test]
    fn up_to_date_when_stamp_matches() {
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();
        fs::write(proto_dir.join("foo.proto"), b"syntax = \"proto3\";").unwrap();

        let manifest = sample_manifest();
        write_stamp_at(tmp.path(), &manifest);
        let stamp_path = tmp.path().join("stamp.json");
        assert!(is_up_to_date(&manifest, &stamp_path, tmp.path(), TEST_HASH));
    }

    #[test]
    fn stale_when_file_modified() {
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();
        let proto_file = proto_dir.join("foo.proto");
        fs::write(&proto_file, b"syntax = \"proto3\";").unwrap();

        let manifest = sample_manifest();
        write_stamp_at(tmp.path(), &manifest);
        let stamp_path = tmp.path().join("stamp.json");

        // Advance the file's mtime by rewriting it.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&proto_file, b"syntax = \"proto3\"; // modified").unwrap();

        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path(), TEST_HASH));
    }

    #[test]
    fn stale_when_file_added_to_watched_dir() {
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();

        let manifest = sample_manifest();
        write_stamp_at(tmp.path(), &manifest);
        let stamp_path = tmp.path().join("stamp.json");

        // Add a new file — the directory mtime changes.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(proto_dir.join("new.proto"), b"syntax = \"proto3\";").unwrap();

        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path(), TEST_HASH));
    }

    #[test]
    fn stale_when_config_hash_changes() {
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();
        fs::write(proto_dir.join("foo.proto"), b"syntax = \"proto3\";").unwrap();

        let manifest = sample_manifest();
        // Stamp written with one config hash...
        write_stamp_at(tmp.path(), &manifest);
        let stamp_path = tmp.path().join("stamp.json");

        // ...but the current build has a different config hash (e.g. modelPackage
        // changed).  No input file moved, yet the plugin must re-run.
        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path(), "different-hash"));
    }

    #[test]
    fn up_to_date_when_config_hash_matches() {
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();
        fs::write(proto_dir.join("foo.proto"), b"syntax = \"proto3\";").unwrap();

        let manifest = sample_manifest();
        write_stamp(&manifest, &tmp.path().join("stamp.json"), tmp.path(), "abc123").unwrap();
        let stamp_path = tmp.path().join("stamp.json");
        assert!(is_up_to_date(&manifest, &stamp_path, tmp.path(), "abc123"));
    }

    #[test]
    fn stale_when_stamp_predates_config_hash_field() {
        // A legacy stamp written before config_hash existed deserializes with an
        // empty hash, which never matches a real hash → one forced regeneration.
        let tmp = TempDir::new().unwrap();
        let proto_dir = tmp.path().join("proto");
        fs::create_dir_all(&proto_dir).unwrap();
        fs::write(proto_dir.join("foo.proto"), b"syntax = \"proto3\";").unwrap();

        let manifest = sample_manifest();
        let stamp_path = tmp.path().join("stamp.json");
        // Emulate an old stamp: write a fresh one, then strip the config_hash key.
        write_stamp(&manifest, &stamp_path, tmp.path(), "real-hash").unwrap();
        let json = fs::read_to_string(&stamp_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("config_hash");
        fs::write(&stamp_path, serde_json::to_string(&value).unwrap()).unwrap();

        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path(), "real-hash"));
    }

    #[test]
    fn config_hash_changes_with_config_and_version() {
        let env_a = r#"{"curie_version":"0.6.0","config":{"modelPackage":"a"}}"#;
        let env_b = r#"{"curie_version":"0.6.0","config":{"modelPackage":"b"}}"#;
        // Different config → different hash.
        assert_ne!(config_hash(env_a, "1.0.0"), config_hash(env_b, "1.0.0"));
        // Different plugin version → different hash.
        assert_ne!(config_hash(env_a, "1.0.0"), config_hash(env_a, "2.0.0"));
    }

    #[test]
    fn config_hash_stable_for_same_input() {
        let env = r#"{"curie_version":"0.6.0","config":{"version":"3.25.0"}}"#;
        assert_eq!(config_hash(env, "1.0.0"), config_hash(env, "1.0.0"));
    }

    #[test]
    fn artifact_cache_path_layout() {
        let art = PluginArtifact {
            id: "protoc".into(),
            group: "com.google.protobuf".into(),
            artifact: "protoc".into(),
            version: "3.25.0".into(),
            classifier: Some("linux-x86_64".into()),
            extension: "exe".into(),
            executable: true,
        };
        let path = artifact_cache_path(&art).unwrap();
        let s = path.to_string_lossy();
        assert!(s.contains("com/google/protobuf/protoc/3.25.0"), "got: {s}");
        assert!(s.contains("protoc-3.25.0-linux-x86_64.exe"), "got: {s}");
    }

    #[test]
    fn artifact_relative_path_with_classifier() {
        let art = PluginArtifact {
            id: "protoc".into(),
            group: "com.google.protobuf".into(),
            artifact: "protoc".into(),
            version: "3.25.0".into(),
            classifier: Some("linux-x86_64".into()),
            extension: "exe".into(),
            executable: true,
        };
        assert_eq!(
            artifact_relative_path(&art),
            "com/google/protobuf/protoc/3.25.0/protoc-3.25.0-linux-x86_64.exe"
        );
    }

    #[test]
    fn artifact_relative_path_no_classifier() {
        let art = PluginArtifact {
            id: "foo".into(),
            group: "com.example".into(),
            artifact: "foo".into(),
            version: "1.0".into(),
            classifier: None,
            extension: "jar".into(),
            executable: false,
        };
        assert_eq!(
            artifact_relative_path(&art),
            "com/example/foo/1.0/foo-1.0.jar"
        );
    }

    // ── download_artifact behaviour ──────────────────────────────────────────

    #[test]
    fn download_artifact_errors_when_no_repos_configured() {
        let art = nonexistent_art();
        let result = download_artifact(&art, &[], false);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("no repositories configured") || msg.contains("could not download"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn download_artifact_offline_without_cache_fails() {
        let art = nonexistent_art();
        // Pass any repo; the offline check fires before any network call.
        let result = download_artifact(&art, &[test_repo("https://example.com/m2")], true);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("offline"), "expected 'offline' in: {msg}");
    }

    #[test]
    fn download_artifact_fetches_from_provided_repo_not_hardcoded_central() {
        let tmp = TempDir::new().unwrap();
        let art = fake_art();
        let body: &[u8] = b"fake-jar-bytes";
        let sha1 = sha1_hex(body);
        let rel = artifact_relative_path(&art);

        let mut server = mockito::Server::new();
        let _m_jar = server
            .mock("GET", format!("/{rel}").as_str())
            .with_status(200)
            .with_body(body)
            .create();
        // The hardened path prefers .sha256; return 404 so it falls back to .sha1.
        let _m_sha256 = server
            .mock("GET", format!("/{rel}.sha256").as_str())
            .with_status(404)
            .create();
        let _m_sha1 = server
            .mock("GET", format!("/{rel}.sha1").as_str())
            .with_status(200)
            .with_body(sha1.as_str())
            .create();

        let repos = vec![test_repo(&server.url())];
        {
            let _home = crate::testenv::set_home(tmp.path());
            let result = download_artifact(&art, &repos, false);
            assert!(result.is_ok(), "expected success: {:#}", result.unwrap_err());
        }

        // The mock was hit — meaning the repo URL was used, not Maven Central.
        _m_jar.assert();
        _m_sha256.assert();
        _m_sha1.assert();
    }

    #[test]
    fn download_artifact_falls_back_to_second_repo_when_first_returns_404() {
        let tmp = TempDir::new().unwrap();
        let art = fake_art();
        let body: &[u8] = b"fake-jar-bytes";
        let sha1 = sha1_hex(body);
        let rel = artifact_relative_path(&art);

        let mut server = mockito::Server::new();
        // First repo: artifact returns 404.
        let _m_404_jar = server
            .mock("GET", format!("/{rel}").as_str())
            .with_status(404)
            .create();
        // Second repo (different server instance via a second mockito server).
        let mut server2 = mockito::Server::new();
        let _m2_jar = server2
            .mock("GET", format!("/{rel}").as_str())
            .with_status(200)
            .with_body(body)
            .create();
        // Hardened path tries .sha256 first (404) then .sha1.
        let _m2_sha256 = server2
            .mock("GET", format!("/{rel}.sha256").as_str())
            .with_status(404)
            .create();
        let _m2_sha1 = server2
            .mock("GET", format!("/{rel}.sha1").as_str())
            .with_status(200)
            .with_body(sha1.as_str())
            .create();

        let repos = vec![test_repo(&server.url()), test_repo(&server2.url())];
        {
            let _home = crate::testenv::set_home(tmp.path());
            let result = download_artifact(&art, &repos, false);
            assert!(result.is_ok(), "expected fallback success: {:#}", result.unwrap_err());
        }

        _m2_jar.assert();
        _m2_sha256.assert();
        _m2_sha1.assert();
    }

    // ── output-set tracking ──────────────────────────────────────────────────

    fn manifest_with_output_dir(dir: &Path) -> PluginManifest {
        serde_json::from_str(&format!(
            r#"{{
                "name": "test",
                "description": "test plugin",
                "version": "0.1.0",
                "types": ["source-generator"],
                "inputs": {{"dirs": [], "file_regex": null, "files": []}},
                "outputs": {{"source_dirs": ["{}"]}},
                "artifacts": []
            }}"#,
            dir.to_string_lossy().replace('\\', "/"),
        ))
        .unwrap()
    }

    #[test]
    fn plugin_output_set_captures_generated_files() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("gen");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("Foo.java"), b"class Foo {}").unwrap();
        fs::write(out_dir.join("Bar.java"), b"class Bar {}").unwrap();

        let manifest = manifest_with_output_dir(&out_dir);
        let set = current_plugin_output_set(&manifest, tmp.path());

        assert_eq!(set.len(), 2);
        assert!(set.iter().any(|p| p.ends_with("Foo.java")));
        assert!(set.iter().any(|p| p.ends_with("Bar.java")));
    }

    #[test]
    fn outputs_intact_when_all_stamped_files_present() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("gen");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("Foo.java"), b"class Foo {}").unwrap();

        let manifest = manifest_with_output_dir(&out_dir);
        let stamped_set = current_plugin_output_set(&manifest, tmp.path());
        let on_disk_set = current_plugin_output_set(&manifest, tmp.path());

        assert!(on_disk_set.is_superset(&stamped_set));
    }

    #[test]
    fn outputs_not_intact_when_file_deleted() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("gen");
        fs::create_dir_all(&out_dir).unwrap();
        let foo = out_dir.join("Foo.java");
        fs::write(&foo, b"class Foo {}").unwrap();

        let manifest = manifest_with_output_dir(&out_dir);
        let stamped_set = current_plugin_output_set(&manifest, tmp.path());

        // Simulate manual deletion.
        fs::remove_file(&foo).unwrap();

        let on_disk_set = current_plugin_output_set(&manifest, tmp.path());
        assert!(!on_disk_set.is_superset(&stamped_set));
    }

    #[test]
    fn wipe_orphaned_outputs_removes_stale_files() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("Foo.java");
        let bar = tmp.path().join("Bar.java");
        fs::write(&foo, b"class Foo {}").unwrap();
        fs::write(&bar, b"class Bar {}").unwrap();

        let prev: BTreeSet<String> = [
            foo.canonicalize().unwrap().to_string_lossy().into_owned(),
            bar.canonicalize().unwrap().to_string_lossy().into_owned(),
        ]
        .into();
        // Current set no longer contains Foo — it's an orphan.
        let current: BTreeSet<String> =
            [bar.canonicalize().unwrap().to_string_lossy().into_owned()].into();

        let wiped = wipe_orphaned_plugin_outputs(&prev, &current);
        assert_eq!(wiped.len(), 1);
        assert!(!foo.exists(), "orphan should have been deleted");
        assert!(bar.exists(), "current file must be kept");
    }

    #[test]
    fn wipe_orphaned_outputs_keeps_current_files() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("Foo.java");
        fs::write(&foo, b"class Foo {}").unwrap();

        let path = foo.canonicalize().unwrap().to_string_lossy().into_owned();
        let set: BTreeSet<String> = [path].into();

        // Same set in prev and current — nothing to wipe.
        let wiped = wipe_orphaned_plugin_outputs(&set, &set);
        assert!(wiped.is_empty());
        assert!(foo.exists());
    }

    // ── stdin write thread + error masking fix (bug #14) ──────────────────────

    #[cfg(unix)]
    fn make_fake_plugin_script(dir: &Path, name: &str, script_body: &str) -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, script_body)?;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
        Ok(path)
    }

    #[test]
    #[cfg(unix)]
    fn fetch_manifest_large_envelope_no_deadlock_and_parses() {
        let tmp = TempDir::new().unwrap();
        // Script: read all stdin (to unblock writer), emit a minimal valid manifest on stdout.
        let script = r#"#!/bin/sh
cat >/dev/null
echo '{"name":"dummy","description":"test","version":"0.0.0","types":[],"inputs":{"dirs":[],"files":[]},"outputs":{"source_dirs":[]},"artifacts":[]}'
"#;
        let script_path = make_fake_plugin_script(tmp.path(), "curie-dummy", script).unwrap();

        // Prepend script dir to PATH for this test only.
        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", tmp.path().display(), old_path);
        std::env::set_var("PATH", &new_path);

        // Large envelope (>64KiB) that would previously risk deadlock with blocking write_all.
        let big = "x".repeat(128 * 1024);
        let envelope = format!(
            r#"{{"curie_version":"0.6.0","config":{{"big":"{}"}}}}"#,
            big
        );

        let result = fetch_manifest("dummy", &envelope, tmp.path());
        std::env::set_var("PATH", old_path);

        assert!(result.is_ok(), "expected success: {:#}", result.unwrap_err());
        let m = result.unwrap();
        assert_eq!(m.name, "dummy");
    }

    #[test]
    #[cfg(unix)]
    fn fetch_manifest_reports_child_status_not_write_error_on_early_exit() {
        let tmp = TempDir::new().unwrap();
        // Script: ignore stdin, write real error to stderr, exit non-zero.
        let script = r#"#!/bin/sh
echo 'PLUGIN FATAL: invalid config' >&2
exit 42
"#;
        let _ = make_fake_plugin_script(tmp.path(), "curie-errplug", script).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", tmp.path().display(), old_path);
        std::env::set_var("PATH", &new_path);

        let large = "y".repeat(100 * 1024);
        let envelope = format!(r#"{{"curie_version":"0","config":{{"data":"{}"}}}}"#, large);

        let result = fetch_manifest("errplug", &envelope, tmp.path());
        std::env::set_var("PATH", old_path);

        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("42") || msg.contains("exited with status"),
            "should report child exit status, got: {}",
            msg
        );
        // Must not be the old generic "failed to write stdin" as the primary message.
        assert!(
            !msg.contains("failed to write stdin to curie-errplug"),
            "should not mask with write error: {}",
            msg
        );
    }

    #[test]
    #[cfg(unix)]
    fn generate_sources_reports_child_status_not_write_error() {
        let tmp = TempDir::new().unwrap();
        let script = r#"#!/bin/sh
echo 'GEN ERROR: something went wrong' >&2
exit 77
"#;
        let _ = make_fake_plugin_script(tmp.path(), "curie-generr", script).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", tmp.path().display(), old_path);
        std::env::set_var("PATH", &new_path);

        let large = "z".repeat(80 * 1024);
        let envelope = format!(r#"{{"curie_version":"0","config":{{"x":"{}"}}}}"#, large);
        let resolved = BTreeMap::new();

        let result = generate_sources("generr", &envelope, &resolved, tmp.path(), false);
        std::env::set_var("PATH", old_path);

        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("77") || msg.contains("exited with status"),
            "expected child status, got: {}",
            msg
        );
    }
}
