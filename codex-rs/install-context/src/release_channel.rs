//! The release channel is compiled into the package, shared by client and daemon.
//! Runtime environment changes cannot redirect a previously installed updater.

use semver::Version;

pub const REPOSITORY: &str = env!("CODEX_RELEASE_REPOSITORY");
pub const LATEST_RELEASE_URL: &str = env!("CODEX_RELEASE_API_URL");
pub const RELEASE_NOTES_URL: &str = env!("CODEX_RELEASE_NOTES_URL");
pub const INSTALLER_URL: &str = env!("CODEX_INSTALLER_URL");
pub const WINDOWS_INSTALLER_URL: &str = env!("CODEX_WINDOWS_INSTALLER_URL");
pub const INSTALLER_COMMAND: &str = env!("CODEX_INSTALLER_COMMAND");
pub const WINDOWS_INSTALLER_COMMAND: &str = env!("CODEX_WINDOWS_INSTALLER_COMMAND");
pub const VERSION_CACHE_FILENAME: &str = env!("CODEX_VERSION_CACHE");

pub fn is_downstream() -> bool {
    REPOSITORY != "openai/codex"
}

/// Build metadata has no SemVer precedence. Our channel explicitly orders its
/// `+patched.N` revisions after comparing the upstream major/minor/patch tuple.
pub fn is_newer(latest: &str, current: &str) -> Option<bool> {
    is_newer_for_channel(latest, current, is_downstream())
}

fn is_newer_for_channel(latest: &str, current: &str, downstream: bool) -> Option<bool> {
    Some(version_key(latest, downstream)? > version_key(current, downstream)?)
}

pub fn is_stable_version(version: &str) -> bool {
    version_key(version, is_downstream())
        .is_some_and(|(major, minor, patch, _)| (major, minor, patch) != (0, 0, 0))
}

fn version_key(version: &str, downstream: bool) -> Option<(u64, u64, u64, u64)> {
    let version = Version::parse(version.trim()).ok()?;
    if !version.pre.is_empty() {
        return None;
    }
    let revision = if version.build.is_empty() {
        0
    } else if downstream {
        let revision = version.build.as_str().strip_prefix("patched.")?;
        let value = revision.parse::<u64>().ok()?;
        if value == 0 || value.to_string() != revision {
            return None;
        }
        value
    } else {
        return None;
    };
    Some((version.major, version.minor, version.patch, revision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_revisions_are_numeric_and_follow_upstream_precedence() {
        for (latest, current, expected) in [
            ("0.166.0+patched.10", "0.166.0+patched.9", true),
            ("0.166.0+patched.1", "0.166.0", true),
            ("0.167.0+patched.1", "0.166.0+patched.100", true),
            ("0.166.0+patched.9", "0.166.0+patched.10", false),
            ("0.166.0+patched.10", "0.166.0+patched.10", false),
        ] {
            assert_eq!(is_newer_for_channel(latest, current, true), Some(expected));
        }
    }

    #[test]
    fn other_metadata_and_prereleases_are_never_auto_updated() {
        for version in [
            "0.166.0-alpha.1",
            "0.166.0+dev",
            "0.166.0+patched.0",
            "0.166.0+patched.01",
            "0.166.0+patched.1.2",
            "0.166.0+patched.18446744073709551616",
        ] {
            assert_eq!(version_key(version, true), None, "{version}");
        }
        assert_eq!(version_key("0.166.0+patched.1", false), None);
        assert_eq!(
            is_newer_for_channel("0.167.0", "0.166.0", false),
            Some(true)
        );
    }

    #[test]
    fn channel_endpoints_and_cache_have_the_same_owner() {
        if is_downstream() {
            assert_eq!(
                LATEST_RELEASE_URL,
                format!("{RELEASE_NOTES_URL}/download/release.json")
            );
            assert_eq!(
                INSTALLER_URL,
                format!("{RELEASE_NOTES_URL}/download/install.sh")
            );
            assert_ne!(VERSION_CACHE_FILENAME, "version.json");
        } else {
            assert_eq!(
                LATEST_RELEASE_URL,
                format!("https://api.github.com/repos/{REPOSITORY}/releases/latest")
            );
            assert_eq!(INSTALLER_URL, "https://chatgpt.com/codex/install.sh");
            assert_eq!(VERSION_CACHE_FILENAME, "version.json");
        }
    }
}
