use crate::agent::AgentEvent;
use crate::agent_registry;
use crate::agent_runner::{run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED};
use crate::git;
use crate::github;
use crate::minion_resolver;
use anyhow::{Context, Result};
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
pub async fn handle_rebase(
    target: Option<String>,
    push: bool,
    timeout: Option<&str>,
) -> Result<i32> {
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
                force_push(&worktree_path).await?;
                println!("🚀 Force-pushed rebased branch");
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
                    force_push(&worktree_path).await?;
                    println!("🚀 Force-pushed rebased branch");
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
async fn is_up_to_date(worktree_path: &Path, base_branch: &str) -> Result<bool> {
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
        log::warn!("git rebase --abort warning: {}", stderr.trim());
    }

    Ok(())
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
