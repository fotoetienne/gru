use crate::ci::CheckRun;
use crate::github;
use crate::github::DEFAULT_MAX_RETRIES;
use crate::labels;
use crate::merge_readiness;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::process::Output;
use tokio::time::{sleep, Duration, Instant};

const POLL_INTERVAL_SECS: u64 = 30;

/// Alias for the shared retry helper in `github.rs`.
async fn gh_api_with_retry(host: &str, args: &[&str], max_retries: u32) -> Result<Output> {
    github::gh_api_with_retry(host, args, max_retries).await
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    state: String,
    merged: bool,
    head: Head,
    user: User,
    /// GitHub's mergeable field: true, false, or null (still computing).
    mergeable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Head {
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Review {
    id: u64,
    pub(crate) submitted_at: DateTime<Utc>,
    pub(crate) user: User,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    pub(crate) login: String,
}

/// A review comment with file location and content
#[derive(Debug, Clone)]
pub(crate) struct ReviewComment {
    pub(crate) file: String,
    pub(crate) line: Option<u64>,
    pub(crate) body: String,
    pub(crate) reviewer: String,
    pub(crate) comment_id: u64,
}

/// A review-level body (not tied to a specific file/line)
#[derive(Debug, Clone)]
pub(crate) struct ReviewBody {
    pub body: String,
    pub reviewer: String,
    pub state: String,
}

/// All feedback from reviews: inline comments + review bodies
#[derive(Debug, Clone)]
pub(crate) struct ReviewFeedback {
    pub comments: Vec<ReviewComment>,
    pub bodies: Vec<ReviewBody>,
    /// True when one or more review comment fetches failed.  The caller
    /// should avoid advancing the baseline timestamp so the reviews can
    /// be retried on the next poll cycle.
    pub had_fetch_failures: bool,
}

impl ReviewFeedback {
    pub(crate) fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.bodies.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct ApiReviewComment {
    id: u64,
    path: String,
    line: Option<u64>,
    body: String,
    user: User,
}

/// Check if a CI check run conclusion indicates a failure.
///
/// Failed states include: failure, cancelled, timed_out, action_required.
/// Non-failed states include: success, skipped, neutral, and in-progress (None).
fn is_failed_check(check_run: &CheckRun) -> bool {
    check_run.conclusion.as_ref().is_some_and(|c| c.is_failed())
}

/// Count failed checks only when all runs have completed.
///
/// Returns `Some(count)` if every check has `status == Completed` and at least
/// one has a failed conclusion. Returns `None` if any check is still running
/// or if all checks passed.
///
/// Note: `Iterator::all` returns `true` for an empty iterator (no checks
/// registered yet). In that case `failed == 0`, so we return `None` and the
/// outer monitor loop re-polls on the next cycle.
fn count_completed_failures(check_runs: &[CheckRun]) -> Option<usize> {
    let all_completed = check_runs
        .iter()
        .all(|c| c.status == crate::ci::CheckStatus::Completed);
    if all_completed {
        let failed = check_runs.iter().filter(|c| is_failed_check(c)).count();
        if failed > 0 {
            return Some(failed);
        }
    }
    None
}

const READY_TO_MERGE_LABEL: &str = labels::READY_TO_MERGE;
const AUTO_MERGE_LABEL: &str = labels::AUTO_MERGE;

/// Ensure a label exists in the repository with canonical color/description.
///
/// Uses `gh label create --force` which creates the label if missing or updates
/// it if it already exists, keeping labels consistent with their definitions.
async fn ensure_label_exists(host: &str, owner: &str, repo: &str, label_name: &str) -> Result<()> {
    let (color, description) =
        labels::get_label_info(label_name).expect("label must be in ALL_LABELS");
    if let Err(e) =
        github::create_label_via_cli(host, owner, repo, label_name, color, description).await
    {
        log::warn!("Failed to create {} label: {}", label_name, e);
    }
    Ok(())
}

/// Ensure the `gru:ready-to-merge` label exists in the repository, creating it if needed.
pub(crate) async fn ensure_ready_to_merge_label(host: &str, owner: &str, repo: &str) -> Result<()> {
    ensure_label_exists(host, owner, repo, READY_TO_MERGE_LABEL).await
}

/// Check if a PR currently has a specific label.
async fn has_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    label_name: &str,
) -> Result<bool> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch labels for PR #{}: {}", pr_number, stderr);
    }

    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let fetched_labels: Vec<Label> =
        serde_json::from_slice(&output.stdout).context("Failed to parse labels JSON")?;
    Ok(fetched_labels.iter().any(|l| l.name == label_name))
}

/// Check if a PR currently has the `ready-to-merge` label.
async fn has_ready_to_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<bool> {
    has_label(host, owner, repo, pr_number, READY_TO_MERGE_LABEL).await
}

/// Check if a PR currently has the `gru:auto-merge` label.
async fn has_auto_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<bool> {
    has_label(host, owner, repo, pr_number, AUTO_MERGE_LABEL).await
}

/// Ensure the `gru:auto-merge` label exists in the repository, creating it if needed.
pub(crate) async fn ensure_auto_merge_label(host: &str, owner: &str, repo: &str) -> Result<()> {
    ensure_label_exists(host, owner, repo, AUTO_MERGE_LABEL).await
}

/// Add the `gru:auto-merge` label to a PR.
pub(crate) async fn add_auto_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = github::repo_slug(owner, repo);
    github::run_gh(
        host,
        &[
            "pr",
            "edit",
            pr_number,
            "--add-label",
            AUTO_MERGE_LABEL,
            "-R",
            &repo_full,
        ],
    )
    .await?;

    Ok(())
}

/// Add the `gru:ready-to-merge` label to a PR.
async fn add_ready_to_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");
    let output = gh_api_with_retry(
        host,
        &[
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &format!("labels[]={READY_TO_MERGE_LABEL}"),
        ],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to add ready-to-merge label to PR #{}: {}",
            pr_number,
            stderr
        );
    }

    Ok(())
}

