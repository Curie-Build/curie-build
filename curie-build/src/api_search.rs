//! Maven Central REST API search client for `curie add`.
//!
//! Queries `https://search.maven.org/solrsearch/select` and returns a list of
//! matching artifacts without requiring a locally cached index.

use anyhow::{Context, Result};
use serde::Deserialize;

const API_BASE: &str = "https://search.maven.org/solrsearch/select";
const TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One artifact hit returned by the Maven Central search API.
#[derive(Debug, Clone)]
pub struct ArtifactHit {
    /// `"group:artifact"` coordinate.
    pub coord: String,
    pub latest_version: String,
}

// ---------------------------------------------------------------------------
// Deserialization structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ApiResponse {
    response: ResponseBody,
}

#[derive(Deserialize)]
struct ResponseBody {
    docs: Vec<ApiDoc>,
}

#[derive(Deserialize)]
struct ApiDoc {
    #[serde(rename = "g")]
    group: String,
    #[serde(rename = "a")]
    artifact: String,
    #[serde(rename = "latestVersion", default)]
    latest_version: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Query the Maven Central REST API and return up to `rows` results.
pub fn search_api(query: &str, rows: usize) -> Result<Vec<ArtifactHit>> {
    let client = build_client()?;
    let url = format!("{}?q={}&rows={}&wt=json", API_BASE, percent_encode(query), rows);
    let body = client
        .get(&url)
        .send()
        .context("Maven Central API request failed")?
        .text()
        .context("failed to read Maven Central API response")?;

    parse_response(&body)
}

fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("curie/0.1")
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client")
}

/// Percent-encode a query string for inclusion in a URL.
/// Spaces become `+`; other non-unreserved characters are `%XX`-encoded.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' | b':' => out.push(byte as char),
            b' ' => out.push('+'),
            b => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn parse_response(body: &str) -> Result<Vec<ArtifactHit>> {
    let parsed: ApiResponse =
        serde_json::from_str(body).context("failed to parse Maven Central API response")?;
    Ok(parsed
        .response
        .docs
        .into_iter()
        .map(|d| ArtifactHit {
            coord: format!("{}:{}", d.group, d.artifact),
            latest_version: d.latest_version,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_api_response_empty() {
        let json = r#"{"responseHeader":{"status":0,"QTime":5},"response":{"numFound":0,"start":0,"docs":[]}}"#;
        let hits = parse_response(json).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_api_response_single() {
        let json = r#"{
            "responseHeader":{"status":0,"QTime":10},
            "response":{"numFound":1,"start":0,"docs":[
                {"g":"com.google.guava","a":"guava","latestVersion":"33.0.0-jre","versionCount":45}
            ]}
        }"#;
        let hits = parse_response(json).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].coord, "com.google.guava:guava");
        assert_eq!(hits[0].latest_version, "33.0.0-jre");
    }

    #[test]
    fn parse_api_response_missing_optional_fields() {
        let json = r#"{
            "responseHeader":{"status":0,"QTime":3},
            "response":{"numFound":1,"start":0,"docs":[
                {"g":"org.example","a":"lib"}
            ]}
        }"#;
        let hits = parse_response(json).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].coord, "org.example:lib");
        assert_eq!(hits[0].latest_version, "");
    }
}
