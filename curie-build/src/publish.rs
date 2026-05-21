//! `curie publish` — package + upload artifacts to a Maven repository.
//!
//! Produces Maven Central-compliant artifacts by default:
//!   * `<artifact>-<version>.jar`           (already built by `curie build`)
//!   * `<artifact>-<version>-sources.jar`   (built here)
//!   * `<artifact>-<version>-javadoc.jar`   (built here; opt out via --no-javadoc)
//!   * `<artifact>-<version>.pom`           (generated from descriptor)
//!   * `.asc` GPG signatures for each of the above (opt out via --no-sign)
//!   * `.sha1`, `.md5`, `.sha256`, `.sha512` sidecars for each artifact
//!
//! Upload is HTTP PUT to `{base}/{group_path}/{artifact}/{version}/<file>` with
//! Basic auth resolved from `[[credentials]]` in `~/.curie/config.toml` (or
//! the `CURIE_PUBLISH_USERNAME`/`CURIE_PUBLISH_PASSWORD` env vars when the
//! target is an inline URL).

use crate::build::{build_with_desc, BuildOptions};
use crate::config;
use crate::descriptor::{self, Descriptor};
use crate::pom_writer;
use crate::sources_jar;
use anyhow::{bail, Context, Result};
use curie_deps::{DepEntry, Gav, ResolveOptions};
use std::path::{Path, PathBuf};

/// CLI options.  Mapped from the `Publish` subcommand in `main.rs`.
#[derive(Debug)]
pub struct PublishOptions {
    /// Override `[publish] repository`/`url` from `Curie.toml`.
    pub repo_url: Option<String>,
    /// Force GPG signing off.
    pub no_sign: bool,
    /// Force javadoc jar off.
    pub no_javadoc: bool,
    /// Build everything and print the upload plan but do not PUT.
    pub dry_run: bool,
    /// Skip the test phase during the rebuild that precedes publishing.
    /// (Not yet exposed on the CLI — reserved for future `--skip-tests`.)
    #[allow(dead_code)]
    pub skip_tests: bool,
}

