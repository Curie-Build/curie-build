use crate::config::OsgiConfig;
use curie_plugin::{Manifest, PluginContext, PHASE_POST_PACKAGE, PHASE_PUBLISH};
use std::path::PathBuf;

pub fn build(cfg: &OsgiConfig, ctx: Option<&PluginContext>) -> Manifest {
    let mut files = Vec::new();
    if let Some(ctx) = ctx {
        if let Some(jar) = &ctx.jar {
            files.push(relative_to_project(jar, &ctx.project_root));
        }
    }
    let _ = cfg; // config is reserved for future input filters

    Manifest {
        name: "osgi".to_string(),
        description: "Package the project JAR as an OSGi bundle and publish it".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        types: vec!["lifecycle".to_string()],
        phases: vec![PHASE_POST_PACKAGE.to_string(), PHASE_PUBLISH.to_string()],
        inputs: curie_plugin::Inputs {
            dirs: vec![],
            file_regex: None,
            files,
        },
        outputs: curie_plugin::Outputs {
            source_dirs: vec![],
            files: vec![],
        },
        artifacts: vec![],
    }
}

fn relative_to_project(path: &std::path::Path, project_root: &std::path::Path) -> PathBuf {
    path.strip_prefix(project_root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_lifecycle_phases() {
        let m = build(&OsgiConfig::default(), None);
        assert!(m.types.iter().any(|t| t == "lifecycle"));
        assert!(m.phases.iter().any(|p| p == PHASE_POST_PACKAGE));
        assert!(m.phases.iter().any(|p| p == PHASE_PUBLISH));
        assert!(!curie_plugin::is_source_generator(&m));
        assert!(curie_plugin::participates_in(&m, PHASE_POST_PACKAGE));
    }

    #[test]
    fn manifest_watches_jar_from_context() {
        let ctx = PluginContext {
            project_root: PathBuf::from("/proj"),
            jar: Some(PathBuf::from("/proj/target/greeter-1.0.0.jar")),
            ..Default::default()
        };
        let m = build(&OsgiConfig::default(), Some(&ctx));
        assert_eq!(
            m.inputs.files,
            vec![PathBuf::from("target/greeter-1.0.0.jar")]
        );
    }
}
