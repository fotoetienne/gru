use crate::github::gh_cli_command;
use tokio::sync::oneshot;

/// The repository to check for new releases.
const RELEASE_REPO: &str = "fotoetienne/gru";

/// Current version from Cargo.toml.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// Prints a version notification if a newer version is available.
/// Non-blocking: awaits the result from a previously spawned check.
/// Silently does nothing if the check failed or no update is available.
pub async fn print_if_newer(rx: oneshot::Receiver<Option<String>>) {
    if let Ok(Some(msg)) = rx.await {
        eprintln!("{}", msg);
    }
}

/// Checks GitHub releases for a newer version.
/// Returns `Some(message)` if an upgrade is available, `None` otherwise.
/// Returns `None` on any error (offline, API failure, parse failure).
async fn check_latest_version() -> Option<String> {
    let output = gh_cli_command("github.com")
        .args([
            "release",
            "list",
            "--repo",
            RELEASE_REPO,
            "--limit",
            "1",
            "--json",
            "tagName,isLatest",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let releases: Vec<ReleaseInfo> = serde_json::from_str(&stdout).ok()?;
    let latest = releases.first()?;

    let latest_version = latest
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&latest.tag_name);

    if is_newer(latest_version, CURRENT_VERSION) {
        Some(format!(
            "gru v{} available (current: v{}) — run \"gru upgrade\" to update",
            latest_version, CURRENT_VERSION
        ))
    } else {
        None
    }
}

/// Minimal release info from `gh release list --json`.
#[derive(serde::Deserialize)]
struct ReleaseInfo {
    #[serde(rename = "tagName")]
    tag_name: String,
}

/// Compares two semver-like version strings (major.minor.patch).
/// Returns true if `latest` is strictly newer than `current`.
/// Falls back to false on parse errors.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
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
}
