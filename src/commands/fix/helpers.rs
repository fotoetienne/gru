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
/// Logs success/failure but does not propagate errors.
pub(super) async fn try_mark_issue_blocked(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::mark_issue_blocked_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {}", e);
        }
    }
}

/// Attempts to mark an issue as failed via CLI (fire-and-forget).
/// Logs success/failure but does not propagate errors.
pub(super) async fn try_mark_issue_failed(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::mark_issue_failed_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::FAILED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {}", e);
        }
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
            log::warn!("⚠️  Failed to post progress comment: {}", e);
            false
        }
    }
}
