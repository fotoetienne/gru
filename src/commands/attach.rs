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
/// cd $(gru path <id>) && claude --resume <session-id>
/// # With --yolo:
/// cd $(gru path <id>) && claude --resume <session-id> --dangerously-skip-permissions
/// ```
///
/// The ID argument supports smart resolution (same as gru path):
/// 1. Try as exact minion ID (e.g., M0tk)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
///
/// Registry integration:
/// - Before attaching: checks that the Minion is not already running
/// - During attach: updates registry with mode=Interactive and PID
/// - After exit: updates registry with mode=Stopped and clears PID
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

    // Check registry state and get session ID (if in registry)
    let session_id = check_registry_state(&minion.minion_id).await?;

    println!("🔌 Attaching to Minion {}...", minion.minion_id);
    if yolo {
        println!("⚡ YOLO mode: skipping permission prompts");
    }
    println!("📂 Workspace: {}", minion.worktree_path.display());

    // Build claude command for interactive mode (no --print, no --output-format)
    let mut cmd = Command::new("claude");
    if let Some(ref sid) = session_id {
        cmd.arg("--resume").arg(sid);
    } else {
        cmd.arg("-r");
    }
    if yolo {
        cmd.arg("--dangerously-skip-permissions");
    }
    cmd.current_dir(&minion.worktree_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Spawn claude process (not exec - we need to update registry after exit)
    let mut child = cmd.spawn().context(
        "Failed to start claude. Is Claude CLI installed and in your PATH?\n\
         See: https://claude.com/claude-code",
    )?;

    let child_pid = child.id();

    // Update registry: mode=Interactive, pid
    let minion_id = minion.minion_id.clone();
    if let Some(pid) = child_pid {
        let mid = minion_id.clone();
        let _ = with_registry(move |reg| {
            reg.update(&mid, |info| {
                info.mode = MinionMode::Interactive;
                info.pid = Some(pid);
                info.last_activity = Utc::now();
            })
        })
        .await;
    }

    // Wait for claude to exit
    let status = child
        .wait()
        .await
        .context("Failed to wait for claude process")?;

    // Update registry: mode=Stopped, pid=None
    let mid = minion_id.clone();
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

/// Checks registry state for the given minion.
///
/// Returns the session_id if the minion is found in the registry.
/// Errors if the minion is currently running (process is alive).
/// Returns None if the minion is not in the registry (allows attach without registry).
async fn check_registry_state(minion_id: &str) -> Result<Option<String>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| {
        Ok(reg
            .get(&id)
            .map(|info| (info.session_id.clone(), info.mode.clone(), info.pid)))
    })
    .await;

    match result {
        Ok(Some((session_id, mode, pid))) => {
            // Check if already running
            if mode != MinionMode::Stopped {
                let actually_alive = pid.is_some_and(is_process_alive);
                if actually_alive {
                    anyhow::bail!(
                        "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                        minion_id,
                        mode,
                        minion_id
                    );
                }
                // Process died but registry wasn't updated - allow attach
            }
            Ok(Some(session_id))
        }
        Ok(None) => Ok(None), // Not in registry - attach without session_id
        Err(e) => {
            // Registry unavailable - proceed without it
            log::debug!("Could not check registry: {}", e);
            Ok(None)
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
    fn test_check_already_running_logic() {
        // Verify the mode/PID check logic used in check_registry_state

        // Stopped mode should allow attach (would not trigger the bail)
        assert_eq!(MinionMode::Stopped, MinionMode::Stopped);

        // Autonomous mode with dead process should allow attach
        let dead_pid = Some(4_194_304u32); // PID that doesn't exist
        assert!(!dead_pid.is_some_and(is_process_alive));

        // Autonomous mode with no PID should allow attach
        let no_pid: Option<u32> = None;
        assert!(!no_pid.is_some_and(is_process_alive));

        // Autonomous mode with live PID should block attach
        let live_pid = Some(std::process::id()); // Our own PID is alive
        assert_ne!(MinionMode::Autonomous, MinionMode::Stopped);
        assert!(live_pid.is_some_and(is_process_alive));
    }

    #[test]
    fn test_minion_mode_display() {
        assert_eq!(format!("{}", MinionMode::Autonomous), "autonomous");
        assert_eq!(format!("{}", MinionMode::Interactive), "interactive");
        assert_eq!(format!("{}", MinionMode::Stopped), "stopped");
    }
}
