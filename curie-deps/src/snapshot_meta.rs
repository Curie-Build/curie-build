//! Version-level `maven-metadata.xml` parsing for unique SNAPSHOT artifacts.
//!
//! Artifact-level metadata (`…/artifact/maven-metadata.xml`, a flat list of
//! published versions) is handled elsewhere (`fetch_available_versions` /
//! `curie update`). This module only parses the *version-level* document at
//! `…/artifact/1.0-SNAPSHOT/maven-metadata.xml` that names the current
//! timestamped filename for a snapshot.
//!
//! Freshness is not policy-gated: without a `Curie.lock` pin (or with `-U`),
//! metadata is always re-fetched from the repository. The lock *is* the pin.

use anyhow::{bail, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// One `<snapshotVersion>` entry from version-level metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotVersionEntry {
    pub extension: String,
    pub classifier: Option<String>,
    /// Timestamped filename version, e.g. `1.0-20260610.123456-3`.
    pub value: String,
}

/// Parsed version-level `maven-metadata.xml` for a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotMetadata {
    pub timestamp: Option<String>,
    pub build_number: Option<String>,
    pub snapshot_versions: Vec<SnapshotVersionEntry>,
}

impl SnapshotMetadata {
    /// Select the unique filename version for `(extension, classifier)`.
    ///
    /// Prefers an exact `<snapshotVersions>` match; falls back to
    /// `{baseWithoutSnapshot}-{timestamp}-{buildNumber}` when the
    /// `<snapshot>` block is present but no matching entry exists; returns
    /// `None` when the metadata describes a non-unique (local) snapshot
    /// with neither block usable.
    pub fn resolve_unique_version(
        &self,
        base_version: &str,
        extension: &str,
        classifier: Option<&str>,
    ) -> Option<String> {
        let want_classifier = classifier.filter(|c| !c.is_empty());
        for entry in &self.snapshot_versions {
            if entry.extension != extension {
                continue;
            }
            let entry_c = entry.classifier.as_deref().filter(|c| !c.is_empty());
            if entry_c == want_classifier {
                return Some(entry.value.clone());
            }
        }

        // Fallback: <snapshot><timestamp>+<buildNumber>
        let ts = self.timestamp.as_deref()?;
        let bn = self.build_number.as_deref()?;
        if ts.is_empty() || bn.is_empty() {
            return None;
        }
        let base = base_version
            .strip_suffix("-SNAPSHOT")
            .unwrap_or(base_version);
        Some(format!("{base}-{ts}-{bn}"))
    }
}

