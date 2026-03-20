use crate::agent::{AgentBackend, AgentEvent};
use crate::agent_registry;
use crate::agent_runner::{
    is_stuck_or_timeout_error, run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::commands::fix::{
    handle_pr_creation, monitor_ci_after_fix, monitor_pr_lifecycle, try_mark_issue_blocked,
    try_mark_issue_failed, update_orchestration_phase, IssueContext, WorktreeContext,
};
use crate::config::{LabConfig, DEFAULT_MAX_RESUME_ATTEMPTS};
use crate::minion_registry::{
    mark_minion_failed, revert_to_stopped, with_registry, MinionMode, MinionRegistry,
    OrchestrationPhase,
};
use crate::minion_resolver;
use crate::pr_monitor::MonitorResult;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::session_claim;
use crate::tmux::TmuxGuard;
use anyhow::{bail, Context, Result};
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
    // Priority: explicit additional_prompt > wake_reason (review-focused) > generic continuation.
    //
    // Note: when the lab daemon wakes a minion for new reviews it sets start_phase =
    // MonitoringPr, so the agent-run branch below (`start_phase <= RunningAgent`) is
    // skipped and `prompt` is never passed to the agent directly.  In that case
    // `wake_reason` acts as metadata signalling WHY the minion was woken, and the actual
    // review response is handled by `monitor_pr_lifecycle`'s own review-detection loop
    // (which uses `last_review_check_time` as a baseline for new reviews).
    let prompt = if let Some(ref extra) = additional_prompt {
        format!(
            "Continue working on this issue. Additional instructions: {}",
            extra
        )
    } else if let Some(ref reason) = wake_reason {
        reason.clone()
    } else {
        format!(
            "Continue working on issue #{}. Pick up where you left off. \
             If you've already completed the implementation, proceed to push and write PR_DESCRIPTION.md.",
            issue_num
        )
    };

    // Rename tmux window for the resume session
    let _tmux_guard = TmuxGuard::new(&format!("gru:{}", minion.minion_id));

    println!(
        "🔄 Resuming Minion {} in autonomous mode...",
        minion.minion_id
    );
    let checkout_path = minion.checkout_path();
    println!("📂 Workspace: {}", checkout_path.display());

    // Build WorktreeContext for PR creation
    let wt_ctx = WorktreeContext {
        minion_id: minion.minion_id.clone(),
        branch_name: branch_name.clone(),
        minion_dir: minion.worktree_path.clone(),
        checkout_path,
        session_id: session_uuid,
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
    let host = resolve_host_from_worktree(&wt_ctx.checkout_path, &owner).await;
    let issue_ctx = IssueContext {
        owner,
        repo: repo_name,
        host,
        issue_num,
        details: None,
    };

    // Phase: Run agent (skip if already past this phase)
    if start_phase <= OrchestrationPhase::RunningAgent {
        update_orchestration_phase(&minion.minion_id, OrchestrationPhase::RunningAgent).await;

        let claude_result = run_autonomous_agent(
            &*backend,
            &wt_ctx,
            &prompt,
            quiet,
            effective_timeout.as_deref(),
            issue_num,
            &issue_ctx.host,
        )
        .await;

        match claude_result {
            Ok(status) => {
                if !status.success() {
                    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                    try_mark_issue_failed(
                        &issue_ctx.host,
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        issue_ctx.issue_num,
                    )
                    .await;
                    println!("❌ Claude session exited with non-zero status");
                    return Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
                }
            }
            Err(e) if is_stuck_or_timeout_error(&e) => {
                update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
                log::error!("🚨 {:#}", e);
                return Ok(1);
            }
            Err(e) => {
                update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        }
    } else {
        println!("⏭️  Skipping agent session (already completed)");
    }

    // Phase: Create PR (skip if already past this phase)
    let pr_number = if start_phase <= OrchestrationPhase::CreatingPr {
        update_orchestration_phase(&minion.minion_id, OrchestrationPhase::CreatingPr).await;

        let pr_number = match handle_pr_creation(&issue_ctx, &wt_ctx).await {
            Ok(pr) => pr,
            Err(e) => {
                update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        };

        // If handle_pr_creation didn't return a PR number, check the registry
        // (the PR may have been created in a previous session)
        match pr_number {
            Some(pr) => Some(pr),
            None => {
                let mid = minion.minion_id.clone();
                with_registry(move |registry| {
                    Ok(registry.get(&mid).and_then(|info| info.pr.clone()))
                })
                .await
                .unwrap_or_else(|e| {
                    log::warn!("Failed to look up PR number from registry: {}", e);
                    None
                })
            }
        }
    } else {
        println!("⏭️  Skipping PR creation (already completed)");
        let mid = minion.minion_id.clone();
        with_registry(move |registry| Ok(registry.get(&mid).and_then(|info| info.pr.clone())))
            .await
            .unwrap_or_else(|e| {
                log::warn!("Failed to look up PR number from registry: {}", e);
                None
            })
    };

    // Last resort: discover PR by head branch (handles manual PR creation or missing registry state)
    let pr_number = match pr_number {
        Some(pr) => Some(pr),
        None => {
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
    };

    // Respect no_watch: skip lifecycle monitoring for fire-and-forget minions
    if no_watch {
        if let Some(ref pr_num) = pr_number {
            println!(
                "PR #{}. Skipping lifecycle monitoring (--no-watch).",
                pr_num
            );
        }
        update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Completed).await;
        println!("✅ Resume completed for Minion {}", minion.minion_id);
        return Ok(0);
    }

    // Phase: Monitor PR lifecycle (reviews, CI, merge)
    let mut pr_terminal_result = None;
    if let Some(ref pr_num) = pr_number {
        update_orchestration_phase(&minion.minion_id, OrchestrationPhase::MonitoringPr).await;
        let monitor_timeout = Duration::from_secs(24 * 3600);
        pr_terminal_result = monitor_pr_lifecycle(
            &*backend,
            &issue_ctx,
            &wt_ctx,
            pr_num,
            effective_timeout.as_deref(),
            None, // review_timeout: use default
            monitor_timeout,
        )
        .await;
    }

    // CI monitoring (only if a PR exists and wasn't already merged/closed)
    let skip_ci = matches!(
        pr_terminal_result,
        Some(MonitorResult::Merged) | Some(MonitorResult::Closed)
    );
    if pr_number.is_some() && !skip_ci {
        let ci_passed = monitor_ci_after_fix(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            &wt_ctx.branch_name,
            &wt_ctx.checkout_path,
            &wt_ctx.minion_id,
        )
        .await;
        match ci_passed {
            Ok(true) => log::info!("✅ CI checks passed"),
            Ok(false) => {
                log::warn!("⚠️  CI checks failed or were escalated");
                update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
                return Ok(1);
            }
            Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {}", e),
        }
    }

    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Completed).await;
    println!("✅ Resume completed for Minion {}", minion.minion_id);
    Ok(0)
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

/// Runs the agent in autonomous mode with stream monitoring.
///
/// Spawns the agent with resume and stream-json output, tracks progress,
/// and updates the registry with PID/mode during execution.
async fn run_autonomous_agent(
    backend: &dyn AgentBackend,
    wt_ctx: &WorktreeContext,
    prompt: &str,
    quiet: bool,
    timeout_opt: Option<&str>,
    issue_num: u64,
    github_host: &str,
) -> Result<std::process::ExitStatus> {
    let mut cmd = backend
        .build_resume_command(
            &wt_ctx.checkout_path,
            &wt_ctx.session_id,
            prompt,
            github_host,
        )
        .context("Agent backend does not support resume")?;
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);

    let config = ProgressConfig {
        minion_id: wt_ctx.minion_id.clone(),
        issue: issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let callback = move |event: &AgentEvent| {
        progress.handle_event(event);
    };

    let on_spawn =
        MinionRegistry::pid_callback(wt_ctx.minion_id.clone(), Some(MinionMode::Autonomous));

    let run_result = run_agent_with_stream_monitoring(
        cmd,
        backend,
        &wt_ctx.minion_dir,
        timeout_opt,
        Some(callback),
        Some(on_spawn),
    )
    .await;

    // Cleanup: clear PID, set mode to Stopped, save token usage
    let token_usage = run_result.as_ref().ok().map(|r| r.token_usage.clone());
    let exit_minion_id = wt_ctx.minion_id.clone();
    let _ = with_registry(move |registry| {
        registry.update(&exit_minion_id, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
            if let Some(usage) = token_usage {
                info.token_usage = Some(usage);
            }
        })
    })
    .await;

    let agent_run = run_result?;
    Ok(agent_run.status)
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
