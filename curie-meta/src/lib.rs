pub mod descriptor;
pub mod foreign;
pub mod workspace;

pub use descriptor::{
    AnnotationProcessor, AnnotationProcessorDetailed, Bom, BuildInfo, DependencyDetailed,
    DependencyValue, Descriptor, DescriptorKind, Developer, Docker, ForeignCommand, ForeignDecl,
    ForeignProject, GitMember, Groovy, Java, Kotlin, MemberEntry, MissingMembers, NativeImage,
    PublishConfig, RepositoryEntry, Scm, Spock, Test, WorkspaceDep, WorkspaceSection,
    DEFAULT_GROOVY_VERSION, DEFAULT_JUNIT_PLATFORM_VERSION, DEFAULT_KOTLIN_VERSION,
    DEFAULT_SPOCK_VERSION,
};
pub use foreign::ForeignTool;
pub use workspace::{Member, Workspace, WorkspaceContext};

use std::path::Path;

/// Open a workspace root or a single-project directory and return a loaded
/// [`Workspace`].
///
/// - If `path` contains a `Curie.toml` with `[workspace]`, all members are
///   loaded and returned in topological build order.
/// - Otherwise `path` is treated as a standalone project and wrapped in a
///   single-member [`Workspace`].
pub fn open(path: &Path) -> anyhow::Result<Workspace> {
    let desc = descriptor::load(path)?;
    if matches!(desc.kind, DescriptorKind::Workspace(_)) {
        workspace::load(path)
    } else {
        let member = workspace::Member {
            path: path.to_path_buf(),
            declared: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            descriptor: desc,
            workspace_deps: Vec::new(),
        };
        Ok(Workspace {
            root: path.to_path_buf(),
            members: vec![member],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_app(dir: &Path, name: &str, version: &str) {
        fs::write(
            dir.join("Curie.toml"),
            format!(
                "[application]\nname = \"{name}\"\nversion = \"{version}\"\nmainClass = \"Main\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn open_single_project() {
        let dir = tempfile::tempdir().unwrap();
        write_app(dir.path(), "my-app", "1.2.3");
        let ws = open(dir.path()).unwrap();
        assert_eq!(ws.members.len(), 1);
        assert!(matches!(
            ws.members[0].descriptor.kind,
            DescriptorKind::Application(_)
        ));
        assert_eq!(ws.members[0].descriptor.project_name(), Some("my-app"));
    }

    #[test]
    fn open_workspace_topo_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Curie.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\n",
        )
        .unwrap();
        for name in ["a", "b", "c"] {
            let mdir = root.join(name);
            fs::create_dir_all(&mdir).unwrap();
            fs::write(
                mdir.join("Curie.toml"),
                format!("[library]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
        }
        // Give c a workspace-dep on b, and b a workspace-dep on a.
        fs::write(
            root.join("b").join("Curie.toml"),
            "[library]\nname = \"b\"\nversion = \"0.1.0\"\n\n[workspace-dependencies]\na = { path = \"../a\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("c").join("Curie.toml"),
            "[library]\nname = \"c\"\nversion = \"0.1.0\"\n\n[workspace-dependencies]\nb = { path = \"../b\" }\n",
        )
        .unwrap();
        let ws = open(root).unwrap();
        assert_eq!(ws.members.len(), 3);
        let names: Vec<_> = ws.members.iter().map(|m| m.declared.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn open_standalone_project_has_correct_deps() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Curie.toml"),
            concat!(
                "[application]\n",
                "name = \"demo\"\nversion = \"0.1.0\"\nmainClass = \"Demo\"\n\n",
                "[dependencies]\n",
                "\"com.example:foo\" = \"1.0\"\n",
                "\"com.example:bar\" = { version = \"2.0\" }\n",
            ),
        )
        .unwrap();
        let ws = open(dir.path()).unwrap();
        assert_eq!(ws.members.len(), 1);
        let deps = &ws.members[0].descriptor.dependencies;
        assert!(deps.contains_key("com.example:foo"));
        assert!(deps.contains_key("com.example:bar"));
        assert_eq!(deps["com.example:foo"].version(), "1.0");
        assert_eq!(deps["com.example:bar"].version(), "2.0");
    }

    #[test]
    fn open_returns_error_for_missing_curie_toml() {
        let dir = tempfile::tempdir().unwrap();
        let result = open(dir.path());
        assert!(result.is_err());
    }
}
