use crate::agent::AgentEvent;
use crate::agent_registry;
use crate::agent_runner::{run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED};
use crate::git;
use crate::github;
use crate::minion_resolver;
use crate::tmux::TmuxGuard;
use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use uuid::Uuid;

/// Handles the `gru rebase` command.
///
/// Rebases the current worktree's branch onto the latest base branch.
/// If conflicts arise, spawns Claude Code with the `/rebase` command for
/// intelligent resolution.
///
/// Returns the process exit code (0 = success).
pub(crate) async fn handle_rebase(
    target: Option<String>,
    push: bool,
    yes: bool,
    timeout: Option<&str>,
) -> Result<i32> {
    let _tmux_guard = TmuxGuard::new("gru:rebase");

    let worktree_path = match target {
        Some(ref arg) => resolve_worktree_from_arg(arg).await?,
        None => resolve_worktree_from_cwd().await?,
    };

    println!("🔄 Rebasing worktree: {}", worktree_path.display());

    // Pre-flight: fetch latest from origin
    println!("📡 Fetching latest changes from origin...");
    fetch_origin(&worktree_path).await?;

    // Detect the base branch
    let base_branch = detect_base_branch(&worktree_path).await?;
    println!("🎯 Base branch: {}", base_branch);

    // Pre-flight: check for uncommitted changes
    check_clean_worktree(&worktree_path).await?;

    // Check if already up-to-date
    if is_up_to_date(&worktree_path, &base_branch).await? {
        println!("✅ Already up-to-date with origin/{}", base_branch);
        return Ok(0);
    }

    // Attempt the rebase
    let rebase_result = attempt_rebase(&worktree_path, &base_branch).await?;

    match rebase_result {
        RebaseOutcome::Clean { commit_count } => {
            println!(
                "✅ Clean rebase: {} commit{} replayed",
                commit_count,
                if commit_count == 1 { "" } else { "s" }
            );

            if push {
                if !maybe_force_push(&worktree_path, yes).await? {
                    return Ok(1);
                }
            } else {
                println!("ℹ️  Use --push to force-push the rebased branch to origin");
            }
            Ok(0)
        }
        RebaseOutcome::Conflicts => {
            println!("⚠️  Conflicts detected, launching Claude Code to resolve...");

            // Abort the in-progress rebase so Claude starts with a clean state
            // (the /rebase command will re-initiate the rebase itself)
            abort_rebase(&worktree_path).await?;

            // Spawn Claude Code with /rebase command
            let exit_code = run_agent_rebase(&worktree_path, timeout).await?;

            if exit_code == 0 {
                if push {
                    // Defensively force push in case the /rebase skill didn't push
                    if !maybe_force_push(&worktree_path, yes).await? {
                        return Ok(1);
                    }
                } else {
                    println!("ℹ️  Use --push to force-push the rebased branch to origin");
                }
                Ok(0)
            } else {
                println!(
                    "❌ Claude Code exited with code {}. The previous rebase was aborted, so no rebase is currently in progress.\n\
                     You can retry with `gru rebase`, or perform the rebase manually with `git rebase origin/{}`.",
                    exit_code, base_branch
                );
                Ok(exit_code)
            }
        }
    }
}

/// Outcome of a rebase attempt
pub(crate) enum RebaseOutcome {
    /// Rebase completed cleanly
    Clean { commit_count: usize },
    /// Rebase hit conflicts
    Conflicts,
}

/// Resolves the worktree path from the current working directory.
async fn resolve_worktree_from_cwd() -> Result<PathBuf> {
    // Check we're in a git repository and return the repo root
    let repo_root = git::detect_git_repo().await.context(
        "Not in a git repository. Run from a Minion worktree or provide an issue/PR number.",
    )?;

    Ok(repo_root)
}

/// Resolves the worktree path from an explicit argument (issue number, PR number, Minion ID, or URL).
async fn resolve_worktree_from_arg(arg: &str) -> Result<PathBuf> {
    // Strip leading # if present (e.g., "#123" -> "123")
    let cleaned = arg.strip_prefix('#').unwrap_or(arg);

    // Use the minion resolver to find the worktree
    let info = minion_resolver::resolve_minion(cleaned)
        .await
        .with_context(|| format!("Could not find worktree for '{}'", arg))?;

    if !info.worktree_path.exists() {
        anyhow::bail!(
            "Worktree for {} no longer exists at {}.\n\
             Try running `/setup-worktree {}` to recreate it.",
            info.minion_id,
            info.worktree_path.display(),
            arg
        );
    }

    Ok(info.worktree_path)
}