/// Parse version-level snapshot `maven-metadata.xml`.
pub fn parse_snapshot_metadata(xml: &str) -> Result<SnapshotMetadata> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut meta = SnapshotMetadata::default();
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();

    // In-progress snapshotVersion fields.
    let mut cur_extension: Option<String> = None;
    let mut cur_classifier: Option<String> = None;
    let mut cur_value: Option<String> = None;
    let mut in_snapshot_version = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "snapshotVersion" {
                    in_snapshot_version = true;
                    cur_extension = None;
                    cur_classifier = None;
                    cur_value = None;
                }
                path.push(name);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "snapshotVersion" {
                    if let Some(value) = cur_value.take() {
                        if let Some(extension) = cur_extension.take() {
                            meta.snapshot_versions.push(SnapshotVersionEntry {
                                extension,
                                classifier: cur_classifier.take(),
                                value,
                            });
                        }
                    }
                    in_snapshot_version = false;
                    cur_extension = None;
                    cur_classifier = None;
                    cur_value = None;
                }
                if path.last().map(|s| s.as_str()) == Some(name.as_str()) {
                    path.pop();
                }
            }
            Ok(Event::Text(t)) => {
                let decoded = match t.decode() {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let text = match quick_xml::escape::unescape(&decoded) {
                    Ok(u) => u.into_owned(),
                    Err(_) => decoded.into_owned(),
                };
                if text.is_empty() {
                    // nothing
                } else if in_snapshot_version {
                    match path.last().map(|s| s.as_str()) {
                        Some("extension") => cur_extension = Some(text),
                        Some("classifier") => cur_classifier = Some(text),
                        Some("value") => cur_value = Some(text),
                        _ => {}
                    }
                } else {
                    // <versioning><snapshot><timestamp|buildNumber>
                    let joined = path.join("/");
                    if joined.ends_with("snapshot/timestamp")
                        || joined.ends_with("versioning/snapshot/timestamp")
                    {
                        meta.timestamp = Some(text);
                    } else if joined.ends_with("snapshot/buildNumber")
                        || joined.ends_with("versioning/snapshot/buildNumber")
                    {
                        meta.build_number = Some(text);
                    } else if path.last().map(|s| s.as_str()) == Some("timestamp")
                        && path.iter().any(|p| p == "snapshot")
                    {
                        meta.timestamp = Some(text);
                    } else if path.last().map(|s| s.as_str()) == Some("buildNumber")
                        && path.iter().any(|p| p == "snapshot")
                    {
                        meta.build_number = Some(text);
                    }
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) => break,
            Err(e) => bail!("failed to parse snapshot maven-metadata.xml: {e}"),
            _ => {}
        }
        buf.clear();
    }

    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>com.example</groupId>
  <artifactId>foo</artifactId>
  <version>1.0-SNAPSHOT</version>
  <versioning>
    <snapshot>
      <timestamp>20260610.123456</timestamp>
      <buildNumber>3</buildNumber>
    </snapshot>
    <lastUpdated>20260610123456</lastUpdated>
    <snapshotVersions>
      <snapshotVersion>
        <extension>jar</extension>
        <value>1.0-20260610.123456-3</value>
        <updated>20260610123456</updated>
      </snapshotVersion>
      <snapshotVersion>
        <extension>pom</extension>
        <value>1.0-20260610.123456-3</value>
        <updated>20260610123456</updated>
      </snapshotVersion>
      <snapshotVersion>
        <classifier>sources</classifier>
        <extension>jar</extension>
        <value>1.0-20260610.123456-3</value>
        <updated>20260610123456</updated>
      </snapshotVersion>
    </snapshotVersions>
  </versioning>
</metadata>
"#;

    #[test]
    fn parse_snapshot_versions_and_select() {
        let meta = parse_snapshot_metadata(SAMPLE).unwrap();
        assert_eq!(meta.timestamp.as_deref(), Some("20260610.123456"));
        assert_eq!(meta.build_number.as_deref(), Some("3"));
        assert_eq!(meta.snapshot_versions.len(), 3);
        assert_eq!(
            meta.resolve_unique_version("1.0-SNAPSHOT", "jar", None)
                .as_deref(),
            Some("1.0-20260610.123456-3")
        );
        assert_eq!(
            meta.resolve_unique_version("1.0-SNAPSHOT", "pom", None)
                .as_deref(),
            Some("1.0-20260610.123456-3")
        );
        assert_eq!(
            meta.resolve_unique_version("1.0-SNAPSHOT", "jar", Some("sources"))
                .as_deref(),
            Some("1.0-20260610.123456-3")
        );
    }

    #[test]
    fn fallback_from_timestamp_build_number() {
        let xml = r#"<?xml version="1.0"?>
<metadata>
  <versioning>
    <snapshot>
      <timestamp>20260101.000001</timestamp>
      <buildNumber>7</buildNumber>
    </snapshot>
  </versioning>
</metadata>"#;
        let meta = parse_snapshot_metadata(xml).unwrap();
        assert_eq!(
            meta.resolve_unique_version("2.5-SNAPSHOT", "jar", None)
                .as_deref(),
            Some("2.5-20260101.000001-7")
        );
    }

    #[test]
    fn missing_snapshot_block_returns_none() {
        // Non-unique / local install: no <snapshot> and no <snapshotVersions>.
        let xml = r#"<?xml version="1.0"?><metadata><versioning></versioning></metadata>"#;
        let meta = parse_snapshot_metadata(xml).unwrap();
        assert!(meta
            .resolve_unique_version("1.0-SNAPSHOT", "jar", None)
            .is_none());
    }
}
