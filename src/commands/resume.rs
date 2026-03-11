use crate::agent::AgentEvent;
use crate::agent_registry::AgentRegistry;
use crate::agent_runner::{
    is_stuck_or_timeout_error, run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::commands::fix::{
    handle_pr_creation, update_orchestration_phase, IssueContext, WorktreeContext,
};
use crate::config::AgentConfig;
use crate::github::GitHubClient;
use crate::minion_registry::{
    is_process_alive, with_registry, MinionMode, MinionRegistry, OrchestrationPhase,
};
use crate::minion_resolver;
use crate::progress::{ProgressConfig, ProgressDisplay};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use uuid::Uuid;

/// Typed errors from the resume command.
#[derive(Debug)]
enum ResumeError {
    /// The minion has a live process — user must stop it first.
    AlreadyRunning { minion_id: String, mode: MinionMode },
    /// Registry shows a non-Stopped mode but no PID is recorded.
    InconsistentState { minion_id: String, mode: MinionMode },
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeError::AlreadyRunning { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                    minion_id, mode, minion_id
                )
            }
            ResumeError::InconsistentState { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is currently in {} mode without an associated process. \
                     Please wait or run 'gru status' / 'gru stop {}' to recover.",
                    minion_id, mode, minion_id
                )
            }
        }
    }
}

impl std::error::Error for ResumeError {}

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
    let registry_info = check_and_claim_session(&minion.minion_id).await?;

    let (session_id, owner, repo, issue_num, branch_name) = match registry_info {
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

    // Update orchestration phase
    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::RunningClaude).await;

    // Run Claude in autonomous mode with stream monitoring
    let claude_result =
        run_autonomous_agent(&wt_ctx, &prompt, quiet, timeout_opt.as_deref(), issue_num).await;

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

    let github_client = match GitHubClient::from_env(&owner, &repo).await {
        Ok(client) => Some(client),
        Err(e) => {
            log::warn!("⚠️  GitHub client unavailable: {}", e);
            None
        }
    };

    let issue_ctx = IssueContext {
        owner,
        repo,
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

    update_orchestration_phase(&minion.minion_id, OrchestrationPhase::Completed).await;
    println!("✅ Resume completed for Minion {}", minion.minion_id);
    Ok(0)
}

/// Reverts the registry to Stopped mode (best-effort).
/// Used when claim succeeded but we can't proceed with the spawn.
async fn revert_to_stopped(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.pid = None;
            info.last_activity = Utc::now();
        })
    })
    .await;
}

/// Runs the agent in autonomous mode with stream monitoring.
///
/// Spawns the agent with resume and stream-json output, tracks progress,
/// and updates the registry with PID/mode during execution.
async fn run_autonomous_agent(
    wt_ctx: &WorktreeContext,
    prompt: &str,
    quiet: bool,
    timeout_opt: Option<&str>,
    issue_num: u64,
) -> Result<std::process::ExitStatus> {
    let registry = AgentRegistry::from_config(&AgentConfig::default())?;
    let backend = registry.default_backend();
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

    // Register PID on spawn. Uses MinionRegistry::load directly instead of
    // with_registry because the on_spawn callback is synchronous (FnOnce, not async).
    // This is consistent with the same pattern in fix.rs::run_agent_session_inner.
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

/// Atomically checks if the minion is available and claims it as Autonomous.
///
/// Returns the (session_id, owner, repo, issue_num, branch) if the minion is
/// found in the registry and is available for resume.
/// Returns None if the minion is not in the registry.
/// Errors with [`ResumeError::AlreadyRunning`] if the minion has a live process.
async fn check_and_claim_session(
    minion_id: &str,
) -> Result<Option<(String, String, String, u64, String)>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| {
        let info_data = reg.get(&id).map(|info| {
            (
                info.session_id.clone(),
                info.mode.clone(),
                info.pid,
                info.repo.clone(),
                info.issue,
                info.branch.clone(),
            )
        });

        match info_data {
            Some((session_id, mode, pid, repo, issue, branch)) => {
                // Check if already running
                if mode != MinionMode::Stopped {
                    match pid {
                        Some(pid_val) => {
                            if is_process_alive(pid_val) {
                                return Err(ResumeError::AlreadyRunning {
                                    minion_id: id,
                                    mode,
                                }
                                .into());
                            }
                            // Stale entry: reset to Stopped before claiming
                            reg.update(&id, |info| {
                                info.mode = MinionMode::Stopped;
                                info.pid = None;
                                info.last_activity = Utc::now();
                            })?;
                        }
                        None => {
                            // Inconsistent state: mode != Stopped but no PID recorded.
                            // Treat this as locked/in use to avoid double-claiming.
                            return Err(ResumeError::InconsistentState {
                                minion_id: id,
                                mode,
                            }
                            .into());
                        }
                    }
                }

                // Claim as Autonomous
                reg.update(&id, |info| {
                    info.mode = MinionMode::Autonomous;
                    info.last_activity = Utc::now();
                })?;

                // Parse owner/repo from "owner/repo" format
                let (owner, repo_name) = repo
                    .split_once('/')
                    .map(|(o, r)| (o.to_string(), r.to_string()))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Invalid repo format in registry: '{}' (expected 'owner/repo')",
                            repo
                        )
                    })?;

                Ok(Some((session_id, owner, repo_name, issue, branch)))
            }
            None => Ok(None),
        }
    })
    .await;

    result.context("Failed to access minion registry")
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
    fn test_resume_error_display_already_running() {
        let err = ResumeError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("already running"));
        assert!(msg.contains("autonomous"));
        assert!(msg.contains("gru stop M001"));
    }

    #[test]
    fn test_resume_error_display_inconsistent_state() {
        let err = ResumeError::InconsistentState {
            minion_id: "M002".to_string(),
            mode: MinionMode::Interactive,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("interactive mode"));
        assert!(msg.contains("without an associated process"));
        assert!(msg.contains("gru stop M002"));
    }

    #[test]
    fn test_resume_error_is_downcastable() {
        let err: anyhow::Error = ResumeError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        }
        .into();
        assert!(err.downcast_ref::<ResumeError>().is_some());
    }
}