/// Top-level entry point invoked by `curie publish`.
pub fn publish(project_root: &Path, opts: PublishOptions) -> Result<()> {
    let desc = descriptor::load(project_root)?;
    if desc.is_workspace() {
        bail!("`curie publish` cannot run on a workspace root; target a member with --project");
    }

    let group_id = desc
        .group_id()
        .ok_or_else(|| anyhow::anyhow!(
            "groupId is required for publishing — add `groupId = \"...\"` to the [{}] section",
            if desc.is_library() { "library" } else { "application" },
        ))?
        .to_string();

    validate_for_publish(&desc)?;

    let cfg = config::load_config().unwrap_or_default();
    // Resolve the target URL eagerly (needed to print the upload plan).
    // Credentials are deferred to upload time — dry runs don't need them.
    let target_url = resolve_target_url(&desc, opts.repo_url.as_deref())?;

    let sign = desc.publish.sign && !opts.no_sign;
    let javadoc = desc.publish.javadoc && !opts.no_javadoc;

    // --- run the full build pipeline (compile + test + package the main jar) -
    let build_out = build_with_desc(
        project_root,
        &desc,
        BuildOptions {
            no_docker: true, // publishing never builds docker
            no_native: true, // publishing never builds native binaries
            offline: false,
        },
        &[],
    )
    .context("build before publish failed")?;

    let target_dir = project_root.join("target");
    let artifact_id = desc.buildable_name();
    let version = desc.buildable_version();
    let base_name = format!("{}-{}", artifact_id, version);

    // --- sources jar ---------------------------------------------------------
    let sources_jar_path = target_dir.join(format!("{}-sources.jar", base_name));
    let src_roots = collect_src_roots(project_root);
    let resources_dir = build_out.resources_dir.as_deref();
    sources_jar::write_sources_jar(&sources_jar_path, &src_roots, resources_dir)
        .context("failed to build sources jar")?;
    println!("  Sources jar     {}", sources_jar_path.file_name().unwrap().to_string_lossy());

    // --- javadoc jar (optional) ----------------------------------------------
    let javadoc_jar_path: Option<PathBuf> = if javadoc {
        let p = target_dir.join(format!("{}-javadoc.jar", base_name));
        build_javadoc_jar(project_root, &src_roots, &p)
            .context("failed to build javadoc jar")?;
        println!("  Javadoc jar     {}", p.file_name().unwrap().to_string_lossy());
        Some(p)
    } else {
        None
    };

    // --- POM -----------------------------------------------------------------
    let declared_gavs = resolve_declared_dep_gavs(&desc)?;
    let pom_path = target_dir.join(format!("{}.pom", base_name));
    pom_writer::write_pom(&desc, &declared_gavs, &pom_path)
        .context("failed to write POM")?;
    println!("  POM             {}", pom_path.file_name().unwrap().to_string_lossy());

    // --- collect all artifacts to upload -------------------------------------
    let mut artifacts: Vec<UploadArtifact> = Vec::new();
    artifacts.push(UploadArtifact::new(&build_out.jar, "")); // main jar
    artifacts.push(UploadArtifact::new(&sources_jar_path, "-sources"));
    if let Some(ref p) = javadoc_jar_path {
        artifacts.push(UploadArtifact::new(p, "-javadoc"));
    }
    artifacts.push(UploadArtifact::pom(&pom_path));

    // --- GPG sign ------------------------------------------------------------
    if sign {
        for a in &mut artifacts {
            let asc = gpg_sign(&a.path)?;
            a.signature = Some(asc);
        }
        println!("  Signed          {} artifact(s)", artifacts.len());
    }

    // --- build and print upload plan ----------------------------------------
    let group_path = group_id.replace('.', "/");
    let base_dir = format!(
        "{}/{}/{}/{}",
        target_url.trim_end_matches('/'),
        group_path,
        artifact_id,
        version,
    );
    println!("  Publishing to   {}", base_dir);

    let upload_jobs = build_upload_plan(&artifacts, &base_dir);
    for j in &upload_jobs {
        println!("    → {}", j.url);
    }

    if opts.dry_run {
        println!("  Dry-run         {} file(s) would be uploaded", upload_jobs.len());
        return Ok(());
    }

    // --- credentials + actually PUT ------------------------------------------
    let credentials = resolve_credentials(&desc, &cfg, opts.repo_url.as_deref())?;
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-publish/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?;

    for job in &upload_jobs {
        upload_one(&client, &credentials, job)
            .with_context(|| format!("upload failed: {}", job.url))?;
    }
    println!("  Uploaded        {} file(s)", upload_jobs.len());
    Ok(())
}

/// Validate that all POM-metadata fields required for a clean publish are set.
/// Emits a single error listing every missing field.
pub fn validate_for_publish(desc: &Descriptor) -> Result<()> {
    let p = &desc.publish;
    let mut missing = Vec::new();
    if p.description.is_none() {
        missing.push("[publish] description");
    }
    if p.homepage.is_none() {
        missing.push("[publish] homepage");
    }
    if p.licenses.is_empty() {
        missing.push("[publish] licenses");
    }
    if p.developers.is_empty() {
        missing.push("[publish] developers");
    }
    if p.scm.is_none() {
        missing.push("[publish] scm");
    }
    if !missing.is_empty() {
        bail!(
            "the following fields are required for publishing:\n  - {}",
            missing.join("\n  - "),
        );
    }
    Ok(())
}

#[derive(Debug)]
struct Credentials {
    username: String,
    password: String,
}

/// Determine the publish target URL from descriptor + CLI override.
/// No credential lookup happens here, so this can be used by `--dry-run`
/// without requiring `[[credentials]]` to be configured.
fn resolve_target_url(desc: &Descriptor, cli_override: Option<&str>) -> Result<String> {
    let p = &desc.publish;
    if let Some(url) = cli_override {
        return Ok(url.to_string());
    }
    if let Some(repo_id) = &p.repository {
        let repo = desc
            .repositories
            .iter()
            .find(|r| &r.id == repo_id)
            .ok_or_else(|| anyhow::anyhow!(
                "[publish] repository = \"{}\" does not match any [[repositories]] entry in Curie.toml",
                repo_id,
            ))?;
        return Ok(repo.url.clone());
    }
    if let Some(url) = &p.url {
        return Ok(url.clone());
    }
    bail!(
        "no publish target — set `[publish] repository = \"<id>\"` (must match a [[repositories]] entry) \
         or `[publish] url = \"...\"`, or pass --repo <url>"
    );
}

