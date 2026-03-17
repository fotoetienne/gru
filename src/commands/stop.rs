use crate::minion_registry::{is_process_alive_with_start_time, with_registry, MinionMode};
use crate::minion_resolver;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

/// Sends a signal to a process using libc::kill.
/// Returns true if the signal was delivered successfully.
#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> bool {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid_i32, signal) == 0 }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: i32) -> bool {
    false
}

/// Escapes POSIX extended regex metacharacters in a string for use with `pgrep -f`.
fn escape_regex(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '[' | ']' | '{' | '}' | '(' | ')' | '*' | '+' | '?' | '^' | '$' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Handles the stop command to terminate a running Minion.
/// Returns 0 on success, 1 on error.
///
/// Termination strategy:
/// 1. Look up PID from the minion registry (primary path for background workers)
/// 2. Fall back to pgrep scanning for legacy minions without PID in registry
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
        // Fall back to pgrep scanning for legacy minions or if PID wasn't in registry
        let terminated = terminate_claude_in_worktree(&checkout_path, force).await?;

        if terminated > 0 {
            println!(
                "🔪 Terminated {} process(es) in worktree{}",
                terminated,
                if force { " (forced)" } else { "" }
            );
        } else {
            println!("ℹ️  No active processes found for minion");
        }
    }

    // Update registry to mark minion as stopped
    let minion_id = minion.minion_id.clone();
    match with_registry(move |registry| {
        registry.update(&minion_id, |info| {
            info.status = "stopped".to_string();
            info.clear_pid();
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
    let (pid, pid_start_time) = match with_registry(move |reg| {
        Ok(reg.get(&mid).map(|info| (info.pid, info.pid_start_time)))
    })
    .await
    {
        Ok(Some((Some(pid), start_time))) => (pid, start_time),
        _ => return false,
    };

    if !is_process_alive_with_start_time(pid, pid_start_time) {
        return false;
    }

    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };

        if send_signal(pid, signal) {
            log::info!(
                "Sent {} to worker process (PID: {})",
                if force { "SIGKILL" } else { "SIGTERM" },
                pid
            );
            return true;
        }
        log::warn!("Failed to send signal to PID {}", pid);
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, force);
        log::warn!("Process termination not supported on this platform");
    }

    false
}

/// Terminates Claude/Gru processes running in the specified worktree (legacy fallback).
///
/// Uses `pgrep -f` with a pattern that matches only `claude` or `gru` processes
/// whose command line references the worktree path. The path is regex-escaped to
/// prevent metacharacters (e.g., `.`, `[`, `]`) from causing false matches.
///
/// Note: `pgrep -f` interprets the pattern as a POSIX extended regex. Paths containing
/// unusual characters beyond what `escape_regex` handles may still produce unexpected
/// matches, though this is unlikely with standard gru worktree paths.
///
/// Returns the number of processes terminated.
async fn terminate_claude_in_worktree(worktree_path: &Path, force: bool) -> Result<usize> {
    let escaped_path = escape_regex(&worktree_path.to_string_lossy());
    // Match only claude or gru processes referencing this worktree (either order)
    let pattern = format!(
        "(claude|gru).*{path}|{path}.*(claude|gru)",
        path = escaped_path
    );

    let output = match Command::new("pgrep").args(["-f", &pattern]).output().await {
        Ok(o) => o,
        Err(e) => {
            log::warn!("pgrep not available, cannot scan for processes: {}", e);
            return Ok(0);
        }
    };

    // pgrep exits 1 when no processes match, 2+ on error
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code == 1 {
        return Ok(0);
    } else if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!("pgrep exited with code {}: {}", exit_code, stderr.trim());
        return Ok(0);
    }

    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let signal_name = if force { "SIGKILL" } else { "SIGTERM" };
        let pgrep_output = String::from_utf8_lossy(&output.stdout);
        let mut terminated = 0;

        for line in pgrep_output.lines() {
            let pid: u32 = match line.trim().parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            if send_signal(pid, signal) {
                log::info!("Sent {} to process (PID: {})", signal_name, pid);
                terminated += 1;
            } else {
                log::warn!("Failed to send {} to process {}", signal_name, pid);
            }
        }

        Ok(terminated)
    }

    #[cfg(not(unix))]
    {
        let _ = force;
        log::warn!("Process termination not supported on this platform");
        Ok(0)
    }
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
        let temp_path = std::env::temp_dir().join("gru-stop-test-sigterm-no-match");
        let result = terminate_claude_in_worktree(&temp_path, false).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_terminate_claude_in_worktree_force() {
        let temp_path = std::env::temp_dir().join("gru-stop-test-sigkill-no-match");
        let result = terminate_claude_in_worktree(&temp_path, true).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_escape_regex_metacharacters() {
        assert_eq!(escape_regex("/tmp/foo.bar"), "/tmp/foo\\.bar");
        assert_eq!(escape_regex("path[0]"), "path\\[0\\]");
        assert_eq!(escape_regex("a(b)c"), "a\\(b\\)c");
        assert_eq!(escape_regex("no+meta*chars?"), "no\\+meta\\*chars\\?");
        // Normal worktree path should only escape dots
        assert_eq!(
            escape_regex("/Users/me/.gru/work/owner/repo/minion/issue-42-M001/checkout"),
            "/Users/me/\\.gru/work/owner/repo/minion/issue-42-M001/checkout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_send_signal_nonexistent_pid() {
        // High PID that doesn't exist
        assert!(!send_signal(4_194_304, libc::SIGTERM));
    }
}
