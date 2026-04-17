use anyhow::{Context, Result};

use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio::process::Command;

/// Default branch names to try when creating new worktrees, in priority order.
///
/// Bare repositories may store branches either:
/// - Directly (e.g., "main") - newer repos cloned with --bare
/// - With remote prefix (e.g., "origin/main") - repos with mirror-style refspec
///
/// We try both patterns to handle existing repos with different configurations.
const DEFAULT_BRANCHES: &[&str] = &["main", "origin/main", "master", "origin/master"];

/// Holds temporary resources needed for authenticated git commands.
///
/// The `GIT_ASKPASS` script file must outlive the git `Command` execution.
/// Keep this struct alive until the command completes.
struct AuthenticatedGitCommand {
    cmd: Command,
    /// Temp file for the GIT_ASKPASS script. Must remain alive while cmd runs.
    _askpass_file: Option<NamedTempFile>,
}

/// Creates a git `Command` pre-configured with `GIT_ASKPASS` authentication.
///
/// If a token is provided, creates a temporary script that outputs the token on
/// stdout. Git invokes this script when it needs credentials, so the token never
/// appears in command-line arguments (visible in `/proc/*/cmdline`) or git error
/// messages. The token is passed via `GRU_ASKPASS_TOKEN` env var to the child
/// process, which is visible in `/proc/*/environ` (same-uid only) — standard
/// practice for passing secrets to subprocesses.
///
/// The returned `AuthenticatedGitCommand` must be kept alive until the command
/// finishes so the temporary askpass script file is not deleted prematurely.
fn git_command_with_auth(token: Option<&str>) -> Result<AuthenticatedGitCommand> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let mut cmd = Command::new("git");
    // Always disable interactive prompts for non-interactive operation
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    let askpass_file = if let Some(token) = token {
        // Write a script that reads the token from an env var, keeping
        // the token out of the file content entirely.
        let mut file = NamedTempFile::new().context("Failed to create GIT_ASKPASS temp file")?;
        writeln!(
            file,
            "#!/bin/sh\ncase \"$1\" in\n*assword*) printf '%s\\n' \"$GRU_ASKPASS_TOKEN\" ;;\n*) echo x-access-token ;;\nesac"
        )
            .context("Failed to write GIT_ASKPASS script")?;
        file.flush().context("Failed to flush GIT_ASKPASS script")?;

        // Make the script executable
        #[cfg(unix)]
        {
            let metadata = file
                .as_file()
                .metadata()
                .context("Failed to read GIT_ASKPASS file metadata")?;
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            file.as_file()
                .set_permissions(perms)
                .context("Failed to set GIT_ASKPASS file permissions")?;
        }

        cmd.env("GIT_ASKPASS", file.path());
        // Pass the token via env var so it never appears in the script file
        cmd.env("GRU_ASKPASS_TOKEN", token);
        Some(file)
    } else {
        None
    };

    Ok(AuthenticatedGitCommand {
        cmd,
        _askpass_file: askpass_file,
    })
}

/// Redacts credential-related content from git error output.
///
/// Strips any `credential.helper` configuration values from stderr to prevent
/// accidental credential exposure in logs or error messages.
fn redact_credentials(stderr: &str) -> String {
    let mut result = String::with_capacity(stderr.len());
    for line in stderr.lines() {
        if !result.is_empty() {
            result.push('\n');
        }
        // Redact credential.helper values that may appear in verbose git output
        if let Some(idx) = line.find("credential.helper") {
            result.push_str(&line[..idx]);
            result.push_str("credential.helper=<REDACTED>");
        } else {
            result.push_str(line);
        }
    }
    result
}

