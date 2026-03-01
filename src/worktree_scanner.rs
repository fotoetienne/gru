use crate::git;
use crate::github;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Represents a discovered worktree
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Path to the worktree directory
    pub path: PathBuf,
    /// Branch name associated with this worktree
    pub branch: String,
    /// Repository identifier (e.g., "owner/repo")
    pub repo: String,
    /// Path to the bare repository
    pub bare_repo_path: PathBuf,
}

/// Status of a worktree indicating whether it can be cleaned
#[derive(Debug, PartialEq)]
pub enum WorktreeStatus {
    /// Branch has been merged into the base branch
    Merged,
    /// PR was merged on GitHub (e.g., squash merge where commit hashes differ)
    PrMerged,
    /// Associated GitHub issue is closed
    IssueClosed,
    /// Branch has been deleted on remote
    RemoteDeleted,
    /// Minion process is stopped (no live process in registry)
    MinionStopped,
    /// Worktree is still active and should not be cleaned
    Active,
}

impl Worktree {
    /// Extract issue number from branch name
    /// Supports formats like "issue-36", "gru/issue-36", "fix/issue-36"
    fn extract_issue_number(&self) -> Option<u32> {
        // Look for "issue-" or "issues/" followed by a number
        for part in self.branch.split(&['/', '-', '_']) {
            if part == "issue" || part == "issues" {
                // The next part might be the number
                continue;
            }
            // Check if this part comes after "issue"
            if self.branch.contains(&format!("issue-{}", part))
                || self.branch.contains(&format!("issue/{}", part))
                || self.branch.contains(&format!("issues-{}", part))
                || self.branch.contains(&format!("issues/{}", part))
            {
                if let Ok(num) = part.parse::<u32>() {
                    return Some(num);
                }
            }
        }
        None
    }

    /// Check if the worktree's branch has been merged into the base branch
    pub async fn check_merged(&self, base_branch: &str) -> Result<bool> {
        let output = Command::new("git")
            .args([
                "-C",
                &self.bare_repo_path.to_string_lossy(),
                "branch",
                "--merged",
                base_branch,
                "--list",
                &self.branch,
            ])
            .output()
            .await
            .context("Failed to check if branch is merged")?;

        Ok(!output.stdout.is_empty())
    }

