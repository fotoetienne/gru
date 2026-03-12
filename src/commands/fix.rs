use crate::agent::{AgentBackend, AgentEvent};
use crate::agent_registry::{AgentRegistry, DEFAULT_AGENT_NAME};
use crate::agent_runner::{
    is_stuck_or_timeout_error, parse_timeout, run_agent_with_stream_monitoring,
    EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::ci;
use crate::git;
use crate::github::{gh_command_for_repo, GitHubClient};
use crate::minion;
use crate::minion_registry::{
    is_process_alive, with_registry, MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry,
    OrchestrationPhase,
};
use crate::pr_monitor::{self, MonitorResult};
use crate::pr_state::PrState;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::progress_comments::{MinionPhase, ProgressCommentTracker};
use crate::prompt_loader;
use crate::prompt_renderer::{render_template, PromptContext};
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

/// Maximum size of the output buffer for test detection (in bytes)
const MAX_OUTPUT_BUFFER_SIZE: usize = 10000;

/// Size to trim the output buffer to when it exceeds the maximum (in bytes)
const TRIM_OUTPUT_BUFFER_SIZE: usize = 5000;

/// Maximum number of review rounds to handle automatically
/// After this limit, the user must handle additional reviews manually
const MAX_REVIEW_ROUNDS: usize = 5;

/// Maximum number of auto-rebase attempts per monitoring session
/// After this limit, the minion escalates via PR comment
const MAX_REBASE_ATTEMPTS: usize = 2;

// ---------------------------------------------------------------------------
// Phase context structs
// ---------------------------------------------------------------------------

/// Result of resolving an issue argument into validated context.
/// Contains the parsed issue number as `u64`, eliminating repeated string parsing.
pub(crate) struct IssueContext {
    pub owner: String,
    pub repo: String,
    pub issue_num: u64,
    /// Fetched issue details: (title, body, labels). None if fetch failed.
    pub details: Option<IssueDetails>,
    pub github_client: Option<GitHubClient>,
}

/// Fetched issue metadata from GitHub.
pub(crate) struct IssueDetails {
    pub title: String,
    pub body: String,
    pub labels: String,
}

/// Result of setting up a worktree for a minion.
pub(crate) struct WorktreeContext {
    pub minion_id: String,
    pub branch_name: String,
    /// Top-level minion directory where metadata lives (events.jsonl, PR_DESCRIPTION.md, etc.)
    pub minion_dir: PathBuf,
    /// Git worktree checkout path (minion_dir/checkout for new layout, minion_dir for legacy)
    pub checkout_path: PathBuf,
    pub session_id: Uuid,
}

/// Result of running an agent session.
pub(crate) struct AgentResult {
    pub status: ExitStatus,
}

// ---------------------------------------------------------------------------
// Helper functions (unchanged from original)
// ---------------------------------------------------------------------------

/// Checks if a branch has been pushed to the remote by querying GitHub's API.
///
/// Uses the `gh`/`ghe` CLI instead of local git tracking refs, because gru
/// worktrees are backed by bare repos whose `origin` remote points to the
/// local bare repo — not to GitHub.
pub(crate) async fn is_branch_pushed(owner: &str, repo: &str, branch_name: &str) -> Result<bool> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let endpoint = format!("repos/{}/git/ref/heads/{}", repo_full, branch_name);
    let output = TokioCommand::new(gh_cmd)
        .args(["api", &endpoint, "--silent"])
        .output()
        .await
        .context("Failed to run gh api to check if branch is pushed")?;

    if output.status.success() {
        return Ok(true);
    }

    // 404 means the branch doesn't exist on the remote
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("404") || stderr.contains("Not Found") {
        return Ok(false);
    }

    // Any other failure (auth, network, rate limit) is a real error
    Err(anyhow::anyhow!(
        "gh api failed while checking if branch '{}' is pushed: {}",
        branch_name,
        stderr.trim()
    ))
}

/// Creates a WIP PR title and body template
fn create_wip_template(minion_id: &str, issue_num: u64, issue_title: &str) -> (String, String) {
    let title = format!("[{}] Fixes #{}: {}", minion_id, issue_num, issue_title);
    let body = format!(
        "## Summary\nAutomated fix for #{} by Minion {}\n\n\
         ## Status\n- [ ] Implementation\n- [ ] Tests\n- [ ] Review\n\n\
         Fixes #{}\n",
        issue_num, minion_id, issue_num,
    );
    (title, body)
}

/// Creates a PR for the given issue, returning the PR number
#[allow(clippy::too_many_arguments)]
async fn create_pr_for_issue(
    owner: &str,
    repo: &str,
    branch_name: &str,
    issue_num: u64,
    minion_id: &str,
    checkout_path: &Path,
    minion_dir: &Path,
    issue_title_opt: Option<&str>,
) -> Result<String> {
    // Detect base branch
    let base_output = TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()
        .await
        .context("Failed to detect base branch")?;

    let base_branch = if base_output.status.success() {
        let raw = String::from_utf8_lossy(&base_output.stdout);
        raw.trim()
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("main")
            .to_string()
    } else {
        "main".to_string()
    };

    // Get issue title - use provided title if available, otherwise fetch
    let issue_title = if let Some(title) = issue_title_opt {
        title.to_string()
    } else if let Some(github_client) = GitHubClient::try_from_env(owner, repo).await {
        match github_client.get_issue(owner, repo, issue_num).await {
            Ok(issue) => issue.title,
            Err(_) => "Fix issue".to_string(),
        }
    } else {
        match crate::github::get_issue_via_cli(owner, repo, issue_num).await {
            Ok(info) => info.title,
            Err(_) => "Fix issue".to_string(),
        }
    };

    // Check if work is complete (description file exists in minion_dir)
    let description_path = minion_dir.join("PR_DESCRIPTION.md");
    let should_mark_ready = match tokio::fs::try_exists(&description_path).await {
        Ok(exists) => exists,
        Err(e) => {
            log::warn!(
                "⚠️  Warning: Failed to check if PR_DESCRIPTION.md exists: {}",
                e
            );
            false
        }
    };

    let (pr_title, pr_body) = if should_mark_ready {
        // Read the description file
        match tokio::fs::read_to_string(&description_path).await {
            Ok(content) if !content.trim().is_empty() => {
                // Work is complete - use description and mark ready
                let pr_title = format!("Fixes #{}: {}", issue_num, issue_title);
                let closing_line = format!("Fixes #{}", issue_num);
                let mut pr_body = content.trim().to_string();
                // Append closing keyword if not already present
                if !pr_body.contains(&closing_line) {
                    if !pr_body.ends_with('\n') {
                        pr_body.push('\n');
                    }
                    pr_body.push('\n');
                    pr_body.push_str(&closing_line);
                }
                (pr_title, pr_body)
            }
            Ok(_) => {
                // File exists but is empty - treat as WIP
                log::warn!("⚠️  Warning: PR_DESCRIPTION.md exists but is empty");
                create_wip_template(minion_id, issue_num, &issue_title)
            }
            Err(e) => {
                // File couldn't be read - treat as WIP
                log::warn!("⚠️  Failed to read PR_DESCRIPTION.md: {}", e);
                create_wip_template(minion_id, issue_num, &issue_title)
            }
        }
    } else {
        // No description file - work in progress
        create_wip_template(minion_id, issue_num, &issue_title)
    };

    // Create the draft PR using gh CLI (with URL validation)
    let pr_number = crate::github::create_draft_pr_via_cli(
        owner,
        repo,
        branch_name,
        &base_branch,
        &pr_title,
        &pr_body,
    )
    .await
    .context("Failed to create draft PR using gh CLI")?;

    // Mark ready if description was provided
    if should_mark_ready {
        match crate::github::mark_pr_ready_via_cli(owner, repo, &pr_number).await {
            Ok(_) => {
                println!("✅ PR #{} marked ready for review", pr_number);
            }
            Err(e) => {
                log::warn!("⚠️  Warning: Failed to mark PR ready: {}", e);
                log::warn!(
                    "   PR #{} created as draft - you can mark it ready manually",
                    pr_number
                );
            }
        }

        // Clean up description file
        if let Err(e) = tokio::fs::remove_file(&description_path).await {
            log::warn!("⚠️  Warning: Failed to remove PR_DESCRIPTION.md: {}", e);
            log::warn!("   File will be cleaned up by 'gru clean'");
        }
    }

    Ok(pr_number)
}

