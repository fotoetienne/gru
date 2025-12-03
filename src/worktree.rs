use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Represents a Git worktree discovered in the workspace
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Path to the worktree directory
    pub path: PathBuf,
    /// Branch name associated with this worktree
    pub branch: String,
    /// Repository owner/name (extracted from path)
    pub repo: String,
}

/// Status of a worktree indicating whether it can be cleaned
#[derive(Debug, PartialEq)]
pub enum WorktreeStatus {
    /// Branch has been merged into the base branch
    Merged,
    /// Associated GitHub issue is closed
    IssueClosed,
    /// Branch has been deleted on remote
    RemoteDeleted,
    /// Worktree is still active and should not be cleaned
    Active,
}

impl Worktree {
    /// Parse a worktree from a git worktree list line
    /// Format: "/path/to/worktree  <commit-hash>  [branch-name]"
    pub fn from_git_output(line: &str, workspace_root: &Path) -> Option<Self> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            return None;
        }

        let path = PathBuf::from(parts[0]);

        // Only process worktrees within our workspace
        if !path.starts_with(workspace_root) {
            return None;
        }

        // Extract branch name from brackets
        let branch = parts[2].trim_start_matches('[').trim_end_matches(']');

        // Skip if it's the main worktree or HEAD
        if branch == "HEAD" || !path.to_str()?.contains("/work/") {
            return None;
        }

        // Extract repo name from path: ~/.gru/work/owner/repo/issue-XX
        let path_str = path.to_str()?;
        let work_prefix = "/work/";
        let work_idx = path_str.find(work_prefix)?;
        let after_work = &path_str[work_idx + work_prefix.len()..];

        // Split by / and take first two parts (owner/repo)
        let parts: Vec<&str> = after_work.split('/').collect();
        if parts.len() < 3 {
            return None;
        }

        let repo = format!("{}/{}", parts[0], parts[1]);

        Some(Worktree {
            path,
            branch: branch.to_string(),
            repo,
        })
    }

    /// Check if the worktree's branch has been merged into the base branch
    pub async fn check_merged(&self, base_branch: &str) -> Result<bool> {
        let output = Command::new("git")
            .args(["branch", "--merged", base_branch, "--list", &self.branch])
            .current_dir(&self.path)
            .output()
            .context("Failed to check if branch is merged")?;

        Ok(!output.stdout.is_empty())
    }

    /// Check if the associated GitHub issue is closed
    /// Extracts issue number from branch name (e.g., "gru/issue-36" -> 36)
    pub async fn check_issue_closed(&self) -> Result<Option<bool>> {
        let issue_num = self.extract_issue_number();
        if issue_num.is_none() {
            return Ok(None);
        }

        let issue_num = issue_num.unwrap();

        // Use gh CLI to check issue status
        let output = Command::new("gh")
            .args([
                "issue",
                "view",
                &issue_num.to_string(),
                "--json",
                "state",
                "--repo",
                &self.repo,
            ])
            .output()
            .context("Failed to check issue status")?;

        if !output.status.success() {
            return Ok(None);
        }

        #[derive(Deserialize)]
        struct IssueState {
            state: String,
        }

        let state: IssueState =
            serde_json::from_slice(&output.stdout).context("Failed to parse issue state")?;

        Ok(Some(state.state == "CLOSED"))
    }

    /// Check if the branch has been deleted on the remote
    pub async fn check_remote_deleted(&self) -> Result<bool> {
        // Fetch to ensure we have latest remote info
        let _ = Command::new("git")
            .args(["fetch", "--prune"])
            .current_dir(&self.path)
            .output();

        let output = Command::new("git")
            .args(["ls-remote", "--heads", "origin", &self.branch])
            .current_dir(&self.path)
            .output()
            .context("Failed to check remote branch")?;

        Ok(output.stdout.is_empty())
    }

    /// Determine the overall status of this worktree
    pub async fn status(&self, base_branch: &str) -> Result<WorktreeStatus> {
        // Check in order of preference
        if self.check_merged(base_branch).await? {
            return Ok(WorktreeStatus::Merged);
        }

        if let Some(true) = self.check_issue_closed().await? {
            return Ok(WorktreeStatus::IssueClosed);
        }

        if self.check_remote_deleted().await? {
            return Ok(WorktreeStatus::RemoteDeleted);
        }

        Ok(WorktreeStatus::Active)
    }

    /// Extract issue number from branch name
    /// Supports formats like: "gru/issue-36", "issue-36", "fix/issue-123"
    fn extract_issue_number(&self) -> Option<u32> {
        let branch = &self.branch;

        // Try to find "issue-" pattern
        if let Some(idx) = branch.find("issue-") {
            let after_prefix = &branch[idx + 6..];
            // Take digits until we hit a non-digit
            let num_str: String = after_prefix
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            return num_str.parse().ok();
        }

        None
    }
}

