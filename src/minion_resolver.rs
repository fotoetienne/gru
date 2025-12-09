use crate::workspace;
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

/// Information about a resolved Minion worktree
#[derive(Debug, Clone)]
pub struct MinionInfo {
    pub minion_id: String,
    pub issue_number: Option<u64>,
    pub repo_name: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub status: String,
    pub uptime: String,
}

/// Smart ID resolution that tries multiple strategies
/// 1. Try as exact minion ID (e.g., M0wy)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
pub async fn resolve_minion(id: &str) -> Result<MinionInfo> {
    // Strategy 1: Try as exact minion ID
    if let Ok(info) = find_by_minion_id(id) {
        return Ok(info);
    }

    // Strategy 2: Try with M prefix if not already present
    if !id.starts_with('M') {
        if let Ok(info) = find_by_minion_id(&format!("M{}", id)) {
            return Ok(info);
        }
    }

    // Strategy 3: Try as issue/PR number
    if let Ok(num) = id.parse::<u64>() {
        if let Ok(info) = find_by_issue_number(num).await {
            return Ok(info);
        }
    }

    anyhow::bail!(
        "Could not resolve ID '{}'. Tried:\n  \
         - Minion ID: {}\n  \
         - Minion ID: M{}\n  \
         - Issue/PR number: {}\n\n\
         Try 'gru status' to see active minions.",
        id,
        id,
        id,
        id
    )
}

/// Find a minion by exact minion ID
pub fn find_by_minion_id(minion_id: &str) -> Result<MinionInfo> {
    let minions = scan_all_minions()?;

    minions
        .into_iter()
        .find(|m| m.minion_id == minion_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No worktree found for Minion {}. It may not have been created yet.",
                minion_id
            )
        })
}

/// Find a minion by issue or PR number
/// First searches locally, then falls back to GitHub API
pub async fn find_by_issue_number(issue_num: u64) -> Result<MinionInfo> {
    // First try local resolution
    let minions = scan_all_minions()?;

    if let Some(minion) = minions
        .into_iter()
        .find(|m| m.issue_number == Some(issue_num))
    {
        return Ok(minion);
    }

    // Fallback to GitHub API (might be a PR)
    // Try to resolve as PR first, which will get the linked issue
    if let Ok(minion_id) = resolve_minion_from_pr(issue_num).await {
        return find_by_minion_id(&minion_id);
    }

    // Try to resolve as issue directly
    if let Ok(minion_id) = resolve_minion_from_issue(issue_num).await {
        return find_by_minion_id(&minion_id);
    }

    anyhow::bail!(
        "No local worktree found for issue/PR #{}. Try 'gru status' to see active minions.",
        issue_num
    )
}

/// Scans all minion worktrees and returns MinionInfo structs
pub fn scan_all_minions() -> Result<Vec<MinionInfo>> {
    let workspace = workspace::Workspace::new().context("Failed to initialize workspace")?;
    let work_path = workspace.work();

    if !work_path.exists() {
        return Ok(Vec::new());
    }

    let mut minions = Vec::new();

    // Iterate over owner directories
    for owner_entry in std::fs::read_dir(work_path)? {
        let owner_entry = owner_entry?;
        if !owner_entry.path().is_dir() {
            continue;
        }

        // Iterate over repo directories
        for repo_entry in std::fs::read_dir(owner_entry.path())? {
            let repo_entry = repo_entry?;
            if !repo_entry.path().is_dir() {
                continue;
            }

            // Iterate over minion directories (should start with 'M')
            for minion_entry in std::fs::read_dir(repo_entry.path())? {
                let minion_entry = minion_entry?;
                let minion_path = minion_entry.path();

                if !minion_path.is_dir() {
                    continue;
                }

                let minion_id = minion_entry.file_name().to_string_lossy().to_string();

                // Validate minion ID against path traversal and invalid characters
                if minion_id.contains('/') || minion_id.contains('\\') || minion_id.contains("..") {
                    continue; // Skip invalid directory names
                }

                if !minion_id.starts_with('M') || !minion_id.chars().all(|c| c.is_alphanumeric()) {
                    continue; // Skip non-minion directories
                }

                // Check if this is a valid git worktree
                let git_dir = minion_path.join(".git");
                if !git_dir.exists() {
                    continue;
                }

                // Get the branch name from git
                let branch_output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&minion_path)
                    .arg("branch")
                    .arg("--show-current")
                    .output()?;

                let branch = String::from_utf8_lossy(&branch_output.stdout)
                    .trim()
                    .to_string();

                // Parse issue number from branch name (format: minion/issue-<num>-<id>)
                let issue_number = parse_issue_from_branch(&branch);

                // Determine status (Active or Idle) based on git index modification time
                let status = determine_status(&minion_path)?;

                // Calculate uptime from worktree creation time
                let uptime = calculate_uptime(&minion_path)?;

                // Build repo name from path components
                let owner = owner_entry.file_name().to_string_lossy().to_string();
                let repo = repo_entry.file_name().to_string_lossy().to_string();
                let repo_name = format!("{}/{}", owner, repo);

                minions.push(MinionInfo {
                    minion_id,
                    issue_number,
                    repo_name,
                    branch,
                    worktree_path: minion_path,
                    status,
                    uptime,
                });
            }
        }
    }

    // Sort by minion ID
    minions.sort_by(|a, b| a.minion_id.cmp(&b.minion_id));

    Ok(minions)
}