/// Checks that the worktree has no uncommitted changes.
pub(crate) async fn check_clean_worktree(worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--porcelain"])
        .output()
        .await
        .context("Failed to check working tree status")?;

    if !output.stdout.is_empty() {
        anyhow::bail!(
            "Working directory has uncommitted changes. Commit or stash them before rebasing."
        );
    }

    Ok(())
}

/// Fetches the latest changes from origin in a worktree.
pub(crate) async fn fetch_origin(worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["fetch", "origin"])
        .output()
        .await
        .context("Failed to execute git fetch origin")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git fetch origin failed: {}", stderr.trim());
    }

    Ok(())
}

/// Gets the remote origin URL for a worktree (uses -C to target the right repo).
async fn get_remote_url(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .context("Failed to get remote URL")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get remote URL: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Detects the base branch for the current worktree.
///
/// First checks if there's a PR associated with this branch (which specifies a base),
/// then falls back to detecting the default branch from the remote.
pub(crate) async fn detect_base_branch(worktree_path: &Path) -> Result<String> {
    // Get current branch name
    let branch = get_current_branch(worktree_path).await?;

    // Try to get base branch from an associated PR
    match get_pr_base_branch(worktree_path, &branch).await {
        Ok(Some(base)) => return Ok(base),
        Ok(None) => {} // No associated PR; fall through to default branch detection
        Err(e) => log::warn!(
            "Could not detect PR base branch for '{}': {}. Falling back to default branch detection.",
            branch,
            e
        ),
    }

    // Fall back to detecting the default branch from remote
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .await;

    if let Ok(ref out) = output {
        if out.status.success() {
            let refname = String::from_utf8_lossy(&out.stdout);
            if let Some(branch_name) = refname.trim().strip_prefix("refs/remotes/origin/") {
                return Ok(branch_name.to_string());
            }
        }
    }

    // Query GitHub API for the default branch
    let github_hosts = crate::config::load_host_registry().all_hosts();
    if let Ok(remote_url) = get_remote_url(worktree_path).await {
        match git::parse_github_remote(&remote_url, &github_hosts) {
            Ok((host, owner, repo)) => {
                match github::get_default_branch(&host, &owner, &repo).await {
                    Ok(branch_name) => return Ok(branch_name),
                    Err(e) => log::warn!(
                        "Could not determine default branch from GitHub API: {}. Falling back to 'main'.",
                        e
                    ),
                }
            }
            Err(e) => log::debug!(
                "Could not parse remote URL '{}' as a GitHub remote: {}",
                remote_url,
                e
            ),
        }
    }

    // Final fallback: "main"
    Ok("main".to_string())
}

/// Gets the current branch name in a worktree.
async fn get_current_branch(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["branch", "--show-current"])
        .output()
        .await
        .context("Failed to get current branch")?;

    if !output.status.success() {
        anyhow::bail!("Failed to determine current branch (detached HEAD?)");
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        anyhow::bail!("No branch checked out (detached HEAD state)");
    }

    Ok(branch)
}

/// Tries to get the base branch from an associated PR via GitHub CLI.
async fn get_pr_base_branch(worktree_path: &Path, branch: &str) -> Result<Option<String>> {
    // Detect repo from the worktree (not CWD) to pick gh vs ghe
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let remote_url = get_remote_url(worktree_path).await?;
    let (host, owner, repo) = git::parse_github_remote(&remote_url, &github_hosts)?;
    let repo_full = github::repo_slug(&owner, &repo);

    let output = github::gh_cli_command(&host)
        .args([
            "pr",
            "view",
            branch,
            "--repo",
            &repo_full,
            "--json",
            "baseRefName",
            "--jq",
            ".baseRefName",
        ])
        .current_dir(worktree_path)
        .output()
        .await
        .context("Failed to execute gh pr view")?;

    if !output.status.success() {
        return Ok(None);
    }

    let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if base.is_empty() {
        return Ok(None);
    }

    Ok(Some(base))
}

/// Checks if the current branch is already up-to-date with the base branch.
pub(crate) async fn is_up_to_date(worktree_path: &Path, base_branch: &str) -> Result<bool> {
    let remote_ref = format!("origin/{}", base_branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["merge-base", "--is-ancestor", &remote_ref, "HEAD"])
        .output()
        .await
        .context("Failed to check if branch is up-to-date")?;

    // Exit code 0 means remote_ref is an ancestor of HEAD (we're up-to-date)
    Ok(output.status.success())
}

