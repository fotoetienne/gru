use crate::github;
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
/// * `repo` - Repository identifier in "owner/repo" format (used to select `gh` or `ghe`)
/// * `args` - The arguments to pass to the gh command
/// * `max_retries` - Maximum number of retry attempts (default: 5)
///
/// # Returns
/// The command output on success, or an error after all retries are exhausted.
async fn gh_api_with_retry(repo: &str, args: &[&str], max_retries: u32) -> Result<Output> {
    let gh_cmd = github::gh_command_for_repo(repo);
    let mut attempts = 0;
    let args_str = args.join(" ");

    loop {
        let output = tokio::process::Command::new(gh_cmd)
            .args(args)
            .output()
            .await
            .with_context(|| format!("Failed to execute: {} {}", gh_cmd, args_str))?;

        if output.status.success() {
            if attempts > 0 {
                log::info!(
                    "GitHub API call succeeded after {} retries: {} {}",
                    attempts,
                    gh_cmd,
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
    draft: Option<bool>,
    head: Head,
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
    state: Option<String>,
    submitted_at: DateTime<Utc>,
    #[allow(dead_code)]
    // Available for future use; currently only id and submitted_at are accessed
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

#[derive(Debug, Deserialize)]
struct CheckRun {
    status: Option<String>,
    conclusion: Option<String>,
}

/// Check if a CI check run conclusion indicates a failure.
///
/// Failed states include: failure, cancelled, timed_out, action_required.
/// Non-failed states include: success, skipped, neutral, and in-progress (None).
fn is_failed_check(check_run: &CheckRun) -> bool {
    matches!(
        check_run.conclusion.as_deref(),
        Some("failure") | Some("cancelled") | Some("timed_out") | Some("action_required")
    )
}

/// Check if a CI check run is still in progress (not yet completed).
///
/// A check is pending when it has no conclusion yet. The `status` field is used
/// as a secondary signal but only when `conclusion` is `None`, to avoid marking
/// a completed-but-failed check as pending.
fn is_pending_check(check_run: &CheckRun) -> bool {
    check_run.conclusion.is_none()
        && matches!(
            check_run.status.as_deref(),
            None | Some("queued") | Some("in_progress")
        )
}

/// Result of a merge-readiness evaluation.
#[derive(Debug, Clone)]
pub struct MergeReadiness {
    pub ready: bool,
    pub reasons: Vec<String>,
}

/// Evaluate whether a PR is ready to merge based on CI checks, reviews, and draft status.
fn evaluate_merge_readiness(
    pr: &PullRequest,
    check_runs: &[CheckRun],
    reviews: &[Review],
) -> MergeReadiness {
    let mut reasons = Vec::new();

    // Check draft status
    if pr.draft.unwrap_or(false) {
        reasons.push("PR is still a draft".to_string());
    }

    // Check CI: must have at least one check, none failed, none pending
    if check_runs.is_empty() {
        reasons.push("No CI checks found".to_string());
    } else {
        let failed = check_runs.iter().filter(|c| is_failed_check(c)).count();
        let pending = check_runs.iter().filter(|c| is_pending_check(c)).count();

        if failed > 0 {
            reasons.push(format!("{} CI check(s) failed", failed));
        }
        if pending > 0 {
            reasons.push(format!("{} CI check(s) still pending", pending));
        }
    }

    // Check reviews: need at least one APPROVED review.
    // Use only the latest review per reviewer to handle superseded reviews
    // (e.g., reviewer approves, then later requests changes).
    let mut latest_by_reviewer: std::collections::HashMap<&str, &Review> =
        std::collections::HashMap::new();
    for review in reviews {
        let login = review.user.login.as_str();
        if latest_by_reviewer
            .get(login)
            .map_or(true, |prev| review.submitted_at > prev.submitted_at)
        {
            latest_by_reviewer.insert(login, review);
        }
    }
    let has_approval = latest_by_reviewer
        .values()
        .any(|r| r.state.as_deref() == Some("APPROVED"));
    if !has_approval {
        reasons.push("No approving review".to_string());
    }

    MergeReadiness {
        ready: reasons.is_empty(),
        reasons,
    }
}

const READY_TO_MERGE_LABEL: &str = "ready-to-merge";

/// Ensure the `ready-to-merge` label exists in the repository, creating it if needed.
pub async fn ensure_ready_to_merge_label(owner: &str, repo: &str) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/labels");
    let name_field = format!("name={READY_TO_MERGE_LABEL}");

    let output = gh_api_with_retry(
        &repo_full,
        &[
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &name_field,
            "-f",
            "color=0e8a16",
            "-f",
            "description=All merge-readiness checks pass",
        ],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 422 means label already exists - that's fine (idempotent)
        if !stderr.contains("already_exists") {
            log::warn!("Failed to create ready-to-merge label: {}", stderr.trim());
        }
    }

    Ok(())
}

/// Check if a PR currently has the `ready-to-merge` label.
async fn has_ready_to_merge_label(owner: &str, repo: &str, pr_number: &str) -> Result<bool> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch labels for PR #{}: {}", pr_number, stderr);
    }

    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let labels: Vec<Label> =
        serde_json::from_slice(&output.stdout).context("Failed to parse labels JSON")?;
    Ok(labels.iter().any(|l| l.name == READY_TO_MERGE_LABEL))
}

/// Add the `ready-to-merge` label to a PR.
async fn add_ready_to_merge_label(owner: &str, repo: &str, pr_number: &str) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");
    let output = gh_api_with_retry(
        &repo_full,
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

/// Remove the `ready-to-merge` label from a PR.
async fn remove_ready_to_merge_label(owner: &str, repo: &str, pr_number: &str) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let label_encoded = READY_TO_MERGE_LABEL; // No special chars to encode
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels/{label_encoded}");
    let output = gh_api_with_retry(
        &repo_full,
        &["api", &endpoint, "-X", "DELETE"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 404 means label wasn't present - that's fine (idempotent)
        if !stderr.contains("404") && !stderr.contains("Not Found") {
            anyhow::bail!(
                "Failed to remove ready-to-merge label from PR #{}: {}",
                pr_number,
                stderr
            );
        }
    }

    Ok(())
}

/// Check merge readiness and update the `ready-to-merge` label accordingly.
///
/// Tracks transitions: adds label when becoming ready, removes when regressing.
/// Returns the current readiness state for the caller to track.
async fn update_readiness_label(
    owner: &str,
    repo: &str,
    pr_number: &str,
    pr: &PullRequest,
    check_runs: &[CheckRun],
    reviews: &[Review],
    was_ready: bool,
) -> bool {
    let readiness = evaluate_merge_readiness(pr, check_runs, reviews);

    if readiness.ready && !was_ready {
        // Transition: not ready → ready
        match add_ready_to_merge_label(owner, repo, pr_number).await {
            Ok(()) => println!("✅ PR #{} is ready to merge", pr_number),
            Err(e) => log::warn!("Failed to add ready-to-merge label: {}", e),
        }
    } else if !readiness.ready && was_ready {
        // Transition: ready → not ready
        let reason = readiness.reasons.join(", ");
        match remove_ready_to_merge_label(owner, repo, pr_number).await {
            Ok(()) => println!(
                "⚠️  PR #{} is no longer ready to merge ({})",
                pr_number, reason
            ),
            Err(e) => log::warn!("Failed to remove ready-to-merge label: {}", e),
        }
    }

    readiness.ready
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
    let mut was_ready = has_ready_to_merge_label(owner, repo, pr_number)
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
            result = poll_once(owner, repo, pr_number, &mut last_check_time, &mut was_ready) => {
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
    owner: &str,
    repo: &str,
    pr_number: &str,
    last_check_time: &mut DateTime<Utc>,
    was_ready: &mut bool,
) -> Result<Option<MonitorResult>> {
    // Fetch PR state
    let pr = get_pr(owner, repo, pr_number).await?;

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
    let all_reviews = get_all_reviews(owner, repo, pr_number).await?;
    let has_new_reviews = all_reviews
        .iter()
        .any(|r| r.submitted_at >= *last_check_time);
    if has_new_reviews {
        // Extract only the new reviews for comment fetching
        let new_reviews: Vec<Review> = all_reviews
            .into_iter()
            .filter(|r| r.submitted_at >= *last_check_time)
            .collect();
        let comments = get_review_comments(owner, repo, pr_number, &new_reviews).await?;
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

    // Check for failed CI runs - include all error states
    let check_runs = get_check_runs(owner, repo, &pr.head.sha).await?;
    let failed_checks = check_runs.iter().filter(|c| is_failed_check(c)).count();

    if failed_checks > 0 {
        return Ok(Some(MonitorResult::FailedChecks(failed_checks)));
    }

    // Check merge readiness and update label on transitions.
    *was_ready = update_readiness_label(
        owner,
        repo,
        pr_number,
        &pr,
        &check_runs,
        &all_reviews,
        *was_ready,
    )
    .await;

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
    /// Monitoring timed out after the configured duration
    Timeout,
    /// Monitoring was interrupted by the user (e.g., Ctrl+C)
    Interrupted,
}

/// Fetch PR details using gh CLI with retry logic for transient failures
async fn get_pr(owner: &str, repo: &str, pr_number: &str) -> Result<PullRequest> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR: {}", stderr);
    }

    let pr: PullRequest =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON response")?;

    Ok(pr)
}

/// Fetch all reviews for a PR with retry logic for transient failures
async fn get_all_reviews(owner: &str, repo: &str, pr_number: &str) -> Result<Vec<Review>> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
        let output =
            gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
async fn get_check_runs(owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/commits/{sha}/check-runs");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
            }
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
            }
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
            }
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
        let json = r#"{
            "conclusion": "failure"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some("failure".to_string()));
    }

    #[test]
    fn test_check_run_deserialize_success() {
        let json = r#"{
            "conclusion": "success"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some("success".to_string()));
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
        let json = r#"{
            "id": 123456,
            "name": "build",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2024-06-15T10:00:00Z",
            "completed_at": "2024-06-15T10:05:00Z"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.conclusion, Some("success".to_string()));
    }

    #[test]
    fn test_check_runs_response_deserialize() {
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
            Some("success".to_string())
        );
        assert_eq!(
            response.check_runs[1].conclusion,
            Some("failure".to_string())
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

    /// Helper to create a CheckRun with only a conclusion (status defaults to None)
    fn make_check(conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            status: None,
            conclusion: conclusion.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_failed_check_detection_failure() {
        assert!(is_failed_check(&make_check(Some("failure"))));
    }

    #[test]
    fn test_failed_check_detection_cancelled() {
        assert!(is_failed_check(&make_check(Some("cancelled"))));
    }

    #[test]
    fn test_failed_check_detection_timed_out() {
        assert!(is_failed_check(&make_check(Some("timed_out"))));
    }

    #[test]
    fn test_failed_check_detection_action_required() {
        assert!(is_failed_check(&make_check(Some("action_required"))));
    }

    #[test]
    fn test_successful_check_not_counted_as_failure() {
        assert!(!is_failed_check(&make_check(Some("success"))));
    }

    #[test]
    fn test_skipped_check_not_counted_as_failure() {
        assert!(!is_failed_check(&make_check(Some("skipped"))));
    }

    #[test]
    fn test_neutral_check_not_counted_as_failure() {
        assert!(!is_failed_check(&make_check(Some("neutral"))));
    }

    #[test]
    fn test_in_progress_check_not_counted_as_failure() {
        assert!(!is_failed_check(&make_check(None)));
    }

    #[test]
    fn test_multiple_checks_mixed_results() {
        let checks = [
            make_check(Some("success")),
            make_check(Some("failure")),
            make_check(None),
            make_check(Some("cancelled")),
            make_check(Some("success")),
        ];
        let failed_count = checks.iter().filter(|c| is_failed_check(c)).count();
        assert_eq!(failed_count, 2); // failure + cancelled
    }

    #[test]
    fn test_all_failure_states_detected() {
        let checks = [
            make_check(Some("failure")),
            make_check(Some("cancelled")),
            make_check(Some("timed_out")),
            make_check(Some("action_required")),
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
            state: None,
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
            "head": {"sha": "abc123"}
        }"#;

        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.mergeable, None);
    }

    // ========================================================================
    // Merge Readiness Tests
    // ========================================================================

    fn make_pr(draft: bool) -> PullRequest {
        PullRequest {
            state: "open".to_string(),
            merged: false,
            draft: Some(draft),
            head: Head {
                sha: "abc123".to_string(),
            },
            mergeable: Some(true),
        }
    }

    fn make_approved_review(id: u64) -> Review {
        Review {
            id,
            state: Some("APPROVED".to_string()),
            submitted_at: "2024-06-15T10:00:00Z".parse().unwrap(),
            user: User {
                login: "reviewer".to_string(),
            },
        }
    }

    fn make_changes_requested_review(id: u64) -> Review {
        Review {
            id,
            state: Some("CHANGES_REQUESTED".to_string()),
            submitted_at: "2024-06-15T10:00:00Z".parse().unwrap(),
            user: User {
                login: "reviewer".to_string(),
            },
        }
    }

    fn make_completed_check(conclusion: &str) -> CheckRun {
        CheckRun {
            status: Some("completed".to_string()),
            conclusion: Some(conclusion.to_string()),
        }
    }

    #[test]
    fn test_merge_readiness_all_conditions_met() {
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result.ready);
        assert!(result.reasons.is_empty());
    }

    #[test]
    fn test_merge_readiness_draft_pr_not_ready() {
        let pr = make_pr(true);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("draft")));
    }

    #[test]
    fn test_merge_readiness_no_checks_not_ready() {
        let pr = make_pr(false);
        let checks: Vec<CheckRun> = vec![];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("No CI checks")));
    }

    #[test]
    fn test_merge_readiness_failed_checks_not_ready() {
        let pr = make_pr(false);
        let checks = vec![
            make_completed_check("success"),
            make_completed_check("failure"),
        ];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("failed")));
    }

    #[test]
    fn test_merge_readiness_pending_checks_not_ready() {
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success"), make_check(None)];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("pending")));
    }

    #[test]
    fn test_merge_readiness_no_approval_not_ready() {
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![make_changes_requested_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("approving")));
    }

    #[test]
    fn test_merge_readiness_no_reviews_not_ready() {
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews: Vec<Review> = vec![];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("approving")));
    }

    #[test]
    fn test_merge_readiness_multiple_issues() {
        let pr = make_pr(true); // draft
        let checks: Vec<CheckRun> = vec![]; // no checks
        let reviews: Vec<Review> = vec![]; // no reviews

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert_eq!(result.reasons.len(), 3);
    }

    #[test]
    fn test_merge_readiness_skipped_checks_ok() {
        let pr = make_pr(false);
        let checks = vec![
            make_completed_check("success"),
            make_completed_check("skipped"),
        ];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result.ready);
    }

    #[test]
    fn test_merge_readiness_neutral_checks_ok() {
        let pr = make_pr(false);
        let checks = vec![
            make_completed_check("success"),
            make_completed_check("neutral"),
        ];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result.ready);
    }

    #[test]
    fn test_pending_check_detection() {
        // No conclusion = pending
        let check = make_check(None);
        assert!(is_pending_check(&check));

        // Queued status = pending
        let check = CheckRun {
            status: Some("queued".to_string()),
            conclusion: None,
        };
        assert!(is_pending_check(&check));

        // In progress status = pending
        let check = CheckRun {
            status: Some("in_progress".to_string()),
            conclusion: None,
        };
        assert!(is_pending_check(&check));

        // Completed with success = not pending
        let check = make_completed_check("success");
        assert!(!is_pending_check(&check));

        // Completed with failure = not pending (even if status says in_progress)
        let check = CheckRun {
            status: Some("in_progress".to_string()),
            conclusion: Some("failure".to_string()),
        };
        assert!(!is_pending_check(&check));
    }

    #[test]
    fn test_merge_readiness_superseded_approval() {
        // Reviewer approves first, then requests changes - should NOT be ready
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![
            Review {
                id: 1,
                state: Some("APPROVED".to_string()),
                submitted_at: "2024-06-15T10:00:00Z".parse().unwrap(),
                user: User {
                    login: "alice".to_string(),
                },
            },
            Review {
                id: 2,
                state: Some("CHANGES_REQUESTED".to_string()),
                submitted_at: "2024-06-15T11:00:00Z".parse().unwrap(),
                user: User {
                    login: "alice".to_string(),
                },
            },
        ];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(!result.ready);
        assert!(result.reasons.iter().any(|r| r.contains("approving")));
    }

    #[test]
    fn test_merge_readiness_multiple_reviewers_one_approves() {
        // Two reviewers: alice approves, bob requests changes - should be ready
        // (at least one approval from latest reviews)
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![
            Review {
                id: 1,
                state: Some("APPROVED".to_string()),
                submitted_at: "2024-06-15T10:00:00Z".parse().unwrap(),
                user: User {
                    login: "alice".to_string(),
                },
            },
            Review {
                id: 2,
                state: Some("CHANGES_REQUESTED".to_string()),
                submitted_at: "2024-06-15T10:00:00Z".parse().unwrap(),
                user: User {
                    login: "bob".to_string(),
                },
            },
        ];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result.ready);
    }

    #[test]
    fn test_merge_readiness_steady_state_ready() {
        // Already ready, still ready - should remain ready
        let pr = make_pr(false);
        let checks = vec![make_completed_check("success")];
        let reviews = vec![make_approved_review(1)];

        let result = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result.ready);
        // Calling again should still be ready
        let result2 = evaluate_merge_readiness(&pr, &checks, &reviews);
        assert!(result2.ready);
    }
}
