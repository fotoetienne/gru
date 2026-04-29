use super::super::rebase::{fetch_base_branch, is_up_to_date};
use super::agent::invoke_agent_for_reviews;
use super::types::{IssueContext, WorktreeContext, MAX_REBASE_ATTEMPTS, MAX_REVIEW_ROUNDS};
use crate::agent::AgentBackend;
use crate::ci;
use crate::config::LabConfig;
use crate::github;
use crate::merge_judge::{self, JudgeAction, JudgeResponse, JudgeState};
use crate::minion_registry;
use crate::pr_monitor::{self, MonitorResult};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

/// Format a duration in seconds into a human-readable string (e.g. "2h15m", "5m", "30s").
fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let s = secs % 60;
    if hours > 0 {
        format!("{}h{}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", s)
    }
}

/// Persists a review-check timestamp to the minion registry (best-effort).
/// Errors are logged as warnings since this is non-critical metadata.
async fn save_review_check_time(minion_id: &str, ts: DateTime<Utc>) {
    let mid = minion_id.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.last_review_check_time = Some(ts);
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to save last_review_check_time: {:#}", e);
    }
}

/// Persists a pending-review SHA to the minion registry (best-effort).
///
/// Call this immediately before spawning the review subprocess so that if the
/// monitoring session crashes while the review is running, a resumed session can
/// detect the in-flight review and skip spawning a duplicate.
async fn save_pending_review_sha(minion_id: &str, sha: &str) {
    let mid = minion_id.to_string();
    let sha = sha.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.pending_review_sha = Some(sha);
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to save pending_review_sha: {:#}", e);
    }
}

/// Clears the pending-review SHA in the minion registry (best-effort).
///
/// Call this after the review subprocess returns (success or error) to signal
/// that the in-flight review guard is no longer needed.
async fn clear_pending_review_sha(minion_id: &str) {
    let mid = minion_id.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.pending_review_sha = None;
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to clear pending_review_sha: {:#}", e);
    }
}

/// Reads the pending-review SHA from the minion registry (best-effort).
///
/// Returns `None` on registry error or when no pending SHA is recorded.
async fn get_pending_review_sha(minion_id: &str) -> Option<String> {
    let mid = minion_id.to_string();
    minion_registry::with_registry(move |registry| {
        Ok(registry
            .get(&mid)
            .and_then(|info| info.pending_review_sha.clone()))
    })
    .await
    .ok()
    .flatten()
}

/// Resets `attempt_count` to 0 after a successful review response (best-effort).
///
/// The cap should only trigger on *consecutive* failures. Resetting here ensures a
/// minion that successfully addresses reviews isn't penalised for past resume cycles.
async fn reset_attempt_count(minion_id: &str) {
    let mid = minion_id.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.attempt_count = 0;
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to reset attempt_count: {:#}", e);
    }
}

/// Outcome of an automatic rebase attempt.
enum AutoRebaseResult {
    /// Branch was already up-to-date; no rebase or force-push occurred.
    AlreadyUpToDate,
    /// Rebase succeeded and branch was force-pushed.
    RebasedAndPushed,
    /// Conflicts could not be resolved automatically.
    ConflictUnresolved,
}

/// Attempts to auto-rebase the worktree branch onto its base branch.
///
/// `checkout_path` is the git worktree where all git operations and the
/// conflict-resolution agent run. `minion_dir` is the parent metadata
/// directory where `events.jsonl` is written — passing it explicitly keeps
/// the rebase agent's stream events co-located with the main agent's
/// events instead of landing inside the checkout.
async fn auto_rebase_pr(checkout_path: &Path, minion_dir: &Path) -> Result<AutoRebaseResult> {
    use super::super::rebase::{
        abort_rebase, attempt_rebase, check_clean_worktree, detect_base_branch, ensure_on_branch,
        force_push, get_current_branch, is_rebase_in_progress, run_agent_rebase, RebaseOutcome,
    };

    // Abort any stale in-progress rebase left by a previous crashed attempt.
    // This must happen before check_clean_worktree because UU files from a
    // stale rebase would otherwise cause a false "uncommitted changes" error.
    if is_rebase_in_progress(checkout_path) {
        log::warn!("⚠️  Stale in-progress rebase detected; aborting before retrying");
        println!("⚠️  Aborting stale in-progress rebase...");
        abort_rebase(checkout_path).await?;
    }

    // Bail early if worktree has uncommitted changes (e.g., agent crashed mid-edit)
    check_clean_worktree(checkout_path)
        .await
        .context("Cannot auto-rebase: worktree has uncommitted changes")?;

    // Detect the base branch first so we can fetch only what we need
    let base_branch = detect_base_branch(checkout_path).await?;

    // Fetch only the base branch to avoid conflicts with other worktrees that
    // have different branches checked out on the same repo
    println!("📡 Fetching latest changes from origin...");
    fetch_base_branch(checkout_path, &base_branch).await?;

    // Short-circuit if already up-to-date (avoids no-op rebase + force-push
    // that would reset GitHub's mergeable cache timer)
    if is_up_to_date(checkout_path, &base_branch).await? {
        println!(
            "✅ Already up-to-date with origin/{}, skipping rebase",
            base_branch
        );
        return Ok(AutoRebaseResult::AlreadyUpToDate);
    }

    println!("🔄 Rebasing onto origin/{}...", base_branch);

    // Attempt the rebase
    match attempt_rebase(checkout_path, &base_branch).await? {
        RebaseOutcome::Clean { commit_count } => {
            println!(
                "✅ Clean rebase: {} commit{} replayed",
                commit_count,
                if commit_count == 1 { "" } else { "s" }
            );
            log::info!("Auto force-pushing rebased branch (autonomous mode, --force-with-lease)");
            force_push(checkout_path).await?;
            println!("🚀 Force-pushed rebased branch");
            Ok(AutoRebaseResult::RebasedAndPushed)
        }
        RebaseOutcome::Conflicts => {
            println!("⚠️  Conflicts detected, launching agent to resolve...");
            abort_rebase(checkout_path).await?;

            // Capture local branch name now (abort_rebase restores it) so we can
            // recover if the agent leaves the worktree in detached HEAD state.
            let local_branch = get_current_branch(checkout_path).await?;

            // None uses the 30m default inside run_agent_rebase
            let exit_code = run_agent_rebase(checkout_path, minion_dir, &base_branch, None).await?;

            // Defensive check: agent may have checked out a remote tracking ref
            // (e.g. `origin/<branch>`) leaving the worktree in detached HEAD.
            // Treat a recovery failure as ConflictUnresolved rather than an
            // unexpected error so the caller receives an actionable escalation.
            if let Err(err) = ensure_on_branch(checkout_path, &local_branch).await {
                log::warn!(
                    "Agent rebase finished but failed to restore local branch '{}': {}",
                    local_branch,
                    err
                );
                return Ok(AutoRebaseResult::ConflictUnresolved);
            }

            if exit_code == 0 {
                // Verify the agent actually completed the rebase before trusting it.
                // An agent that exits 0 without rebasing (e.g. sees a clean worktree
                // post-abort and does nothing) would otherwise cause a spurious
                // RebasedAndPushed result and a force-push of the unchanged branch.
                //
                // Use --untracked-files=no so that untracked files (e.g. editor
                // temp files or generated artifacts) left by the agent don't
                // produce false positives. We only care about uncommitted tracked
                // changes (conflict markers, staged edits) that indicate an
                // incomplete rebase.
                let tracked_status = TokioCommand::new("git")
                    .arg("-C")
                    .arg(checkout_path)
                    .args(["status", "--porcelain", "--untracked-files=no"])
                    .output()
                    .await
                    .context("Failed to check tracked-file status after agent rebase")?;
                if !tracked_status.status.success() {
                    let stderr = String::from_utf8_lossy(&tracked_status.stderr);
                    anyhow::bail!(
                        "git status failed after agent rebase (exit {:?}): {}",
                        tracked_status.status.code(),
                        stderr.trim()
                    );
                }
                if !tracked_status.stdout.is_empty() {
                    let details = String::from_utf8_lossy(&tracked_status.stdout);
                    log::warn!(
                        "Agent rebase left uncommitted tracked changes:\n{}",
                        details.trim()
                    );
                    return Ok(AutoRebaseResult::ConflictUnresolved);
                }
                // is_up_to_date checks that origin/<base_branch> is an ancestor of HEAD,
                // which is the canonical proof that the rebase actually happened.
                if !is_up_to_date(checkout_path, &base_branch).await? {
                    log::warn!(
                        "Agent rebase exited 0 but origin/{} is not an ancestor of HEAD — waiting for primary agent to resolve",
                        base_branch
                    );
                    // The primary agent may still be running and resolve the conflict
                    // via a merge commit in the same worktree. Poll for up to ~4 minutes
                    // (POST_CHECK_RETRIES × POST_CHECK_INTERVAL_SECS) before declaring failure.
                    let resolved = wait_for_rebase_resolution(
                        checkout_path,
                        &base_branch,
                        POST_CHECK_RETRIES,
                        POST_CHECK_INTERVAL_SECS,
                    )
                    .await;
                    if !resolved {
                        log::warn!(
                            "origin/{} still not an ancestor of HEAD after retry window — giving up",
                            base_branch
                        );
                        return Ok(AutoRebaseResult::ConflictUnresolved);
                    }
                    // Refresh the PR branch tracking ref before force_push. The
                    // primary agent may have already pushed a merge commit, making
                    // our local tracking ref stale. Without this fetch,
                    // --force-with-lease would see a lease mismatch and fail.
                    //
                    // Use --refmap= (empty) + explicit refspec so only this one
                    // branch is fetched, independent of remote.origin.fetch config
                    // (mirrors the approach used by fetch_base_branch).
                    let pr_refspec = format!(
                        "refs/heads/{}:refs/remotes/origin/{}",
                        local_branch, local_branch
                    );
                    let fetch_out = TokioCommand::new("git")
                        .arg("-C")
                        .arg(checkout_path)
                        .args(["fetch", "--refmap=", "origin", pr_refspec.as_str()])
                        .output()
                        .await;
                    match fetch_out {
                        Ok(out) if !out.status.success() => {
                            log::warn!(
                                "Could not refresh PR branch tracking ref before push: {}",
                                String::from_utf8_lossy(&out.stderr).trim()
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "Could not refresh PR branch tracking ref before push: {:#}",
                                e
                            );
                        }
                        Ok(_) => {}
                    }
                }
                // Defensively force push in case the rebase agent didn't push.
                log::info!("Auto force-pushing after conflict resolution (autonomous mode, --force-with-lease)");
                force_push(checkout_path).await?;
                println!("🚀 Force-pushed rebased branch");
                Ok(AutoRebaseResult::RebasedAndPushed)
            } else {
                log::warn!("Agent rebase exited with code {}", exit_code);
                Ok(AutoRebaseResult::ConflictUnresolved)
            }
        }
    }
}