/// Resolve Basic-auth credentials for the publish target.
/// Looks up `[[credentials]]` by `repo_id` when one is configured; falls
/// back to `CURIE_PUBLISH_USERNAME` / `CURIE_PUBLISH_PASSWORD` env vars for
/// url-only targets.
fn resolve_credentials(
    desc: &Descriptor,
    cfg: &config::CurieConfig,
    cli_override: Option<&str>,
) -> Result<Credentials> {
    let p = &desc.publish;
    // CLI override forces env-var fallback (no repo_id to key on).
    if cli_override.is_some() || p.url.is_some() {
        let u = std::env::var("CURIE_PUBLISH_USERNAME").context(
            "CURIE_PUBLISH_USERNAME env var must be set when publishing to an inline URL",
        )?;
        let pw = std::env::var("CURIE_PUBLISH_PASSWORD").context(
            "CURIE_PUBLISH_PASSWORD env var must be set when publishing to an inline URL",
        )?;
        return Ok(Credentials { username: u, password: pw });
    }
    let repo_id = p.repository.as_deref().ok_or_else(|| anyhow::anyhow!(
        "internal: resolve_target_url should have rejected this case",
    ))?;
    let cred_entry = config::credentials_for(cfg, repo_id)
        .ok_or_else(|| anyhow::anyhow!(
            "no [[credentials]] entry for repo_id = \"{}\" in ~/.curie/config.toml",
            repo_id,
        ))?;
    let (u, pw) = cred_entry.resolve()?;
    Ok(Credentials { username: u, password: pw })
}

/// Build the same source-root list `compile.rs` uses, so the sources jar
/// mirrors what was actually compiled.
fn collect_src_roots(project_root: &Path) -> Vec<PathBuf> {
    use crate::compile::flat_package_src_dirs;
    let mut out = Vec::new();
    let maven_java = project_root.join("src").join("main").join("java");
    let maven_kotlin = project_root.join("src").join("main").join("kotlin");
    if maven_java.exists() {
        out.push(maven_java);
    }
    if maven_kotlin.exists() {
        out.push(maven_kotlin);
    }
    out.extend(flat_package_src_dirs(project_root));
    out
}

fn build_javadoc_jar(
    project_root: &Path,
    src_roots: &[PathBuf],
    out_jar: &Path,
) -> Result<()> {
    use std::process::Command;
    if which::which("javadoc").is_err() {
        bail!("`javadoc` not found on PATH — install a JDK or pass --no-javadoc");
    }
    let out_dir = project_root.join("target").join("javadoc-out");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).context("failed to create target/javadoc-out")?;

    let mut all_java_files: Vec<PathBuf> = Vec::new();
    for root in src_roots {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|s| s.to_str()) == Some("java")
            {
                all_java_files.push(entry.path().to_path_buf());
            }
        }
    }
    if all_java_files.is_empty() {
        // Nothing to document — write an empty javadoc jar so the artifact exists.
        return crate::sources_jar::write_sources_jar(out_jar, &[], None);
    }

    let status = Command::new("javadoc")
        .arg("-quiet")
        .arg("-Xdoclint:none")
        .arg("-d")
        .arg(&out_dir)
        .args(&all_java_files)
        .status()
        .context("failed to spawn javadoc")?;
    if !status.success() {
        bail!("javadoc exited with status {}", status);
    }

    write_dir_as_jar(out_jar, &out_dir).context("failed to package javadoc output as jar")
}

/// Zip the contents of `src_dir` into a deterministic jar at `jar_path`.
fn write_dir_as_jar(jar_path: &Path, src_dir: &Path) -> Result<()> {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("cannot create {}", jar_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0).unwrap())
        .unix_permissions(0o644);

    let mut entries: std::collections::BTreeMap<String, PathBuf> = Default::default();
    for entry in walkdir::WalkDir::new(src_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(src_dir).unwrap();
            entries.insert(
                rel.to_string_lossy().replace('\\', "/"),
                entry.path().to_path_buf(),
            );
        }
    }
    for (name, fs_path) in &entries {
        let bytes = std::fs::read(fs_path)?;
        zip.start_file(name.as_str(), opts)?;
        zip.write_all(&bytes)?;
    }
    zip.finish().context("failed to finalize javadoc jar")?;
    Ok(())
}

