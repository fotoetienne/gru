use super::types::{ExistingMinionCheck, IssueContext, IssueDetails};
use crate::minion_registry::{is_process_alive, with_registry, MinionMode};
use crate::url_utils::parse_issue_info;
use anyhow::{Context, Result};

/// Resolves an issue argument into validated context.
///
/// Parses the issue string and fetches issue details via gh CLI.
/// Note: Does NOT claim the issue — that happens after worktree setup to avoid
/// marking issues as in-progress when setup fails.
/// Note: Existing minion check is done in `handle_fix` to support resume logic.
pub(crate) async fn resolve_issue(issue: &str, github_hosts: &[String]) -> Result<IssueContext> {
    let (owner, repo, issue_num_str, host) = parse_issue_info(issue, github_hosts).await?;
    let issue_num: u64 = issue_num_str
        .parse()
        .context("Failed to parse issue number")?;

    // Fetch issue details via CLI
    let details = fetch_issue_details(&owner, &repo, &host, issue_num).await;

    Ok(IssueContext {
        owner,
        repo,
        host,
        issue_num,
        details,
    })
}

/// Checks if there are existing minions working on this issue.
///
/// Returns `Resumable` when a stopped minion is found (for auto-resume),
/// `AlreadyRunning` when a running minion is found (with suggestions printed),
/// or `None` when no minions exist.
pub(super) async fn check_existing_minions(
    owner: &str,
    repo: &str,
    issue_num: u64,
) -> Result<ExistingMinionCheck> {
    let repo_for_check = format!("{}/{}", owner, repo);
    let mut existing =
        with_registry(move |registry| Ok(registry.find_by_issue(&repo_for_check, issue_num)))
            .await?;

    if existing.is_empty() {
        return Ok(ExistingMinionCheck::None);
    }

    // Sort deterministically: running Minions first, then by most recent start time.
    existing.sort_by(|(_, a), (_, b)| {
        let a_running = a.pid.map(is_process_alive).unwrap_or(false);
        let b_running = b.pid.map(is_process_alive).unwrap_or(false);
        b_running
            .cmp(&a_running)
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });

    // Check if any minion is actually running
    let any_running = existing
        .iter()
        .any(|(_, info)| info.pid.map(is_process_alive).unwrap_or(false));

    if any_running {
        // A minion is actively running - show error with suggestions
        eprintln!(
            "Error: {} existing Minion(s) found for issue {}:\n",
            existing.len(),
            issue_num
        );

        for (minion_id, info) in &existing {
            let actually_running = info.pid.map(is_process_alive).unwrap_or(false);
            let status_msg = if actually_running {
                match info.mode {
                    MinionMode::Autonomous => "running (autonomous)",
                    MinionMode::Interactive => "running (interactive)",
                    MinionMode::Stopped => "running",
                }
            } else {
                "stopped"
            };
            eprintln!("  {} - status: {}", minion_id, status_msg);
        }

        let (best_id, _) = existing.first().unwrap();
        eprintln!("\nOptions:");
        eprintln!("  - Attach interactively: gru attach {}", best_id);
        eprintln!(
            "  - Create new session:   gru do https://github.com/{}/{}/issues/{} --force-new",
            owner, repo, issue_num
        );

        return Ok(ExistingMinionCheck::AlreadyRunning);
    }

    // All minions are stopped - find the best candidate for resume.
    // Look for one that isn't in a terminal state and whose worktree still exists.
    let resumable = existing
        .iter()
        .find(|(_, info)| !info.orchestration_phase.is_terminal() && info.worktree.exists());

    if let Some((minion_id, info)) = resumable {
        return Ok(ExistingMinionCheck::Resumable(
            minion_id.clone(),
            Box::new(info.clone()),
        ));
    }

    // All existing minions are in terminal states (Failed/Completed) — allow a
    // fresh attempt. This lets Lab automatically retry failed issues without
    // requiring --force-new, while still blocking when a non-terminal minion exists.
    Ok(ExistingMinionCheck::None)
}

/// Claims an issue by adding the in-progress label via CLI.
///
/// Fetches current labels first to detect race conditions (another minion
/// already claimed the issue). Returns silently on errors since claiming
/// is fire-and-forget.
pub(super) async fn claim_issue(host: &str, owner: &str, repo: &str, issue_num: u64) {
    // Check current labels to detect race conditions
    match crate::github::get_issue_via_cli(owner, repo, host, issue_num).await {
        Ok(info) => {
            let has_in_progress = info
                .labels
                .iter()
                .any(|l| l.name == crate::labels::IN_PROGRESS);
            if has_in_progress {
                log::warn!(
                    "⚠️  Issue #{} is already claimed by another Minion",
                    issue_num
                );
                log::warn!("   This may indicate a race condition or multiple gru instances.");
                log::warn!("   Continuing anyway; will proceed to create or reuse a worktree...");
                return;
            }
        }
        Err(e) => {
            log::warn!("⚠️  Failed to check issue labels: {}", e);
            log::warn!("   Proceeding with claim attempt anyway...");
        }
    }

    match crate::github::claim_issue_via_cli(host, owner, repo, issue_num, crate::labels::TODO)
        .await
    {
        Ok(()) => {
            println!(
                "🏷️  Added '{}' label to issue #{}",
                crate::labels::IN_PROGRESS,
                issue_num
            );
        }
        Err(e) => {
            log::warn!("⚠️  Failed to add label to issue: {}", e);
            log::warn!("   Continuing anyway...");
        }
    }
}

/// Fetches issue details from GitHub using the gh CLI.
async fn fetch_issue_details(
    owner: &str,
    repo: &str,
    host: &str,
    issue_num: u64,
) -> Option<IssueDetails> {
    match crate::github::get_issue_via_cli(owner, repo, host, issue_num).await {
        Ok(info) => {
            let labels = info
                .labels
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Some(IssueDetails {
                title: info.title,
                body: info.body.unwrap_or_default(),
                labels,
            })
        }
        Err(e) => {
            eprintln!("⚠️  Failed to fetch issue details via CLI: {}", e);
            eprintln!("   Fix authentication with: gh auth login");
            None
        }
    }
}
