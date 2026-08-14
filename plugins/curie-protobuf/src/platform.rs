pub fn maven_classifier() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "osx-x86_64",
        ("macos", "aarch64") => "osx-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        (os, arch) => anyhow::bail!("unsupported platform: {os}/{arch}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_classifier_is_nonempty() {
        let c = maven_classifier().expect("current platform should be recognized");
        assert!(!c.is_empty());
    }
}