/// Remove the `gru:ready-to-merge` label from a PR.
async fn remove_ready_to_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = github::repo_slug(owner, repo);

    let label_encoded = READY_TO_MERGE_LABEL.replace(':', "%3A");
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels/{label_encoded}");
    let output = gh_api_with_retry(
        host,
        &["api", &endpoint, "-X", "DELETE"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("404") && !stderr.contains("Not Found") {
            anyhow::bail!(
                "Failed to remove {} label from PR #{}: {}",
                READY_TO_MERGE_LABEL,
                pr_number,
                stderr
            );
        }
    }

    Ok(())
}

/// Check merge readiness using the unified module and update the `ready-to-merge` label.
///
/// Tracks transitions: adds label when becoming ready, removes when regressing.
/// Returns `Some(readiness)` if all readiness checks pass, or `None` if not ready
/// or if the readiness check failed. The `was_ready` bool is updated in place.
async fn update_readiness_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    pr_number_u64: u64,
    was_ready: &mut bool,
) -> Option<merge_readiness::MergeReadiness> {
    let readiness =
        match merge_readiness::check_merge_readiness(host, owner, repo, pr_number_u64).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed to check merge readiness: {}", e);
                return None;
            }
        };

    let is_ready = readiness.is_ready();

    if is_ready && !*was_ready {
        // Transition: not ready → ready
        match add_ready_to_merge_label(host, owner, repo, pr_number).await {
            Ok(()) => println!("✅ PR #{} is ready to merge", pr_number),
            Err(e) => log::warn!("Failed to add ready-to-merge label: {}", e),
        }
    } else if !is_ready && *was_ready {
        // Transition: ready → not ready
        let reason = readiness.failure_reasons().join(", ");
        match remove_ready_to_merge_label(host, owner, repo, pr_number).await {
            Ok(()) => println!(
                "⚠️  PR #{} is no longer ready to merge ({})",
                pr_number, reason
            ),
            Err(e) => log::warn!("Failed to remove ready-to-merge label: {}", e),
        }
    }

    *was_ready = is_ready;

    if is_ready {
        Some(readiness)
    } else {
        None
    }
}

#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRun>,
}

/// Monitor a PR for actionable events (reviews, CI failures, merge conflicts, etc.).
///
/// `baseline` optionally provides a timestamp to seed the review tracker.
/// When `Some`, reviews posted after that time will be detected on the first poll,
/// which lets the caller preserve review tracking across re-entries (e.g. after a
/// rebase). When `None`, `last_check_time` is seeded to `Utc::now()` so that
/// pre-existing reviews aren't re-detected.
///
/// Returns `(MonitorResult, DateTime<Utc>)` where the second element is the
/// internal `last_check_time` at the time the event was detected. Callers
/// should pass this value as `baseline` on the next invocation so that:
/// - Already-handled reviews are not re-fetched.
/// - Reviews posted during event handling (rebase, review response) are caught.
pub(crate) async fn monitor_pr(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    max_duration: Option<Duration>,
    baseline: Option<DateTime<Utc>>,
) -> Result<(MonitorResult, DateTime<Utc>)> {
    let start_time = Instant::now();

    let mut last_check_time = baseline.unwrap_or_else(Utc::now);

    // Track merge-readiness state across polls to detect transitions.
    // Seed from the current label state so we don't add/remove on first poll.
    let mut was_ready = has_ready_to_merge_label(host, owner, repo, pr_number)
        .await
        .unwrap_or(false);

    // Register the Ctrl+C listener once to avoid signal loss between iterations
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        // Check if we've exceeded the maximum duration
        if let Some(max) = max_duration {
            let elapsed = start_time.elapsed();
            if elapsed >= max {
                return Ok((MonitorResult::Timeout, last_check_time));
            }
        }

        // Race the polling iteration against Ctrl+C
        tokio::select! {
            result = poll_once(host, owner, repo, pr_number, &mut last_check_time, &mut was_ready) => {
                if let Some(monitor_result) = result? {
                    return Ok((monitor_result, last_check_time));
                }
            }
            _ = &mut ctrl_c => {
                return Ok((MonitorResult::Interrupted, last_check_time));
            }
        }

        // Sleep between polls, still responding to Ctrl+C
        tokio::select! {
            _ = sleep(Duration::from_secs(POLL_INTERVAL_SECS)) => {}
            _ = &mut ctrl_c => {
                return Ok((MonitorResult::Interrupted, last_check_time));
            }
        }
    }
}

/// Check if a PR has reached a terminal state (merged or closed).
///
/// Returns `Some(MonitorResult::Merged)` for merged PRs, `Some(MonitorResult::Closed)`
/// for closed-but-not-merged PRs, or `None` if the PR is still open.
pub(crate) fn determine_pr_terminal_state(state: &str, merged: bool) -> Option<MonitorResult> {
    if state == "closed" {
        if merged {
            Some(MonitorResult::Merged)
        } else {
            Some(MonitorResult::Closed)
        }
    } else {
        None
    }
}

/// Filter reviews to only include those submitted at or after `since` by users
/// other than the PR author.
///
/// This excludes self-reviews (to prevent feedback loops) and old reviews
/// (already processed in a previous poll cycle). Uses inclusive `>=` so that
/// a review landing exactly at `since` is captured; contrast with
/// `count_unaddressed_reviews` which uses exclusive `>` for a different purpose.
pub(crate) fn filter_new_external_reviews(
    reviews: &[Review],
    since: DateTime<Utc>,
    pr_author: &str,
) -> Vec<Review> {
    reviews
        .iter()
        .filter(|r| r.submitted_at >= since && r.user.login != pr_author)
        .cloned()
        .collect()
}

