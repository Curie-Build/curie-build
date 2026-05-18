# Curie

> **Research project** — exploring whether a Rust-based build tool for Java is feasible as a drop-in replacement for Maven and Gradle.

The Java build tooling landscape has been largely static for two decades. Maven arrived in 2004 and brought convention over configuration and a centralized repository — genuinely transformative at the time. Gradle followed in 2008 and replaced XML with a programmable DSL. Both tools have since accumulated layers of abstraction, plugins, and workarounds to accommodate workflows that simply did not exist when they were designed: containerised deployments, reproducible supply chains, polyglot monorepos, sub-second feedback loops in CI. The result is that a new Java project in 2026 routinely ships with hundreds of lines of build configuration and additional scripts.

Other language ecosystems have quietly raised the bar. Cargo, Rust's built-in build tool, ships workspaces, a lockfile, and reproducible dependency resolution as first-class features with no plugins required — a new project is correct and reproducible by default. Go takes the same philosophy further: `go.mod` and `go.sum` are part of the toolchain itself, so deterministic builds and zero-configuration module management are simply the starting point, not optional add-ons. Java developers working across languages notice the gap.

This is how progress in tooling tends to work: an ecosystem experiments, different approaches compete, and over time the field converges on what actually works. Maven followed this approach by proposing excellent conventions. What Cargo and Go modules show is that the conventions have moved — there is a new baseline - Maven's conventions have not evolved.

