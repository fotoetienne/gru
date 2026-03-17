use crate::ci::CheckRun;
use crate::github;
use crate::labels;
use crate::merge_readiness;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;
use std::process::Output;
use tokio::time::{sleep, Duration, Instant};

const POLL_INTERVAL_SECS: u64 = 30;
// Use 5 retries for PR monitoring (lower than CLAUDE.md's 10-15 guideline)
// because we poll every 30 seconds anyway. Total sleep time: ~62 seconds max
// (2+4+8+16+32=62s between retries, not including API call duration).
const DEFAULT_MAX_RETRIES: u32 = 5;
const BASE_DELAY_SECS: u64 = 2;
const MAX_DELAY_SECS: u64 = 60; // Cap exponential backoff at 60 seconds

/// Check if an error message indicates a transient failure that should be retried.
/// Transient failures include network issues, rate limiting, and temporary server errors.
fn is_retryable_error(stderr: &str) -> bool {
    // All patterns must be lowercase for case-insensitive matching
    let retryable_patterns = [
        // HTTP status codes
        "502",
        "503",
        "504",
        "429",
        // Network errors
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "network unreachable",
        "network error",
        "etimedout",
        "econnreset",
        "econnrefused",
        // DNS errors
        "resolve host",
        "name resolution",
        // Rate limiting
        "rate limit",
        "rate-limit",
        "too many requests",
        // Server errors
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        // Generic transient
        "temporary",
        "try again",
    ];

    let lower_stderr = stderr.to_lowercase();
    retryable_patterns
        .iter()
        .any(|pattern| lower_stderr.contains(pattern))
}

/// Calculate retry delay with exponential backoff capped at MAX_DELAY_SECS.
///
/// Uses `BASE_DELAY_SECS^attempt` formula, capped at `MAX_DELAY_SECS`.
/// Attempt numbers are 1-indexed (first retry = attempt 1).
fn calculate_retry_delay(attempt: u32) -> u64 {
    std::cmp::min(BASE_DELAY_SECS.pow(attempt), MAX_DELAY_SECS)
}

/// Execute a gh API command with retry logic and exponential backoff.
///
/// # Arguments
/// * `host` - GitHub hostname (e.g., "github.com" or "ghe.example.com")
/// * `args` - The arguments to pass to the gh command
/// * `max_retries` - Maximum number of retry attempts (default: 5)
///
/// # Returns
/// The command output on success, or an error after all retries are exhausted.
async fn gh_api_with_retry(host: &str, args: &[&str], max_retries: u32) -> Result<Output> {
    let mut attempts = 0;
    let args_str = args.join(" ");

    loop {
        let output = github::gh_cli_command(host)
            .args(args)
            .output()
            .await
            .with_context(|| format!("Failed to execute: gh {}", args_str))?;

        if output.status.success() {
            if attempts > 0 {
                log::info!(
                    "GitHub API call succeeded after {} retries: gh {}",
                    attempts,
                    args_str
                );
            }
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check if this is a retryable error
        if attempts < max_retries && is_retryable_error(&stderr) {
            attempts += 1;
            let delay_secs = calculate_retry_delay(attempts);
            let delay = Duration::from_secs(delay_secs);
            log::warn!(
                "GitHub API call failed (retry {}/{}): {}. Waiting {:?}...",
                attempts,
                max_retries,
                stderr.trim(),
                delay
            );
            sleep(delay).await;
        } else {
            // Either not retryable or max retries exceeded
            return Ok(output);
        }
    }
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

#[derive(Debug, Deserialize)]
struct Review {
    id: u64,
    submitted_at: DateTime<Utc>,
    user: User,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

/// A review comment with file location and content
#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub file: String,
    pub line: Option<u64>,
    pub body: String,
    pub reviewer: String,
}

#[derive(Debug, Deserialize)]
struct ApiReviewComment {
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

const READY_TO_MERGE_LABEL: &str = labels::READY_TO_MERGE;
const AUTO_MERGE_LABEL: &str = labels::AUTO_MERGE;

/// Ensure the `gru:ready-to-merge` label exists in the repository, creating it if needed.
pub async fn ensure_ready_to_merge_label(host: &str, owner: &str, repo: &str) -> Result<()> {
    let (color, description) =
        labels::get_label_info(READY_TO_MERGE_LABEL).expect("READY_TO_MERGE must be in ALL_LABELS");
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/labels");
    let name_field = format!("name={READY_TO_MERGE_LABEL}");
    let color_field = format!("color={color}");
    let desc_field = format!("description={description}");

    let output = gh_api_with_retry(
        host,
        &[
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &name_field,
            "-f",
            &color_field,
            "-f",
            &desc_field,
        ],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 422 means label already exists - that's fine (idempotent)
        if !stderr.contains("already_exists") {
            log::warn!(
                "Failed to create {} label: {}",
                READY_TO_MERGE_LABEL,
                stderr.trim()
            );
        }
    }

    Ok(())
}

/// Check if a PR currently has a specific label.
async fn has_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    label_name: &str,
) -> Result<bool> {
    let repo_full = format!("{owner}/{repo}");
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
pub async fn ensure_auto_merge_label(host: &str, owner: &str, repo: &str) -> Result<()> {
    let (color, description) =
        labels::get_label_info(AUTO_MERGE_LABEL).expect("AUTO_MERGE must be in ALL_LABELS");
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/labels");
    let name_field = format!("name={AUTO_MERGE_LABEL}");
    let color_field = format!("color={color}");
    let desc_field = format!("description={description}");

    let output = gh_api_with_retry(
        host,
        &[
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &name_field,
            "-f",
            &color_field,
            "-f",
            &desc_field,
        ],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 422 means label already exists - that's fine (idempotent)
        if !stderr.contains("already_exists") {
            log::warn!(
                "Failed to create {} label: {}",
                AUTO_MERGE_LABEL,
                stderr.trim()
            );
        }
    }

    Ok(())
}

/// Add the `gru:auto-merge` label to a PR.
pub async fn add_auto_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let output = github::gh_cli_command(host)
        .args([
            "pr",
            "edit",
            pr_number,
            "--add-label",
            AUTO_MERGE_LABEL,
            "-R",
            &repo_full,
        ])
        .output()
        .await
        .context("Failed to add gru:auto-merge label")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to add gru:auto-merge label to PR #{}: {}",
            pr_number,
            stderr
        );
    }

    Ok(())
}

