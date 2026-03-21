use crate::agent::TokenUsage;
use crate::minion_registry::{is_process_alive_with_start_time, with_registry, MinionMode};
use anyhow::{Context, Result};

/// Combined Minion information from registry and filesystem scanning
#[derive(Debug, Clone)]
struct EnhancedMinionInfo {
    minion_id: String,
    repo: String,
    issue: u64,
    task: String,
    pr: Option<String>,
    branch: String,
    is_running: bool,
    mode_display: String,
    uptime: String,
    token_usage: Option<TokenUsage>,
    session_id: String,
    pid: Option<u32>,
    worktree_path: String,
    agent_name: String,
    is_stale: bool,
}

/// Intermediate Minion data extracted from registry (without expensive status checks)
#[derive(Debug, Clone)]
struct BasicMinionData {
    minion_id: String,
    repo: String,
    issue: u64,
    task: String,
    pr: Option<String>,
    branch: String,
    started_at: chrono::DateTime<chrono::Utc>,
    worktree: std::path::PathBuf,
    pid: Option<u32>,
    pid_start_time: Option<i64>,
    mode: MinionMode,
    session_id: String,
    token_usage: Option<TokenUsage>,
    agent_name: String,
    is_stale: bool,
}

/// Calculates uptime from a timestamp
fn calculate_uptime(started_at: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(started_at);

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();

    if days > 0 {
        format!("{}d", days)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        "< 1m".to_string()
    }
}

/// Gets the current branch name from a worktree
/// Returns the actual branch name, or a placeholder for special cases:
/// - "(detached)" if HEAD is detached
/// - "{branch} (!)" if the branch differs from what was registered (e.g., changed or registry is stale)
/// - "(error)" if git command fails
fn get_current_branch(worktree_path: &std::path::Path, registry_branch: &str) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("branch")
        .arg("--show-current")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if branch.is_empty() {
                // Empty output means detached HEAD
                "(detached)".to_string()
            } else if branch != registry_branch {
                // Branch changed from what was registered
                format!("{} (!)", branch)
            } else {
                branch
            }
        }
        _ => "(error)".to_string(),
    }
}

/// Determines whether the process is running and the display mode string
fn format_mode_display(
    pid: Option<u32>,
    pid_start_time: Option<i64>,
    mode: &MinionMode,
) -> (bool, String) {
    match pid {
        Some(pid) if is_process_alive_with_start_time(pid, pid_start_time) => {
            let display = match mode {
                MinionMode::Autonomous => "running (autonomous)".to_string(),
                MinionMode::Interactive => "running (interactive)".to_string(),
                MinionMode::Stopped => {
                    log::warn!("Minion has live PID {} but mode is Stopped", pid);
                    "running (unknown)".to_string()
                }
            };
            (true, display)
        }
        _ => (false, "stopped".to_string()),
    }
}

