//! `curie setup` — install shell completions for the current user.
//!
//! Detects the running shell from `$SHELL`, constructs a URL pointing at the
//! exact completion file that matches this binary's Git commit, downloads it,
//! and writes it to the conventional per-shell completions directory.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// Git commit hash baked in at compile time by `build.rs`.
const GIT_COMMIT_HASH: &str = env!("GIT_COMMIT_HASH");

const GITHUB_RAW_BASE: &str = "https://raw.githubusercontent.com/atteo/curie-build";

// ── Shell ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shell {
    Fish,
    Bash,
    Zsh,
}

impl Shell {
    /// File name of the completion script inside `completions/` in the repo.
    fn source_filename(self) -> &'static str {
        match self {
            Shell::Fish => "curie.fish",
            Shell::Bash => "curie.bash",
            Shell::Zsh => "curie.zsh",
        }
    }

    /// File name to use when installing (zsh uses the `_<cmd>` convention).
    fn dest_filename(self) -> &'static str {
        match self {
            Shell::Fish => "curie.fish",
            Shell::Bash => "curie",
            Shell::Zsh => "_curie",
        }
    }

    /// Conventional per-user completions directory for this shell.
    fn dest_dir(self) -> Result<PathBuf> {
        match self {
            Shell::Fish => {
                let config = dirs::config_dir().context("cannot determine XDG config directory")?;
                Ok(config.join("fish/completions"))
            }
            Shell::Bash => {
                let data =
                    dirs::data_local_dir().context("cannot determine XDG local data directory")?;
                Ok(data.join("bash-completion/completions"))
            }
            Shell::Zsh => {
                let home = dirs::home_dir().context("cannot determine home directory")?;
                Ok(home.join(".zsh/completions"))
            }
        }
    }

    /// Full destination path (directory + filename).
    fn dest_path(self) -> Result<PathBuf> {
        Ok(self.dest_dir()?.join(self.dest_filename()))
    }
}

impl std::fmt::Display for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Shell::Fish => write!(f, "fish"),
            Shell::Bash => write!(f, "bash"),
            Shell::Zsh => write!(f, "zsh"),
        }
    }
}

// ── Shell detection ───────────────────────────────────────────────────────────

/// Parse a shell name from an explicit string (e.g. from `--shell`).
pub fn shell_from_name(name: &str) -> Result<Shell> {
    match name {
        "fish" => Ok(Shell::Fish),
        "bash" => Ok(Shell::Bash),
        "zsh" => Ok(Shell::Zsh),
        other => bail!(
            "unsupported shell {:?}; supported shells are: fish, bash, zsh",
            other
        ),
    }
}

/// Detect the current user's shell from `$SHELL`.
pub fn detect_shell() -> Result<Shell> {
    let shell_path = std::env::var("SHELL")
        .context("$SHELL is not set; use --shell <fish|bash|zsh> to specify")?;
    let shell_name = shell_name_from_path(&shell_path);
    shell_from_name(shell_name).with_context(|| {
        format!("$SHELL is {shell_path:?}; use --shell <fish|bash|zsh> to override")
    })
}

/// Extract the bare shell name (e.g. `"fish"`) from an absolute path.
fn shell_name_from_path(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

// ── Download ──────────────────────────────────────────────────────────────────

fn completion_url(shell: Shell) -> String {
    format!(
        "{}/{}/completions/{}",
        GITHUB_RAW_BASE,
        GIT_COMMIT_HASH,
        shell.source_filename(),
    )
}

fn download_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("curie/setup")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to connect to {url}"))?;

    if !response.status().is_success() {
        bail!("server returned {} for {url}", response.status());
    }

    response.text().context("failed to read response body")
}

// ── Install ───────────────────────────────────────────────────────────────────

fn write_completion(content: &str, dest: &std::path::Path) -> Result<()> {
    let dir = dest.parent().expect("dest path has no parent");
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::write(dest, content).with_context(|| format!("failed to write {}", dest.display()))
}

fn post_install_hint(shell: Shell, dest: &std::path::Path) {
    match shell {
        Shell::Fish | Shell::Bash => {
            println!("  Open a new terminal, or run: source {}", dest.display());
        }
        Shell::Zsh => {
            println!("  Add to ~/.zshrc if not already present:");
            println!(
                "    fpath=({} $fpath)",
                dest.parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            println!("    autoload -Uz compinit && compinit");
            println!("  Then open a new terminal.");
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run `curie setup [--shell <shell>]`.
pub fn run_setup(shell_override: Option<String>) -> Result<()> {
    let shell = match shell_override {
        Some(ref name) => shell_from_name(name)?,
        None => detect_shell()?,
    };

    let url = completion_url(shell);
    let dest = shell.dest_path()?;

    println!("  Shell           {shell}");
    println!("  Downloading     {url}");

    let content = download_text(&url)?;
    write_completion(&content, &dest)?;

    println!("  Installed       {}", dest.display());
    post_install_hint(shell, &dest);

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_name_from_absolute_path() {
        assert_eq!(shell_name_from_path("/usr/bin/fish"), "fish");
        assert_eq!(shell_name_from_path("/bin/bash"), "bash");
        assert_eq!(shell_name_from_path("/usr/local/bin/zsh"), "zsh");
    }

    #[test]
    fn shell_from_name_roundtrip() {
        assert_eq!(shell_from_name("fish").unwrap(), Shell::Fish);
        assert_eq!(shell_from_name("bash").unwrap(), Shell::Bash);
        assert_eq!(shell_from_name("zsh").unwrap(), Shell::Zsh);
        assert!(shell_from_name("tcsh").is_err());
        assert!(shell_from_name("").is_err());
    }

    #[test]
    fn completion_url_contains_commit_and_filename() {
        let url = completion_url(Shell::Fish);
        assert!(
            url.contains(GIT_COMMIT_HASH),
            "URL should contain commit hash"
        );
        assert!(
            url.ends_with("curie.fish"),
            "fish URL should end with curie.fish"
        );

        let url = completion_url(Shell::Bash);
        assert!(url.ends_with("curie.bash"));

        let url = completion_url(Shell::Zsh);
        assert!(url.ends_with("curie.zsh"));
    }

    #[test]
    fn dest_filenames_follow_conventions() {
        assert_eq!(Shell::Fish.dest_filename(), "curie.fish");
        assert_eq!(Shell::Bash.dest_filename(), "curie");
        assert_eq!(Shell::Zsh.dest_filename(), "_curie");
    }

    #[test]
    fn dest_path_includes_filename() {
        // Just verify the path ends with the expected filename — don't assume
        // a specific home/config directory since it varies by test environment.
        let fish_dest = Shell::Fish.dest_path().unwrap();
        assert_eq!(fish_dest.file_name().unwrap(), "curie.fish");

        let bash_dest = Shell::Bash.dest_path().unwrap();
        assert_eq!(bash_dest.file_name().unwrap(), "curie");

        let zsh_dest = Shell::Zsh.dest_path().unwrap();
        assert_eq!(zsh_dest.file_name().unwrap(), "_curie");
    }
}
