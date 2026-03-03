use crate::minion_registry::{is_process_alive, with_registry, MinionMode};
use crate::minion_resolver;
use anyhow::{Context, Result};
use chrono::Utc;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::Duration;

/// Typed errors from the attach command that callers can match on via
/// `downcast_ref::<AttachError>()` instead of brittle string matching.
#[derive(Debug)]
enum AttachError {
    /// The minion has a live process — user must stop it first.
    AlreadyRunning { minion_id: String, mode: MinionMode },
    /// Registry shows a non-Stopped mode but no PID is recorded.
    InconsistentState { minion_id: String, mode: MinionMode },
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::AlreadyRunning { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                    minion_id, mode, minion_id
                )
            }
            AttachError::InconsistentState { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is currently in {} mode without an associated process. \
                     Please wait or run 'gru status' / 'gru stop {}' to recover.",
                    minion_id, mode, minion_id
                )
            }
        }
    }
}

impl std::error::Error for AttachError {}

/// Grace period for the child process to exit after receiving a signal
/// before we force-kill it.
const CTRL_C_GRACE_SECS: u64 = 5;

/// Handles the attach command to attach to a Minion's Claude session
/// Returns 0 on success, 1 on error
///
/// This command attaches to a stopped Minion's session interactively,
/// allowing you to unstick it without losing conversation context.
///
/// It is functionally equivalent to:
/// ```bash
/// # With session_id from registry:
/// cd $(gru path <id>) && claude --resume <session-id>
/// # Without registry (fallback):
/// cd $(gru path <id>) && claude -r
/// ```
///
/// The ID argument supports smart resolution (same as gru path):
/// 1. Try as exact minion ID (e.g., M0tk)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
///
/// Registry integration:
/// - Before attaching: atomically checks mode and claims the session as Interactive
/// - During attach: updates registry with PID after spawn
/// - After exit: updates registry with mode=Stopped and clears PID
/// - Signal handling: Ctrl-C is caught to ensure registry cleanup runs
///
/// Note: This command is identical to `gru resume` - both attach to the
/// same Claude session interactively. The `attach` name is used for
/// consistency with documentation and expected UX.
pub async fn handle_attach(
    id: String,
    yolo: bool,
    no_auto_resume: bool,
    quiet: bool,
) -> Result<i32> {
    // Reuse exact same resolution as gru path
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Verify worktree still exists
    if !minion.worktree_path.exists() {
        anyhow::bail!(
            "Worktree directory no longer exists: {}\n\
             The worktree may have been removed. Try 'gru status' to see active minions.",
            minion.worktree_path.display()
        );
    }

    // Atomically check registry state, get session_id, and claim as Interactive.
    // This prevents TOCTOU races where two `gru attach` calls could both see
    // mode=Stopped and proceed simultaneously.
    let session_id = check_and_claim_session(&minion.minion_id).await?;

    println!("🔌 Attaching to Minion {}...", minion.minion_id);
    if yolo {
        println!("⚡ YOLO mode: skipping permission prompts");
    }
    println!("📂 Workspace: {}", minion.worktree_path.display());

    // Build claude command for interactive mode (no --print, no --output-format)
    let mut cmd = Command::new("claude");
    match &session_id {
        Some(sid) => {
            cmd.arg("--resume").arg(sid);
        }
        None => {
            cmd.arg("-r");
        }
    }
    if yolo {
        cmd.arg("--dangerously-skip-permissions");
    }
    cmd.current_dir(&minion.worktree_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Spawn claude process (not exec - we need to update registry after exit)
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Spawn failed - revert registry to Stopped
            let mid = minion.minion_id.clone();
            let _ = with_registry(move |reg| {
                reg.update(&mid, |info| {
                    info.mode = MinionMode::Stopped;
                    info.pid = None;
                    info.last_activity = Utc::now();
                })
            })
            .await;
            return Err(e).context(
                "Failed to start claude. Is Claude CLI installed and in your PATH?\n\
                 See: https://claude.com/claude-code",
            );
        }
    };

    // Update registry with PID and ensure mode=Interactive.
    // Setting mode here is idempotent when check_and_claim_session already claimed,
    // but covers the edge case where the claim fell back to Ok(None) due to a
    // transient registry failure while the registry is now available again.
    let pid_at_spawn = child.id();
    let mid = minion.minion_id.clone();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Interactive;
            info.pid = pid_at_spawn;
            info.last_activity = Utc::now();
        })
    })
    .await;

    // Wait for claude to exit, handling Ctrl-C gracefully.
    // Without this select!, SIGINT would kill gru immediately (default OS behavior),
    // skipping registry cleanup. By catching ctrl_c(), we ensure the "mode=Stopped"
    // update always runs.
    let status = tokio::select! {
        result = child.wait() => result.context("Failed to wait for claude process")?,
        _ = tokio::signal::ctrl_c() => {
            // Explicitly signal the child to terminate in case it didn't receive
            // the SIGINT (different process group, platform differences, etc.)
            signal_child(&mut child);

            // Wait with a grace period, then force-kill if still running
            match tokio::time::timeout(
                Duration::from_secs(CTRL_C_GRACE_SECS),
                child.wait(),
            )
            .await
            {
                Ok(result) => result.context("Failed to wait for claude after interrupt")?,
                Err(_) => {
                    log::warn!("Claude process did not exit within {}s, force-killing", CTRL_C_GRACE_SECS);
                    let _ = child.kill().await;
                    child.wait().await.context("Failed to reap claude after force-kill")?
                }
            }
        }
    };

    // Update registry: mode=Stopped, pid=None
    let mid = minion.minion_id.clone();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.pid = None;
            info.last_activity = Utc::now();
        })
    })
    .await;

    // On successful exit, offer to resume autonomous monitoring
    if status.success() && !no_auto_resume && prompt_auto_resume() {
        println!("Resuming autonomous monitoring...");
        return crate::commands::resume::handle_resume(minion.minion_id, None, None, quiet).await;
    }

    Ok(if status.success() { 0 } else { 1 })
}