/// Handles the status command by displaying active Minions
/// Optionally filters by a specific ID (minion ID, issue number, or PR number)
///
/// # Approach
///
/// To minimize registry lock hold time and prevent blocking other minions:
///
/// **Pruning (async):** Remove stale entries (missing worktrees) via
/// [`crate::minion_registry::prune_stale_entries`], which checks GitHub PR
/// status before removing entries with open PRs. Non-fatal on error.
///
/// **Phase 1 (with lock):** Load registry, detect dead processes, extract basic
/// minion data, then release lock by dropping the registry.
///
/// **Phase 2 (no lock):** Perform PID-based liveness checks and git branch
/// detection for each worktree.
///
/// This ensures the lock is only held for the minimum time needed to read/write
/// the registry file, not for I/O operations.
pub(crate) async fn handle_status(id: Option<String>, verbose: bool) -> Result<i32> {
    // Prune stale entries using the shared two-phase approach that checks
    // GitHub PR status before removing entries with open PRs.
    // Non-fatal: a transient GitHub API error should not prevent status display.
    if let Err(e) = crate::minion_registry::prune_stale_entries().await {
        log::warn!("Failed to prune stale registry entries: {:#}", e);
    }

    // Phase 1: Load registry and perform remaining cleanup (with lock held)
    let basic_minions = with_registry(|registry| {
        // Detect dead processes and update registry.
        // Only on Unix where process liveness checks are available.
        // On non-Unix, is_process_alive always returns false which would incorrectly
        // mark all minions as dead.
        // Note: is_running() calls kill(pid, 0) plus a fast proc_pidinfo/procfs lookup
        // to detect PID reuse. Both are fast kernel calls (microseconds each), so running
        // them under the registry lock is acceptable (unlike git operations in Phase 2).
        #[cfg(unix)]
        {
            let registry_minions = registry.list();
            for (minion_id, info) in &registry_minions {
                if info.pid.is_some() && !info.is_running() {
                    registry.update(minion_id, |info| {
                        info.mode = MinionMode::Stopped;
                        info.clear_pid();
                    })?;
                }
            }
        }

        // Get updated registry after cleanup
        let registry_minions = registry.list();

        // Extract basic data without expensive operations
        let basic: Vec<BasicMinionData> = registry_minions
            .into_iter()
            .map(|(minion_id, info)| {
                let is_stale = !info.worktree.exists();
                BasicMinionData {
                    minion_id,
                    repo: info.repo,
                    issue: info.issue,
                    task: info.command,
                    pr: info.pr,
                    branch: info.branch,
                    started_at: info.started_at,
                    worktree: info.worktree,
                    pid: info.pid,
                    pid_start_time: info.pid_start_time,
                    mode: info.mode,
                    session_id: info.session_id,
                    token_usage: info.token_usage,
                    agent_name: info.agent_name,
                    is_stale,
                }
            })
            .collect();

        Ok(basic)
        // Registry is dropped here, releasing the lock
    })
    .await?;

    // Phase 2: Perform status checks and git operations (no lock held)
    let mut minions = tokio::task::spawn_blocking(move || {
        basic_minions
            .into_iter()
            .map(|basic| {
                if basic.is_stale || !basic.worktree.exists() {
                    // Stale entry: worktree doesn't exist (detected in Phase 1,
                    // or removed between Phase 1 and Phase 2). Skip git operations.
                    let uptime = calculate_uptime(basic.started_at);
                    EnhancedMinionInfo {
                        minion_id: basic.minion_id,
                        repo: basic.repo,
                        issue: basic.issue,
                        task: basic.task,
                        pr: basic.pr,
                        branch: basic.branch,
                        is_running: false,
                        mode_display: "stale".to_string(),
                        uptime,
                        token_usage: basic.token_usage,
                        session_id: basic.session_id,
                        pid: None,
                        worktree_path: basic.worktree.display().to_string(),
                        agent_name: basic.agent_name,
                        is_stale: true,
                    }
                } else {
                    let (is_running, mode_display) =
                        format_mode_display(basic.pid, basic.pid_start_time, &basic.mode);
                    let uptime = calculate_uptime(basic.started_at);
                    // Get current branch from checkout path (checks for detached HEAD, branch changes, etc.)
                    let checkout_path = crate::workspace::resolve_checkout_path(&basic.worktree);
                    let branch = get_current_branch(&checkout_path, &basic.branch);
                    let worktree_path = checkout_path.display().to_string();

                    EnhancedMinionInfo {
                        minion_id: basic.minion_id,
                        repo: basic.repo,
                        issue: basic.issue,
                        task: basic.task,
                        pr: basic.pr,
                        branch,
                        is_running,
                        mode_display,
                        uptime,
                        token_usage: basic.token_usage,
                        session_id: basic.session_id,
                        pid: basic.pid,
                        worktree_path,
                        agent_name: basic.agent_name,
                        is_stale: false,
                    }
                }
            })
            .collect::<Vec<EnhancedMinionInfo>>()
    })
    .await
    .context("Failed to complete status checks for minions")?;

    // Filter by ID if provided
    if let Some(filter_id) = id {
        // Try as issue/PR number first (most common case)
        let filtered: Vec<_> = if let Ok(num) = filter_id.parse::<u64>() {
            minions
                .iter()
                .filter(|m| {
                    m.issue == num
                        || m.pr.as_ref().and_then(|pr| pr.parse::<u64>().ok()) == Some(num)
                })
                .cloned()
                .collect()
        } else {
            // Try as minion ID (exact or with M prefix)
            minions
                .iter()
                .filter(|m| m.minion_id == filter_id || m.minion_id == format!("M{}", filter_id))
                .cloned()
                .collect()
        };

        if filtered.is_empty() {
            log::warn!("No Minions found matching '{}'", filter_id);
            return Ok(1);
        }
        minions = filtered;
    }

    if minions.is_empty() {
        println!("No active Minions");
        return Ok(0);
    }

    // Sort by: running first, then stopped, then stale last; within each group by minion_id
    minions.sort_by(|a, b| match (a.is_stale, b.is_stale) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => match (a.is_running, b.is_running) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.minion_id.cmp(&b.minion_id),
        },
    });

    // Print table header
    if verbose {
        println!(
            "{:<8} {:<8} {:<8} {:<22} {:<38} {:<8} PATH",
            "ID", "ISSUE", "AGENT", "MODE", "SESSION ID", "PID"
        );
    } else {
        println!(
            "{:<8} {:<8} {:<20} {:<8} {:<10} {:<8} {:<30} {:<22} {:<8} TOKENS",
            "MINION", "AGENT", "REPO", "ISSUE", "TASK", "PR", "BRANCH", "MODE", "UPTIME"
        );
    }

    // Print each minion
    for minion in &minions {
        if verbose {
            let issue_display = format!("#{}", minion.issue);
            let pid_display = minion
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());

            println!(
                "{:<8} {:<8} {:<8} {:<22} {:<38} {:<8} {}",
                minion.minion_id,
                issue_display,
                minion.agent_name,
                minion.mode_display,
                minion.session_id,
                pid_display,
                minion.worktree_path
            );
        } else {
            let issue_display = format!("#{}", minion.issue);
            let pr_display = minion
                .pr
                .as_ref()
                .map(|pr| format!("#{}", pr))
                .unwrap_or_else(|| "-".to_string());
            let tokens_display = minion
                .token_usage
                .as_ref()
                .map(|u| u.display_compact())
                .unwrap_or_else(|| "-".to_string());

            println!(
                "{:<8} {:<8} {:<20} {:<8} {:<10} {:<8} {:<30} {:<22} {:<8} {}",
                minion.minion_id,
                minion.agent_name,
                minion.repo,
                issue_display,
                minion.task,
                pr_display,
                minion.branch,
                minion.mode_display,
                minion.uptime,
                tokens_display
            );
        }
    }

    println!();
    let stale_count = minions.iter().filter(|m| m.is_stale).count();
    if stale_count > 0 {
        println!(
            "{} Minion(s) found ({} stale - run 'gru clean' to remove)",
            minions.len(),
            stale_count
        );
    } else {
        println!("{} Minion(s) found", minions.len());
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Integration test - performs real I/O and git operations
    async fn test_handle_status_no_filter() {
        // This test verifies that handle_status succeeds without filtering
        let result = handle_status(None, false).await;
        assert!(result.is_ok());
    }

    // --- calculate_uptime tests ---

    #[test]
    fn test_calculate_uptime_seconds() {
        let started = chrono::Utc::now() - chrono::Duration::seconds(30);
        assert_eq!(calculate_uptime(started), "< 1m");
    }

    #[test]
    fn test_calculate_uptime_minutes() {
        let started = chrono::Utc::now() - chrono::Duration::minutes(5);
        assert_eq!(calculate_uptime(started), "5m");
    }

    #[test]
    fn test_calculate_uptime_hours() {
        let started = chrono::Utc::now() - chrono::Duration::hours(3);
        assert_eq!(calculate_uptime(started), "3h");
    }

    #[test]
    fn test_calculate_uptime_days() {
        let started = chrono::Utc::now() - chrono::Duration::days(2);
        assert_eq!(calculate_uptime(started), "2d");
    }

    #[test]
    fn test_calculate_uptime_just_now() {
        let started = chrono::Utc::now();
        assert_eq!(calculate_uptime(started), "< 1m");
    }

    #[test]
    fn test_calculate_uptime_boundary_59_minutes() {
        let started = chrono::Utc::now() - chrono::Duration::minutes(59);
        assert_eq!(calculate_uptime(started), "59m");
    }

    #[test]
    fn test_calculate_uptime_boundary_60_minutes() {
        let started = chrono::Utc::now() - chrono::Duration::minutes(60);
        assert_eq!(calculate_uptime(started), "1h");
    }

    #[test]
    fn test_calculate_uptime_boundary_23_hours() {
        let started = chrono::Utc::now() - chrono::Duration::hours(23);
        assert_eq!(calculate_uptime(started), "23h");
    }

    #[test]
    fn test_calculate_uptime_boundary_24_hours() {
        let started = chrono::Utc::now() - chrono::Duration::hours(24);
        assert_eq!(calculate_uptime(started), "1d");
    }

    #[test]
    fn test_calculate_uptime_future_timestamp() {
        // Handles clock skew: started_at in the future results in negative duration,
        // which makes num_minutes/hours/days return 0 or negative, falling through to "< 1m"
        let started = chrono::Utc::now() + chrono::Duration::minutes(5);
        assert_eq!(calculate_uptime(started), "< 1m");
    }

    // --- format_mode_display tests ---

    #[test]
    fn test_format_mode_display_no_pid() {
        let (is_running, display) = format_mode_display(None, None, &MinionMode::Autonomous);
        assert!(!is_running);
        assert_eq!(display, "stopped");
    }

    #[test]
    fn test_format_mode_display_autonomous_alive() {
        // Our own PID should be alive
        let pid = std::process::id();
        let (is_running, display) = format_mode_display(Some(pid), None, &MinionMode::Autonomous);
        assert!(is_running);
        assert_eq!(display, "running (autonomous)");
    }

    #[test]
    fn test_format_mode_display_interactive_alive() {
        let pid = std::process::id();
        let (is_running, display) = format_mode_display(Some(pid), None, &MinionMode::Interactive);
        assert!(is_running);
        assert_eq!(display, "running (interactive)");
    }

    #[test]
    fn test_format_mode_display_dead_pid() {
        // Use a very high PID that's still valid as i32 (positive) but almost certainly
        // doesn't exist. Avoid u32::MAX which wraps to -1 as i32, causing kill(-1,0)
        // to signal all processes.
        let (is_running, display) =
            format_mode_display(Some(i32::MAX as u32), None, &MinionMode::Autonomous);
        assert!(!is_running);
        assert_eq!(display, "stopped");
    }

    #[test]
    fn test_format_mode_display_stopped_mode_alive_pid() {
        // Edge case: PID alive but mode is Stopped (shouldn't normally happen)
        let pid = std::process::id();
        let (is_running, display) = format_mode_display(Some(pid), None, &MinionMode::Stopped);
        assert!(is_running);
        assert_eq!(display, "running (unknown)");
    }
}