    /// Check if the associated GitHub issue is closed
    pub async fn check_issue_closed(&self) -> Result<Option<bool>> {
        let issue_num = match self.extract_issue_number() {
            Some(num) => num,
            None => return Ok(None),
        };

        let gh_cmd = github::gh_command_for_repo(&self.repo);
        let output = Command::new(gh_cmd)
            .args([
                "issue",
                "view",
                &issue_num.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
                "--repo",
                &self.repo,
            ])
            .output()
            .await
            .context("Failed to check issue status")?;

        if !output.status.success() {
            return Ok(None);
        }

        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(state == "CLOSED"))
    }

    /// Check if the branch has been deleted on the remote
    pub async fn check_remote_deleted(&self) -> Result<bool> {
        // First fetch to ensure we have latest remote info
        let fetch_output = Command::new("git")
            .args([
                "-C",
                &self.bare_repo_path.to_string_lossy(),
                "fetch",
                "--prune",
            ])
            .output()
            .await
            .context("Failed to fetch from remote")?;

        if !fetch_output.status.success() {
            // If fetch fails, be conservative and assume branch exists
            log::warn!(
                "Warning: Failed to fetch from remote: {}",
                String::from_utf8_lossy(&fetch_output.stderr)
            );
            return Ok(false);
        }

        // Check if remote branch exists
        let output = Command::new("git")
            .args([
                "-C",
                &self.bare_repo_path.to_string_lossy(),
                "ls-remote",
                "--heads",
                "origin",
                &self.branch,
            ])
            .output()
            .await
            .context("Failed to check remote branch")?;

        // If ls-remote returns empty, the branch doesn't exist on remote
        Ok(output.stdout.is_empty())
    }

    /// Check if a PR for this branch was merged on GitHub (handles squash merges)
    ///
    /// Squash merges create new commit hashes, so `git branch --merged` won't detect them.
    /// This method uses `gh pr list --state merged --head <branch>` to check GitHub directly.
    ///
    /// # Error behavior
    /// - Failure to spawn the `gh`/`ghe` process propagates as `Err` (system-level problem).
    /// - Non-zero CLI exit (e.g., auth failure, network error) returns `Ok(false)` to degrade
    ///   gracefully without blocking cleanup of other worktrees.
    pub async fn check_pr_merged_on_github(&self) -> Result<bool> {
        let gh_cmd = github::gh_command_for_repo(&self.repo);
        let output = Command::new(gh_cmd)
            .args([
                "pr",
                "list",
                "--state",
                "merged",
                "--head",
                &self.branch,
                "--repo",
                &self.repo,
                "--json",
                "number",
                "--jq",
                "length",
            ])
            .output()
            .await
            .context("Failed to check PR merge status on GitHub")?;

        if !output.status.success() {
            return Ok(false);
        }

        let count_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let count: u64 = if count_str.is_empty() {
            0
        } else {
            match count_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "Unexpected output from '{} pr list --jq length': {:?}",
                        gh_cmd,
                        count_str
                    );
                    0
                }
            }
        };
        Ok(count > 0)
    }

    /// Determine the overall status of this worktree
    pub async fn status(&self, base_branch: &str) -> Result<WorktreeStatus> {
        // Check in order of priority

        // 1. Check if merged (git-level)
        if self
            .check_merged(base_branch)
            .await
            .map_err(|e| {
                log::warn!("Warning: Failed to check if branch is merged: {}", e);
                e
            })
            .unwrap_or(false)
        {
            return Ok(WorktreeStatus::Merged);
        }

        // 2. Check if PR was merged on GitHub (handles squash merges)
        if self
            .check_pr_merged_on_github()
            .await
            .map_err(|e| {
                log::warn!("Warning: Failed to check PR merge status on GitHub: {}", e);
                e
            })
            .unwrap_or(false)
        {
            return Ok(WorktreeStatus::PrMerged);
        }

        // 3. Check if issue is closed
        if let Some(true) = self
            .check_issue_closed()
            .await
            .map_err(|e| {
                log::warn!("Warning: Failed to check issue status: {}", e);
                e
            })
            .unwrap_or(None)
        {
            return Ok(WorktreeStatus::IssueClosed);
        }

        // 4. Check if remote branch is deleted
        if self
            .check_remote_deleted()
            .await
            .map_err(|e| {
                log::warn!("Warning: Failed to check remote status: {}", e);
                e
            })
            .unwrap_or(false)
        {
            return Ok(WorktreeStatus::RemoteDeleted);
        }

        Ok(WorktreeStatus::Active)
    }
}

/// Discover all worktrees in the given repos directory
pub async fn discover_worktrees(repos_dir: &Path) -> Result<Vec<Worktree>> {
    let mut worktrees = Vec::new();

    if !repos_dir.exists() {
        return Ok(worktrees);
    }

    // Find all bare repositories recursively
    let bare_repos = find_bare_repos(repos_dir).await?;

    for bare_repo_path in bare_repos {
        // Extract repo name from git config
        let repo_name = match extract_repo_from_git_config(&bare_repo_path).await {
            Ok(name) => name,
            Err(e) => {
                log::warn!(
                    "Warning: Failed to extract repo name from {}: {}",
                    bare_repo_path.display(),
                    e
                );
                continue;
            }
        };

        // List worktrees for this bare repo
        let output = Command::new("git")
            .args([
                "-C",
                &bare_repo_path.to_string_lossy(),
                "worktree",
                "list",
                "--porcelain",
            ])
            .output()
            .await;

        let output = match output {
            Ok(out) => out,
            Err(e) => {
                log::warn!(
                    "Warning: Failed to list worktrees for {}: {}",
                    bare_repo_path.display(),
                    e
                );
                continue;
            }
        };

        if !output.status.success() {
            continue;
        }

        // Parse worktree list output
        let stdout = String::from_utf8_lossy(&output.stdout);
        let discovered = parse_worktree_list(&stdout, &repo_name, &bare_repo_path)?;
        worktrees.extend(discovered);
    }

    Ok(worktrees)
}

