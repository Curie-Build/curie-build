//! Content-addressed local OCI blob cache under `~/.curie/oci/blobs/`.

use super::layer::sha256_digest;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Root of the Curie OCI cache (`~/.curie/oci`).
pub fn oci_cache_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".curie").join("oci"))
}

/// Path of a blob given its `sha256:…` digest.
pub fn blob_path(digest: &str) -> Result<PathBuf> {
    let hex = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported digest algorithm: {digest}"))?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid sha256 digest: {digest}");
    }
    Ok(oci_cache_root()?.join("blobs").join("sha256").join(hex))
}

/// Return cached blob bytes if present and digest-verified.
pub fn get_blob(digest: &str) -> Result<Option<Vec<u8>>> {
    let path = blob_path(digest)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read cached blob {}", path.display()))?;
    let actual = sha256_digest(&bytes);
    if actual != digest {
        // Corrupt cache entry — remove and treat as miss.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// True when the blob is already in the cache (verified).
#[allow(dead_code)]
pub fn has_blob(digest: &str) -> Result<bool> {
    Ok(get_blob(digest)?.is_some())
}

/// Store blob bytes under their digest. Verifies the digest matches.
pub fn put_blob(digest: &str, bytes: &[u8]) -> Result<PathBuf> {
    let actual = sha256_digest(bytes);
    if actual != digest {
        bail!("blob digest mismatch: expected {digest}, got {actual}");
    }
    let path = blob_path(digest)?;
    if path.exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let staging = path.with_extension(format!(
        "part.{}.{}",
        std::process::id(),
        std::process::id()
    ));
    std::fs::write(&staging, bytes)
        .with_context(|| format!("failed to write staging blob {}", staging.display()))?;
    std::fs::rename(&staging, &path).with_context(|| {
        format!(
            "failed to rename {} → {}",
            staging.display(),
            path.display()
        )
    })?;
    Ok(path)
}

/// Copy a blob from the cache into `dest` (hard-link when possible, else copy).
pub fn materialize_blob(digest: &str, dest: &Path) -> Result<()> {
    let src = blob_path(digest)?;
    if !src.exists() {
        bail!("blob {digest} is not in the local OCI cache");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        return Ok(());
    }
    match std::fs::hard_link(&src, dest) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(&src, dest).map(|_| ()).with_context(|| {
            format!(
                "failed to copy blob {} to {}",
                src.display(),
                dest.display()
            )
        }),
    }
}

/// Cache a tag→digest resolution so offline mode can resolve tags.
pub fn put_tag_resolution(registry: &str, repository: &str, tag: &str, digest: &str) -> Result<()> {
    let path = tag_resolution_path(registry, repository, tag)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, digest)
        .with_context(|| format!("failed to write tag cache {}", path.display()))?;
    Ok(())
}

pub fn get_tag_resolution(registry: &str, repository: &str, tag: &str) -> Result<Option<String>> {
    let path = tag_resolution_path(registry, repository, tag)?;
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&path)?;
    let s = s.trim().to_string();
    if s.starts_with("sha256:") {
        Ok(Some(s))
    } else {
        Ok(None)
    }
}

fn tag_resolution_path(registry: &str, repository: &str, tag: &str) -> Result<PathBuf> {
    // Encode path-unsafe chars.
    let reg = registry.replace('/', "_");
    let repo = repository.replace('/', "_");
    let tag = tag.replace('/', "_");
    Ok(oci_cache_root()?
        .join("tags")
        .join(reg)
        .join(repo)
        .join(tag))
}

#[cfg(test)]
mod tests {
    use super::super::layer::sha256_digest;
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let data = b"curie-oci-cache-test-bytes";
        let digest = sha256_digest(data);
        // Use the real cache location but unique content so we don't collide.
        let path = put_blob(&digest, data).unwrap();
        assert!(path.exists());
        let got = get_blob(&digest).unwrap().expect("blob present");
        assert_eq!(got, data);
    }

    #[test]
    fn rejects_digest_mismatch() {
        let err = put_blob(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            b"nope",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("digest mismatch"), "got: {err}");
    }
}
