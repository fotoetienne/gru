use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Represents a Git repository with owner and repo name
#[allow(dead_code)]
pub struct GitRepo {
    pub owner: String,
    pub repo: String,
    pub bare_path: PathBuf,
}

#[allow(dead_code)]
impl GitRepo {
    /// Create a new GitRepo instance
    pub fn new(owner: impl Into<String>, repo: impl Into<String>, bare_path: PathBuf) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            bare_path,
        }
    }

    /// Ensures the repository is cloned as a bare repository
    /// If the repository doesn't exist, it will be cloned
    /// If it already exists, it will fetch the latest changes
    pub fn ensure_bare_clone(&self) -> Result<()> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .context("GRU_GITHUB_TOKEN environment variable not set")?;

        if token.is_empty() {
            anyhow::bail!("GRU_GITHUB_TOKEN environment variable is empty");
        }

        // Check if the bare repository already exists
        if self.bare_path.exists() {
            // Repository exists, fetch latest changes
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("fetch")
                .arg("--all")
                .status()
                .context("Failed to execute git fetch")?;

            if !status.success() {
                anyhow::bail!("git fetch failed with exit code: {:?}", status.code());
            }
        } else {
            // Clone as bare repository
            let url = format!(
                "https://{}@github.com/{}/{}.git",
                token, self.owner, self.repo
            );

            // Create parent directory if it doesn't exist
            if let Some(parent) = self.bare_path.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for bare repository")?;
            }

            let status = Command::new("git")
                .arg("clone")
                .arg("--bare")
                .arg(&url)
                .arg(&self.bare_path)
                .status()
                .context("Failed to execute git clone")?;

            if !status.success() {
                anyhow::bail!("git clone failed with exit code: {:?}", status.code());
            }
        }

        Ok(())
    }

    /// Creates a new worktree from the bare repository
    /// The worktree will have a new branch checked out
    pub fn create_worktree(&self, branch_name: &str, worktree_path: &Path) -> Result<()> {
        // Ensure the bare repository exists first
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}. Call ensure_bare_clone() first.",
                self.bare_path.display()
            );
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for worktree")?;
        }

        // Create the worktree with a new branch
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("add")
            .arg(worktree_path)
            .arg("-b")
            .arg(branch_name)
            .status()
            .context("Failed to execute git worktree add")?;

        if !status.success() {
            anyhow::bail!(
                "git worktree add failed with exit code: {:?}",
                status.code()
            );
        }

        Ok(())
    }

    /// Removes a worktree
    pub fn cleanup_worktree(&self, worktree_path: &Path) -> Result<()> {
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        let status = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("remove")
            .arg(worktree_path)
            .status()
            .context("Failed to execute git worktree remove")?;

        if !status.success() {
            anyhow::bail!(
                "git worktree remove failed with exit code: {:?}",
                status.code()
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_git_repo_new() {
        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/repo.git"));
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
        assert_eq!(repo.bare_path, PathBuf::from("/tmp/repo.git"));
    }

    #[test]
    fn test_ensure_bare_clone_fails_without_token() {
        // Save the current token value if it exists
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();

        // Remove the token
        env::remove_var("GRU_GITHUB_TOKEN");

        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/test-repo.git"));
        let result = repo.ensure_bare_clone();

        // Restore the original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        }

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GRU_GITHUB_TOKEN"));
    }

    #[test]
    fn test_ensure_bare_clone_fails_with_empty_token() {
        // Save the current token value if it exists
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();

        // Set an empty token
        env::set_var("GRU_GITHUB_TOKEN", "");

        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/test-repo.git"));
        let result = repo.ensure_bare_clone();

        // Restore the original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        } else {
            env::remove_var("GRU_GITHUB_TOKEN");
        }

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_create_worktree_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo.create_worktree("test-branch", Path::new("/tmp/test-worktree"));

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
    }

    #[test]
    fn test_cleanup_worktree_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo.cleanup_worktree(Path::new("/tmp/test-worktree"));

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
    }

    // Integration tests that actually clone a repository
    // These are marked with #[ignore] and should be run explicitly with:
    // cargo test git_operations -- --ignored
    #[test]
    #[ignore]
    fn test_git_operations_integration() {
        use std::fs;

        // This test requires GRU_GITHUB_TOKEN to be set
        if env::var("GRU_GITHUB_TOKEN").is_err() {
            eprintln!("Skipping integration test: GRU_GITHUB_TOKEN not set");
            return;
        }

        let temp_dir = env::temp_dir();
        let bare_path = temp_dir.join("test-gru-bare.git");
        let worktree_path = temp_dir.join("test-gru-worktree");

        // Clean up any existing test directories
        let _ = fs::remove_dir_all(&bare_path);
        let _ = fs::remove_dir_all(&worktree_path);

        // Test cloning a real repository (using the gru repo itself)
        let repo = GitRepo::new("fotoetienne", "gru", bare_path.clone());

        // Test ensure_bare_clone (first time - should clone)
        let result = repo.ensure_bare_clone();
        assert!(
            result.is_ok(),
            "Failed to clone bare repository: {:?}",
            result
        );
        assert!(bare_path.exists(), "Bare repository was not created");

        // Test ensure_bare_clone (second time - should fetch)
        let result = repo.ensure_bare_clone();
        assert!(
            result.is_ok(),
            "Failed to fetch in existing repository: {:?}",
            result
        );

        // Test create_worktree
        let result = repo.create_worktree("test-branch", &worktree_path);
        assert!(result.is_ok(), "Failed to create worktree: {:?}", result);
        assert!(worktree_path.exists(), "Worktree was not created");

        // Verify the worktree has the correct branch
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&worktree_path)
            .arg("branch")
            .arg("--show-current")
            .output()
            .expect("Failed to check branch");

        let branch_name = String::from_utf8_lossy(&branch_check.stdout);
        assert_eq!(branch_name.trim(), "test-branch");

        // Test cleanup_worktree
        let result = repo.cleanup_worktree(&worktree_path);
        assert!(result.is_ok(), "Failed to cleanup worktree: {:?}", result);

        // Clean up test directories
        let _ = fs::remove_dir_all(&bare_path);
        let _ = fs::remove_dir_all(&worktree_path);
    }
}