/// Perform a single polling iteration: check PR state, reviews, CI, and merge readiness.
///
/// Returns `Ok(Some(result))` if an actionable event was detected,
/// or `Ok(None)` if nothing happened and the caller should sleep and retry.
async fn poll_once(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    last_check_time: &mut DateTime<Utc>,
    was_ready: &mut bool,
) -> Result<Option<MonitorResult>> {
    // Fetch PR state
    let pr = get_pr(host, owner, repo, pr_number).await?;

    // Check terminal states - merged PRs are also in "closed" state
    // Must check merged flag first to distinguish merged from just closed
    if let Some(result) = determine_pr_terminal_state(&pr.state, pr.merged) {
        return Ok(Some(result));
    }

    // Fetch all reviews once, then filter for new ones to avoid a double API call.
    // The full list is reused below for merge-readiness evaluation.
    // Check for new reviews BEFORE merge conflicts so that reviewer feedback
    // is never silently dropped when conflicts and reviews overlap.
    let all_reviews = get_all_reviews(host, owner, repo, pr_number).await?;
    let pr_author = pr.user.login.as_str();
    let new_reviews = filter_new_external_reviews(&all_reviews, *last_check_time, pr_author);
    if !new_reviews.is_empty() {
        let feedback = get_review_feedback(host, owner, repo, pr_number, &new_reviews).await?;
        // Only advance the baseline when we successfully fetched all reviews.
        // If some fetches failed we leave last_check_time unchanged so the
        // reviews are retried on the next poll cycle.
        if !feedback.had_fetch_failures {
            *last_check_time = Utc::now();
        }
        // Only emit NewReviews if there is actual feedback to act on.
        // DISMISSED reviews or reviews with empty bodies and no inline
        // comments can produce an empty ReviewFeedback.
        if !feedback.is_empty() {
            return Ok(Some(MonitorResult::NewReviews(feedback)));
        }
    }

    // Check merge conflict status.
    // mergeable == Some(false) means GitHub detected conflicts.
    // mergeable == None means GitHub is still computing — skip and re-check next cycle.
    if pr.mergeable == Some(false) {
        return Ok(Some(MonitorResult::MergeConflict));
    }

    // Check for failed CI runs - only report failures when all checks have completed.
    // If any checks are still queued or in progress, skip and re-check next cycle.
    let check_runs = get_check_runs(host, owner, repo, &pr.head.sha).await?;
    if let Some(failed_checks) = count_completed_failures(&check_runs) {
        // Advance the review baseline so reviews are not missed when monitor_pr
        // is re-entered after CI handling in the lifecycle loop.
        *last_check_time = Utc::now();
        return Ok(Some(MonitorResult::FailedChecks(failed_checks)));
    }

    // Check merge readiness (via unified module) and update label on transitions.
    let pr_number_u64: u64 = match pr_number.parse() {
        Ok(n) => n,
        Err(_) => {
            log::warn!(
                "Could not parse PR number '{}', skipping readiness check",
                pr_number
            );
            *last_check_time = Utc::now();
            return Ok(None);
        }
    };
    if update_readiness_label(host, owner, repo, pr_number, pr_number_u64, was_ready)
        .await
        .is_some()
    {
        // PR is ready — check if gru:auto-merge label is present
        match has_auto_merge_label(host, owner, repo, pr_number).await {
            Ok(true) => {
                *last_check_time = Utc::now();
                return Ok(Some(MonitorResult::ReadyToMerge));
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!(
                    "Failed to check gru:auto-merge label on PR #{}: {}",
                    pr_number,
                    e
                );
            }
        }
    }

    // Update last check time
    *last_check_time = Utc::now();
    Ok(None)
}

/// Result of monitoring a PR
#[derive(Debug)]
pub(crate) enum MonitorResult {
    /// PR was successfully merged
    Merged,
    /// PR was closed without merging
    Closed,
    /// New review feedback detected (inline comments and/or review bodies)
    NewReviews(ReviewFeedback),
    /// CI checks failed (count)
    FailedChecks(usize),
    /// PR has merge conflicts (mergeable: false)
    MergeConflict,
    /// All readiness checks pass and `gru:auto-merge` label is present
    ReadyToMerge,
    /// Monitoring timed out after the configured duration
    Timeout,
    /// Monitoring was interrupted by the user (e.g., Ctrl+C)
    Interrupted,
}

/// Fetch PR details using gh CLI with retry logic for transient failures
async fn get_pr(host: &str, owner: &str, repo: &str, pr_number: &str) -> Result<PullRequest> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR: {}", stderr);
    }

    let pr: PullRequest =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON response")?;

    Ok(pr)
}

/// Fetch all reviews for a PR with retry logic for transient failures
pub(crate) async fn get_all_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<Review>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch reviews for PR #{}: {}", pr_number, stderr);
    }

    let reviews: Vec<Review> =
        serde_json::from_slice(&output.stdout).context("Failed to parse reviews JSON response")?;

    Ok(reviews)
}