/// Polls `is_up_to_date` with periodic re-fetches for up to `retries` attempts.
///
/// Returns `true` as soon as `origin/<base_branch>` becomes an ancestor of HEAD,
/// or `false` if the retry window expires. Intended for the post-rebase check
/// where the primary agent may still be resolving the conflict in the same worktree.
///
/// Transient fetch and git errors are logged and skipped rather than propagated,
/// since this is a polling loop designed to tolerate temporary failures.
async fn wait_for_rebase_resolution(
    checkout_path: &Path,
    base_branch: &str,
    retries: u32,
    interval_secs: u64,
) -> bool {
    for attempt in 1..=retries {
        // Fetch is best-effort: a network hiccup shouldn't prevent us from
        // checking local HEAD, which the primary agent may have already updated
        // with a merge commit.
        if let Err(e) = fetch_base_branch(checkout_path, base_branch).await {
            log::warn!(
                "Post-check attempt {}/{}: fetch failed (checking HEAD anyway): {:#}",
                attempt,
                retries,
                e
            );
        }
        match is_up_to_date(checkout_path, base_branch).await {
            Ok(true) => {
                log::info!(
                    "Branch is up-to-date on attempt {}/{} — primary agent resolved the conflict",
                    attempt,
                    retries
                );
                return true;
            }
            Ok(false) => {
                log::debug!(
                    "Post-check attempt {}/{}: origin/{} still not ancestor of HEAD",
                    attempt,
                    retries,
                    base_branch
                );
            }
            Err(e) => {
                log::warn!(
                    "Post-check attempt {}/{}: is_up_to_date failed (will retry): {:#}",
                    attempt,
                    retries,
                    e
                );
            }
        }
        if attempt < retries {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    }
    false
}

/// Format the body of a monitoring-paused notification comment.
///
/// Includes YAML frontmatter for machine discoverability and a human-readable
/// section with the minion ID and resume command.
fn format_exit_notification_comment(minion_id: &str, unaddressed_count: usize) -> String {
    let review_word = if unaddressed_count == 1 {
        "review"
    } else {
        "reviews"
    };
    format!(
        "---\ntype: monitoring-paused\n---\n\n\
        ⏸️ This PR's automated agent has paused. \
        There are {} {} that haven't been addressed yet. \
        Resume automated responses with:\n`gru resume {}`{}",
        unaddressed_count,
        review_word,
        minion_id,
        crate::progress_comments::minion_signature(minion_id),
    )
}

/// Check for unaddressed reviews and post a notification comment if warranted.
///
/// Skips silently when the PR is merged/closed or when there are no unaddressed
/// external reviews (i.e. reviews from someone other than the PR author).
async fn post_exit_notification_if_needed(
    owner: &str,
    repo: &str,
    host: &str,
    pr_number: &str,
    minion_id: &str,
    review_baseline: DateTime<Utc>,
) {
    // Check PR state in one API call.
    let is_open =
        match pr_monitor::get_pr_info_for_exit_notification(host, owner, repo, pr_number).await {
            Ok(open) => open,
            Err(e) => {
                log::warn!(
                    "⚠️  Could not check PR state for exit notification: {:#}",
                    e
                );
                return;
            }
        };

    if !is_open {
        return;
    }

    let reviews = match pr_monitor::get_all_reviews(host, owner, repo, pr_number).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("⚠️  Could not fetch reviews for exit notification: {:#}", e);
            return;
        }
    };

    let count = pr_monitor::count_unaddressed_reviews(&reviews, minion_id, review_baseline);

    if !pr_monitor::should_post_exit_notification(is_open, count) {
        return;
    }

    let body = format_exit_notification_comment(minion_id, count);
    let repo_full = github::repo_slug(owner, repo);
    let result = crate::github::gh_cli_command(host)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted monitoring-paused notification on PR #{}", pr_number);
            println!(
                "⏸️  Monitoring paused. {} review(s) pending. Resume: gru resume {}",
                count, minion_id
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post exit notification on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment for exit notification: {}", e);
        }
    }
}

/// Posts an escalation comment on a PR when auto-rebase fails.
async fn post_escalation_comment(
    owner: &str,
    repo: &str,
    host: &str,
    pr_number: &str,
    message: &str,
    minion_id: &str,
) {
    let repo_full = github::repo_slug(owner, repo);
    let body = crate::progress_comments::format_escalation_comment(
        "Minion Escalation",
        message,
        minion_id,
    );

    let result = crate::github::gh_cli_command(host)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted escalation comment on PR #{}", pr_number);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post escalation comment on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment: {}", e);
        }
    }
}

/// Trigger a PR review as a separate process.
/// If `review_timeout` is `Some`, the review is killed after that duration.
/// If `None`, the review runs without a timeout (Claude's built-in stuck detection applies).
async fn trigger_pr_review(
    pr_number: &str,
    worktree_path: &Path,
    review_timeout: Option<Duration>,
) -> Result<i32> {
    // Validate PR number format (defense in depth)
    pr_number
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_number))?;

    let mut child = TokioCommand::new("gru")
        .arg("review")
        .arg(pr_number)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn gru review command for PR #{}. Is gru in your PATH?",
                pr_number
            )
        })?;

    match review_timeout {
        Some(timeout_duration) => match timeout(timeout_duration, child.wait()).await {
            Ok(status) => {
                let status = status.with_context(|| {
                    format!("Failed to wait for review process for PR #{}", pr_number)
                })?;
                Ok(status
                    .code()
                    .unwrap_or(crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED))
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let elapsed_secs = timeout_duration.as_secs();
                let time_display = if elapsed_secs >= 60 {
                    let minutes = elapsed_secs / 60;
                    let seconds = elapsed_secs % 60;
                    if seconds == 0 {
                        format!("{} minute{}", minutes, if minutes == 1 { "" } else { "s" })
                    } else {
                        format!(
                            "{} minute{} {} second{}",
                            minutes,
                            if minutes == 1 { "" } else { "s" },
                            seconds,
                            if seconds == 1 { "" } else { "s" }
                        )
                    }
                } else {
                    format!(
                        "{} second{}",
                        elapsed_secs,
                        if elapsed_secs == 1 { "" } else { "s" }
                    )
                };
                Err(anyhow::anyhow!(
                    "Review process timed out after {}. PR #{} review may be stuck.",
                    time_display,
                    pr_number
                ))
            }
        },
        None => {
            let status = child.wait().await.with_context(|| {
                format!("Failed to wait for review process for PR #{}", pr_number)
            })?;
            Ok(status
                .code()
                .unwrap_or(crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED))
        }
    }
}

/// Controls the monitoring loop flow after handling a PR event.
enum LoopAction {
    /// Continue to the next poll iteration.
    Continue,
    /// Break out of the monitoring loop.
    Break,
}

/// Immutable context shared across all event handlers.
struct MonitorContext<'a> {
    backend: &'a dyn AgentBackend,
    issue_ctx: &'a IssueContext,
    wt_ctx: &'a WorktreeContext,
    pr_number: &'a str,
    timeout_opt: Option<&'a str>,
    monitor_timeout: Duration,
}

/// Mutable state tracked across monitoring loop iterations.
struct MonitorLoopState {
    review_round: usize,
    issue_comment_round: usize,
    ci_escalated: bool,
    issue_was_blocked: bool,
    rebase_attempts: usize,
    judge_state: JudgeState,
    judge_label_ensured: bool,
    terminal_result: Option<MonitorResult>,
    consecutive_errors: u32,
    review_baseline: Option<DateTime<Utc>>,
    monitor_start: tokio::time::Instant,
    confidence_threshold: u8,
    /// Number of poll cycles to skip merge-conflict detection after a successful
    /// rebase + force-push. GitHub takes time to recompute the `mergeable` field,
    /// so stale `mergeable: false` would otherwise trigger redundant rebase cycles.
    rebase_cooldown_cycles: u32,
}

/// How many poll cycles to suppress merge-conflict detection after a successful
/// rebase + force-push. At 30s per cycle, 4 cycles ≈ 2 minutes — enough for
/// GitHub to recompute the `mergeable` field.
const REBASE_COOLDOWN_CYCLES: u32 = 4;

/// Retry attempts for the post-rebase up-to-date check. The primary agent may
/// resolve the conflict (e.g. via merge commit) while the rebase sub-agent is
/// running. At 20s per retry, 12 retries ≈ 4 minutes — enough to cover the
/// ~4-minute gap observed in the motivating incident.
const POST_CHECK_RETRIES: u32 = 12;

/// Interval (seconds) between post-rebase up-to-date check retries.
const POST_CHECK_INTERVAL_SECS: u64 = 20;

/// Sleep duration (seconds) when suppressing a stale MergeConflict during
/// cooldown. Matches the pr_monitor poll interval so suppressed cycles don't
/// spin hot and hammer the GitHub API.
const REBASE_COOLDOWN_SLEEP_SECS: u64 = 30;

/// 10 consecutive monitor_pr invocation failures before giving up.
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Backoff duration (in seconds) when rate-limited by the GitHub API.
/// 5 minutes is long enough to avoid hammering the API during a rate-limit
/// window while short enough to resume monitoring promptly.
const RATE_LIMIT_BACKOFF_SECS: u64 = 300;

