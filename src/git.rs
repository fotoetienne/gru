use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Validates a branch name according to Git ref naming rules
fn validate_branch_name(branch_name: &str) -> Result<()> {
    if branch_name.is_empty() {
        anyhow::bail!("Branch name cannot be empty");
    }

    if branch_name.starts_with('-') {
        anyhow::bail!("Branch name cannot start with '-'");
    }

    // Git ref name validation
    if branch_name.contains("..")
        || branch_name.contains("@{")
        || branch_name.contains('\\')
        || branch_name.ends_with('.')
        || branch_name.ends_with(".lock")
        || branch_name.contains('\x00')
    {
        anyhow::bail!("Invalid branch name: {}", branch_name);
    }

    Ok(())
}

/// Represents a Git repository with owner and repo name
#[allow(dead_code)]
pub struct GitRepo {
    owner: String,
    repo: String,
    bare_path: PathBuf,
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
    ///
    /// Authentication is handled in the following order:
    /// 1. If `GRU_GITHUB_TOKEN` is set, use it via credential helper
    /// 2. Otherwise, use system git credentials (SSH keys, credential helpers, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The git clone or fetch command fails (network issues, authentication, etc.)
    /// - Unable to create parent directories
    pub fn ensure_bare_clone(&self) -> Result<()> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        // Check if the bare repository already exists
        if self.bare_path.exists() {
            // Repository exists, fetch latest changes
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("fetch")
                .arg("--all")
                .output()
                .context("Failed to execute git fetch")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git fetch failed with exit code {:?}: {}",
                    output.status.code(),
                    stderr
                );
            }
        } else {
            // Clone as bare repository
            let url = format!("https://github.com/{}/{}.git", self.owner, self.repo);

            // Create parent directory if it doesn't exist
            if let Some(parent) = self.bare_path.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for bare repository")?;
            }

            let mut cmd = Command::new("git");

            // If token is provided, use credential helper to provide it securely
            // Otherwise, rely on system git credentials (SSH keys, credential helpers, etc.)
            if let Some(token) = token {
                cmd.arg("-c").arg(format!(
                    "credential.helper=!f() {{ echo username=oauth2; echo password={}; }}; f",
                    token
                ));
            }

            cmd.arg("clone")
                .arg("--bare")
                .arg(&url)
                .arg(&self.bare_path)
                .env("GIT_TERMINAL_PROMPT", "0"); // Disable interactive prompts

            let output = cmd.output().context("Failed to execute git clone")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git clone failed with exit code {:?}: {}",
                    output.status.code(),
                    stderr
                );
            }
        }

        Ok(())
    }

    /// Creates a new worktree from the bare repository
    /// The worktree will have a new branch checked out
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The branch name is invalid or already exists
    /// - The worktree path already exists
    /// - Git worktree creation fails
    pub fn create_worktree(&self, branch_name: &str, worktree_path: &Path) -> Result<()> {
        // Validate branch name
        validate_branch_name(branch_name)?;

        // Ensure the bare repository exists first
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}. Call ensure_bare_clone() first.",
                self.bare_path.display()
            );
        }

        // Check if worktree path already exists
        if worktree_path.exists() {
            anyhow::bail!(
                "Path already exists: {}. Remove it first or choose a different path.",
                worktree_path.display()
            );
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for worktree")?;
        }

        // Create the worktree with a new branch
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("add")
            .arg(worktree_path)
            .arg("-b")
            .arg(branch_name)
            .output()
            .context("Failed to execute git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree add failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        Ok(())
    }

    /// Removes a worktree
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The worktree removal fails (e.g., worktree doesn't exist or has uncommitted changes)
    pub fn cleanup_worktree(&self, worktree_path: &Path) -> Result<()> {
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("remove")
            .arg(worktree_path)
            .output()
            .context("Failed to execute git worktree remove")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree remove failed with exit code {:?}: {}",
                output.status.code(),
                stderr
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

    #[test]
    fn test_create_worktree_rejects_empty_branch_name() {
        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/test-repo.git"));
        let result = repo.create_worktree("", Path::new("/tmp/test-worktree"));

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Branch name cannot be empty"));
    }

    #[test]
    fn test_create_worktree_rejects_branch_starting_with_dash() {
        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/test-repo.git"));
        let result = repo.create_worktree("-branch", Path::new("/tmp/test-worktree"));

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Branch name cannot start with '-'"));
    }

    #[test]
    fn test_create_worktree_rejects_invalid_branch_names() {
        let repo = GitRepo::new("owner", "repo", PathBuf::from("/tmp/test-repo.git"));

        // Test various invalid branch names
        let invalid_names = vec![
            "branch..name",
            "branch@{name",
            "branch\\name",
            "branch.",
            "branch.lock",
        ];

        for name in invalid_names {
            let result = repo.create_worktree(name, Path::new("/tmp/test-worktree"));
            assert!(
                result.is_err(),
                "Expected '{}' to be rejected as invalid",
                name
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid branch name"),
                "Expected error message about invalid branch name for '{}'",
                name
            );
        }
    }

    // Integration tests that actually clone a repository
    // These are marked with #[ignore] and should be run explicitly with:
    // cargo test git_operations -- --ignored
    //
    // Note: This test will use GRU_GITHUB_TOKEN if set, otherwise it will
    // fall back to system git credentials (SSH keys, credential helpers, etc.)
    #[test]
    #[ignore]
    fn test_git_operations_integration() {
        use std::fs;

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
