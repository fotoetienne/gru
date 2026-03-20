use crate::agent::AgentBackend;
use crate::agent_registry;
use crate::agent_runner::{is_stuck_or_timeout_error, EXIT_CODE_SIGNAL_TERMINATED};
use crate::commands::fix::{
    agent_exit_code, create_pr_phase, monitor_pr_phase, run_agent_phase,
    update_orchestration_phase, IssueContext, WorktreeContext,
};
use crate::config::{LabConfig, DEFAULT_MAX_RESUME_ATTEMPTS};
use crate::minion_registry::{
    mark_minion_failed, revert_to_stopped, with_registry, MinionMode, OrchestrationPhase,
};
use crate::minion_resolver;
use crate::session_claim;
use crate::tmux::TmuxGuard;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use tokio::time::Duration;
use uuid::Uuid;

/// Load the `max_resume_attempts` setting from config, falling back to the default.
fn load_max_resume_attempts() -> u32 {
    LabConfig::default_path()
        .ok()
        .and_then(|p| LabConfig::load_partial(&p).ok())
        .map(|c| c.daemon.max_resume_attempts)
        .unwrap_or(DEFAULT_MAX_RESUME_ATTEMPTS)
}

/// Context produced by `check_resumption_preconditions`, consumed by `run_resume_pipeline`.
struct ResumeContext {
    wt_ctx: WorktreeContext,
    issue_ctx: IssueContext,
    backend: Box<dyn AgentBackend>,
    resume_prompt: Option<String>,
    start_phase: OrchestrationPhase,
    effective_timeout: Option<String>,
    no_watch: bool,
}

/// Handles the resume command: resumes a stopped Minion in autonomous mode.
///
/// Unlike `gru attach` (which runs interactively), `gru resume` runs in
/// autonomous mode with stream-json monitoring and auto-PR creation — the
/// same execution model as `gru do`.
///
/// Flow:
/// 1. Resolve the minion ID and load registry info
/// 2. Check that the minion is stopped (error if already running)
/// 3. Spawn Claude with `--resume` in stream-json mode
/// 4. Monitor output with progress display and timeout detection
/// 5. Auto-create PR if branch was pushed
/// 6. Update registry on exit
pub async fn handle_resume(
    id: String,
    additional_prompt: Option<String>,
    timeout_opt: Option<String>,
    quiet: bool,
) -> Result<i32> {
    let ctx = check_resumption_preconditions(id, additional_prompt, timeout_opt).await?;
    run_resume_pipeline(ctx, quiet).await
}

