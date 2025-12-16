use crate::minion_registry::MinionRegistry;
use anyhow::{Context, Result};
use std::time::SystemTime;

/// Combined Minion information from registry and filesystem scanning
#[derive(Debug, Clone)]
struct EnhancedMinionInfo {
    minion_id: String,
    repo: String,
    issue: u64,
    task: String,
    pr: Option<String>,
    branch: String,
    status: String,
    uptime: String,
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

/// Determines if a Minion is Active or Idle based on git index modification time
fn determine_status(worktree_path: &std::path::Path) -> String {
    // Use git rev-parse to get the actual git directory path
    let git_dir_output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg("--git-dir")
        .output();

    let git_dir_output = match git_dir_output {
        Ok(output) if output.status.success() => output,
        _ => return "Idle".to_string(),
    };

    let git_dir = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();
    let git_index = std::path::PathBuf::from(git_dir).join("index");

    if !git_index.exists() {
        return "Idle".to_string();
    }

    let metadata = match std::fs::metadata(&git_index) {
        Ok(m) => m,
        Err(_) => return "Idle".to_string(),
    };

    let modified = match metadata.modified() {
        Ok(m) => m,
        Err(_) => return "Idle".to_string(),
    };

    let now = SystemTime::now();
    let elapsed = now.duration_since(modified).unwrap_or_default();

    // Consider active if modified within the last 5 minutes
    if elapsed.as_secs() < 300 {
        "Active".to_string()
    } else {
        "Idle".to_string()
    }
}

/// Handles the status command by displaying active Minions
/// Optionally filters by a specific ID (minion ID, issue number, or PR number)
///
/// # Two-Phase Approach
///
/// To minimize registry lock hold time and prevent blocking other minions:
///
/// **Phase 1 (with lock):** Load registry, clean up stale entries,
/// extract basic minion data, then release lock by dropping the registry.
///
/// **Phase 2 (no lock):** Perform expensive git operations via `determine_status()`
/// for each worktree to determine active/idle status.
///
/// This ensures the lock is only held for the minimum time needed to read/write
/// the registry file, not for I/O operations.
pub async fn handle_status(id: Option<String>) -> Result<i32> {
    // Phase 1: Load registry and clean up (with lock held)
    let basic_minions = tokio::task::spawn_blocking(|| {
        let mut registry = MinionRegistry::load(None)?;

        // Get all minions from registry
        let registry_minions = registry.list();

        // Check for stale entries (worktrees that no longer exist)
        let mut stale_ids = Vec::new();
        for (minion_id, info) in &registry_minions {
            if !info.worktree.exists() {
                stale_ids.push(minion_id.clone());
            }
        }

        // Remove stale entries from registry
        if !stale_ids.is_empty() {
            for minion_id in &stale_ids {
                registry.remove(minion_id)?;
            }
            eprintln!(
                "🗑️  Removed {} stale Minion(s) from registry",
                stale_ids.len()
            );
        }

        // Get updated registry after cleanup
        let registry_minions = registry.list();

        // Extract basic data without expensive operations
        let basic: Vec<BasicMinionData> = registry_minions
            .into_iter()
            .map(|(minion_id, info)| BasicMinionData {
                minion_id,
                repo: info.repo,
                issue: info.issue,
                task: info.command,
                pr: info.pr,
                branch: info.branch,
                started_at: info.started_at,
                worktree: info.worktree,
            })
            .collect();

        Ok::<Vec<BasicMinionData>, anyhow::Error>(basic)
        // Registry is dropped here, releasing the lock
    })
    .await
    .context("Failed to spawn blocking task for loading registry")??;

    // Phase 2: Perform expensive status checks (no lock held)
    let mut minions = tokio::task::spawn_blocking(move || {
        basic_minions
            .into_iter()
            // Filter out worktrees that were removed between Phase 1 and Phase 2
            .filter(|basic| basic.worktree.exists())
            .map(|basic| {
                // Get current status from filesystem (active/idle detection)
                let status = determine_status(&basic.worktree);
                let uptime = calculate_uptime(basic.started_at);
                // Get current branch from worktree (checks for detached HEAD, branch changes, etc.)
                let branch = get_current_branch(&basic.worktree, &basic.branch);

                EnhancedMinionInfo {
                    minion_id: basic.minion_id,
                    repo: basic.repo,
                    issue: basic.issue,
                    task: basic.task,
                    pr: basic.pr,
                    branch,
                    status,
                    uptime,
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
            eprintln!("No Minions found matching '{}'", filter_id);
            return Ok(1);
        }
        minions = filtered;
    }

    if minions.is_empty() {
        println!("No active Minions");
        return Ok(0);
    }

    // Sort by: status (active first), then started_at (newest first)
    // Since we don't have started_at directly, we'll sort by status and then minion_id
    minions.sort_by(|a, b| match (a.status.as_str(), b.status.as_str()) {
        ("Active", "Idle") => std::cmp::Ordering::Less,
        ("Idle", "Active") => std::cmp::Ordering::Greater,
        _ => a.minion_id.cmp(&b.minion_id),
    });

    // Print table header
    println!(
        "{:<8} {:<20} {:<8} {:<10} {:<8} {:<30} {:<10} {:<8}",
        "MINION", "REPO", "ISSUE", "TASK", "PR", "BRANCH", "STATUS", "UPTIME"
    );

    // Print each minion
    for minion in &minions {
        let issue_display = format!("#{}", minion.issue);
        let pr_display = minion
            .pr
            .as_ref()
            .map(|pr| format!("#{}", pr))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<8} {:<20} {:<8} {:<10} {:<8} {:<30} {:<10} {:<8}",
            minion.minion_id,
            minion.repo,
            issue_display,
            minion.task,
            pr_display,
            minion.branch,
            minion.status,
            minion.uptime
        );
    }

    println!();
    println!("{} Minion(s) found", minions.len());

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Integration test - performs real I/O and git operations
    async fn test_handle_status_no_filter() {
        // This test verifies that handle_status succeeds without filtering
        let result = handle_status(None).await;
        assert!(result.is_ok());
    }
}
