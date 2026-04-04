use crate::minion_registry::{with_registry, OrchestrationPhase};

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

/// Posts a comment on an issue and attempts to mark it as blocked via CLI (fire-and-forget).
/// The comment is posted before the label; the label is still applied even if the comment fails.
pub(crate) async fn try_mark_issue_blocked(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    reason: &str,
) {
    if let Err(e) = crate::github::post_comment_via_cli(host, owner, repo, issue_num, reason).await
    {
        log::warn!("⚠️  Failed to post blocked comment on issue: {}", e);
    }
    match crate::github::mark_issue_blocked_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {:#}", e);
        }
    }
}

/// Removes `gru:blocked` from the PR and restores `gru:in-progress` on the issue.
/// Fire-and-forget: logs on failure but does not propagate errors.
/// Idempotent: safe to call even if the label is not present.
pub(crate) async fn try_remove_blocked_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    issue_num: u64,
) {
    match crate::github::remove_blocked_label(host, owner, repo, pr_number, issue_num).await {
        Ok(()) => {
            println!("🏷️  Removed '{}' label", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to remove blocked label: {:#}", e);
        }
    }
}

/// Attempts to mark an issue as failed via CLI (fire-and-forget).
/// Logs success/failure but does not propagate errors.
pub(crate) async fn try_mark_issue_failed(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::mark_issue_failed_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::FAILED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {:#}", e);
        }
    }
}

/// Attempts to unclaim an issue by restoring `gru:todo` and removing `gru:in-progress`.
/// Fire-and-forget: logs on failure but does not propagate errors.
/// Used when `--discuss` abort needs to reverse a `claim_issue` call.
pub(super) async fn try_unclaim_issue(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::edit_labels_via_cli(
        host,
        owner,
        repo,
        issue_num,
        &[crate::labels::TODO],
        &[crate::labels::IN_PROGRESS],
    )
    .await
    {
        Ok(()) => {
            println!(
                "🏷️  Restored '{}' label on issue #{}",
                crate::labels::TODO,
                issue_num
            );
        }
        Err(e) => {
            log::warn!("⚠️  Failed to unclaim issue #{}: {}", issue_num, e);
        }
    }
}

/// Posts an explanatory comment on an issue (fire-and-forget).
/// Logs a warning if posting fails but does not propagate the error.
pub(crate) async fn try_post_issue_comment(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) {
    if let Err(e) = crate::github::post_comment_via_cli(host, owner, repo, issue_num, body).await {
        log::warn!("⚠️  Failed to post comment on issue: {}", e);
    }
}

/// Posts a progress comment to the issue via CLI (fire-and-forget).
pub(super) async fn try_post_progress_comment(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) -> bool {
    match crate::github::post_comment_via_cli(host, owner, repo, issue_num, body).await {
        Ok(()) => true,
        Err(e) => {
            log::warn!("⚠️  Failed to post progress comment: {:#}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    // These tests verify the exact message strings produced at each blocked/escalation call site.
    // They are format-string snapshot tests, not behavioral tests of the async helpers.
    use crate::agent_runner::AgentRunnerError;
    use std::time::Duration;

    #[test]
    fn test_blocked_reason_inactivity_stuck() {
        let minion_id = "M042";
        let err = AgentRunnerError::InactivityStuck { minutes: 15 };
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: No activity for 15 minutes - task appears stuck. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_stream_timeout() {
        let minion_id = "M042";
        let err = AgentRunnerError::StreamTimeout { seconds: 300 };
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: Timeout: agent process hasn't produced output in 300 seconds. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_max_timeout() {
        let minion_id = "M042";
        let err = AgentRunnerError::MaxTimeout(Duration::from_secs(600));
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: Task exceeded maximum timeout of 600s. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_ci_exhausted() {
        let pr_number = "123";
        let reason = format!(
            "CI auto-fix failed after {} attempts. See PR #{} for details. Human intervention required.",
            crate::ci::MAX_CI_FIX_ATTEMPTS,
            pr_number
        );
        assert_eq!(
            reason,
            format!(
                "CI auto-fix failed after {} attempts. See PR #123 for details. Human intervention required.",
                crate::ci::MAX_CI_FIX_ATTEMPTS
            )
        );
    }

    #[test]
    fn test_blocked_reason_judge_escalated() {
        let pr_number = "456";
        let reason = format!(
            "Merge judge escalated PR #{} for human review. See PR for details.",
            pr_number
        );
        assert_eq!(
            reason,
            "Merge judge escalated PR #456 for human review. See PR for details."
        );
    }
}
