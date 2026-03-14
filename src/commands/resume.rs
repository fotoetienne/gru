use crate::agent::{AgentBackend, AgentEvent};
use crate::agent_registry;
use crate::agent_runner::{
    is_stuck_or_timeout_error, run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::commands::fix::{
    handle_pr_creation, update_orchestration_phase, IssueContext, WorktreeContext,
};
use crate::config::{LabConfig, DEFAULT_MAX_RESUME_ATTEMPTS};
use crate::git;
use crate::github::GitHubClient;
use crate::minion_registry::{
    mark_minion_failed, revert_to_stopped, with_registry, MinionMode, MinionRegistry,
    OrchestrationPhase,
};
use crate::minion_resolver;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::session_claim;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
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

    // Build the continuation prompt
    let prompt = if let Some(ref extra) = additional_prompt {
        format!(
            "Continue working on this issue. Additional instructions: {}",
            extra
        )
    } else {
        format!(
            "Continue working on issue #{}. Pick up where you left off. \
             If you've already completed the implementation, proceed to push and write PR_DESCRIPTION.md.",
            issue_num
        )
    };

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

    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::RunningAgent).await;

    // Run agent in autonomous mode with stream monitoring
    let claude_result = run_autonomous_agent(
        &*backend,
        &wt_ctx,
        &prompt,
        quiet,
        effective_timeout.as_deref(),
        issue_num,
    )
    .await;

    match claude_result {
        Ok(status) => {
            if !status.success() {
                update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
                println!("❌ Claude session exited with non-zero status");
                return Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
            }
        }
        Err(e) if is_stuck_or_timeout_error(&e) => {
            update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Failed).await;
            log::error!("🚨 {:#}", e);
            return Ok(1);
        }
        Err(e) => return Err(e),
    }

    // Phase: Create PR (handle_pr_creation checks if branch was pushed internally)
    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::CreatingPr).await;

    // Detect host from the worktree's git remote
    let checkout = wt_ctx.checkout_path.clone();
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let host = detect_host_from_worktree(&checkout, &github_hosts)
        .await
        .unwrap_or_else(|_| "github.com".to_string());
    let github_client = match GitHubClient::from_env_with_host(&owner, &repo_name, &host).await {
        Ok(client) => Some(client),
        Err(e) => {
            log::warn!("⚠️  GitHub client unavailable: {}", e);
            None
        }
    };
    let issue_ctx = IssueContext {
        owner,
        repo: repo_name,
        host: host.clone(),
        issue_num,
        details: None,
        github_client,
    };

    match handle_pr_creation(&issue_ctx, &wt_ctx).await {
        Ok(Some(_)) => {}
        Ok(None) => {}
        Err(e) => {
            log::warn!("⚠️  PR creation failed: {}", e);
        }
    }

    // Respect no_watch: skip lifecycle monitoring for fire-and-forget minions
    if no_watch {
        println!("🏁 PR created. Skipping lifecycle monitoring (no_watch).");
        update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Completed).await;
        println!("✅ Resume completed for Minion {}", minion.minion_id);
        return Ok(0);
    }

    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Completed).await;
    println!("✅ Resume completed for Minion {}", minion.minion_id);
    Ok(0)
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
) -> Result<std::process::ExitStatus> {
    let mut cmd = backend
        .build_resume_command(&wt_ctx.checkout_path, &wt_ctx.session_id, prompt)
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
            info.pid = None;
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

/// Detect the GitHub host from a worktree's git remote.
async fn detect_host_from_worktree(
    worktree_path: &std::path::Path,
    github_hosts: &[String],
) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .context("Failed to execute git remote get-url")?;

    if !output.status.success() {
        anyhow::bail!("Failed to get remote URL from worktree");
    }

    let remote_url = String::from_utf8(output.stdout)
        .context("Remote URL is not valid UTF-8")?
        .trim()
        .to_string();

    let (host, _, _) = git::parse_github_remote(&remote_url, github_hosts)
        .context("Failed to parse GitHub remote URL")?;
    Ok(host)
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
}
