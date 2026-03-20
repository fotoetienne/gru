use super::helpers::{try_mark_issue_blocked, try_mark_issue_failed, update_orchestration_phase};
use super::monitor::{monitor_ci_after_fix, monitor_pr_lifecycle};
use super::pr::handle_pr_creation;
use super::types::{AgentResult, IssueContext, WorktreeContext};
use crate::agent::AgentBackend;
use crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED;
use crate::minion_registry::OrchestrationPhase;
use crate::pr_monitor;
use agent::{resume_agent_session, run_agent_session};
use anyhow::Result;
use tokio::time::Duration;

use super::agent;

/// Runs the agent session phase (Phase 3).
///
/// Returns `Some(AgentResult)` on success, or `Ok(None)` if the phase was
/// already completed (resume skip). Returns an exit code via `Err` on
/// stuck/timeout or other failures that should stop the pipeline.
pub(super) async fn run_agent_phase(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    start_phase: &OrchestrationPhase,
    quiet: bool,
    timeout_opt: Option<&str>,
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
        resume_agent_session(backend, issue_ctx, wt_ctx, quiet, timeout_opt).await
    } else {
        run_agent_session(backend, issue_ctx, wt_ctx, quiet, timeout_opt).await
    };

    match result {
        Ok(result) => {
            if !result.status.success() {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                try_mark_issue_failed(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
            }
            Ok(Some(result))
        }
        Err(e) if crate::agent_runner::is_stuck_or_timeout_error(&e) => {
            update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
            log::error!("🚨 {:#}", e);
            try_mark_issue_blocked(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
                issue_ctx.issue_num,
            )
            .await;
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
/// the `gru:auto-merge` label. Returns the PR number if one was created.
pub(super) async fn create_pr_phase(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    start_phase: &OrchestrationPhase,
    auto_merge: bool,
) -> Result<Option<String>> {
    let pr_number = if *start_phase <= OrchestrationPhase::CreatingPr {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::CreatingPr).await;
        match handle_pr_creation(issue_ctx, wt_ctx).await {
            Ok(pr) => pr,
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
                Ok(pr) => pr,
                Err(e) => {
                    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                    return Err(e);
                }
            }
        }
    };

    // Add gru:auto-merge label if --auto-merge flag was set
    if auto_merge {
        if let Some(ref pr_num) = pr_number {
            if let Err(e) = pr_monitor::ensure_auto_merge_label(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
            )
            .await
            {
                log::warn!("⚠️  Failed to ensure gru:auto-merge label: {}", e);
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
                Err(e) => log::warn!("⚠️  Failed to add gru:auto-merge label: {}", e),
            }
        }
    }

    Ok(pr_number)
}

/// Runs the PR monitoring phase (Phase 5).
///
/// Monitors the PR lifecycle (reviews, CI, merge state) or falls back to
/// standalone CI monitoring when no PR exists. Returns the suggested exit code.
pub(super) async fn monitor_pr_phase(
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
        monitor_pr_lifecycle(
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
    }

    // CI monitoring — only when PR creation failed (no PR number), since
    // monitor_pr_lifecycle handles CI internally when a PR exists.
    if pr_number.is_none() {
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
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
                return Err(anyhow::anyhow!("CI checks failed"));
            }
            Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {}", e),
        }
    }

    Ok(())
}

/// Computes the agent exit code from an optional `AgentResult`.
pub(super) fn agent_exit_code(agent_result: &Option<AgentResult>) -> i32 {
    agent_result
        .as_ref()
        .map(|r| r.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
        .unwrap_or(0)
}
