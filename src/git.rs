use anyhow::{Context, Result};

use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Default branch names to try when creating new worktrees, in priority order.
///
/// Bare repositories may store branches either:
/// - Directly (e.g., "main") - newer repos cloned with --bare
/// - With remote prefix (e.g., "origin/main") - repos with mirror-style refspec
///
/// We try both patterns to handle existing repos with different configurations.
const DEFAULT_BRANCHES: &[&str] = &["main", "origin/main", "master", "origin/master"];

/// Creates a git `Command` pre-configured with credential helper authentication.
///
/// If a token is provided, the command will include `-c credential.helper=...` to
/// inject the token via a credential helper. This avoids duplicating the credential
/// setup logic across clone and fetch operations.
///
/// Note: Git spawns a shell to execute credential.helper values prefixed with `!`.
/// The single-quote escaping applies at the shell level that git invokes,
/// not at the `Command::arg` level (which does not invoke a shell).
fn git_command_with_auth(token: Option<&str>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(token) = token {
        let safe_token = token.replace('\'', "'\\''");
        cmd.arg("-c").arg(format!(
            "credential.helper=!f() {{ echo username=oauth2; echo password='{}'; }}; f",
            safe_token
        ));
    }
    cmd
}

/// Detects if the current directory is within a git repository
/// Returns the root path of the git repository
pub async fn detect_git_repo() -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .await
        .context("Failed to execute git rev-parse")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Not in a git repository. Run from within a git repository or provide the full GitHub URL.\n{}",
            stderr.trim()
        );
    }

    let path_str = String::from_utf8(output.stdout)
        .context("Git output is not valid UTF-8")?
        .trim()
        .to_string();

    Ok(PathBuf::from(path_str))
}

/// Gets the GitHub remote URL from the current git repository
/// Tries "origin" first, then falls back to the first GitHub remote found
pub async fn get_github_remote(github_hosts: &[String]) -> Result<String> {
    // Use `git remote -v` to get all remotes and their URLs in one call
    let output = Command::new("git")
        .arg("remote")
        .arg("-v")
        .output()
        .await
        .context("Failed to execute git remote -v")?;

    if !output.status.success() {
        anyhow::bail!("Failed to list git remotes");
    }

    let remote_lines =
        String::from_utf8(output.stdout).context("Git remote -v output is not valid UTF-8")?;

    // Parse remotes, prioritizing "origin"
    let mut origin_url: Option<String> = None;
    let mut first_github_url: Option<String> = None;

    // Each line format: <name> <url> (fetch|push)
    for line in remote_lines.lines() {
        let mut parts = line.split_whitespace();
        let remote_name = parts.next();
        let remote_url = parts.next();

        if let (Some(name), Some(url)) = (remote_name, remote_url) {
            if is_github_url(url, github_hosts) {
                // Prioritize "origin" remote
                if name == "origin" && origin_url.is_none() {
                    origin_url = Some(url.to_string());
                } else if first_github_url.is_none() {
                    first_github_url = Some(url.to_string());
                }
            }
        }
    }

    // Return origin if found, otherwise return first GitHub remote
    origin_url.or(first_github_url).ok_or_else(|| {
        anyhow::anyhow!(
            "No GitHub remote found. Add a GitHub remote or provide the full issue URL.\n\
                 Example: git remote add origin https://github.com/owner/repo.git"
        )
    })
}

/// Checks if a URL is a GitHub URL (including configured GHE hosts).
///
/// `github_hosts` should contain all recognized hosts (e.g., `["github.com", "ghe.example.com"]`).
fn is_github_url(url: &str, github_hosts: &[String]) -> bool {
    for host in github_hosts {
        if url.starts_with(&format!("https://{}/", host))
            || url.starts_with(&format!("http://{}/", host))
            || url.starts_with(&format!("git@{}:", host))
        {
            return true;
        }
    }
    false
}

