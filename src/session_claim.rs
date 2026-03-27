use crate::minion_registry::{with_registry, MinionInfo, MinionMode, MinionRegistry};
use anyhow::Result;
use chrono::Utc;

/// Typed errors from session-claim operations (shared by attach and resume).
#[derive(Debug)]
pub(crate) enum SessionClaimError {
    /// The minion has a live process — user must stop it first.
    AlreadyRunning { minion_id: String, mode: MinionMode },
}

impl std::fmt::Display for SessionClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionClaimError::AlreadyRunning { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                    minion_id, mode, minion_id
                )
            }
        }
    }
}

impl std::error::Error for SessionClaimError {}

/// Core claim logic shared by the production function and the test-only variant.
///
/// Checks if a minion is available in the given registry and claims it with
/// `target_mode`. Returns a snapshot of the `MinionInfo` before claiming.
fn claim_session_in_registry(
    reg: &mut MinionRegistry,
    minion_id: &str,
    target_mode: MinionMode,
) -> Result<Option<MinionInfo>> {
    let id = minion_id.to_string();
    let info = match reg.get(&id) {
        Some(info) => info.clone(),
        None => return Ok(None),
    };

    if info.mode != MinionMode::Stopped {
        match info.pid {
            Some(_pid_val) => {
                // On Unix, verify the process is actually alive (with PID reuse detection).
                // On non-Unix, is_process_alive always returns false, so be
                // conservative and treat any recorded PID as alive.
                let is_running = cfg!(not(unix)) || info.is_running();
                if is_running {
                    // Do not wrap with .context() — the caller uses
                    // downcast_ref to distinguish this from IO errors.
                    return Err(SessionClaimError::AlreadyRunning {
                        minion_id: id,
                        mode: info.mode.clone(),
                    }
                    .into());
                }
                // Stale entry: process is dead but registry still thinks
                // it's running. Reset to Stopped before claiming.
                reg.update(&id, |i| {
                    i.mode = MinionMode::Stopped;
                    i.clear_pid();
                    i.last_activity = Utc::now();
                })?;
            }
            None => {
                // Stale entry: mode is non-Stopped but no PID was recorded
                // (e.g., process crashed before PID could be saved).
                // Treat the same as a dead-PID entry: reset to Stopped.
                log::warn!(
                    "Minion {} has mode {} but no PID recorded — resetting stale entry to Stopped",
                    id,
                    info.mode
                );
                reg.update(&id, |i| {
                    i.mode = MinionMode::Stopped;
                    i.clear_pid();
                    i.last_activity = Utc::now();
                })?;
            }
        }
    }

    // Claim the session with the requested mode.
    // Clear archived_at so a resumed/attached minion is visible in `gru status`.
    reg.update(&id, |i| {
        i.mode = target_mode.clone();
        i.last_activity = Utc::now();
        i.archived_at = None;
    })?;

    Ok(Some(info))
}

/// Applies graceful-degradation logic to a claim result.
///
/// `SessionClaimError` variants always propagate. Other errors (IO, lock
/// contention) are swallowed when `graceful` is true, returning `Ok(None)`.
fn handle_claim_result(
    result: Result<Option<MinionInfo>>,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    match result {
        Ok(info) => Ok(info),
        Err(e) => {
            if e.downcast_ref::<SessionClaimError>().is_some() {
                Err(e)
            } else if graceful {
                // Registry unavailable — proceed without it
                log::debug!("Could not check registry: {}", e);
                Ok(None)
            } else {
                Err(e.context("Failed to access minion registry"))
            }
        }
    }
}

/// Atomically checks if a minion is available and claims it with the given mode.
///
/// This combines the mode check and the mode update in a single `with_registry`
/// call, which holds an exclusive file lock for the duration. This prevents
/// TOCTOU races between concurrent attach/resume calls.
///
/// Returns a clone of the [`MinionInfo`] (as it was before claiming) if the
/// minion is found in the registry. Callers can extract whatever fields they
/// need from this snapshot.
///
/// # Errors
///
/// - [`SessionClaimError::AlreadyRunning`] if the minion has a live process.
///
/// # Graceful degradation
///
/// If `graceful` is true, transient registry failures (lock contention, IO
/// errors) are swallowed and the function returns `Ok(None)`. When false, all
/// registry errors propagate.
pub(crate) async fn check_and_claim_session(
    minion_id: &str,
    target_mode: MinionMode,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| claim_session_in_registry(reg, &id, target_mode)).await;

    handle_claim_result(result, graceful)
}

