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

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// ── Manifest types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PluginManifest {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub inputs: PluginInputs,
    #[serde(default)]
    pub outputs: PluginOutputs,
    #[serde(default)]
    pub artifacts: Vec<PluginArtifact>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PluginInputs {
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    pub file_regex: Option<String>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PluginOutputs {
    #[serde(default)]
    pub source_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginArtifact {
    pub id: String,
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub classifier: Option<String>,
    pub extension: String,
    #[serde(default)]
    pub executable: bool,
}

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
    offline: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut resolved = BTreeMap::new();
    for art in artifacts {
        let path = download_artifact(art, offline)?;
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

fn download_artifact(art: &PluginArtifact, offline: bool) -> Result<PathBuf> {
    let cache_path = artifact_cache_path(art)?;

    if cache_path.exists() {
        return Ok(cache_path);
    }

    if offline {
        bail!(
            "artifact {}:{} not cached at {} (offline mode)",
            art.group,
            art.artifact,
            cache_path.display()
        );
    }

    let url = artifact_url(art);
    let sha1_url = format!("{url}.sha1");

    let bytes = reqwest::blocking::get(&url)
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {url}"))?
        .bytes()
        .with_context(|| format!("failed to read body from {url}"))?;

    // Verify SHA-1.
    let expected_sha1 = reqwest::blocking::get(&sha1_url)
        .with_context(|| format!("GET {sha1_url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {sha1_url}"))?
        .text()
        .with_context(|| format!("failed to read SHA-1 from {sha1_url}"))?;
    let expected_sha1 = expected_sha1.trim().split_whitespace().next().unwrap_or("").to_lowercase();

    use sha1::Digest as _;
    let actual_sha1 = hex::encode(sha1::Sha1::digest(&bytes));
    anyhow::ensure!(
        actual_sha1 == expected_sha1,
        "SHA-1 mismatch for {}: expected {expected_sha1}, got {actual_sha1}",
        art.artifact
    );

    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }
    std::fs::write(&cache_path, &bytes)
        .with_context(|| format!("failed to write {}", cache_path.display()))?;

    if art.executable {
        set_executable(&cache_path)?;
    }

    Ok(cache_path)
}

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

fn artifact_url(art: &PluginArtifact) -> String {
    let group_path = art.group.replace('.', "/");
    let filename = match &art.classifier {
        Some(c) => format!("{}-{}-{}.{}", art.artifact, art.version, c, art.extension),
        None => format!("{}-{}.{}", art.artifact, art.version, art.extension),
    };
    format!(
        "https://repo1.maven.org/maven2/{}/{}/{}/{}",
        group_path, art.artifact, art.version, filename
    )
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
    fn artifact_url_format() {
        let art = PluginArtifact {
            id: "protoc".into(),
            group: "com.google.protobuf".into(),
            artifact: "protoc".into(),
            version: "3.25.0".into(),
            classifier: Some("linux-x86_64".into()),
            extension: "exe".into(),
            executable: true,
        };
        let url = artifact_url(&art);
        assert_eq!(
            url,
            "https://repo1.maven.org/maven2/com/google/protobuf/protoc/3.25.0/protoc-3.25.0-linux-x86_64.exe"
        );
    }
}