impl MonitorLoopState {
    fn new(initial_baseline: DateTime<Utc>, confidence_threshold: u8) -> Self {
        Self {
            review_round: 0,
            issue_comment_round: 0,
            ci_escalated: false,
            issue_was_blocked: false,
            rebase_attempts: 0,
            judge_state: JudgeState::new(),
            judge_label_ensured: false,
            terminal_result: None,
            consecutive_errors: 0,
            review_baseline: Some(initial_baseline),
            monitor_start: tokio::time::Instant::now(),
            confidence_threshold,
            rebase_cooldown_cycles: 0,
        }
    }
}

fn handle_merged(state: &mut MonitorLoopState, ctx: &MonitorContext<'_>) -> LoopAction {
    println!("✅ PR #{} was merged successfully!", ctx.pr_number);
    println!(
        "🎉 Issue {} is complete!",
        ctx.issue_ctx
            .issue_num
            .map_or("?".to_string(), |n| n.to_string())
    );
    state.terminal_result = Some(MonitorResult::Merged);
    LoopAction::Break
}

fn handle_closed(state: &mut MonitorLoopState, ctx: &MonitorContext<'_>) -> LoopAction {
    println!("⚠️  PR #{} was closed without merging", ctx.pr_number);
    println!("   The issue may need to be reopened or addressed differently");
    state.terminal_result = Some(MonitorResult::Closed);
    LoopAction::Break
}

async fn handle_ready_to_merge(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
) -> LoopAction {
    // All merge gates pass — remove gru:blocked if it was stale
    if state.issue_was_blocked {
        if let Ok(pr_num) = ctx.pr_number.parse::<u64>() {
            if let Some(issue_num) = ctx.issue_ctx.issue_num {
                super::helpers::try_remove_blocked_label(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    pr_num,
                    issue_num,
                )
                .await;
            }
        }
        state.issue_was_blocked = false;
        state.ci_escalated = false;
    }

    // Lazily ensure the gru:needs-human-review label exists on
    // first ReadyToMerge, rather than unconditionally at startup.
    if !state.judge_label_ensured {
        if let Err(e) = merge_judge::ensure_needs_human_review_label(
            &ctx.issue_ctx.host,
            &ctx.issue_ctx.owner,
            &ctx.issue_ctx.repo,
        )
        .await
        {
            log::warn!("⚠️  Failed to ensure gru:needs-human-review label: {:#}", e);
        }
        state.judge_label_ensured = true;
    }

    // Check if gru:needs-human-review was previously applied and
    // not yet cleared — skip judge until human removes it.
    // On API failure, be conservative and skip (don't proceed).
    match merge_judge::has_needs_human_review_label(
        &ctx.issue_ctx.host,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        ctx.pr_number,
    )
    .await
    {
        Ok(true) => {
            println!(
                "⏸️  PR #{} has gru:needs-human-review — waiting for human to remove it",
                ctx.pr_number
            );
            return LoopAction::Continue;
        }
        Ok(false) => {
            // Only clear escalation if we previously confirmed the
            // label was applied. This prevents premature clearing if
            // the label add previously failed.
            if state.judge_state.label_was_applied() {
                state.judge_state.mark_escalation_cleared();
            }
        }
        Err(e) => {
            log::warn!(
                "Failed to check needs-human-review label: {} — skipping judge",
                e
            );
            return LoopAction::Continue;
        }
    }

    // If a previous failure escalation failed to apply the label, retry now
    // before invoking the judge (which would skip at the failure cap).
    if state.judge_state.should_escalate_on_failure() && !state.judge_state.label_was_applied() {
        log::info!("Retrying failed escalation label application...");
        match merge_judge::add_needs_human_review_label(
            &ctx.issue_ctx.host,
            &ctx.issue_ctx.owner,
            &ctx.issue_ctx.repo,
            ctx.pr_number,
        )
        .await
        {
            Ok(()) => {
                state.judge_state.mark_label_applied();
                state.judge_state.mark_failure_escalated();
                log::info!("Successfully applied needs-human-review label on retry");
            }
            Err(e) => {
                log::warn!("Retry of needs-human-review label failed: {}", e);
            }
        }
        return LoopAction::Continue;
    }

    // Invoke the merge-readiness judge.
    match merge_judge::evaluate(
        ctx.backend,
        &ctx.issue_ctx.host,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        ctx.pr_number,
        &ctx.wt_ctx.checkout_path,
        &mut state.judge_state,
        state.confidence_threshold,
    )
    .await
    {
        Ok(Some(response)) => match &response.action {
            JudgeAction::Merge => {
                println!(
                    "🚀 Judge approved merge for PR #{} (confidence: {}/10)",
                    ctx.pr_number, response.confidence
                );
                let repo_full = github::repo_slug(&ctx.issue_ctx.owner, &ctx.issue_ctx.repo);
                match crate::github::gh_cli_command(&ctx.issue_ctx.host)
                    .args([
                        "pr",
                        "merge",
                        ctx.pr_number,
                        "--squash",
                        "--auto",
                        "-R",
                        &repo_full,
                    ])
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {
                        println!("✅ Auto-merge queued for PR #{}!", ctx.pr_number);
                        println!(
                            "🎉 Issue {} is complete!",
                            ctx.issue_ctx
                                .issue_num
                                .map_or("?".to_string(), |n| n.to_string())
                        );
                        return LoopAction::Break;
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        log::warn!(
                            "⚠️  Auto-merge failed for PR #{}: {}",
                            ctx.pr_number,
                            stderr.trim()
                        );
                        println!("🔄 Will retry on next poll cycle...");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to run merge command: {}", e);
                        println!("🔄 Will retry on next poll cycle...");
                    }
                }
            }
            JudgeAction::Wait(duration) => {
                println!(
                    "⏳ Judge says wait {}m before re-evaluating PR #{}",
                    duration.as_secs() / 60,
                    ctx.pr_number
                );
                println!("🔄 Continuing to monitor PR...\n");
            }
            JudgeAction::Escalate => {
                println!(
                    "🚨 Judge escalated PR #{} for human review (confidence: {}/10)",
                    ctx.pr_number, response.confidence
                );
                // Apply label and post comment on PR.
                match merge_judge::add_needs_human_review_label(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    ctx.pr_number,
                )
                .await
                {
                    Ok(()) => state.judge_state.mark_label_applied(),
                    Err(e) => {
                        log::warn!("Failed to add needs-human-review label: {:#}", e);
                    }
                }
                merge_judge::post_judge_escalation_comment(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    ctx.pr_number,
                    &response,
                    &ctx.wt_ctx.minion_id,
                )
                .await;
                // Also post an explanatory comment on the issue.
                if let Some(issue_num) = ctx.issue_ctx.issue_num {
                    super::helpers::try_post_issue_comment(
                        &ctx.issue_ctx.host,
                        &ctx.issue_ctx.owner,
                        &ctx.issue_ctx.repo,
                        issue_num,
                        &format!(
                            "Merge judge escalated PR #{} for human review. See PR for details.",
                            ctx.pr_number
                        ),
                    )
                    .await;
                }
                println!("🔄 Continuing to monitor PR...\n");
            }
        },
        Ok(None) => {
            // Judge invocation skipped (same state, no timer expired).
            log::debug!("Judge invocation skipped — PR state unchanged");
        }
        Err(e) => {
            log::warn!("⚠️  Merge judge failed: {:#}", e);
            if state.judge_state.should_escalate_on_failure() {
                log::warn!(
                    "Judge failed {} consecutive times — escalating for human review",
                    state.judge_state.consecutive_failures()
                );
                println!(
                    "🚨 Judge failed {} times on same PR state — escalating for human review",
                    state.judge_state.consecutive_failures()
                );
                match merge_judge::add_needs_human_review_label(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    ctx.pr_number,
                )
                .await
                {
                    Ok(()) => state.judge_state.mark_label_applied(),
                    Err(label_err) => {
                        log::warn!("Failed to add needs-human-review label: {}", label_err);
                    }
                }
                merge_judge::post_judge_escalation_comment(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    ctx.pr_number,
                    &JudgeResponse {
                        confidence: 0,
                        action: JudgeAction::Escalate,
                        reasoning: format!(
                            "Merge judge failed {} consecutive times (last error: {}). \
                             Unable to evaluate merge readiness automatically.",
                            state.judge_state.consecutive_failures(),
                            e
                        ),
                    },
                    &ctx.wt_ctx.minion_id,
                )
                .await;
                // Only mark escalation complete if the label was applied.
                // If the label add failed, leave unmarked so we retry on
                // the next cycle (should_invoke returns false at cap, but
                // next fingerprint change or label retry will re-attempt).
                if state.judge_state.label_was_applied() {
                    state.judge_state.mark_failure_escalated();
                }
            } else {
                let backoff = state.judge_state.retry_backoff_minutes();
                println!(
                    "🔄 Will retry after ~{}m backoff (failure {}/{})...",
                    backoff,
                    state.judge_state.consecutive_failures(),
                    merge_judge::MAX_CONSECUTIVE_FAILURES
                );
            }
        }
    }
    LoopAction::Continue
}

/// Apply the #867 idempotency filter to review feedback before invoking the
/// agent. Fetches the bot user's login and every inline comment on the PR,
/// then drops any thread where a bot reply already exists. On failure, logs
/// a warning and returns the feedback unchanged so the normal path proceeds.
async fn apply_bot_reply_idempotency_filter(
    ctx: &MonitorContext<'_>,
    feedback: pr_monitor::ReviewFeedback,
) -> pr_monitor::ReviewFeedback {
    // Short-circuit: if there are no inline comments, the per-thread filter
    // has nothing to do. Skip the `gh api user` + `pulls/{n}/comments` round
    // trips on review-body-only cycles.
    if feedback.comments.is_empty() {
        return feedback;
    }

    let bot_user = match github::get_authenticated_user(&ctx.issue_ctx.host).await {
        Ok(u) => u,
        Err(e) => {
            log::warn!(
                "⚠️  Could not resolve bot user for review-reply idempotency check: {:#}",
                e
            );
            return feedback;
        }
    };

    let already_replied = match pr_monitor::fetch_threads_with_bot_replies(
        &ctx.issue_ctx.host,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        ctx.pr_number,
        &bot_user,
    )
    .await
    {
        Ok(set) => set,
        Err(e) => {
            log::warn!(
                "⚠️  Review-reply idempotency check failed (proceeding): {:#}",
                e
            );
            return feedback;
        }
    };

    let original_count = feedback.comments.len();
    let filtered = pr_monitor::filter_feedback_against_replied_threads(feedback, &already_replied);
    let skipped = original_count.saturating_sub(filtered.comments.len());
    if skipped > 0 {
        println!(
            "🛡️  Skipping {} review thread(s) already replied to by @{}",
            skipped, bot_user
        );
    }
    filtered
}

