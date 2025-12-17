use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;
use std::process::Output;
use tokio::time::{sleep, Duration};

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

/// Execute a gh API command with retry logic and exponential backoff.
///
/// # Arguments
/// * `args` - The arguments to pass to `gh` command
/// * `max_retries` - Maximum number of retry attempts (default: 5)
///
/// # Returns
/// The command output on success, or an error after all retries are exhausted.
async fn gh_api_with_retry(args: &[&str], max_retries: u32) -> Result<Output> {
    let mut attempts = 0;
    let args_str = args.join(" ");

    loop {
        let output = tokio::process::Command::new("gh")
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
            // Cap delay at MAX_DELAY_SECS to prevent extreme waits
            let delay_secs = std::cmp::min(BASE_DELAY_SECS.pow(attempts), MAX_DELAY_SECS);
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
}

#[derive(Debug, Deserialize)]
struct Head {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct Review {
    id: u64,
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
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRun>,
}

/// Monitor a PR for review comments, CI failures, and merge/close events.
///
/// This function polls the PR every 30 seconds and:
/// - Detects when the PR is merged (exits successfully)
/// - Detects when the PR is closed without merging (exits with message)
/// - Detects new review comments (returns for handling)
/// - Detects CI failures (returns for handling)
///
/// # Arguments
/// * `worktree_path` - Reserved for future use (e.g., reading local git state, logging)
///
/// Returns `Ok(MonitorResult)` when an event requires action or the PR reaches a terminal state.
pub async fn monitor_pr(
    owner: &str,
    repo: &str,
    pr_number: &str,
    _worktree_path: &Path,
) -> Result<MonitorResult> {
    // Initialize last_check_time to avoid missing reviews submitted during monitor startup
    // Get existing reviews to set the baseline
    let existing_reviews = get_all_reviews(owner, repo, pr_number).await?;
    let mut last_check_time = existing_reviews
        .last()
        .map(|r| r.submitted_at)
        .unwrap_or_else(Utc::now);

    loop {
        // Fetch PR state
        let pr = get_pr(owner, repo, pr_number).await?;

        // Check terminal states - merged PRs are also in "closed" state
        // Must check merged flag first to distinguish merged from just closed
        if pr.state == "closed" {
            if pr.merged {
                return Ok(MonitorResult::Merged);
            } else {
                return Ok(MonitorResult::Closed);
            }
        }

        // Check for new reviews (use >= to avoid missing reviews at exact timestamp)
        let reviews = get_reviews_since(owner, repo, pr_number, last_check_time).await?;
        if !reviews.is_empty() {
            // Fetch detailed comments for the new reviews
            let comments = get_review_comments(owner, repo, pr_number, &reviews).await?;
            return Ok(MonitorResult::NewReviews(comments));
        }

        // Check for failed CI runs - include all error states
        let check_runs = get_check_runs(owner, repo, &pr.head.sha).await?;
        let failed_checks = check_runs
            .iter()
            .filter(|c| {
                matches!(
                    c.conclusion.as_deref(),
                    Some("failure")
                        | Some("cancelled")
                        | Some("timed_out")
                        | Some("action_required")
                )
            })
            .count();

        if failed_checks > 0 {
            return Ok(MonitorResult::FailedChecks(failed_checks));
        }

        // Update last check time and sleep
        last_check_time = Utc::now();
        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
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
}

/// Fetch PR details using gh CLI with retry logic for transient failures
async fn get_pr(owner: &str, repo: &str, pr_number: &str) -> Result<PullRequest> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{pr_number}");
    let output = gh_api_with_retry(&["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
    let endpoint = format!("repos/{owner}/{repo}/pulls/{pr_number}/reviews");
    let output = gh_api_with_retry(&["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch reviews for PR #{}: {}", pr_number, stderr);
    }

    let reviews: Vec<Review> =
        serde_json::from_slice(&output.stdout).context("Failed to parse reviews JSON response")?;

    Ok(reviews)
}

/// Fetch reviews submitted after a given time
async fn get_reviews_since(
    owner: &str,
    repo: &str,
    pr_number: &str,
    since: DateTime<Utc>,
) -> Result<Vec<Review>> {
    let reviews = get_all_reviews(owner, repo, pr_number).await?;

    // Filter to reviews submitted at or after 'since' (use >= to avoid missing exact timestamps)
    let new_reviews: Vec<Review> = reviews
        .into_iter()
        .filter(|r| r.submitted_at >= since)
        .collect();

    Ok(new_reviews)
}

/// Fetch review comments for specific reviews with retry logic for transient failures
async fn get_review_comments(
    owner: &str,
    repo: &str,
    pr_number: &str,
    reviews: &[Review],
) -> Result<Vec<ReviewComment>> {
    let mut all_comments = Vec::new();
    let mut failed_reviews = 0;

    for review in reviews {
        // Fetch comments for this specific review with retry
        let endpoint = format!(
            "repos/{owner}/{repo}/pulls/{pr_number}/reviews/{}/comments",
            review.id
        );
        let output = gh_api_with_retry(&["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
    let endpoint = format!("repos/{owner}/{repo}/commits/{sha}/check-runs");
    let output = gh_api_with_retry(&["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

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
    fn test_monitor_result_debug() {
        // Ensure MonitorResult variants can be formatted
        let merged = MonitorResult::Merged;
        assert_eq!(format!("{:?}", merged), "Merged");

        let closed = MonitorResult::Closed;
        assert_eq!(format!("{:?}", closed), "Closed");

        let reviews = MonitorResult::NewReviews(vec![]);
        assert!(format!("{:?}", reviews).starts_with("NewReviews("));

        let checks = MonitorResult::FailedChecks(2);
        assert_eq!(format!("{:?}", checks), "FailedChecks(2)");
    }

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
    fn test_review_comment_clone() {
        let comment = ReviewComment {
            file: "src/lib.rs".to_string(),
            line: Some(100),
            body: "Consider using a more efficient algorithm.".to_string(),
            reviewer: "dave".to_string(),
        };

        let cloned = comment.clone();
        assert_eq!(comment.file, cloned.file);
        assert_eq!(comment.line, cloned.line);
        assert_eq!(comment.body, cloned.body);
        assert_eq!(comment.reviewer, cloned.reviewer);
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
}
