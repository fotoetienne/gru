use super::types::{ExistingMinionCheck, IssueContext, IssueDetails};
use crate::config::HostRegistry;
use crate::minion_registry::{with_registry, MinionInfo, MinionMode};
use crate::url_utils::parse_issue_info;
use anyhow::{Context, Result};

/// Resolves an issue argument into validated context.
///
/// Parses the issue string and fetches issue details via gh CLI.
/// Note: Does NOT claim the issue — that happens after worktree setup to avoid
/// marking issues as in-progress when setup fails.
/// Note: Existing minion check is done in `handle_fix` to support resume logic.
pub(crate) async fn resolve_issue(
    issue: &str,
    host_registry: &HostRegistry,
) -> Result<IssueContext> {
    let (owner, repo, issue_num_str, host) = parse_issue_info(issue, host_registry).await?;
    let issue_num: u64 = issue_num_str
        .parse()
        .context("Failed to parse issue number")?;

    // Fetch issue details via CLI
    let details = fetch_issue_details(&owner, &repo, &host, issue_num).await;

    Ok(IssueContext {
        owner,
        repo,
        host,
        issue_num: Some(issue_num),
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
    issue_num: Option<u64>,
) -> Result<ExistingMinionCheck> {
    let Some(issue_num) = issue_num else {
        return Ok(ExistingMinionCheck::None);
    };
    let repo_for_check = crate::github::repo_slug(owner, repo);
    let existing =
        with_registry(move |registry| Ok(registry.find_by_issue(&repo_for_check, issue_num)))
            .await?;

    Ok(evaluate_existing_minions(owner, repo, issue_num, existing))
}

/// Pure core of [`check_existing_minions`], extracted so it can be unit-tested
/// without a live registry.
fn evaluate_existing_minions(
    owner: &str,
    repo: &str,
    issue_num: u64,
    mut existing: Vec<(String, MinionInfo)>,
) -> ExistingMinionCheck {
    if existing.is_empty() {
        return ExistingMinionCheck::None;
    }

    // On macOS and Linux, `get_process_start_time` returns `Some`, so we can
    // validate that the live PID actually belongs to the recorded process.
    // On other platforms the start time is always `None`, so we fall back to
    // the plain `is_running()` check to avoid ignoring all running minions.
    //
    // Terminal entries (Failed/Completed) are intentionally excluded: lab's
    // try_spawn_for_issue stamps the new gru-do child PID onto all existing
    // registry entries for the issue (including terminal ones) to close a
    // race window where gru-do creates its entry after lab reads the pid.
    // Without this guard a Failed minion would appear "AlreadyRunning" because
    // the gru-do parent's live PID is sitting on it, causing EXIT_ALREADY_RUNNING
    // and a thrash loop (issue #879).
    let is_validated_running = |info: &MinionInfo| {
        !info.orchestration_phase.is_terminal()
            && info.is_running()
            && (info.pid_start_time.is_some()
                || !cfg!(any(target_os = "macos", target_os = "linux")))
    };

    // Sort deterministically: validated-running Minions first, then by most recent
    // start time. Use the same predicate as `any_running` below so the sort order
    // is consistent with what triggers the AlreadyRunning path.
    existing.sort_by(|(_, a), (_, b)| {
        is_validated_running(b)
            .cmp(&is_validated_running(a))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });

    // Check if any minion is actually running.
    let any_running = existing.iter().any(|(_, info)| is_validated_running(info));

    if any_running {
        // Only show non-terminal entries in the error output — terminal ones
        // were excluded from any_running and would just confuse the operator.
        let non_terminal: Vec<_> = existing
            .iter()
            .filter(|(_, info)| !info.orchestration_phase.is_terminal())
            .collect();
        eprintln!(
            "Error: {} existing Minion(s) found for issue {}:\n",
            non_terminal.len(),
            issue_num
        );

        for (minion_id, info) in &non_terminal {
            let actually_running = info.is_running();
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

        let (best_id, _) = non_terminal.first().unwrap();
        eprintln!("\nOptions:");
        eprintln!("  - Attach interactively: gru attach {}", best_id);
        eprintln!(
            "  - Create new session:   gru do https://github.com/{}/{}/issues/{} --force-new",
            owner, repo, issue_num
        );

        return ExistingMinionCheck::AlreadyRunning;
    }

    // All minions are stopped - find the best candidate for resume.
    // Look for one that isn't in a terminal state and whose worktree still exists.
    // Note: Setup-phase minions without a live process may match here if their
    // worktree exists (since Setup is not terminal). In practice, stale Setup
    // entries rarely have worktrees — Lab's prune_stale_entries cleans them up.
    let resumable = existing
        .iter()
        .find(|(_, info)| !info.orchestration_phase.is_terminal() && info.worktree.exists());

    if let Some((minion_id, info)) = resumable {
        return ExistingMinionCheck::Resumable(minion_id.clone(), Box::new(info.clone()));
    }

    // No running and no resumable minions — allow a fresh attempt. This covers
    // both all-terminal minions (Failed/Completed) and non-terminal ones whose
    // worktrees no longer exist. Lab can automatically retry without --force-new.
    ExistingMinionCheck::None
}

/// Claims an issue by adding the in-progress label via CLI.
///
/// Fetches current labels first to detect race conditions (another minion
/// already claimed the issue). Returns silently on errors since claiming
/// is fire-and-forget.
pub(super) async fn claim_issue(host: &str, owner: &str, repo: &str, issue_num: Option<u64>) {
    let Some(issue_num) = issue_num else {
        return;
    };
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
            log::warn!("⚠️  Failed to check issue labels: {:#}", e);
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
            log::warn!("⚠️  Failed to add label to issue: {:#}", e);
            log::warn!("   Continuing anyway...");
        }
    }
}

/// Fetches issue details from GitHub using the gh CLI.
pub(crate) async fn fetch_issue_details(
    owner: &str,
    repo: &str,
    host: &str,
    issue_num: u64,
) -> Option<IssueDetails> {
    match crate::github::get_issue_via_cli(owner, repo, host, issue_num).await {
        Ok(info) => {
            let labels = info.labels.into_iter().map(|l| l.name).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minion_registry::{get_process_start_time, MinionMode, OrchestrationPhase};
    use chrono::Utc;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn make_info(phase: OrchestrationPhase, pid: Option<u32>, worktree: PathBuf) -> MinionInfo {
        let now = Utc::now();
        MinionInfo {
            repo: "owner/repo".to_string(),
            issue: Some(329),
            command: "do".to_string(),
            prompt: "/do 329".to_string(),
            started_at: now,
            branch: "minion/issue-329-M1ku".to_string(),
            worktree,
            status: "active".to_string(),
            pr: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            pid,
            pid_start_time: pid.and_then(get_process_start_time),
            mode: if pid.is_some() {
                MinionMode::Autonomous
            } else {
                MinionMode::Stopped
            },
            last_activity: now,
            orchestration_phase: phase,
            token_usage: None,
            agent_name: "claude".to_string(),
            timeout_deadline: None,
            attempt_count: 1,
            no_watch: false,
            last_review_check_time: None,
            wake_reason: None,
            archived_at: None,
            pending_review_sha: None,
        }
    }

    /// A Failed minion with a live PID (from lab's spurious PID stamp) must not
    /// block a fresh start. Without the is_terminal() guard this would return
    /// AlreadyRunning, causing EXIT_ALREADY_RUNNING and a thrash loop (#879).
    #[test]
    fn failed_minion_with_live_pid_returns_none() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        // Simulate lab stamping the new gru-do child PID onto the terminal entry.
        let live_pid = std::process::id();
        let info = make_info(OrchestrationPhase::Failed, Some(live_pid), worktree);

        let result = evaluate_existing_minions("owner", "repo", 329, vec![("M1ku".into(), info)]);

        assert!(
            matches!(result, ExistingMinionCheck::None),
            "Failed minion with live PID must not block a fresh start, got: {:?}",
            result
        );
    }

    /// A Failed minion with no PID and an existing worktree must also return None
    /// (not Resumable), so a fresh minion ID is allocated.
    #[test]
    fn failed_minion_no_pid_with_worktree_returns_none() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        let info = make_info(OrchestrationPhase::Failed, None, worktree);

        let result = evaluate_existing_minions("owner", "repo", 329, vec![("M1ku".into(), info)]);

        assert!(
            matches!(result, ExistingMinionCheck::None),
            "Failed minion must not be returned as Resumable, got: {:?}",
            result
        );
    }

    /// A Completed minion with a live PID must also be excluded from AlreadyRunning.
    #[test]
    fn completed_minion_with_live_pid_returns_none() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        let live_pid = std::process::id();
        let info = make_info(OrchestrationPhase::Completed, Some(live_pid), worktree);

        let result = evaluate_existing_minions("owner", "repo", 329, vec![("M1ku".into(), info)]);

        assert!(
            matches!(result, ExistingMinionCheck::None),
            "Completed minion with live PID must not block a fresh start, got: {:?}",
            result
        );
    }

    /// A non-terminal minion with a live PID and existing worktree must still
    /// report AlreadyRunning (unchanged behavior for genuinely active minions).
    #[test]
    fn active_minion_with_live_pid_returns_already_running() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        let live_pid = std::process::id();
        let info = make_info(OrchestrationPhase::RunningAgent, Some(live_pid), worktree);

        let result = evaluate_existing_minions("owner", "repo", 329, vec![("M1ku".into(), info)]);

        assert!(
            matches!(result, ExistingMinionCheck::AlreadyRunning),
            "Active minion with live PID must be detected as AlreadyRunning, got: {:?}",
            result
        );
    }

    /// A non-terminal stopped minion with an existing worktree must be Resumable.
    #[test]
    fn stopped_non_terminal_minion_with_worktree_is_resumable() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        let info = make_info(OrchestrationPhase::RunningAgent, None, worktree);

        let result = evaluate_existing_minions("owner", "repo", 329, vec![("M1ku".into(), info)]);

        assert!(
            matches!(result, ExistingMinionCheck::Resumable(ref id, _) if id == "M1ku"),
            "Stopped non-terminal minion with worktree must be Resumable, got: {:?}",
            result
        );
    }

    /// A Failed minion with live PID alongside a genuinely active non-terminal
    /// minion must still surface AlreadyRunning — the terminal entry must not
    /// suppress detection of the real running minion.
    #[test]
    fn active_minion_not_suppressed_by_failed_sibling_with_live_pid() {
        let tmp = tempdir().unwrap();
        let worktree = tmp.path().to_path_buf();
        let live_pid = std::process::id();
        let failed = make_info(OrchestrationPhase::Failed, Some(live_pid), worktree.clone());
        let active = make_info(OrchestrationPhase::RunningAgent, Some(live_pid), worktree);
        let entries = vec![("M1ku".into(), failed), ("M1lj".into(), active)];

        let result = evaluate_existing_minions("owner", "repo", 329, entries);

        assert!(
            matches!(result, ExistingMinionCheck::AlreadyRunning),
            "Active non-terminal sibling must still be detected as AlreadyRunning, got: {:?}",
            result
        );
    }
}