/// Handles GitHub CLI fetch result with fallback
async fn handle_cli_fetch_result(result: Result<crate::github::IssueInfo>) -> Option<IssueDetails> {
    match result {
        Ok(info) => Some(IssueDetails {
            title: info.title,
            body: info.body.unwrap_or_default(),
            labels: String::new(), // CLI doesn't return labels in IssueInfo
        }),
        Err(e) => {
            eprintln!("⚠️  Failed to fetch issue details via CLI: {}", e);
            None
        }
    }
}

/// Attempts to auto-rebase the worktree branch onto its base branch.
///
/// Returns `Ok(true)` if the rebase succeeded (clean or Claude resolved conflicts),
/// `Ok(false)` if Claude couldn't resolve conflicts, or `Err` on unexpected failures.
async fn auto_rebase_pr(worktree_path: &Path) -> Result<bool> {
    use super::rebase::{
        abort_rebase, attempt_rebase, detect_base_branch, fetch_origin, force_push,
        run_agent_rebase, RebaseOutcome,
    };

    // Fetch latest from origin
    println!("📡 Fetching latest changes from origin...");
    fetch_origin(worktree_path).await?;

    // Detect the base branch
    let base_branch = detect_base_branch(worktree_path).await?;
    println!("🔄 Rebasing onto origin/{}...", base_branch);

    // Attempt the rebase
    match attempt_rebase(worktree_path, &base_branch).await? {
        RebaseOutcome::Clean { commit_count } => {
            println!(
                "✅ Clean rebase: {} commit{} replayed",
                commit_count,
                if commit_count == 1 { "" } else { "s" }
            );
            force_push(worktree_path).await?;
            println!("🚀 Force-pushed rebased branch");
            Ok(true)
        }
        RebaseOutcome::Conflicts => {
            println!("⚠️  Conflicts detected, launching agent to resolve...");
            abort_rebase(worktree_path).await?;

            let exit_code = run_agent_rebase(worktree_path).await?;
            if exit_code == 0 {
                // Defensively force push in case the /rebase skill didn't push
                force_push(worktree_path).await?;
                println!("🚀 Force-pushed rebased branch");
                Ok(true)
            } else {
                log::warn!("Agent rebase exited with code {}", exit_code);
                Ok(false)
            }
        }
    }
}

/// Posts an escalation comment on a PR when auto-rebase fails.
async fn post_escalation_comment(owner: &str, repo: &str, pr_number: &str, message: &str) {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let body = format!("🤖 **Minion Escalation**\n\n{}", message);

    let result = TokioCommand::new(gh_cmd)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted escalation comment on PR #{}", pr_number);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post escalation comment on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment: {}", e);
        }
    }
}

/// Invokes the agent to address review comments using the same session
async fn invoke_agent_for_reviews(
    backend: &dyn AgentBackend,
    checkout_path: &Path,
    minion_dir: &Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
) -> Result<()> {
    let cmd = backend
        .build_resume_command(checkout_path, session_id, prompt)
        .context("Agent backend does not support resume")?;

    let result = run_agent_with_stream_monitoring(
        cmd,
        backend,
        minion_dir,
        timeout_opt,
        None::<fn(&AgentEvent)>,
        None::<Box<dyn FnOnce(u32) + Send>>,
    )
    .await?;

    if !result.status.success() {
        let exit_code = result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        anyhow::bail!("Review response process exited with code {}", exit_code);
    }
    Ok(())
}

/// Trigger a PR review as a separate process.
/// If `review_timeout` is `Some`, the review is killed after that duration.
/// If `None`, the review runs without a timeout (Claude's built-in stuck detection applies).
async fn trigger_pr_review(
    pr_number: &str,
    worktree_path: &Path,
    review_timeout: Option<Duration>,
) -> Result<i32> {
    // Validate PR number format (defense in depth)
    pr_number
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_number))?;

    let mut child = TokioCommand::new("gru")
        .arg("review")
        .arg(pr_number)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn gru review command for PR #{}. Is gru in your PATH?",
                pr_number
            )
        })?;

    match review_timeout {
        Some(timeout_duration) => match timeout(timeout_duration, child.wait()).await {
            Ok(status) => {
                let status = status.with_context(|| {
                    format!("Failed to wait for review process for PR #{}", pr_number)
                })?;
                Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let elapsed_secs = timeout_duration.as_secs();
                let time_display = if elapsed_secs >= 60 {
                    let minutes = elapsed_secs / 60;
                    let seconds = elapsed_secs % 60;
                    if seconds == 0 {
                        format!("{} minute{}", minutes, if minutes == 1 { "" } else { "s" })
                    } else {
                        format!(
                            "{} minute{} {} second{}",
                            minutes,
                            if minutes == 1 { "" } else { "s" },
                            seconds,
                            if seconds == 1 { "" } else { "s" }
                        )
                    }
                } else {
                    format!(
                        "{} second{}",
                        elapsed_secs,
                        if elapsed_secs == 1 { "" } else { "s" }
                    )
                };
                Err(anyhow::anyhow!(
                    "Review process timed out after {}. PR #{} review may be stuck.",
                    time_display,
                    pr_number
                ))
            }
        },
        None => {
            let status = child.wait().await.with_context(|| {
                format!("Failed to wait for review process for PR #{}", pr_number)
            })?;
            Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
        }
    }
}

/// Attempts to mark an issue as blocked (fire-and-forget).
/// Logs success/failure but does not propagate errors.
async fn try_mark_issue_blocked(client: &GitHubClient, owner: &str, repo: &str, issue_num: u64) {
    match client.mark_issue_blocked(owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to 'minion:blocked'");
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {}", e);
        }
    }
}

