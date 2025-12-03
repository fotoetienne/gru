use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Associated GitHub issue is closed
    IssueClosed,
    /// Branch has been deleted on remote
    RemoteDeleted,
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
    pub fn check_merged(&self, base_branch: &str) -> Result<bool> {
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
            .context("Failed to check if branch is merged")?;

        Ok(!output.stdout.is_empty())
    }

    /// Check if the associated GitHub issue is closed
    pub fn check_issue_closed(&self) -> Result<Option<bool>> {
        let issue_num = match self.extract_issue_number() {
            Some(num) => num,
            None => return Ok(None),
        };

        // Use gh CLI to check issue status
        let output = Command::new("gh")
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
            .context("Failed to check issue status")?;

        if !output.status.success() {
            return Ok(None);
        }

        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(state == "CLOSED"))
    }

    /// Check if the branch has been deleted on the remote
    pub fn check_remote_deleted(&self) -> Result<bool> {
        // First fetch to ensure we have latest remote info
        let fetch_output = Command::new("git")
            .args([
                "-C",
                &self.bare_repo_path.to_string_lossy(),
                "fetch",
                "--prune",
            ])
            .output()
            .context("Failed to fetch from remote")?;

        if !fetch_output.status.success() {
            // If fetch fails, be conservative and assume branch exists
            eprintln!(
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
            .context("Failed to check remote branch")?;

        // If ls-remote returns empty, the branch doesn't exist on remote
        Ok(output.stdout.is_empty())
    }

    /// Determine the overall status of this worktree
    pub fn status(&self, base_branch: &str) -> Result<WorktreeStatus> {
        // Check in order of priority

        // 1. Check if merged
        if self.check_merged(base_branch).unwrap_or(false) {
            return Ok(WorktreeStatus::Merged);
        }

        // 2. Check if issue is closed
        if let Some(true) = self.check_issue_closed().unwrap_or(None) {
            return Ok(WorktreeStatus::IssueClosed);
        }

        // 3. Check if remote branch is deleted
        if self.check_remote_deleted().unwrap_or(false) {
            return Ok(WorktreeStatus::RemoteDeleted);
        }

        Ok(WorktreeStatus::Active)
    }
}

/// Discover all worktrees in the given repos directory
pub fn discover_worktrees(repos_dir: &Path) -> Result<Vec<Worktree>> {
    let mut worktrees = Vec::new();

    if !repos_dir.exists() {
        return Ok(worktrees);
    }

    // Find all bare repositories recursively
    let bare_repos = find_bare_repos(repos_dir)?;

    for bare_repo_path in bare_repos {
        // Extract repo name from git config
        let repo_name = match extract_repo_from_git_config(&bare_repo_path) {
            Ok(name) => name,
            Err(e) => {
                eprintln!(
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
            .output();

        let output = match output {
            Ok(out) => out,
            Err(e) => {
                eprintln!(
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
fn find_bare_repos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut bare_repos = Vec::new();
    let mut dirs_to_scan = vec![dir.to_path_buf()];

    while let Some(current_dir) = dirs_to_scan.pop() {
        let entries = match std::fs::read_dir(&current_dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!(
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

            // Check if this looks like a bare repo (ends with .git or contains objects/refs)
            let is_bare = path.extension().and_then(|e| e.to_str()) == Some("git")
                || (path.join("objects").exists() && path.join("refs").exists());

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
/// Example: https://github.com/owner/repo.git -> "owner/repo"
///          git@github.com:owner/repo.git -> "owner/repo"
fn extract_repo_from_git_config(path: &Path) -> Result<String> {
    // Get remote.origin.url from git config
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "config", "remote.origin.url"])
        .output()
        .context("Failed to run git config")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get remote.origin.url: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Parse the URL to extract owner/repo
    parse_repo_from_url(&url).context("Failed to parse repo from URL")
}

/// Parse owner/repo from a git URL
/// Handles both HTTPS and SSH formats
fn parse_repo_from_url(url: &str) -> Result<String> {
    // Handle HTTPS format: https://github.com/owner/repo.git
    if url.starts_with("https://") || url.starts_with("http://") {
        let parts: Vec<&str> = url.split('/').collect();
        if parts.len() >= 2 {
            let owner = parts[parts.len() - 2];
            let repo = parts[parts.len() - 1].trim_end_matches(".git");
            return Ok(format!("{}/{}", owner, repo));
        }
    }
    // Handle SSH format: git@github.com:owner/repo.git
    else if url.contains(':') {
        if let Some(path_part) = url.split(':').nth(1) {
            let repo = path_part.trim_end_matches(".git");
            return Ok(repo.to_string());
        }
    }

    anyhow::bail!("Unable to parse repo from URL: {}", url)
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
    fn test_parse_repo_from_url() {
        // HTTPS format
        assert_eq!(
            parse_repo_from_url("https://github.com/fotoetienne/gru.git").unwrap(),
            "fotoetienne/gru"
        );

        assert_eq!(
            parse_repo_from_url("https://github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );

        // Without .git suffix
        assert_eq!(
            parse_repo_from_url("https://github.com/owner/repo").unwrap(),
            "owner/repo"
        );

        // SSH format
        assert_eq!(
            parse_repo_from_url("git@github.com:fotoetienne/gru.git").unwrap(),
            "fotoetienne/gru"
        );

        assert_eq!(
            parse_repo_from_url("git@github.com:owner/repo.git").unwrap(),
            "owner/repo"
        );

        // Without .git suffix
        assert_eq!(
            parse_repo_from_url("git@github.com:owner/repo").unwrap(),
            "owner/repo"
        );
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
