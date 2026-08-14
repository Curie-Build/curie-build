use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

pub type Envelope = curie_plugin::Envelope<OsgiConfig>;

pub fn read_envelope() -> Result<Envelope> {
    curie_plugin::read_envelope()
}

/// `[plugin.osgi]` configuration.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct OsgiConfig {
    #[serde(rename = "symbolicName")]
    pub symbolic_name: Option<String>,
    #[serde(rename = "bundleVersion")]
    pub bundle_version: Option<String>,
    #[serde(rename = "bundleName")]
    pub bundle_name: Option<String>,
    pub activator: Option<String>,
    #[serde(rename = "exportPackage")]
    pub export_package: Option<String>,
    #[serde(rename = "importPackage")]
    pub import_package: Option<String>,
    #[serde(rename = "privatePackage")]
    pub private_package: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// OSGi repository URL (`file:` or `http(s):`) or a `[[repositories]]` id.
    pub repository: Option<String>,
}

/// Convert a Maven version to OSGi `major.minor.micro.qualifier` form.
/// `1.0.0-SNAPSHOT` becomes `1.0.0.SNAPSHOT`.
pub fn to_osgi_version(maven: &str) -> String {
    match maven.split_once('-') {
        Some((base, qual)) => format!("{}.{}", base, qual.replace('-', ".")),
        None => maven.to_string(),
    }
}

/// Bundle-SymbolicName: explicit config, else `groupId.artifactId`, else artifactId.
pub fn symbolic_name(cfg: &OsgiConfig, ctx: &curie_plugin::PluginContext) -> String {
    if let Some(s) = &cfg.symbolic_name {
        return s.clone();
    }
    match &ctx.group_id {
        Some(g) if !g.is_empty() => format!("{}.{}", g, ctx.artifact_id),
        _ => ctx.artifact_id.clone(),
    }
}

pub fn bundle_version(cfg: &OsgiConfig, ctx: &curie_plugin::PluginContext) -> String {
    cfg.bundle_version
        .clone()
        .unwrap_or_else(|| to_osgi_version(&ctx.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_empty() {
        let env: Envelope =
            serde_json::from_str(r#"{"curie_version":"0.7.0","config":{}}"#).unwrap();
        assert!(env.config.symbolic_name.is_none());
        assert!(env.config.headers.is_empty());
        assert!(env.config.repository.is_none());
    }

    #[test]
    fn config_deserializes_full() {
        let env: Envelope = serde_json::from_str(
            r#"{
                "curie_version": "0.7.0",
                "config": {
                    "symbolicName": "com.example.greeter",
                    "bundleVersion": "1.0.0",
                    "bundleName": "Greeter",
                    "activator": "com.example.Activator",
                    "exportPackage": "com.example.api",
                    "importPackage": "org.osgi.framework",
                    "privatePackage": "com.example.internal",
                    "headers": {"Bundle-Vendor": "Example"},
                    "repository": "file:target/osgi-repo"
                }
            }"#,
        )
        .unwrap();
        let c = &env.config;
        assert_eq!(c.symbolic_name.as_deref(), Some("com.example.greeter"));
        assert_eq!(c.bundle_name.as_deref(), Some("Greeter"));
        assert_eq!(
            c.headers.get("Bundle-Vendor").map(String::as_str),
            Some("Example")
        );
        assert_eq!(c.repository.as_deref(), Some("file:target/osgi-repo"));
    }

    #[test]
    fn snapshot_version_becomes_osgi_qualifier() {
        assert_eq!(to_osgi_version("1.0.0-SNAPSHOT"), "1.0.0.SNAPSHOT");
        assert_eq!(to_osgi_version("1.2.3-RC1"), "1.2.3.RC1");
        assert_eq!(to_osgi_version("1.0.0"), "1.0.0");
    }

    #[test]
    fn symbolic_name_prefers_config() {
        let cfg = OsgiConfig {
            symbolic_name: Some("explicit.bsn".into()),
            ..Default::default()
        };
        let ctx = curie_plugin::PluginContext {
            group_id: Some("com.example".into()),
            artifact_id: "greeter".into(),
            ..Default::default()
        };
        assert_eq!(symbolic_name(&cfg, &ctx), "explicit.bsn");
    }

    #[test]
    fn symbolic_name_joins_group_and_artifact() {
        let cfg = OsgiConfig::default();
        let ctx = curie_plugin::PluginContext {
            group_id: Some("com.example".into()),
            artifact_id: "greeter".into(),
            ..Default::default()
        };
        assert_eq!(symbolic_name(&cfg, &ctx), "com.example.greeter");
    }
}
