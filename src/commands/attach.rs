use crate::minion_resolver;
use anyhow::{Context, Result};

/// Handles the attach command to attach to a Minion's Claude session
/// Returns 0 on success, 1 on error
///
/// This command attaches to a running or paused Minion's session,
/// allowing you to see live output and send keyboard input.
///
/// It is functionally equivalent to:
/// ```bash
/// cd $(gru path <id>) && claude -r
/// ```
///
/// The ID argument supports smart resolution (same as gru path):
/// 1. Try as exact minion ID (e.g., M0tk)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
///
/// On Unix systems, this command replaces the current process with claude.
/// On Windows, it spawns claude and waits for completion.
///
/// Note: This command is identical to `gru resume` - both attach to the
/// same Claude session interactively. The `attach` name is used for
/// consistency with documentation and expected UX.
pub async fn handle_attach(id: String) -> Result<i32> {
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

    println!("🔌 Attaching to Minion {}...", minion.minion_id);
    println!("📂 Workspace: {}", minion.worktree_path.display());

    // Unix: exec() replaces the current process
    // Windows: spawn() creates a new process and waits for it
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("claude")
            .arg("-r")
            .current_dir(&minion.worktree_path)
            .exec(); // Replaces current process

        // If we reach here, exec failed
        Err(err).context(
            "Failed to exec claude. Is Claude CLI installed and in your PATH?\n\
             See: https://claude.com/claude-code",
        )
    }

    #[cfg(not(unix))]
    {
        // On Windows, spawn the process and wait for it to complete
        let status = std::process::Command::new("claude")
            .arg("-r")
            .current_dir(&minion.worktree_path)
            .status()
            .context(
                "Failed to run claude. Is Claude CLI installed and in your PATH?\n\
                 See: https://claude.com/claude-code",
            )?;

        Ok(if status.success() { 0 } else { 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_attach_with_invalid_id() {
        // Test that handle_attach returns an error for an invalid ID
        // This verifies the minion_resolver integration works correctly
        let result = handle_attach("nonexistent-minion-xyz".to_string()).await;
        assert!(result.is_err());

        // Verify the error message suggests using gru status
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }
}
