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
    status: String,
    uptime: String,
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
pub async fn handle_status(id: Option<String>) -> Result<i32> {
    // Load registry and run migration if needed (spawn_blocking to avoid blocking the async runtime)
    let mut minions = tokio::task::spawn_blocking(|| {
        let mut registry = MinionRegistry::load(None)?;

        // Run migration on first status call
        let migrated = registry.migrate_from_worktrees()?;
        if migrated > 0 {
            eprintln!("📦 Migrated {} existing Minion(s) to registry", migrated);
        }

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

        // Convert to enhanced info with current status from filesystem
        let enhanced: Vec<EnhancedMinionInfo> = registry_minions
            .into_iter()
            .map(|(minion_id, info)| {
                // Get current status from filesystem (active/idle detection)
                let status = determine_status(&info.worktree);
                let uptime = calculate_uptime(info.started_at);

                EnhancedMinionInfo {
                    minion_id,
                    repo: info.repo,
                    issue: info.issue,
                    task: info.command,
                    pr: info.pr,
                    status,
                    uptime,
                }
            })
            .collect();

        Ok::<Vec<EnhancedMinionInfo>, anyhow::Error>(enhanced)
    })
    .await
    .context("Failed to spawn blocking task for loading registry")??;

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
        "{:<8} {:<20} {:<8} {:<10} {:<8} {:<10} {:<8}",
        "MINION", "REPO", "ISSUE", "TASK", "PR", "STATUS", "UPTIME"
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
            "{:<8} {:<20} {:<8} {:<10} {:<8} {:<10} {:<8}",
            minion.minion_id,
            minion.repo,
            issue_display,
            minion.task,
            pr_display,
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
    async fn test_handle_status_no_filter() {
        // This test verifies that handle_status succeeds without filtering
        let result = handle_status(None).await;
        assert!(result.is_ok());
    }
}
