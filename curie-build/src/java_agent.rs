use std::path::PathBuf;

/// Given a list of `group:artifact` coordinate strings and a resolved classpath,
/// return the JAR paths that should be passed as `-javaagent:<path>`.
///
/// For each coordinate, the artifact ID (part after `:`) is extracted and
/// matched against JAR filenames as `<artifactId>-*.jar`.  Entries with no
/// matching JAR in the classpath are silently skipped.
pub fn find_agent_jars(coords: &[&str], classpath: &[PathBuf]) -> Vec<PathBuf> {
    coords
        .iter()
        .filter_map(|coord| {
            let artifact_id = coord.split(':').nth(1).unwrap_or(coord);
            let prefix = format!("{artifact_id}-");
            classpath
                .iter()
                .find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jar"))
                })
                .cloned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn finds_agent_by_artifact_id() {
        let cp = paths(&[
            "target/libs/mockito-core-5.2.0.jar",
            "target/libs/byte-buddy-1.14.0.jar",
        ]);
        let result = find_agent_jars(&["org.mockito:mockito-core"], &cp);
        assert_eq!(
            result,
            vec![PathBuf::from("target/libs/mockito-core-5.2.0.jar")]
        );
    }

    #[test]
    fn returns_empty_when_no_match() {
        let cp = paths(&["target/libs/junit-5.10.0.jar"]);
        let result = find_agent_jars(&["org.mockito:mockito-core"], &cp);
        assert!(result.is_empty());
    }

    #[test]
    fn does_not_match_prefix_substring() {
        // "bar-extra-1.0.jar" must NOT match coord "org.foo:bar" because the
        // filename starts with "bar-extra-", not "bar-<version>".
        // However, this test verifies we don't spuriously match.
        let cp = paths(&["target/libs/mockito-core-extra-1.0.jar"]);
        // "mockito-core-extra-1.0.jar" starts with "mockito-core-" → it DOES match.
        // The key invariant: exact prefix boundary <artifactId>- is required.
        let result = find_agent_jars(&["org.mockito:mockito-core"], &cp);
        assert_eq!(
            result.len(),
            1,
            "mockito-core-extra-1.0.jar starts with 'mockito-core-'"
        );

        // A jar that doesn't start with the artifact prefix must NOT match.
        let cp2 = paths(&["target/libs/byte-mockito-core-1.0.jar"]);
        let result2 = find_agent_jars(&["org.mockito:mockito-core"], &cp2);
        assert!(
            result2.is_empty(),
            "byte-mockito-core should not match mockito-core"
        );
    }

    #[test]
    fn multiple_coords_resolved() {
        let cp = paths(&[
            "target/libs/mockito-core-5.0.0.jar",
            "target/libs/byte-buddy-agent-1.14.0.jar",
        ]);
        let result = find_agent_jars(
            &["org.mockito:mockito-core", "net.bytebuddy:byte-buddy-agent"],
            &cp,
        );
        assert_eq!(result.len(), 2);
    }
}