/// Recursively find all bare repositories in the given directory
async fn find_bare_repos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut bare_repos = Vec::new();
    let mut dirs_to_scan = vec![dir.to_path_buf()];

    while let Some(current_dir) = dirs_to_scan.pop() {
        let entries = match std::fs::read_dir(&current_dir) {
            Ok(entries) => entries,
            Err(e) => {
                log::warn!(
                    "Warning: Failed to read directory {}: {}",
                    current_dir.display(),
                    e
                );
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            // Check if this is a bare repo using git rev-parse
            let is_bare = match Command::new("git")
                .args([
                    "-C",
                    &path.to_string_lossy(),
                    "rev-parse",
                    "--is-bare-repository",
                ])
                .output()
                .await
            {
                Ok(output) => {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == "true"
                }
                Err(_) => false,
            };

            if is_bare {
                bare_repos.push(path);
            } else {
                // Not a bare repo, so continue scanning deeper
                dirs_to_scan.push(path);
            }
        }
    }

    Ok(bare_repos)
}

/// Extract repository identifier from git config
/// Uses `git config remote.origin.url` to get the actual repo URL and parses it
/// via `git::parse_github_remote` to avoid duplicating URL parsing logic.
/// Example: https://github.com/owner/repo.git -> "owner/repo"
///          git@github.com:owner/repo.git -> "owner/repo"
async fn extract_repo_from_git_config(path: &Path) -> Result<String> {
    // Get remote.origin.url from git config
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "config", "remote.origin.url"])
        .output()
        .await
        .context("Failed to run git config")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get remote.origin.url: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let (owner, repo) =
        git::parse_github_remote(&url).context("Failed to parse repo from remote URL")?;
    Ok(format!("{}/{}", owner, repo))
}

/// Parse git worktree list --porcelain output
fn parse_worktree_list(output: &str, repo: &str, bare_repo_path: &Path) -> Result<Vec<Worktree>> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if line.starts_with("worktree ") {
            current_path = Some(PathBuf::from(line.strip_prefix("worktree ").unwrap()));
        } else if line.starts_with("branch ") {
            let branch_ref = line.strip_prefix("branch ").unwrap();
            // Extract branch name from refs/heads/branch-name
            current_branch = branch_ref
                .strip_prefix("refs/heads/")
                .map(|s| s.to_string());
        } else if line.is_empty() {
            // End of worktree entry
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                // Skip the main bare repo worktree
                if path != bare_repo_path {
                    worktrees.push(Worktree {
                        path,
                        branch,
                        repo: repo.to_string(),
                        bare_repo_path: bare_repo_path.to_path_buf(),
                    });
                }
            }
        }
    }

    // Handle last entry if file doesn't end with newline
    if let (Some(path), Some(branch)) = (current_path, current_branch) {
        if path != bare_repo_path {
            worktrees.push(Worktree {
                path,
                branch,
                repo: repo.to_string(),
                bare_repo_path: bare_repo_path.to_path_buf(),
            });
        }
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_issue_number() {
        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "issue-36".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), Some(36));

        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "gru/issue-42".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), Some(42));

        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "fix/issue-123".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), Some(123));

        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "feature-branch".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), None);
    }

    #[test]
    fn test_parse_worktree_list() {
        let output = "worktree /Users/test/.gru/repos/owner_repo.git\nHEAD 1234567890abcdef\nbare\n\nworktree /Users/test/.gru/work/owner/repo/issue-36\nHEAD abcdef1234567890\nbranch refs/heads/issue-36\n\n";
        let bare_path = PathBuf::from("/Users/test/.gru/repos/owner_repo.git");
        let worktrees = parse_worktree_list(output, "owner/repo", &bare_path).unwrap();

        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, "issue-36");
        assert_eq!(worktrees[0].repo, "owner/repo");
        assert_eq!(
            worktrees[0].path,
            PathBuf::from("/Users/test/.gru/work/owner/repo/issue-36")
        );
    }
}
