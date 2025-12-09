use crate::workspace;
use anyhow::{Context, Result};

/// Represents the status of a single Minion
#[derive(Debug)]
struct MinionStatus {
    minion_id: String,
    issue_number: String,
    repo_name: String,
    branch: String,
    status: String,
    uptime: String,
}

/// Scans the ~/.gru/work directory for active Minion worktrees
fn scan_worktrees() -> Result<Vec<MinionStatus>> {
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

                minions.push(MinionStatus {
                    minion_id,
                    issue_number,
                    repo_name,
                    branch,
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
fn parse_issue_from_branch(branch: &str) -> String {
    if let Some(issue_part) = branch.strip_prefix("minion/issue-") {
        // Extract the number before the next hyphen
        if let Some(pos) = issue_part.find('-') {
            return issue_part[..pos].to_string();
        }
    }
    "?".to_string()
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

/// Handles the status command by displaying active Minions
pub fn handle_status() -> Result<i32> {
    let minions = scan_worktrees()?;

    if minions.is_empty() {
        println!("No active Minions");
        return Ok(0);
    }

    // Print table header
    println!(
        "{:<8} {:<8} {:<20} {:<30} {:<10} {:<8}",
        "MINION", "ISSUE", "REPO", "BRANCH", "STATUS", "UPTIME"
    );

    // Print each minion
    for minion in &minions {
        println!(
            "{:<8} #{:<7} {:<20} {:<30} {:<10} {:<8}",
            minion.minion_id,
            minion.issue_number,
            minion.repo_name,
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

    #[test]
    fn test_parse_issue_from_branch_valid() {
        assert_eq!(parse_issue_from_branch("minion/issue-42-M001"), "42");
        assert_eq!(parse_issue_from_branch("minion/issue-123-M999"), "123");
        assert_eq!(parse_issue_from_branch("minion/issue-1-M0tk"), "1");
    }

    #[test]
    fn test_parse_issue_from_branch_invalid() {
        assert_eq!(parse_issue_from_branch("main"), "?");
        assert_eq!(parse_issue_from_branch("feature/branch"), "?");
        assert_eq!(parse_issue_from_branch(""), "?");
        assert_eq!(parse_issue_from_branch("minion/issue-"), "?");
    }

    #[test]
    fn test_scan_worktrees_returns_valid_vec() {
        // This test verifies that scan_worktrees succeeds and returns a valid vector
        // Note: A more thorough test would use a temporary directory with known contents
        let result = scan_worktrees();
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