/// Parses a GitHub remote URL to extract host, owner, and repo name.
///
/// Supports both HTTPS and SSH formats for any configured GitHub host:
/// - `https://<host>/owner/repo.git`
/// - `git@<host>:owner/repo.git`
///
/// `github_hosts` should contain all recognized hosts (e.g., `["github.com", "ghe.example.com"]`).
pub fn parse_github_remote(url: &str, github_hosts: &[String]) -> Result<(String, String, String)> {
    if !is_github_url(url, github_hosts) {
        anyhow::bail!("Not a GitHub URL: {}", url);
    }

    for host in github_hosts {
        let https_prefix = format!("https://{}/", host);
        let http_prefix = format!("http://{}/", host);
        let ssh_prefix = format!("git@{}:", host);

        // Handle HTTPS format
        if url.starts_with(&https_prefix) || url.starts_with(&http_prefix) {
            let path = url
                .trim_start_matches(&https_prefix)
                .trim_start_matches(&http_prefix)
                .trim_end_matches(".git")
                .trim_end_matches('/');

            let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Ok((host.clone(), parts[0].to_string(), parts[1].to_string()));
            }
        }

        // Handle SSH format: git@<host>:owner/repo.git
        if url.starts_with(&ssh_prefix) {
            let path = url
                .trim_start_matches(&ssh_prefix)
                .trim_end_matches(".git")
                .trim_end_matches('/');

            let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Ok((host.clone(), parts[0].to_string(), parts[1].to_string()));
            }
        }
    }

    anyhow::bail!("Could not parse GitHub URL: {}", url);
}

/// Represents a single entry from `git worktree list --porcelain` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Filesystem path to the worktree
    pub path: PathBuf,
    /// Branch name (without refs/heads/ prefix), None for detached HEAD or bare entries
    pub branch: Option<String>,
}

/// Parses `git worktree list --porcelain` output into structured entries.
///
/// This is the single source of truth for porcelain parsing, used by both
/// `GitRepo::find_worktree_for_branch` and the worktree scanner module.
///
/// Assumes well-formed porcelain output where each stanza is terminated by a
/// blank line. If a `worktree` line appears without a preceding blank line,
/// the in-progress entry is silently discarded (consistent with git's guarantee
/// for `--porcelain` output).
pub fn parse_porcelain_worktrees(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for raw_line in output.lines() {
        // Normalize CRLF: str::lines() splits on \n but doesn't strip \r
        let line = raw_line.trim_end();
        if line.starts_with("worktree ") {
            current_branch = None; // reset stale branch from prior incomplete stanza
            current_path = Some(PathBuf::from(line.strip_prefix("worktree ").unwrap()));
        } else if line.starts_with("branch ") {
            let branch_ref = line.strip_prefix("branch ").unwrap();
            current_branch = branch_ref
                .strip_prefix("refs/heads/")
                .map(|s| s.to_string());
        } else if line.is_empty() {
            if let Some(path) = current_path.take() {
                entries.push(WorktreeEntry {
                    path,
                    branch: current_branch.take(),
                });
            }
            current_branch = None;
        }
    }

    // Handle last entry if output doesn't end with a blank line
    if let Some(path) = current_path {
        entries.push(WorktreeEntry {
            path,
            branch: current_branch,
        });
    }

    entries
}

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

/// Configures git hooks in a worktree if a `.githooks/` directory exists.
///
/// Projects that use the `.githooks/` convention get `core.hooksPath` set
/// automatically so that pre-commit hooks (formatting, linting, tests) run
/// inside minion worktrees — preventing trivial CI failures.
async fn configure_hooks(worktree_path: &Path) -> Result<()> {
    let githooks_dir = worktree_path.join(".githooks");
    if !githooks_dir.is_dir() {
        return Ok(());
    }

    // Check if core.hooksPath is already set — don't overwrite existing config
    let existing = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("config")
        .arg("core.hooksPath")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
        .ok();

    if let Some(ref out) = existing {
        if out.status.success() {
            let current = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !current.is_empty() && current != ".githooks" {
                log::debug!(
                    "core.hooksPath already set to '{}' in {}, skipping",
                    current,
                    worktree_path.display()
                );
                return Ok(());
            }
            if current == ".githooks" {
                return Ok(());
            }
        }
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("config")
        .arg("core.hooksPath")
        .arg(".githooks")
        // Clear git env vars that may be inherited from a parent git process
        // (e.g., pre-commit hook), which would cause git to operate on the
        // parent repo instead of the target worktree.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
        .context("Failed to execute git config core.hooksPath")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to configure git hooks: {}", stderr);
    }

    log::info!(
        "Configured core.hooksPath=.githooks in {}",
        worktree_path.display()
    );
    Ok(())
}

