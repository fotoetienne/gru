use crate::minion_registry::{with_registry, MinionInfo, MinionMode, MinionRegistry};
use anyhow::Result;
use chrono::Utc;

/// Typed errors from session-claim operations (shared by attach and resume).
#[derive(Debug)]
pub(crate) enum SessionClaimError {
    /// The registry says the minion has a live process — user must stop it first.
    /// Carries the registry-recorded mode so callers (`gru attach`) can decide
    /// whether to auto-stop an autonomous owner before taking over.
    AlreadyRunning { minion_id: String, mode: MinionMode },
    /// Another process holds the per-minion advisory lockfile (issue #865).
    /// Surfaced from [`MinionLock::try_acquire`](crate::minion_lock::MinionLock::try_acquire).
    /// Distinct from [`AlreadyRunning`](Self::AlreadyRunning) because the registry
    /// mode is unknown at the point of contention — the lock-holder owns both
    /// the file lock and the authoritative registry state.
    LockContention { minion_id: String },
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
            SessionClaimError::LockContention { minion_id } => {
                write!(
                    f,
                    "Minion {} already has a live owner (advisory lock held). \
                     Stop it first with: gru stop {}",
                    minion_id, minion_id
                )
            }
        }
    }
}

impl std::error::Error for SessionClaimError {}