fn gpg_sign(path: &Path) -> Result<PathBuf> {
    use std::process::Command;
    if which::which("gpg").is_err() {
        bail!("`gpg` not found on PATH — install GnuPG or pass --no-sign");
    }
    let asc = path.with_extension(format!(
        "{}.asc",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let mut cmd = Command::new("gpg");
    cmd.arg("--batch")
        .arg("--yes")
        .arg("--detach-sign")
        .arg("--armor")
        .arg("--output")
        .arg(&asc);
    if let Ok(key) = std::env::var("GPG_KEY") {
        cmd.arg("--local-user").arg(key);
    }
    cmd.arg(path);
    let status = cmd.status().context("failed to spawn gpg")?;
    if !status.success() {
        bail!("gpg signing failed for {}", path.display());
    }
    Ok(asc)
}

/// One artifact to upload, plus optional `.asc` signature for it.
struct UploadArtifact {
    path: PathBuf,
    /// Suffix appended to base name on the remote, e.g. `""`, `"-sources"`,
    /// `"-javadoc"`.  For POMs this is "" and `extension_override` is `"pom"`.
    classifier: String,
    extension_override: Option<String>,
    signature: Option<PathBuf>,
}

impl UploadArtifact {
    fn new(path: &Path, classifier: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            classifier: classifier.to_string(),
            extension_override: None,
            signature: None,
        }
    }
    fn pom(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            classifier: String::new(),
            extension_override: Some("pom".to_string()),
            signature: None,
        }
    }
}

/// A single PUT job: local source path + remote URL.
#[derive(Debug)]
struct UploadJob {
    source: PathBuf,
    url: String,
    body_kind: BodyKind,
}

#[derive(Debug)]
enum BodyKind {
    /// Read bytes from `source` and PUT them as-is.
    File,
    /// Compute the digest of `source` and PUT it as a hex sidecar.
    Sha1,
    Sha256,
    Sha512,
}

fn build_upload_plan(artifacts: &[UploadArtifact], base_dir: &str) -> Vec<UploadJob> {
    let mut jobs = Vec::new();
    for a in artifacts {
        let ext = a
            .extension_override
            .as_deref()
            .unwrap_or_else(|| a.path.extension().and_then(|s| s.to_str()).unwrap_or("jar"));
        let base_name = a
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("artifact");
        // For pom: base_name is "<artifact>-<version>" with no classifier suffix
        // already; for jar artifacts the classifier suffix is part of file_stem.
        let _ = a.classifier; // classifier is purely informational here
        let main_url = format!("{}/{}.{}", base_dir, base_name, ext);

        jobs.push(UploadJob { source: a.path.clone(), url: main_url.clone(), body_kind: BodyKind::File });
        jobs.push(UploadJob { source: a.path.clone(), url: format!("{main_url}.sha1"),   body_kind: BodyKind::Sha1   });
        jobs.push(UploadJob { source: a.path.clone(), url: format!("{main_url}.sha256"), body_kind: BodyKind::Sha256 });
        jobs.push(UploadJob { source: a.path.clone(), url: format!("{main_url}.sha512"), body_kind: BodyKind::Sha512 });

        if let Some(asc) = &a.signature {
            let asc_url = format!("{main_url}.asc");
            jobs.push(UploadJob { source: asc.clone(), url: asc_url.clone(),         body_kind: BodyKind::File });
            jobs.push(UploadJob { source: asc.clone(), url: format!("{asc_url}.sha1"), body_kind: BodyKind::Sha1 });
        }
    }
    jobs
}