/// Parses the issue number from a branch name
/// Expected format: minion/issue-<num>-<id>
fn parse_issue_from_branch(branch: &str) -> Option<u64> {
    if let Some(issue_part) = branch.strip_prefix("minion/issue-") {
        // Extract the number before the next hyphen
        if let Some(pos) = issue_part.find('-') {
            if let Ok(num) = issue_part[..pos].parse::<u64>() {
                return Some(num);
            }
        }
    }
    None
}

/// Determines if a Minion is Active or Idle based on git index modification time
/// A Minion is considered Active if the git index was modified in the last 5 minutes
fn determine_status(worktree_path: &std::path::Path) -> Result<String> {
    // Use git rev-parse to get the actual git directory path
    // In worktrees, .git is a file, not a directory
    let git_dir_output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg("--git-dir")
        .output()?;

    if !git_dir_output.status.success() {
        return Ok("Idle".to_string());
    }

    let git_dir = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();
    let git_index = std::path::PathBuf::from(git_dir).join("index");

    if !git_index.exists() {
        return Ok("Idle".to_string());
    }

    let metadata = std::fs::metadata(&git_index)?;
    let modified = metadata.modified()?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(modified).unwrap_or_default();

    // Consider active if modified within the last 5 minutes
    if elapsed.as_secs() < 300 {
        Ok("Active".to_string())
    } else {
        Ok("Idle".to_string())
    }
}

/// Calculates the uptime of a worktree based on its creation time
fn calculate_uptime(worktree_path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(worktree_path)?;
    let created = metadata.created().or_else(|_| metadata.modified())?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(created).unwrap_or_default();

    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        Ok(format!("{}d", days))
    } else if hours > 0 {
        Ok(format!("{}h", hours))
    } else if minutes > 0 {
        Ok(format!("{}m", minutes))
    } else {
        Ok("< 1m".to_string())
    }
}

/// Resolves a Minion ID from a GitHub issue number
async fn resolve_minion_from_issue(issue_num: u64) -> Result<String> {
    // Use gh CLI to get issue labels
    let output = Command::new("gh")
        .args(["issue", "view", &issue_num.to_string(), "--json", "labels"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch issue #{}: {}", issue_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    // Look for in-progress:M<id> label
    let labels = json["labels"]
        .as_array()
        .context("Issue labels field is not an array")?;

    for label in labels {
        let label_name = label["name"]
            .as_str()
            .context("Label name is not a string")?;

        if let Some(minion_id) = label_name.strip_prefix("in-progress:") {
            if minion_id.starts_with('M') {
                return Ok(minion_id.to_string());
            }
        }
    }

    anyhow::bail!(
        "No active Minion found for issue #{}. Issue may not be in progress.",
        issue_num
    );
}

/// Resolves a Minion ID from a GitHub PR number
async fn resolve_minion_from_pr(pr_num: u64) -> Result<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static ISSUE_LINK_REGEX: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)(?:fixes|closes|resolves)\s+#(\d+)")
            .expect("Failed to compile issue link regex")
    });

    // Use gh CLI to get linked issue from PR body
    let output = Command::new("gh")
        .args(["pr", "view", &pr_num.to_string(), "--json", "body"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR #{}: {}", pr_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    let body = json["body"]
        .as_str()
        .context("PR body field is not a string")?;

    // Look for "Fixes #<issue>" or "Closes #<issue>" in the PR body
    if let Some(captures) = ISSUE_LINK_REGEX.captures(body) {
        let issue_num = captures[1]
            .parse::<u64>()
            .context("Failed to parse issue number from PR body")?;

        // Now resolve the Minion from that issue
        return resolve_minion_from_issue(issue_num).await;
    }

    anyhow::bail!(
        "No linked issue found for PR #{}. PR must contain 'Fixes #<issue>' in its description.",
        pr_num
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_issue_from_branch_valid() {
        assert_eq!(parse_issue_from_branch("minion/issue-42-M001"), Some(42));
        assert_eq!(parse_issue_from_branch("minion/issue-123-M999"), Some(123));
        assert_eq!(parse_issue_from_branch("minion/issue-1-M0tk"), Some(1));
    }

    #[test]
    fn test_parse_issue_from_branch_invalid() {
        assert_eq!(parse_issue_from_branch("main"), None);
        assert_eq!(parse_issue_from_branch("feature/branch"), None);
        assert_eq!(parse_issue_from_branch(""), None);
        assert_eq!(parse_issue_from_branch("minion/issue-"), None);
    }

    #[test]
    fn test_scan_all_minions_returns_valid_vec() {
        // This test verifies that scan_all_minions succeeds and returns a valid vector
        let result = scan_all_minions();
        assert!(result.is_ok());

        // Verify we get a vector and all minions have required fields
        let minions = result.unwrap();
        for minion in &minions {
            assert!(!minion.minion_id.is_empty());
            assert!(!minion.repo_name.is_empty());
            assert!(!minion.status.is_empty());
            assert!(!minion.uptime.is_empty());
        }
    }
}
