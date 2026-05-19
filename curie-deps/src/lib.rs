//! curie-deps — Maven dependency resolution for the Curie build tool.
//!
//! # Modules
//! - [`gav`]      — Parse and represent `group:artifact:version` coordinates.
//! - [`pom`]      — Minimal POM XML parser (compile-scoped dependencies, parent chain).
//! - [`repo`]     — Repository configuration (Maven Central default + user additions).
//! - [`resolver`] — Orchestrates cache lookup, download, and transitive resolution.

pub mod gav;
pub mod pom;
pub mod repo;
pub mod resolver;

pub use gav::Gav;
pub use repo::Repository;
pub use resolver::{resolve, DepEntry, ResolveOptions};