/// Represents a Git repository with owner and repo name
pub struct GitRepo {
    owner: String,
    repo: String,
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    host: String,
    bare_path: PathBuf,
}

impl GitRepo {
    /// Create a new GitRepo instance
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        host: impl Into<String>,
        bare_path: PathBuf,
    ) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            host: host.into(),
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
    pub async fn ensure_bare_clone(&self) -> Result<()> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        // Check if the bare repository already exists
        if self.bare_path.exists() {
            // Fetch only the default branch to keep it up to date for new worktree creation.
            // We avoid fetching all branches (refs/heads/*) because git refuses to update
            // any ref that is checked out in a worktree, causing the entire fetch to fail.
            // Feature branches are fetched on demand via fetch_branch().
            let mut fetched = false;
            for branch in DEFAULT_BRANCHES {
                // Skip origin/-prefixed entries; we fetch from origin by branch name
                if branch.starts_with("origin/") {
                    continue;
                }

                let output = git_command_with_auth(token.as_deref())
                    .arg("-C")
                    .arg(&self.bare_path)
                    .arg("fetch")
                    .arg("origin")
                    .arg(format!("+refs/heads/{}:refs/heads/{}", branch, branch))
                    .output()
                    .await
                    .context("Failed to execute git fetch")?;

                if output.status.success() {
                    fetched = true;
                    break;
                }

                let stderr = String::from_utf8_lossy(&output.stderr);

                // If the default branch is checked out in a worktree, git refuses
                // to update it. Treat this like a missing branch and try the next.
                if stderr.contains("refusing to fetch into branch")
                    && stderr.contains("checked out at")
                {
                    log::debug!(
                        "Branch '{}' is checked out in a worktree, skipping fetch",
                        branch
                    );
                    continue;
                }

                // Only continue to the next candidate if this branch doesn't exist on remote.
                // Different git versions may produce different messages, so check several.
                // For any other failure (auth, network, etc.), propagate the error immediately.
                let is_missing_ref = stderr.contains("couldn't find remote ref")
                    || stderr.contains("could not find remote ref")
                    || stderr.contains("no such ref")
                    || stderr.contains("unknown revision");

                if !is_missing_ref {
                    anyhow::bail!(
                        "git fetch failed for branch '{}' with exit code {:?}: {}",
                        branch,
                        output.status.code(),
                        stderr.trim()
                    );
                }
            }

            if !fetched {
                log::warn!(
                    "Could not fetch any default branch ({}). \
                     The local copy may be stale, but feature branches will be fetched on demand.",
                    DEFAULT_BRANCHES
                        .iter()
                        .filter(|b| !b.starts_with("origin/"))
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        } else {
            // Clone as bare repository
            let url = format!("https://{}/{}/{}.git", self.host, self.owner, self.repo);

            // Create parent directory if it doesn't exist
            if let Some(parent) = self.bare_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context("Failed to create parent directory for bare repository")?;
            }

            let mut cmd = git_command_with_auth(token.as_deref());

            cmd.arg("clone")
                .arg("--bare")
                .arg(&url)
                .arg(&self.bare_path)
                .env("GIT_TERMINAL_PROMPT", "0"); // Disable interactive prompts

            let output = cmd.output().await.context("Failed to execute git clone")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git clone failed with exit code {:?}: {}",
                    output.status.code(),
                    stderr
                );
            }

            // Configure fetch refspec so future fetches update local branches directly
            // (git clone --bare doesn't set this by default)
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("config")
                .arg("remote.origin.fetch")
                .arg("+refs/heads/*:refs/heads/*")
                .output()
                .await
                .context("Failed to execute git config for remote.origin.fetch")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git config remote.origin.fetch failed with exit code {:?}: {}",
                    output.status.code(),
                    stderr
                );
            }
        }

        Ok(())
    }

    /// Determines the default branch to use as base for new worktrees
    ///
    /// Queries the remote repository to discover the actual default branch dynamically.
    /// This works with any default branch name (main, master, develop, trunk, etc.).
    ///
    /// Falls back to [`DEFAULT_BRANCHES`] if remote query fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - Remote query fails and fallback branches don't exist
    async fn get_base_branch(&self) -> Result<String> {
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        // Query the remote to discover the default branch
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("ls-remote")
            .arg("--symref")
            .arg("origin")
            .arg("HEAD")
            .output()
            .await
            .context("Failed to query remote for default branch")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse: "ref: refs/heads/main\tHEAD"
            if let Some(line) = stdout.lines().next() {
                if let Some(branch_name) = line
                    .strip_prefix("ref: refs/heads/")
                    .and_then(|s| s.split('\t').next())
                {
                    // Bare repos may store branches either directly ("main") or with
                    // remote prefix ("origin/main") depending on how they were cloned.
                    // Try both patterns and return whichever exists.
                    for candidate in [branch_name.to_string(), format!("origin/{}", branch_name)] {
                        let check = Command::new("git")
                            .arg("-C")
                            .arg(&self.bare_path)
                            .arg("rev-parse")
                            .arg("--verify")
                            .arg(&candidate)
                            .output()
                            .await;

                        if let Ok(result) = check {
                            if result.status.success() {
                                return Ok(candidate);
                            }
                        }
                    }
                }
            }
        }

        // Fallback to trying common default branch names
        for branch in DEFAULT_BRANCHES {
            let check = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("rev-parse")
                .arg("--verify")
                .arg(branch)
                .output()
                .await
                .with_context(|| format!("Failed to check if branch '{}' exists", branch))?;

            if check.status.success() {
                return Ok(branch.to_string());
            }
        }

        anyhow::bail!(
            "Could not determine default branch from remote and fallback branches not found. \
             Tried: {}. Ensure the repository has been fetched with ensure_bare_clone().",
            DEFAULT_BRANCHES.join(", ")
        )
    }

    /// Creates a new worktree from the bare repository
    /// The worktree will have a new branch checked out
    ///
    /// If the branch already exists (from a previous minion), it will check it out.
    /// If the branch doesn't exist, it will be created based on the repository's default
    /// branch (as determined by querying the remote, e.g., origin/main, origin/master, origin/develop, origin/trunk, etc.).
    /// If git reports that the worktree is already checked out elsewhere, this will fail
    /// with an error (respecting git's internal locking).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The branch name is invalid
    /// - The branch is already checked out in another worktree
    /// - Git worktree creation fails
    pub async fn create_worktree(&self, branch_name: &str, worktree_path: &Path) -> Result<()> {
        // Validate branch name
        validate_branch_name(branch_name)?;

        // Ensure the bare repository exists first
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}. Call ensure_bare_clone() first.",
                self.bare_path.display()
            );
        }

        // Check if the branch already exists
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("show-ref")
            .arg("--verify")
            .arg(format!("refs/heads/{}", branch_name))
            .output()
            .await
            .context("Failed to check if branch exists")?;

        let branch_exists = branch_check.status.success();

        // Create or checkout the worktree
        // Let git handle directory creation and locking
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("add")
            .arg(worktree_path);

        if branch_exists {
            // Branch exists, just check it out
            cmd.arg(branch_name);
        } else {
            // Branch doesn't exist, create it based on the default branch
            let base_branch = self.get_base_branch().await?;
            cmd.arg("-b").arg(branch_name).arg(base_branch);
        }

        let output = cmd
            .output()
            .await
            .context("Failed to execute git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Provide helpful error messages for common cases
            if stderr.contains("already checked out") {
                anyhow::bail!(
                    "Branch '{}' is already checked out in another worktree. \
                     Another minion may be working on this issue. \
                     Check active worktrees with: git -C {} worktree list",
                    branch_name,
                    self.bare_path.display()
                );
            }

            anyhow::bail!(
                "git worktree add failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        if let Err(e) = configure_hooks(worktree_path).await {
            log::warn!(
                "Failed to configure git hooks in {}: {}",
                worktree_path.display(),
                e
            );
        }

        Ok(())
    }

    /// Creates a worktree for an existing branch
    /// Unlike create_worktree, this checks out an existing branch instead of creating a new one
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The branch name is invalid or doesn't exist
    /// - The worktree path already exists
    /// - Git worktree creation fails
    pub async fn checkout_worktree(&self, branch_name: &str, worktree_path: &Path) -> Result<()> {
        // Validate branch name
        validate_branch_name(branch_name)?;

        // Ensure the bare repository exists first
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}. Call ensure_bare_clone() first.",
                self.bare_path.display()
            );
        }

        // Check if worktree path already exists (defensive check)
        // Callers should check for existence first to provide better error messages
        if worktree_path.exists() {
            anyhow::bail!(
                "Worktree path already exists: {}. This is likely a programming error - \
                 the caller should check for existing worktrees before calling this method.",
                worktree_path.display()
            );
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = worktree_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("Failed to create parent directory for worktree")?;
        }

        // Create the worktree for an existing branch (no -b flag)
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("add")
            .arg(worktree_path)
            .arg(branch_name)
            .output()
            .await
            .context("Failed to execute git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree add failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        if let Err(e) = configure_hooks(worktree_path).await {
            log::warn!(
                "Failed to configure git hooks in {}: {}",
                worktree_path.display(),
                e
            );
        }

        Ok(())
    }

    /// Finds an existing worktree that has the specified branch checked out
    ///
    /// Uses `git worktree list --porcelain` to get machine-readable output
    /// and parses it to find if the branch is currently checked out in any worktree.
    ///
    /// Note: This only matches worktrees with branches checked out, not:
    /// - Detached HEAD worktrees
    /// - The bare repository itself
    ///
    /// # Arguments
    ///
    /// * `branch_name` - The branch name without the `refs/heads/` prefix
    ///   (e.g., "main" or "minion/issue-64-M0u1")
    ///
    /// # Returns
    ///
    /// - `Ok(Some(PathBuf))` if a worktree with the branch is found
    /// - `Ok(None)` if no worktree has the branch checked out
    /// - `Err` if the git command fails or the bare repository doesn't exist
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The branch name is invalid
    /// - The bare repository doesn't exist
    /// - The git worktree list command fails
    pub async fn find_worktree_for_branch(&self, branch_name: &str) -> Result<Option<PathBuf>> {
        // Validate branch name
        validate_branch_name(branch_name)?;

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
            .arg("list")
            .arg("--porcelain")
            .output()
            .await
            .context("Failed to execute git worktree list")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree list failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let entries = parse_porcelain_worktrees(&stdout);

        Ok(entries.into_iter().find_map(|entry| {
            if entry.branch.as_deref() == Some(branch_name) {
                Some(entry.path)
            } else {
                None
            }
        }))
    }

    /// Removes a worktree
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The worktree removal fails (e.g., worktree doesn't exist or has uncommitted changes)
    #[allow(dead_code)] // Part of worktree API; clean.rs uses inline git commands currently
    pub async fn cleanup_worktree(&self, worktree_path: &Path) -> Result<()> {
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
            .await
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

    /// Removes a worktree forcefully, handling stale or locked worktrees
    ///
    /// This is useful when a worktree is locked or stale from a previous minion session.
    /// It uses the `--force` flag to bypass checks for locks, but will refuse to remove
    /// a worktree with uncommitted changes to prevent data loss.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The worktree has uncommitted changes (safety check)
    /// - The worktree removal fails
    #[allow(dead_code)] // Part of worktree API; clean.rs uses inline git commands currently
    pub async fn cleanup_worktree_force(&self, worktree_path: &Path) -> Result<()> {
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        // Safety check: refuse to force-remove worktree with uncommitted changes
        // First check if this is a valid git worktree
        if worktree_path.exists() {
            let is_worktree = Command::new("git")
                .arg("-C")
                .arg(worktree_path)
                .arg("rev-parse")
                .arg("--is-inside-work-tree")
                .output()
                .await;

            // If it's a valid worktree, check for uncommitted changes
            if let Ok(output) = is_worktree {
                if output.status.success() {
                    let status = Command::new("git")
                        .arg("-C")
                        .arg(worktree_path)
                        .arg("status")
                        .arg("--porcelain")
                        .output()
                        .await;

                    if let Ok(status_output) = status {
                        if !status_output.stdout.is_empty() {
                            anyhow::bail!(
                                "Worktree at {} has uncommitted changes. Refusing to force-remove. \
                                 Commit or stash changes first.",
                                worktree_path.display()
                            );
                        }
                    }
                }
            }
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path)
            .output()
            .await
            .context("Failed to execute git worktree remove --force")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree remove --force failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        Ok(())
    }

    /// Fetches the latest changes for a branch in the bare repository
    /// This is useful for updating an existing worktree's branch before checking it out
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The branch name is invalid
    /// - The git fetch command fails
    pub async fn fetch_branch(&self, branch_name: &str) -> Result<()> {
        // Validate branch name using existing validation
        validate_branch_name(branch_name)?;

        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        // Fetch the specific branch with explicit refspec
        // +refs/heads/branch:refs/heads/branch ensures we:
        // 1. Fetch from the remote's refs/heads/branch
        // 2. Update the local refs/heads/branch (even if non-fast-forward due to +)
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("fetch")
            .arg("origin")
            .arg(format!(
                "+refs/heads/{}:refs/heads/{}",
                branch_name, branch_name
            ))
            .output()
            .await
            .context("Failed to execute git fetch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to fetch branch '{}' from origin in repository {}: git fetch exited with code {:?}: {}",
                branch_name,
                self.bare_path.display(),
                output.status.code(),
                stderr.trim()
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn default_hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    fn hosts_with_ghe() -> Vec<String> {
        vec!["github.com".to_string(), "ghe.example.com".to_string()]
    }

    #[test]
    fn test_parse_github_remote_https() {
        let result =
            parse_github_remote("https://github.com/owner/repo.git", &default_hosts()).unwrap();
        assert_eq!(result.0, "github.com");
        assert_eq!(result.1, "owner");
        assert_eq!(result.2, "repo");
    }

    #[test]
    fn test_parse_github_remote_https_without_git_extension() {
        let result =
            parse_github_remote("https://github.com/owner/repo", &default_hosts()).unwrap();
        assert_eq!(result.0, "github.com");
        assert_eq!(result.1, "owner");
        assert_eq!(result.2, "repo");
    }

    #[test]
    fn test_parse_github_remote_ssh() {
        let result =
            parse_github_remote("git@github.com:owner/repo.git", &default_hosts()).unwrap();
        assert_eq!(result.0, "github.com");
        assert_eq!(result.1, "owner");
        assert_eq!(result.2, "repo");
    }

    #[test]
    fn test_parse_github_remote_ssh_without_git_extension() {
        let result = parse_github_remote("git@github.com:owner/repo", &default_hosts()).unwrap();
        assert_eq!(result.0, "github.com");
        assert_eq!(result.1, "owner");
        assert_eq!(result.2, "repo");
    }

    #[test]
    fn test_parse_github_remote_ghe_https() {
        let result = parse_github_remote(
            "https://ghe.example.com/netflix/service.git",
            &hosts_with_ghe(),
        )
        .unwrap();
        assert_eq!(result.0, "ghe.example.com");
        assert_eq!(result.1, "netflix");
        assert_eq!(result.2, "service");
    }

    #[test]
    fn test_parse_github_remote_ghe_ssh() {
        let result =
            parse_github_remote("git@ghe.example.com:netflix/service.git", &hosts_with_ghe())
                .unwrap();
        assert_eq!(result.0, "ghe.example.com");
        assert_eq!(result.1, "netflix");
        assert_eq!(result.2, "service");
    }

    #[test]
    fn test_parse_github_remote_rejects_non_github() {
        let result = parse_github_remote("https://gitlab.com/owner/repo.git", &default_hosts());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not a GitHub URL"));
    }

    #[test]
    fn test_parse_github_remote_rejects_unconfigured_host() {
        // ghe.example.com not in default hosts
        let result =
            parse_github_remote("https://ghe.example.com/owner/repo.git", &default_hosts());
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_github_remote_rejects_invalid_format() {
        let result = parse_github_remote("https://github.com/incomplete", &default_hosts());
        assert!(result.is_err());
    }

    #[test]
    fn test_is_github_url() {
        let hosts = default_hosts();
        // Valid GitHub URLs
        assert!(is_github_url("https://github.com/owner/repo.git", &hosts));
        assert!(is_github_url("http://github.com/owner/repo.git", &hosts));
        assert!(is_github_url("git@github.com:owner/repo.git", &hosts));

        // Invalid - not GitHub
        assert!(!is_github_url("https://gitlab.com/owner/repo.git", &hosts));

        // Invalid - security: malicious URLs that contain "github.com" but aren't GitHub
        assert!(!is_github_url(
            "https://evil.com/github.com/malware.git",
            &hosts
        ));
        assert!(!is_github_url(
            "https://github.com.attacker.com/repo.git",
            &hosts
        ));
        assert!(!is_github_url(
            "user@attacker.com:github.com:malware.git",
            &hosts
        ));
    }

    #[test]
    fn test_is_github_url_with_ghe() {
        let hosts = hosts_with_ghe();
        assert!(is_github_url(
            "https://ghe.example.com/owner/repo.git",
            &hosts
        ));
        assert!(is_github_url("git@ghe.example.com:owner/repo.git", &hosts));
    }

    #[test]
    fn test_parse_porcelain_worktrees_basic() {
        let output = "worktree /repos/owner_repo.git\nHEAD abc123\nbare\n\nworktree /work/issue-36\nHEAD def456\nbranch refs/heads/issue-36\n\n";
        let entries = parse_porcelain_worktrees(output);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/repos/owner_repo.git"));
        assert_eq!(entries[0].branch, None); // bare entry has no branch
        assert_eq!(entries[1].path, PathBuf::from("/work/issue-36"));
        assert_eq!(entries[1].branch.as_deref(), Some("issue-36"));
    }

    #[test]
    fn test_parse_porcelain_worktrees_no_trailing_newline() {
        let output = "worktree /work/feature\nHEAD abc123\nbranch refs/heads/feature-branch";
        let entries = parse_porcelain_worktrees(output);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/work/feature"));
        assert_eq!(entries[0].branch.as_deref(), Some("feature-branch"));
    }

    #[test]
    fn test_parse_porcelain_worktrees_detached_head() {
        let output = "worktree /work/detached\nHEAD abc123\ndetached\n\n";
        let entries = parse_porcelain_worktrees(output);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/work/detached"));
        assert_eq!(entries[0].branch, None);
    }

    #[test]
    fn test_parse_porcelain_worktrees_empty_output() {
        let entries = parse_porcelain_worktrees("");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_porcelain_worktrees_crlf() {
        let output = "worktree /work/issue-1\r\nHEAD abc123\r\nbranch refs/heads/issue-1\r\n\r\n";
        let entries = parse_porcelain_worktrees(output);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/work/issue-1"));
        assert_eq!(entries[0].branch.as_deref(), Some("issue-1"));
    }

    #[test]
    fn test_git_repo_new() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/repo.git"),
        );
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.bare_path, PathBuf::from("/tmp/repo.git"));
    }

    #[tokio::test]
    async fn test_create_worktree_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo
            .create_worktree("test-branch", Path::new("/tmp/test-worktree"))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
    }

    #[tokio::test]
    async fn test_cleanup_worktree_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo.cleanup_worktree(Path::new("/tmp/test-worktree")).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
    }

    #[tokio::test]
    async fn test_create_worktree_rejects_empty_branch_name() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/test-repo.git"),
        );
        let result = repo
            .create_worktree("", Path::new("/tmp/test-worktree"))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Branch name cannot be empty"));
    }

    #[tokio::test]
    async fn test_create_worktree_rejects_branch_starting_with_dash() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/test-repo.git"),
        );
        let result = repo
            .create_worktree("-branch", Path::new("/tmp/test-worktree"))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Branch name cannot start with '-'"));
    }

    #[tokio::test]
    async fn test_create_worktree_rejects_invalid_branch_names() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/test-repo.git"),
        );

        // Test various invalid branch names
        let invalid_names = vec![
            "branch..name",
            "branch@{name",
            "branch\\name",
            "branch.",
            "branch.lock",
        ];

        for name in invalid_names {
            let result = repo
                .create_worktree(name, Path::new("/tmp/test-worktree"))
                .await;
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

    #[tokio::test]
    async fn test_find_worktree_for_branch_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            "github.com",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo.find_worktree_for_branch("test-branch").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
    }

    /// Creates a git Command with GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE cleared
    /// so tests work correctly even when run from a pre-commit hook.
    fn clean_git_cmd() -> Command {
        let mut cmd = Command::new("git");
        cmd.env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        cmd
    }

    #[tokio::test]
    async fn test_configure_hooks_sets_hooks_path_when_githooks_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();

        // Initialize a git repo
        let init_output = clean_git_cmd()
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .await
            .unwrap();
        assert!(
            init_output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init_output.stderr)
        );

        // Create .githooks directory
        std::fs::create_dir_all(dir.join(".githooks")).unwrap();

        // Run configure_hooks
        let result = configure_hooks(dir).await;
        assert!(result.is_ok(), "configure_hooks failed: {:?}", result);

        // Verify core.hooksPath was set locally
        let output = clean_git_cmd()
            .arg("-C")
            .arg(dir)
            .arg("config")
            .arg("--local")
            .arg("core.hooksPath")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git config --local core.hooksPath failed: status={:?}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            ".githooks",
            "unexpected core.hooksPath value"
        );
    }

    #[tokio::test]
    async fn test_configure_hooks_skips_when_no_githooks_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();

        // Initialize a git repo (no .githooks directory)
        let init_output = clean_git_cmd()
            .arg("-C")
            .arg(dir)
            .arg("init")
            .output()
            .await
            .unwrap();
        assert!(
            init_output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init_output.stderr)
        );

        // Run configure_hooks — should be a no-op
        let result = configure_hooks(dir).await;
        assert!(result.is_ok(), "configure_hooks failed: {:?}", result);

        // Verify core.hooksPath was NOT set locally
        let output = clean_git_cmd()
            .arg("-C")
            .arg(dir)
            .arg("config")
            .arg("--local")
            .arg("core.hooksPath")
            .output()
            .await
            .unwrap();
        assert!(
            !output.status.success(),
            "core.hooksPath should not be set locally when .githooks doesn't exist"
        );
    }

    // Integration tests that actually clone a repository
    // These are marked with #[ignore] and should be run explicitly with:
    // cargo test git_operations -- --ignored
    //
    // Note: This test will use GRU_GITHUB_TOKEN if set, otherwise it will
    // fall back to system git credentials (SSH keys, credential helpers, etc.)
    #[tokio::test]
    #[ignore]
    async fn test_git_operations_integration() {
        use std::fs;

        let temp_dir = env::temp_dir();
        let bare_path = temp_dir.join("test-gru-bare.git");
        let worktree_path = temp_dir.join("test-gru-worktree");

        // Clean up any existing test directories
        let _ = fs::remove_dir_all(&bare_path);
        let _ = fs::remove_dir_all(&worktree_path);

        // Test cloning a real repository (using the gru repo itself)
        let repo = GitRepo::new("fotoetienne", "gru", "github.com", bare_path.clone());

        // Test ensure_bare_clone (first time - should clone)
        let result = repo.ensure_bare_clone().await;
        assert!(
            result.is_ok(),
            "Failed to clone bare repository: {:?}",
            result
        );
        assert!(bare_path.exists(), "Bare repository was not created");

        // Test ensure_bare_clone (second time - should fetch)
        let result = repo.ensure_bare_clone().await;
        assert!(
            result.is_ok(),
            "Failed to fetch in existing repository: {:?}",
            result
        );

        // Test create_worktree
        let result = repo.create_worktree("test-branch", &worktree_path).await;
        assert!(result.is_ok(), "Failed to create worktree: {:?}", result);
        assert!(worktree_path.exists(), "Worktree was not created");

        // Verify the worktree has the correct branch
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&worktree_path)
            .arg("branch")
            .arg("--show-current")
            .output()
            .await
            .expect("Failed to check branch");

        let branch_name = String::from_utf8_lossy(&branch_check.stdout);
        assert_eq!(branch_name.trim(), "test-branch");

        // Test find_worktree_for_branch - should find the worktree we just created
        let result = repo.find_worktree_for_branch("test-branch").await;
        assert!(
            result.is_ok(),
            "Failed to find worktree for branch: {:?}",
            result
        );
        assert_eq!(
            result.unwrap(),
            Some(worktree_path.clone()),
            "Found worktree path should match the created worktree"
        );

        // Test find_worktree_for_branch with non-existent branch
        let result = repo.find_worktree_for_branch("nonexistent-branch").await;
        assert!(
            result.is_ok(),
            "find_worktree_for_branch should not error for non-existent branch"
        );
        assert_eq!(
            result.unwrap(),
            None,
            "Should return None for non-existent branch"
        );

        // Test cleanup_worktree
        let result = repo.cleanup_worktree(&worktree_path).await;
        assert!(result.is_ok(), "Failed to cleanup worktree: {:?}", result);

        // Test find_worktree_for_branch after cleanup - should return None
        let result = repo.find_worktree_for_branch("test-branch").await;
        assert!(
            result.is_ok(),
            "find_worktree_for_branch should not error after cleanup"
        );
        assert_eq!(
            result.unwrap(),
            None,
            "Should return None after worktree is cleaned up"
        );

        // Clean up test directories
        let _ = fs::remove_dir_all(&bare_path);
        let _ = fs::remove_dir_all(&worktree_path);
    }
}
