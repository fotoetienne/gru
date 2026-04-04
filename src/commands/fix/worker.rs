use super::agent::{resume_agent_session, run_agent_session};
use super::helpers::{try_mark_issue_blocked, try_mark_issue_failed, update_orchestration_phase};
use super::monitor::{monitor_ci_after_fix, monitor_pr_lifecycle};
use super::pr::handle_pr_creation;
use super::types::{AgentResult, IssueContext, WorktreeContext};
use crate::agent::AgentBackend;
use crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED;
use crate::minion_registry::OrchestrationPhase;
use crate::pr_monitor;
use anyhow::Result;
use tokio::time::Duration;

/// Runs the agent session phase (Phase 3).
///
/// Returns `Ok(Some(AgentResult))` when the agent ran (the result may indicate
/// a non-zero exit — callers must check `result.status.success()`).
/// Returns `Ok(None)` if the phase was already completed (resume skip).
/// Returns `Err` on stuck/timeout or other failures that should stop the pipeline.
///
/// If `resume_prompt` is provided, it overrides the default continuation prompt
/// when resuming an interrupted session.
pub(crate) async fn run_agent_phase(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    start_phase: &OrchestrationPhase,
    quiet: bool,
    timeout_opt: Option<&str>,
    resume_prompt: Option<&str>,
) -> Result<Option<AgentResult>> {
    if *start_phase > OrchestrationPhase::RunningAgent {
        println!("⏭️  Skipping agent session (already completed)");
        return Ok(None);
    }

    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::RunningAgent).await;

    // Use --resume only if the agent has already run (session ID was used).
    // If interrupted during Setup, the session was never started.
    let use_resume = *start_phase > OrchestrationPhase::Setup;
    let result = if use_resume {
        resume_agent_session(
            backend,
            issue_ctx,
            wt_ctx,
            quiet,
            timeout_opt,
            resume_prompt,
        )
        .await
    } else {
        run_agent_session(backend, issue_ctx, wt_ctx, quiet, timeout_opt).await
    };

    match result {
        Ok(result) => {
            if !result.status.success() {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                if let Some(issue_num) = issue_ctx.issue_num {
                    try_mark_issue_failed(
                        &issue_ctx.host,
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        issue_num,
                    )
                    .await;
                }
            }
            Ok(Some(result))
        }
        Err(e) if crate::agent_runner::is_stuck_or_timeout_error(&e) => {
            update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
            log::error!("🚨 {:#}", e);
            if let Some(issue_num) = issue_ctx.issue_num {
                let description = e
                    .downcast_ref::<crate::agent_runner::AgentRunnerError>()
                    .map(|err| err.to_string())
                    .unwrap_or_else(|| "stopped responding".to_string());
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_num,
                    &format!(
                        "Minion `{}` stopped: {}. Human intervention required.",
                        wt_ctx.minion_id, description
                    ),
                )
                .await;
            }
            Err(e)
        }
        Err(e) => {
            update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
            Err(e)
        }
    }
}