fn upload_one(
    client: &reqwest::blocking::Client,
    creds: &Credentials,
    job: &UploadJob,
) -> Result<()> {
    let body: Vec<u8> = match job.body_kind {
        BodyKind::File => std::fs::read(&job.source)
            .with_context(|| format!("failed to read {}", job.source.display()))?,
        BodyKind::Sha1 => digest_hex_sha1(&job.source)?.into_bytes(),
        BodyKind::Sha256 => digest_hex_sha256(&job.source)?.into_bytes(),
        BodyKind::Sha512 => digest_hex_sha512(&job.source)?.into_bytes(),
    };
    let response = client
        .put(&job.url)
        .basic_auth(&creds.username, Some(&creds.password))
        .body(body)
        .send()
        .with_context(|| format!("HTTP PUT failed for {}", job.url))?;
    if !response.status().is_success() {
        bail!("HTTP {} from PUT {}", response.status(), job.url);
    }
    Ok(())
}

fn digest_hex_sha1(path: &Path) -> Result<String> {
    use sha1::Digest as _;
    let bytes = std::fs::read(path)?;
    let mut h = sha1::Sha1::new();
    h.update(&bytes);
    Ok(hex_encode(&h.finalize()))
}
fn digest_hex_sha256(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let bytes = std::fs::read(path)?;
    let mut h = sha2::Sha256::new();
    h.update(&bytes);
    Ok(hex_encode(&h.finalize()))
}
fn digest_hex_sha512(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let bytes = std::fs::read(path)?;
    let mut h = sha2::Sha512::new();
    h.update(&bytes);
    Ok(hex_encode(&h.finalize()))
}
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