/// Validates resumption preconditions: resolves the minion, claims the session,
/// checks attempt count and timeout, builds the prompt, and resolves the backend.
///
/// Returns a `ResumeContext` ready for `run_resume_pipeline`.
async fn check_resumption_preconditions(
    id: String,
    additional_prompt: Option<String>,
    timeout_opt: Option<String>,
) -> Result<ResumeContext> {
    // Resolve the minion ID (same smart resolution as gru path/attach)
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Verify minion directory still exists
    if !minion.worktree_path.exists() {
        bail!(
            "Minion directory no longer exists: {}\n\
             The worktree may have been removed. Try 'gru status' to see active minions.",
            minion.worktree_path.display()
        );
    }

    // Atomically check registry state and claim as Autonomous
    let registry_info = session_claim::check_and_claim_session(
        &minion.minion_id,
        MinionMode::Autonomous,
        false, // not graceful: resume requires registry
    )
    .await?;

    let info = match registry_info {
        Some(info) => info,
        None => {
            bail!(
                "Minion {} is not in the registry. Cannot resume without session context.\n\
                 Use 'gru attach {}' for interactive mode instead.",
                minion.minion_id,
                minion.minion_id
            );
        }
    };

    // Parse owner/repo from "owner/repo" format
    let (owner, repo_name) = info
        .repo
        .split_once('/')
        .map(|(o, r)| (o.to_string(), r.to_string()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid repo format in registry: '{}' (expected 'owner/repo')",
                info.repo
            )
        })?;

    let session_id = info.session_id;
    let issue_num = info.issue;
    let branch_name = info.branch;
    let agent_name = info.agent_name;
    let timeout_deadline: Option<DateTime<Utc>> = info.timeout_deadline;
    let attempt_count = info.attempt_count;
    let no_watch = info.no_watch;
    let start_phase = info.orchestration_phase.clone();
    let wake_reason = info.wake_reason.clone();

    // Check if timeout_deadline has passed — fail instead of resuming
    if let Some(deadline) = timeout_deadline {
        if Utc::now() >= deadline {
            mark_minion_failed(&minion.minion_id).await;
            bail!(
                "Minion {} has passed its timeout deadline ({}). Marking as failed.",
                minion.minion_id,
                deadline
            );
        }
    }

    // Increment attempt_count for this resume
    let mid = minion.minion_id.clone();
    let new_attempt_count = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.attempt_count = info.attempt_count.saturating_add(1);
        })?;
        let count = reg.get(&mid).map(|i| i.attempt_count).unwrap_or(0);
        Ok(count)
    })
    .await
    .unwrap_or(attempt_count.saturating_add(1));

    // Check if attempt_count exceeds max_resume_attempts
    let max_attempts = load_max_resume_attempts();
    if new_attempt_count > max_attempts {
        mark_minion_failed(&minion.minion_id).await;
        bail!(
            "Minion {} has exceeded maximum resume attempts ({} > {}). Marking as failed.",
            minion.minion_id,
            new_attempt_count,
            max_attempts
        );
    }

    let session_uuid = match Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            // Revert registry to Stopped since we claimed Autonomous but can't proceed
            revert_to_stopped(&minion.minion_id).await;
            return Err(anyhow::anyhow!(e).context("Failed to parse session ID from registry"));
        }
    };

    // Clear wake_reason unconditionally so it never leaks in the registry,
    // regardless of which prompt branch wins below.
    if wake_reason.is_some() {
        let mid = minion.minion_id.clone();
        if let Err(e) = with_registry(move |reg| {
            reg.update(&mid, |i| {
                i.wake_reason = None;
            })
        })
        .await
        {
            log::warn!(
                "Failed to clear wake_reason for {}: {}",
                minion.minion_id,
                e
            );
        }
    }

    // Build the continuation prompt.
    // Priority: explicit additional_prompt > wake_reason (review-focused) > None (use default).
    //
    // Note: when the lab daemon wakes a minion for new reviews it sets start_phase =
    // MonitoringPr, so the agent-run phase is skipped and the prompt is never passed
    // to the agent directly. In that case `wake_reason` acts as metadata signalling
    // WHY the minion was woken, and the actual review response is handled by
    // `monitor_pr_lifecycle`'s own review-detection loop.
    let resume_prompt = if let Some(ref extra) = additional_prompt {
        Some(format!(
            "Continue working on this issue. Additional instructions: {}",
            extra
        ))
    } else {
        wake_reason
    };

    // Resolve the agent backend from registry (use stored agent name)
    let backend = match agent_registry::resolve_backend(&agent_name) {
        Ok(b) => b,
        Err(e) => {
            // Revert registry to Stopped since we claimed Autonomous but can't proceed
            revert_to_stopped(&minion.minion_id).await;
            return Err(e.context("Failed to resolve agent backend for resume"));
        }
    };

    // Compute effective timeout: use CLI flag if provided, otherwise compute
    // remaining time from timeout_deadline. This ensures resumed minions honor
    // the original timeout budget rather than resetting it.
    let effective_timeout: Option<String> = if timeout_opt.is_some() {
        timeout_opt
    } else if let Some(deadline) = timeout_deadline {
        let remaining = deadline - Utc::now();
        if remaining.num_seconds() > 0 {
            Some(format!("{}s", remaining.num_seconds()))
        } else {
            // Deadline just passed between the check above and here — treat as expired
            mark_minion_failed(&minion.minion_id).await;
            bail!(
                "Minion {} has passed its timeout deadline ({}). Marking as failed.",
                minion.minion_id,
                deadline
            );
        }
    } else {
        None
    };

    // Resolve host from worktree git remote, falling back to config-based inference
    let checkout_path = minion.checkout_path();
    let host = resolve_host_from_worktree(&checkout_path, &owner).await;

    let wt_ctx = WorktreeContext {
        minion_id: minion.minion_id.clone(),
        branch_name,
        minion_dir: minion.worktree_path.clone(),
        checkout_path,
        session_id: session_uuid,
    };

    let issue_ctx = IssueContext {
        owner,
        repo: repo_name,
        host,
        issue_num,
        details: None,
    };

    Ok(ResumeContext {
        wt_ctx,
        issue_ctx,
        backend,
        resume_prompt,
        start_phase,
        effective_timeout,
        no_watch,
    })
}

