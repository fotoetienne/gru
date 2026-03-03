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
    /// Extract issue number from branch name.
    ///
    /// Splits the branch by `/`, then looks for a segment starting with `issue-`
    /// and parses the number immediately after the prefix. This correctly handles
    /// branches like `minion/issue-42-M001` and avoids false positives on
    /// branches like `issue-fix-42`.
    fn extract_issue_number(&self) -> Option<u32> {
        self.branch.split('/').find_map(|segment| {
            segment
                .strip_prefix("issue-")
                .and_then(|rest| rest.split('-').next())
                .and_then(|num_str| num_str.parse().ok())
        })
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

    /// Count PRs in a given state for this branch on GitHub.
    ///
    /// Runs `gh pr list --state <state> --head <branch> --json number --jq length`.
    ///
    /// # Error behavior
    /// - Failure to spawn the `gh`/`ghe` process propagates as `Err`.
    /// - Non-zero CLI exit (e.g., auth failure, network error) propagates as `Err`.
    ///   Callers decide the conservative default for their use case.
    async fn count_prs_in_state(&self, state: &str) -> Result<u64> {
        let gh_cmd = github::gh_command_for_repo(&self.repo);
        let output = Command::new(gh_cmd)
            .args([
                "pr",
                "list",
                "--state",
                state,
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
            .with_context(|| format!("Failed to run `{} pr list --state {}`", gh_cmd, state))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "`{} pr list --state {}` exited with {}: {}",
                gh_cmd,
                state,
                output.status,
                stderr
            );
        }

        let count_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if count_str.is_empty() {
            return Ok(0);
        }

        Ok(count_str.parse().unwrap_or_else(|_| {
            log::warn!(
                "Unexpected output from '{} pr list --jq length': {:?}",
                gh_cmd,
                count_str
            );
            0
        }))
    }

    /// Check if a PR for this branch was merged on GitHub (handles squash merges)
    ///
    /// Squash merges create new commit hashes, so `git branch --merged` won't detect them.
    /// This method uses `gh pr list --state merged --head <branch>` to check GitHub directly.
    pub async fn check_pr_merged_on_github(&self) -> Result<bool> {
        Ok(self.count_prs_in_state("merged").await? > 0)
    }

    /// Check if there is an open PR for this branch on GitHub.
    ///
    /// Used to prevent cleaning worktrees that have PRs under review.
    /// Callers should treat errors conservatively (i.e., assume an open PR may exist).
    pub async fn check_has_open_pr(&self) -> Result<bool> {
        Ok(self.count_prs_in_state("open").await? > 0)
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
        let discovered = parse_worktree_list(&stdout, &repo_name, &bare_repo_path);
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

/// Parse git worktree list --porcelain output.
///
/// Delegates to `git::parse_porcelain_worktrees` for the actual parsing,
/// then enriches entries with repo metadata and filters out the bare repo itself.
fn parse_worktree_list(output: &str, repo: &str, bare_repo_path: &Path) -> Vec<Worktree> {
    git::parse_porcelain_worktrees(output)
        .into_iter()
        .filter_map(|entry| {
            // Skip the bare repo entry and entries without a branch
            if entry.path == bare_repo_path {
                return None;
            }
            let branch = entry.branch?;
            Some(Worktree {
                path: entry.path,
                branch,
                repo: repo.to_string(),
                bare_repo_path: bare_repo_path.to_path_buf(),
            })
        })
        .collect()
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

        // Minion branch format: minion/issue-42-M001
        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "minion/issue-42-M001".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), Some(42));

        // Should NOT false-positive on "issue-fix-42"
        let wt = Worktree {
            path: PathBuf::from("/tmp/work"),
            branch: "issue-fix-42".to_string(),
            repo: "owner/repo".to_string(),
            bare_repo_path: PathBuf::from("/tmp/repo.git"),
        };
        assert_eq!(wt.extract_issue_number(), None);
    }

    #[test]
    fn test_parse_worktree_list() {
        let output = "worktree /Users/test/.gru/repos/owner_repo.git\nHEAD 1234567890abcdef\nbare\n\nworktree /Users/test/.gru/work/owner/repo/issue-36\nHEAD abcdef1234567890\nbranch refs/heads/issue-36\n\n";
        let bare_path = PathBuf::from("/Users/test/.gru/repos/owner_repo.git");
        let worktrees = parse_worktree_list(output, "owner/repo", &bare_path);

        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, "issue-36");
        assert_eq!(worktrees[0].repo, "owner/repo");
        assert_eq!(
            worktrees[0].path,
            PathBuf::from("/Users/test/.gru/work/owner/repo/issue-36")
        );
    }
}
