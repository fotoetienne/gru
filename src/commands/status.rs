use crate::agent::TokenUsage;
use crate::config;
use crate::minion_registry::{is_process_alive_with_start_time, with_registry, MinionMode};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::cmp::{max, min};

/// Combined Minion information from registry and filesystem scanning
#[derive(Debug, Clone)]
struct EnhancedMinionInfo {
    minion_id: String,
    repo: String,
    issue: Option<u64>,
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
    is_archived: bool,
}

/// Intermediate Minion data extracted from registry (without expensive status checks)
#[derive(Debug, Clone)]
struct BasicMinionData {
    minion_id: String,
    repo: String,
    issue: Option<u64>,
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
    archived_at: Option<DateTime<Utc>>,
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

/// Gets the current branch name from a worktree by reading `.git/HEAD` directly.
///
/// This avoids spawning a git subprocess (~100ms each), reducing Phase 2 time
/// from O(N × 100ms) to O(N × file_read_time) for N minions.
///
/// Returns the actual branch name, or a placeholder for special cases:
/// - "(detached)" if HEAD is detached (commit hash instead of ref)
/// - "{branch} (!)" if the branch differs from what was registered
/// - "(error)" if the HEAD file cannot be read or parsed
fn get_current_branch(worktree_path: &std::path::Path, registry_branch: &str) -> String {
    let dot_git = worktree_path.join(".git");

    // Determine the path to the HEAD file.
    // In a git worktree (which all Minions use), `.git` is a file containing
    // "gitdir: /path/to/.git/worktrees/branch-name". The actual HEAD lives
    // inside that gitdir directory.
    let head_path = if dot_git.is_file() {
        // Linked worktree: resolve the gitdir pointer.
        match std::fs::read_to_string(&dot_git) {
            Ok(content) => {
                let trimmed = content.trim();
                match trimmed.strip_prefix("gitdir: ") {
                    Some(gitdir) => {
                        let gitdir_path = if std::path::Path::new(gitdir).is_absolute() {
                            std::path::PathBuf::from(gitdir)
                        } else {
                            worktree_path.join(gitdir)
                        };
                        gitdir_path.join("HEAD")
                    }
                    None => return "(error)".to_string(),
                }
            }
            Err(_) => return "(error)".to_string(),
        }
    } else if dot_git.is_dir() {
        // Root repository (non-worktree checkout).
        dot_git.join("HEAD")
    } else {
        return "(error)".to_string();
    };

    // Parse HEAD: "ref: refs/heads/<branch>" or a bare commit hash (detached).
    match std::fs::read_to_string(&head_path) {
        Ok(content) => {
            let trimmed = content.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                if branch == registry_branch {
                    branch.to_string()
                } else {
                    format!("{} (!)", branch)
                }
            } else if trimmed.is_empty() {
                "(error)".to_string()
            } else {
                // Bare commit hash → detached HEAD.
                "(detached)".to_string()
            }
        }
        Err(_) => "(error)".to_string(),
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

/// Maximum column width before truncation with ellipsis.
const MAX_COL_WIDTH: usize = 40;

/// Truncates a string to `max_width` characters, appending `…` if it exceeds the limit.
fn truncate(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max_width {
        s.to_string()
    } else if max_width == 1 {
        "\u{2026}".to_string()
    } else {
        let cut: String = s.chars().take(max_width - 1).collect();
        format!("{cut}\u{2026}")
    }
}

/// Returns the character width for a column: at least `min_width`, at most `MAX_COL_WIDTH`,
/// and wide enough to fit the header and all values (measured in characters).
fn col_width(header: &str, values: impl Iterator<Item = usize>, min_width: usize) -> usize {
    let data_max = values.fold(header.chars().count(), max);
    min(max(data_max, min_width), MAX_COL_WIDTH)
}

/// Prints the normal (non-verbose) status table with dynamic column widths.
fn print_normal_table(minions: &[EnhancedMinionInfo]) {
    // Pre-compute display strings for columns that need formatting
    let issue_displays: Vec<String> = minions
        .iter()
        .map(|m| m.issue.map_or("-".to_string(), |n| format!("#{}", n)))
        .collect();
    let pr_displays: Vec<String> = minions
        .iter()
        .map(|m| {
            m.pr.as_ref()
                .map(|pr| format!("#{}", pr))
                .unwrap_or_else(|| "-".to_string())
        })
        .collect();
    let tokens_displays: Vec<String> = minions
        .iter()
        .map(|m| {
            m.token_usage
                .as_ref()
                .map(|u| u.display_compact())
                .unwrap_or_else(|| "-".to_string())
        })
        .collect();

    // Calculate column widths dynamically (use chars().count() for correct Unicode widths)
    let w_minion = col_width(
        "MINION",
        minions.iter().map(|m| m.minion_id.chars().count()),
        6,
    );
    let w_agent = col_width(
        "AGENT",
        minions.iter().map(|m| m.agent_name.chars().count()),
        5,
    );
    let w_repo = col_width("REPO", minions.iter().map(|m| m.repo.chars().count()), 4);
    let w_issue = col_width("ISSUE", issue_displays.iter().map(|s| s.chars().count()), 5);
    let w_task = col_width("TASK", minions.iter().map(|m| m.task.chars().count()), 4);
    let w_pr = col_width("PR", pr_displays.iter().map(|s| s.chars().count()), 2);
    let w_branch = col_width(
        "BRANCH",
        minions.iter().map(|m| m.branch.chars().count()),
        6,
    );
    let w_mode = col_width(
        "MODE",
        minions.iter().map(|m| m.mode_display.chars().count()),
        4,
    );
    let w_uptime = col_width(
        "UPTIME",
        minions.iter().map(|m| m.uptime.chars().count()),
        6,
    );

    // Print header
    println!(
        "{:<w_minion$} {:<w_agent$} {:<w_repo$} {:<w_issue$} {:<w_task$} {:<w_pr$} {:<w_branch$} {:<w_mode$} {:<w_uptime$} TOKENS",
        "MINION", "AGENT", "REPO", "ISSUE", "TASK", "PR", "BRANCH", "MODE", "UPTIME",
    );

    // Print rows
    for (i, minion) in minions.iter().enumerate() {
        println!(
            "{:<w_minion$} {:<w_agent$} {:<w_repo$} {:<w_issue$} {:<w_task$} {:<w_pr$} {:<w_branch$} {:<w_mode$} {:<w_uptime$} {}",
            truncate(&minion.minion_id, w_minion),
            truncate(&minion.agent_name, w_agent),
            truncate(&minion.repo, w_repo),
            truncate(&issue_displays[i], w_issue),
            truncate(&minion.task, w_task),
            truncate(&pr_displays[i], w_pr),
            truncate(&minion.branch, w_branch),
            truncate(&minion.mode_display, w_mode),
            truncate(&minion.uptime, w_uptime),
            tokens_displays[i],
        );
    }
}

/// Prints the verbose status table with dynamic column widths.
fn print_verbose_table(minions: &[EnhancedMinionInfo]) {
    // Pre-compute display strings
    let issue_displays: Vec<String> = minions
        .iter()
        .map(|m| m.issue.map_or("-".to_string(), |n| format!("#{}", n)))
        .collect();
    let pid_displays: Vec<String> = minions
        .iter()
        .map(|m| {
            m.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        })
        .collect();

    // Calculate column widths dynamically (use chars().count() for correct Unicode widths)
    let w_id = col_width("ID", minions.iter().map(|m| m.minion_id.chars().count()), 2);
    let w_issue = col_width("ISSUE", issue_displays.iter().map(|s| s.chars().count()), 5);
    let w_agent = col_width(
        "AGENT",
        minions.iter().map(|m| m.agent_name.chars().count()),
        5,
    );
    let w_mode = col_width(
        "MODE",
        minions.iter().map(|m| m.mode_display.chars().count()),
        4,
    );
    let w_session = col_width(
        "SESSION ID",
        minions.iter().map(|m| m.session_id.chars().count()),
        10,
    );
    let w_pid = col_width("PID", pid_displays.iter().map(|s| s.chars().count()), 3);

    // Print header
    println!(
        "{:<w_id$} {:<w_issue$} {:<w_agent$} {:<w_mode$} {:<w_session$} {:<w_pid$} PATH",
        "ID", "ISSUE", "AGENT", "MODE", "SESSION ID", "PID",
    );

    // Print rows
    for (i, minion) in minions.iter().enumerate() {
        println!(
            "{:<w_id$} {:<w_issue$} {:<w_agent$} {:<w_mode$} {:<w_session$} {:<w_pid$} {}",
            truncate(&minion.minion_id, w_id),
            truncate(&issue_displays[i], w_issue),
            truncate(&minion.agent_name, w_agent),
            truncate(&minion.mode_display, w_mode),
            truncate(&minion.session_id, w_session),
            truncate(&pid_displays[i], w_pid),
            minion.worktree_path,
        );
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
pub(crate) async fn handle_status(
    id: Option<String>,
    verbose: bool,
    show_all: bool,
) -> Result<i32> {
    // Spawn non-blocking version check in background (runs while we do real work)
    let mut version_rx = crate::version_check::spawn_version_check();

    // Prune stale entries using the shared three-phase approach that checks
    // GitHub PR status before removing entries with open PRs.
    // Non-fatal: a transient GitHub API error should not prevent status display.
    if let Err(e) = crate::minion_registry::prune_stale_entries().await {
        log::warn!("Failed to prune stale registry entries: {:#}", e);
    }

    // Auto-archive stopped minions whose PR is merged or issue is closed.
    // Non-fatal: a transient GitHub API error should not prevent status display.
    if let Err(e) = crate::minion_registry::auto_archive_completed_minions().await {
        log::warn!("Failed to auto-archive completed minions: {:#}", e);
    }

    // Auto-archive stopped Minions with no archivable signal after TTL.
    // Non-fatal: transient errors should not prevent status display.
    let ttl_hours = config::try_load_config()
        .map(|c| c.daemon.archive_ttl_hours)
        .unwrap_or(config::DEFAULT_ARCHIVE_TTL_HOURS);
    if let Err(e) = crate::minion_registry::auto_archive_stopped_minions(ttl_hours).await {
        log::warn!("Failed to auto-archive stopped Minions: {:#}", e);
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
                    archived_at: info.archived_at,
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
                    let is_archived = basic.archived_at.is_some();
                    EnhancedMinionInfo {
                        minion_id: basic.minion_id,
                        repo: basic.repo,
                        issue: basic.issue,
                        task: basic.task,
                        pr: basic.pr,
                        branch: basic.branch,
                        is_running: false,
                        mode_display: if is_archived {
                            "archived".to_string()
                        } else {
                            "stale".to_string()
                        },
                        uptime,
                        token_usage: basic.token_usage,
                        session_id: basic.session_id,
                        pid: None,
                        worktree_path: basic.worktree.display().to_string(),
                        agent_name: basic.agent_name,
                        is_stale: true,
                        is_archived,
                    }
                } else {
                    let (is_running, mode_display) =
                        format_mode_display(basic.pid, basic.pid_start_time, &basic.mode);
                    // Treat a minion as archived only when it has an archived_at
                    // timestamp and is not currently running.
                    let is_archived = basic.archived_at.is_some() && !is_running;
                    let mode_display = if is_archived {
                        "archived".to_string()
                    } else {
                        mode_display
                    };
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
                        is_archived,
                    }
                }
            })
            .collect::<Vec<EnhancedMinionInfo>>()
    })
    .await
    .context("Failed to complete status checks for minions")?;

    // Print version notification if already available — never waits.
    // Placed here (before early returns) so the notification is shown even with no minions.
    crate::version_check::print_if_ready(&mut version_rx);

    // Filter by ID if provided
    if let Some(filter_id) = id {
        // Try as issue/PR number first (most common case)
        let filtered: Vec<_> = if let Ok(num) = filter_id.parse::<u64>() {
            minions
                .iter()
                .filter(|m| {
                    m.issue == Some(num)
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

    // Count archived entries before filtering them out
    let archived_count = minions.iter().filter(|m| m.is_archived).count();

    // Hide archived entries unless --all is passed
    if !show_all {
        minions.retain(|m| !m.is_archived);
    }

    if minions.is_empty() {
        if archived_count > 0 {
            println!(
                "No active Minions ({} archived — use --all to show)",
                archived_count
            );
        } else {
            println!("No active Minions");
        }
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

    // Print table with dynamic column widths
    if verbose {
        print_verbose_table(&minions);
    } else {
        print_normal_table(&minions);
    }

    println!();
    let stale_count = minions
        .iter()
        .filter(|m| m.is_stale && !m.is_archived)
        .count();
    // Build footer with counts
    let mut footer_parts = Vec::new();
    if stale_count > 0 {
        footer_parts.push(format!("{} stale - run 'gru clean' to remove", stale_count));
    }
    // Show archived hint when entries are hidden (not using --all)
    if !show_all && archived_count > 0 {
        footer_parts.push(format!("{} archived — use --all to show", archived_count));
    }

    if footer_parts.is_empty() {
        println!("{} Minion(s) found", minions.len());
    } else {
        println!(
            "{} Minion(s) found ({})",
            minions.len(),
            footer_parts.join(", ")
        );
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
        let result = handle_status(None, false, false).await;
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

    // --- get_current_branch tests ---

    #[test]
    fn test_get_current_branch_regular_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        std::fs::create_dir(&dot_git).unwrap();
        std::fs::write(dot_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        assert_eq!(get_current_branch(tmp.path(), "main"), "main");
        assert_eq!(get_current_branch(tmp.path(), "other"), "main (!)");
    }

    #[test]
    fn test_get_current_branch_detached_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        std::fs::create_dir(&dot_git).unwrap();
        std::fs::write(
            dot_git.join("HEAD"),
            "abc123def456abc123def456abc123def456abc1\n",
        )
        .unwrap();

        assert_eq!(get_current_branch(tmp.path(), "main"), "(detached)");
    }

    #[test]
    fn test_get_current_branch_worktree_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tempfile::tempdir().unwrap();

        // Write HEAD to the gitdir location (simulates a bare repo worktree entry)
        std::fs::write(
            gitdir.path().join("HEAD"),
            "ref: refs/heads/minion/issue-42-M001\n",
        )
        .unwrap();

        // .git in the worktree is a file pointing to the gitdir
        std::fs::write(
            tmp.path().join(".git"),
            format!("gitdir: {}\n", gitdir.path().display()),
        )
        .unwrap();

        assert_eq!(
            get_current_branch(tmp.path(), "minion/issue-42-M001"),
            "minion/issue-42-M001"
        );
    }

    #[test]
    fn test_get_current_branch_missing_dot_git() {
        let tmp = tempfile::tempdir().unwrap();
        // No .git file or directory
        assert_eq!(get_current_branch(tmp.path(), "main"), "(error)");
    }

    #[test]
    fn test_get_current_branch_bad_gitdir_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        // .git file with malformed content (no "gitdir: " prefix)
        std::fs::write(tmp.path().join(".git"), "not a valid gitdir pointer\n").unwrap();
        assert_eq!(get_current_branch(tmp.path(), "main"), "(error)");
    }

    // --- truncate tests ---

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_width() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_over_width() {
        assert_eq!(
            truncate("fotoetienne/jeapi-cli", 20),
            "fotoetienne/jeapi-c\u{2026}"
        );
    }

    #[test]
    fn test_truncate_width_one() {
        assert_eq!(truncate("hello", 1), "\u{2026}");
    }

    #[test]
    fn test_truncate_width_zero() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn test_truncate_multibyte_chars() {
        // Should not panic on multi-byte UTF-8 characters
        assert_eq!(truncate("org/repoé-name", 10), "org/repoé\u{2026}");
    }

    // --- col_width tests ---

    #[test]
    fn test_col_width_uses_header_when_wider() {
        let w = col_width("BRANCH", vec![3_usize, 4].into_iter(), 4);
        assert_eq!(w, 6); // "BRANCH".len() == 6
    }

    #[test]
    fn test_col_width_uses_data_when_wider() {
        let w = col_width("ID", vec![10_usize, 15].into_iter(), 2);
        assert_eq!(w, 15);
    }

    #[test]
    fn test_col_width_capped_at_max() {
        let w = col_width("X", vec![100_usize].into_iter(), 2);
        assert_eq!(w, MAX_COL_WIDTH);
    }

    #[test]
    fn test_col_width_respects_min_width() {
        let w = col_width("X", vec![1_usize].into_iter(), 8);
        assert_eq!(w, 8);
    }
}