/// Updates the orchestration phase for a minion in the registry.
/// Logs a warning if the update fails, since phase tracking is important for resume correctness.
pub(crate) async fn update_orchestration_phase(minion_id: &str, phase: OrchestrationPhase) {
    let minion_id_owned = minion_id.to_string();
    let phase_name = format!("{:?}", phase);
    if let Err(e) = with_registry(move |registry| {
        registry.update(&minion_id_owned, |info| {
            info.orchestration_phase = phase;
        })
    })
    .await
    {
        log::warn!(
            "⚠️  Failed to update orchestration phase for {} to {}: {}",
            minion_id,
            phase_name,
            e
        );
    }
}

/// Posts a progress comment to the issue (fire-and-forget).
async fn try_post_progress_comment(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) -> bool {
    match client.post_comment(owner, repo, issue_num, body).await {
        Ok(_) => true,
        Err(e) => {
            log::warn!("⚠️  Failed to post progress comment: {}", e);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Resolve Issue
// ---------------------------------------------------------------------------

/// Resolves an issue argument into validated context.
///
/// Parses the issue string, initializes the GitHub client, and fetches issue details.
/// Note: Does NOT claim the issue — that happens after worktree setup to avoid
/// marking issues as in-progress when setup fails.
/// Note: Existing minion check is done in `handle_fix` to support resume logic.
async fn resolve_issue(issue: &str) -> Result<IssueContext> {
    let (owner, repo, issue_num_str) = parse_issue_info(issue).await?;
    let issue_num: u64 = issue_num_str
        .parse()
        .context("Failed to parse issue number")?;

    // Initialize GitHub client (optional - only if token is available)
    let github_client = match GitHubClient::from_env(&owner, &repo).await {
        Ok(client) => Some(client),
        Err(err) => {
            eprintln!(
                "⚠️  GitHub authentication error - progress comments will not be posted: {err}"
            );
            None
        }
    };

    // Fetch issue details
    let details = fetch_issue_details(&owner, &repo, issue_num, &github_client).await;

    Ok(IssueContext {
        owner,
        repo,
        issue_num,
        details,
        github_client,
    })
}

/// Result of checking for existing minions on an issue.
enum ExistingMinionCheck {
    /// No existing minions found, proceed with new session.
    None,
    /// Found a stopped minion that can be resumed.
    Resumable(String, Box<RegistryMinionInfo>),
    /// Found running minion(s), user shown options and should exit.
    AlreadyRunning,
}

/// Checks if there are existing minions working on this issue.
///
/// Returns `Resumable` when a stopped minion is found (for auto-resume),
/// `AlreadyRunning` when a running minion is found (with suggestions printed),
/// or `None` when no minions exist.
async fn check_existing_minions(
    owner: &str,
    repo: &str,
    issue_num: u64,
) -> Result<ExistingMinionCheck> {
    let repo_for_check = format!("{}/{}", owner, repo);
    let mut existing =
        with_registry(move |registry| Ok(registry.find_by_issue(&repo_for_check, issue_num)))
            .await?;

    if existing.is_empty() {
        return Ok(ExistingMinionCheck::None);
    }

    // Sort deterministically: running Minions first, then by most recent start time.
    existing.sort_by(|(_, a), (_, b)| {
        let a_running = a.pid.map(is_process_alive).unwrap_or(false);
        let b_running = b.pid.map(is_process_alive).unwrap_or(false);
        b_running
            .cmp(&a_running)
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });

    // Check if any minion is actually running
    let any_running = existing
        .iter()
        .any(|(_, info)| info.pid.map(is_process_alive).unwrap_or(false));

    if any_running {
        // A minion is actively running - show error with suggestions
        eprintln!(
            "Error: {} existing Minion(s) found for issue {}:\n",
            existing.len(),
            issue_num
        );

        for (minion_id, info) in &existing {
            let actually_running = info.pid.map(is_process_alive).unwrap_or(false);
            let status_msg = if actually_running {
                match info.mode {
                    MinionMode::Autonomous => "running (autonomous)",
                    MinionMode::Interactive => "running (interactive)",
                    MinionMode::Stopped => "running",
                }
            } else {
                "stopped"
            };
            eprintln!("  {} - status: {}", minion_id, status_msg);
        }

        let (best_id, _) = existing.first().unwrap();
        eprintln!("\nOptions:");
        eprintln!("  - Attach interactively: gru attach {}", best_id);
        eprintln!(
            "  - Create new session:   gru do https://github.com/{}/{}/issues/{} --force-new",
            owner, repo, issue_num
        );

        return Ok(ExistingMinionCheck::AlreadyRunning);
    }

    // All minions are stopped - find the best candidate for resume.
    // Look for one that hasn't completed/failed and whose worktree still exists.
    let resumable = existing.iter().find(|(_, info)| {
        !matches!(
            info.orchestration_phase,
            OrchestrationPhase::Completed | OrchestrationPhase::Failed
        ) && info.worktree.exists()
    });

    if let Some((minion_id, info)) = resumable {
        return Ok(ExistingMinionCheck::Resumable(
            minion_id.clone(),
            Box::new(info.clone()),
        ));
    }

    // Check if any minion failed — require --force-new to prevent silent retry
    let has_failed = existing
        .iter()
        .any(|(_, info)| matches!(info.orchestration_phase, OrchestrationPhase::Failed));

    if has_failed {
        let (failed_id, _) = existing
            .iter()
            .find(|(_, info)| matches!(info.orchestration_phase, OrchestrationPhase::Failed))
            .unwrap();
        eprintln!(
            "Error: Minion {} previously failed for issue {}.",
            failed_id, issue_num
        );
        eprintln!("\nOptions:");
        eprintln!(
            "  - Create new session:   gru do https://github.com/{}/{}/issues/{} --force-new",
            owner, repo, issue_num
        );
        return Ok(ExistingMinionCheck::AlreadyRunning);
    }

    Ok(ExistingMinionCheck::None)
}

/// Claims an issue by adding the in-progress label.
async fn claim_issue(client: &GitHubClient, owner: &str, repo: &str, issue_num: u64) {
    match client.claim_issue(owner, repo, issue_num).await {
        Ok(true) => {
            println!("🏷️  Added 'in-progress' label to issue #{}", issue_num);
        }
        Ok(false) => {
            log::warn!(
                "⚠️  Issue #{} is already claimed by another Minion",
                issue_num
            );
            log::warn!("   This may indicate a race condition or multiple gru instances.");
            log::warn!("   Continuing anyway; will proceed to create or reuse a worktree...");
        }
        Err(e) => {
            log::warn!("⚠️  Failed to add label to issue: {}", e);
            log::warn!("   Continuing anyway...");
        }
    }
}

/// Fetches issue details from GitHub API with CLI fallback.
async fn fetch_issue_details(
    owner: &str,
    repo: &str,
    issue_num: u64,
    github_client: &Option<GitHubClient>,
) -> Option<IssueDetails> {
    if let Some(ref client) = github_client {
        match client.get_issue(owner, repo, issue_num).await {
            Ok(issue) => {
                let labels = issue
                    .labels
                    .iter()
                    .map(|l| l.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let body = issue.body.unwrap_or_default();
                Some(IssueDetails {
                    title: issue.title,
                    body,
                    labels,
                })
            }
            Err(e) => {
                eprintln!("⚠️  Failed to fetch issue details via API: {}", e);
                eprintln!("   Falling back to gh CLI...");
                let cli_result = crate::github::get_issue_via_cli(owner, repo, issue_num).await;
                let result = handle_cli_fetch_result(cli_result).await;
                if result.is_none() {
                    eprintln!("   Fix authentication with: gh auth login");
                }
                result
            }
        }
    } else {
        let cli_result = crate::github::get_issue_via_cli(owner, repo, issue_num).await;
        let result = handle_cli_fetch_result(cli_result).await;
        if result.is_none() {
            eprintln!("   Fix authentication with: gh auth login");
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Setup Worktree
// ---------------------------------------------------------------------------

/// Sets up the workspace: generates minion ID, clones repo, creates worktree,
/// registers the minion in the registry.
async fn setup_worktree(ctx: &IssueContext) -> Result<WorktreeContext> {
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    println!(
        "🚀 Setting up workspace for {}/{}#{}",
        ctx.owner, ctx.repo, ctx.issue_num
    );

    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    let bare_path = workspace
        .repos()
        .join(&ctx.owner)
        .join(format!("{}.git", ctx.repo));
    let git_repo = git::GitRepo::new(&ctx.owner, &ctx.repo, bare_path);

    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .await
        .context("Failed to clone or update repository")?;

    let branch_name = format!("minion/issue-{}-{}", ctx.issue_num, minion_id);
    println!("🌿 Creating worktree with branch: {}", branch_name);

    let repo_name = format!("{}/{}", ctx.owner, ctx.repo);
    let minion_dir = workspace
        .work_dir(&repo_name, &branch_name)
        .context("Failed to compute minion directory path")?;

    // Create checkout subdirectory for the git worktree
    let checkout_path = minion_dir.join("checkout");

    // Ensure the minion directory exists
    tokio::fs::create_dir_all(&minion_dir)
        .await
        .context("Failed to create minion directory")?;

    git_repo
        .create_worktree(&branch_name, &checkout_path)
        .await
        .context("Failed to create worktree")?;

    println!("📂 Workspace created at: {}", checkout_path.display());

    let session_id = Uuid::new_v4();

    // Register the Minion in the registry (worktree field stores the minion_dir)
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: repo_name.clone(),
        issue: ctx.issue_num,
        command: "do".to_string(),
        prompt: format!("/do {}", ctx.issue_num),
        started_at: now,
        branch: branch_name.clone(),
        worktree: minion_dir.clone(),
        status: "active".to_string(),
        pr: None,
        session_id: session_id.to_string(),
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
        orchestration_phase: OrchestrationPhase::Setup,
        token_usage: None,
        agent_backend: DEFAULT_AGENT_NAME.to_string(),
    };

    let minion_id_clone = minion_id.clone();
    with_registry(move |registry| registry.register(minion_id_clone, registry_info)).await?;

    println!("📝 Registered Minion {} in registry", minion_id);

    Ok(WorktreeContext {
        minion_id,
        branch_name,
        minion_dir,
        checkout_path,
        session_id,
    })
}

// ---------------------------------------------------------------------------
// Phase 3: Run Claude
// ---------------------------------------------------------------------------

/// Builds the prompt string from issue context using the prompt template system.
///
/// Loads the "do" prompt template (built-in or overridden via `.gru/prompts/do.md`
/// or legacy `.gru/prompts/fix.md`), builds a `PromptContext` from the issue
/// details, and renders the template.
/// Falls back to `/do <issue_num>` when issue details are unavailable.
fn build_fix_prompt(ctx: &IssueContext, wt_ctx: &WorktreeContext) -> String {
    let Some(ref details) = ctx.details else {
        return format!("/do {}", ctx.issue_num);
    };

    // Try to load the prompt through the template system (allows overrides).
    // Use the worktree path as the repo root so `.gru/prompts/do.md` is found.
    let prompt_template = match prompt_loader::resolve_prompt("do", Some(&wt_ctx.checkout_path)) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Failed to load do prompt: {e}, using /do fallback");
            None
        }
    };

    let template_content = match prompt_template {
        Some(ref p) => &p.content,
        None => {
            log::warn!("No 'do' prompt found (built-in or override), using /do fallback");
            return format!("/do {}", ctx.issue_num);
        }
    };

    // Build the context for rendering
    let labels_value = if details.labels.is_empty() {
        String::new()
    } else {
        format!("Labels: {}", details.labels)
    };

    let mut prompt_ctx = PromptContext::new();
    prompt_ctx.issue_number = Some(ctx.issue_num);
    prompt_ctx.issue_title = Some(details.title.clone());
    prompt_ctx.issue_body = Some(details.body.clone());
    prompt_ctx.repo_owner = Some(ctx.owner.clone());
    prompt_ctx.repo_name = Some(ctx.repo.clone());
    prompt_ctx.worktree_path = Some(wt_ctx.checkout_path.clone());
    prompt_ctx.minion_dir = Some(wt_ctx.minion_dir.clone());
    prompt_ctx.branch_name = Some(wt_ctx.branch_name.clone());

    let mut variables = prompt_ctx.to_variables();
    // Add the labels variable (fix-specific, not in the standard PromptContext).
    // Value is "Labels: x, y" when present or empty string when none.
    // The template places {{ labels }} on its own line to handle both cases.
    variables.insert("labels".to_string(), labels_value);

    render_template(template_content, &variables)
}

/// Runs an agent session with stream monitoring and progress tracking.
///
/// Spawns the agent CLI, tracks progress, records PID in registry,
/// and cleans up on exit (success or failure).
async fn run_agent_session(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    println!("🤖 Launching {}...\n", backend.name());
    let prompt = build_fix_prompt(issue_ctx, wt_ctx);
    let mut cmd = backend.build_command(&wt_ctx.checkout_path, &wt_ctx.session_id, &prompt);
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);
    run_agent_session_inner(backend, issue_ctx, wt_ctx, cmd, quiet, timeout_opt).await
}

/// Runs a resumed agent session, continuing from a previous interrupted session.
///
/// Uses the backend's resume command to continue the existing conversation.
async fn resume_agent_session(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    println!("🔄 Resuming {} session...\n", backend.name());
    let prompt = format!(
        "Continue working on issue #{}. Pick up where you left off. \
         If you've already completed the implementation, proceed to push and write PR_DESCRIPTION.md.",
        issue_ctx.issue_num
    );
    let mut cmd = backend
        .build_resume_command(&wt_ctx.checkout_path, &wt_ctx.session_id, &prompt)
        .context("Agent backend does not support resume")?;
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);
    run_agent_session_inner(backend, issue_ctx, wt_ctx, cmd, quiet, timeout_opt).await
}

/// Shared implementation for running an agent session (new or resumed).
///
/// Handles stream monitoring, progress tracking, PID registration, and cleanup.
async fn run_agent_session_inner(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    cmd: TokioCommand,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    let config = ProgressConfig {
        minion_id: wt_ctx.minion_id.clone(),
        issue: issue_ctx.issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let mut progress_tracker = ProgressCommentTracker::new(wt_ctx.minion_id.clone());

    struct CallbackState<'a> {
        text_output_buffer: String,
        progress: &'a ProgressDisplay,
        progress_tracker: &'a mut ProgressCommentTracker,
    }

    let mut callback_state = CallbackState {
        text_output_buffer: String::new(),
        progress: &progress,
        progress_tracker: &mut progress_tracker,
    };

    let callback = |event: &AgentEvent| {
        // Accumulate text output for phase detection
        if let AgentEvent::TextDelta { ref text } = event {
            callback_state.text_output_buffer.push_str(text);
            if callback_state.text_output_buffer.len() > MAX_OUTPUT_BUFFER_SIZE {
                let mut trim_pos = TRIM_OUTPUT_BUFFER_SIZE;
                while trim_pos > 0 && !callback_state.text_output_buffer.is_char_boundary(trim_pos)
                {
                    trim_pos -= 1;
                }
                callback_state.text_output_buffer =
                    callback_state.text_output_buffer.split_off(trim_pos);
            }

            // Detect phase transitions from a rolling window of the accumulated
            // buffer (last 200 chars). Using the buffer instead of just the current
            // chunk ensures we catch markers split across multiple TextDelta events.
            let buf = &callback_state.text_output_buffer;
            let window_start = buf.len().saturating_sub(200);
            // Align to a char boundary
            let window_start = (window_start..buf.len())
                .find(|&i| buf.is_char_boundary(i))
                .unwrap_or(buf.len());
            let window = &buf[window_start..];

            let previous_phase = callback_state.progress_tracker.current_phase();
            if window.contains("Plan") || window.contains("plan") {
                if previous_phase != MinionPhase::Planning {
                    callback_state
                        .progress_tracker
                        .set_phase(MinionPhase::Planning);
                }
            } else if (window.contains("Implement") || window.contains("Writing"))
                && previous_phase != MinionPhase::Implementing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Implementing);
            } else if (window.contains("test") || window.contains("Test"))
                && previous_phase != MinionPhase::Testing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Testing);
            }
        }

        callback_state.progress.handle_event(event);
    };

    let pid_minion_id = wt_ctx.minion_id.clone();
    let on_spawn: Box<dyn FnOnce(u32) + Send> = Box::new(move |pid: u32| {
        if let Ok(mut registry) = MinionRegistry::load(None) {
            let _ = registry.update(&pid_minion_id, |info| {
                info.pid = Some(pid);
                info.mode = MinionMode::Autonomous;
                info.last_activity = Utc::now();
            });
        }
    });

    let run_result = run_agent_with_stream_monitoring(
        cmd,
        backend,
        &wt_ctx.minion_dir,
        timeout_opt,
        Some(callback),
        Some(on_spawn),
    )
    .await;

    // Best-effort cleanup: clear PID, set mode to Stopped, and save token usage.
    // Token usage is persisted regardless of exit status (Ok with non-zero exit) because
    // cost data is valuable even for failed tasks. Only stream-level errors (timeout, stuck)
    // result in Err, in which case partial usage is not saved.
    let token_usage = run_result.as_ref().ok().map(|r| r.token_usage.clone());
    let exit_minion_id = wt_ctx.minion_id.clone();
    let _ = with_registry(move |registry| {
        registry.update(&exit_minion_id, |info| {
            info.pid = None;
            info.mode = MinionMode::Stopped;
            if let Some(usage) = token_usage {
                info.token_usage = Some(usage);
            }
        })
    })
    .await;

    let agent_run = run_result?;

    // Post final completion comment
    if let Some(ref client) = issue_ctx.github_client {
        progress_tracker.set_phase(MinionPhase::Completed);

        let final_message = if agent_run.status.success() {
            if agent_run.token_usage.total_tokens() > 0 {
                format!(
                    "✅ Task completed successfully! (tokens: {})",
                    agent_run.token_usage.display_compact()
                )
            } else {
                "✅ Task completed successfully!".to_string()
            }
        } else {
            "❌ Task failed.".to_string()
        };

        let update = progress_tracker.create_update(final_message);
        let comment_body = update.format_comment();

        try_post_progress_comment(
            client,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_ctx.issue_num,
            &comment_body,
        )
        .await;
    }

    // Finish the progress display
    if agent_run.status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue_ctx.issue_num));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue_ctx.issue_num));
    }

    Ok(AgentResult {
        status: agent_run.status,
    })
}