/// Parses the default branch ref from `git ls-remote --symref origin HEAD` output.
///
/// Expected first line shape: `ref: refs/heads/<branch>\tHEAD`.
/// Returns the full `refs/heads/<branch>` ref, or `None` if the remote HEAD is
/// detached, absent, or points outside `refs/heads/` (e.g., tags) — writing a
/// bogus target into `refs/remotes/origin/HEAD` would corrupt the ref.
fn parse_symref_head(output: &str) -> Option<String> {
    const HEADS_PREFIX: &str = "refs/heads/";
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("ref:") else {
            continue;
        };
        let rest = rest.trim_start();
        let target = rest
            .split_once(|c: char| c.is_whitespace())
            .map(|(t, _)| t)
            .unwrap_or(rest);
        if target.starts_with(HEADS_PREFIX) && target.len() > HEADS_PREFIX.len() {
            return Some(target.to_string());
        }
    }
    None
}

/// Detects if the current directory is within a git repository
/// Returns the root path of the git repository
pub(crate) async fn detect_git_repo() -> Result<PathBuf> {
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
pub(crate) async fn get_github_remote(github_hosts: &[String]) -> Result<String> {
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
pub(crate) fn parse_github_remote(
    url: &str,
    github_hosts: &[String],
) -> Result<(String, String, String)> {
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
pub(crate) struct WorktreeEntry {
    /// Filesystem path to the worktree
    pub(crate) path: PathBuf,
    /// Branch name (without refs/heads/ prefix), None for detached HEAD or bare entries
    pub(crate) branch: Option<String>,
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
pub(crate) fn parse_porcelain_worktrees(output: &str) -> Vec<WorktreeEntry> {
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
pub(crate) struct GitRepo {
    owner: String,
    repo: String,
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    host: String,
    bare_path: PathBuf,
}

impl GitRepo {
    /// Create a new GitRepo instance
    pub(crate) fn new(
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
    pub(crate) async fn ensure_bare_clone(&self) -> Result<()> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        // Check if the bare repository already exists
        if self.bare_path.exists() {
            // Ensure the fetch refspec maps remote branches into refs/remotes/origin/*
            // (the standard git tracking convention). This is set unconditionally so
            // that repos cloned before this convention was adopted are also corrected.
            // With this refmap, plain `git fetch origin` (e.g. run by agents) never
            // tries to update a ref checked out in a worktree. Failure is non-fatal —
            // the repo remains usable, but agents running plain `git fetch origin`
            // may still hit the worktree-checkout conflict on that repo.
            match Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("config")
                .arg("remote.origin.fetch")
                .arg("+refs/heads/*:refs/remotes/origin/*")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .await
            {
                Err(e) => log::warn!(
                    "{}/{}: could not update fetch refspec: {}",
                    self.owner,
                    self.repo,
                    e
                ),
                Ok(out) if !out.status.success() => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    log::warn!(
                        "{}/{}: git config for fetch refspec exited {:?}: {}",
                        self.owner,
                        self.repo,
                        out.status.code(),
                        stderr.trim()
                    );
                }
                Ok(_) => {}
            }

            // Fetch only the default branch to keep it up to date for new worktree creation.
            // Feature branches are fetched on demand via fetch_branch().
            let mut fetched = false;
            for branch in DEFAULT_BRANCHES {
                // Skip origin/-prefixed entries; we fetch from origin by branch name
                if branch.starts_with("origin/") {
                    continue;
                }

                let auth = git_command_with_auth(token.as_deref())?;
                let AuthenticatedGitCommand {
                    mut cmd,
                    _askpass_file,
                } = auth;
                let output = cmd
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
                        redact_credentials(stderr.trim())
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

            // Refresh origin/HEAD after fetch so default branch changes on the
            // remote (e.g., master → main) are reflected locally.
            self.update_origin_head(token.as_deref()).await;
        } else {
            // Clone as bare repository
            let url = format!("https://{}/{}/{}.git", self.host, self.owner, self.repo);

            // Create parent directory if it doesn't exist
            if let Some(parent) = self.bare_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context("Failed to create parent directory for bare repository")?;
            }

            let auth = git_command_with_auth(token.as_deref())?;
            let AuthenticatedGitCommand {
                mut cmd,
                _askpass_file,
            } = auth;

            cmd.arg("clone")
                .arg("--bare")
                .arg(&url)
                .arg(&self.bare_path);

            let output = cmd.output().await.context("Failed to execute git clone")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git clone failed with exit code {:?}: {}",
                    output.status.code(),
                    redact_credentials(&stderr)
                );
            }

            // Configure fetch refspec to map remote branches into refs/remotes/origin/*
            // (the standard git convention). Using refs/remotes/origin/* instead of
            // refs/heads/* means `git fetch origin` (with no explicit refspec) never tries
            // to update a ref that may be checked out in a worktree, eliminating the
            // "refusing to fetch into branch" error that occurs when another worktree has
            // the default branch checked out.
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .arg("config")
                .arg("remote.origin.fetch")
                .arg("+refs/heads/*:refs/remotes/origin/*")
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

            // Set origin/HEAD so default branch detection works in worktrees
            // via `git symbolic-ref refs/remotes/origin/HEAD`
            self.update_origin_head(token.as_deref()).await;
        }

        Ok(())
    }

    /// Updates `refs/remotes/origin/HEAD` so that worktrees can detect the
    /// default branch via `git symbolic-ref refs/remotes/origin/HEAD` without
    /// needing an API call.
    ///
    /// Uses `git ls-remote --symref origin HEAD` to discover the remote's
    /// default branch, then writes the symbolic-ref directly. This avoids
    /// `git remote set-head --auto`, which fails on bare repos where
    /// `refs/remotes/origin/<branch>` has not yet been populated (bare clones
    /// place branches under `refs/heads/*`, and explicit-refspec fetches never
    /// touch `refs/remotes/origin/*`).
    ///
    /// Failures are logged but not propagated — this is a best-effort
    /// optimisation that doesn't block clone or fetch.
    async fn update_origin_head(&self, token: Option<&str>) {
        let result: Result<()> = async {
            let auth = git_command_with_auth(token)?;
            let AuthenticatedGitCommand {
                mut cmd,
                _askpass_file,
            } = auth;
            let output = cmd
                .arg("-C")
                .arg(&self.bare_path)
                .args(["ls-remote", "--symref", "origin", "HEAD"])
                .output()
                .await
                .context("Failed to execute git ls-remote")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git ls-remote exited {:?}: {}",
                    output.status.code(),
                    redact_credentials(stderr.trim())
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let default_ref = parse_symref_head(&stdout).ok_or_else(|| {
                anyhow::anyhow!("could not parse default branch from ls-remote --symref output")
            })?;

            // parse_symref_head guarantees the refs/heads/ prefix.
            let branch = default_ref
                .strip_prefix("refs/heads/")
                .expect("parse_symref_head returned non-refs/heads/ ref");
            let target = format!("refs/remotes/origin/{}", branch);

            // symbolic-ref is a local-only operation; no auth needed.
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.bare_path)
                .args(["symbolic-ref", "refs/remotes/origin/HEAD", &target])
                .output()
                .await
                .context("Failed to execute git symbolic-ref")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git symbolic-ref exited {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                );
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            log::warn!(
                "{}/{}: could not update origin/HEAD: {}",
                self.owner,
                self.repo,
                e
            );
        }
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
            if let Some(full_ref) = parse_symref_head(&stdout) {
                let branch_name = full_ref
                    .strip_prefix("refs/heads/")
                    .expect("parse_symref_head returned non-refs/heads/ ref");
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

    /// Fetches the default branch from origin immediately before creating a new worktree.
    ///
    /// In a bare repository, `git fetch +refs/heads/main:refs/heads/main` fails because
    /// git considers the bare repo HEAD to be "checked out" on that branch and refuses
    /// to update it. Using `--update-head-ok` bypasses this restriction, ensuring the
    /// local `main` ref is always up-to-date before we branch off it.
    ///
    /// Any failure (network blip, missing git binary, disk full for askpass temp file)
    /// logs a warning and returns `Ok(())` — a slightly stale base is acceptable, but
    /// Minion startup must not be blocked.
    async fn fetch_default_branch_for_worktree(&self) -> Result<()> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        for branch in DEFAULT_BRANCHES {
            if branch.starts_with("origin/") {
                continue;
            }

            // `git_command_with_auth` creates a NamedTempFile for the askpass script.
            // It must be constructed inside the loop so the file outlives the command.
            let auth = match git_command_with_auth(token.as_deref()) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!(
                        "Failed to build auth for fetch before worktree creation: {} \
                         — proceeding with possibly stale base",
                        e
                    );
                    return Ok(());
                }
            };
            let AuthenticatedGitCommand {
                mut cmd,
                _askpass_file,
            } = auth;
            let output = match cmd
                .arg("-C")
                .arg(&self.bare_path)
                .arg("fetch")
                .arg("--update-head-ok")
                .arg("origin")
                .arg(format!("+refs/heads/{}:refs/heads/{}", branch, branch))
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    log::warn!(
                        "Failed to spawn git fetch before worktree creation: {} \
                         — proceeding with possibly stale base",
                        e
                    );
                    return Ok(());
                }
            };

            if output.status.success() {
                log::debug!(
                    "Fetched default branch '{}' before worktree creation",
                    branch
                );
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);

            // If this branch doesn't exist on the remote, try the next candidate.
            let is_missing_ref = stderr.contains("couldn't find remote ref")
                || stderr.contains("could not find remote ref")
                || stderr.contains("no such ref")
                || stderr.contains("unknown revision");

            if is_missing_ref {
                continue;
            }

            // For other errors (auth, network, etc.), warn and return — don't block startup.
            log::warn!(
                "Failed to fetch '{}' before worktree creation (exit code {:?}): {} \
                 — proceeding with possibly stale base",
                branch,
                output.status.code(),
                redact_credentials(stderr.trim())
            );
            return Ok(());
        }

        log::warn!(
            "Could not find a default branch to fetch before worktree creation \
             — proceeding with possibly stale base"
        );
        Ok(())
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
    pub(crate) async fn create_worktree(
        &self,
        branch_name: &str,
        worktree_path: &Path,
    ) -> Result<()> {
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
            // Branch doesn't exist, create it based on the default branch.
            // Fetch the default branch first so the new worktree branches off the
            // latest commit and not a stale snapshot from when the bare repo was cloned.
            if let Err(e) = self.fetch_default_branch_for_worktree().await {
                log::warn!(
                    "Failed to fetch default branch before creating worktree '{}': {} \
                     — proceeding with possibly stale base",
                    branch_name,
                    e
                );
            }
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
    pub(crate) async fn checkout_worktree(
        &self,
        branch_name: &str,
        worktree_path: &Path,
    ) -> Result<()> {
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
    pub(crate) async fn find_worktree_for_branch(
        &self,
        branch_name: &str,
    ) -> Result<Option<PathBuf>> {
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

    /// Fetches the latest changes for a branch in the bare repository
    /// This is useful for updating an existing worktree's branch before checking it out
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bare repository doesn't exist
    /// - The branch name is invalid
    /// - The git fetch command fails
    pub(crate) async fn fetch_branch(&self, branch_name: &str) -> Result<()> {
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
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        let auth = git_command_with_auth(token.as_deref())?;
        let AuthenticatedGitCommand {
            mut cmd,
            _askpass_file,
        } = auth;
        let output = cmd
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
                redact_credentials(stderr.trim())
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
    fn test_parse_symref_head_standard() {
        let output = "ref: refs/heads/main\tHEAD\n0123abcd\tHEAD\n";
        assert_eq!(
            parse_symref_head(output),
            Some("refs/heads/main".to_string())
        );
    }

    #[test]
    fn test_parse_symref_head_master() {
        let output = "ref: refs/heads/master\tHEAD\n";
        assert_eq!(
            parse_symref_head(output),
            Some("refs/heads/master".to_string())
        );
    }

    #[test]
    fn test_parse_symref_head_missing() {
        let output = "0123abcd\tHEAD\n";
        assert_eq!(parse_symref_head(output), None);
    }

    #[test]
    fn test_parse_symref_head_empty() {
        assert_eq!(parse_symref_head(""), None);
    }

    #[test]
    fn test_parse_symref_head_rejects_non_heads_namespace() {
        let output = "ref: refs/tags/v1.0\tHEAD\n";
        assert_eq!(parse_symref_head(output), None);
    }

    #[test]
    fn test_parse_symref_head_rejects_bare_heads_prefix() {
        let output = "ref: refs/heads/\tHEAD\n";
        assert_eq!(parse_symref_head(output), None);
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

    /// Verify that fetch_default_branch_for_worktree() never returns Err, even when
    /// the underlying git fetch fails. A fetch failure should warn but not block
    /// Minion startup (the acceptance criterion from issue #790).
    #[tokio::test]
    async fn test_fetch_default_branch_for_worktree_is_not_fatal_on_failure() {
        // Point at a temp dir that is not a git repo — git fetch will fail
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::new("owner", "repo", "github.com", temp_dir.path().to_path_buf());

        // Must succeed (Ok) even though git fetch will fail in a non-repo directory
        let result = repo.fetch_default_branch_for_worktree().await;
        assert!(
            result.is_ok(),
            "fetch_default_branch_for_worktree should never return Err — got: {:?}",
            result
        );
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

    #[test]
    fn test_redact_credentials_removes_credential_helper() {
        let input = "fatal: could not read Username for 'https://github.com': credential.helper=!f() { echo username=oauth2; echo password='ghp_secret123'; }; f";
        let result = redact_credentials(input);
        assert!(
            !result.contains("ghp_secret123"),
            "Token should be redacted from output"
        );
        assert!(
            result.contains("credential.helper=<REDACTED>"),
            "Should contain redacted placeholder"
        );
    }

    #[test]
    fn test_redact_credentials_preserves_safe_output() {
        let input = "fatal: repository 'https://github.com/owner/repo.git' not found";
        let result = redact_credentials(input);
        assert_eq!(result, input, "Safe output should be unchanged");
    }

    #[test]
    fn test_redact_credentials_handles_multiline() {
        let input = "line 1\ncredential.helper=!secret stuff\nline 3";
        let result = redact_credentials(input);
        assert!(
            !result.contains("secret stuff"),
            "Secret should be redacted"
        );
        assert!(result.contains("line 1"), "Non-secret lines preserved");
        assert!(result.contains("line 3"), "Non-secret lines preserved");
    }

    #[test]
    fn test_git_command_with_auth_no_token() {
        let auth = git_command_with_auth(None).unwrap();
        assert!(
            auth._askpass_file.is_none(),
            "No askpass file should be created without a token"
        );
    }

    #[test]
    fn test_git_command_with_auth_with_token() {
        let auth = git_command_with_auth(Some("test-token-123")).unwrap();
        assert!(
            auth._askpass_file.is_some(),
            "Askpass file should be created with a token"
        );
        let file = auth._askpass_file.as_ref().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = file.as_file().metadata().unwrap();
            assert_eq!(
                metadata.permissions().mode() & 0o700,
                0o700,
                "Askpass script should be executable"
            );
        }
        // Script should reference the env var, NOT contain the actual token
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("GRU_ASKPASS_TOKEN"),
            "Askpass script should reference the env var"
        );
        assert!(
            !content.contains("test-token-123"),
            "Askpass script must NOT contain the raw token"
        );
        assert!(
            content.starts_with("#!/bin/sh"),
            "Askpass script should be a shell script"
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

        // Verify origin/HEAD was set during clone
        let origin_head = Command::new("git")
            .arg("-C")
            .arg(&bare_path)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
            .output()
            .await
            .expect("Failed to check origin/HEAD");
        assert!(
            origin_head.status.success(),
            "refs/remotes/origin/HEAD should be set after bare clone"
        );
        let origin_head_ref = String::from_utf8_lossy(&origin_head.stdout);
        assert!(
            origin_head_ref.trim().starts_with("refs/remotes/origin/"),
            "origin/HEAD should point to a valid remote ref, got: {}",
            origin_head_ref.trim()
        );

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

        // Clean up worktree using git directly
        let cleanup_output = Command::new("git")
            .arg("-C")
            .arg(&bare_path)
            .arg("worktree")
            .arg("remove")
            .arg(&worktree_path)
            .output()
            .await
            .expect("Failed to execute git worktree remove");
        assert!(
            cleanup_output.status.success(),
            "Failed to cleanup worktree"
        );

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

    /// Verifies that `ensure_bare_clone` rewrites `remote.origin.fetch` to the
    /// `refs/remotes/origin/*` convention when called on a bare repo that still
    /// has the legacy `+refs/heads/*:refs/heads/*` mapping.
    ///
    /// Uses a fully local setup (no network): a "source" bare repo with a real
    /// branch is created, a "clone" bare repo is set up pointing at it, the old
    /// refmap is planted on the clone, and then `ensure_bare_clone` is called.
    /// Because the local source is reachable, the fetch inside `ensure_bare_clone`
    /// also succeeds, exercising the full code path.
    #[tokio::test]
    #[ignore = "local only: no network required, but requires git binary"]
    async fn test_ensure_bare_clone_migrates_fetch_refspec() {
        use tokio::process::Command;

        // Use tempfile::tempdir() for a unique, auto-cleaned directory so
        // concurrent runs or leftover directories don't cause collisions.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let work_path = tmp.path().join("work");
        let source_path = tmp.path().join("source.git");
        let clone_path = tmp.path().join("clone.git");

        // Create a working directory with an initial commit so the source bare
        // repo has at least one branch (required for a successful fetch later).
        Command::new("git")
            .args(["init"])
            .arg(&work_path)
            .output()
            .await
            .expect("git init failed");
        for (k, v) in [("user.email", "test@test.com"), ("user.name", "Test")] {
            Command::new("git")
                .arg("-C")
                .arg(&work_path)
                .args(["config", k, v])
                .output()
                .await
                .expect("git config failed");
        }
        Command::new("git")
            .arg("-C")
            .arg(&work_path)
            .args(["commit", "--allow-empty", "-m", "init"])
            .output()
            .await
            .expect("git commit failed");

        // Create the "source" bare repo from the working directory.
        Command::new("git")
            .args(["clone", "--bare"])
            .arg(&work_path)
            .arg(&source_path)
            .output()
            .await
            .expect("git clone --bare failed");

        // Create the "clone" bare repo from the source.
        Command::new("git")
            .args(["clone", "--bare"])
            .arg(&source_path)
            .arg(&clone_path)
            .output()
            .await
            .expect("git clone --bare (clone) failed");

        // Plant the legacy refmap on the clone to simulate a pre-fix repo.
        Command::new("git")
            .arg("-C")
            .arg(&clone_path)
            .args([
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
            ])
            .output()
            .await
            .expect("git config failed");

        // Pre-condition: verify old refmap is set.
        let before = Command::new("git")
            .arg("-C")
            .arg(&clone_path)
            .args(["config", "remote.origin.fetch"])
            .output()
            .await
            .expect("git config read failed");
        assert_eq!(
            String::from_utf8_lossy(&before.stdout).trim(),
            "+refs/heads/*:refs/heads/*",
            "pre-condition: old refmap should be set before calling ensure_bare_clone"
        );

        // Call ensure_bare_clone — this should migrate the refmap.
        let repo = GitRepo::new(
            "test-owner",
            "test-repo",
            "github.example.com",
            clone_path.clone(),
        );
        let result = repo.ensure_bare_clone().await;
        assert!(
            result.is_ok(),
            "ensure_bare_clone should succeed on a local repo: {:?}",
            result
        );

        // Post-condition: refmap must be updated.
        let after = Command::new("git")
            .arg("-C")
            .arg(&clone_path)
            .args(["config", "remote.origin.fetch"])
            .output()
            .await
            .expect("git config read failed");
        assert_eq!(
            String::from_utf8_lossy(&after.stdout).trim(),
            "+refs/heads/*:refs/remotes/origin/*",
            "fetch refspec should be updated to refs/remotes/origin/* after ensure_bare_clone"
        );
        // `tmp` drops here, cleaning up all directories automatically.
    }
}
