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

    // Detect the base branch first so we can fetch only what we need
    let base_branch = detect_base_branch(&worktree_path).await?;
    println!("🎯 Base branch: {}", base_branch);

    // Pre-flight: fetch only the base branch to avoid conflicts with other
    // worktrees that have different branches checked out
    println!("📡 Fetching latest changes from origin...");
    fetch_base_branch(&worktree_path, &base_branch).await?;

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

            // Capture local branch name now (abort_rebase restores it) so we can
            // recover if the agent leaves the worktree in detached HEAD state.
            let local_branch = get_current_branch(&worktree_path).await?;

            // Spawn Claude Code with /rebase command
            let exit_code = run_agent_rebase(&worktree_path, timeout).await?;

            if exit_code == 0 {
                // Agent succeeded: enforce branch recovery (any failure is a real error).
                ensure_on_branch(&worktree_path, &local_branch).await?;

                // Postcondition: verify the rebase actually completed.
                // Claude Code exits 0 by default even when the rebase fails, so
                // we check independently that origin/<base_branch> is now an ancestor
                // of HEAD — the canonical proof that the rebase landed.
                if !is_up_to_date(&worktree_path, &base_branch).await? {
                    println!(
                        "❌ Agent exited 0 but origin/{} is not an ancestor of HEAD — rebase did not complete.\n\
                         You can retry with `gru rebase`, or perform the rebase manually with `git rebase origin/{}`.",
                        base_branch, base_branch
                    );
                    return Ok(1);
                }

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
                // Agent already failed; attempt recovery but don't let a branch-restore
                // failure mask the primary non-zero exit code guidance.
                if let Err(err) = ensure_on_branch(&worktree_path, &local_branch).await {
                    log::warn!(
                        "Branch recovery after failed agent rebase also failed: {}",
                        err
                    );
                }
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

/// Builds the argument list (after `git -C <path>`) used by [`fetch_base_branch`].
///
/// `--refmap=` (empty string) ensures only our explicit refspec runs, regardless
/// of what `remote.origin.fetch` is configured to.  The configured refmap now
/// uses `refs/remotes/origin/*` (safe), but `--refmap=` is kept as defense-in-depth
/// and to avoid fetching all branches when only one is needed.
fn make_fetch_args(base_branch: &str) -> Vec<String> {
    vec![
        "fetch".to_string(),
        "origin".to_string(),
        "--refmap=".to_string(),
        format!("+refs/heads/{base_branch}:refs/remotes/origin/{base_branch}"),
    ]
}

/// Fetches only the specified base branch from origin.
///
/// Uses an explicit refspec that writes to `refs/remotes/origin/<base_branch>`,
/// combined with `--refmap=` to ensure only the named branch is fetched.
pub(crate) async fn fetch_base_branch(worktree_path: &Path, base_branch: &str) -> Result<()> {
    let args = make_fetch_args(base_branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(&args)
        .output()
        .await
        .with_context(|| format!("Failed to execute git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
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
pub(crate) async fn get_current_branch(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["branch", "--show-current"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
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

/// Ensures the worktree is on the expected local branch.
///
/// After a conflict-resolution agent run the worktree can end up in detached
/// HEAD state — for example when the agent checks out a remote tracking ref
/// such as `origin/minion/issue-42-M001` instead of the local branch.  This
/// function detects that condition and restores the local branch so that
/// subsequent operations (force-push, further commits) work correctly.
///
/// # Limitation: orphaned commits
///
/// This function restores the *branch pointer* by running `git checkout
/// <expected_branch>`.  Any commits the agent made while in detached HEAD
/// (e.g. conflict-resolution commits) are left unreachable and will
/// eventually be garbage-collected.  In practice the autonomous monitor
/// path force-pushes after this function returns, so if the agent did not
/// push its work before control returned here, those commits are silently
/// discarded.  Callers that need to preserve detached-HEAD commits should
/// cherry-pick or reset before calling this function.
pub(crate) async fn ensure_on_branch(worktree_path: &Path, expected_branch: &str) -> Result<()> {
    anyhow::ensure!(
        !expected_branch.is_empty(),
        "expected_branch must not be empty"
    );

    // We don't call get_current_branch() here because that function returns
    // Err on detached HEAD (empty output), whereas we want to treat detached
    // HEAD as a recoverable condition rather than an error.
    // Unset git env vars so that `-C worktree_path` is the authoritative
    // way to target the repo (GIT_DIR would otherwise override it).
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["branch", "--show-current"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
        .context("Failed to check current branch")?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git branch --show-current failed (exit {}): {}",
            code,
            stderr.trim()
        );
    }

    let current = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if current == expected_branch {
        return Ok(());
    }

    if current.is_empty() {
        // Capture the detached HEAD SHA before switching so the operator
        // can see (and potentially recover) commits the agent made while
        // detached, which will become unreachable after `git checkout`.
        let head_sha = {
            let out = Command::new("git")
                .arg("-C")
                .arg(worktree_path)
                .args(["rev-parse", "HEAD"])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => String::new(),
            }
        };

        // Warn when the detached HEAD has commits not yet present on
        // the expected branch — those commits will be orphaned.
        if !head_sha.is_empty() {
            let is_ancestor = Command::new("git")
                .arg("-C")
                .arg(worktree_path)
                .args(["merge-base", "--is-ancestor", &head_sha, expected_branch])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);

            if !is_ancestor {
                let short = &head_sha[..head_sha.len().min(8)];
                log::warn!(
                    "⚠️  Detached HEAD at {} has commits not present on '{}'; \
                     they will be unreachable after checkout. \
                     To recover: git -C <worktree> branch recover-{} {}",
                    short,
                    expected_branch,
                    short,
                    short
                );
                println!(
                    "⚠️  Detached HEAD at {} has commits not on '{}' — \
                     they will be unreachable after checkout.",
                    short, expected_branch
                );
            }
        }

        log::warn!(
            "⚠️  Worktree is in detached HEAD state; checking out local branch '{}'",
            expected_branch
        );
        println!(
            "⚠️  Recovering from detached HEAD: checking out '{}'...",
            expected_branch
        );
    } else {
        log::warn!(
            "⚠️  Worktree is on unexpected branch '{}' instead of '{}'; switching",
            current,
            expected_branch
        );
        println!(
            "⚠️  Unexpected branch '{}'; switching to '{}'...",
            current, expected_branch
        );
    }

    let checkout_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["checkout", expected_branch])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
        .with_context(|| format!("Failed to checkout branch '{}'", expected_branch))?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        anyhow::bail!(
            "git checkout '{}' failed: {}\n\
             The working tree may have uncommitted changes from the agent run. \
             Inspect with `git -C <worktree> status` and stash or reset before retrying.",
            expected_branch,
            stderr.trim()
        );
    }

    Ok(())
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
        // Parse "gitdir: <path>" tolerantly: split on the first ':' and trim
        // both sides so leading/trailing whitespace variations are handled.
        let git_dir_path = match content.splitn(2, ':').collect::<Vec<_>>().as_slice() {
            [key, value] if key.trim().eq_ignore_ascii_case("gitdir") => {
                PathBuf::from(value.trim())
            }
            _ => return false,
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
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "git rebase --abort failed (exit {}): stderr={} stdout={}",
            code,
            stderr.trim(),
            stdout.trim()
        );
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
         Run `git fetch origin <base_branch>` and try again.",
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
    use tempfile::TempDir;

    /// Creates a temp dir with a fake `.git/` directory for testing.
    fn make_fake_git_dir() -> TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::create_dir_all(dir.path().join(".git")).expect("create .git dir");
        dir
    }

    #[test]
    fn test_fetch_base_branch_refspec_format() {
        // Verify the full argument list: --refmap= appears before the refspec
        // to ensure only our explicit refspec runs (defense-in-depth and avoids
        // fetching all branches when only one is needed).
        // The refspec writes to refs/remotes/origin/* (never checked against
        // worktree state).  The leading '+' forces the update after a force-push.
        assert_eq!(
            make_fetch_args("main"),
            vec![
                "fetch",
                "origin",
                "--refmap=",
                "+refs/heads/main:refs/remotes/origin/main",
            ]
        );

        // Branch names with slashes are valid and handled correctly.
        assert_eq!(
            make_fetch_args("release/1.0"),
            vec![
                "fetch",
                "origin",
                "--refmap=",
                "+refs/heads/release/1.0:refs/remotes/origin/release/1.0",
            ]
        );
    }

    #[test]
    fn test_is_rebase_in_progress_clean_worktree() {
        let dir = make_fake_git_dir();
        assert!(
            !is_rebase_in_progress(dir.path()),
            "clean worktree should not report rebase in progress"
        );
    }

    #[test]
    fn test_is_rebase_in_progress_with_rebase_merge_dir() {
        let dir = make_fake_git_dir();
        fs::create_dir_all(dir.path().join(".git").join("rebase-merge"))
            .expect("create rebase-merge");
        assert!(
            is_rebase_in_progress(dir.path()),
            "should detect rebase-merge directory"
        );
    }

    #[test]
    fn test_is_rebase_in_progress_with_rebase_apply_dir() {
        let dir = make_fake_git_dir();
        fs::create_dir_all(dir.path().join(".git").join("rebase-apply"))
            .expect("create rebase-apply");
        assert!(
            is_rebase_in_progress(dir.path()),
            "should detect rebase-apply directory"
        );
    }

    #[test]
    fn test_is_rebase_in_progress_nonexistent_path() {
        // Use a child of a tempdir that is guaranteed not to exist yet.
        let base = tempfile::tempdir().expect("create temp dir");
        let nonexistent = base.path().join("does-not-exist");
        assert!(
            !is_rebase_in_progress(&nonexistent),
            "nonexistent path should return false"
        );
    }

    #[test]
    fn test_is_rebase_in_progress_worktree_gitdir_file() {
        // Simulate a git worktree where .git is a file pointing to the real git dir
        // (absolute path).
        let worktree = tempfile::tempdir().expect("create worktree dir");
        let real_git_dir = tempfile::tempdir().expect("create real git dir");
        let gitdir_content = format!("gitdir: {}\n", real_git_dir.path().display());
        fs::write(worktree.path().join(".git"), &gitdir_content).unwrap();

        assert!(!is_rebase_in_progress(worktree.path()));

        fs::create_dir_all(real_git_dir.path().join("rebase-merge")).unwrap();
        assert!(is_rebase_in_progress(worktree.path()));
    }

    #[test]
    fn test_is_rebase_in_progress_worktree_gitdir_relative_path() {
        // Simulate a git worktree with a relative gitdir pointer.
        let worktree = tempfile::tempdir().expect("create worktree dir");
        let real_git_dir = worktree.path().join("fake-gitdir");
        fs::create_dir_all(&real_git_dir).unwrap();
        fs::write(worktree.path().join(".git"), "gitdir: fake-gitdir\n").unwrap();

        assert!(!is_rebase_in_progress(worktree.path()));

        fs::create_dir_all(real_git_dir.join("rebase-merge")).unwrap();
        assert!(is_rebase_in_progress(worktree.path()));
    }

    #[test]
    fn test_is_rebase_in_progress_gitdir_extra_whitespace() {
        // Verify the parser tolerates extra whitespace after "gitdir:" (e.g. "gitdir:  /path").
        let worktree = tempfile::tempdir().expect("create worktree dir");
        let real_git_dir = tempfile::tempdir().expect("create real git dir");
        let gitdir_content = format!("gitdir:  {}\n", real_git_dir.path().display());
        fs::write(worktree.path().join(".git"), &gitdir_content).unwrap();
        fs::create_dir_all(real_git_dir.path().join("rebase-merge")).unwrap();
        assert!(
            is_rebase_in_progress(worktree.path()),
            "should handle extra whitespace after 'gitdir:'"
        );
    }

    /// Initialises a throwaway git repo with one commit and a named branch.
    ///
    /// Each git invocation explicitly removes `GIT_DIR`, `GIT_WORK_TREE`, and
    /// `GIT_INDEX_FILE` so that the commands target the fresh temp repo even
    /// when the test process was started from within a git hook (which sets
    /// `GIT_DIR`).  Using per-invocation `env_remove` (rather than
    /// `std::env::remove_var`) is safe under both nextest and `cargo test`.
    ///
    /// Returns the temp dir (must stay alive for the lifetime of the test).
    async fn make_git_repo_with_branch(branch: &str) -> TempDir {
        use tokio::process::Command as TokioCmd;

        let dir = tempfile::tempdir().expect("create temp dir");
        let p = dir.path();

        macro_rules! git {
            ($($arg:expr),+) => {{
                let status = TokioCmd::new("git")
                    .args([$($arg),+])
                    .current_dir(p)
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .env_remove("GIT_INDEX_FILE")
                    .status()
                    .await
                    .expect("git command failed");
                assert!(status.success(), "git {} failed", stringify!($($arg),+));
            }};
        }

        git!("init", "-b", branch);
        git!("config", "user.email", "test@test.com");
        git!("config", "user.name", "Test");
        // Create an initial commit so the branch ref exists
        fs::write(p.join("README"), "test").unwrap();
        git!("add", "README");
        git!("commit", "--no-gpg-sign", "-m", "init");

        dir
    }

    #[tokio::test]
    async fn test_ensure_on_branch_already_on_correct_branch() {
        let dir = make_git_repo_with_branch("my-feature").await;
        // Should be a no-op — already on the right branch.
        ensure_on_branch(dir.path(), "my-feature")
            .await
            .expect("should succeed when already on correct branch");
        // Verify still on my-feature
        let branch = get_current_branch(dir.path()).await.unwrap();
        assert_eq!(branch, "my-feature");
    }

    #[tokio::test]
    async fn test_ensure_on_branch_detached_head_recovery() {
        use tokio::process::Command as TokioCmd;
        let dir = make_git_repo_with_branch("my-feature").await;
        let p = dir.path();

        // Put the worktree into detached HEAD by checking out the commit SHA directly.
        let sha_out = TokioCmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(p)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .await
            .expect("git rev-parse failed");
        assert!(
            sha_out.status.success(),
            "git rev-parse HEAD exited with {:?}: {}",
            sha_out.status.code(),
            String::from_utf8_lossy(&sha_out.stderr)
        );
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

        let status = TokioCmd::new("git")
            .args(["checkout", "--detach", &sha])
            .current_dir(p)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .status()
            .await
            .expect("git checkout --detach failed");
        assert!(status.success());

        // Confirm detached HEAD.
        let branch_out = TokioCmd::new("git")
            .args(["branch", "--show-current"])
            .current_dir(p)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .await
            .unwrap();
        let current = String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .to_string();
        assert!(
            current.is_empty(),
            "expected detached HEAD, got '{}'",
            current
        );

        // ensure_on_branch should restore the local branch.
        ensure_on_branch(p, "my-feature")
            .await
            .expect("should recover from detached HEAD");

        let branch = get_current_branch(p).await.unwrap();
        assert_eq!(branch, "my-feature", "should be back on my-feature");
    }

    #[tokio::test]
    async fn test_ensure_on_branch_wrong_named_branch() {
        use tokio::process::Command as TokioCmd;
        let dir = make_git_repo_with_branch("my-feature").await;
        let p = dir.path();

        // Create a second branch and switch to it so the worktree is on
        // the "wrong" named branch (not detached HEAD).
        let status = TokioCmd::new("git")
            .args(["checkout", "-b", "wrong-branch"])
            .current_dir(p)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .status()
            .await
            .expect("git checkout -b failed");
        assert!(status.success());

        // Confirm we are on the wrong branch.
        let current = get_current_branch(p).await.unwrap();
        assert_eq!(current, "wrong-branch");

        // ensure_on_branch should switch back to the expected branch.
        ensure_on_branch(p, "my-feature")
            .await
            .expect("should switch from wrong-branch to my-feature");

        let branch = get_current_branch(p).await.unwrap();
        assert_eq!(branch, "my-feature", "should be on my-feature");
    }
}