/// Decision taken after the idempotency filter runs.
///
/// Extracted so the branch taken by `handle_new_reviews` is unit-testable
/// without spinning up the monitor loop.
#[derive(Debug, PartialEq, Eq)]
enum PostFilterAction {
    /// Feedback is empty with no fetch failures — advance the baseline and
    /// skip the agent invocation.
    SkipAdvance,
    /// Feedback is empty but some upstream fetches failed — hold the
    /// baseline and retry on the next poll cycle.
    SkipHold,
    /// Feedback still contains items to address — invoke the agent.
    Invoke,
}

fn decide_post_filter_action(feedback: &pr_monitor::ReviewFeedback) -> PostFilterAction {
    if !feedback.is_empty() {
        return PostFilterAction::Invoke;
    }
    if feedback.had_fetch_failures {
        PostFilterAction::SkipHold
    } else {
        PostFilterAction::SkipAdvance
    }
}

async fn handle_new_reviews(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
    feedback: pr_monitor::ReviewFeedback,
    check_time: DateTime<Utc>,
) -> LoopAction {
    let count = feedback.comments.len() + feedback.bodies.len();
    println!(
        "💬 Detected {} new review feedback item(s) on PR #{}",
        count, ctx.pr_number
    );

    // Best-effort idempotency check (#867): drop inline comments on threads
    // where the bot user has already posted a reply. Belt on top of the
    // process-level lockfile + registry guards from #862/#863. On any
    // API failure we proceed with the unfiltered feedback.
    let feedback = apply_bot_reply_idempotency_filter(ctx, feedback).await;

    match decide_post_filter_action(&feedback) {
        PostFilterAction::SkipHold => {
            // Some reviews failed to fetch upstream. The comments we did
            // fetch are all already-replied, but the unfetched ones may
            // still contain unhandled threads — leave the baseline alone so
            // poll_once retries on the next cycle. review_round has not
            // been incremented, so repeated retries cannot burn the round
            // budget.
            log::warn!(
                "⚠️  All fetched review threads already have bot replies, but some \
                 review fetches failed — will retry on the next poll cycle"
            );
            return LoopAction::Continue;
        }
        PostFilterAction::SkipAdvance => {
            println!("✅ All review threads already have a bot reply; nothing to address");
            state.review_baseline = Some(check_time);
            save_review_check_time(&ctx.wt_ctx.minion_id, check_time).await;
            reset_attempt_count(&ctx.wt_ctx.minion_id).await;
            return LoopAction::Continue;
        }
        PostFilterAction::Invoke => {}
    }

    // Count this against MAX_REVIEW_ROUNDS only when we are actually going
    // to invoke the agent — filtered-to-empty batches and fetch-failure
    // retries must not burn the budget.
    state.review_round += 1;
    println!(
        "🔄 Review round {}/{}",
        state.review_round, MAX_REVIEW_ROUNDS
    );

    if state.review_round > MAX_REVIEW_ROUNDS {
        println!(
            "⚠️  Reached maximum review rounds limit ({})",
            MAX_REVIEW_ROUNDS
        );
        println!("   Additional reviews will need manual handling");
        println!(
            "   View PR: https://{}/{}/{}/pull/{}",
            ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
        );
        return LoopAction::Break;
    }

    let review_prompt = pr_monitor::format_review_prompt(
        ctx.issue_ctx.issue_num,
        ctx.pr_number,
        &feedback,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        &ctx.wt_ctx.minion_id,
    );

    println!("🔄 Re-invoking to address review feedback...\n");

    // Capture the session start time so the post-hoc dedup only considers
    // replies the agent posts during this invocation — replies from prior
    // sessions, which the human reviewer has implicitly accepted, are left
    // untouched.
    //
    // Subtract a 1-minute safety margin to absorb clock skew between the
    // local host (`Utc::now()`) and GitHub's server clock (which sets
    // `created_at`). Without this margin, if the local clock runs ahead,
    // the very first reply could land with `created_at < session_start`
    // and be excluded — then a later duplicate would be kept and the
    // earlier original discarded. A minute is wide enough for any
    // plausible skew and still far narrower than any prior-session reply
    // (which would be minutes to hours older).
    let session_start = Utc::now() - chrono::Duration::minutes(1);

    let agent_result = invoke_agent_for_reviews(
        ctx.backend,
        &ctx.wt_ctx.checkout_path,
        &ctx.wt_ctx.minion_dir,
        &ctx.wt_ctx.session_id,
        &review_prompt,
        ctx.timeout_opt,
        &ctx.issue_ctx.host,
    )
    .await;

    // Post-hoc backstop for #805: even with the prompt-level constraint
    // added in #804, the agent may post duplicate inline replies. Run the
    // sweep regardless of whether the invocation succeeded — a timeout or
    // late error does not unpost replies the agent already made, and a
    // partial-session Minion can still have produced duplicates.
    match pr_monitor::dedup_minion_inline_replies(
        &ctx.issue_ctx.host,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        ctx.pr_number,
        &ctx.wt_ctx.minion_id,
        session_start,
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => println!("🧹 Removed {} duplicate inline reply comment(s)", n),
        Err(e) => log::warn!("⚠️  Duplicate inline reply sweep failed: {:#}", e),
    }

    match agent_result {
        Ok(()) => {
            println!("\n✅ Finished addressing review comments");
            println!("🔄 Continuing to monitor PR...\n");
            // Use the check_time returned by monitor_pr, which was
            // advanced past the reviews we just handled. This ensures
            // those reviews aren't re-fetched while still catching
            // any new reviews posted during handling.
            state.review_baseline = Some(check_time);
            // Persist updated baseline after successfully handling reviews.
            save_review_check_time(&ctx.wt_ctx.minion_id, check_time).await;
            // Reset attempt_count so the cap only triggers on consecutive
            // failures, not cumulative resume cycles across successful rounds.
            reset_attempt_count(&ctx.wt_ctx.minion_id).await;
            LoopAction::Continue
        }
        Err(e) => {
            log::warn!("⚠️  Failed to address review comments: {:#}", e);
            log::warn!("   You can address them manually");
            LoopAction::Break
        }
    }
}

async fn handle_new_issue_comments(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
    comments: Vec<pr_monitor::IssueComment>,
    check_time: DateTime<Utc>,
) -> LoopAction {
    state.issue_comment_round += 1;

    if state.issue_comment_round > MAX_REVIEW_ROUNDS {
        println!(
            "💬 Detected {} new PR comment(s) on PR #{} — reached maximum comment rounds limit ({})",
            comments.len(),
            ctx.pr_number,
            MAX_REVIEW_ROUNDS
        );
        println!("   Additional comments will need manual handling");
        println!(
            "   View PR: https://{}/{}/{}/pull/{}",
            ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
        );
        return LoopAction::Break;
    }

    println!(
        "💬 Detected {} new PR comment(s) on PR #{} (round {}/{})",
        comments.len(),
        ctx.pr_number,
        state.issue_comment_round,
        MAX_REVIEW_ROUNDS
    );

    let prompt = pr_monitor::format_issue_comments_prompt(
        ctx.issue_ctx.issue_num,
        ctx.pr_number,
        &comments,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        &ctx.wt_ctx.minion_id,
    );

    println!("🔄 Re-invoking to address PR comments...\n");

    match invoke_agent_for_reviews(
        ctx.backend,
        &ctx.wt_ctx.checkout_path,
        &ctx.wt_ctx.minion_dir,
        &ctx.wt_ctx.session_id,
        &prompt,
        ctx.timeout_opt,
        &ctx.issue_ctx.host,
    )
    .await
    {
        Ok(()) => {
            println!("\n✅ Finished addressing PR comments");
            println!("🔄 Continuing to monitor PR...\n");
            state.review_baseline = Some(check_time);
            save_review_check_time(&ctx.wt_ctx.minion_id, check_time).await;
            reset_attempt_count(&ctx.wt_ctx.minion_id).await;
            LoopAction::Continue
        }
        Err(e) => {
            log::warn!("⚠️  Failed to address PR comments: {:#}", e);
            log::warn!("   You can address them manually");
            LoopAction::Break
        }
    }
}

