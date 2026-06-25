//! curie-deps — Maven dependency resolution for the Curie build tool.
//!
//! # Modules
//! - [`gav`]      — Parse and represent `group:artifact:version` coordinates.
//! - [`pom`]      — Minimal POM XML parser (compile-scoped dependencies, parent chain).
//! - [`repo`]     — Repository configuration (Maven Central default + user additions).
//! - [`resolver`] — Orchestrates cache lookup, download, and transitive resolution.
//! - [`version`]  — Maven version comparison and version-range handling.

pub mod gav;
pub mod pom;
pub mod repo;
pub mod resolver;
pub mod version;

pub use gav::Gav;
pub use repo::Repository;
pub use resolver::{
    fetch_artifact, fetch_artifact_file, fetch_available_versions, fetch_pom_only, resolve,
    resolve_boms, resolve_declared_gavs, resolve_tree, resolve_with_pins, DepEntry, DepTree,
    RangeViolation, ResolvedDep, ResolveOptions, SkippedDep, VersionRangeError,
};
