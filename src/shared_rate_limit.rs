//! Cross-Minion rate limit coordination.
//!
//! When one Minion hits a GitHub rate limit and learns the reset timestamp,
//! it writes that epoch to `~/.gru/state/rate_limit_until_{host}.txt`. Other
//! Minions check this file before making `gh` API calls and skip the call
//! (sleeping until the reset window) instead of burning independent failed
//! attempts.
//!
//! Per-host files allow `github.com` and GHES to track rate limits
//! independently. Writes are atomic (temp-file + rename). Reads may observe
//! stale state; the worst case is one extra failed API call before the local
//! Minion also writes the shared state — by design, no locking is used.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::workspace::Workspace;

/// Small jitter added after the reset timestamp to avoid thundering herd
/// when many Minions resume simultaneously.
const JITTER_SECS: u64 = 5;

/// Files older than this (measured against the stored reset epoch) are
/// considered stale and removed on read. GitHub rate-limit windows are
/// at most ~60 minutes; anything claiming 2h in the past is corruption.
const STALE_THRESHOLD_SECS: u64 = 2 * 60 * 60;

/// Sanitize a hostname into a filesystem-safe component by replacing any
/// character that isn't an ASCII alphanumeric with `_`.
fn sanitize_host(host: &str) -> String {
    host.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Resolve the shared rate-limit file path for the given host.
/// Returns `None` if the workspace cannot be initialized.
fn rate_limit_path(host: &str) -> Option<PathBuf> {
    let ws = Workspace::global().ok()?;
    let filename = format!("rate_limit_until_{}.txt", sanitize_host(host));
    Some(ws.state().join(filename))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Read the stored reset epoch for `host`, cleaning up stale state.
///
/// Returns `Some(epoch)` only when the file exists, parses, and points to a
/// future time within the sanity window. Returns `None` (and removes the
/// file) when the stored value is expired, stale (>2h in the past), or
/// implausibly far in the future (>2h ahead).
pub(crate) fn read_rate_limit_until(host: &str) -> Option<u64> {
    let path = rate_limit_path(host)?;
    let contents = fs::read_to_string(&path).ok()?;
    let reset: u64 = contents.trim().parse().ok().or_else(|| {
        // Unparseable — drop it so callers aren't stuck on corruption.
        let _ = fs::remove_file(&path);
        None
    })?;

    let now = now_secs();

    if reset <= now {
        // Window has passed — clean up and proceed normally.
        let _ = fs::remove_file(&path);
        return None;
    }

    // Sanity check: rate-limit windows are ≤ 1h. Anything claiming >2h in
    // the future is stale/corrupt and should be ignored.
    if reset.saturating_sub(now) > STALE_THRESHOLD_SECS {
        log::warn!(
            "Shared rate-limit file for {} points >2h in the future ({}s); \
             treating as stale and removing.",
            host,
            reset.saturating_sub(now),
        );
        let _ = fs::remove_file(&path);
        return None;
    }

    Some(reset)
}

/// Atomically write the reset epoch for `host` using temp-file + rename.
///
/// Errors are logged and swallowed: shared coordination is best-effort, and
/// a failed write just means this Minion won't signal its peers.
pub(crate) fn write_rate_limit_until(host: &str, reset_epoch: u64) {
    let Some(path) = rate_limit_path(host) else {
        log::debug!("Cannot resolve shared rate-limit path for {}", host);
        return;
    };

    let temp_path = path.with_extension("txt.tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)?;
        file.write_all(reset_epoch.to_string().as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, &path)?;
        Ok(())
    })();

    if let Err(e) = write_result {
        log::warn!("Failed to write shared rate-limit file for {}: {}", host, e);
        let _ = fs::remove_file(&temp_path);
    }
}

/// Remove the shared rate-limit file for `host`, if present.
///
/// Called after a successful API call to clear the gate for other Minions.
/// Missing files are not an error.
pub(crate) fn clear_rate_limit(host: &str) {
    let Some(path) = rate_limit_path(host) else {
        return;
    };
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::debug!("Failed to clear shared rate-limit file for {}: {}", host, e),
    }
}