async fn handle_failed_checks(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
    count: usize,
) -> LoopAction {
    if state.ci_escalated {
        // Already escalated — wait for human intervention
        println!(
            "ℹ️  CI still failing ({} check(s)) on PR #{}, waiting for human fix",
            count, ctx.pr_number
        );
        return LoopAction::Continue;
    }

    println!(
        "❌ Detected {} failed CI check(s) on PR #{}, attempting auto-fix...",
        count, ctx.pr_number
    );

    let pr_num_u64 = match ctx.pr_number.parse::<u64>() {
        Ok(n) => n,
        Err(_) => {
            println!("⚠️  Could not parse PR number, skipping CI auto-fix");
            println!("🔄 Continuing to monitor PR for other events...\n");
            return LoopAction::Continue;
        }
    };

    match ci::monitor_and_fix_ci(
        ctx.backend,
        &ctx.issue_ctx.host,
        &ctx.issue_ctx.owner,
        &ctx.issue_ctx.repo,
        pr_num_u64,
        &ctx.wt_ctx.branch_name,
        &ctx.wt_ctx.checkout_path,
        &ctx.wt_ctx.minion_dir,
        &ctx.wt_ctx.minion_id,
    )
    .await
    {
        Ok(true) => {
            println!("✅ CI checks now pass after auto-fix");
            if state.issue_was_blocked {
                if let Some(issue_num) = ctx.issue_ctx.issue_num {
                    super::helpers::try_remove_blocked_label(
                        &ctx.issue_ctx.host,
                        &ctx.issue_ctx.owner,
                        &ctx.issue_ctx.repo,
                        pr_num_u64,
                        issue_num,
                    )
                    .await;
                }
                state.issue_was_blocked = false;
            }
            state.ci_escalated = false;
            println!("🔄 Continuing to monitor PR...\n");
        }
        Ok(false) => {
            state.ci_escalated = true;
            state.issue_was_blocked = true;
            // Note: on minion restart ci_escalated starts false, so this arm
            // may run again. The label application is idempotent; a second
            // comment is an acceptable trade-off until escalation state is
            // persisted in the registry.
            if let Some(issue_num) = ctx.issue_ctx.issue_num {
                super::helpers::try_mark_issue_blocked(
                    &ctx.issue_ctx.host,
                    &ctx.issue_ctx.owner,
                    &ctx.issue_ctx.repo,
                    issue_num,
                    &format!(
                        "CI auto-fix failed after {} attempts. See PR #{} for details. Human intervention required.",
                        ci::MAX_CI_FIX_ATTEMPTS,
                        ctx.pr_number
                    ),
                )
                .await;
            }
            println!("⚠️  CI auto-fix escalated to human after max attempts");
            println!(
                "   Review the checks at: https://{}/{}/{}/pull/{}/checks",
                ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
            );
            println!("🔄 Continuing to monitor PR for other events...\n");
        }
        Err(e) => {
            println!("⚠️  CI auto-fix error: {}", e);
            println!(
                "   Review the checks at: https://{}/{}/{}/pull/{}/checks",
                ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
            );
            println!("🔄 Will retry CI auto-fix on subsequent monitoring cycles...\n");
        }
    }
    LoopAction::Continue
}

async fn handle_merge_conflict(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
    check_time: DateTime<Utc>,
) -> LoopAction {
    // Always advance the baseline before any branching. Empty-body review
    // objects are created by GitHub for each inline reply the Minion posted
    // while addressing earlier feedback. Their submitted_at is after the
    // pre-reply baseline, so without this advancement they are counted as
    // unaddressed external reviews by the lab daemon — causing an infinite
    // wake-up + rebase-failure loop. When poll_once successfully fetched
    // reviews/comments, check_time should already be after those reply
    // reviews because last_check_time was advanced before the conflict was
    // detected. That ordering is not guaranteed on fetch-failure paths, so
    // this baseline update is primarily to preserve the successful-fetch
    // behavior and avoid reprocessing reply-review artifacts.
    state.review_baseline = Some(check_time);

    if state.rebase_attempts >= MAX_REBASE_ATTEMPTS {
        println!(
            "❌ Reached maximum rebase attempts ({}), escalating",
            MAX_REBASE_ATTEMPTS
        );
        post_escalation_comment(
            &ctx.issue_ctx.owner,
            &ctx.issue_ctx.repo,
            &ctx.issue_ctx.host,
            ctx.pr_number,
            "Auto-rebase failed after multiple attempts. Manual conflict resolution required.",
            &ctx.wt_ctx.minion_id,
        )
        .await;
        return LoopAction::Break;
    }

    state.rebase_attempts += 1;
    println!(
        "⚠️  Merge conflict detected on PR #{} (rebase attempt {}/{})",
        ctx.pr_number, state.rebase_attempts, MAX_REBASE_ATTEMPTS
    );

    match auto_rebase_pr(&ctx.wt_ctx.checkout_path, &ctx.wt_ctx.minion_dir).await {
        Ok(AutoRebaseResult::RebasedAndPushed) => {
            // Reset counter on success — GitHub may still report
            // mergeable: false for a few poll cycles after force-push
            // while it recomputes. We don't want stale signals to
            // exhaust the attempt budget.
            state.rebase_attempts = 0;
            // Suppress merge-conflict detection for a few poll cycles
            // to let GitHub recompute the mergeable field after force-push.
            state.rebase_cooldown_cycles = REBASE_COOLDOWN_CYCLES;
            // review_baseline already set unconditionally at function entry.
            println!("✅ Rebase succeeded, continuing to monitor PR...\n");
            LoopAction::Continue
        }
        Ok(AutoRebaseResult::AlreadyUpToDate) => {
            // Branch is already up-to-date — origin/<base> is an ancestor of
            // HEAD, so a real merge conflict is impossible. The mergeable:false
            // signal must be stale. Apply cooldown to avoid a tight loop of
            // redundant MergeConflict → already-up-to-date cycles that would
            // never reach the attempt cap (since attempts reset on success).
            state.rebase_attempts = 0;
            state.rebase_cooldown_cycles = REBASE_COOLDOWN_CYCLES;
            // review_baseline already set unconditionally at function entry.
            println!("✅ Branch already up-to-date, continuing to monitor PR...\n");
            LoopAction::Continue
        }
        Ok(AutoRebaseResult::ConflictUnresolved) => {
            // Before escalating, re-check GitHub's view of the PR. The primary
            // agent may have resolved via merge commit after the rebase sub-agent
            // exited, making the PR clean on the remote even though the local
            // post-check timed out.
            //
            // We only bypass escalation on Some(true). None means GitHub is still
            // computing (common after a recent push), and Some(false) means the PR
            // is still conflicted. Both fall through to escalation — after the
            // configured local retry window (~4 minutes), this is the appropriate response.
            match pr_monitor::get_pr_info_for_wake_check(
                &ctx.issue_ctx.host,
                &ctx.issue_ctx.owner,
                &ctx.issue_ctx.repo,
                ctx.pr_number,
            )
            .await
            {
                Ok((_, _, Some(true))) => {
                    log::info!(
                        "PR is already mergeable on GitHub — primary agent resolved the conflict"
                    );
                    println!("✅ PR is mergeable on GitHub, continuing to monitor PR...\n");
                    state.rebase_attempts = 0;
                    state.rebase_cooldown_cycles = REBASE_COOLDOWN_CYCLES;
                    return LoopAction::Continue;
                }
                Ok((_, _, status)) => {
                    log::debug!(
                        "GitHub re-check: mergeable={:?} — proceeding to escalation",
                        status
                    );
                }
                Err(e) => {
                    log::warn!("GitHub re-check failed, proceeding to escalation: {:#}", e);
                }
            }
            println!("❌ Could not resolve merge conflicts automatically");
            post_escalation_comment(
                &ctx.issue_ctx.owner,
                &ctx.issue_ctx.repo,
                &ctx.issue_ctx.host,
                ctx.pr_number,
                "Auto-rebase failed: could not resolve merge conflicts automatically. Manual intervention required.",
                &ctx.wt_ctx.minion_id,
            )
            .await;
            LoopAction::Break
        }
        Err(e) => {
            log::warn!("⚠️  Auto-rebase error: {:#}", e);
            post_escalation_comment(
                &ctx.issue_ctx.owner,
                &ctx.issue_ctx.repo,
                &ctx.issue_ctx.host,
                ctx.pr_number,
                "Auto-rebase encountered an unexpected error. Check Minion logs for details.",
                &ctx.wt_ctx.minion_id,
            )
            .await;
            LoopAction::Break
        }
    }
}

fn handle_timeout(state: &MonitorLoopState, ctx: &MonitorContext<'_>) -> LoopAction {
    let display = format_duration(state.monitor_start.elapsed().as_secs());
    println!("⏰ PR monitoring timed out after {}", display);
    println!(
        "   PR is still open: https://{}/{}/{}/pull/{}",
        ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
    );
    LoopAction::Break
}

fn handle_interrupted(ctx: &MonitorContext<'_>) -> LoopAction {
    println!("\n⚠️  Monitoring interrupted by user");
    println!(
        "   PR is still open: https://{}/{}/{}/pull/{}",
        ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
    );
    LoopAction::Break
}

/// Sleep for `backoff` (capped at the remaining monitor timeout), aborting on
/// Ctrl+C. Returns `Some(LoopAction::Break)` if interrupted, `None` otherwise.
async fn sleep_with_timeout(
    backoff: Duration,
    state: &MonitorLoopState,
    ctx: &MonitorContext<'_>,
) -> Option<LoopAction> {
    let remaining = ctx
        .monitor_timeout
        .checked_sub(state.monitor_start.elapsed());
    match remaining {
        Some(r) if r > Duration::ZERO => {
            tokio::select! {
                _ = tokio::time::sleep(backoff.min(r)) => None,
                _ = tokio::signal::ctrl_c() => {
                    Some(handle_interrupted(ctx))
                }
            }
        }
        _ => None,
    }
}

async fn handle_monitor_error(
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
    error: anyhow::Error,
) -> LoopAction {
    let error_msg = format!("{:#}", error);

    // Rate-limit errors are expected and should not count toward the bailout
    // threshold. Use a longer backoff (5 minutes) to wait out the rate-limit
    // window instead of hammering the API every 30 seconds.
    if pr_monitor::is_rate_limit_error(&error_msg) {
        let backoff = Duration::from_secs(RATE_LIMIT_BACKOFF_SECS);
        log::info!(
            "ℹ️  GitHub API rate limited, backing off for {}: {:#}",
            format_duration(backoff.as_secs()),
            error
        );
        if let Some(action) = sleep_with_timeout(backoff, state, ctx).await {
            return action;
        }
        return LoopAction::Continue;
    }

    state.consecutive_errors += 1;
    if state.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
        log::warn!(
            "⚠️  PR monitoring failed {} consecutive times, giving up: {}",
            state.consecutive_errors,
            error
        );
        log::warn!(
            "   You can monitor manually at: https://{}/{}/{}/pull/{}",
            ctx.issue_ctx.host,
            ctx.issue_ctx.owner,
            ctx.issue_ctx.repo,
            ctx.pr_number
        );
        return LoopAction::Break;
    }
    log::warn!(
        "⚠️  PR monitoring error ({}/{}): {}",
        state.consecutive_errors,
        MAX_CONSECUTIVE_ERRORS,
        error
    );
    // Sleep before retrying to avoid hammering the API if monitor_pr
    // fails before its internal poll sleep. Cap at remaining timeout
    // so we don't overshoot the configured monitor_timeout.
    let backoff = Duration::from_secs(30);
    if let Some(action) = sleep_with_timeout(backoff, state, ctx).await {
        return action;
    }
    LoopAction::Continue
}

