use crate::agent_registry;
use crate::minion_lock::MinionLock;
use crate::minion_registry::{
    is_process_alive_with_start_time, revert_to_stopped, with_registry, MinionMode,
};
use crate::minion_resolver;
use crate::session_claim::{self, SessionClaimError};
use crate::tmux::TmuxGuard;
use anyhow::{Context, Result};
use chrono::Utc;
use std::process::Stdio;
use tokio::process::Command;
use uuid::Uuid;

/// Handles the attach command to interactively attach to a Minion's agent session.
/// Returns 0 on success, 1 on error.
///
/// Reads the `agent_name` from the minion registry and resolves the correct
/// backend via `agent_registry::resolve_backend`. The backend's
/// `build_interactive_resume_command` method builds the CLI invocation.
/// Backends that don't support interactive mode (e.g. Codex) return `None`,
/// producing a clear error suggesting `gru resume` for autonomous mode.
///
/// For the default Claude backend this is equivalent to:
/// ```bash
/// cd $(gru path <id>) && claude --resume <session-id>
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
/// - On error: registry is reverted to Stopped via [`revert_to_stopped`]
pub(crate) async fn handle_attach(
    id: String,
    yolo: bool,
    no_auto_resume: bool,
    quiet: bool,
) -> Result<i32> {
    // Reuse exact same resolution as gru path
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Verify minion directory still exists
    if !minion.worktree_path.exists() {
        anyhow::bail!(
            "Minion directory no longer exists: {}\n\
             The worktree may have been removed. Try 'gru status' to see active minions.",
            minion.worktree_path.display()
        );
    }

    let checkout_path = minion.checkout_path();

    // Atomically check registry state, get session_id + agent_name, and claim as Interactive.
    // This prevents TOCTOU races where two `gru attach` calls could both see
    // mode=Stopped and proceed simultaneously.
    //
    // If the minion is already running autonomously, auto-stop it first so the
    // user can take over interactively without a separate `gru stop` step.
    let registry_data = match session_claim::check_and_claim_session(
        &minion.minion_id,
        MinionMode::Interactive,
        None, // PID is stamped after spawn() below via a separate registry update
        true, // graceful: allow attach without registry
    )
    .await
    {
        Ok(data) => data,
        Err(e) => {
            if let Some(SessionClaimError::AlreadyRunning { mode, .. }) = e.downcast_ref() {
                if *mode != MinionMode::Autonomous {
                    // Don't auto-stop interactive sessions — only autonomous ones
                    return Err(e);
                }
                // Auto-stop the running autonomous minion, then retry the claim
                auto_stop_minion(&minion.minion_id, &minion.worktree_path).await?;

                // Retry the claim now that the process is stopped
                session_claim::check_and_claim_session(
                    &minion.minion_id,
                    MinionMode::Interactive,
                    None,
                    true,
                )
                .await?
            } else {
                return Err(e);
            }
        }
    };

    // Extract session_id, agent_name, and repo; default to "claude" when not in registry
    let (session_id, agent_name, repo_str) = match registry_data {
        Some(info) => (Some(info.session_id), info.agent_name, Some(info.repo)),
        None => (None, agent_registry::DEFAULT_AGENT.to_string(), None),
    };

    // Resolve the agent backend from the stored agent name
    let backend = match agent_registry::resolve_backend(&agent_name) {
        Ok(b) => b,
        Err(e) => {
            revert_to_stopped(&minion.minion_id).await;
            return Err(e).context(format!(
                "Failed to resolve agent backend '{}' for attach",
                agent_name
            ));
        }
    };

    // Rename tmux window for the attach session
    let _tmux_guard = TmuxGuard::new(&format!("gru:{}", minion.minion_id));

    println!("🔌 Attaching to Minion {}...", minion.minion_id);
    if yolo {
        println!("⚡ YOLO mode: skipping permission prompts");
    }
    println!("📂 Workspace: {}", checkout_path.display());

    // Resolve the GitHub host from the worktree's git remote so spawned
    // processes can target the correct GHE instance without discovery.
    let owner_hint = repo_str
        .as_deref()
        .and_then(|r| r.split('/').next())
        .unwrap_or("");
    let github_host = super::resume::resolve_host_from_worktree(&checkout_path, owner_hint).await;

    // Build command for interactive mode via the resolved backend
    let mut cmd = match &session_id {
        Some(sid) => {
            let session_uuid = match Uuid::parse_str(sid) {
                Ok(uuid) => uuid,
                Err(e) => {
                    revert_to_stopped(&minion.minion_id).await;
                    return Err(
                        anyhow::anyhow!(e).context("Failed to parse session ID from registry")
                    );
                }
            };
            match backend.build_interactive_resume_command(
                &checkout_path,
                &session_uuid,
                &github_host,
            ) {
                Some(c) => c,
                None => {
                    revert_to_stopped(&minion.minion_id).await;
                    anyhow::bail!(
                        "Agent backend '{}' does not support interactive mode. \
                         Use 'gru resume {}' for autonomous mode instead.",
                        agent_name,
                        minion.minion_id
                    );
                }
            }
        }
        None => {
            // No session_id — legacy fallback only makes sense for Claude.
            // For any other backend we cannot safely guess at the session.
            if agent_name != agent_registry::DEFAULT_AGENT {
                anyhow::bail!(
                    "Cannot attach to Minion with agent '{}': no session ID in registry",
                    agent_name
                );
            }
            let mut c = Command::new("claude");
            c.arg("-r")
                .current_dir(&checkout_path)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .env("GH_HOST", &github_host);
            c
        }
    };
    if yolo {
        cmd.arg("--dangerously-skip-permissions");
    }

    // Acquire the per-minion advisory lock just before spawning. This is the
    // defence-in-depth layer for issue #865: even if the registry claim above
    // silently races (stale PID not detected, mode transition bug, concurrent
    // write), the kernel-level lock makes a second live agent structurally
    // impossible. Held until this function returns (normal exit or panic).
    //
    // Placed after the auto-stop retry path so that, when `check_and_claim_session`
    // dislodged a running autonomous minion, the child process has had time to
    // exit and release its own lock fd.
    let _minion_lock = match MinionLock::try_acquire(&minion.minion_id) {
        Ok(lock) => lock,
        Err(e) => {
            revert_to_stopped(&minion.minion_id).await;
            if let Some(SessionClaimError::AlreadyRunning { minion_id, .. }) = e.downcast_ref() {
                anyhow::bail!(
                    "Minion {} already has a live owner (advisory lock held). \
                     Stop it first with: gru stop {}",
                    minion_id,
                    minion_id
                );
            }
            return Err(e);
        }
    };

    // Spawn agent process (not exec - we need to update registry after exit)
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            revert_to_stopped(&minion.minion_id).await;
            return Err(e).context(format!(
                "Failed to start agent '{}'. Is the CLI installed and in your PATH?",
                agent_name
            ));
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
            info.pid_start_time =
                pid_at_spawn.and_then(crate::minion_registry::get_process_start_time);
            info.last_activity = Utc::now();
        })
    })
    .await;

    // Wait for agent to exit, handling Ctrl-C gracefully.
    // Without this select!, SIGINT would kill gru immediately (default OS behavior),
    // skipping registry cleanup. By catching ctrl_c(), we ensure the "mode=Stopped"
    // update always runs.
    let status = super::child_process::wait_with_ctrlc_handling(&mut child).await?;

    // Update registry: mode=Stopped, pid=None
    let mid = minion.minion_id.clone();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.clear_pid();
            info.last_activity = Utc::now();
        })
    })
    .await;

    // On successful exit, offer to resume autonomous monitoring
    if status.success() && !no_auto_resume && !quiet && prompt_auto_resume().await {
        println!("Resuming autonomous monitoring...");
        // Release our advisory lock before handing off to the resume pipeline;
        // `handle_resume` acquires its own lock on the same minion and would
        // otherwise deadlock against this process's fd (fs2 locks are
        // per-fd — reopening the same path in the same process still blocks).
        drop(_minion_lock);
        return crate::commands::resume::handle_resume(minion.minion_id, None, None, quiet).await;
    }

    Ok(if status.success() { 0 } else { 1 })
}

