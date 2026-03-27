use crate::github::gh_cli_command;
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};

/// The repository to check for new releases (must match github.com host below).
const RELEASE_REPO: &str = "fotoetienne/gru";

/// Current version from Cargo.toml.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum time to wait for the GitHub API before giving up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawns a non-blocking version check that runs in the background.
/// Returns a receiver that will eventually contain the upgrade message (if any).
/// The check is fire-and-forget: callers can poll the receiver when convenient.
pub fn spawn_version_check() -> oneshot::Receiver<Option<String>> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = check_latest_version().await;
        // Ignore send error — receiver may have been dropped if the caller moved on.
        let _ = tx.send(result);
    });
    rx
}

/// Prints a version notification if one is already available, without waiting.
/// Uses `try_recv` so the caller is never blocked — if the background check
/// hasn't completed yet, the notification is silently skipped.
/// Output goes to stderr so it doesn't interfere with stdout parsing/scripting.
pub fn print_if_ready(rx: &mut oneshot::Receiver<Option<String>>) {
    if let Ok(Some(msg)) = rx.try_recv() {
        eprintln!("{}", msg);
    }
}

/// Checks GitHub releases for a newer version.
/// Returns `Some(message)` if an upgrade is available, `None` otherwise.
/// Returns `None` on any error (offline, API failure, parse failure, timeout).
async fn check_latest_version() -> Option<String> {
    let output = timeout(
        CHECK_TIMEOUT,
        gh_cli_command("github.com")
            .args([
                "release",
                "view",
                "--repo",
                RELEASE_REPO,
                "--json",
                "tagName",
            ])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let release: ReleaseInfo = serde_json::from_str(&stdout).ok()?;

    let latest_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if is_newer(latest_version, CURRENT_VERSION) {
        Some(format!(
            "gru v{} available (current: v{}) — see GitHub releases to update",
            latest_version, CURRENT_VERSION
        ))
    } else {
        None
    }
}

/// Minimal release info from `gh release view --json`.
#[derive(serde::Deserialize)]
struct ReleaseInfo {
    #[serde(rename = "tagName")]
    tag_name: String,
}

/// Compares two semver-like version strings (major.minor.patch).
/// Returns true if `latest` is strictly newer than `current`.
/// Strips pre-release suffixes (e.g., `1.0.0-rc.1` → `1.0.0`) before comparing.
/// Falls back to false on parse errors.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        // Strip pre-release suffix (e.g., "1.0.0-rc.1" → "1.0.0")
        let v = v.split('-').next().unwrap_or(v);
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    };

    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_patch_bump() {
        assert!(is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn test_is_newer_minor_bump() {
        assert!(is_newer("0.2.0", "0.1.0"));
    }

    #[test]
    fn test_is_newer_major_bump() {
        assert!(is_newer("1.0.0", "0.1.0"));
    }

    #[test]
    fn test_is_newer_same_version() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn test_is_newer_older_version() {
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn test_is_newer_invalid_format() {
        assert!(!is_newer("invalid", "0.1.0"));
        assert!(!is_newer("0.1.0", "invalid"));
    }

    #[test]
    fn test_is_newer_two_part_version() {
        assert!(!is_newer("0.1", "0.1.0"));
    }

    #[test]
    fn test_is_newer_prerelease_stripped() {
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
    }

    #[test]
    fn test_is_newer_same_base_with_prerelease() {
        // 0.1.0-rc.1 is not newer than 0.1.0 (same base version)
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
    }

    #[test]
    fn test_is_newer_current_is_prerelease() {
        // Same base version after stripping pre-release suffix — not treated as newer.
        // Conscious trade-off: a pre-release user won't be nagged to "upgrade" to the
        // same base version; they'll get notified on the next actual bump.
        assert!(!is_newer("0.2.0", "0.2.0-beta.1"));
        // But a higher stable version is newer
        assert!(is_newer("0.3.0", "0.2.0-beta.1"));
    }
}
