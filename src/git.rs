use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default branch names to try when creating new worktrees, in priority order.
///
/// Bare repositories may store branches either:
/// - Directly (e.g., "main") - newer repos cloned with --bare
/// - With remote prefix (e.g., "origin/main") - repos with mirror-style refspec
///
/// We try both patterns to handle existing repos with different configurations.
const DEFAULT_BRANCHES: &[&str] = &["main", "origin/main", "master", "origin/master"];

/// Detects if the current directory is within a git repository
/// Returns the root path of the git repository
pub fn detect_git_repo() -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
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
pub fn get_github_remote() -> Result<String> {
    // Use `git remote -v` to get all remotes and their URLs in one call
    let output = Command::new("git")
        .arg("remote")
        .arg("-v")
        .output()
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
            if is_github_url(url) {
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

/// Checks if a URL is a GitHub URL
/// Only matches URLs that start with recognized GitHub URL patterns
fn is_github_url(url: &str) -> bool {
    url.starts_with("https://github.com/")
        || url.starts_with("http://github.com/")
        || url.starts_with("git@github.com:")
}

/// Parses a GitHub remote URL to extract owner and repo name
/// Supports both HTTPS and SSH formats:
/// - https://github.com/owner/repo.git
/// - git@github.com:owner/repo.git
pub fn parse_github_remote(url: &str) -> Result<(String, String)> {
    if !is_github_url(url) {
        anyhow::bail!("Not a GitHub URL: {}", url);
    }

    // Handle HTTPS format
    if url.starts_with("https://github.com/") || url.starts_with("http://github.com/") {
        let path = url
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/")
            .trim_end_matches(".git")
            .trim_end_matches('/');

        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // Handle SSH format: git@github.com:owner/repo.git
    if url.starts_with("git@github.com:") {
        let path = url
            .trim_start_matches("git@github.com:")
            .trim_end_matches(".git")
            .trim_end_matches('/');

        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    anyhow::bail!("Could not parse GitHub URL: {}", url);
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
            // Use explicit refspec to update local branches directly (bare repos store
            // branches without remote prefix, so we need to map refs/heads/* to refs/heads/*)
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("fetch")
                .arg("origin")
                .arg("+refs/heads/*:refs/heads/*")
                .output()
                .context("Failed to execute git fetch")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // If fetch fails because a branch is checked out in a worktree,
                // fall back to fetching just the default branch (e.g. main) so
                // new worktrees aren't created from stale refs.
                if stderr.contains("refusing to fetch into branch")
                    && stderr.contains("checked out at")
                {
                    log::warn!(
                        "Full fetch failed due to worktree conflict: {}",
                        stderr.trim()
                    );
                    log::warn!(
                        "Stale worktrees are preventing a full fetch. \
                         Run `gru clean` to remove merged/closed worktrees."
                    );

                    // Fetch just the default branch so new worktrees are up to date.
                    // Read the local HEAD symref (set at clone time) to avoid a
                    // network round-trip — the bare repo already knows its default branch.
                    let default_branch = self.local_default_branch_name()?;
                    let fallback = Command::new("git")
                        .arg("-C")
                        .arg(&self.bare_path)
                        .arg("fetch")
                        .arg("origin")
                        .arg(format!(
                            "+refs/heads/{}:refs/heads/{}",
                            default_branch, default_branch
                        ))
                        .output()
                        .context("Failed to execute fallback git fetch for default branch")?;

                    if !fallback.status.success() {
                        let fallback_stderr = String::from_utf8_lossy(&fallback.stderr);
                        // If even the default branch fetch fails (e.g. it's checked out
                        // in a worktree too), warn but don't fail — the repo still exists
                        // and may have a recent-enough copy.
                        log::warn!(
                            "Could not fetch default branch '{}' either: {}",
                            default_branch,
                            fallback_stderr.trim()
                        );
                    }
                } else {
                    anyhow::bail!(
                        "git fetch failed with exit code {:?}: {}",
                        output.status.code(),
                        stderr
                    );
                }
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
                // Escape single quotes in the token to prevent command injection
                let safe_token = token.replace('\'', "'\\''");
                cmd.arg("-c").arg(format!(
                    "credential.helper=!f() {{ echo username=oauth2; echo password='{}'; }}; f",
                    safe_token
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

            // Configure fetch refspec so future fetches update local branches directly
            // (git clone --bare doesn't set this by default)
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("config")
                .arg("remote.origin.fetch")
                .arg("+refs/heads/*:refs/heads/*")
                .output()
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

    /// Reads the local HEAD symref from the bare repository to determine the default
    /// branch name (e.g. "main", "master"). This is a local operation with no network
    /// access, making it suitable for fallback paths where speed matters.
    ///
    /// Falls back to "main" if the symref can't be read or parsed.
    fn local_default_branch_name(&self) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("symbolic-ref")
            .arg("HEAD")
            .output()
            .context("Failed to read symbolic HEAD from bare repo")?;

        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            // Output is "refs/heads/main\n" — strip prefix and trim
            if let Some(branch) = raw.trim().strip_prefix("refs/heads/") {
                return Ok(branch.to_string());
            }
        }

        log::warn!("Could not read local HEAD symref, falling back to 'main'");
        Ok("main".to_string())
    }

    /// Queries the remote to discover the default branch name (e.g. "main", "master").
    ///
    /// Returns just the branch name as it appears on the remote (without any
    /// "origin/" prefix or "refs/heads/" prefix). Falls back to "main" if the
    /// remote query fails or can't be parsed.
    fn query_default_branch_name(&self) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("ls-remote")
            .arg("--symref")
            .arg("origin")
            .arg("HEAD")
            .output()
            .context("Failed to query remote for default branch")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse: "ref: refs/heads/main\tHEAD"
            if let Some(line) = stdout.lines().next() {
                if let Some(branch_name) = line
                    .strip_prefix("ref: refs/heads/")
                    .and_then(|s| s.split('\t').next())
                {
                    return Ok(branch_name.to_string());
                }
            }
        }

        log::warn!("Could not determine default branch from remote, falling back to 'main'");
        Ok("main".to_string())
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
    fn get_base_branch(&self) -> Result<String> {
        if !self.bare_path.exists() {
            anyhow::bail!(
                "Bare repository does not exist at {}",
                self.bare_path.display()
            );
        }

        // Query the remote to discover the default branch
        let default_branch = self.query_default_branch_name()?;

        // Bare repos may store branches either directly ("main") or with
        // remote prefix ("origin/main") depending on how they were cloned.
        // Try both patterns and return whichever exists.
        for candidate in [default_branch.clone(), format!("origin/{}", default_branch)] {
            let check = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("rev-parse")
                .arg("--verify")
                .arg(&candidate)
                .output();

            if let Ok(result) = check {
                if result.status.success() {
                    return Ok(candidate);
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

        // Check if the branch already exists
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&self.bare_path)
            .arg("show-ref")
            .arg("--verify")
            .arg(format!("refs/heads/{}", branch_name))
            .output()
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
            let base_branch = self.get_base_branch()?;
            cmd.arg("-b").arg(branch_name).arg(base_branch);
        }

        let output = cmd.output().context("Failed to execute git worktree add")?;

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
    pub fn checkout_worktree(&self, branch_name: &str, worktree_path: &Path) -> Result<()> {
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
            std::fs::create_dir_all(parent)
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
    pub fn find_worktree_for_branch(&self, branch_name: &str) -> Result<Option<PathBuf>> {
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
            .context("Failed to execute git worktree list")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree list failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        // Parse the porcelain output
        // Format:
        // worktree /path/to/worktree
        // HEAD <commit-sha>
        // branch refs/heads/branch-name
        // <blank line>
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current_worktree: Option<PathBuf> = None;

        for line in stdout.lines() {
            let line = line.trim();

            // Empty line indicates end of worktree entry - reset state
            // Entries without branches (detached HEAD, bare repo) are intentionally skipped
            if line.is_empty() {
                current_worktree = None;
                continue;
            }

            if line.starts_with("worktree ") {
                current_worktree = Some(PathBuf::from(line.trim_start_matches("worktree ")));
            } else if line.starts_with("branch ") {
                let branch_ref = line.trim_start_matches("branch ");
                // Git worktree list --porcelain outputs branches in refs/heads/ format
                if branch_ref == format!("refs/heads/{}", branch_name) {
                    match current_worktree {
                        Some(worktree_path) => return Ok(Some(worktree_path)),
                        None => anyhow::bail!(
                            "Malformed git worktree list output: found branch entry without preceding worktree path"
                        ),
                    }
                }
            }
        }

        Ok(None)
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
    pub fn cleanup_worktree_force(&self, worktree_path: &Path) -> Result<()> {
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
                .output();

            // If it's a valid worktree, check for uncommitted changes
            if let Ok(output) = is_worktree {
                if output.status.success() {
                    let status = Command::new("git")
                        .arg("-C")
                        .arg(worktree_path)
                        .arg("status")
                        .arg("--porcelain")
                        .output();

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
    pub fn fetch_branch(&self, branch_name: &str) -> Result<()> {
        // Validate branch name using existing comprehensive validation
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

    /// Push a branch to the remote repository
    ///
    /// # Arguments
    /// * `worktree_path` - Path to the worktree containing the branch to push
    /// * `branch_name` - Name of the branch to push
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The branch name is invalid
    /// - The worktree path doesn't exist
    /// - Git push fails (authentication, network, conflicts, etc.)
    ///
    /// # Note
    ///
    /// Reserved for future use when programmatically pushing branches.
    /// Currently, branches are pushed by the Claude agent via git commands.
    #[allow(dead_code)]
    pub fn push_branch(&self, worktree_path: &Path, branch_name: &str) -> Result<()> {
        // Validate branch name
        validate_branch_name(branch_name)?;

        if !worktree_path.exists() {
            anyhow::bail!("Worktree does not exist at {}", worktree_path.display());
        }

        // Push the branch to origin with --set-upstream
        let output = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .arg("push")
            .arg("--set-upstream")
            .arg("origin")
            .arg(branch_name)
            .output()
            .context("Failed to execute git push")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to push branch '{}' from worktree {}: git push exited with code {:?}: {}",
                branch_name,
                worktree_path.display(),
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

    #[test]
    fn test_parse_github_remote_https() {
        let result = parse_github_remote("https://github.com/owner/repo.git").unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
    }

    #[test]
    fn test_parse_github_remote_https_without_git_extension() {
        let result = parse_github_remote("https://github.com/owner/repo").unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
    }

    #[test]
    fn test_parse_github_remote_ssh() {
        let result = parse_github_remote("git@github.com:owner/repo.git").unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
    }

    #[test]
    fn test_parse_github_remote_ssh_without_git_extension() {
        let result = parse_github_remote("git@github.com:owner/repo").unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
    }

    #[test]
    fn test_parse_github_remote_rejects_non_github() {
        let result = parse_github_remote("https://gitlab.com/owner/repo.git");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not a GitHub URL"));
    }

    #[test]
    fn test_parse_github_remote_rejects_invalid_format() {
        let result = parse_github_remote("https://github.com/incomplete");
        assert!(result.is_err());
    }

    #[test]
    fn test_is_github_url() {
        // Valid GitHub URLs
        assert!(is_github_url("https://github.com/owner/repo.git"));
        assert!(is_github_url("http://github.com/owner/repo.git"));
        assert!(is_github_url("git@github.com:owner/repo.git"));

        // Invalid - not GitHub
        assert!(!is_github_url("https://gitlab.com/owner/repo.git"));

        // Invalid - security: malicious URLs that contain "github.com" but aren't GitHub
        assert!(!is_github_url("https://evil.com/github.com/malware.git"));
        assert!(!is_github_url("https://github.com.attacker.com/repo.git"));
        assert!(!is_github_url("user@attacker.com:github.com:malware.git"));
    }

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

    #[test]
    fn test_find_worktree_for_branch_fails_without_bare_repo() {
        let repo = GitRepo::new(
            "owner",
            "repo",
            PathBuf::from("/tmp/nonexistent-bare-repo.git"),
        );
        let result = repo.find_worktree_for_branch("test-branch");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bare repository does not exist"));
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

        // Test find_worktree_for_branch - should find the worktree we just created
        let result = repo.find_worktree_for_branch("test-branch");
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
        let result = repo.find_worktree_for_branch("nonexistent-branch");
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
        let result = repo.cleanup_worktree(&worktree_path);
        assert!(result.is_ok(), "Failed to cleanup worktree: {:?}", result);

        // Test find_worktree_for_branch after cleanup - should return None
        let result = repo.find_worktree_for_branch("test-branch");
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