/// Fetch review feedback (inline comments + review bodies) for specific reviews
/// with retry logic for transient failures.
async fn get_review_feedback(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    reviews: &[Review],
) -> Result<ReviewFeedback> {
    let repo_full = github::repo_slug(owner, repo);
    let mut all_comments = Vec::new();
    let mut all_bodies = Vec::new();
    let mut failed_reviews = 0;

    for review in reviews {
        // Collect review body text (non-empty bodies that aren't just whitespace).
        // Skip DISMISSED reviews — their body is the dismissal reason, not
        // actionable feedback for the implementer.
        let state = review.state.as_deref().unwrap_or("COMMENTED");
        if state != "DISMISSED" {
            if let Some(ref body) = review.body {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    all_bodies.push(ReviewBody {
                        body: trimmed.to_string(),
                        reviewer: review.user.login.clone(),
                        state: state.to_string(),
                    });
                }
            }
        }

        // Fetch inline comments for this specific review with retry
        let endpoint = format!(
            "repos/{repo_full}/pulls/{pr_number}/reviews/{}/comments",
            review.id
        );
        let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Log error but continue processing other reviews
            log::warn!(
                "Warning: Failed to fetch comments for review {}: {}",
                review.id,
                stderr
            );
            failed_reviews += 1;
            continue;
        }

        let api_comments: Vec<ApiReviewComment> = serde_json::from_slice(&output.stdout)
            .context("Failed to parse review comments JSON response")?;

        // Convert API comments to our format
        for comment in api_comments {
            all_comments.push(ReviewComment {
                file: comment.path,
                line: comment.line,
                body: comment.body,
                reviewer: comment.user.login,
                comment_id: comment.id,
            });
        }
    }

    // Log summary if any reviews failed to fetch
    if failed_reviews > 0 {
        log::warn!(
            "⚠️  Failed to fetch comments from {} out of {} review(s)",
            failed_reviews,
            reviews.len()
        );
    }

    Ok(ReviewFeedback {
        comments: all_comments,
        bodies: all_bodies,
        had_fetch_failures: failed_reviews > 0,
    })
}

/// Format review feedback (bodies + inline comments) into a prompt for Claude
pub(crate) fn format_review_prompt(
    issue_num: Option<u64>,
    pr_number: &str,
    feedback: &ReviewFeedback,
    owner: &str,
    repo: &str,
    minion_id: &str,
) -> String {
    let preamble = match issue_num {
        Some(n) => format!(
            "You previously implemented a fix for issue #{}. Review feedback has been provided \
            on PR #{}.",
            n, pr_number
        ),
        None => format!("Review feedback has been provided on PR #{}.", pr_number),
    };
    let mut prompt = format!("{} Please address the following feedback:\n\n", preamble);

    // Include review bodies (top-level review feedback not tied to specific lines)
    for (i, review_body) in feedback.bodies.iter().enumerate() {
        prompt.push_str(&format!("## Review {} ({})\n", i + 1, review_body.state));
        prompt.push_str(&format!("**Reviewer:** @{}\n", review_body.reviewer));
        prompt.push_str(&format!("**Feedback:**\n{}\n\n", review_body.body));
    }

    // Include inline comments (file/line-specific feedback)
    if !feedback.comments.is_empty() {
        if !feedback.bodies.is_empty() {
            prompt.push_str("---\n\n");
        }
        for (i, comment) in feedback.comments.iter().enumerate() {
            prompt.push_str(&format!("## Inline Comment {}\n", i + 1));
            prompt.push_str(&format!("**File:** {}", comment.file));
            if let Some(line) = comment.line {
                prompt.push_str(&format!(":{}", line));
            }
            prompt.push('\n');
            prompt.push_str(&format!("**Reviewer:** @{}\n", comment.reviewer));
            prompt.push_str(&format!("**Comment ID:** {}\n", comment.comment_id));
            prompt.push_str(&format!("**Comment:** {}\n\n", comment.body));
        }
    }

    prompt.push_str("Please make the requested changes, run tests, and commit.\n\n");

    // Instruct the agent to reply to each inline review comment thread
    if !feedback.comments.is_empty() {
        prompt.push_str(&format!(
            "After committing your changes, reply to EACH inline review comment thread to explain what you changed. \
For each comment, post an inline reply using the GitHub API:\n\n\
```\n\
gh api --method POST repos/{owner}/{repo}/pulls/{pr_number}/comments \\\n  \
-f body=$'<reply text>\\n\\n<sub>🤖 {minion_id}</sub>' \\\n  \
-F in_reply_to=<comment_id>\n\
```\n\n\
Where `<comment_id>` is the Comment ID listed above for each inline review comment. \
Each reply must:\n\
- Summarize what was changed to address the feedback\n\
- End with the signature: `\\n\\n<sub>🤖 {minion_id}</sub>`\n"
        ));
    }

    prompt
}

/// Fetch check runs for a given commit SHA with retry logic for transient failures
async fn get_check_runs(host: &str, owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/commits/{sha}/check-runs");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch check runs: {}", stderr);
    }

    let response: CheckRunsResponse = serde_json::from_slice(&output.stdout)
        .context("Failed to parse check runs JSON response")?;

    Ok(response.check_runs)
}

/// Returns the count of external (non-author) reviews submitted after `since`.
///
/// "Unaddressed" means: submitted after the baseline and not from the PR author.
/// Used to decide whether to post a monitoring-paused notification on exit.
pub(crate) fn has_unaddressed_reviews(
    reviews: &[Review],
    pr_author: &str,
    since: DateTime<Utc>,
) -> usize {
    reviews
        .iter()
        .filter(|r| r.submitted_at > since && r.user.login != pr_author)
        .count()
}

/// Returns true if a monitoring-paused notification should be posted.
///
/// Notification is suppressed when the PR is closed/merged (`pr_open == false`)
/// or when there are no unaddressed external reviews.
pub(crate) fn should_post_exit_notification(pr_open: bool, unaddressed_count: usize) -> bool {
    pr_open && unaddressed_count > 0
}

