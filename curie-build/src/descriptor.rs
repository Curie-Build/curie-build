pub use curie_meta::descriptor::*;

// Test helpers retained here because they construct curie-build-internal test
// fixtures and are used by pom_writer and publish tests.
#[cfg(test)]
pub(crate) fn fake_bom_desc(
    group_id: Option<&str>,
    name: &str,
    version: &str,
    dependencies: std::collections::BTreeMap<String, DependencyValue>,
    bom_imports: std::collections::BTreeMap<String, String>,
    publish: PublishConfig,
) -> Descriptor {
    Descriptor {
        kind: DescriptorKind::Bom(Bom {
            name: name.to_string(),
            version: version.to_string(),
            group_id: group_id.map(String::from),
        }),
        java: Java::default(),
        test: Test::default(),
        kotlin: Kotlin::default(),
        groovy: Groovy::default(),
        spock: Spock::default(),
        native_image: NativeImage::default(),
        docker: Docker::default(),
        build_info: BuildInfo::default(),
        dependencies,
        test_dependencies: std::collections::BTreeMap::new(),
        repositories: vec![],
        bom_imports,
        test_bom_imports: std::collections::BTreeMap::new(),
        inherited_bom_imports: std::collections::BTreeMap::new(),
        inherited_test_bom_imports: std::collections::BTreeMap::new(),
        workspace_dependencies: std::collections::BTreeMap::new(),
        annotation_processors: std::collections::BTreeMap::new(),
        test_annotation_processors: std::collections::BTreeMap::new(),
        inherited_annotation_processors: std::collections::BTreeMap::new(),
        inherited_test_annotation_processors: std::collections::BTreeMap::new(),
        annotation_processor_options: std::collections::BTreeMap::new(),
        test_annotation_processor_options: std::collections::BTreeMap::new(),
        inherited_annotation_processor_options: std::collections::BTreeMap::new(),
        inherited_test_annotation_processor_options: std::collections::BTreeMap::new(),
        publish,
        plugins: std::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
pub(crate) fn fake_library_desc(
    group_id: Option<&str>,
    name: &str,
    version: &str,
    publish: PublishConfig,
) -> Descriptor {
    Descriptor {
        kind: DescriptorKind::Library(Library {
            name: name.to_string(),
            version: version.to_string(),
            group_id: group_id.map(String::from),
        }),
        java: Java::default(),
        test: Test::default(),
        kotlin: Kotlin::default(),
        groovy: Groovy::default(),
        spock: Spock::default(),
        native_image: NativeImage::default(),
        docker: Docker::default(),
        build_info: BuildInfo::default(),
        dependencies: std::collections::BTreeMap::new(),
        test_dependencies: std::collections::BTreeMap::new(),
        repositories: vec![],
        bom_imports: std::collections::BTreeMap::new(),
        test_bom_imports: std::collections::BTreeMap::new(),
        inherited_bom_imports: std::collections::BTreeMap::new(),
        inherited_test_bom_imports: std::collections::BTreeMap::new(),
        workspace_dependencies: std::collections::BTreeMap::new(),
        annotation_processors: std::collections::BTreeMap::new(),
        test_annotation_processors: std::collections::BTreeMap::new(),
        inherited_annotation_processors: std::collections::BTreeMap::new(),
        inherited_test_annotation_processors: std::collections::BTreeMap::new(),
        annotation_processor_options: std::collections::BTreeMap::new(),
        test_annotation_processor_options: std::collections::BTreeMap::new(),
        inherited_annotation_processor_options: std::collections::BTreeMap::new(),
        inherited_test_annotation_processor_options: std::collections::BTreeMap::new(),
        publish,
        plugins: std::collections::BTreeMap::new(),
    }
}
