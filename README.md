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
- **Multiple source layouts** — supports both the Maven layout (`src/main/java/`, `src/main/kotlin/`) and a flat-package layout where source roots are dot-named directories directly under `src/` (e.g. `src/com.example.myapp/`). The two layouts can coexist.
- **Kotlin support** — `.kt` files are compiled automatically with no configuration. Curie detects Kotlin sources, downloads `kotlinc` from Maven Central (version configurable via the workspace-inheritable `[kotlin] version` key), and runs a two-phase compile (kotlinc first, then javac) so Java and Kotlin can reference each other freely. Mixed Java/Kotlin projects work out of the box.
- **Resources** — `src/main/resources` (Maven layout) or a top-level `resources/` directory (flat-package layout) are included in the JAR and classpath. Test resources (`src/test/resources` / `test-resources/`) are added to the test classpath.
- **Workspace / multi-module projects** — a workspace `Curie.toml` lists member directories; Curie builds them in dependency order. Members can declare `[workspace-dependencies]` to depend on sibling members. Workspace-level `[java]`, `[[repositories]]`, `[bom-imports]`, `[test-bom-imports]`, `[test]`, and `[kotlin]` are inherited by all members.
- **Offline mode** — `--offline` prevents any network access; a cache miss is an immediate error.
- **Build info** — when the project is inside a Git repository, Curie automatically embeds `META-INF/build-info.properties` in the JAR with the commit id of the build. If the working tree has local changes the id is suffixed with `-dirty`. Can be disabled per-project.

### Commands

```
curie new app   <name>  # scaffold a new application project in a new subdirectory
curie new lib   <name>  # scaffold a new library project in a new subdirectory
curie new workspace <name>  # scaffold a new workspace in a new subdirectory
curie init app          # initialise an application project in the current directory
curie init lib          # initialise a library project in the current directory
curie init workspace    # initialise a workspace in the current directory

curie build             # resolve deps, compile, run tests, package JAR, build Docker image (if enabled)
curie test              # compile and run tests only — no JAR or Docker build
curie run               # build, then run via java -jar (or Docker)
curie fmt               # format all Java source files with palantir-java-format
curie clean             # remove target/
curie list              # list the members of a workspace
curie audit             # emit CycloneDX SBOM and scan dependencies against OSV

curie build --no-docker # suppress Docker even if Curie.toml enables it
curie build --offline   # use only locally cached artifacts; fail on any cache miss
curie run   --no-docker
curie run   -- --my-arg # extra arguments are forwarded to the application
curie test  --filter com.example.MyTest  # run only tests matching a class-name pattern (regex)
curie fmt   --check     # check formatting without modifying files (exits non-zero if any file needs changes)
curie fmt   --offline   # use only cached formatter JARs; fail if not present

curie --project path/to/member build  # target a specific project or workspace member
```

---

## Scaffolding

`curie new` and `curie init` generate a ready-to-build project skeleton — the
`cargo new` ergonomics that Maven archetypes never delivered.

### `curie new` — create a project in a new subdirectory

```sh
curie new app   my-app      # creates ./my-app/ with an application skeleton
curie new lib   my-lib      # creates ./my-lib/ with a library skeleton
curie new workspace my-ws   # creates ./my-ws/ with an empty workspace
```

For `app` and `lib`, `[name]` defaults to the current directory name when
omitted.

### `curie init` — initialise in the current directory

```sh
mkdir my-app && cd my-app
curie init app              # writes Curie.toml + source skeleton into ./
curie init lib
curie init workspace
```

Fails immediately if `Curie.toml` already exists.

### Generated layout

For `curie new app my-app` (or `curie init app` inside `my-app/`):

```
my-app/
├── .gitignore          # target/
├── Curie.toml
└── src/
    └── com.example.myapp/
        └── MyApp.java
```

For `curie new lib my-lib`:

```
my-lib/
├── .gitignore
├── Curie.toml
└── src/
    └── com.example.mylib/
        └── MyLib.java
```

For `curie new workspace my-ws`:

```
my-ws/
├── .gitignore
└── Curie.toml          # [workspace] members = []
```

### Package name

The Java package is derived automatically from the project name:

| Project name | Package | Class |
|---|---|---|
| `my-app` | `com.example.myapp` | `MyApp` |
| `hello-world` | `com.example.helloworld` | `HelloWorld` |
| `string_utils` | `com.example.stringutils` | `StringUtils` |

Override with `--package`:

```sh
curie new app my-app --package org.acme.demo
```

### Workspace auto-registration

If you run `curie new` or `curie init` inside a directory that already
contains a workspace `Curie.toml`, Curie automatically appends the new
project to `members`:

```sh
cd my-workspace
curie new app hello         # also adds "hello" to ./Curie.toml members
```

The workspace file is updated with format-preserving edits — comments and
ordering are preserved.

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