// ---------------------------------------------------------------------------
// Phase 4: Create PR
// ---------------------------------------------------------------------------

/// Creates a PR for the branch and updates labels/registry.
/// Returns the PR number if successful.
pub(crate) async fn handle_pr_creation(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Result<Option<String>> {
    println!("\n🔍 Checking if branch was pushed...");
    let branch_pushed =
        is_branch_pushed(&issue_ctx.owner, &issue_ctx.repo, &wt_ctx.branch_name).await?;

    if !branch_pushed {
        println!("ℹ️  Branch was not pushed. No PR will be created.");
        println!(
            "   Push your changes with: git push origin {}",
            wt_ctx.branch_name
        );
        return Ok(None);
    }

    println!("📋 Branch was pushed, creating pull request...");

    let issue_title_cached = issue_ctx.details.as_ref().map(|d| d.title.as_str());

    match create_pr_for_issue(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        issue_ctx.issue_num,
        &wt_ctx.minion_id,
        &wt_ctx.checkout_path,
        &wt_ctx.minion_dir,
        issue_title_cached,
    )
    .await
    {
        Ok(pr_number) => {
            // Save PR state to minion_dir (metadata)
            let pr_state = PrState::new(pr_number.clone(), issue_ctx.issue_num.to_string());
            pr_state
                .save(&wt_ctx.minion_dir)
                .context("Failed to save PR state")?;

            // Update registry with PR number
            let minion_id_clone = wt_ctx.minion_id.clone();
            let pr_number_clone = pr_number.clone();
            with_registry(move |registry| {
                registry.update(&minion_id_clone, |info| {
                    info.pr = Some(pr_number_clone);
                    info.status = "idle".to_string();
                })
            })
            .await?;

            println!("✅ Draft PR created: #{}", pr_number);
            println!(
                "🔗 View PR at: https://github.com/{}/{}/pull/{}",
                issue_ctx.owner, issue_ctx.repo, pr_number
            );

            // Mark issue as done (fire-and-forget)
            if let Some(ref client) = issue_ctx.github_client {
                match client
                    .mark_issue_done(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num)
                    .await
                {
                    Ok(()) => {
                        println!("🏷️  Updated issue label to 'minion:done'");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to update issue label: {}", e);
                    }
                }
            }

            Ok(Some(pr_number))
        }
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("already exists") || err_msg.contains("A pull request for branch") {
                log::info!(
                    "ℹ️  A PR already exists for branch '{}'",
                    wt_ctx.branch_name
                );
                log::info!(
                    "   Check: https://github.com/{}/{}/pulls",
                    issue_ctx.owner,
                    issue_ctx.repo
                );
            } else if err_msg.contains("branch not found") || err_msg.contains("does not exist") {
                log::warn!("⚠️  Branch was pushed but is no longer available.");
                log::warn!("   It may have been deleted or force-pushed.");
            } else {
                log::warn!("⚠️  Failed to create PR: {}", e);
            }
            log::warn!("   You can create the PR manually if needed.");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 5: Monitor PR lifecycle
// ---------------------------------------------------------------------------

/// Monitors a PR for reviews, CI failures, and merge/close events.
/// Handles automatic review rounds up to MAX_REVIEW_ROUNDS.
async fn monitor_pr_lifecycle(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &str,
    timeout_opt: Option<&str>,
    review_timeout: Option<Duration>,
    monitor_timeout: Duration,
) {
    // Auto-trigger review for Minion-created PRs
    println!("\n🔍 Starting automated PR review...");
    match trigger_pr_review(pr_number, &wt_ctx.checkout_path, review_timeout).await {
        Ok(review_exit_code) => {
            if review_exit_code == 0 {
                println!("✅ PR review completed successfully");
            } else {
                log::warn!(
                    "⚠️  PR review completed with exit code: {}",
                    review_exit_code
                );
            }
        }
        Err(e) => {
            log::warn!("⚠️  Failed to run PR review: {}", e);
            log::warn!("   You can review manually with: gru review {}", pr_number);
        }
    }

    // Start monitoring the PR for review comments, CI failures, and merge/close events
    println!("\n👀 Monitoring PR for updates (polling every 30s)...");
    println!("   Press Ctrl+C to stop monitoring\n");

    let monitor_start = tokio::time::Instant::now();
    let mut review_round = 0;
    let mut ci_escalated = false;
    let mut rebase_attempts = 0;
    loop {
        // Compute remaining time so the timeout spans the entire lifecycle,
        // not just a single monitor_pr invocation.
        let remaining = monitor_timeout.checked_sub(monitor_start.elapsed());
        if remaining.is_none() || remaining == Some(Duration::ZERO) {
            let elapsed = monitor_start.elapsed();
            let total_secs = elapsed.as_secs();
            let hours = total_secs / 3600;
            let minutes = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            let display = if hours > 0 {
                format!("{}h{}m", hours, minutes)
            } else if minutes > 0 {
                format!("{}m", minutes)
            } else {
                format!("{}s", secs)
            };
            println!("⏰ PR monitoring timed out after {}", display);
            println!(
                "   PR is still open: https://github.com/{}/{}/pull/{}",
                issue_ctx.owner, issue_ctx.repo, pr_number
            );
            break;
        }

        match pr_monitor::monitor_pr(
            &issue_ctx.owner,
            &issue_ctx.repo,
            pr_number,
            &wt_ctx.checkout_path,
            remaining,
        )
        .await
        {
            Ok(MonitorResult::Merged) => {
                println!("✅ PR #{} was merged successfully!", pr_number);
                println!("🎉 Issue {} is complete!", issue_ctx.issue_num);
                break;
            }
            Ok(MonitorResult::Closed) => {
                println!("⚠️  PR #{} was closed without merging", pr_number);
                println!("   The issue may need to be reopened or addressed differently");
                break;
            }
            Ok(MonitorResult::NewReviews(comments)) => {
                review_round += 1;
                let count = comments.len();
                println!(
                    "💬 Detected {} new review comment(s) on PR #{} (review round {}/{})",
                    count, pr_number, review_round, MAX_REVIEW_ROUNDS
                );

                if review_round > MAX_REVIEW_ROUNDS {
                    println!(
                        "⚠️  Reached maximum review rounds limit ({})",
                        MAX_REVIEW_ROUNDS
                    );
                    println!("   Additional reviews will need manual handling");
                    println!(
                        "   View PR: https://github.com/{}/{}/pull/{}",
                        issue_ctx.owner, issue_ctx.repo, pr_number
                    );
                    break;
                }

                let review_prompt =
                    pr_monitor::format_review_prompt(issue_ctx.issue_num, pr_number, &comments);

                println!("🔄 Re-invoking to address review feedback...\n");

                let review_registry = AgentRegistry::default_registry();
                let backend = review_registry.default_backend();
                match invoke_agent_for_reviews(
                    backend,
                    &wt_ctx.checkout_path,
                    &wt_ctx.minion_dir,
                    &wt_ctx.session_id,
                    &review_prompt,
                    timeout_opt,
                )
                .await
                {
                    Ok(()) => {
                        println!("\n✅ Finished addressing review comments");
                        println!("🔄 Continuing to monitor PR...\n");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to address review comments: {}", e);
                        log::warn!("   You can address them manually");
                        break;
                    }
                }
            }
            Ok(MonitorResult::FailedChecks(count)) => {
                if ci_escalated {
                    // Already escalated — wait for human intervention
                    println!(
                        "ℹ️  CI still failing ({} check(s)) on PR #{}, waiting for human fix",
                        count, pr_number
                    );
                    // Continue monitoring for merge/close/review events
                    continue;
                }

                println!(
                    "❌ Detected {} failed CI check(s) on PR #{}, attempting auto-fix...",
                    count, pr_number
                );

                // Parse pr_number for the CI fix API
                let pr_num_u64 = match pr_number.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => {
                        println!("⚠️  Could not parse PR number, skipping CI auto-fix");
                        println!("🔄 Continuing to monitor PR for other events...\n");
                        continue;
                    }
                };

                match ci::monitor_and_fix_ci(
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    pr_num_u64,
                    &wt_ctx.branch_name,
                    &wt_ctx.checkout_path,
                )
                .await
                {
                    Ok(true) => {
                        println!("✅ CI checks now pass after auto-fix");
                        println!("🔄 Continuing to monitor PR...\n");
                    }
                    Ok(false) => {
                        ci_escalated = true;
                        println!("⚠️  CI auto-fix escalated to human after max attempts");
                        println!(
                            "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                            issue_ctx.owner, issue_ctx.repo, pr_number
                        );
                        println!("🔄 Continuing to monitor PR for other events...\n");
                    }
                    Err(e) => {
                        println!("⚠️  CI auto-fix error: {}", e);
                        println!(
                            "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                            issue_ctx.owner, issue_ctx.repo, pr_number
                        );
                        println!("🔄 Will retry CI auto-fix on subsequent monitoring cycles...\n");
                    }
                }
            }
            Ok(MonitorResult::MergeConflict) => {
                rebase_attempts += 1;
                println!(
                    "⚠️  Merge conflict detected on PR #{} (rebase attempt {}/{})",
                    pr_number, rebase_attempts, MAX_REBASE_ATTEMPTS
                );

                if rebase_attempts > MAX_REBASE_ATTEMPTS {
                    println!(
                        "❌ Reached maximum rebase attempts ({}), escalating",
                        MAX_REBASE_ATTEMPTS
                    );
                    post_escalation_comment(
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        pr_number,
                        "Auto-rebase failed after multiple attempts. Manual conflict resolution required.",
                    )
                    .await;
                    break;
                }

                match auto_rebase_pr(&wt_ctx.checkout_path).await {
                    Ok(true) => {
                        println!("✅ Rebase succeeded, continuing to monitor PR...\n");
                    }
                    Ok(false) => {
                        // Claude couldn't resolve conflicts
                        println!("❌ Could not resolve merge conflicts automatically");
                        post_escalation_comment(
                            &issue_ctx.owner,
                            &issue_ctx.repo,
                            pr_number,
                            "Auto-rebase failed: could not resolve merge conflicts automatically. Manual intervention required.",
                        )
                        .await;
                        break;
                    }
                    Err(e) => {
                        log::warn!("⚠️  Auto-rebase error: {}", e);
                        post_escalation_comment(
                            &issue_ctx.owner,
                            &issue_ctx.repo,
                            pr_number,
                            &format!("Auto-rebase encountered an error: {}. Manual intervention required.", e),
                        )
                        .await;
                        break;
                    }
                }
            }
            Ok(MonitorResult::Timeout) => {
                // Use the lifecycle-level start time for an accurate total elapsed display
                let total_secs = monitor_start.elapsed().as_secs();
                let hours = total_secs / 3600;
                let minutes = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let display = if hours > 0 {
                    format!("{}h{}m", hours, minutes)
                } else if minutes > 0 {
                    format!("{}m", minutes)
                } else {
                    format!("{}s", secs)
                };
                println!("⏰ PR monitoring timed out after {}", display);
                println!(
                    "   PR is still open: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner, issue_ctx.repo, pr_number
                );
                break;
            }
            Ok(MonitorResult::Interrupted) => {
                println!("\n⚠️  Monitoring interrupted by user");
                println!(
                    "   PR is still open: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner, issue_ctx.repo, pr_number
                );
                break;
            }
            Err(e) => {
                log::warn!("⚠️  PR monitoring failed: {}", e);
                log::warn!(
                    "   You can monitor manually at: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner,
                    issue_ctx.repo,
                    pr_number
                );
                break;
            }
        }
    }
}

/// Monitors CI after the initial fix and attempts auto-fixes if checks fail.
/// Returns Ok(true) if CI passed, Ok(false) if escalated/failed.
async fn monitor_ci_after_fix(
    owner: &str,
    repo: &str,
    branch: &str,
    worktree_path: &Path,
) -> Result<bool> {
    let pr_number = match ci::get_pr_number(owner, repo, branch).await? {
        Some(num) => num,
        None => {
            eprintln!(
                "ℹ️  No PR found for branch {}, skipping CI monitoring",
                branch
            );
            return Ok(true);
        }
    };

    eprintln!("🔍 Monitoring CI for PR #{}", pr_number);
    ci::monitor_and_fix_ci(owner, repo, pr_number, branch, worktree_path).await
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Handles the fix command by delegating to the agent backend.
/// Returns the exit code from the agent process.
///
/// Orchestrates 5 phases:
/// 1. `resolve_issue` - Parse issue, check duplicates, fetch details
/// 2. `setup_worktree` - Clone repo, create worktree, register minion
/// 3. `run_agent_session` - Build prompt, run agent, track progress
/// 4. `handle_pr_creation` - Push check, create PR, update labels
/// 5. `monitor_pr_lifecycle` - Review, poll for updates, handle feedback
///
/// If a previous session for the same issue was interrupted, it will
/// automatically resume from the last completed phase.
pub async fn handle_fix(
    issue: &str,
    timeout_opt: Option<String>,
    review_timeout_opt: Option<String>,
    monitor_timeout_opt: Option<String>,
    quiet: bool,
    force_new: bool,
) -> Result<i32> {
    // Parse review timeout if provided
    let review_timeout = review_timeout_opt
        .map(|s| parse_timeout(&s))
        .transpose()
        .context("Invalid --review-timeout value")?;

    // Parse monitor timeout if provided; default to 24 hours
    let monitor_timeout = match monitor_timeout_opt {
        Some(s) => {
            let d = parse_timeout(&s).context("Invalid --monitor-timeout value")?;
            if d.is_zero() {
                anyhow::bail!("--monitor-timeout must be greater than zero");
            }
            d
        }
        None => Duration::from_secs(24 * 3600),
    };

    // Phase 1: Resolve issue (always runs - need fresh issue details)
    let issue_ctx = resolve_issue(issue).await?;

    // Determine whether to resume an existing session or start fresh
    let (wt_ctx, resume_phase) = if force_new {
        (setup_worktree(&issue_ctx).await?, None)
    } else {
        match check_existing_minions(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num).await?
        {
            ExistingMinionCheck::None => (setup_worktree(&issue_ctx).await?, None),
            ExistingMinionCheck::Resumable(minion_id, info) => {
                let phase = info.orchestration_phase.clone();
                println!(
                    "🔄 Resuming interrupted session {} (phase: {:?})",
                    minion_id, phase
                );
                let session_id = Uuid::parse_str(&info.session_id)
                    .context("Failed to parse session ID from registry")?;
                let checkout_path = info.checkout_path();
                let wt_ctx = WorktreeContext {
                    minion_id,
                    branch_name: info.branch,
                    minion_dir: info.worktree,
                    checkout_path,
                    session_id,
                };
                (wt_ctx, Some(phase))
            }
            ExistingMinionCheck::AlreadyRunning => return Ok(1),
        }
    };

    // Determine the starting phase for the orchestration
    let is_resume = resume_phase.is_some();
    let start_phase = resume_phase.unwrap_or(OrchestrationPhase::Setup);

    // Claim the issue on fresh starts (skip on resume — already claimed)
    if !is_resume {
        if let Some(ref client) = issue_ctx.github_client {
            claim_issue(
                client,
                &issue_ctx.owner,
                &issue_ctx.repo,
                issue_ctx.issue_num,
            )
            .await;
        }
    }

    // Phase 3: Run agent (skip if already past this phase)
    let registry = AgentRegistry::default_registry();
    let backend = registry.default_backend();
    let agent_result = if start_phase <= OrchestrationPhase::RunningAgent {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::RunningAgent).await;

        // Only use --resume if we're resuming a session that actually ran the agent.
        // If interrupted during Setup (before the agent ever ran), the session ID was
        // never used, so resume would fail.
        let use_resume = is_resume && start_phase > OrchestrationPhase::Setup;
        let result = if use_resume {
            resume_agent_session(backend, &issue_ctx, &wt_ctx, quiet, timeout_opt.as_deref()).await
        } else {
            run_agent_session(backend, &issue_ctx, &wt_ctx, quiet, timeout_opt.as_deref()).await
        };

        match result {
            Ok(result) => Some(result),
            Err(e) if is_stuck_or_timeout_error(&e) => {
                // Mark as Failed so the next `gru do` won't retry indefinitely.
                // The user can explicitly `--force-new` to start a fresh session.
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                log::error!("🚨 {:#}", e);
                if let Some(ref client) = issue_ctx.github_client {
                    try_mark_issue_blocked(
                        client,
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        issue_ctx.issue_num,
                    )
                    .await;
                }
                return Ok(1);
            }
            Err(e) => return Err(e),
        }
    } else {
        // Skipping Claude phase — already completed in a previous run
        println!("⏭️  Skipping Claude session (already completed)");
        None
    };

    // Check Claude result if we ran it
    if let Some(ref result) = agent_result {
        if !result.status.success() {
            update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;

            if let Some(ref client) = issue_ctx.github_client {
                match client
                    .mark_issue_failed(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num)
                    .await
                {
                    Ok(()) => {
                        println!("🏷️  Updated issue label to 'minion:failed'");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to update issue label: {}", e);
                    }
                }
            }

            return Ok(result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
        }
    }

    // Phase 4: Create PR (skip if already past this phase)
    let pr_number = if start_phase <= OrchestrationPhase::CreatingPr {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::CreatingPr).await;
        handle_pr_creation(&issue_ctx, &wt_ctx).await?
    } else {
        // Already past PR creation — look up the PR from registry
        println!("⏭️  Skipping PR creation (already completed)");
        let minion_id = wt_ctx.minion_id.clone();
        let existing_pr = with_registry(move |registry| {
            Ok(registry.get(&minion_id).and_then(|info| info.pr.clone()))
        })
        .await?;

        if existing_pr.is_some() {
            existing_pr
        } else {
            // PR number not found in registry (crash during PR creation) — re-run
            log::info!("ℹ️  PR not found in registry, retrying PR creation");
            handle_pr_creation(&issue_ctx, &wt_ctx).await?
        }
    };

    // Phase 5: Monitor PR lifecycle (review + polling)
    if let Some(ref pr_num) = pr_number {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::MonitoringPr).await;
        monitor_pr_lifecycle(
            &issue_ctx,
            &wt_ctx,
            pr_num,
            timeout_opt.as_deref(),
            review_timeout,
            monitor_timeout,
        )
        .await;
    }

    // CI monitoring
    let ci_passed = monitor_ci_after_fix(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        &wt_ctx.checkout_path,
    )
    .await;
    match ci_passed {
        Ok(true) => log::info!("✅ CI checks passed"),
        Ok(false) => {
            log::warn!("⚠️  CI checks failed or were escalated");
            if let Some(ref client) = issue_ctx.github_client {
                try_mark_issue_blocked(
                    client,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
            }
            return Ok(1);
        }
        Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {}", e),
    }

    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;

    let exit_code = agent_result
        .map(|r| r.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
        .unwrap_or(0);

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::AgentRunnerError;

    #[tokio::test]
    async fn test_is_branch_pushed_nonexistent() {
        // Test with a nonexistent branch — gh api should return 404 → Ok(false)
        let result = is_branch_pushed("fotoetienne", "gru", "nonexistent-branch-xyz-12345").await;

        // gh api returns 404 for nonexistent branches, which we map to Ok(false)
        // Skip assertion if gh CLI is not available (e.g., CI without auth)
        match result {
            Ok(pushed) => assert!(!pushed),
            Err(e) => {
                let msg = e.to_string();
                // Acceptable failures: gh not installed, not authenticated
                assert!(
                    msg.contains("gh api failed") || msg.contains("Failed to run gh api"),
                    "Unexpected error: {}",
                    msg
                );
            }
        }
    }

    /// Creates a test `WorktreeContext` with separate minion_dir and checkout_path.
    fn test_wt_ctx(path: &std::path::Path) -> WorktreeContext {
        let checkout = path.join("checkout");
        // Create checkout dir with .git marker so resolve_checkout_path detects new layout
        let _ = std::fs::create_dir_all(&checkout);
        let _ = std::fs::write(checkout.join(".git"), "gitdir: test");
        WorktreeContext {
            minion_id: "M001".to_string(),
            branch_name: "minion/issue-42-M001".to_string(),
            minion_dir: path.to_path_buf(),
            checkout_path: checkout,
            session_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn test_build_fix_prompt_with_details() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 42,
            details: Some(IssueDetails {
                title: "Fix the widget".to_string(),
                body: "The widget is broken".to_string(),
                labels: "bug, priority:high".to_string(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx, &wt_ctx);
        assert!(prompt.starts_with("# Issue #42: Fix the widget"));
        assert!(prompt.contains("octocat/hello-world/issues/42"));
        assert!(prompt.contains("The widget is broken"));
        assert!(prompt.contains("Labels: bug, priority:high"));
        assert!(prompt.contains("## 1. Check if Decomposition is Needed"));
    }

    #[test]
    fn test_build_fix_prompt_without_details() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 42,
            details: None,
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx, &wt_ctx);
        assert_eq!(prompt, "/do 42");
    }

    #[test]
    fn test_build_fix_prompt_empty_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 7,
            details: Some(IssueDetails {
                title: "Add feature".to_string(),
                body: "Please add this feature".to_string(),
                labels: String::new(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx, &wt_ctx);
        assert!(prompt.contains("# Issue #7: Add feature"));
        assert!(!prompt.contains("Labels:"));
    }

    #[test]
    fn test_build_fix_prompt_uses_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "myorg".to_string(),
            repo: "myproject".to_string(),
            issue_num: 99,
            details: Some(IssueDetails {
                title: "Template test".to_string(),
                body: "Body content here".to_string(),
                labels: "enhancement".to_string(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx, &wt_ctx);

        // Verify template variables were substituted (no {{ }} patterns remaining
        // for known variables)
        assert!(!prompt.contains("{{ issue_number }}"));
        assert!(!prompt.contains("{{ issue_title }}"));
        assert!(!prompt.contains("{{ issue_body }}"));
        assert!(!prompt.contains("{{ repo_owner }}"));
        assert!(!prompt.contains("{{ repo_name }}"));

        // Verify the substituted values are present
        assert!(prompt.contains("99"));
        assert!(prompt.contains("Template test"));
        assert!(prompt.contains("Body content here"));
        assert!(prompt.contains("myorg"));
        assert!(prompt.contains("myproject"));
        assert!(prompt.contains("Labels: enhancement"));
    }

    #[test]
    fn test_build_fix_prompt_repo_override() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        // Create a custom do prompt in the checkout dir (where the repo lives)
        let prompts_dir = wt_ctx.checkout_path.join(".gru").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(
            prompts_dir.join("do.md"),
            r#"---
description: Custom do
requires: [issue]
---
CUSTOM: Fix #{{ issue_number }} - {{ issue_title }}"#,
        )
        .unwrap();

        let ctx = IssueContext {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            issue_num: 55,
            details: Some(IssueDetails {
                title: "Custom test".to_string(),
                body: "Custom body".to_string(),
                labels: String::new(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx, &wt_ctx);
        assert_eq!(prompt, "CUSTOM: Fix #55 - Custom test");
    }

    #[test]
    fn test_create_wip_template() {
        let (title, body) = create_wip_template("M042", 123, "Fix login bug");
        assert_eq!(title, "[M042] Fixes #123: Fix login bug");
        assert!(body.contains("Automated fix for #123 by Minion M042"));
        assert!(body.contains("- [ ] Implementation"));
        assert!(body.contains("Fixes #123"));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_stuck() {
        let err: anyhow::Error = AgentRunnerError::InactivityStuck { minutes: 15 }.into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_task_timeout() {
        let err: anyhow::Error =
            AgentRunnerError::MaxTimeout(tokio::time::Duration::from_secs(600)).into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_stream_timeout() {
        let err: anyhow::Error = AgentRunnerError::StreamTimeout { seconds: 300 }.into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_other_error() {
        let err = anyhow::anyhow!("Failed to spawn claude process");
        assert!(!is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_wrapped_in_context() {
        // Typed errors survive context wrapping, unlike string matching
        let err: anyhow::Error = AgentRunnerError::InactivityStuck { minutes: 15 }.into();
        let wrapped = err.context("Claude session failed");
        assert!(is_stuck_or_timeout_error(&wrapped));
    }
}
