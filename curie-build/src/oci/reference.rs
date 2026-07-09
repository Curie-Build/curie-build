//! OCI / Docker image reference parsing.
//!
//! Accepts the familiar forms:
//!   * `ubuntu` → `docker.io/library/ubuntu:latest`
//!   * `eclipse-temurin:21-jre-alpine`
//!   * `ghcr.io/acme/app:1.0`
//!   * `registry.example.com:5000/acme/app@sha256:…`

use anyhow::{bail, Result};

/// Parsed image reference ready for registry HTTP calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// Registry host, e.g. `registry-1.docker.io` or `ghcr.io`.
    pub registry: String,
    /// Repository path *without* registry, e.g. `library/ubuntu` or `acme/app`.
    pub repository: String,
    /// Tag, defaulting to `latest` when neither tag nor digest was given.
    pub tag: Option<String>,
    /// Content digest (`sha256:…`) when the ref was pinned with `@`.
    pub digest: Option<String>,
}

impl ImageReference {
    /// Parse a user-facing image string into registry / repository / tag / digest.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("image reference is empty");
        }

        let (name_part, digest) = if let Some((left, dig)) = input.split_once('@') {
            if dig.is_empty() {
                bail!("image reference has empty digest: {input}");
            }
            if !dig.starts_with("sha256:") {
                bail!("image digest must start with sha256:, got: {dig}");
            }
            (left, Some(dig.to_string()))
        } else {
            (input, None)
        };

        if name_part.is_empty() {
            bail!("image reference has empty name: {input}");
        }

        // Tag is the last `:` that is not part of a host:port. Heuristic:
        // a colon after the final `/` is a tag; a colon before any `/` with
        // only digits after is host:port.
        let (name_no_tag, tag) = split_tag(name_part)?;

        let (registry, repository) = split_registry_repo(&name_no_tag)?;

        let tag = if digest.is_some() {
            tag // may be None when digest-pinned
        } else {
            Some(tag.unwrap_or_else(|| "latest".to_string()))
        };

        Ok(ImageReference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Reference used for display / tagging, e.g. `library/ubuntu:latest`.
    pub fn display_ref(&self) -> String {
        let base = if self.registry == "registry-1.docker.io" {
            self.repository.clone()
        } else {
            format!("{}/{}", self.registry, self.repository)
        };
        if let Some(d) = &self.digest {
            format!("{base}@{d}")
        } else if let Some(t) = &self.tag {
            format!("{base}:{t}")
        } else {
            base
        }
    }

    /// Registry base URL for Distribution API calls.
    ///
    /// Uses `http` for loopback hosts (unit tests / local registries) and
    /// `https` everywhere else.
    pub fn registry_url(&self) -> String {
        let host = self.registry.split(':').next().unwrap_or(&self.registry);
        let scheme = if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}", self.registry)
    }

    /// Manifest path segment: digest if present, else tag.
    #[allow(dead_code)]
    pub fn manifest_reference(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .expect("tag or digest always set after parse")
    }
}

fn split_tag(name: &str) -> Result<(String, Option<String>)> {
    // Find the last colon. If it appears after the last slash, it's a tag.
    // If there's no slash and the part after colon is all digits, it's host:port
    // (no tag). Otherwise if no slash, colon is a tag (`ubuntu:22.04`).
    let Some(colon) = name.rfind(':') else {
        return Ok((name.to_string(), None));
    };
    let after = &name[colon + 1..];
    let before = &name[..colon];
    let last_slash = name.rfind('/');

    if let Some(slash) = last_slash {
        if colon > slash {
            // repo:tag
            if after.is_empty() {
                bail!("image reference has empty tag: {name}");
            }
            return Ok((before.to_string(), Some(after.to_string())));
        }
        // colon is in the host:port portion
        return Ok((name.to_string(), None));
    }

    // No slash. `host:5000` (port) vs `name:tag`.
    if after.chars().all(|c| c.is_ascii_digit()) {
        Ok((name.to_string(), None))
    } else {
        if after.is_empty() {
            bail!("image reference has empty tag: {name}");
        }
        Ok((before.to_string(), Some(after.to_string())))
    }
}

fn split_registry_repo(name: &str) -> Result<(String, String)> {
    // A registry is present when the first path component contains a `.` or
    // `:` or is exactly `localhost`.
    let first = name.split('/').next().unwrap_or(name);
    let has_registry = first.contains('.') || first.contains(':') || first == "localhost";

    if has_registry {
        let Some((reg, repo)) = name.split_once('/') else {
            bail!(
                "image reference \"{name}\" looks like a registry host but has no repository path"
            );
        };
        if repo.is_empty() {
            bail!("image reference has empty repository: {name}");
        }
        let registry = normalize_registry(reg);
        Ok((registry, repo.to_string()))
    } else {
        // Docker Hub. Bare names go under library/.
        let repository = if name.contains('/') {
            name.to_string()
        } else {
            format!("library/{name}")
        };
        Ok(("registry-1.docker.io".to_string(), repository))
    }
}

fn normalize_registry(reg: &str) -> String {
    match reg {
        "docker.io" | "index.docker.io" | "registry.docker.io" => {
            "registry-1.docker.io".to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_becomes_docker_hub_library() {
        let r = ImageReference::parse("ubuntu").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.tag.as_deref(), Some("latest"));
        assert!(r.digest.is_none());
    }

    #[test]
    fn name_with_tag() {
        let r = ImageReference::parse("eclipse-temurin:21-jre-alpine").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/eclipse-temurin");
        assert_eq!(r.tag.as_deref(), Some("21-jre-alpine"));
    }

    #[test]
    fn docker_hub_user_image() {
        let r = ImageReference::parse("library/hello-world:latest").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/hello-world");
        assert_eq!(r.tag.as_deref(), Some("latest"));
    }

    #[test]
    fn ghcr_image() {
        let r = ImageReference::parse("ghcr.io/acme/app:1.0").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "acme/app");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
    }

    #[test]
    fn host_port_registry() {
        let r = ImageReference::parse("registry.example.com:5000/acme/app:dev").unwrap();
        assert_eq!(r.registry, "registry.example.com:5000");
        assert_eq!(r.repository, "acme/app");
        assert_eq!(r.tag.as_deref(), Some("dev"));
    }

    #[test]
    fn digest_pin() {
        let r = ImageReference::parse(
            "eclipse-temurin@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert_eq!(r.repository, "library/eclipse-temurin");
        assert!(r.tag.is_none());
        assert!(r.digest.as_deref().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn docker_io_normalized() {
        let r = ImageReference::parse("docker.io/library/ubuntu:22.04").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
    }

    #[test]
    fn empty_rejected() {
        assert!(ImageReference::parse("").is_err());
        assert!(ImageReference::parse("   ").is_err());
    }

    #[test]
    fn bad_digest_rejected() {
        assert!(ImageReference::parse("ubuntu@md5:abc").is_err());
    }
}
