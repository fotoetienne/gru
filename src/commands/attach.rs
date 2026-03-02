use crate::minion_registry::{is_process_alive, with_registry, MinionMode};
use crate::minion_resolver;
use anyhow::{Context, Result};
use chrono::Utc;
use std::process::Stdio;
use tokio::process::Command;

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
pub async fn handle_attach(id: String, yolo: bool) -> Result<i32> {
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

    // Update registry with PID (may be None if process exited instantly)
    let pid_at_spawn = child.id();
    let mid = minion.minion_id.clone();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
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
            // SIGINT was sent to the process group - child also received it.
            // Wait for the child to exit so we can clean up the registry.
            child.wait().await.context("Failed to wait for claude after interrupt")?
        }
    };

    // Update registry: mode=Stopped, pid=None
    let mid = minion.minion_id;
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.pid = None;
            info.last_activity = Utc::now();
        })
    })
    .await;

    Ok(if status.success() { 0 } else { 1 })
}

/// Atomically checks if the minion is available and claims it as Interactive.
///
/// This combines the mode check and the mode update in a single `with_registry`
/// call, which holds an exclusive file lock for the duration. This prevents
/// TOCTOU races between concurrent `gru attach` calls.
///
/// Returns the session_id if the minion is found in the registry.
/// Errors if the minion is currently running (process is alive).
/// Returns None if the minion is not in the registry (allows attach without registry).
async fn check_and_claim_session(minion_id: &str) -> Result<Option<String>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| {
        // Clone data from the immutable borrow before mutating
        let info_data = reg.get(&id).map(|info| {
            (info.session_id.clone(), info.mode.clone(), info.pid)
        });

        match info_data {
            Some((session_id, mode, pid)) => {
                // Check if already running
                if mode != MinionMode::Stopped {
                    // Verify the process is actually alive. If pid is None (inconsistent
                    // state) or the process has exited (stale registry entry), we allow
                    // attach to recover gracefully.
                    if pid.is_some_and(is_process_alive) {
                        anyhow::bail!(
                            "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                            id,
                            mode,
                            id
                        );
                    }
                }
                // Atomically claim the session as Interactive
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
            // Propagate "already running" errors to the user
            let err_str = format!("{}", e);
            if err_str.contains("already running") {
                Err(e)
            } else {
                // Registry unavailable - proceed without it
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
        let result = handle_attach("nonexistent-minion-xyz".to_string(), false).await;
        assert!(result.is_err());

        // Verify the error message suggests using gru status
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_handle_attach_yolo_with_invalid_id() {
        // Test that handle_attach with yolo=true still validates the ID
        let result = handle_attach("nonexistent-minion-xyz".to_string(), true).await;
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

    #[test]
    fn test_running_check_with_live_process() {
        // Live PID should block attach
        let live_pid = Some(std::process::id());
        assert!(live_pid.is_some_and(is_process_alive));
    }

    #[test]
    fn test_minion_mode_display() {
        assert_eq!(format!("{}", MinionMode::Autonomous), "autonomous");
        assert_eq!(format!("{}", MinionMode::Interactive), "interactive");
        assert_eq!(format!("{}", MinionMode::Stopped), "stopped");
    }
}