/// Add the `gru:ready-to-merge` label to a PR.
async fn add_ready_to_merge_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
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
    let repo_full = format!("{owner}/{repo}");

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
pub async fn monitor_pr(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    _worktree_path: &Path,
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
    if pr.state == "closed" {
        if pr.merged {
            return Ok(Some(MonitorResult::Merged));
        } else {
            return Ok(Some(MonitorResult::Closed));
        }
    }

    // Fetch all reviews once, then filter for new ones to avoid a double API call.
    // The full list is reused below for merge-readiness evaluation.
    // Check for new reviews BEFORE merge conflicts so that reviewer feedback
    // is never silently dropped when conflicts and reviews overlap.
    let all_reviews = get_all_reviews(host, owner, repo, pr_number).await?;
    let pr_author = pr.user.login.as_str();
    let has_new_reviews = all_reviews
        .iter()
        .any(|r| r.submitted_at >= *last_check_time && r.user.login != pr_author);
    if has_new_reviews {
        // Extract only the new reviews for comment fetching, excluding self-reviews
        // to prevent the minion from entering a feedback loop with its own reviews.
        let new_reviews: Vec<Review> = all_reviews
            .into_iter()
            .filter(|r| r.submitted_at >= *last_check_time && r.user.login != pr_author)
            .collect();
        let comments = get_review_comments(host, owner, repo, pr_number, &new_reviews).await?;
        // Advance past these reviews so they are not re-fetched if the caller
        // passes the returned last_check_time back as the next baseline.
        *last_check_time = Utc::now();
        return Ok(Some(MonitorResult::NewReviews(comments)));
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
    let all_completed = check_runs
        .iter()
        .all(|c| c.status == crate::ci::CheckStatus::Completed);

    if all_completed {
        let failed_checks = check_runs.iter().filter(|c| is_failed_check(c)).count();
        if failed_checks > 0 {
            return Ok(Some(MonitorResult::FailedChecks(failed_checks)));
        }
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
pub enum MonitorResult {
    /// PR was successfully merged
    Merged,
    /// PR was closed without merging
    Closed,
    /// New review comments detected with details
    NewReviews(Vec<ReviewComment>),
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
    let repo_full = format!("{owner}/{repo}");
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
async fn get_all_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<Review>> {
    let repo_full = format!("{owner}/{repo}");
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

/// Fetch review comments for specific reviews with retry logic for transient failures
async fn get_review_comments(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    reviews: &[Review],
) -> Result<Vec<ReviewComment>> {
    let repo_full = format!("{owner}/{repo}");
    let mut all_comments = Vec::new();
    let mut failed_reviews = 0;

    for review in reviews {
        // Fetch comments for this specific review with retry
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

    Ok(all_comments)
}

/// Format review comments into a prompt for Claude
pub fn format_review_prompt(issue_num: u64, pr_number: &str, comments: &[ReviewComment]) -> String {
    let mut prompt = format!(
        "You previously implemented a fix for issue #{}. Review feedback has been provided \
        on PR #{}. Please address the following comments:\n\n",
        issue_num, pr_number
    );

    for (i, comment) in comments.iter().enumerate() {
        prompt.push_str(&format!("## Review Comment {}\n", i + 1));
        prompt.push_str(&format!("**File:** {}", comment.file));
        if let Some(line) = comment.line {
            prompt.push_str(&format!(":{}", line));
        }
        prompt.push('\n');
        prompt.push_str(&format!("**Reviewer:** @{}\n", comment.reviewer));
        prompt.push_str(&format!("**Comment:** {}\n\n", comment.body));
    }

    prompt.push_str("Please make the requested changes, run tests, and commit.\n");

    prompt
}

/// Fetch check runs for a given commit SHA with retry logic for transient failures
async fn get_check_runs(host: &str, owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let repo_full = format!("{owner}/{repo}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_review_prompt_single_comment() {
        let comments = vec![ReviewComment {
            file: "src/main.rs".to_string(),
            line: Some(45),
            body: "This function needs error handling for null inputs.".to_string(),
            reviewer: "alice".to_string(),
        }];

        let prompt = format_review_prompt(123, "456", &comments);

        assert!(prompt.contains("issue #123"));
        assert!(prompt.contains("PR #456"));
        assert!(prompt.contains("## Review Comment 1"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**Reviewer:** @alice"));
        assert!(prompt.contains("This function needs error handling for null inputs."));
        assert!(prompt.contains("Please make the requested changes, run tests, and commit."));
    }

    #[test]
    fn test_format_review_prompt_multiple_comments() {
        let comments = vec![
            ReviewComment {
                file: "src/main.rs".to_string(),
                line: Some(45),
                body: "Add error handling.".to_string(),
                reviewer: "alice".to_string(),
            },
            ReviewComment {
                file: "tests/test_main.rs".to_string(),
                line: Some(12),
                body: "Add a test case for the edge case.".to_string(),
                reviewer: "bob".to_string(),
            },
        ];

        let prompt = format_review_prompt(123, "456", &comments);

        assert!(prompt.contains("## Review Comment 1"));
        assert!(prompt.contains("## Review Comment 2"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**File:** tests/test_main.rs:12"));
        assert!(prompt.contains("@alice"));
        assert!(prompt.contains("@bob"));
    }

    #[test]
    fn test_format_review_prompt_no_line_number() {
        let comments = vec![ReviewComment {
            file: "README.md".to_string(),
            line: None,
            body: "Update the documentation.".to_string(),
            reviewer: "charlie".to_string(),
        }];

        let prompt = format_review_prompt(123, "456", &comments);

        // Should not have a colon if line is None
        assert!(prompt.contains("**File:** README.md\n"));
        assert!(!prompt.contains("README.md:"));
    }

    #[test]
    fn test_is_retryable_error_http_status_codes() {
        // HTTP 5xx errors should be retryable
        assert!(is_retryable_error("HTTP 502 Bad Gateway"));
        assert!(is_retryable_error("error: 503 Service Unavailable"));
        assert!(is_retryable_error("status: 504 Gateway Timeout"));

        // Rate limiting should be retryable
        assert!(is_retryable_error("HTTP 429 Too Many Requests"));
    }

    #[test]
    fn test_is_retryable_error_network_errors() {
        // Network-related errors should be retryable
        assert!(is_retryable_error("connection timed out"));
        assert!(is_retryable_error("ETIMEDOUT")); // uppercase - should match via case-insensitive
        assert!(is_retryable_error("connection reset by peer"));
        assert!(is_retryable_error("ECONNRESET")); // uppercase - should match via case-insensitive
        assert!(is_retryable_error("connection refused"));
        assert!(is_retryable_error("ECONNREFUSED")); // uppercase - should match via case-insensitive
        assert!(is_retryable_error("network unreachable"));
    }

    #[test]
    fn test_is_retryable_error_dns_errors() {
        // DNS errors should be retryable
        assert!(is_retryable_error("could not resolve host"));
        assert!(is_retryable_error("name resolution failed"));
    }

    #[test]
    fn test_is_retryable_error_server_errors() {
        // Server error messages should be retryable
        assert!(is_retryable_error("Internal Server Error"));
        assert!(is_retryable_error("Service Unavailable"));
        assert!(is_retryable_error("Bad Gateway"));
        assert!(is_retryable_error("Gateway Timeout"));
    }

    #[test]
    fn test_is_retryable_error_generic_transient() {
        // Generic transient messages should be retryable
        assert!(is_retryable_error("temporary failure"));
        assert!(is_retryable_error("please try again later"));
        assert!(is_retryable_error("rate limit exceeded"));
        assert!(is_retryable_error("rate-limit"));
    }

    #[test]
    fn test_is_retryable_error_non_retryable() {
        // Non-retryable errors should not match
        assert!(!is_retryable_error("not found"));
        assert!(!is_retryable_error("HTTP 404"));
        assert!(!is_retryable_error("unauthorized"));
        assert!(!is_retryable_error("HTTP 401"));
        assert!(!is_retryable_error("forbidden"));
        assert!(!is_retryable_error("HTTP 403"));
        assert!(!is_retryable_error("bad request"));
        assert!(!is_retryable_error("HTTP 400"));
        assert!(!is_retryable_error("invalid token"));
        assert!(!is_retryable_error("permission denied"));
    }

    #[test]
    fn test_is_retryable_error_case_insensitive() {
        // Should match regardless of case
        assert!(is_retryable_error("TIMEOUT"));
        assert!(is_retryable_error("Timeout"));
        assert!(is_retryable_error("RATE LIMIT"));
        assert!(is_retryable_error("Rate Limit"));
        assert!(is_retryable_error("SERVICE UNAVAILABLE"));
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

    /// Helper to simulate PR state checking logic
    fn determine_pr_terminal_state(state: &str, merged: bool) -> Option<MonitorResult> {
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

    /// Helper to filter reviews by timestamp
    fn filter_reviews_since(reviews: Vec<Review>, since: DateTime<Utc>) -> Vec<Review> {
        reviews
            .into_iter()
            .filter(|r| r.submitted_at >= since)
            .collect()
    }

    fn make_review(id: u64, timestamp: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: "reviewer".to_string(),
            },
        }
    }

    #[test]
    fn test_reviews_after_timestamp_included() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review(1, "2024-06-15T09:00:00Z"), // Before - excluded
            make_review(2, "2024-06-15T11:00:00Z"), // After - included
        ];

        let filtered = filter_reviews_since(reviews, since);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 2);
    }

    #[test]
    fn test_reviews_at_exact_timestamp_included() {
        // Edge case: review at exactly the since timestamp should be included
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review(1, "2024-06-15T10:00:00Z")];

        let filtered = filter_reviews_since(reviews, since);
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

        let filtered = filter_reviews_since(reviews, since);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_empty_review_list_returns_empty() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews: Vec<Review> = vec![];

        let filtered = filter_reviews_since(reviews, since);
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

        let filtered = filter_reviews_since(reviews, since);
        assert_eq!(filtered.len(), 3);
    }

    // ========================================================================
    // Self-Review Filtering Tests
    // ========================================================================

    /// Helper that mirrors the production filter in poll_once: excludes reviews
    /// by the PR author AND before the last check time.
    fn filter_new_external_reviews(
        reviews: Vec<Review>,
        since: DateTime<Utc>,
        pr_author: &str,
    ) -> Vec<Review> {
        reviews
            .into_iter()
            .filter(|r| r.submitted_at >= since && r.user.login != pr_author)
            .collect()
    }

    fn make_review_by(id: u64, timestamp: &str, login: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: login.to_string(),
            },
        }
    }

    #[test]
    fn test_self_review_excluded_from_new_reviews() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review_by(1, "2024-06-15T11:00:00Z", "pr-author"),
            make_review_by(2, "2024-06-15T11:00:00Z", "external-reviewer"),
        ];

        let filtered = filter_new_external_reviews(reviews, since, "pr-author");
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

        let filtered = filter_new_external_reviews(reviews, since, "pr-author");
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

    // --- calculate_retry_delay tests ---

    #[test]
    fn test_retry_delay_progression() {
        // BASE_DELAY_SECS = 2, so 2^attempt: 2, 4, 8, 16, 32
        assert_eq!(calculate_retry_delay(1), 2);
        assert_eq!(calculate_retry_delay(2), 4);
        assert_eq!(calculate_retry_delay(3), 8);
        assert_eq!(calculate_retry_delay(4), 16);
        assert_eq!(calculate_retry_delay(5), 32);
    }

    #[test]
    fn test_retry_delay_caps_at_max() {
        // 2^6 = 64 > MAX_DELAY_SECS (60), so it should be capped
        assert_eq!(calculate_retry_delay(6), MAX_DELAY_SECS);
        assert_eq!(calculate_retry_delay(7), MAX_DELAY_SECS);
        assert_eq!(calculate_retry_delay(10), MAX_DELAY_SECS);
    }

    // ========================================================================
    // MonitorResult::Timeout Tests
    // ========================================================================

    #[test]
    fn test_timeout_variant() {
        let result = MonitorResult::Timeout;
        assert!(matches!(result, MonitorResult::Timeout));
    }

    #[test]
    fn test_timeout_variant_debug_format() {
        let result = MonitorResult::Timeout;
        let debug = format!("{:?}", result);
        assert!(debug.contains("Timeout"));
    }

    // ========================================================================
    // MonitorResult::Interrupted Tests
    // ========================================================================

    #[test]
    fn test_interrupted_variant() {
        let result = MonitorResult::Interrupted;
        assert!(matches!(result, MonitorResult::Interrupted));
    }

    #[test]
    fn test_interrupted_variant_debug_format() {
        let result = MonitorResult::Interrupted;
        let debug = format!("{:?}", result);
        assert!(debug.contains("Interrupted"));
    }

    // ========================================================================
    // MonitorResult::MergeConflict Tests
    // ========================================================================

    #[test]
    fn test_merge_conflict_variant() {
        let result = MonitorResult::MergeConflict;
        assert!(matches!(result, MonitorResult::MergeConflict));
    }

    #[test]
    fn test_merge_conflict_variant_debug_format() {
        let result = MonitorResult::MergeConflict;
        let debug = format!("{:?}", result);
        assert!(debug.contains("MergeConflict"));
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

    /// Simulates the poll_once CI check logic: only report failures when all
    /// checks have completed.
    fn evaluate_ci_failures(check_runs: &[CheckRun]) -> Option<usize> {
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

    #[test]
    fn test_ci_failure_not_reported_while_checks_in_progress() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::InProgress, None),
        ];
        assert_eq!(evaluate_ci_failures(&checks), None);
    }

    #[test]
    fn test_ci_failure_not_reported_while_checks_queued() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::Queued, None),
        ];
        assert_eq!(evaluate_ci_failures(&checks), None);
    }

    #[test]
    fn test_ci_failure_reported_when_all_completed() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Failure)),
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
        ];
        assert_eq!(evaluate_ci_failures(&checks), Some(1));
    }

    #[test]
    fn test_ci_no_failure_when_all_pass() {
        use crate::ci::{CheckConclusion, CheckStatus};
        let checks = vec![
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
            make_check_with_status(CheckStatus::Completed, Some(CheckConclusion::Success)),
        ];
        assert_eq!(evaluate_ci_failures(&checks), None);
    }

    #[test]
    fn test_ci_empty_checks_no_failure() {
        let checks: Vec<CheckRun> = vec![];
        assert_eq!(evaluate_ci_failures(&checks), None);
    }

    #[test]
    fn test_ci_all_in_progress_no_failure() {
        use crate::ci::CheckStatus;
        let checks = vec![
            make_check_with_status(CheckStatus::InProgress, None),
            make_check_with_status(CheckStatus::InProgress, None),
        ];
        assert_eq!(evaluate_ci_failures(&checks), None);
    }

    // Merge readiness tests are in the unified merge_readiness module.
}
