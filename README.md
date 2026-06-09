# Curie

Fast, minimal build tool for Java, Kotlin, and Groovy — one `Curie.toml` replaces hundreds of lines of POM XML.

---

## Getting started

### Prerequisites

- **JDK 21** or newer on `PATH` — Curie shells out to `javac`.
- **Cargo** (from [rustup](https://rustup.rs)) — Curie installs via `cargo install`.

### Install

```bash
cargo install curie-build
curie --version
```

### Create a project

```bash
curie new app hello
cd hello
```

### Build

```bash
curie build

Building hello v0.1.0
  Compile         1 source file(s)
  Tests           no test sources found
  Package         hello-0.1.0.jar
  Done            target/hello-0.1.0.jar
```

---

## Documentation

Full documentation at **[https://curie-build.org/docs/](https://curie-build.org/docs/)**.
