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
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// ── Manifest types (re-exported from curie-plugin) ────────────────────────────

pub use curie_plugin::Artifact as PluginArtifact;
pub use curie_plugin::Manifest as PluginManifest;

// ── Stamp types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Stamp {
    dir_mtimes: Vec<MtimeEntry>,
    file_mtimes: Vec<MtimeEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MtimeEntry {
    path: PathBuf,
    mtime_ns: u128,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Invoke `curie-<name> manifest`, returning the parsed manifest.
pub fn fetch_manifest(
    name: &str,
    envelope_json: &str,
    project_root: &Path,
) -> Result<PluginManifest> {
    let bin_name = format!("curie-{name}");
    let bin = which::which(&bin_name)
        .with_context(|| format!("{bin_name} not found on PATH (required by [plugin.{name}])"))?;

    let mut child = std::process::Command::new(&bin)
        .args(["manifest", "--project"])
        .arg(project_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn {bin_name}"))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(envelope_json.as_bytes())
        .with_context(|| format!("failed to write stdin to {bin_name}"))?;

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {bin_name}"))?;

    anyhow::ensure!(
        output.status.success(),
        "{bin_name} manifest exited with status {:?}",
        output.status.code()
    );

    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{bin_name} manifest produced invalid JSON on stdout"))
}

/// Return true when all inputs recorded in the stamp are still unchanged.
pub fn is_up_to_date(
    manifest: &PluginManifest,
    stamp_path: &Path,
    project_root: &Path,
) -> bool {
    let stamp = match read_stamp(stamp_path) {
        Ok(s) => s,
        Err(_) => return false,
    };

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

    let mut child = cmd.spawn().with_context(|| format!("failed to spawn {bin_name}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(run_json.as_bytes())
        .with_context(|| format!("failed to write stdin to {bin_name}"))?;

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {bin_name}"))?;

    anyhow::ensure!(
        status.success(),
        "{bin_name} generate-sources exited with status {:?}",
        status.code()
    );
    Ok(())
}

/// Write a fresh stamp file reflecting current input mtimes from the manifest.
pub fn write_stamp(
    manifest: &PluginManifest,
    stamp_path: &Path,
    project_root: &Path,
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

    let stamp = Stamp { dir_mtimes, file_mtimes };

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
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let group_path = art.group.replace('.', "/");
    let filename = match &art.classifier {
        Some(c) => format!("{}-{}-{}.{}", art.artifact, art.version, c, art.extension),
        None => format!("{}-{}.{}", art.artifact, art.version, art.extension),
    };
    Ok(home
        .join(".m2")
        .join("repository")
        .join(&group_path)
        .join(&art.artifact)
        .join(&art.version)
        .join(filename))
}

#[cfg(test)]
fn artifact_relative_path(art: &PluginArtifact) -> String {
    let group_path = art.group.replace('.', "/");
    let filename = match &art.classifier {
        Some(c) => format!("{}-{}-{}.{}", art.artifact, art.version, c, art.extension),
        None => format!("{}-{}.{}", art.artifact, art.version, art.extension),
    };
    format!("{}/{}/{}/{}", group_path, art.artifact, art.version, filename)
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

    fn write_stamp_at(dir: &Path, manifest: &PluginManifest) {
        let stamp_path = dir.join("stamp.json");
        write_stamp(manifest, &stamp_path, dir).unwrap();
    }

    #[test]
    fn stale_when_stamp_absent() {
        let tmp = TempDir::new().unwrap();
        let manifest = sample_manifest();
        fs::create_dir_all(tmp.path().join("proto")).unwrap();
        let stamp = tmp.path().join("nonexistent.stamp");
        assert!(!is_up_to_date(&manifest, &stamp, tmp.path()));
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
        assert!(is_up_to_date(&manifest, &stamp_path, tmp.path()));
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

        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path()));
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

        assert!(!is_up_to_date(&manifest, &stamp_path, tmp.path()));
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
}