/// Attempts a git rebase onto the base branch.
pub(crate) async fn attempt_rebase(
    worktree_path: &Path,
    base_branch: &str,
) -> Result<RebaseOutcome> {
    let remote_ref = format!("origin/{}", base_branch);

    // Count commits that will be replayed (for reporting)
    let count_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-list", "--count", &format!("{}..HEAD", remote_ref)])
        .output()
        .await
        .context("Failed to count commits")?;

    let commit_count = if count_output.status.success() {
        String::from_utf8_lossy(&count_output.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0)
    } else {
        0
    };

    // Attempt the rebase
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rebase", &remote_ref])
        .output()
        .await
        .context("Failed to execute git rebase")?;

    if output.status.success() {
        Ok(RebaseOutcome::Clean { commit_count })
    } else {
        // Git writes "CONFLICT ..." to stdout and "error: could not apply ..." to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("CONFLICT")
            || stderr.contains("CONFLICT")
            || stderr.contains("could not apply")
        {
            Ok(RebaseOutcome::Conflicts)
        } else {
            // Some other rebase failure - abort and report
            abort_rebase(worktree_path).await.ok();
            anyhow::bail!("git rebase failed: {}", stderr.trim());
        }
    }
}

/// Checks whether a rebase is already in progress in the given worktree.
///
/// Git stores rebase state in `rebase-merge/` (interactive/merge rebase) or
/// `rebase-apply/` (apply-based rebase) inside the git directory. In a regular
/// checkout the git dir is `<worktree>/.git/`. In a git worktree, `.git` is a
/// file containing `gitdir: <path>` pointing to the real git dir.
///
/// This function reads the `.git` marker directly (no git subprocess) so it is
/// unaffected by `GIT_DIR` / `GIT_WORK_TREE` environment variables that may be
/// set when called from within a git hook.
pub(crate) fn is_rebase_in_progress(worktree_path: &Path) -> bool {
    let git_marker = worktree_path.join(".git");

    let git_dir = if git_marker.is_dir() {
        // Regular repo: .git is the git directory itself.
        git_marker
    } else if git_marker.is_file() {
        // Worktree: .git is a file with "gitdir: <path>" pointing to the real git dir.
        let content = match std::fs::read_to_string(&git_marker) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let git_dir_path = match content.trim().strip_prefix("gitdir: ") {
            Some(p) => PathBuf::from(p.trim()),
            None => return false,
        };
        if git_dir_path.is_absolute() {
            git_dir_path
        } else {
            worktree_path.join(git_dir_path)
        }
    } else {
        return false;
    };

    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

/// Aborts an in-progress rebase.
pub(crate) async fn abort_rebase(worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rebase", "--abort"])
        .output()
        .await
        .context("Failed to abort rebase")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rebase --abort failed: {}", stderr.trim());
    }

    Ok(())
}

/// Confirms with the user, then force-pushes. Skips the prompt when `yes` is true.
///
/// Returns `true` if the push happened, `false` if the user cancelled.
async fn maybe_force_push(worktree_path: &Path, yes: bool) -> Result<bool> {
    let branch = get_current_branch(worktree_path).await?;

    if !yes {
        println!(
            "⚠️  About to force-push branch '{}' to origin (using --force-with-lease)",
            branch
        );

        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "Non-interactive terminal detected. Use --yes to skip the confirmation prompt, \
                 or push manually:\n    git push --force-with-lease origin HEAD"
            );
        }

        if !confirm_force_push().await {
            println!("Force-push cancelled.");
            println!("ℹ️  Run again with --yes to skip this prompt, or push manually:");
            println!("    git push --force-with-lease origin HEAD");
            return Ok(false);
        }
    }

    force_push(worktree_path).await?;
    println!("🚀 Force-pushed rebased branch");
    Ok(true)
}

/// Prompts the user to confirm a force-push.
///
/// Returns `true` if confirmed (y/yes/Enter), `false` otherwise.
async fn confirm_force_push() -> bool {
    use std::io::Write;
    use tokio::io::AsyncBufReadExt;

    print!("Proceed? [Y/n] ");
    if std::io::stdout().flush().is_err() {
        return false;
    }

    let mut input = String::new();
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);

    tokio::select! {
        result = reader.read_line(&mut input) => {
            match result {
                Ok(0) | Err(_) => false,
                Ok(_) => crate::prompt_utils::is_affirmative(&input),
            }
        }
        _ = tokio::signal::ctrl_c() => false,
    }
}