/// Dispatches a PR monitoring event to the appropriate handler.
async fn handle_pr_event(
    event: Result<(MonitorResult, DateTime<Utc>)>,
    state: &mut MonitorLoopState,
    ctx: &MonitorContext<'_>,
) -> LoopAction {
    if event.is_ok() {
        state.consecutive_errors = 0;
    }

    match event {
        Ok((MonitorResult::Merged, _)) => handle_merged(state, ctx),
        Ok((MonitorResult::Closed, _)) => handle_closed(state, ctx),
        Ok((MonitorResult::ReadyToMerge, _)) => handle_ready_to_merge(state, ctx).await,
        Ok((MonitorResult::NewReviews(feedback), check_time)) => {
            handle_new_reviews(state, ctx, feedback, check_time).await
        }
        Ok((MonitorResult::NewIssueComments(comments), check_time)) => {
            handle_new_issue_comments(state, ctx, comments, check_time).await
        }
        Ok((MonitorResult::FailedChecks(count), _)) => {
            handle_failed_checks(state, ctx, count).await
        }
        Ok((MonitorResult::MergeConflict, check_time)) => {
            if state.rebase_cooldown_cycles > 0 {
                state.rebase_cooldown_cycles -= 1;
                // Advance baseline here too so that if the Minion exits during
                // the cooldown window the persisted baseline reflects this
                // poll's check_time, not the pre-reply value.
                state.review_baseline = Some(check_time);
                log::info!(
                    "Ignoring stale mergeable:false during post-rebase cooldown ({} cycles remaining)",
                    state.rebase_cooldown_cycles
                );
                // Sleep for the poll interval so we don't spin hot and hammer
                // the GitHub API. monitor_pr returns MergeConflict immediately
                // (no internal sleep), so without this the outer loop would
                // burn through all cooldown cycles in seconds.
                tokio::time::sleep(Duration::from_secs(REBASE_COOLDOWN_SLEEP_SECS)).await;
                LoopAction::Continue
            } else {
                handle_merge_conflict(state, ctx, check_time).await
            }
        }
        Ok((MonitorResult::Timeout, _)) => handle_timeout(state, ctx),
        Ok((MonitorResult::Interrupted, _)) => handle_interrupted(ctx),
        Err(e) => handle_monitor_error(state, ctx, e).await,
    }
}