/// Discover all worktrees in the workspace by scanning bare repos
pub fn discover_worktrees(workspace_root: &Path) -> Result<Vec<Worktree>> {
    use std::fs;

    let mut all_worktrees = Vec::new();

    // Get the repos directory (where bare repos live)
    let repos_dir = workspace_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid workspace path"))?
        .join("repos");

    if !repos_dir.exists() {
        return Ok(all_worktrees);
    }

    // Walk through the repos directory to find bare repos
    fn find_bare_repos(dir: &Path, bare_repos: &mut Vec<PathBuf>) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        // Check if this is a bare repo
        if dir.to_str().is_some_and(|s| s.ends_with(".git")) {
            bare_repos.push(dir.to_path_buf());
            return Ok(());
        }

        // Recursively search subdirectories
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                find_bare_repos(&path, bare_repos)?;
            }
        }

        Ok(())
    }

    let mut bare_repos = Vec::new();
    find_bare_repos(&repos_dir, &mut bare_repos)?;

    // For each bare repo, list its worktrees
    for bare_repo in bare_repos {
        let output = Command::new("git")
            .args(["worktree", "list"])
            .current_dir(&bare_repo)
            .output()
            .context(format!(
                "Failed to list worktrees for {}",
                bare_repo.display()
            ))?;

        if !output.status.success() {
            continue; // Skip repos that fail
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(wt) = Worktree::from_git_output(line, workspace_root) {
                all_worktrees.push(wt);
            }
        }
    }

    Ok(all_worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_issue_number() {
        let wt = Worktree {
            path: PathBuf::from("/tmp/test"),
            branch: "gru/issue-36".to_string(),
            repo: "owner/repo".to_string(),
        };
        assert_eq!(wt.extract_issue_number(), Some(36));

        let wt2 = Worktree {
            path: PathBuf::from("/tmp/test"),
            branch: "issue-123".to_string(),
            repo: "owner/repo".to_string(),
        };
        assert_eq!(wt2.extract_issue_number(), Some(123));

        let wt3 = Worktree {
            path: PathBuf::from("/tmp/test"),
            branch: "feature/something".to_string(),
            repo: "owner/repo".to_string(),
        };
        assert_eq!(wt3.extract_issue_number(), None);
    }

    #[test]
    fn test_from_git_output() {
        let workspace_root = PathBuf::from("/Users/test/.gru");
        let line = "/Users/test/.gru/work/owner/repo/issue-36  1234567  [gru/issue-36]";

        let wt = Worktree::from_git_output(line, &workspace_root);
        assert!(wt.is_some());

        let wt = wt.unwrap();
        assert_eq!(wt.branch, "gru/issue-36");
        assert_eq!(wt.repo, "owner/repo");
        assert_eq!(
            wt.path,
            PathBuf::from("/Users/test/.gru/work/owner/repo/issue-36")
        );
    }

    #[test]
    fn test_from_git_output_rejects_non_workspace() {
        let workspace_root = PathBuf::from("/Users/test/.gru");
        let line = "/other/path/repo  1234567  [main]";

        let wt = Worktree::from_git_output(line, &workspace_root);
        assert!(wt.is_none());
    }

    #[test]
    fn test_from_git_output_rejects_head() {
        let workspace_root = PathBuf::from("/Users/test/.gru");
        let line = "/Users/test/.gru/repos/owner/repo.git  1234567  [HEAD]";

        let wt = Worktree::from_git_output(line, &workspace_root);
        assert!(wt.is_none());
    }
}
