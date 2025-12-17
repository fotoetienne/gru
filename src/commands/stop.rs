use crate::minion_registry::MinionRegistry;
use crate::minion_resolver;
use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

/// Handles the stop command to terminate a running Minion
/// Returns 0 on success, 1 on error
///
/// This command terminates a running Minion by:
/// 1. Resolving the minion ID to find the worktree
/// 2. Sending SIGTERM to any Claude processes running in that worktree
/// 3. Updating the minion registry to mark it as "stopped"
///
/// The ID argument supports smart resolution:
/// - Exact minion ID (e.g., M0tk)
/// - With M prefix (e.g., 12 -> M12)
/// - Issue number (finds minion working on that issue)
/// - PR number (finds minion via linked issue)
pub async fn handle_stop(id: String) -> Result<i32> {
    // Resolve the minion ID to get the worktree path
    let minion = minion_resolver::resolve_minion(&id).await?;

    println!("🛑 Stopping Minion {}...", minion.minion_id);
    println!("📂 Workspace: {}", minion.worktree_path.display());

    // Check if worktree exists
    if !minion.worktree_path.exists() {
        println!(
            "⚠️  Worktree no longer exists: {}",
            minion.worktree_path.display()
        );
        println!("   Removing from registry...");

        // Remove from registry since worktree is gone
        let minion_id = minion.minion_id.clone();
        tokio::task::spawn_blocking(move || {
            let mut registry = MinionRegistry::load(None)?;
            registry.remove(&minion_id)
        })
        .await
        .context("Failed to spawn blocking task for registry removal")??;

        println!("✅ Minion {} removed from registry", minion.minion_id);
        return Ok(0);
    }

    // Try to terminate Claude processes running in this worktree
    let terminated = terminate_claude_in_worktree(&minion.worktree_path).await?;

    if terminated > 0 {
        println!(
            "🔪 Terminated {} Claude process(es) in worktree",
            terminated
        );
    } else {
        println!("ℹ️  No active Claude processes found in worktree");
    }

    // Update registry to mark minion as stopped
    let minion_id = minion.minion_id.clone();
    let update_result = tokio::task::spawn_blocking(move || {
        let mut registry = MinionRegistry::load(None)?;
        registry.update(&minion_id, |info| {
            info.status = "stopped".to_string();
        })
    })
    .await
    .context("Failed to spawn blocking task for registry update")?;

    match update_result {
        Ok(()) => {
            println!("📝 Updated registry: status = stopped");
        }
        Err(e) => {
            // Minion might not be in registry (old worktree), that's okay
            log::warn!("⚠️  Could not update registry: {}", e);
        }
    }

    println!("\n✅ Minion {} stopped", minion.minion_id);
    println!(
        "💡 Worktree preserved at: {}",
        minion.worktree_path.display()
    );
    println!("   To clean up, run: gru clean");

    Ok(0)
}

/// Terminates Claude processes running in the specified worktree
/// Returns the number of processes terminated
async fn terminate_claude_in_worktree(worktree_path: &Path) -> Result<usize> {
    // Get list of claude processes with their working directories
    // We use `ps` with format options to get PID and working directory
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

    // Find claude processes that might be running in this worktree
    // We look for processes with 'claude' in the command that also reference the worktree
    for line in ps_output.lines() {
        let line = line.trim();

        // Skip header line
        if line.starts_with("PID") || line.is_empty() {
            continue;
        }

        // Check if this is a claude process
        if !line.contains("claude") {
            continue;
        }

        // Parse PID from the line (first column)
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let pid_str = parts[0];
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check if this process is associated with our worktree
        // We do this by checking if the process's current directory matches
        let cwd_check = Command::new("lsof")
            .args(["-p", &pid.to_string(), "-Fn"])
            .output()
            .await;

        let is_in_worktree = match cwd_check {
            Ok(output) => {
                let lsof_output = String::from_utf8_lossy(&output.stdout);
                lsof_output.contains(&*worktree_str)
            }
            Err(_) => {
                // lsof might not be available or process might have exited
                // Fall back to checking if the session ID matches the minion ID
                line.contains(&*worktree_str)
            }
        };

        if is_in_worktree {
            // Send SIGTERM to the process
            let kill_result = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output()
                .await;

            match kill_result {
                Ok(output) if output.status.success() => {
                    log::info!("Sent SIGTERM to claude process (PID: {})", pid);
                    terminated += 1;
                }
                Ok(_) => {
                    log::warn!("Failed to terminate process {}", pid);
                }
                Err(e) => {
                    log::warn!("Failed to send signal to process {}: {}", pid, e);
                }
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
        // Test that handle_stop returns an error for an invalid ID
        let result = handle_stop("nonexistent-minion-xyz".to_string()).await;
        assert!(result.is_err());

        // Verify the error message suggests using gru status
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_terminate_claude_in_worktree_nonexistent() {
        // Test with a non-existent directory - should return 0 (no processes found)
        let temp_path = std::env::temp_dir().join("gru-test-nonexistent-worktree");
        let result = terminate_claude_in_worktree(&temp_path).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }
}