/// Test-only variant that operates on a registry in the given `state_dir`
/// instead of the global workspace. This avoids the thread-local issue with
/// `with_registry` + `spawn_blocking`.
#[cfg(test)]
pub(crate) async fn check_and_claim_session_with_dir(
    state_dir: &std::path::Path,
    minion_id: &str,
    target_mode: MinionMode,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    use anyhow::Context as _;
    let id = minion_id.to_string();
    let dir = state_dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        let mut reg = MinionRegistry::load(Some(&dir))?;
        claim_session_in_registry(&mut reg, &id, target_mode)
    })
    .await
    .context("Registry task panicked")?;

    handle_claim_result(result, graceful)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minion_registry::{get_process_start_time, MinionRegistry, OrchestrationPhase};
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Creates a test MinionInfo with sensible defaults (mode: Stopped, no PID).
    fn test_minion_info() -> MinionInfo {
        let now = Utc::now();
        MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: Some(42),
            command: "do".to_string(),
            prompt: "Do issue #42".to_string(),
            started_at: now,
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            pid: None,
            pid_start_time: None,
            mode: MinionMode::Stopped,
            last_activity: now,
            orchestration_phase: OrchestrationPhase::Setup,
            token_usage: None,
            agent_name: "claude".to_string(),
            timeout_deadline: None,
            attempt_count: 0,
            no_watch: false,
            last_review_check_time: None,
            wake_reason: None,
            archived_at: None,
        }
    }

    // --- Display / downcast tests (pre-existing) ---

    #[test]
    fn test_session_claim_error_display_already_running() {
        let err = SessionClaimError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("already running"));
        assert!(msg.contains("autonomous"));
        assert!(msg.contains("gru stop M001"));
    }

    #[test]
    fn test_session_claim_error_is_downcastable() {
        let err: anyhow::Error = SessionClaimError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        }
        .into();
        assert!(err.downcast_ref::<SessionClaimError>().is_some());
    }

    // --- Async tests for check_and_claim_session ---

    /// Helper: register a minion directly in a temp-dir registry.
    fn register_minion(state_dir: &std::path::Path, id: &str, info: MinionInfo) {
        let mut reg = MinionRegistry::load(Some(state_dir)).unwrap();
        reg.register(id.to_string(), info).unwrap();
    }

    /// Helper: read back a minion from a temp-dir registry.
    fn read_minion(state_dir: &std::path::Path, id: &str) -> Option<MinionInfo> {
        let reg = MinionRegistry::load(Some(state_dir)).unwrap();
        reg.get(id).cloned()
    }

    #[tokio::test]
    async fn test_stopped_minion_claim_succeeds() {
        let tmp = tempdir().unwrap();
        let info = test_minion_info(); // mode: Stopped, pid: None
        register_minion(tmp.path(), "M001", info.clone());

        let result =
            check_and_claim_session_with_dir(tmp.path(), "M001", MinionMode::Interactive, false)
                .await
                .unwrap();

        // Returns the pre-claim snapshot
        let snapshot = result.expect("should return Some for existing minion");
        assert_eq!(snapshot.mode, MinionMode::Stopped);
        assert_eq!(snapshot.repo, "fotoetienne/gru");

        // Registry should now show Interactive mode
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Interactive);
    }

    #[tokio::test]
    async fn test_already_running_with_live_pid() {
        let tmp = tempdir().unwrap();
        let our_pid = std::process::id();
        let info = MinionInfo {
            mode: MinionMode::Autonomous,
            pid: Some(our_pid),
            pid_start_time: get_process_start_time(our_pid),
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let err =
            check_and_claim_session_with_dir(tmp.path(), "M001", MinionMode::Interactive, false)
                .await
                .unwrap_err();

        let claim_err = err.downcast_ref::<SessionClaimError>().unwrap();
        let SessionClaimError::AlreadyRunning { minion_id, mode } = claim_err;
        assert_eq!(minion_id, "M001");
        assert_eq!(*mode, MinionMode::Autonomous);

        // Registry should be unchanged
        let unchanged = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(unchanged.mode, MinionMode::Autonomous);
    }

    #[tokio::test]
    async fn test_dead_pid_resets_and_claims() {
        let tmp = tempdir().unwrap();
        // PID 4,194,304 (2^22) exceeds Linux's PID_MAX_LIMIT and typical macOS
        // pid_max, so it is guaranteed never to be assigned to a live process.
        let dead_pid = 4_194_304_u32;
        let info = MinionInfo {
            mode: MinionMode::Autonomous,
            pid: Some(dead_pid),
            pid_start_time: Some(1_000_000),
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let result =
            check_and_claim_session_with_dir(tmp.path(), "M001", MinionMode::Interactive, false)
                .await
                .unwrap();

        // Should succeed — the dead PID was detected and the entry was reset
        let snapshot = result.expect("should return Some after dead-PID reset");
        // Snapshot is from *before* the reset, so it still shows Autonomous
        assert_eq!(snapshot.mode, MinionMode::Autonomous);
        assert_eq!(snapshot.pid, Some(dead_pid));

        // Registry should now show Interactive mode with PID cleared
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Interactive);
        assert_eq!(updated.pid, None);
        assert_eq!(updated.pid_start_time, None);
    }

    #[tokio::test]
    async fn test_stale_no_pid_resets_and_claims() {
        let tmp = tempdir().unwrap();
        let info = MinionInfo {
            mode: MinionMode::Autonomous,
            pid: None, // Non-Stopped mode but no PID — stale entry
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let result =
            check_and_claim_session_with_dir(tmp.path(), "M001", MinionMode::Interactive, false)
                .await
                .unwrap();

        // Should succeed — the stale entry was detected and reset
        let snapshot = result.expect("should return Some after stale-entry reset");
        // Snapshot is from *before* the reset, so it still shows Autonomous
        assert_eq!(snapshot.mode, MinionMode::Autonomous);
        assert_eq!(snapshot.pid, None);

        // Registry should now show Interactive mode with PID fields cleared
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Interactive);
        assert_eq!(updated.pid, None);
        assert_eq!(updated.pid_start_time, None);
    }

    #[tokio::test]
    async fn test_graceful_swallows_registry_io_errors() {
        // Point at a non-existent path that can't be created (nested under a file)
        let tmp = tempdir().unwrap();
        let bad_path = tmp.path().join("not-a-dir");
        std::fs::write(&bad_path, "blocker").unwrap(); // create a file, not a dir
        let impossible_dir = bad_path.join("state"); // can't create dir inside a file

        let result = check_and_claim_session_with_dir(
            &impossible_dir,
            "M001",
            MinionMode::Interactive,
            true, // graceful
        )
        .await;

        // graceful=true should swallow the IO error and return Ok(None)
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_non_graceful_propagates_registry_io_errors() {
        let tmp = tempdir().unwrap();
        let bad_path = tmp.path().join("not-a-dir");
        std::fs::write(&bad_path, "blocker").unwrap();
        let impossible_dir = bad_path.join("state");

        let result = check_and_claim_session_with_dir(
            &impossible_dir,
            "M001",
            MinionMode::Interactive,
            false, // non-graceful
        )
        .await;

        // graceful=false should propagate the IO error
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("Failed to access minion registry"),
            "unexpected error: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_nonexistent_minion_returns_none() {
        let tmp = tempdir().unwrap();
        // Empty registry — no minions registered
        let result =
            check_and_claim_session_with_dir(tmp.path(), "M999", MinionMode::Interactive, false)
                .await
                .unwrap();

        assert!(result.is_none());
    }
}