/// Graceful stop timeout before escalating to hard kill.
const AUTO_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Auto-stops a running Minion so `attach` can take over interactively.
///
/// Strategy: send SIGTERM via the registry PID (or fall back to pgrep), then
/// wait up to 10 seconds for the process to exit. If it's still alive after
/// the timeout, hard-kill it.
async fn auto_stop_minion(minion_id: &str, worktree_path: &std::path::Path) -> Result<()> {
    println!("Stopping Minion {}... attaching.", minion_id);

    // Try graceful termination first
    let pid_terminated = super::stop::terminate_via_registry_pid(minion_id, false).await;

    if !pid_terminated {
        // Legacy fallback: scan for processes via pgrep
        super::stop::terminate_claude_in_worktree(worktree_path, false).await?;
    }

    // Wait for the process to actually exit (up to 10s), then hard-kill
    let mid = minion_id.to_string();
    let deadline = tokio::time::Instant::now() + AUTO_STOP_TIMEOUT;
    loop {
        // Check if process is still alive via registry PID
        let mid_clone = mid.clone();
        let still_alive = match with_registry(move |reg| {
            Ok(reg.get(&mid_clone).map(|info| {
                info.pid
                    .map(|pid| is_process_alive_with_start_time(pid, info.pid_start_time))
                    .unwrap_or(false)
            }))
        })
        .await
        {
            Ok(Some(alive)) => alive,
            _ => false,
        };

        if !still_alive {
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            // Hard-kill after timeout
            println!("Force-killing Minion {} after 10s timeout.", minion_id);
            let killed = super::stop::terminate_via_registry_pid(minion_id, true).await;
            if !killed {
                log::warn!(
                    "Failed to hard-kill Minion {}; proceeding with attach anyway",
                    minion_id
                );
            }
            // Brief pause for the kill to take effect
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Don't update the registry here — check_and_claim_session's retry will
    // atomically detect the dead PID and reset to Stopped before claiming as
    // Interactive, avoiding a race window between two separate registry writes.

    Ok(())
}

/// Prompts the user whether to auto-resume autonomous monitoring.
///
/// Returns `true` if the user confirmed (Enter, "y", or "yes"), `false` otherwise
/// (including "n", Ctrl+C, Ctrl+D, or any read error).
async fn prompt_auto_resume() -> bool {
    use std::io::Write;
    use tokio::io::AsyncBufReadExt;

    print!("Auto-resume autonomous monitoring? [Y/n] ");
    if std::io::stdout().flush().is_err() {
        return false;
    }

    let mut input = String::new();
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);

    // Race stdin against Ctrl+C so the user isn't stuck at the prompt
    // if SIGINT is caught by Tokio instead of killing the process.
    tokio::select! {
        result = reader.read_line(&mut input) => {
            match result {
                Ok(0) | Err(_) => false, // EOF (Ctrl+D) or error
                Ok(_) => crate::prompt_utils::is_affirmative(&input),
            }
        }
        _ = tokio::signal::ctrl_c() => false,
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

    #[tokio::test]
    async fn test_auto_stop_minion_no_process() {
        // When no process is running, auto_stop_minion should succeed
        // (the registry won't have this minion, so it gracefully exits)
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let minion_id = format!("TEST_{}", Uuid::new_v4().simple());
        let result = auto_stop_minion(&minion_id, temp_dir.path()).await;
        assert!(result.is_ok());
    }
}