/// If another Minion has signalled a rate limit on `host`, sleep until the
/// reset window passes. Intended to be called at the top of each attempt in
/// `gh_api_with_retry` before spawning `gh`.
pub(crate) async fn wait_if_shared_rate_limited(host: &str) {
    let Some(reset_epoch) = read_rate_limit_until(host) else {
        return;
    };
    let now = now_secs();
    // read_rate_limit_until already filtered past/stale values; reset > now.
    let wait_secs = reset_epoch.saturating_sub(now).saturating_add(JITTER_SECS);
    log::warn!(
        "Shared rate limit active for {} (signalled by another Minion). \
         Sleeping {}s until reset.",
        host,
        wait_secs,
    );
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
    // Window elapsed — remove the file so the next caller doesn't re-read it.
    clear_rate_limit(host);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::set_test_workspace;

    #[test]
    fn sanitize_host_replaces_dots_and_special_chars() {
        assert_eq!(sanitize_host("github.com"), "github_com");
        assert_eq!(sanitize_host("ghe.netflix.com"), "ghe_netflix_com");
        assert_eq!(sanitize_host("host:8080"), "host_8080");
        assert_eq!(sanitize_host("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_host("plain"), "plain");
    }

    #[test]
    fn read_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();
        assert!(read_rate_limit_until("github.com").is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        let future_epoch = now_secs() + 300;
        write_rate_limit_until("github.com", future_epoch);

        // File should land at the per-host path and contain exactly the epoch.
        let path = rate_limit_path("github.com").unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.trim(), future_epoch.to_string());

        assert_eq!(read_rate_limit_until("github.com"), Some(future_epoch));
    }

    #[test]
    fn read_past_epoch_is_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        let past_epoch = now_secs() - 10;
        write_rate_limit_until("github.com", past_epoch);

        assert!(read_rate_limit_until("github.com").is_none());
        // File should be removed as a side effect.
        let path = rate_limit_path("github.com").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn read_far_future_epoch_is_stale_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        // 3h in the future — outside the 2h sanity window.
        let bogus_epoch = now_secs() + 3 * 60 * 60;
        write_rate_limit_until("github.com", bogus_epoch);

        assert!(read_rate_limit_until("github.com").is_none());
        let path = rate_limit_path("github.com").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn read_unparseable_contents_is_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        let path = rate_limit_path("github.com").unwrap();
        fs::write(&path, "not-a-number").unwrap();

        assert!(read_rate_limit_until("github.com").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn clear_rate_limit_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        // Missing file is not an error.
        clear_rate_limit("github.com");

        let future_epoch = now_secs() + 300;
        write_rate_limit_until("github.com", future_epoch);
        let path = rate_limit_path("github.com").unwrap();
        assert!(path.exists());

        clear_rate_limit("github.com");
        assert!(!path.exists());

        // Second clear on missing file still works.
        clear_rate_limit("github.com");
    }

    #[test]
    fn per_host_files_are_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        let reset_a = now_secs() + 200;
        let reset_b = now_secs() + 400;
        write_rate_limit_until("github.com", reset_a);
        write_rate_limit_until("ghe.netflix.com", reset_b);

        assert_eq!(read_rate_limit_until("github.com"), Some(reset_a));
        assert_eq!(read_rate_limit_until("ghe.netflix.com"), Some(reset_b));

        // Clearing one does not affect the other.
        clear_rate_limit("github.com");
        assert!(read_rate_limit_until("github.com").is_none());
        assert_eq!(read_rate_limit_until("ghe.netflix.com"), Some(reset_b));
    }

    #[test]
    fn write_is_atomic_no_temp_file_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = set_test_workspace(tmp.path().to_path_buf()).unwrap();

        let future_epoch = now_secs() + 300;
        write_rate_limit_until("github.com", future_epoch);

        let path = rate_limit_path("github.com").unwrap();
        let temp_path = path.with_extension("txt.tmp");
        assert!(path.exists());
        assert!(!temp_path.exists());
    }
}