/// Runs the PR creation phase (Phase 4).
///
/// Creates the PR (or looks it up if already created) and optionally applies
/// the `gru:auto-merge` label when either the CLI flag is set or the source
/// issue carries the label. Returns the PR number if one was created.
pub(crate) async fn create_pr_phase(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    start_phase: &OrchestrationPhase,
    auto_merge: bool,
) -> Result<Option<String>> {
    let pr_number = if *start_phase <= OrchestrationPhase::CreatingPr {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::CreatingPr).await;
        match handle_pr_creation(issue_ctx, wt_ctx).await {
            Ok(Some(pr)) => Some(pr),
            Ok(None) => {
                // PR creation was attempted but no PR was created (e.g., branch not
                // pushed, creation failed with no recovery). Keep the phase at
                // CreatingPr so that `gru resume` can retry.
                log::warn!(
                    "⚠️  PR creation did not produce a PR — phase stays at CreatingPr for retry"
                );
                return Err(anyhow::anyhow!(
                    "PR creation failed: no PR was created for branch '{}'. \
                     Use `gru resume` to retry.",
                    wt_ctx.branch_name
                ));
            }
            Err(e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        }
    } else {
        println!("⏭️  Skipping PR creation (already completed)");
        let minion_id_owned = wt_ctx.minion_id.clone();
        let existing_pr = match crate::minion_registry::with_registry(move |registry| {
            Ok(registry
                .get(&minion_id_owned)
                .and_then(|info| info.pr.clone()))
        })
        .await
        {
            Ok(pr) => pr,
            Err(e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        };

        if existing_pr.is_some() {
            existing_pr
        } else {
            log::info!("ℹ️  PR not found in registry, retrying PR creation");
            match handle_pr_creation(issue_ctx, wt_ctx).await {
                Ok(Some(pr)) => Some(pr),
                Ok(None) => {
                    log::warn!("⚠️  PR retry did not produce a PR — reverting phase to CreatingPr");
                    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::CreatingPr)
                        .await;
                    return Err(anyhow::anyhow!(
                        "PR creation failed: no PR was created for branch '{}'. \
                         Use `gru resume` to retry.",
                        wt_ctx.branch_name
                    ));
                }
                Err(e) => {
                    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                    return Err(e);
                }
            }
        }
    };

    // Add gru:auto-merge label if --auto-merge flag was set or issue has the label
    let issue_has_auto_merge = issue_ctx
        .details
        .as_ref()
        .map(|d| crate::labels::has_label(&d.labels, crate::labels::AUTO_MERGE))
        .unwrap_or(false);
    if auto_merge || issue_has_auto_merge {
        if !auto_merge && issue_has_auto_merge {
            println!(
                "🏷️  Issue #{} has gru:auto-merge label — propagating to PR",
                issue_ctx
                    .issue_num
                    .map_or("?".to_string(), |n| n.to_string())
            );
        }
        if let Some(ref pr_num) = pr_number {
            if let Err(e) = pr_monitor::ensure_auto_merge_label(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
            )
            .await
            {
                log::warn!("⚠️  Failed to ensure gru:auto-merge label: {:#}", e);
            }
            match pr_monitor::add_auto_merge_label(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
                pr_num,
            )
            .await
            {
                Ok(()) => println!("🏷️  Added gru:auto-merge label to PR #{}", pr_num),
                Err(e) => log::warn!("⚠️  Failed to add gru:auto-merge label: {:#}", e),
            }
        }
    }

    Ok(pr_number)
}

/// Runs the PR monitoring phase (Phase 5).
///
/// Monitors the PR lifecycle (reviews, CI, merge state) or falls back to
/// standalone CI monitoring when no PR exists. Returns `Ok(())` on normal
/// completion and `Err` only for failures (for example, CI-failure fallback
/// when no PR exists).
pub(crate) async fn monitor_pr_phase(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &Option<String>,
    timeout_opt: Option<&str>,
    review_timeout: Option<Duration>,
    monitor_timeout: Duration,
) -> Result<()> {
    if let Some(ref pr_num) = pr_number {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::MonitoringPr).await;
        let _ = monitor_pr_lifecycle(
            backend,
            issue_ctx,
            wt_ctx,
            pr_num,
            timeout_opt,
            review_timeout,
            monitor_timeout,
        )
        .await;
    } else {
        log::warn!(
            "⚠️  No PR number available — skipping PR lifecycle monitoring. \
             Branch may not have been pushed, or PR lookup failed."
        );

        // CI monitoring fallback — only when no PR exists, since
        // monitor_pr_lifecycle handles CI internally when a PR is present.
        let ci_passed = monitor_ci_after_fix(
            backend,
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
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                if let Some(issue_num) = issue_ctx.issue_num {
                    try_mark_issue_blocked(
                        &issue_ctx.host,
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        issue_num,
                        "CI checks failed and could not be auto-fixed. Human intervention required.",
                    )
                    .await;
                }
                return Err(anyhow::anyhow!("CI checks failed"));
            }
            Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {:#}", e),
        }
    }

    Ok(())
}

/// Computes the agent exit code from an optional `AgentResult`.
pub(crate) fn agent_exit_code(agent_result: &Option<AgentResult>) -> i32 {
    agent_result
        .as_ref()
        .map(|r| r.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        // On Unix, the raw wait status encodes the exit code in the high byte.
        ExitStatusExt::from_raw((code & 0xff) << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatusExt::from_raw(code as u32)
    }

    #[test]
    fn test_agent_exit_code_none_returns_zero() {
        assert_eq!(agent_exit_code(&None), 0);
    }

    #[test]
    fn test_agent_exit_code_success() {
        let result = AgentResult {
            status: exit_status(0),
        };
        assert_eq!(agent_exit_code(&Some(result)), 0);
    }

    #[test]
    fn test_agent_exit_code_failure() {
        let result = AgentResult {
            status: exit_status(1),
        };
        assert_eq!(agent_exit_code(&Some(result)), 1);
    }

    #[test]
    fn test_agent_exit_code_custom_code() {
        let result = AgentResult {
            status: exit_status(42),
        };
        assert_eq!(agent_exit_code(&Some(result)), 42);
    }
}
