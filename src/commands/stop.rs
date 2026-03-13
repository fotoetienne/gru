use crate::minion_registry::{is_process_alive, with_registry, MinionMode};
use crate::minion_resolver;
use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

/// Sends a signal to a process using libc::kill.
/// Returns true if the signal was delivered successfully.
#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> bool {
    unsafe { libc::kill(pid as i32, signal) == 0 }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: i32) -> bool {
    false
}

/// Handles the stop command to terminate a running Minion.
/// Returns 0 on success, 1 on error.
///
/// Termination strategy:
/// 1. Look up PID from the minion registry (primary path for background workers)
/// 2. Fall back to ps/lsof scanning for legacy minions without PID in registry
/// 3. Update registry to mark minion as stopped
///
/// The ID argument supports smart resolution:
/// - Exact minion ID (e.g., M0tk)
/// - With M prefix (e.g., 12 -> M12)
/// - Issue number (finds minion working on that issue)
/// - PR number (finds minion via linked issue)
pub async fn handle_stop(id: String, force: bool) -> Result<i32> {
    // Resolve the minion ID to get the worktree path
    let minion = minion_resolver::resolve_minion(&id).await?;

    println!("🛑 Stopping Minion {}...", minion.minion_id);
    let checkout_path = minion.checkout_path();
    println!("📂 Workspace: {}", checkout_path.display());

    // Check if worktree exists
    if !minion.worktree_path.exists() {
        println!(
            "⚠️  Worktree no longer exists: {}",
            minion.worktree_path.display()
        );
        println!("   Removing from registry...");

        let minion_id = minion.minion_id.clone();
        with_registry(move |registry| {
            registry.remove(&minion_id)?;
            Ok(())
        })
        .await?;

        println!("✅ Minion {} removed from registry", minion.minion_id);
        return Ok(0);
    }

    // Try PID-based termination first (primary path for background workers)
    let pid_terminated = terminate_via_registry_pid(&minion.minion_id, force).await;

    if pid_terminated {
        println!(
            "🔪 Terminated Minion {} worker process{}",
            minion.minion_id,
            if force { " (forced)" } else { "" }
        );
    } else {
        // Fall back to ps/lsof scanning for legacy minions or if PID wasn't in registry
        let terminated = terminate_claude_in_worktree(&checkout_path).await?;

        if terminated > 0 {
            println!("🔪 Terminated {} process(es) in worktree", terminated);
        } else {
            println!("ℹ️  No active processes found for minion");
        }
    }

    // Update registry to mark minion as stopped
    let minion_id = minion.minion_id.clone();
    match with_registry(move |registry| {
        registry.update(&minion_id, |info| {
            info.status = "stopped".to_string();
            info.pid = None;
            info.mode = MinionMode::Stopped;
        })
    })
    .await
    {
        Ok(()) => {
            println!("📝 Updated registry: status = stopped");
        }
        Err(e) => {
            log::warn!("⚠️  Could not update registry: {}", e);
        }
    }

    println!("\n✅ Minion {} stopped", minion.minion_id);
    println!("💡 Worktree preserved at: {}", checkout_path.display());
    println!("   To clean up, run: gru clean");

    Ok(0)
}

/// Terminates a minion's worker process using the PID stored in the registry.
/// Returns true if a process was found and signalled.
async fn terminate_via_registry_pid(minion_id: &str, force: bool) -> bool {
    let mid = minion_id.to_string();
    let pid = match with_registry(move |reg| Ok(reg.get(&mid).and_then(|info| info.pid))).await {
        Ok(Some(pid)) => pid,
        _ => return false,
    };

    if !is_process_alive(pid) {
        return false;
    }

    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };

    if send_signal(pid, signal) {
        log::info!(
            "Sent {} to worker process (PID: {})",
            if force { "SIGKILL" } else { "SIGTERM" },
            pid
        );
        true
    } else {
        log::warn!("Failed to send signal to PID {}", pid);
        false
    }
}

/// Terminates Claude processes running in the specified worktree (legacy fallback).
/// Returns the number of processes terminated.
async fn terminate_claude_in_worktree(worktree_path: &Path) -> Result<usize> {
    let output = Command::new("ps")
        .args(["-eo", "pid,command"])
        .output()
        .await
        .context("Failed to run ps command")?;

    if !output.status.success() {
        log::warn!("ps command failed, cannot check for running processes");
        return Ok(0);
    }

    let ps_output = String::from_utf8_lossy(&output.stdout);
    let worktree_str = worktree_path.to_string_lossy();

    let mut terminated = 0;

    for line in ps_output.lines() {
        let line = line.trim();

        if line.starts_with("PID") || line.is_empty() {
            continue;
        }

        if !line.contains("claude") && !line.contains("gru") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let pid: u32 = match parts[0].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check if this process is associated with our worktree
        let cwd_check = Command::new("lsof")
            .args(["-p", &pid.to_string(), "-Fn"])
            .output()
            .await;

        let is_in_worktree = match cwd_check {
            Ok(output) => {
                let lsof_output = String::from_utf8_lossy(&output.stdout);
                lsof_output.contains(&*worktree_str)
            }
            Err(_) => line.contains(&*worktree_str),
        };

        if is_in_worktree {
            if send_signal(pid, libc::SIGTERM) {
                log::info!("Sent SIGTERM to process (PID: {})", pid);
                terminated += 1;
            } else {
                log::warn!("Failed to terminate process {}", pid);
            }
        }
    }

    Ok(terminated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_stop_with_invalid_id() {
        let result = handle_stop("nonexistent-minion-xyz".to_string(), false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_terminate_claude_in_worktree_nonexistent() {
        let temp_path = std::env::temp_dir().join("gru-test-nonexistent-worktree");
        let result = terminate_claude_in_worktree(&temp_path).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_send_signal_nonexistent_pid() {
        // High PID that doesn't exist
        assert!(!send_signal(4_194_304, libc::SIGTERM));
    }
}
