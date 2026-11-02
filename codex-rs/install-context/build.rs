use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_RELEASE_REPOSITORY");
    let repository =
        env::var("CODEX_RELEASE_REPOSITORY").unwrap_or_else(|_| "openai/codex".to_owned());
    let components: Vec<_> = repository.split('/').collect();
    assert!(
        components.len() == 2
            && components.iter().all(|component| {
                !component.is_empty()
                    && !matches!(*component, "." | "..")
                    && component.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "CODEX_RELEASE_REPOSITORY must be an owner/repository pair"
    );
    let downstream = repository != "openai/codex";
    let releases = format!("https://github.com/{repository}/releases/latest");
    let installer = if downstream {
        format!("{releases}/download/install.sh")
    } else {
        "https://chatgpt.com/codex/install.sh".to_owned()
    };
    let windows_installer = if downstream {
        format!("{releases}/download/install.ps1")
    } else {
        "https://chatgpt.com/codex/install.ps1".to_owned()
    };
    for (name, value) in [
        ("CODEX_RELEASE_REPOSITORY", repository.clone()),
        (
            "CODEX_RELEASE_API_URL",
            format!("https://api.github.com/repos/{repository}/releases/latest"),
        ),
        ("CODEX_RELEASE_NOTES_URL", releases),
        ("CODEX_INSTALLER_URL", installer.clone()),
        ("CODEX_WINDOWS_INSTALLER_URL", windows_installer.clone()),
        (
            "CODEX_INSTALLER_COMMAND",
            format!(
                "codex_installer=$(curl -fsSL {installer}) && CODEX_NON_INTERACTIVE=1 sh -c \"$codex_installer\""
            ),
        ),
        (
            "CODEX_WINDOWS_INSTALLER_COMMAND",
            format!("$env:CODEX_NON_INTERACTIVE=1; irm {windows_installer} | iex"),
        ),
        (
            "CODEX_VERSION_CACHE",
            if downstream {
                format!("version-{}.json", repository.replace('/', "-"))
            } else {
                "version.json".to_owned()
            },
        ),
    ] {
        println!("cargo:rustc-env={name}={value}");
    }
}
