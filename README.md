# Curie

The Java build tooling landscape has been largely static for two decades. Maven arrived in 2004 and brought convention over configuration and a centralized repository — genuinely transformative at the time. Gradle followed in 2008 and replaced XML with a programmable DSL. Both tools have since accumulated layers of abstraction, plugins, and workarounds to accommodate workflows that simply did not exist when they were designed: containerised deployments, reproducible supply chains, polyglot monorepos. The result is that a new Java project in 2026 routinely ships with hundreds of lines of build configuration and additional scripts.

Other language ecosystems have quietly raised the bar. Cargo, Rust's built-in build tool, ships workspaces, a lockfile, and reproducible dependency resolution as first-class features with no plugins required — a new project is correct and reproducible by default. Go takes the same philosophy further: `go.mod` and `go.sum` are part of the toolchain itself, so deterministic builds and zero-configuration module management are simply the starting point, not optional add-ons. Java developers working across languages notice the gap.

This is how progress in general tends to work: an ecosystem experiments, different approaches compete, and over time the field converges on what actually works. Maven followed this approach by proposing excellent conventions. What Curie shows is that
the conventions have moved — there is a new baseline.

Curie is a fast, minimal build tool for Java projects written in Rust. It handles dependency resolution from Maven Central, incremental compilation, [reproducible builds](https://reproducible-builds.org), test execution, and optional Docker image building — driven by a single `Curie.toml` configuration file.

---

## Documentation

Full documentation is at **[https://curie-build.org/docs/](https://curie-build.org/docs/)**.

Topics covered include dependency resolution, Kotlin and Groovy support, incremental builds, workspaces, the formatter, testing, Docker, publishing, dependency tree, SBOM audit, GraalVM native-image, scaffolding, BOM projects, and the interactive log browser (`curie inspect`).

---

## Getting started

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable)
- A JDK (for `javac` and `java`) — Java 21 recommended
- Docker (optional, only needed for `docker`-enabled builds)

### Build Curie

```bash
git clone <repo-url>
cd curie
cargo build --release
# binary is at: target/release/curie
```

Add `target/release` to your `PATH`, or copy the binary somewhere on your `PATH`.

### Quick start

```bash
curie new app hello --package com.example
cd hello
curie build    # resolve deps, compile, test, package JAR
curie run      # run the JAR
```

---

## Status

This is a research project. The build tool layer is functional and the examples work end-to-end. Contributions and feedback are welcome.