/// Fetch just the fields needed for exit-notification decisions without
/// exposing internal types to callers.
///
/// Returns `(is_open, pr_author_login)`.
pub(crate) async fn get_pr_info_for_exit_notification(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<(bool, String)> {
    let pr = get_pr(host, owner, repo, pr_number).await?;
    // A merged PR has state="closed" AND merged=true. Check both to guard
    // against the narrow race where state hasn't propagated yet.
    let is_open = pr.state != "closed" && !pr.merged;
    Ok((is_open, pr.user.login))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_review_prompt_single_comment() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/main.rs".to_string(),
                line: Some(45),
                body: "This function needs error handling for null inputs.".to_string(),
                reviewer: "alice".to_string(),
                comment_id: 1001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(123), "456", &feedback, "octocat", "hello-world", "M042");

        assert!(prompt.contains("issue #123"));
        assert!(prompt.contains("PR #456"));
        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**Reviewer:** @alice"));
        assert!(prompt.contains("**Comment ID:** 1001"));
        assert!(prompt.contains("This function needs error handling for null inputs."));
        assert!(prompt.contains("Please make the requested changes, run tests, and commit."));
        assert!(prompt.contains("in_reply_to"));
        assert!(prompt.contains("repos/octocat/hello-world/pulls/456/comments"));
        assert!(prompt.contains("<sub>🤖 M042</sub>"));
    }

    #[test]
    fn test_format_review_prompt_multiple_comments() {
        let feedback = ReviewFeedback {
            comments: vec![
                ReviewComment {
                    file: "src/main.rs".to_string(),
                    line: Some(45),
                    body: "Add error handling.".to_string(),
                    reviewer: "alice".to_string(),
                    comment_id: 2001,
                },
                ReviewComment {
                    file: "tests/test_main.rs".to_string(),
                    line: Some(12),
                    body: "Add a test case for the edge case.".to_string(),
                    reviewer: "bob".to_string(),
                    comment_id: 2002,
                },
            ],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(123), "456", &feedback, "octocat", "hello-world", "M042");

        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("## Inline Comment 2"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**File:** tests/test_main.rs:12"));
        assert!(prompt.contains("@alice"));
        assert!(prompt.contains("@bob"));
        assert!(prompt.contains("**Comment ID:** 2001"));
        assert!(prompt.contains("**Comment ID:** 2002"));
        assert!(prompt.contains("in_reply_to"));
        assert!(prompt.contains("End with the signature"));
    }

    #[test]
    fn test_format_review_prompt_no_line_number() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "README.md".to_string(),
                line: None,
                body: "Update the documentation.".to_string(),
                reviewer: "charlie".to_string(),
                comment_id: 3001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(123), "456", &feedback, "octocat", "hello-world", "M042");

        // Should not have a colon if line is None
        assert!(prompt.contains("**File:** README.md\n"));
        assert!(!prompt.contains("README.md:"));
    }

    #[test]
    fn test_format_review_prompt_body_only() {
        let feedback = ReviewFeedback {
            comments: vec![],
            bodies: vec![ReviewBody {
                body: "Consider refactoring the error handling to use Result types.".to_string(),
                reviewer: "dave".to_string(),
                state: "COMMENTED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(42), "99", &feedback, "octocat", "hello-world", "M001");

        assert!(prompt.contains("issue #42"));
        assert!(prompt.contains("PR #99"));
        assert!(prompt.contains("## Review 1 (COMMENTED)"));
        assert!(prompt.contains("**Reviewer:** @dave"));
        assert!(prompt.contains("Consider refactoring the error handling"));
        assert!(prompt.contains("Please make the requested changes"));
        // No inline comments, so no in_reply_to instructions
        assert!(!prompt.contains("in_reply_to"));
    }

    #[test]
    fn test_format_review_prompt_body_and_comments() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/lib.rs".to_string(),
                line: Some(10),
                body: "Fix this line.".to_string(),
                reviewer: "eve".to_string(),
                comment_id: 6001,
            }],
            bodies: vec![ReviewBody {
                body: "Overall looks good but needs some tweaks.".to_string(),
                reviewer: "eve".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(10), "20", &feedback, "octocat", "hello-world", "M002");

        // Review body appears
        assert!(prompt.contains("## Review 1 (CHANGES_REQUESTED)"));
        assert!(prompt.contains("Overall looks good"));
        // Inline comment appears
        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("**File:** src/lib.rs:10"));
        assert!(prompt.contains("Fix this line."));
        // Reply instructions present for inline comments
        assert!(prompt.contains("in_reply_to"));
    }

    // ========================================================================
    // JSON Deserialization Tests
    // ========================================================================

    #[test]
    fn test_pull_request_deserialize_merged() {
        let json = r#"{
            "state": "closed",
            "merged": true,
            "head": {
                "sha": "abc123def456"
            },
            "user": {"login": "author"}
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.state, "closed");
        assert!(pr.merged);
        assert_eq!(pr.head.sha, "abc123def456");
    }

    #[test]
    fn test_pull_request_deserialize_closed_not_merged() {
        let json = r#"{
            "state": "closed",
            "merged": false,
            "head": {
                "sha": "abc123def456"
            },
            "user": {"login": "author"}
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.state, "closed");
        assert!(!pr.merged);
    }

    #[test]
    fn test_pull_request_deserialize_open() {
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {
                "sha": "abc123def456"
            },
            "user": {"login": "author"}
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.state, "open");
        assert!(!pr.merged);
    }

    #[test]
    fn test_pull_request_deserialize_with_extra_fields() {
        // Real API responses contain many more fields - ensure we handle them gracefully
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {
                "sha": "abc123def456",
                "ref": "feature-branch",
                "repo": {"full_name": "owner/repo"}
            },
            "title": "My PR",
            "number": 42,
            "user": {"login": "author"},
            "created_at": "2024-01-01T00:00:00Z"
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.state, "open");
        assert!(!pr.merged);
        assert_eq!(pr.head.sha, "abc123def456");
        assert_eq!(pr.user.login, "author");
    }

    #[test]
    fn test_review_deserialize() {
        let json = r#"{
            "id": 12345,
            "submitted_at": "2024-06-15T10:30:00Z",
            "user": {
                "login": "reviewer"
            }
        }"#;

        let review: Review = serde_json::from_str(json).unwrap();
        assert_eq!(review.id, 12345);
        assert_eq!(review.user.login, "reviewer");
    }

    #[test]
    fn test_review_deserialize_with_extra_fields() {
        let json = r#"{
            "id": 12345,
            "submitted_at": "2024-06-15T10:30:00Z",
            "user": {
                "login": "reviewer",
                "id": 999,
                "avatar_url": "https://example.com/avatar.png"
            },
            "state": "CHANGES_REQUESTED",
            "body": "Please fix this",
            "html_url": "https://github.com/owner/repo/pull/1#pullrequestreview-12345"
        }"#;

        let review: Review = serde_json::from_str(json).unwrap();
        assert_eq!(review.id, 12345);
        assert_eq!(review.user.login, "reviewer");
    }

    #[test]
    fn test_review_list_deserialize() {
        let json = r#"[
            {
                "id": 1,
                "submitted_at": "2024-06-15T10:30:00Z",
                "user": {"login": "alice"}
            },
            {
                "id": 2,
                "submitted_at": "2024-06-15T11:30:00Z",
                "user": {"login": "bob"}
            }
        ]"#;

        let reviews: Vec<Review> = serde_json::from_str(json).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].id, 1);
        assert_eq!(reviews[0].user.login, "alice");
        assert_eq!(reviews[1].id, 2);
        assert_eq!(reviews[1].user.login, "bob");
    }

    #[test]
    fn test_review_list_empty() {
        let json = "[]";
        let reviews: Vec<Review> = serde_json::from_str(json).unwrap();
        assert!(reviews.is_empty());
    }

    #[test]
    fn test_check_run_deserialize_failure() {
        use crate::ci::CheckConclusion;
        let json = r#"{
            "conclusion": "failure"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some(CheckConclusion::Failure));
    }

    #[test]
    fn test_check_run_deserialize_success() {
        use crate::ci::CheckConclusion;
        let json = r#"{
            "conclusion": "success"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some(CheckConclusion::Success));
    }

    #[test]
    fn test_check_run_deserialize_null_conclusion() {
        // In-progress checks have null conclusion
        let json = r#"{
            "conclusion": null
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert!(check.conclusion.is_none());
    }

    #[test]
    fn test_check_run_deserialize_with_extra_fields() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let json = r#"{
            "id": 123456,
            "name": "build",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2024-06-15T10:00:00Z",
            "completed_at": "2024-06-15T10:05:00Z"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some(CheckConclusion::Success));
        assert_eq!(check.name, "build");
        assert_eq!(check.status, CheckStatus::Completed);
    }

    #[test]
    fn test_check_run_deserialize_in_progress_status() {
        use crate::ci::CheckStatus;
        let json = r#"{"status": "in_progress", "conclusion": null}"#;
        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.status, CheckStatus::InProgress);
        assert!(check.conclusion.is_none());
    }

    #[test]
    fn test_check_run_deserialize_unknown_conclusion() {
        use crate::ci::CheckConclusion;
        let json = r#"{"conclusion": "some_future_value"}"#;
        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some(CheckConclusion::Unknown));
        assert!(!is_failed_check(&check));
    }

    #[test]
    fn test_check_runs_response_deserialize() {
        use crate::ci::CheckConclusion;
        let json = r#"{
            "total_count": 3,
            "check_runs": [
                {"conclusion": "success"},
                {"conclusion": "failure"},
                {"conclusion": null}
            ]
        }"#;

        let response: CheckRunsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.check_runs.len(), 3);
        assert_eq!(
            response.check_runs[0].conclusion,
            Some(CheckConclusion::Success)
        );
        assert_eq!(
            response.check_runs[1].conclusion,
            Some(CheckConclusion::Failure)
        );
        assert!(response.check_runs[2].conclusion.is_none());
    }

    #[test]
    fn test_check_runs_response_empty() {
        let json = r#"{
            "total_count": 0,
            "check_runs": []
        }"#;

        let response: CheckRunsResponse = serde_json::from_str(json).unwrap();
        assert!(response.check_runs.is_empty());
    }

    #[test]
    fn test_api_review_comment_deserialize() {
        let json = r#"{
            "id": 100,
            "path": "src/main.rs",
            "line": 42,
            "body": "This needs refactoring",
            "user": {"login": "reviewer"}
        }"#;

        let comment: ApiReviewComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.path, "src/main.rs");
        assert_eq!(comment.line, Some(42));
        assert_eq!(comment.body, "This needs refactoring");
        assert_eq!(comment.user.login, "reviewer");
    }

    #[test]
    fn test_api_review_comment_deserialize_null_line() {
        // File-level comments don't have a line number
        let json = r#"{
            "id": 200,
            "path": "README.md",
            "line": null,
            "body": "Update documentation",
            "user": {"login": "reviewer"}
        }"#;

        let comment: ApiReviewComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.path, "README.md");
        assert!(comment.line.is_none());
    }

    // ========================================================================
    // PR State Detection Tests
    // ========================================================================

    #[test]
    fn test_merged_pr_returns_merged_result() {
        let result = determine_pr_terminal_state("closed", true);
        assert!(matches!(result, Some(MonitorResult::Merged)));
    }

    #[test]
    fn test_closed_not_merged_returns_closed_result() {
        let result = determine_pr_terminal_state("closed", false);
        assert!(matches!(result, Some(MonitorResult::Closed)));
    }

    #[test]
    fn test_open_pr_returns_none() {
        let result = determine_pr_terminal_state("open", false);
        assert!(result.is_none());
    }

    #[test]
    fn test_merged_takes_precedence_over_closed() {
        // A merged PR has state="closed" AND merged=true
        // Verify that merged is detected, not just closed
        let result = determine_pr_terminal_state("closed", true);
        assert!(
            matches!(result, Some(MonitorResult::Merged)),
            "Merged PR should return Merged, not Closed"
        );
    }

    // ========================================================================
    // CI Check Failure Detection Tests
    // ========================================================================
    // These tests use the shared is_failed_check function from production code

    /// Helper to create a CheckRun with only a conclusion (defaults to Queued status)
    fn make_check(conclusion: Option<crate::ci::CheckConclusion>) -> CheckRun {
        CheckRun {
            name: String::new(),
            status: Default::default(),
            conclusion,
            duration: None,
            output: None,
        }
    }

    /// Helper to create a CheckRun with explicit status and conclusion
    fn make_check_with_status(
        status: crate::ci::CheckStatus,
        conclusion: Option<crate::ci::CheckConclusion>,
    ) -> CheckRun {
        CheckRun {
            name: String::new(),
            status,
            conclusion,
            duration: None,
            output: None,
        }
    }

    #[test]
    fn test_failed_check_detection_failure() {
        use crate::ci::CheckConclusion;
        assert!(is_failed_check(&make_check(Some(CheckConclusion::Failure))));
    }

    #[test]
    fn test_failed_check_detection_cancelled() {
        use crate::ci::CheckConclusion;
        assert!(is_failed_check(&make_check(Some(
            CheckConclusion::Cancelled
        ))));
    }

    #[test]
    fn test_failed_check_detection_timed_out() {
        use crate::ci::CheckConclusion;
        assert!(is_failed_check(&make_check(Some(
            CheckConclusion::TimedOut
        ))));
    }

    #[test]
    fn test_failed_check_detection_action_required() {
        use crate::ci::CheckConclusion;
        assert!(is_failed_check(&make_check(Some(
            CheckConclusion::ActionRequired
        ))));
    }

    #[test]
    fn test_successful_check_not_counted_as_failure() {
        use crate::ci::CheckConclusion;
        assert!(!is_failed_check(&make_check(Some(
            CheckConclusion::Success
        ))));
    }

    #[test]
    fn test_skipped_check_not_counted_as_failure() {
        use crate::ci::CheckConclusion;
        assert!(!is_failed_check(&make_check(Some(
            CheckConclusion::Skipped
        ))));
    }

    #[test]
    fn test_neutral_check_not_counted_as_failure() {
        use crate::ci::CheckConclusion;
        assert!(!is_failed_check(&make_check(Some(
            CheckConclusion::Neutral
        ))));
    }

    #[test]
    fn test_in_progress_check_not_counted_as_failure() {
        assert!(!is_failed_check(&make_check(None)));
    }

    #[test]
    fn test_multiple_checks_mixed_results() {
        use crate::ci::CheckConclusion;
        let checks = [
            make_check(Some(CheckConclusion::Success)),
            make_check(Some(CheckConclusion::Failure)),
            make_check(None),
            make_check(Some(CheckConclusion::Cancelled)),
            make_check(Some(CheckConclusion::Success)),
        ];
        let failed_count = checks.iter().filter(|c| is_failed_check(c)).count();
        assert_eq!(failed_count, 2); // failure + cancelled
    }

    #[test]
    fn test_all_failure_states_detected() {
        use crate::ci::CheckConclusion;
        let checks = [
            make_check(Some(CheckConclusion::Failure)),
            make_check(Some(CheckConclusion::Cancelled)),
            make_check(Some(CheckConclusion::TimedOut)),
            make_check(Some(CheckConclusion::ActionRequired)),
        ];
        assert!(checks.iter().all(is_failed_check));
    }

    #[test]
    fn test_empty_check_runs_no_failures() {
        let checks: Vec<CheckRun> = vec![];
        let failed_count = checks.iter().filter(|c| is_failed_check(c)).count();
        assert_eq!(failed_count, 0);
    }

    // ========================================================================
    // Review Timestamp Filtering Tests
    // ========================================================================
    // These tests exercise the production filter_new_external_reviews function.
    // Tests that focus on timestamp logic use pr_author="not-reviewer" so the
    // author filter is a no-op and the timestamp behavior is isolated.

    fn make_review(id: u64, timestamp: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                // Timestamp-only tests pass pr_author="not-reviewer" so this
                // login must differ from that sentinel to keep the author
                // filter a no-op.
                login: "reviewer".to_string(),
            },
            body: None,
            state: None,
        }
    }

    #[test]
    fn test_reviews_after_timestamp_included() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review(1, "2024-06-15T09:00:00Z"), // Before - excluded
            make_review(2, "2024-06-15T11:00:00Z"), // After - included
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "not-reviewer");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 2);
    }

    #[test]
    fn test_reviews_at_exact_timestamp_included() {
        // Edge case: review at exactly the since timestamp should be included
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review(1, "2024-06-15T10:00:00Z")];

        let filtered = filter_new_external_reviews(&reviews, since, "not-reviewer");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 1);
    }

    #[test]
    fn test_reviews_before_timestamp_excluded() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review(1, "2024-06-15T09:00:00Z"),
            make_review(2, "2024-06-15T09:59:59Z"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "not-reviewer");
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_empty_review_list_returns_empty() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews: Vec<Review> = vec![];

        let filtered = filter_new_external_reviews(&reviews, since, "not-reviewer");
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_all_reviews_after_timestamp() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review(1, "2024-06-15T10:00:01Z"),
            make_review(2, "2024-06-15T11:00:00Z"),
            make_review(3, "2024-06-16T10:00:00Z"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "not-reviewer");
        assert_eq!(filtered.len(), 3);
    }

    // ========================================================================
    // Self-Review Filtering Tests
    // ========================================================================

    fn make_review_by(id: u64, timestamp: &str, login: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: login.to_string(),
            },
            body: None,
            state: None,
        }
    }

    #[test]
    fn test_self_review_excluded_from_new_reviews() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review_by(1, "2024-06-15T11:00:00Z", "pr-author"),
            make_review_by(2, "2024-06-15T11:00:00Z", "external-reviewer"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "pr-author");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].user.login, "external-reviewer");
    }

    #[test]
    fn test_only_self_reviews_returns_empty() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review_by(1, "2024-06-15T11:00:00Z", "pr-author"),
            make_review_by(2, "2024-06-15T12:00:00Z", "pr-author"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "pr-author");
        assert!(filtered.is_empty());
    }

    // ========================================================================
    // JSON Parsing Error Tests
    // ========================================================================

    #[test]
    fn test_pull_request_missing_required_field_fails() {
        // Missing 'merged' field
        let json = r#"{
            "state": "open",
            "head": {"sha": "abc123"}
        }"#;

        let result: Result<PullRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_pull_request_missing_head_fails() {
        let json = r#"{
            "state": "open",
            "merged": false
        }"#;

        let result: Result<PullRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_review_missing_submitted_at_fails() {
        let json = r#"{
            "id": 12345,
            "user": {"login": "reviewer"}
        }"#;

        let result: Result<Review, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_review_invalid_timestamp_fails() {
        let json = r#"{
            "id": 12345,
            "submitted_at": "not-a-timestamp",
            "user": {"login": "reviewer"}
        }"#;

        let result: Result<Review, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_runs_response_missing_check_runs_fails() {
        let json = r#"{
            "total_count": 0
        }"#;

        let result: Result<CheckRunsResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_malformed_json_fails() {
        let json = r#"{ this is not valid json }"#;

        let result: Result<PullRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_json_object_fails_for_pr() {
        let json = "{}";

        let result: Result<PullRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ========================================================================
    // PullRequest Mergeable Field Tests
    // ========================================================================

    #[test]
    fn test_pull_request_deserialize_mergeable_true() {
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {"sha": "abc123"},
            "user": {"login": "author"},
            "mergeable": true
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.mergeable, Some(true));
    }

    #[test]
    fn test_pull_request_deserialize_mergeable_false() {
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {"sha": "abc123"},
            "user": {"login": "author"},
            "mergeable": false
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.mergeable, Some(false));
    }

    #[test]
    fn test_pull_request_deserialize_mergeable_null() {
        // GitHub returns null when still computing mergeable status
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {"sha": "abc123"},
            "user": {"login": "author"},
            "mergeable": null
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.mergeable, None);
    }

    #[test]
    fn test_pull_request_deserialize_mergeable_missing() {
        // The field may be absent in some API responses
        let json = r#"{
            "state": "open",
            "merged": false,
            "head": {"sha": "abc123"},
            "user": {"login": "author"}
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.mergeable, None);
    }

    // ========================================================================
    // CI Failure Reporting: Wait for All Checks to Complete (Issue #461)
    // ========================================================================

    #[test]
    fn test_ci_failure_not_reported_while_checks_in_progress() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::InProgress, None),
        ];
        assert_eq!(count_completed_failures(&checks), None);
    }

    #[test]
    fn test_ci_failure_not_reported_while_checks_queued() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::Queued, None),
        ];
        assert_eq!(count_completed_failures(&checks), None);
    }

    #[test]
    fn test_ci_failure_reported_when_all_completed() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
        ];
        assert_eq!(count_completed_failures(&checks), Some(1));
    }

    #[test]
    fn test_ci_no_failure_when_all_pass() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
        ];
        assert_eq!(count_completed_failures(&checks), None);
    }

    #[test]
    fn test_ci_empty_checks_no_failure() {
        let checks: Vec<CheckRun> = vec![];
        assert_eq!(count_completed_failures(&checks), None);
    }

    #[test]
    fn test_ci_all_in_progress_no_failure() {
        use crate::ci::CheckStatus;
        let checks = vec![
            make_check_with_status(CheckStatus::InProgress, None),
            make_check_with_status(CheckStatus::InProgress, None),
        ];
        assert_eq!(count_completed_failures(&checks), None);
    }

    #[test]
    fn test_ci_unknown_status_not_treated_as_completed() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::Unknown, None),
        ];
        assert_eq!(count_completed_failures(&checks), None);
    }

    // Merge readiness tests are in the unified merge_readiness module.

    // ========================================================================
    // Exit Notification Tests
    // ========================================================================

    fn make_review_with_login(login: &str, submitted_at: &str) -> Review {
        Review {
            id: 1,
            submitted_at: submitted_at.parse().unwrap(),
            user: User {
                login: login.to_string(),
            },
            body: None,
            state: None,
        }
    }

    #[test]
    fn test_has_unaddressed_reviews_filters_pr_author() {
        let since = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let reviews = vec![
            make_review_with_login("minion-bot", "2024-01-02T00:00:00Z"), // PR author — excluded
            make_review_with_login("alice", "2024-01-02T00:00:00Z"),      // external reviewer
        ];
        assert_eq!(has_unaddressed_reviews(&reviews, "minion-bot", since), 1);
    }

    #[test]
    fn test_has_unaddressed_reviews_returns_zero_before_baseline() {
        let since = "2024-01-10T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let reviews = vec![
            make_review_with_login("alice", "2024-01-05T00:00:00Z"), // before baseline
            make_review_with_login("alice", "2024-01-10T00:00:00Z"), // equal to baseline — excluded (uses >)
        ];
        assert_eq!(has_unaddressed_reviews(&reviews, "bot", since), 0);
    }

    #[test]
    fn test_has_unaddressed_reviews_empty_list() {
        let since = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(has_unaddressed_reviews(&[], "bot", since), 0);
    }

    #[test]
    fn test_should_post_exit_notification_false_when_pr_closed() {
        assert!(!should_post_exit_notification(false, 3));
    }

    #[test]
    fn test_should_post_exit_notification_false_when_no_unaddressed() {
        assert!(!should_post_exit_notification(true, 0));
    }

    #[test]
    fn test_should_post_exit_notification_true_when_open_and_unaddressed() {
        assert!(should_post_exit_notification(true, 2));
    }
}