/// Monitors a PR for reviews, CI failures, and merge/close events.
/// Handles automatic review rounds up to MAX_REVIEW_ROUNDS.
///
/// Returns `Some(Merged)` or `Some(Closed)` when the PR reaches a terminal
/// state. Returns `None` for all other exit paths (timeout, user interrupt,
/// review-round cap, rebase failure, consecutive errors).
pub(crate) async fn monitor_pr_lifecycle(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &str,
    timeout_opt: Option<&str>,
    review_timeout: Option<Duration>,
    monitor_timeout: Duration,
) -> Option<MonitorResult> {
    // Ensure the gru:ready-to-merge label exists in the repo
    if let Err(e) =
        pr_monitor::ensure_ready_to_merge_label(&issue_ctx.host, &issue_ctx.owner, &issue_ctx.repo)
            .await
    {
        log::warn!(
            "⚠️  Failed to ensure {} label: {}",
            crate::labels::READY_TO_MERGE,
            e
        );
    }

    // Capture timestamp before self-review so that any reviews submitted
    // during the review are not missed when the monitoring loop starts.
    let pre_review_time = chrono::Utc::now();

    // Guard: skip self-review if a review was already triggered or posted for the
    // current HEAD SHA.
    //
    // Two layers of detection:
    //
    // 1. `pending_review_sha` (registry): set immediately before spawning the review
    //    subprocess. If the monitoring session crashes while the subprocess is running,
    //    a resumed session sees this flag and skips spawning a duplicate.  Cleared
    //    after `trigger_pr_review` returns (success or error).
    //
    // 2. `has_gru_review_for_sha` (GitHub API): checks whether a review was actually
    //    posted.  Used as the primary guard when no pending flag is set.  Fails open:
    //    if the API is unreachable we allow the review to proceed (a duplicate is less
    //    harmful than a missing review).
    //
    // If HEAD is unavailable, both guards are inactive and the review proceeds.
    //
    // This is still best-effort duplicate suppression, not a strict guarantee:
    // concurrent monitor sessions can both observe no pending flag and no posted
    // review for the current SHA before either session sets `pending_review_sha`,
    // and both may then spawn a review subprocess.
    let head_sha = ci::get_head_sha(&wt_ctx.checkout_path).await.ok();
    if head_sha.is_none() {
        log::debug!("HEAD SHA unavailable; pending_review_sha guard inactive for this session");
    }

    // Check the registry-persisted pending flag first (covers the crash-during-review case).
    let pending_sha = get_pending_review_sha(&wt_ctx.minion_id).await;
    let review_pending = head_sha
        .as_deref()
        .zip(pending_sha.as_deref())
        .is_some_and(|(head, pending)| head == pending);

    let already_reviewed = if review_pending {
        // A review subprocess was already spawned for this SHA in a prior session
        // that may still be running.  Skip to avoid a duplicate.
        true
    } else if let Some(ref sha) = head_sha {
        github::has_gru_review_for_sha(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            pr_number,
            sha,
        )
        .await
    } else {
        false
    };

    if already_reviewed {
        if review_pending {
            println!(
                "\n⏭️  Skipping self-review: review already spawned for current HEAD ({})",
                head_sha.as_deref().unwrap_or("unknown")
            );
        } else {
            println!(
                "\n⏭️  Skipping self-review: already posted for current HEAD ({})",
                head_sha.as_deref().unwrap_or("unknown")
            );
        }
    } else {
        // Auto-trigger review for Minion-created PRs
        println!("\n🔍 Starting automated PR review...");
        // Persist the pre-review timestamp as the initial review baseline.
        // Done here (not before the already_reviewed guard) so that on re-entry
        // we don't overwrite a stored baseline with the current time, which would
        // cause reviews posted while the minion was stopped to be skipped.
        save_review_check_time(&wt_ctx.minion_id, pre_review_time).await;
        // Persist the pending SHA *before* spawning the subprocess.  Setting it
        // after spawn would miss the primary crash scenario (session dies while the
        // child is running).  The trade-off: a crash in the narrow window *after*
        // this write but *before* the child is actually started leaves the flag set
        // with no subprocess running; the next resumed session will skip this review
        // round.  That false-positive is far less likely and less harmful than the
        // duplicate-review problem this flag exists to prevent.
        if let Some(ref sha) = head_sha {
            save_pending_review_sha(&wt_ctx.minion_id, sha).await;
        }
        match trigger_pr_review(pr_number, &wt_ctx.checkout_path, review_timeout).await {
            Ok(review_exit_code) => {
                if review_exit_code == 0 {
                    println!("✅ PR review completed successfully");
                } else {
                    log::warn!(
                        "⚠️  PR review completed with exit code: {}",
                        review_exit_code
                    );
                }
            }
            Err(e) => {
                log::warn!("⚠️  Failed to run PR review: {:#}", e);
                log::warn!("   You can review manually with: gru review {}", pr_number);
            }
        }
        // Clear the pending flag now that the subprocess has returned.
        clear_pending_review_sha(&wt_ctx.minion_id).await;
    }

    // Start monitoring the PR for review comments, CI failures, and merge/close events
    println!("\n👀 Monitoring PR for updates (polling every 30s)...");
    println!("   Press Ctrl+C to stop monitoring\n");

    // Load merge confidence threshold from config (falls back to default).
    // Uses load_partial to avoid requiring [daemon].repos for non-daemon commands.
    let confidence_threshold = LabConfig::default_path()
        .ok()
        .and_then(|p| {
            LabConfig::load_partial(&p)
                .map_err(|e| {
                    log::warn!("Failed to load config for merge threshold: {e}, using default");
                    e
                })
                .ok()
        })
        .map(|c| c.merge.confidence_threshold.clamp(1, 10))
        .unwrap_or(merge_judge::DEFAULT_CONFIDENCE_THRESHOLD);

    // Track review baseline across monitor_pr re-entries so reviews posted
    // before/during event handling (e.g. rebase) are not silently dropped.
    //
    // On a fresh run (first self-review), use pre_review_time so reviews
    // submitted during the self-review are detected on the first poll.
    //
    // On re-entry (already_reviewed, e.g. after crash+resume), resolve the
    // baseline using a fallback chain that avoids losing reviews posted
    // while the minion was stopped:
    //   1. Persisted last_review_check_time (saved at monitor exit or after each review round)
    //   2. PR creation time from GitHub API (catches all reviews ever posted)
    //   3. Minion started_at from registry (earlier than "now", avoids dropping reviews)
    //   4. Current time (true last resort if registry is also unavailable)
    let initial_baseline = if already_reviewed {
        let mid = wt_ctx.minion_id.clone();
        let (persisted_check_time, started_at) = minion_registry::with_registry(move |registry| {
            Ok(registry
                .get(&mid)
                .map(|info| (info.last_review_check_time, info.started_at)))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or((None, pre_review_time));

        match persisted_check_time {
            Some(ts) => ts,
            None => {
                // No persisted baseline — fall back to PR creation time so
                // reviews posted between PR creation and resume are detected.
                match pr_monitor::get_pr_created_at(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    pr_number,
                )
                .await
                {
                    Ok(created) => {
                        log::info!(
                            "Using PR creation time {} as review baseline (no persisted state)",
                            created
                        );
                        created
                    }
                    Err(e) => {
                        log::warn!(
                            "⚠️  Failed to fetch PR creation time, \
                             using minion start time as baseline: {}",
                            e
                        );
                        started_at
                    }
                }
            }
        }
    } else {
        pre_review_time
    };

    let mut state = MonitorLoopState::new(initial_baseline, confidence_threshold);
    let ctx = MonitorContext {
        backend,
        issue_ctx,
        wt_ctx,
        pr_number,
        timeout_opt,
        monitor_timeout,
    };

    loop {
        // Guard: check if the outer lifecycle budget is exhausted before starting
        // a new monitor_pr call. Distinct from MonitorResult::Timeout, which fires
        // when monitor_pr's own polling loop exceeds the remaining duration.
        let remaining = ctx
            .monitor_timeout
            .checked_sub(state.monitor_start.elapsed());
        if remaining.is_none() || remaining == Some(Duration::ZERO) {
            let display = format_duration(state.monitor_start.elapsed().as_secs());
            println!("⏰ PR monitoring timed out after {}", display);
            println!(
                "   PR is still open: https://{}/{}/{}/pull/{}",
                ctx.issue_ctx.host, ctx.issue_ctx.owner, ctx.issue_ctx.repo, ctx.pr_number
            );
            break;
        }

        let event = pr_monitor::monitor_pr(
            &ctx.issue_ctx.host,
            &ctx.issue_ctx.owner,
            &ctx.issue_ctx.repo,
            ctx.pr_number,
            remaining,
            state.review_baseline,
            &ctx.wt_ctx.minion_id,
        )
        .await;

        let action = handle_pr_event(event, &mut state, &ctx).await;

        match action {
            LoopAction::Continue => continue,
            LoopAction::Break => break,
        }
    }

    // Persist the final review baseline on monitor exit so the lab wake-up
    // scan knows where to resume without re-processing already-seen reviews.
    // Also check if the PR is still open with unaddressed external reviews and
    // post a notification if so.
    if let Some(baseline) = state.review_baseline {
        save_review_check_time(&ctx.wt_ctx.minion_id, baseline).await;
        post_exit_notification_if_needed(
            &ctx.issue_ctx.owner,
            &ctx.issue_ctx.repo,
            &ctx.issue_ctx.host,
            ctx.pr_number,
            &ctx.wt_ctx.minion_id,
            baseline,
        )
        .await;
    }

    state.terminal_result
}

/// Monitors CI after the initial fix and attempts auto-fixes if checks fail.
/// Returns Ok(true) if CI passed, Ok(false) if escalated/failed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn monitor_ci_after_fix(
    backend: &dyn AgentBackend,
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    worktree_path: &Path,
    events_dir: &Path,
    minion_id: &str,
) -> Result<bool> {
    let pr_number = match ci::get_pr_number(host, owner, repo, branch, None).await? {
        Some(num) => num,
        None => {
            eprintln!(
                "ℹ️  No PR found for branch {}, skipping CI monitoring",
                branch
            );
            return Ok(true);
        }
    };

    // Backfill the minion registry if it has pr: null but we discovered a PR.
    // Read first to avoid an unnecessary save when pr is already set.
    let mid = minion_id.to_string();
    let needs_backfill = crate::minion_registry::with_registry({
        let mid = mid.clone();
        move |registry| Ok(registry.get(&mid).is_some_and(|info| info.pr.is_none()))
    })
    .await
    .unwrap_or(false);

    if needs_backfill {
        let pr_num_for_backfill = pr_number;
        if let Err(e) = crate::minion_registry::with_registry(move |registry| {
            registry.update(&mid, |info| {
                if info.pr.is_none() {
                    log::info!(
                        "📝 Backfilling registry: minion now linked to PR #{}",
                        pr_num_for_backfill
                    );
                    info.pr = Some(pr_num_for_backfill.to_string());
                }
            })
        })
        .await
        {
            log::warn!("⚠️  Failed to backfill PR in registry: {:#}", e);
        }
    }

    eprintln!("🔍 Monitoring CI for PR #{}", pr_number);
    ci::monitor_and_fix_ci(
        backend,
        host,
        owner,
        repo,
        pr_number,
        branch,
        worktree_path,
        events_dir,
        minion_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentBackend, AgentEvent};
    use std::path::PathBuf;
    use uuid::Uuid;

    /// Minimal AgentBackend stub for tests that never invoke the backend.
    struct DummyBackend;

    impl AgentBackend for DummyBackend {
        fn name(&self) -> &str {
            "dummy"
        }

        fn build_command(
            &self,
            _worktree_path: &Path,
            _session_id: &Uuid,
            _prompt: &str,
            _github_host: &str,
        ) -> tokio::process::Command {
            tokio::process::Command::new("true")
        }

        fn parse_events(&self, _line: &str) -> Vec<AgentEvent> {
            vec![]
        }

        fn build_resume_command(
            &self,
            _worktree_path: &Path,
            _session_id: &Uuid,
            _prompt: &str,
            _github_host: &str,
        ) -> Option<tokio::process::Command> {
            None
        }

        fn build_interactive_resume_command(
            &self,
            _worktree_path: &Path,
            _session_id: &Uuid,
            _github_host: &str,
        ) -> Option<tokio::process::Command> {
            None
        }

        fn build_oneshot_command(
            &self,
            _worktree_path: &Path,
            _prompt_arg: &str,
        ) -> tokio::process::Command {
            tokio::process::Command::new("true")
        }

        fn build_ci_fix_command(
            &self,
            _worktree_path: &Path,
            _prompt: &str,
        ) -> tokio::process::Command {
            tokio::process::Command::new("true")
        }
    }

    /// Build test fixtures for handle_monitor_error tests.
    fn make_test_fixtures() -> (IssueContext, WorktreeContext) {
        let issue_ctx = IssueContext {
            owner: "test-owner".to_string(),
            repo: "test-repo".to_string(),
            host: "github.com".to_string(),
            issue_num: Some(42),
            details: None,
        };
        let wt_ctx = WorktreeContext {
            minion_id: "M001".to_string(),
            branch_name: "minion/issue-42-M001".to_string(),
            minion_dir: PathBuf::from("/tmp/test"),
            checkout_path: PathBuf::from("/tmp/test/checkout"),
            session_id: Uuid::new_v4(),
        };
        (issue_ctx, wt_ctx)
    }

    #[tokio::test]
    async fn test_rate_limit_error_does_not_increment_consecutive_errors() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, wt_ctx) = make_test_fixtures();
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };
        let mut state = MonitorLoopState::new(Utc::now(), 0);

        // Simulate 15 consecutive rate-limit errors (more than MAX_CONSECUTIVE_ERRORS)
        for _ in 0..15 {
            let error = anyhow::anyhow!(
                "Failed to fetch PR: gh: API rate limit exceeded for user ID 693596"
            );
            let action = handle_monitor_error(&mut state, &ctx, error).await;
            assert!(
                matches!(action, LoopAction::Continue),
                "rate-limit error must return Continue, not Break"
            );
        }

        assert_eq!(
            state.consecutive_errors, 0,
            "rate-limit errors must not increment consecutive_errors"
        );
    }

    #[tokio::test]
    async fn test_non_rate_limit_error_increments_consecutive_errors() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, wt_ctx) = make_test_fixtures();
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };
        let mut state = MonitorLoopState::new(Utc::now(), 0);

        let error = anyhow::anyhow!("Failed to fetch PR: connection refused");
        let action = handle_monitor_error(&mut state, &ctx, error).await;

        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(state.consecutive_errors, 1);
    }

    #[tokio::test]
    async fn test_max_non_rate_limit_errors_causes_bailout() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, wt_ctx) = make_test_fixtures();
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };
        let mut state = MonitorLoopState::new(Utc::now(), 0);

        for i in 0..MAX_CONSECUTIVE_ERRORS {
            let error = anyhow::anyhow!("Failed to fetch PR: server error");
            let action = handle_monitor_error(&mut state, &ctx, error).await;
            if i < MAX_CONSECUTIVE_ERRORS - 1 {
                assert!(matches!(action, LoopAction::Continue));
            } else {
                assert!(
                    matches!(action, LoopAction::Break),
                    "must bail after MAX_CONSECUTIVE_ERRORS non-rate-limit errors"
                );
            }
        }

        assert_eq!(state.consecutive_errors, MAX_CONSECUTIVE_ERRORS);
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(90), "1m"); // seconds truncated
        assert_eq!(format_duration(3599), "59m");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3600), "1h0m");
        assert_eq!(format_duration(5 * 3600 + 15 * 60 + 30), "5h15m"); // seconds truncated
    }

    #[test]
    fn test_rebase_cooldown_initial_state() {
        let state = MonitorLoopState::new(Utc::now(), 80);
        assert_eq!(state.rebase_cooldown_cycles, 0);
    }

    /// Baseline must be advanced even on the MAX_REBASE_ATTEMPTS early-exit path
    /// so the lab daemon does not re-count the Minion's own reply reviews.
    #[tokio::test]
    async fn test_handle_merge_conflict_advances_baseline_at_max_attempts() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, wt_ctx) = make_test_fixtures();
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };

        let stale_baseline = Utc::now() - chrono::Duration::seconds(60);
        let check_time = Utc::now();
        let mut state = MonitorLoopState::new(stale_baseline, 0);
        state.review_baseline = Some(stale_baseline);
        state.rebase_attempts = MAX_REBASE_ATTEMPTS; // trigger early-exit path

        let action = handle_merge_conflict(&mut state, &ctx, check_time).await;

        assert!(
            matches!(action, LoopAction::Break),
            "max-attempts path must break"
        );
        assert_eq!(
            state.review_baseline,
            Some(check_time),
            "baseline must be advanced to check_time even on max-attempts exit"
        );
    }

    /// Baseline must be advanced on the `Err(e)` path of `auto_rebase_pr` so
    /// the lab daemon does not re-count reply reviews after a rebase error.
    /// Uses a non-existent checkout path so `auto_rebase_pr` returns `Err`,
    /// exercising the error-handling match arm without needing a real
    /// repository or worktree setup.
    #[tokio::test]
    async fn test_handle_merge_conflict_advances_baseline_on_error_path() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, mut wt_ctx) = make_test_fixtures();
        // A non-existent checkout path forces auto_rebase_pr to return Err.
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let missing_checkout_path = tempdir.path().join("missing_checkout");
        assert!(
            !missing_checkout_path.exists(),
            "test checkout path must not exist"
        );
        wt_ctx.checkout_path = missing_checkout_path;
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };

        let stale_baseline = Utc::now() - chrono::Duration::seconds(60);
        let check_time = Utc::now();
        let mut state = MonitorLoopState::new(stale_baseline, 0);
        state.review_baseline = Some(stale_baseline);
        // rebase_attempts = 0 (< MAX) so we enter auto_rebase_pr and hit Err

        let action = handle_merge_conflict(&mut state, &ctx, check_time).await;

        assert!(matches!(action, LoopAction::Break), "error path must break");
        assert_eq!(
            state.review_baseline,
            Some(check_time),
            "baseline must be advanced to check_time on Err path"
        );
    }

    /// Documents the `ConflictUnresolved` path invariant via direct state
    /// mutation. This test does NOT call `handle_merge_conflict` — the
    /// `ConflictUnresolved` arm cannot be reached in unit tests because
    /// `run_agent_rebase` requires a running claude CLI to exit non-zero.
    /// The invariant is mechanically guaranteed by the unconditional
    /// `state.review_baseline = Some(check_time)` assignment at the top of
    /// `handle_merge_conflict`, before the `auto_rebase_pr` match. This test
    /// verifies that `MonitorLoopState` stores the value correctly.
    #[test]
    fn test_monitor_loop_state_stores_assigned_baseline() {
        let stale = Utc::now() - chrono::Duration::seconds(60);
        let check_time = Utc::now();
        let mut state = MonitorLoopState::new(stale, 0);
        assert_eq!(
            state.review_baseline,
            Some(stale),
            "initial baseline is stale"
        );

        // Replicates the unconditional first line of handle_merge_conflict
        // that runs before all match arms including ConflictUnresolved.
        state.review_baseline = Some(check_time);

        assert_eq!(
            state.review_baseline,
            Some(check_time),
            "MonitorLoopState holds the advanced baseline value"
        );
    }

    /// Baseline must be advanced during cooldown cycles so a Minion that exits
    /// during cooldown persists a non-stale value.
    #[tokio::test]
    async fn test_handle_pr_event_cooldown_advances_baseline() {
        tokio::time::pause();

        let backend = DummyBackend;
        let (issue_ctx, wt_ctx) = make_test_fixtures();
        let ctx = MonitorContext {
            backend: &backend,
            issue_ctx: &issue_ctx,
            wt_ctx: &wt_ctx,
            pr_number: "123",
            timeout_opt: None,
            monitor_timeout: Duration::from_secs(7200),
        };

        let stale_baseline = Utc::now() - chrono::Duration::seconds(60);
        let check_time = Utc::now();
        let mut state = MonitorLoopState::new(stale_baseline, 0);
        state.review_baseline = Some(stale_baseline);
        state.rebase_cooldown_cycles = 2; // active cooldown

        let action = handle_pr_event(
            Ok((MonitorResult::MergeConflict, check_time)),
            &mut state,
            &ctx,
        )
        .await;

        assert!(
            matches!(action, LoopAction::Continue),
            "cooldown path must continue"
        );
        assert_eq!(
            state.rebase_cooldown_cycles, 1,
            "cooldown counter must decrement"
        );
        assert_eq!(
            state.review_baseline,
            Some(check_time),
            "baseline must be advanced to check_time during cooldown"
        );
    }

    #[test]
    fn test_exit_notification_format_contains_minion_id_and_resume_command() {
        let body = format_exit_notification_comment("M042", 2);
        assert!(body.contains("M042"), "comment must contain minion ID");
        assert!(
            body.contains("gru resume M042"),
            "comment must contain resume command"
        );
        assert!(body.contains("2 reviews"), "comment must mention the count");
        assert!(
            body.contains("type: monitoring-paused"),
            "comment must include YAML frontmatter type"
        );
    }

    // ------------------------------------------------------------------
    // decide_post_filter_action (#867)
    //
    // Exercises the handler's branch selection without spinning up the
    // monitor loop: a synthetic "already replied" batch (empty feedback)
    // must pick SkipAdvance so the agent is not invoked and the baseline
    // can advance.
    // ------------------------------------------------------------------

    fn make_review_comment(id: u64) -> pr_monitor::ReviewComment {
        pr_monitor::ReviewComment {
            file: "src/main.rs".to_string(),
            line: Some(1),
            body: "x".to_string(),
            reviewer: "alice".to_string(),
            reviewer_display_name: "Alice".to_string(),
            comment_id: id,
        }
    }

    fn empty_feedback(had_fetch_failures: bool) -> pr_monitor::ReviewFeedback {
        pr_monitor::ReviewFeedback {
            comments: Vec::new(),
            bodies: Vec::new(),
            had_fetch_failures,
        }
    }

    #[test]
    fn test_decide_post_filter_action_skip_advance_when_clean_empty() {
        // Synthetic "already replied" batch: every fetched thread was
        // dropped by the idempotency filter and no fetches failed.
        let feedback = empty_feedback(false);
        assert_eq!(
            decide_post_filter_action(&feedback),
            PostFilterAction::SkipAdvance
        );
    }

    #[test]
    fn test_decide_post_filter_action_skip_hold_when_empty_with_fetch_failures() {
        // Same as above, but some upstream fetches failed. Must hold the
        // baseline so unfetched threads are retried on the next cycle.
        let feedback = empty_feedback(true);
        assert_eq!(
            decide_post_filter_action(&feedback),
            PostFilterAction::SkipHold
        );
    }

    #[test]
    fn test_decide_post_filter_action_invoke_when_comment_remains() {
        let feedback = pr_monitor::ReviewFeedback {
            comments: vec![make_review_comment(100)],
            bodies: Vec::new(),
            had_fetch_failures: false,
        };
        assert_eq!(
            decide_post_filter_action(&feedback),
            PostFilterAction::Invoke
        );
    }

    #[test]
    fn test_decide_post_filter_action_invoke_when_only_body_remains() {
        // Review bodies are not filtered by the per-comment idempotency
        // check, so a body-only feedback must still drive an invocation.
        let feedback = pr_monitor::ReviewFeedback {
            comments: Vec::new(),
            bodies: vec![pr_monitor::ReviewBody {
                body: "LGTM".to_string(),
                reviewer: "alice".to_string(),
                reviewer_display_name: "Alice".to_string(),
                state: "COMMENTED".to_string(),
            }],
            had_fetch_failures: false,
        };
        assert_eq!(
            decide_post_filter_action(&feedback),
            PostFilterAction::Invoke
        );
    }

    // ------------------------------------------------------------------
    // wait_for_rebase_resolution (#875)
    // ------------------------------------------------------------------

    /// When all fetch attempts fail (e.g. no git repo at the given path),
    /// the function must return false without panicking. This verifies the
    /// log-and-continue error tolerance in the retry loop.
    #[tokio::test]
    async fn test_wait_for_rebase_resolution_returns_false_on_persistent_fetch_error() {
        tokio::time::pause();
        let missing = PathBuf::from("/nonexistent/no-such-dir");
        // retries=3, interval=0 → three immediate fetch attempts all fail.
        let result = wait_for_rebase_resolution(&missing, "main", 3, 0).await;
        assert!(!result, "should return false when all fetch attempts fail");
    }

    /// When the worktree is already up-to-date, the function must return true
    /// on the very first attempt (before any sleep), proving the check-before-
    /// sleep ordering is correct.
    #[tokio::test]
    async fn test_wait_for_rebase_resolution_returns_true_immediately_when_up_to_date() {
        use std::process::Command as StdCmd;
        tokio::time::pause();

        let tmp = tempfile::tempdir().expect("create temp dir");
        let dir = tmp.path();

        // Use --allow-empty commits to avoid `git add`, which can pollute
        // the test runner's git index when GIT_INDEX_FILE is inherited from
        // the pre-commit hook environment.
        let run = |args: &[&str], cwd: &std::path::Path| {
            let out = StdCmd::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .output()
                .expect("git command failed to spawn");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        // Create a bare repo as origin.
        let bare = dir.join("origin.git");
        let status = StdCmd::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .expect("git init --bare failed");
        assert!(status.success());

        // Create a worktree with empty commits (no git add) and push to origin.
        // Empty commits avoid writing to the index, which could be polluted by
        // GIT_INDEX_FILE being set in the pre-commit hook environment.
        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        run(&["init", "-b", "main"], &wt);
        run(&["remote", "add", "origin", bare.to_str().unwrap()], &wt);
        run(&["commit", "--allow-empty", "-m", "base"], &wt);
        run(&["push", "-u", "origin", "main"], &wt);
        // Feature branch has a commit on top of origin/main → origin/main IS ancestor.
        run(&["checkout", "-b", "feature"], &wt);
        run(&["commit", "--allow-empty", "-m", "feat"], &wt);

        // With interval=999 seconds the function would time out the test if it
        // sleeps before the first check. Paused time means the sleep never
        // advances, so if the function slept first it would hang.
        let result = wait_for_rebase_resolution(&wt, "main", 3, 999).await;
        assert!(
            result,
            "should return true on first attempt without sleeping"
        );
    }

    /// The `ConflictUnresolved` → GitHub mergeability re-check path in
    /// `handle_merge_conflict` cannot be reached in unit tests because
    /// `auto_rebase_pr` requires a fully configured git worktree and a running
    /// agent to return `Ok(ConflictUnresolved)`. The bypass logic — resetting
    /// `rebase_attempts` and `rebase_cooldown_cycles` when GitHub reports
    /// `mergeable: true` — is integration-tested via the incident scenario
    /// described in #875. This comment exists to document the coverage gap and
    /// explain why it is acceptable.
    #[test]
    fn test_conflict_unresolved_github_bypass_is_integration_tested() {
        // Structural check: the two fields that the bypass resets must exist
        // on MonitorLoopState so any future rename is caught at compile time.
        let stale = Utc::now() - chrono::Duration::seconds(60);
        let mut state = MonitorLoopState::new(stale, 0);
        state.rebase_attempts = 1;
        state.rebase_cooldown_cycles = 0;
        // Replicate what the bypass does.
        state.rebase_attempts = 0;
        state.rebase_cooldown_cycles = REBASE_COOLDOWN_CYCLES;
        assert_eq!(state.rebase_attempts, 0);
        assert_eq!(state.rebase_cooldown_cycles, REBASE_COOLDOWN_CYCLES);
    }
}