/// Force-pushes the current branch using --force-with-lease.
///
/// Explicitly specifies `origin HEAD` to avoid relying on upstream tracking
/// configuration, which may not be set in worktrees created from bare repos.
pub(crate) async fn force_push(worktree_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["push", "--force-with-lease", "origin", "HEAD"])
        .output()
        .await
        .context("Failed to execute git push --force-with-lease")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "git push --force-with-lease failed: {}\n\
         The remote branch may have been updated since your last fetch.\n\
         Run `git fetch origin` and try again.",
        stderr.trim()
    );
}

/// Default timeout for agent conflict resolution (30 minutes).
const DEFAULT_CONFLICT_TIMEOUT: &str = "30m";

/// Spawns the agent with the `/rebase` command to resolve conflicts.
///
/// Uses a 30-minute default timeout if none is provided.
/// Returns the agent's exit code.
pub(crate) async fn run_agent_rebase(worktree_path: &Path, timeout: Option<&str>) -> Result<i32> {
    let backend = agent_registry::resolve_backend(agent_registry::DEFAULT_AGENT)?;
    let session_id = Uuid::new_v4();
    let github_host = super::resume::resolve_host_from_worktree(worktree_path, "").await;
    let cmd = backend.build_command(worktree_path, &session_id, "/rebase", &github_host);

    let effective_timeout = Some(timeout.unwrap_or(DEFAULT_CONFLICT_TIMEOUT));
    let result = run_agent_with_stream_monitoring(
        cmd,
        &*backend,
        worktree_path,
        effective_timeout,
        None::<fn(&AgentEvent)>, // no output callback
        None,                    // no on_spawn callback
    )
    .await
    .context("Failed to run agent for rebase")?;

    Ok(result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Creates a unique temp directory with a fake `.git/` for testing.
    fn make_fake_git_dir(suffix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gru-rebase-test-{}-{}", suffix, nanos));
        fs::create_dir_all(dir.join(".git")).expect("create .git dir");
        dir
    }

    #[test]
    fn test_is_rebase_in_progress_clean_worktree() {
        let dir = make_fake_git_dir("clean");
        assert!(
            !is_rebase_in_progress(&dir),
            "clean worktree should not report rebase in progress"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_is_rebase_in_progress_with_rebase_merge_dir() {
        let dir = make_fake_git_dir("merge");
        fs::create_dir_all(dir.join(".git").join("rebase-merge")).expect("create rebase-merge");
        assert!(
            is_rebase_in_progress(&dir),
            "should detect rebase-merge directory"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_is_rebase_in_progress_with_rebase_apply_dir() {
        let dir = make_fake_git_dir("apply");
        fs::create_dir_all(dir.join(".git").join("rebase-apply")).expect("create rebase-apply");
        assert!(
            is_rebase_in_progress(&dir),
            "should detect rebase-apply directory"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_is_rebase_in_progress_nonexistent_path() {
        let dir = PathBuf::from("/tmp/gru-nonexistent-repo-xyz-9999");
        assert!(
            !is_rebase_in_progress(&dir),
            "nonexistent path should return false"
        );
    }

    #[test]
    fn test_is_rebase_in_progress_worktree_gitdir_file() {
        // Simulate a git worktree where .git is a file pointing to the real git dir.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let worktree = std::env::temp_dir().join(format!("gru-rebase-wt-{}", nanos));
        let real_git_dir = std::env::temp_dir().join(format!("gru-rebase-realdir-{}", nanos));
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&real_git_dir).unwrap();

        // Write the gitdir pointer file (absolute path)
        let gitdir_content = format!("gitdir: {}\n", real_git_dir.display());
        fs::write(worktree.join(".git"), &gitdir_content).unwrap();

        // No rebase state yet
        assert!(!is_rebase_in_progress(&worktree));

        // Add rebase-merge to the real git dir
        fs::create_dir_all(real_git_dir.join("rebase-merge")).unwrap();
        assert!(is_rebase_in_progress(&worktree));

        fs::remove_dir_all(&worktree).ok();
        fs::remove_dir_all(&real_git_dir).ok();
    }

    #[test]
    fn test_is_rebase_in_progress_worktree_gitdir_relative_path() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let worktree = std::env::temp_dir().join(format!("gru-rebase-wt-rel-{}", nanos));
        // The "real" git dir is a subdirectory of the worktree (relative path).
        let real_git_dir = worktree.join("fake-gitdir");
        fs::create_dir_all(&real_git_dir).unwrap();
        // Write a relative gitdir pointer
        fs::write(worktree.join(".git"), "gitdir: fake-gitdir\n").unwrap();

        assert!(!is_rebase_in_progress(&worktree));

        fs::create_dir_all(real_git_dir.join("rebase-merge")).unwrap();
        assert!(is_rebase_in_progress(&worktree));

        fs::remove_dir_all(&worktree).ok();
    }
}
