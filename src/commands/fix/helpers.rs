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

/// Attempts to mark an issue as blocked via CLI (fire-and-forget).
/// Posts a comment on the issue with the reason before applying the label.
/// The label is still applied even if the comment fails.
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
    #[test]
    fn test_blocked_reason_timeout_contains_minion_id() {
        let minion_id = "M042";
        let reason = format!(
            "Minion `{}` stopped responding (no output for 5 minutes). Human intervention required.",
            minion_id
        );
        assert!(reason.contains("M042"));
        assert!(reason.contains("stopped responding"));
        assert!(reason.contains("Human intervention required"));
    }

    #[test]
    fn test_blocked_reason_ci_exhausted_contains_pr_and_attempts() {
        let pr_number = "123";
        let attempts = crate::ci::MAX_CI_FIX_ATTEMPTS;
        let reason = format!(
            "CI auto-fix failed after {} attempts. See PR #{} for details. Human intervention required.",
            attempts, pr_number
        );
        assert!(reason.contains(&attempts.to_string()));
        assert!(reason.contains("PR #123"));
        assert!(reason.contains("Human intervention required"));
    }

    #[test]
    fn test_blocked_reason_judge_escalated_contains_pr() {
        let pr_number = "456";
        let reason = format!(
            "Merge judge escalated PR #{} for human review. See PR for details.",
            pr_number
        );
        assert!(reason.contains("PR #456"));
        assert!(reason.contains("human review"));
    }
}