/// Core claim logic shared by the production function and the test-only variant.
///
/// Checks if a minion is available in the given registry and claims it with
/// `target_mode`. When `claim_pid` is `Some((pid, start_time))`, the PID and
/// its start time are written in the same registry save as the mode flip,
/// closing the TOCTOU window where a concurrent claimer would otherwise see
/// `mode=<target>, pid=None` and treat the entry as stale (see issue #864).
///
/// When `claim_pid` is `None`, only `mode`/`last_activity`/`archived_at` are
/// touched in the claim write — whatever `pid`/`pid_start_time` survived from
/// the earlier branches (the stale-reset's `clear_pid`, or the pre-existing
/// values on a clean Stopped entry) remain. The caller is expected to stamp a
/// PID separately. This is the path `attach.rs` uses because the spawned
/// child's PID isn't known until after `cmd.spawn()`.
///
/// Returns a snapshot of the `MinionInfo` before claiming.
fn claim_session_in_registry(
    reg: &mut MinionRegistry,
    minion_id: &str,
    target_mode: MinionMode,
    claim_pid: Option<(u32, Option<i64>)>,
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
    // When claim_pid is provided and orchestration_phase is non-terminal, stamp
    // pid/pid_start_time atomically so no concurrent claimer can observe a
    // `mode=<target>, pid=None` intermediate state (issue #864). Terminal-phase
    // rows skip the PID stamp — see the guard below (issue #878).
    reg.update(&id, |i| {
        i.mode = target_mode.clone();
        i.last_activity = Utc::now();
        i.archived_at = None;
        if let Some((pid, start_time)) = claim_pid {
            // Never stamp a PID onto a terminal-phase row. Terminal rows (Failed,
            // Completed) are historical records; stamping the transient lab-parent
            // PID causes is_running() to return true for that PID. A subsequent
            // worker for the same issue then sees mode=Autonomous with a live-looking
            // PID and returns AlreadyRunning (exit code 3), which triggers the
            // auto-recovery → gru:todo → re-pickup thrash loop (issue #878).
            if !i.orchestration_phase.is_terminal() {
                i.pid = Some(pid);
                i.pid_start_time = start_time;
            }
        }
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
/// When `claim_pid` is `Some((pid, start_time))`, the PID and start time are
/// written in the same registry save as the mode flip. Callers that own a
/// live process (e.g., `gru resume` with `std::process::id()`, or the
/// post-agent cleanup in `fix/agent.rs` transferring ownership from the
/// now-dead child to the parent worker) should use this to close the TOCTOU
/// window where a concurrent claimer would otherwise observe
/// `mode=<target>, pid=None` and treat the entry as stale (issue #864).
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
    claim_pid: Option<(u32, Option<i64>)>,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    let id = minion_id.to_string();
    let result =
        with_registry(move |reg| claim_session_in_registry(reg, &id, target_mode, claim_pid)).await;

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
    claim_pid: Option<(u32, Option<i64>)>,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    use anyhow::Context as _;
    let id = minion_id.to_string();
    let dir = state_dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        let mut reg = MinionRegistry::load(Some(&dir))?;
        claim_session_in_registry(&mut reg, &id, target_mode, claim_pid)
    })
    .await
    .context("Registry task panicked")?;

    handle_claim_result(result, graceful)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minion_registry::{
        get_process_start_time, is_process_alive_with_start_time, MinionRegistry,
        OrchestrationPhase,
    };
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
            pending_review_sha: None,
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

    #[test]
    fn test_session_claim_error_display_lock_contention() {
        let err = SessionClaimError::LockContention {
            minion_id: "M042".to_string(),
        };
        let msg = format!("{}", err);
        // Must NOT include a fabricated registry mode (issue #865 review feedback).
        assert!(!msg.contains("mode:"), "unexpected mode in message: {msg}");
        assert!(msg.contains("advisory lock held"));
        assert!(msg.contains("gru stop M042"));
    }

    #[test]
    fn test_session_claim_error_lock_contention_downcastable() {
        let err: anyhow::Error = SessionClaimError::LockContention {
            minion_id: "M042".to_string(),
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

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Interactive,
            None,
            false,
        )
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

        let err = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Interactive,
            None,
            false,
        )
        .await
        .unwrap_err();

        let claim_err = err.downcast_ref::<SessionClaimError>().unwrap();
        match claim_err {
            SessionClaimError::AlreadyRunning { minion_id, mode } => {
                assert_eq!(minion_id, "M001");
                assert_eq!(*mode, MinionMode::Autonomous);
            }
            other => panic!("expected AlreadyRunning, got {:?}", other),
        }

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

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Interactive,
            None,
            false,
        )
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

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Interactive,
            None,
            false,
        )
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
            None,
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
            None,
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
        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M999",
            MinionMode::Interactive,
            None,
            false,
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }

    // --- issue #864: atomic claim-with-PID ---

    #[tokio::test]
    async fn test_claim_with_pid_writes_pid_atomically() {
        let tmp = tempdir().unwrap();
        let info = test_minion_info(); // mode: Stopped, pid: None
        register_minion(tmp.path(), "M001", info);

        let our_pid = std::process::id();
        let our_start = get_process_start_time(our_pid);

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Autonomous,
            Some((our_pid, our_start)),
            false,
        )
        .await
        .unwrap();

        assert!(result.is_some(), "claim should have succeeded");

        // The registry write should contain mode, pid, and pid_start_time.
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Autonomous);
        assert_eq!(updated.pid, Some(our_pid));
        assert_eq!(updated.pid_start_time, our_start);
    }

    #[tokio::test]
    async fn test_claim_with_pid_after_dead_pid_reset() {
        // Dead PID in the registry (the scenario fix/agent.rs post-exit cleanup
        // sees: registry points at the now-dead agent child). A concurrent claim
        // that passes its own pid should reset the stale entry and atomically
        // stamp the new pid in a single save.
        let tmp = tempdir().unwrap();
        let dead_pid = 4_194_304_u32;
        let info = MinionInfo {
            mode: MinionMode::Autonomous,
            pid: Some(dead_pid),
            pid_start_time: Some(1_000_000),
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let our_pid = std::process::id();
        let our_start = get_process_start_time(our_pid);

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Autonomous,
            Some((our_pid, our_start)),
            false,
        )
        .await
        .unwrap();

        assert!(result.is_some());

        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Autonomous);
        assert_eq!(updated.pid, Some(our_pid));
        assert_eq!(updated.pid_start_time, our_start);
    }

    // --- issue #878: terminal-phase PID protection ---

    /// A Failed minion's pid/pid_start_time must not be overwritten by a
    /// subsequent `check_and_claim_session_with_dir` call even when `claim_pid` is
    /// provided.  Stamping a transient lab-parent PID onto a terminal row
    /// causes `is_running()` to return true, making a concurrent worker
    /// misclassify the dead minion as AlreadyRunning and exit code 3.
    #[tokio::test]
    async fn test_terminal_phase_pid_not_stamped() {
        let tmp = tempdir().unwrap();
        let info = MinionInfo {
            mode: MinionMode::Stopped,
            pid: None,
            pid_start_time: None,
            orchestration_phase: OrchestrationPhase::Failed,
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let our_pid = std::process::id();
        let our_start = get_process_start_time(our_pid);

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Autonomous,
            Some((our_pid, our_start)),
            false,
        )
        .await
        .unwrap();

        assert!(
            result.is_some(),
            "claim should succeed for Stopped+Failed minion"
        );

        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(updated.mode, MinionMode::Autonomous);
        // PID must remain None — the terminal phase guard must have blocked the stamp.
        assert_eq!(
            updated.pid, None,
            "pid must not be stamped on a terminal-phase row"
        );
        assert_eq!(
            updated.pid_start_time, None,
            "pid_start_time must not be stamped on a terminal-phase row"
        );
    }

    /// Reproduce the exact crash scenario from issue #878: mode=Autonomous,
    /// orchestration_phase=Failed, pid=Some(dead_pid). The stale-reset path
    /// fires first (dead PID detected → mode reset to Stopped), then the claim
    /// write runs. The terminal-phase guard must prevent the new PID from being
    /// stamped even after the stale reset.
    #[tokio::test]
    async fn test_terminal_phase_non_stopped_with_dead_pid_not_stamped() {
        let tmp = tempdir().unwrap();
        // PID 4,194,304 (2^22) exceeds Linux's PID_MAX_LIMIT and typical macOS
        // pid_max, so it is guaranteed never to be assigned to a live process.
        let dead_pid = 4_194_304_u32;
        let info = MinionInfo {
            mode: MinionMode::Autonomous, // non-Stopped, as in the real crash
            pid: Some(dead_pid),
            pid_start_time: Some(1_000_000),
            orchestration_phase: OrchestrationPhase::Failed,
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let our_pid = std::process::id();
        let our_start = get_process_start_time(our_pid);

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Autonomous,
            Some((our_pid, our_start)),
            false,
        )
        .await
        .unwrap();

        assert!(
            result.is_some(),
            "claim should succeed after stale-reset of failed minion"
        );
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(
            updated.pid, None,
            "pid must not be stamped on a terminal-phase row after stale reset"
        );
        assert_eq!(updated.pid_start_time, None);
    }

    /// Completed minions are also terminal and must not have their PID stomped.
    #[tokio::test]
    async fn test_completed_phase_pid_not_stamped() {
        let tmp = tempdir().unwrap();
        let info = MinionInfo {
            mode: MinionMode::Stopped,
            pid: None,
            pid_start_time: None,
            orchestration_phase: OrchestrationPhase::Completed,
            ..test_minion_info()
        };
        register_minion(tmp.path(), "M001", info);

        let our_pid = std::process::id();
        let our_start = get_process_start_time(our_pid);

        let result = check_and_claim_session_with_dir(
            tmp.path(),
            "M001",
            MinionMode::Autonomous,
            Some((our_pid, our_start)),
            false,
        )
        .await
        .unwrap();

        assert!(result.is_some());
        let updated = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(
            updated.pid, None,
            "pid must not be stamped on a Completed-phase row"
        );
        assert_eq!(updated.pid_start_time, None);
    }

    /// Concurrent-claim race: two threads race to claim the same minion with
    /// distinct PIDs. Exactly one must win the claim; the other must see
    /// `AlreadyRunning`. This validates that the file-lock + atomic pid-write
    /// in a single save close the TOCTOU window described in issue #864 —
    /// there is no observable intermediate state where `mode=<target>` and
    /// `pid=None`.
    ///
    /// Uses distinct live PIDs (the test process and its parent) so that the
    /// final registry state uniquely identifies which thread won: the loser
    /// must see the winner's PID in the registry, not its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_claim_only_one_wins() {
        let tmp = tempdir().unwrap();
        let info = test_minion_info(); // mode: Stopped, pid: None
        register_minion(tmp.path(), "M001", info);

        // Use two distinct PIDs the OS reports as alive: the test process and
        // its parent (cargo/nextest runner), both live for the duration of
        // the test. The whole point is that whichever thread wins, the loser
        // reads a *live, matching start_time* pid from the registry and
        // correctly returns `AlreadyRunning` — not the "dead pid ⇒ reset +
        // claim" path.
        //
        // If `parent_id()` cannot be resolved to a live process (e.g. some
        // exotic CI runner), the test falls back to reusing `process::id()`,
        // which still exercises the mutual-exclusion invariant (just with a
        // weaker distinct-PID assertion at the end).
        let pid_a = std::process::id();
        let start_a = get_process_start_time(pid_a);
        // Use `libc::getppid()` rather than `std::os::unix::process::parent_id()`
        // — the latter is only available on Rust 1.83+, but the crate MSRV
        // (Cargo.toml `rust-version`) is 1.73. libc is already a direct
        // dependency.
        #[cfg(unix)]
        let parent = {
            let ppid = unsafe { libc::getppid() };
            if ppid > 0 {
                ppid as u32
            } else {
                0
            }
        };
        #[cfg(not(unix))]
        let parent: u32 = 0;
        let (pid_b, start_b) =
            match (parent != 0 && parent != pid_a).then(|| get_process_start_time(parent)) {
                Some(Some(st)) if is_process_alive_with_start_time(parent, Some(st)) => {
                    (parent, Some(st))
                }
                _ => (pid_a, start_a),
            };

        let dir_a = tmp.path().to_path_buf();
        let dir_b = tmp.path().to_path_buf();

        let handle_a = tokio::spawn(async move {
            check_and_claim_session_with_dir(
                &dir_a,
                "M001",
                MinionMode::Autonomous,
                Some((pid_a, start_a)),
                false,
            )
            .await
        });
        let handle_b = tokio::spawn(async move {
            check_and_claim_session_with_dir(
                &dir_b,
                "M001",
                MinionMode::Autonomous,
                Some((pid_b, start_b)),
                false,
            )
            .await
        });

        let (result_a, result_b) = tokio::join!(handle_a, handle_b);
        let result_a = result_a.unwrap();
        let result_b = result_b.unwrap();

        // Exactly one must succeed and one must return AlreadyRunning — the
        // file lock serializes the two claims and the atomic pid-write means
        // the second caller never sees a `pid=None` intermediate state.
        let (winner_pid, loser_result) = match (&result_a, &result_b) {
            (Ok(_), Err(_)) => (pid_a, &result_b),
            (Err(_), Ok(_)) => (pid_b, &result_a),
            (Ok(a), Ok(b)) => panic!(
                "both claims succeeded: a={:?}, b={:?} — the atomic pid-write \
                 should force the second caller to observe AlreadyRunning",
                a, b
            ),
            (Err(a), Err(b)) => panic!("both claims failed: a={:#}, b={:#}", a, b),
        };

        let loser_err = loser_result.as_ref().unwrap_err();
        assert!(
            loser_err.downcast_ref::<SessionClaimError>().is_some(),
            "loser should fail with SessionClaimError::AlreadyRunning, got: {:#}",
            loser_err
        );

        // The registry must reflect the winner's PID — not some arbitrary
        // intermediate state. This directly validates that the claim write
        // and the pid write landed in the same registry save.
        let final_state = read_minion(tmp.path(), "M001").unwrap();
        assert_eq!(final_state.mode, MinionMode::Autonomous);
        assert_eq!(
            final_state.pid,
            Some(winner_pid),
            "registry should contain the winning thread's PID"
        );
    }
}