# Optional tool-version overrides (workspace-inheritable).
# The defaults are the same versions that were previously hardcoded.
[test]
junitPlatformVersion = "6.0.3"   # JUnit Platform Console Standalone runner

[kotlin]
version = "2.1.21"               # kotlinc + kotlin-stdlib (only needed when .kt sources exist)

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

### GraalVM native-image

Curie can compile an application to a standalone native binary using [GraalVM native-image](https://www.graalvm.org/latest/reference-manual/native-image/).  The step is **opt-in**: add a `[native-image]` section to `Curie.toml`.

```toml
[native-image]
# Output binary name written to target/ (default: application.name)
outputName = "my-app"

# Directory containing GraalVM reachability-metadata JSON files
# (reflect-config.json, resource-config.json, proxy-config.json, …).
# Passed as -H:ConfigurationFileDirectories=<abs-path>.
configDir = "src/main/resources/META-INF/native-image"

# Extra flags forwarded verbatim to native-image (appended last).
extraArgs = ["--no-fallback"]
```

`mainClass` must be declared in `[application]`; auto-detection is not supported for native compilation.

**Commands:**

```
curie build               # includes the native-image step when [native-image] is present
curie build --no-native   # suppress native-image even if Curie.toml enables it
curie native              # compile + package JAR (skips tests), then run native-image
```

`curie native` is optimised for the inner compile→native iteration loop.  Use `curie build` when you also want tests to run before the native step.

**Locating `native-image`:**

Curie checks, in order:
1. `$GRAALVM_HOME/bin/native-image`
2. `native-image` on `$PATH`

Install GraalVM from <https://www.graalvm.org/downloads/> or via [sdkman](https://sdkman.io):
```
sdk install java 25.0.1-graal
```

Library projects do not support native-image; declaring `[native-image]` in a library `Curie.toml` is an error.

See `examples/graalvm-hello/` for a working example.



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

### Formatting (`curie fmt`)

`curie fmt` formats all Java source files in the project using
[palantir-java-format](https://github.com/palantir/palantir-java-format) —
a modern, lambda-friendly, 120-character Java formatter.

```
curie fmt           # reformat all .java files in place
curie fmt --check   # exit non-zero if any file is not correctly formatted (CI)
curie fmt --offline # use only cached JARs; fail if not present
```

On the first invocation, Curie downloads `palantir-java-format` and its
transitive dependencies from Maven Central into the local `~/.m2` cache —
the same mechanism used for project dependencies.  Subsequent runs reuse
the cache and require no network access.

Source files under `src/main/java/`, `src/test/java/`, and any
flat-package source roots are all formatted in a single pass.

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

## Audit

`curie audit` resolves your dependency closure, emits a
[CycloneDX 1.6](https://cyclonedx.org/specification/overview/) SBOM at
`target/sbom.cdx.json`, and checks every component against the
[OSV vulnerability database](https://osv.dev/).

```
curie audit                           # scan production deps; exit 1 on any finding
curie audit --include-test            # also include test-scope deps
curie audit --offline                 # emit SBOM only; skip OSV network call
curie audit --full                    # fetch full detail (summary, fixed versions, CVSS)
curie audit --full --severity 9.0     # only fail on CRITICAL findings (CVSS ≥ 9.0)
curie audit --output path/to/sbom.json  # override the SBOM output path
```

### SBOM

The SBOM is written to `target/sbom.cdx.json` by default.  Each dependency in
the closure becomes a `library` component with a Maven PURL
(`pkg:maven/<groupId>/<artifactId>@<version>`).  Production deps are marked
`scope: required`; test deps (when `--include-test` is set) are marked
`scope: optional`.

The `metadata.component` field is populated when `groupId` is set in
`Curie.toml`'s `[application]` or `[library]` section.

### Vulnerability scanning

Without `--full`, only vuln IDs are shown and **any finding causes exit 1**.
This is the conservative default — IDs alone carry no CVSS score.

With `--full`, curie fetches full detail from OSV and exits 1 only when the
highest CVSS score across all findings meets or exceeds `--severity` (default
`7.0`, i.e. HIGH and above).

CVSS scores are derived from the `database_specific.severity` field in OSV
advisories (the primary source for GHSA advisories):

| OSV severity | CVSS equivalent |
|---|---|
| CRITICAL | 9.0 |
| HIGH | 7.0 |
| MEDIUM | 4.0 |
| LOW | 1.0 |

Findings with an unrecognised or absent severity string always trigger exit 1
when `--full` is used, to avoid silently passing unknown risks.

### Workspace

When invoked at a workspace root, `curie audit` runs the full pipeline for
every member in topological order and exits 1 if **any** member has findings
that exceed the threshold.

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
    hello-kotlin/    — Kotlin application (Maven layout, auto-detected, no config needed)
    hello-mixed/     — Java + Kotlin sources in the same flat-package directory (interop demo)
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