/// Runs the resume pipeline: agent session, PR creation, and PR monitoring.
///
/// Reuses the phase helpers from `fix/worker.rs` to avoid duplicating
/// orchestration logic.
async fn run_resume_pipeline(ctx: ResumeContext, quiet: bool) -> Result<i32> {
    let ResumeContext {
        wt_ctx,
        issue_ctx,
        backend,
        resume_prompt,
        start_phase,
        effective_timeout,
        no_watch,
    } = ctx;

    // Rename tmux window for the resume session
    let _tmux_guard = TmuxGuard::new(&format!("gru:{}", wt_ctx.minion_id));

    println!(
        "🔄 Resuming Minion {} in autonomous mode...",
        wt_ctx.minion_id
    );
    println!("📂 Workspace: {}", wt_ctx.checkout_path.display());

    // Phase: Run agent
    let agent_result = match run_agent_phase(
        &*backend,
        &issue_ctx,
        &wt_ctx,
        &start_phase,
        quiet,
        effective_timeout.as_deref(),
        resume_prompt.as_deref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) if is_stuck_or_timeout_error(&e) => {
            log::error!("🚨 {:#}", e);
            return Ok(1);
        }
        Err(e) => return Err(e),
    };

    // Check agent result — non-zero exit means failure
    if let Some(ref result) = agent_result {
        if !result.status.success() {
            println!("❌ Agent session exited with non-zero status");
            return Ok(result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
        }
    }

    // Phase: Create PR
    let mut pr_number = create_pr_phase(&issue_ctx, &wt_ctx, &start_phase, false).await?;

    // Last resort: discover PR by head branch (handles manual PR creation or missing registry state)
    if pr_number.is_none() {
        pr_number = discover_pr_by_branch(&issue_ctx, &wt_ctx).await;
    }

    // Respect no_watch: skip lifecycle monitoring for fire-and-forget minions
    if no_watch {
        if let Some(ref pr_num) = pr_number {
            println!(
                "PR #{}. Skipping lifecycle monitoring (--no-watch).",
                pr_num
            );
        }
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
        cleanup_registry(&wt_ctx.minion_id).await;
        println!("✅ Resume completed for Minion {}", wt_ctx.minion_id);
        return Ok(agent_exit_code(&agent_result));
    }

    // Phase: Monitor PR lifecycle (reviews, CI, merge).
    // When no PR exists, monitor_pr_phase falls back to standalone CI monitoring —
    // aligning resume behavior with `gru do`.
    let monitor_timeout = Duration::from_secs(24 * 3600);
    let monitor_result = monitor_pr_phase(
        &*backend,
        &issue_ctx,
        &wt_ctx,
        &pr_number,
        effective_timeout.as_deref(),
        None, // review_timeout: use default
        monitor_timeout,
    )
    .await;

    if monitor_result.is_err() {
        cleanup_registry(&wt_ctx.minion_id).await;
        return Ok(1);
    }

    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
    cleanup_registry(&wt_ctx.minion_id).await;
    println!("✅ Resume completed for Minion {}", wt_ctx.minion_id);
    Ok(agent_exit_code(&agent_result))
}

