//! Deterministic OCI layer (gzip-compressed tar) construction.
//!
//! Same reproducibility contract as `jar::write_deterministic_jar`: fixed
//! epoch mtimes, sorted entries, uid/gid 0, pinned gzip header.

use anyhow::{Context, Result};
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use tar::{EntryType, Header};

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC (seconds since Unix epoch).
pub const REPRODUCIBLE_EPOCH: u64 = 1_704_067_200;

/// One file entry to place in a layer. Paths use forward slashes and should
/// not start with `/` (tar paths are relative; we store them as absolute
/// container paths by prefixing during write when needed).
#[derive(Debug, Clone)]
pub struct LayerFile {
    /// Path inside the container, e.g. `app/libs/guava.jar` (no leading slash).
    pub path: String,
    /// File bytes.
    pub data: Vec<u8>,
    /// Unix mode bits, e.g. `0o644` or `0o755`.
    pub mode: u32,
}

/// Result of building a compressed layer.
#[derive(Debug, Clone)]
pub struct BuiltLayer {
    /// Gzip-compressed tar bytes.
    pub compressed: Vec<u8>,
    /// `sha256:<hex>` of the compressed bytes (descriptor digest).
    pub digest: String,
    /// `sha256:<hex>` of the *uncompressed* tar (config rootfs.diff_id).
    pub diff_id: String,
    /// Compressed size in bytes.
    pub size: u64,
}

/// Build a deterministic gzip layer from an unordered set of files.
///
/// Entries are sorted by path; directories are synthesized for every parent
/// path so extractors that need dir entries succeed.
pub fn build_layer(files: &[LayerFile]) -> Result<BuiltLayer> {
    // BTreeMap gives us sorted unique paths.
    let mut map: BTreeMap<String, &LayerFile> = BTreeMap::new();
    for f in files {
        let path = f.path.trim_start_matches('/').to_string();
        if path.is_empty() || path.contains("..") {
            anyhow::bail!("invalid layer path: {}", f.path);
        }
        map.insert(path, f);
    }

    // Collect parent directories.
    let mut dirs: BTreeMap<String, ()> = BTreeMap::new();
    for path in map.keys() {
        let mut rest = path.as_str();
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if parent.is_empty() {
                break;
            }
            dirs.insert(parent.to_string(), ());
            rest = parent;
        }
    }

    let mut uncompressed: Vec<u8> = Vec::new();
    {
        let mut archive = tar::Builder::new(&mut uncompressed);
        for dir in dirs.keys() {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Directory);
            header.set_path(format!("{dir}/")).context("set dir path")?;
            header.set_mode(0o755);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(REPRODUCIBLE_EPOCH);
            header.set_size(0);
            header.set_cksum();
            archive.append(&header, std::io::empty())?;
        }
        for (path, file) in &map {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_path(path).context("set file path")?;
            header.set_mode(file.mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(REPRODUCIBLE_EPOCH);
            header.set_size(file.data.len() as u64);
            header.set_cksum();
            archive.append(&header, file.data.as_slice())?;
        }
        archive.finish()?;
    }

    let diff_id = sha256_digest(&uncompressed);

    // Gzip with fixed mtime=0 and default compression level (6).
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::new(6));
    encoder.write_all(&uncompressed)?;
    let compressed = encoder.finish()?;

    let digest = sha256_digest(&compressed);
    let size = compressed.len() as u64;

    Ok(BuiltLayer {
        compressed,
        digest,
        diff_id,
        size,
    })
}

/// Build a layer from host files mapped into container paths.
pub fn build_layer_from_paths(entries: &[(String, &Path, u32)]) -> Result<BuiltLayer> {
    let mut files = Vec::with_capacity(entries.len());
    for (container_path, host_path, mode) in entries {
        let data = std::fs::read(host_path)
            .with_context(|| format!("failed to read {}", host_path.display()))?;
        files.push(LayerFile {
            path: container_path.clone(),
            data,
            mode: *mode,
        });
    }
    build_layer(&files)
}

/// Recursively pack a host directory into layer files under `container_prefix`.
pub fn collect_dir_files(host_dir: &Path, container_prefix: &str) -> Result<Vec<LayerFile>> {
    let mut files = Vec::new();
    collect_dir_files_rec(host_dir, host_dir, container_prefix, &mut files)?;
    Ok(files)
}

fn collect_dir_files_rec(
    root: &Path,
    dir: &Path,
    container_prefix: &str,
    out: &mut Vec<LayerFile>,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let container_path = if container_prefix.is_empty() {
            rel
        } else {
            format!("{container_prefix}/{rel}")
        };
        if path.is_dir() {
            collect_dir_files_rec(root, &path, container_prefix, out)?;
        } else {
            let meta = entry.metadata()?;
            let mode = if is_executable(&meta) { 0o755 } else { 0o644 };
            let data = std::fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            out.push(LayerFile {
                path: container_path,
                data,
                mode,
            });
        }
    }
    Ok(())
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    format!("sha256:{}", hex::encode(hash))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn hello_files() -> Vec<LayerFile> {
        vec![LayerFile {
            path: "app/hello.txt".into(),
            data: b"hello curie\n".to_vec(),
            mode: 0o644,
        }]
    }

    #[test]
    fn layer_is_deterministic() {
        let files = vec![
            LayerFile {
                path: "app/b.txt".into(),
                data: b"bbb".to_vec(),
                mode: 0o644,
            },
            LayerFile {
                path: "app/a.txt".into(),
                data: b"aaa".to_vec(),
                mode: 0o644,
            },
        ];
        let mut rev = files.clone();
        rev.reverse();
        let a = build_layer(&files).unwrap();
        let b = build_layer(&rev).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.diff_id, b.diff_id);
        assert_eq!(a.compressed, b.compressed);
    }

    #[test]
    fn layer_golden_digest() {
        let layer = build_layer(&hello_files()).unwrap();
        // Locked goldens for the single-file "hello curie\n" fixture.
        // Update only when the tar/gzip format intentionally changes.
        assert_eq!(layer.diff_id, HELLO_DIFF_ID, "diff_id drifted: {}", layer.diff_id);
        assert_eq!(layer.digest, HELLO_DIGEST, "digest drifted: {}", layer.digest);
        assert_eq!(layer.size, layer.compressed.len() as u64);
    }

    // Populated after first successful compute (see build script / test output).
    const HELLO_DIFF_ID: &str =
        "sha256:2688d86f9caa1c44c234b2e4e3db40bfdaabf03138b8a8503e8712929e2d50aa";
    const HELLO_DIGEST: &str =
        "sha256:c12e4cea6c73c9bf05e99a27b7d0828c1f8acd5512ce4cf96c76c291f9012902";

    #[test]
    fn rejects_parent_path() {
        let files = [LayerFile {
            path: "app/../etc/passwd".into(),
            data: b"x".to_vec(),
            mode: 0o644,
        }];
        assert!(build_layer(&files).is_err());
    }

    #[test]
    fn libs_entry_names_parity_paths() {
        let dep_a = Path::new("/repo/com/example/foo/1.0/foo-1.0.jar");
        let dep_b = Path::new("/repo/org/other/foo/1.0/foo-1.0.jar");
        let names = crate::jar::libs_entry_names(&[dep_a.to_path_buf(), dep_b.to_path_buf()]);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "colliding bare names must be disambiguated");
        for n in &names {
            assert!(!format!("app/libs/{n}").contains(".."));
        }
    }
}