/// Re-resolve the declared deps to fill in BOM-managed versions.  Returns
/// one `Gav` per entry in `desc.dependencies`, in declaration order.
fn resolve_declared_dep_gavs(desc: &Descriptor) -> Result<Vec<Gav>> {
    if desc.dependencies.is_empty() {
        return Ok(vec![]);
    }
    let entries: Vec<DepEntry> = desc
        .dependencies
        .iter()
        .map(|(k, v)| DepEntry { key: k, version: v.version(), repo_id: v.repository() })
        .collect();
    let opts = ResolveOptions {
        default_repos: crate::build::central_repos(),
        named_repos: crate::build::extra_repos(desc),
        progress: false,
        bom_imports: desc.prod_bom_gavs()?,
        offline: false,
    };
    curie_deps::resolve_declared_gavs(&entries, &opts)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_desc(group_id: Option<&str>, pub_cfg: crate::descriptor::PublishConfig) -> Descriptor {
        use crate::descriptor::*;
        use std::collections::BTreeMap;
        Descriptor {
            kind: DescriptorKind::Library(Library {
                name: "my-lib".into(),
                version: "1.0.0".into(),
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
            dependencies: BTreeMap::new(),
            test_dependencies: BTreeMap::new(),
            repositories: vec![],
            bom_imports: BTreeMap::new(),
            test_bom_imports: BTreeMap::new(),
            inherited_bom_imports: BTreeMap::new(),
            inherited_test_bom_imports: BTreeMap::new(),
            workspace_dependencies: BTreeMap::new(),
            annotation_processors: BTreeMap::new(),
            test_annotation_processors: BTreeMap::new(),
            inherited_annotation_processors: BTreeMap::new(),
            inherited_test_annotation_processors: BTreeMap::new(),
            annotation_processor_options: BTreeMap::new(),
            test_annotation_processor_options: BTreeMap::new(),
            inherited_annotation_processor_options: BTreeMap::new(),
            inherited_test_annotation_processor_options: BTreeMap::new(),
            publish: pub_cfg,
        }
    }

    fn full_publish_cfg() -> crate::descriptor::PublishConfig {
        use crate::descriptor::*;
        PublishConfig {
            repository: Some("nx".into()),
            description: Some("A test lib".into()),
            homepage: Some("https://example.com".into()),
            licenses: vec!["Apache-2.0".into()],
            developers: vec![Developer {
                id: Some("alice".into()),
                name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
            }],
            scm: Some(Scm {
                url: Some("https://github.com/x/y".into()),
                connection: Some("scm:git:git@github.com:x/y.git".into()),
                developer_connection: None,
            }),
            ..PublishConfig::default()
        }
    }

    #[test]
    fn validate_errors_when_publish_metadata_missing() {
        let desc = fake_desc(Some("com.example"), crate::descriptor::PublishConfig::default());
        let err = validate_for_publish(&desc).unwrap_err().to_string();
        assert!(err.contains("description"), "got: {err}");
        assert!(err.contains("homepage"), "got: {err}");
        assert!(err.contains("licenses"), "got: {err}");
        assert!(err.contains("developers"), "got: {err}");
        assert!(err.contains("scm"), "got: {err}");
    }

    #[test]
    fn validate_passes_when_full_metadata_present() {
        let desc = fake_desc(Some("com.example"), full_publish_cfg());
        validate_for_publish(&desc).unwrap();
    }

    #[test]
    fn resolve_target_errors_when_no_repo_configured() {
        let desc = fake_desc(Some("com.example"), {
            let mut p = full_publish_cfg();
            p.repository = None;
            p.url = None;
            p
        });
        let err = resolve_target_url(&desc, None).unwrap_err().to_string();
        assert!(err.contains("no publish target"), "got: {err}");
    }

    #[test]
    fn resolve_target_repo_id_must_match_repositories() {
        let desc = fake_desc(Some("com.example"), full_publish_cfg()); // repository = "nx"
        let err = resolve_target_url(&desc, None).unwrap_err().to_string();
        assert!(err.contains("\"nx\""), "got: {err}");
        assert!(err.contains("[[repositories]]"), "got: {err}");
    }

    #[test]
    fn resolve_target_repo_id_returns_repo_url() {
        let mut desc = fake_desc(Some("com.example"), full_publish_cfg()); // repository = "nx"
        desc.repositories.push(crate::descriptor::RepositoryEntry {
            id: "nx".into(),
            name: None,
            url: "https://nexus.example.com/repo/releases".into(),
        });
        let url = resolve_target_url(&desc, None).unwrap();
        assert_eq!(url, "https://nexus.example.com/repo/releases");
    }

    #[test]
    fn resolve_target_cli_override_wins() {
        let desc = fake_desc(Some("com.example"), full_publish_cfg());
        let url = resolve_target_url(&desc, Some("https://override.example.com/r")).unwrap();
        assert_eq!(url, "https://override.example.com/r");
    }

    #[test]
    fn resolve_credentials_errors_when_no_creds_for_repo_id() {
        let mut desc = fake_desc(Some("com.example"), full_publish_cfg()); // repository = "nx"
        desc.repositories.push(crate::descriptor::RepositoryEntry {
            id: "nx".into(),
            name: None,
            url: "https://nexus.example.com/repo".into(),
        });
        let err = resolve_credentials(&desc, &config::CurieConfig::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"nx\""), "got: {err}");
        assert!(err.contains("[[credentials]]"), "got: {err}");
    }

    #[test]
    fn build_upload_plan_emits_all_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("my-lib-1.0.0.jar");
        std::fs::write(&jar, b"").unwrap();
        let pom = dir.path().join("my-lib-1.0.0.pom");
        std::fs::write(&pom, b"").unwrap();
        let artifacts = vec![
            UploadArtifact::new(&jar, ""),
            UploadArtifact::pom(&pom),
        ];
        let plan = build_upload_plan(&artifacts, "https://nexus.example.com/repo/com/example/my-lib/1.0.0");
        // Each artifact: file + 3 sidecars (sha1, sha256, sha512) = 4.
        // jar = 4 + pom = 4 → total 8.
        assert_eq!(plan.len(), 8);
        let urls: Vec<&str> = plan.iter().map(|j| j.url.as_str()).collect();
        assert!(urls.contains(&"https://nexus.example.com/repo/com/example/my-lib/1.0.0/my-lib-1.0.0.jar"));
        assert!(urls.contains(&"https://nexus.example.com/repo/com/example/my-lib/1.0.0/my-lib-1.0.0.jar.sha256"));
        assert!(urls.contains(&"https://nexus.example.com/repo/com/example/my-lib/1.0.0/my-lib-1.0.0.pom"));
        assert!(urls.contains(&"https://nexus.example.com/repo/com/example/my-lib/1.0.0/my-lib-1.0.0.pom.sha512"));
    }
}