/// Best-effort registry cleanup: clear PID and set mode to Stopped.
///
/// Mirrors the cleanup in `fix::run_worker` so the minion isn't left in
/// Autonomous mode after the pipeline finishes.
async fn cleanup_registry(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
        })
    })
    .await;
}

/// Last-resort PR discovery by head branch name.
///
/// Handles cases where the PR was created manually or the registry state is missing.
async fn discover_pr_by_branch(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Option<String> {
    match crate::ci::get_pr_number(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        None,
    )
    .await
    {
        Ok(Some(num)) => {
            log::info!("Discovered PR #{} by branch name", num);
            Some(num.to_string())
        }
        Ok(None) => None,
        Err(e) => {
            log::warn!("Failed to discover PR by branch: {}", e);
            None
        }
    }
}

/// Resolve the GitHub host for a worktree by inspecting its git remote.
/// Falls back to extracting the host directly from the remote URL, then to
/// config-based `infer_github_host` if neither approach succeeds.
pub(crate) async fn resolve_host_from_worktree(
    checkout_path: &std::path::Path,
    owner: &str,
) -> String {
    let github_hosts = crate::config::load_host_registry().all_hosts();

    // Try to get the host from the worktree's git remote
    let output = tokio::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(checkout_path)
        .output()
        .await;

    if let Ok(output) = output {
        if output.status.success() {
            let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // First try the full parser (validates against known hosts)
            if let Ok((host, _, _)) = crate::git::parse_github_remote(&remote_url, &github_hosts) {
                return host;
            }
            // If the remote URL is valid but the host isn't in the registry,
            // extract the host directly so we don't wrongly default to github.com.
            if let Some(host) = extract_host_from_remote_url(&remote_url) {
                return host;
            }
        }
    }

    // Fallback to config-based heuristic
    crate::github::infer_github_host(owner)
}

/// Extract the hostname from a git remote URL without requiring it to be in the
/// known hosts registry. Supports HTTPS (`https://host/...`) and SSH (`git@host:...`).
fn extract_host_from_remote_url(url: &str) -> Option<String> {
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        // https://host/owner/repo.git -> host
        rest.split('/')
            .next()
            .map(|h| h.to_string())
            .filter(|h| !h.is_empty())
    } else if let Some(rest) = url.strip_prefix("git@") {
        // git@host:owner/repo.git -> host
        rest.split(':')
            .next()
            .map(|h| h.to_string())
            .filter(|h| !h.is_empty())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_resume_with_invalid_id() {
        let result = handle_resume("nonexistent-minion-xyz".to_string(), None, None, false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_handle_resume_with_prompt_and_invalid_id() {
        let result = handle_resume(
            "nonexistent-minion-xyz".to_string(),
            Some("Add error handling".to_string()),
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[test]
    fn test_load_max_resume_attempts_returns_default_without_config() {
        assert_eq!(DEFAULT_MAX_RESUME_ATTEMPTS, 3);
        let max = load_max_resume_attempts();
        assert!(max >= 1, "load_max_resume_attempts must return at least 1");
    }

    #[test]
    fn test_extract_host_from_remote_url_https() {
        assert_eq!(
            extract_host_from_remote_url("https://github.example.com/owner/repo.git"),
            Some("github.example.com".to_string())
        );
        assert_eq!(
            extract_host_from_remote_url("https://github.com/owner/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_extract_host_from_remote_url_ssh() {
        assert_eq!(
            extract_host_from_remote_url("git@github.example.com:owner/repo.git"),
            Some("github.example.com".to_string())
        );
        assert_eq!(
            extract_host_from_remote_url("git@github.com:owner/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_extract_host_from_remote_url_invalid() {
        assert_eq!(extract_host_from_remote_url("not-a-url"), None);
        assert_eq!(extract_host_from_remote_url(""), None);
    }
}
