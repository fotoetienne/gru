use crate::github::GitHubClient;
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

/// Attempts to mark an issue as blocked (fire-and-forget).
/// Logs success/failure but does not propagate errors.
pub(super) async fn try_mark_issue_blocked(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    issue_num: u64,
) {
    match client.mark_issue_blocked(owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {}", e);
        }
    }
}

/// Posts a progress comment to the issue (fire-and-forget).
pub(super) async fn try_post_progress_comment(
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
