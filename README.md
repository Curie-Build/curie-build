# Curie

> **Research project** — exploring whether a Rust-based build tool for Java is feasible as a drop-in replacement for Maven and Gradle.

The Java build tooling landscape has been largely static for two decades. Maven arrived in 2004 and brought convention over configuration and a centralized repository — genuinely transformative at the time. Gradle followed in 2008 and replaced XML with a programmable DSL. Both tools have since accumulated layers of abstraction, plugins, and workarounds to accommodate workflows that simply did not exist when they were designed: containerised deployments, reproducible supply chains, polyglot monorepos, sub-second feedback loops in CI. The result is that a new Java project in 2026 routinely ships with hundreds of lines of build configuration, a Gradle daemon that needs occasional killing, and additional scripts.

There is space here for a tool that starts from the workflows that have become standard rather than grafting them on as plugins.

Curie is a fast, minimal build tool for Java projects written in Rust. It handles dependency resolution from Maven Central, incremental compilation, [reproducible builds](https://reproducible-builds.org), test execution, and optional Docker image building — driven by a single `curie.toml` configuration file.

This is a work in progress. The current implementation covers the core build pipeline.

---

## Why Curie?

Maven and Gradle are powerful but carry significant complexity and overhead. Curie trades extensibility for simplicity and speed.

| | Curie | Maven | Gradle |
|---|---|---|---|
| **Configuration** | `curie.toml` (20 lines for a typical project) | `pom.xml` (100+ lines of XML boilerplate) | `build.gradle` (Groovy/Kotlin DSL, complex) |
| **Startup time** | Near-instant (native binary) | JVM startup (~1–2 s) | JVM startup + daemon overhead |
| **Incremental builds** | Built-in, mtime-based | Plugin-dependent | Built-in but complex |
| **[Reproducible builds](https://reproducible-builds.org)** | Yes, timestamps clamped to epoch | Requires extra plugin config | Requires extra plugin config |
| **Docker support** | First-class, auto-generates optimised Dockerfile | External plugins | External plugins |
| **Learning curve** | Minimal — one file format | High — lifecycle phases, plugin ecosystem | High — DSL, task graph, plugin API |
| **Binary size** | Single static binary, no runtime needed | Requires JVM | Requires JVM |

Curie will never match the plugin ecosystem of Maven or Gradle. The goal is a focused tool that handles the 80% case — compile, test, package, deploy — with far less ceremony.

---

## Current capabilities

- **Dependency resolution** — resolves Maven dependencies transitively from Maven Central (or custom repositories), caches JARs and POMs to `~/.m2/repository` using the standard Maven layout.
- **Incremental compilation** — skips `javac` when sources and `curie.toml` are unchanged relative to the existing class files.
- **[Reproducible builds](https://reproducible-builds.org)** — all ZIP entry timestamps are clamped to a fixed epoch (2024-01-01 UTC); entries are sorted lexicographically, producing byte-identical JARs for identical inputs.
- **Test execution** — discovers and runs JUnit 5 tests automatically, with incremental skip when nothing has changed.
- **Docker support** — builds and optionally runs a Docker image. Curie auto-generates a cache-optimised `Dockerfile` that layers dependency JARs before the application JAR, so a code-only change does not invalidate the dependency layer.
- **Custom repositories** — additional Maven repositories can be declared alongside Maven Central.
- **Library projects** — projects without a `mainClass` compile to a plain JAR with no Docker involvement.

### Commands

```
curie build             # resolve deps, compile, run tests, package JAR, build Docker image (if enabled)
curie test              # compile and run tests only — no JAR or Docker build
curie run               # build, then run via java -jar (or Docker)
curie clean             # remove target/

curie build --no-docker # suppress Docker even if curie.toml enables it
curie run   --no-docker
curie run   -- --my-arg # extra arguments are forwarded to the application
curie test  --filter com.example.MyTest  # run only tests matching a class-name pattern
```

---

## Configuration

Projects are described by a `curie.toml` file in the project root.

### Application project

```toml
[application]
name      = "my-app"
version   = "1.0.0"
mainClass = "com.example.Main"

[java]
sourceCompatibility = "21"   # passed to javac --release; default: 21

[dependencies]
"com.fasterxml.jackson.core:jackson-databind" = "2.17.2"
"com.google.guava:guava"                       = "33.2.0-jre"

[test-dependencies]
"org.junit.jupiter:junit-jupiter" = "6.0.3"

[[repositories]]
name = "My Nexus"
url  = "https://nexus.example.com/repository/maven-public"

[docker]
baseImage = "eclipse-temurin:21-jre-alpine"   # default
imageName = "my-app"    # default: application.name
imageTag  = "latest"    # default: application.version
```

Docker is enabled when either the `[docker]` section is present or a `Dockerfile` exists at the project root. Omit both to get a plain JAR build.

### Library project

Libraries omit `mainClass` and never build a Docker image. Declaring a `[docker]` section or placing a `Dockerfile` at the project root is an error.

```toml
[library]
name    = "my-lib"
version = "1.0.0"

[java]
sourceCompatibility = "21"

[dependencies]
"com.google.guava:guava" = "33.2.0-jre"

[test-dependencies]
"org.junit.jupiter:junit-jupiter" = "6.0.3"
```

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

---

## How it works

### Build pipeline

```
curie build
  │
  ├─ Parse curie.toml
  │
  ├─ Resolve production dependencies (curie-deps)
  │    └─ BFS transitive resolution:
  │         ├─ Check ~/.m2 cache
  │         ├─ Download JAR + POM from Maven Central if missing
  │         │    (atomic write via .part file to survive crashes)
  │         ├─ Parse POM: properties, dependencyManagement, parent chain
  │         └─ Enqueue compile-scoped transitive dependencies
  │
  ├─ Collect production Java sources
  │    └─ WalkDir src/main/java, sorted lexicographically
  │         Test files (*Test.java, *Tests.java, *Spec.java) are excluded
  │
  ├─ Incremental compilation check (mtime comparison)
  │    ├─ NoClassFiles   → compile
  │    ├─ SourceChanged  → compile
  │    ├─ TomlChanged    → compile
  │    └─ UpToDate       → skip javac
  │
  ├─ javac -g --release <N> -d target/classes [-cp deps] <sources...>
  │
  ├─ Test execution (see below)
  │
  ├─ Incremental JAR check (newest .class vs. existing JAR mtime)
  │    └─ Write deterministic JAR:
  │         ├─ MANIFEST.MF (Main-Class + Class-Path, if applicable)
  │         ├─ Entries sorted lexicographically
  │         └─ All timestamps = 2024-01-01 00:00:00 UTC
  │
  └─ Docker build (if enabled, application projects only)
       ├─ User Dockerfile → docker build --build-arg JAR_FILE=...
       └─ Generated Dockerfile:
            ├─ Copy dep JARs to target/libs/ (incremental by mtime)
            ├─ Layer deps before app JAR for Docker cache efficiency
            └─ Stamp file (target/.docker-stamp) tracks freshness
```

### Test execution

`curie test` (and the test phase of `curie build`) follows this pipeline:

```
  ├─ Discover test sources
  │    ├─ src/main/java  — files named *Test.java, *Tests.java, *Spec.java
  │    └─ src/test/java  — all *.java files (directory is optional)
  │
  ├─ Check test-run stamp (target/.test-stamp)
  │    Skip everything below if the stamp is newer than:
  │      • all test source files
  │      • all production class files (target/classes/)
  │      • curie.toml
  │    → print "Tests  up to date" and stop
  │
  ├─ Resolve test dependencies ([test-dependencies] in curie.toml)
  │    └─ Same BFS resolver as production deps; kept separate from the
  │         production JAR classpath
  │
  ├─ Resolve JUnit Platform Console Standalone 6.0.3
  │    └─ Fetched from Maven Central, cached in ~/.m2 like any other dep
  │
  ├─ Incremental test compilation check
  │    └─ javac -g --release <N> -d target/test-classes \
  │             -cp target/classes:<deps>:<test-deps>:<standalone> \
  │             <test-sources...>
  │
  ├─ java -jar junit-platform-console-standalone.jar execute \
  │        -cp target/test-classes:target/classes:<deps>:<test-deps> \
  │        --scan-class-path [--include-classname=<filter>]
  │
  └─ On success: write target/.test-stamp
       The stamp is only written when no --filter is active.
       A partial (filtered) run must not mark the full suite as passing.
```

**Incremental test skipping** — after a successful full test run, Curie writes a stamp file (`target/.test-stamp`). Subsequent builds skip test execution entirely if the stamp is newer than all test sources, all production class files, and `curie.toml`. Any change to production or test code invalidates the stamp and forces a re-run.

**Test failure aborts the build** — when running via `curie build`, a test failure stops the pipeline before the JAR is written. The JAR will only be produced if all tests pass.

**`--filter` always runs** — `curie test --filter <pattern>` bypasses the stamp check and always executes, allowing targeted re-runs without forcing a full suite. Because it is a partial run it does not update the stamp.

### Dependency resolution

`curie-deps` is a standalone library crate that resolves Maven coordinates to local JAR paths. It uses a BFS loop over the transitive dependency graph, downloading only what is not already in the local Maven cache. POM files are parsed in streaming mode (`quick-xml`) — no full DOM is built. Property interpolation, `<dependencyManagement>` BOM handling, and parent POM chains (up to 10 levels) are all supported. Test, provided, and optional scopes are excluded.

### Repository layout

```
curie/
  Cargo.toml          — Cargo workspace (two crates)
  curie-build/        — CLI binary: build pipeline, Docker, run logic
  curie-deps/         — library: Maven resolution, POM parsing, local cache
  examples/
    hello-world/      — minimal application project, no dependencies
    json-greeter/     — application project with Jackson dependency
    string-utils/     — library project with JUnit 5 tests
```

---

## Known limitations

- No multi-module projects.
- No plugin API.
- Source layout is fixed to `src/main/java` (production) and `src/test/java` (tests).
- Stale test or production class files from deleted sources require `curie clean` to clear.
- No IDE integration.

---

## Status

This is a research project. The build tool layer is functional and the examples work end-to-end. Contributions and feedback are welcome.