Curie is an experiment in what a Java build tool looks like if it starts from the new conventions rather than inheriting the old ones. It is a fast, minimal build tool for Java projects written in Rust. It handles dependency resolution from Maven Central, incremental compilation, [reproducible builds](https://reproducible-builds.org), test execution, and optional Docker image building — driven by a single `Curie.toml` configuration file.


This is a work in progress. The current implementation covers the core build pipeline.

---

## Why Curie?

Maven and Gradle are powerful but carry significant complexity and overhead. Curie trades extensibility for simplicity and speed.

| | Curie | Maven | Gradle |
|---|---|---|---|
| **Configuration** | `Curie.toml` (~20 lines) | `pom.xml` (100+ lines of XML) | `build.gradle` (Groovy/Kotlin DSL) |
| **Startup time** | Near-instant (native binary) | JVM startup (~1–2 s) | JVM startup + daemon overhead |
| **Incremental builds** | Built-in | Plugin-dependent | Built-in but complex |
| Reproducible builds | Yes, out of the box | Extra plugin config required | Extra plugin config required |
| **Docker support** | First-class, auto-generated optimised `Dockerfile` | External plugins | External plugins |
| **Learning curve** | Minimal — one file format | High — lifecycle phases, plugin ecosystem | High — DSL, task graph, plugin API |
| **Multi-module projects** | One workspace `Curie.toml`; topo-sort and BOM inherited automatically | Aggregator POM + per-module POMs | `settings.gradle` + per-module `build.gradle`|
| **Binary size** | Single static binary, no JVM needed | Requires JVM | Requires JVM |

Curie will never match the plugin ecosystem of Maven or Gradle. The goal is a focused tool that handles the 80% case — compile, test, package, deploy — with far less ceremony.

---

## Current capabilities

- **Dependency resolution** — resolves Maven dependencies transitively from Maven Central (or custom repositories), caches JARs and POMs to `~/.m2/repository` using the standard Maven layout. Parallel JAR downloads with a progress bar. Maven nearest-wins conflict resolution.
- **BOM imports** — `[bom-imports]` and `[test-bom-imports]` sections import a Bill of Materials to centrally manage dependency versions. Omit the version string (`""`) in `[dependencies]` to have it resolved from the BOM.
- **Incremental compilation** — skips `javac` when sources, `Curie.toml`, and the JDK version are all unchanged relative to the existing class files. Stale `.class` files from deleted sources are removed automatically.
- **[Reproducible builds](https://reproducible-builds.org)** — all ZIP entry timestamps are clamped to a fixed epoch (2024-01-01 UTC); entries are sorted lexicographically, producing byte-identical JARs for identical inputs.
- **Test execution** — discovers and runs JUnit 5 tests automatically, with incremental skip when nothing has changed.
- **Docker support** — builds and optionally runs a Docker image. Curie auto-generates a cache-optimised `Dockerfile` that layers dependency JARs before the application JAR, so a code-only change does not invalidate the dependency layer.
- **Custom repositories** — additional Maven repositories can be declared alongside Maven Central.
- **Multiple source layouts** — supports both the Maven layout (`src/main/java/`) and a flat-package layout where source roots are dot-named directories directly under `src/` (e.g. `src/com.example.myapp/`). The two layouts can coexist.
- **Resources** — `src/main/resources` (Maven layout) or a top-level `resources/` directory (flat-package layout) are included in the JAR and classpath. Test resources (`src/test/resources` / `test-resources/`) are added to the test classpath.
- **Workspace / multi-module projects** — a workspace `Curie.toml` lists member directories; Curie builds them in dependency order. Members can declare `[workspace-dependencies]` to depend on sibling members. Workspace-level `[java]`, `[[repositories]]`, `[bom-imports]`, and `[test-bom-imports]` are inherited by all members.
- **Offline mode** — `--offline` prevents any network access; a cache miss is an immediate error.
- **Build info** — when the project is inside a Git repository, Curie automatically embeds `META-INF/build-info.properties` in the JAR with the commit id of the build. If the working tree has local changes the id is suffixed with `-dirty`. Can be disabled per-project.

### Commands

```
curie build             # resolve deps, compile, run tests, package JAR, build Docker image (if enabled)
curie test              # compile and run tests only — no JAR or Docker build
curie run               # build, then run via java -jar (or Docker)
curie clean             # remove target/
curie list              # list the members of a workspace

curie build --no-docker # suppress Docker even if Curie.toml enables it
curie build --offline   # use only locally cached artifacts; fail on any cache miss
curie run   --no-docker
curie run   -- --my-arg # extra arguments are forwarded to the application
curie test  --filter com.example.MyTest  # run only tests matching a class-name pattern (regex)

curie --project path/to/member build  # target a specific project or workspace member
```

---

## Configuration

Projects are described by a `Curie.toml` file in the project root.

### Application project

```toml
[application]
name      = "my-app"
version   = "1.0.0"
mainClass = "com.example.Main"   # optional — auto-detected from bytecode if omitted

[java]
sourceCompatibility = "21"   # passed to javac --release; default: 21

[dependencies]
"com.fasterxml.jackson.core:jackson-databind" = "2.17.2"
"com.google.guava:guava"                       = "33.2.0-jre"

[test-dependencies]
"org.junit.jupiter:junit-jupiter" = "5.11.0"

[[repositories]]
name = "My Nexus"
url  = "https://nexus.example.com/repository/maven-public"

[docker]
baseImage = "eclipse-temurin:21-jre-alpine"   # default
imageName = "my-app"    # default: application.name
imageTag  = "latest"    # default: application.version
```

Docker is enabled when either the `[docker]` section is present or a `Dockerfile` exists at the project root. Omit both to get a plain JAR build.

### Build info

When the project directory is inside a Git repository, Curie automatically generates `META-INF/build-info.properties` in the root of the JAR's classpath.

```
META-INF/build-info.properties
  git.commit.id=a3f1b2c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0
```

If the working tree has uncommitted changes (staged, unstaged, or untracked files) the value is suffixed with `-dirty`:

```
git.commit.id=a3f1b2c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0-dirty
```

To opt out, add a `[build-info]` section to `Curie.toml`:

```toml
[build-info]
enabled = false
```

No configuration is needed to enable it — the file is generated automatically whenever `git` is on `PATH` and the project is inside a Git repository.

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
"org.junit.jupiter:junit-jupiter" = "5.11.0"
```

### BOM imports

Use `[bom-imports]` to import a Bill of Materials. Dependencies whose version is `""` have their version resolved from the BOM.

```toml
[bom-imports]
"com.fasterxml.jackson:jackson-bom" = "2.17.2"

[test-bom-imports]
"org.junit:junit-bom" = "5.11.0"

[dependencies]
"com.fasterxml.jackson.core:jackson-databind" = ""   # version from jackson-bom

[test-dependencies]
"org.junit.jupiter:junit-jupiter" = ""   # version from junit-bom
```

### Workspace

A workspace `Curie.toml` groups multiple member projects. Members are built in topological order based on their `[workspace-dependencies]`.

```toml
[workspace]
members = ["lib-a", "lib-b", "app"]

[java]
sourceCompatibility = "21"   # inherited by all members

[bom-imports]
"com.fasterxml.jackson:jackson-bom" = "2.17.2"   # inherited by all members

[test-bom-imports]
"org.junit:junit-bom" = "5.11.0"   # inherited by all members
```

A member that depends on a sibling:

```toml
# app/Curie.toml
[application]
name    = "app"
version = "0.1.0"

[workspace-dependencies]
lib-a = { path = "../lib-a" }
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
  ├─ Parse Curie.toml
  │
  ├─ Resolve production dependencies (curie-deps)
  │    └─ BFS transitive resolution:
  │         ├─ Check ~/.m2 cache
  │         ├─ Download JAR + POM from Maven Central if missing
  │         │    (atomic write via .part file to survive crashes;
  │         │     up to 8 parallel downloads with progress bar)
  │         ├─ Parse POM: properties, dependencyManagement, parent chain
  │         │    (up to 10 levels); nearest-wins conflict resolution
  │         └─ Enqueue compile-scoped transitive dependencies
  │
  ├─ Collect production Java sources
  │    ├─ Maven layout:       src/main/java/**/*.java
  │    └─ Flat-package layout: src/<dot.pkg>/**/*.java
  │         Test files (*Test.java, *Tests.java, *Spec.java) are excluded
  │
  ├─ Incremental compilation check
  │    ├─ NoClassFiles   → compile
  │    ├─ SourceChanged  → compile
  │    ├─ TomlChanged    → compile
  │    ├─ StaleClasses   → stale .class files removed, then compile
  │    ├─ JdkChanged     → JDK version changed, full recompile
  │    └─ UpToDate       → skip javac
  │
  ├─ javac -g --release <N> -d target/classes [-cp deps] <sources...>
  │
  ├─ Test execution (see below)
  │
  ├─ Incremental JAR check (newest .class vs. existing JAR mtime)
  │    └─ Write deterministic JAR:
  │         ├─ MANIFEST.MF (Main-Class + Class-Path, if applicable)
  │         ├─ META-INF/build-info.properties (git.commit.id, when in a Git repo)
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
  │    ├─ Maven layout:
  │    │    ├─ src/main/java  — files named *Test.java, *Tests.java, *Spec.java
  │    │    └─ src/test/java  — all *.java files (directory is optional)
  │    └─ Flat-package layout:
  │         ├─ src/<dot.pkg>/ — files named *Test.java, *Tests.java, *Spec.java
  │         └─ tests/<dot.pkg>/ — all *.java files (integration tests)
  │
  ├─ Check test-run stamp (target/.test-stamp)
  │    Skip everything below if the stamp is newer than:
  │      • all test source files
  │      • all production class files (target/classes/)
  │      • Curie.toml
  │      • resource directories (src/main/resources, src/test/resources, etc.)
  │    → print "Tests  up to date" and stop
  │
  ├─ Resolve test dependencies ([test-dependencies] in Curie.toml)
  │    └─ Same BFS resolver as production deps; kept separate from the
  │         production JAR classpath
  │
  ├─ Resolve JUnit Platform Console Standalone
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

**Incremental test skipping** — after a successful full test run, Curie writes a stamp file (`target/.test-stamp`). Subsequent builds skip test execution entirely if the stamp is newer than all test sources, all production class files, resource directories, and `Curie.toml`. Any change to production or test code invalidates the stamp and forces a re-run.

**Test failure aborts the build** — when running via `curie build`, a test failure stops the pipeline before the JAR is written. The JAR will only be produced if all tests pass.

**`--filter` always runs** — `curie test --filter <pattern>` bypasses the stamp check and always executes, allowing targeted re-runs without forcing a full suite. The pattern is a regular expression matched against fully-qualified class names. Because it is a partial run it does not update the stamp.

### Dependency resolution

`curie-deps` is a standalone library crate that resolves Maven coordinates to local JAR paths. It uses a BFS loop over the transitive dependency graph, downloading only what is not already in the local Maven cache. POM files are parsed in streaming mode (`quick-xml`) — no full DOM is built. Property interpolation, `<dependencyManagement>` BOM handling, and parent POM chains (up to 10 levels) are all supported. Maven nearest-wins conflict resolution is applied. Test, provided, and optional scopes are excluded.

### Repository layout

```
curie/
  Cargo.toml          — Cargo workspace (two crates)
  curie-build/        — CLI binary: build pipeline, Docker, run logic
  curie-deps/         — library: Maven resolution, POM parsing, local cache
  examples/
    Curie.toml        — workspace root for all examples
    hello-world/      — flat-package application, no dependencies
    json-greeter/     — flat-package application with Jackson dependency
    string-utils/     — flat-package library with JUnit 5 tests
    string-utils-cli/ — application with [workspace-dependencies] on string-utils
    jackson-bom-greeter/     — flat-package application using BOM-managed versions
    maven-hello-world/       — Maven-layout counterpart of hello-world
    maven-json-greeter/      — Maven-layout counterpart of json-greeter
    maven-string-utils/      — Maven-layout counterpart of string-utils
    maven-jackson-bom-greeter/ — Maven-layout counterpart of jackson-bom-greeter
```

The paired `<name>` / `maven-<name>` examples demonstrate the two source layouts (flat-package vs. `src/main/java/`) side by side with identical functionality.

---

## Known limitations

- No plugin API.
- No IDE integration.
- `curie run` via Docker is not supported for workspace members that have `[workspace-dependencies]`.

---

## Status

This is a research project. The build tool layer is functional and the examples work end-to-end. Contributions and feedback are welcome.