/// Prompts the user whether to auto-resume autonomous monitoring.
///
/// Returns `true` if the user confirmed (Enter, "y", or "yes"), `false` otherwise
/// (including "n", Ctrl+C, or any read error).
fn prompt_auto_resume() -> bool {
    use std::io::{stdin, stdout, Write};

    print!("Auto-resume autonomous monitoring? [Y/n] ");
    if stdout().flush().is_err() {
        return false;
    }

    let mut input = String::new();
    match stdin().read_line(&mut input) {
        Ok(0) | Err(_) => false, // EOF (Ctrl+D) or error
        Ok(_) => is_affirmative(&input),
    }
}

/// Returns `true` if the input is an affirmative answer (empty, "y", or "yes").
fn is_affirmative(input: &str) -> bool {
    let answer = input.trim().to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// Send a termination signal to the child process.
/// On Unix, sends SIGTERM for graceful shutdown.
/// On other platforms, attempts kill (there is no graceful signal equivalent).
fn signal_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill with SIGTERM is safe — it requests graceful termination.
            // The PID was just obtained from the child handle.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        // No graceful signal on non-Unix; start_kill is best-effort
        let _ = child.start_kill();
    }
}

/// Atomically checks if the minion is available and claims it as Interactive.
///
/// This combines the mode check and the mode update in a single `with_registry`
/// call, which holds an exclusive file lock for the duration. This prevents
/// TOCTOU races between concurrent `gru attach` calls.
///
/// Returns the session_id if the minion is found in the registry.
/// Errors with [`AttachError::AlreadyRunning`] if the minion has a live process.
/// Errors with [`AttachError::InconsistentState`] if the mode is non-Stopped but
/// no PID is recorded (ambiguous state that needs manual recovery).
/// Returns None if the minion is not in the registry (allows attach without registry).
async fn check_and_claim_session(minion_id: &str) -> Result<Option<String>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| {
        // Clone data from the immutable borrow before mutating
        let info_data = reg
            .get(&id)
            .map(|info| (info.session_id.clone(), info.mode.clone(), info.pid));

        match info_data {
            Some((session_id, mode, pid)) => {
                // Treat any non-Stopped mode as an exclusive lock by default.
                // Only allow recovery when there is a PID and it is confirmed dead,
                // and bring the entry back to a consistent Stopped state before
                // claiming Interactive within this same registry lock.
                if mode != MinionMode::Stopped {
                    match pid {
                        Some(pid_val) => {
                            // On Unix, verify the process is actually alive.
                            // On non-Unix, is_process_alive always returns false, so be
                            // conservative and treat any recorded PID as alive (matching
                            // the pattern in commands/clean.rs).
                            let is_running = cfg!(not(unix)) || is_process_alive(pid_val);
                            if is_running {
                                return Err(AttachError::AlreadyRunning {
                                    minion_id: id,
                                    mode,
                                }
                                .into());
                            }
                            // Stale entry: process is dead but registry still thinks
                            // it's running. Reset to a consistent Stopped state before
                            // proceeding to claim.
                            reg.update(&id, |info| {
                                info.mode = MinionMode::Stopped;
                                info.pid = None;
                                info.last_activity = Utc::now();
                            })?;
                        }
                        None => {
                            // Inconsistent state: mode != Stopped but no PID recorded.
                            // Treat this as locked/in use to avoid double-attach.
                            return Err(AttachError::InconsistentState {
                                minion_id: id,
                                mode,
                            }
                            .into());
                        }
                    }
                }

                // At this point, the entry is either:
                // - cleanly Stopped, or
                // - was a stale running entry that we just reset to Stopped.
                // Atomically claim the session as Interactive.
                reg.update(&id, |info| {
                    info.mode = MinionMode::Interactive;
                    info.last_activity = Utc::now();
                })?;
                Ok(Some(session_id))
            }
            None => Ok(None), // Not in registry
        }
    })
    .await;

    match result {
        Ok(session_id) => Ok(session_id),
        Err(e) => {
            // Use typed error matching instead of brittle string comparison.
            // AttachError variants are user-facing errors that must propagate.
            if e.downcast_ref::<AttachError>().is_some() {
                Err(e)
            } else {
                // Registry unavailable (lock contention, IO error, etc.) —
                // proceed without it as a graceful degradation.
                log::debug!("Could not check registry: {}", e);
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_attach_with_invalid_id() {
        // Test that handle_attach returns an error for an invalid ID
        // This verifies the minion_resolver integration works correctly
        let result = handle_attach("nonexistent-minion-xyz".to_string(), false, false, false).await;
        assert!(result.is_err());

        // Verify the error message suggests using gru status
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_handle_attach_yolo_with_invalid_id() {
        // Test that handle_attach with yolo=true still validates the ID
        let result = handle_attach("nonexistent-minion-xyz".to_string(), true, false, false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[tokio::test]
    async fn test_handle_attach_no_auto_resume_with_invalid_id() {
        // Test that handle_attach with no_auto_resume=true still validates the ID
        let result = handle_attach("nonexistent-minion-xyz".to_string(), false, true, false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[test]
    fn test_running_check_with_dead_process() {
        // Dead PID should not block attach
        let dead_pid = Some(4_194_304u32); // PID that doesn't exist
        assert!(!dead_pid.is_some_and(is_process_alive));
    }

    #[test]
    fn test_running_check_with_no_pid() {
        // Missing PID should not block attach
        let no_pid: Option<u32> = None;
        assert!(!no_pid.is_some_and(is_process_alive));
    }

    // is_process_alive always returns false on non-Unix, so only assert
    // that a live PID is detected on Unix targets.
    #[cfg(unix)]
    #[test]
    fn test_running_check_with_live_process() {
        // Live PID should block attach on Unix
        let live_pid = Some(std::process::id());
        assert!(live_pid.is_some_and(is_process_alive));
    }

    #[test]
    fn test_minion_mode_display() {
        assert_eq!(format!("{}", MinionMode::Autonomous), "autonomous");
        assert_eq!(format!("{}", MinionMode::Interactive), "interactive");
        assert_eq!(format!("{}", MinionMode::Stopped), "stopped");
    }

    #[test]
    fn test_attach_error_display_already_running() {
        let err = AttachError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("already running"));
        assert!(msg.contains("autonomous"));
        assert!(msg.contains("gru stop M001"));
    }

    #[test]
    fn test_attach_error_display_inconsistent_state() {
        let err = AttachError::InconsistentState {
            minion_id: "M002".to_string(),
            mode: MinionMode::Interactive,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("interactive mode"));
        assert!(msg.contains("without an associated process"));
        assert!(msg.contains("gru stop M002"));
    }

    #[test]
    fn test_attach_error_is_downcastable() {
        let err: anyhow::Error = AttachError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        }
        .into();
        assert!(err.downcast_ref::<AttachError>().is_some());
    }

    #[test]
    fn test_is_affirmative_empty_input() {
        // Enter key (empty input) defaults to yes
        assert!(is_affirmative(""));
        assert!(is_affirmative("\n"));
        assert!(is_affirmative("  \n"));
    }

    #[test]
    fn test_is_affirmative_yes_variants() {
        assert!(is_affirmative("y\n"));
        assert!(is_affirmative("Y\n"));
        assert!(is_affirmative("yes\n"));
        assert!(is_affirmative("YES\n"));
        assert!(is_affirmative("Yes\n"));
        assert!(is_affirmative("  y  \n"));
    }

    #[test]
    fn test_is_affirmative_no_variants() {
        assert!(!is_affirmative("n\n"));
        assert!(!is_affirmative("N\n"));
        assert!(!is_affirmative("no\n"));
        assert!(!is_affirmative("NO\n"));
        assert!(!is_affirmative("nope\n"));
        assert!(!is_affirmative("anything else\n"));
    }
}
