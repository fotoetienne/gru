use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;
use tokio::time::{sleep, Duration};

const POLL_INTERVAL_SECS: u64 = 30;

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
    #[allow(dead_code)] // Used for future features like tracking reviewer identity
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

/// Fetch PR details using gh CLI
async fn get_pr(owner: &str, repo: &str, pr_number: &str) -> Result<PullRequest> {
    let output = tokio::process::Command::new("gh")
        .args(["api", &format!("repos/{owner}/{repo}/pulls/{pr_number}")])
        .output()
        .await
        .context("Failed to execute gh api command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR: {}", stderr);
    }

    let pr: PullRequest =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON response")?;

    Ok(pr)
}

/// Fetch all reviews for a PR
async fn get_all_reviews(owner: &str, repo: &str, pr_number: &str) -> Result<Vec<Review>> {
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner}/{repo}/pulls/{pr_number}/reviews"),
        ])
        .output()
        .await
        .context("Failed to execute gh api command")?;

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

/// Fetch review comments for specific reviews
async fn get_review_comments(
    owner: &str,
    repo: &str,
    pr_number: &str,
    reviews: &[Review],
) -> Result<Vec<ReviewComment>> {
    let mut all_comments = Vec::new();

    for review in reviews {
        // Fetch comments for this specific review
        let output = tokio::process::Command::new("gh")
            .args([
                "api",
                &format!(
                    "repos/{owner}/{repo}/pulls/{pr_number}/reviews/{}/comments",
                    review.id
                ),
            ])
            .output()
            .await
            .context("Failed to execute gh api command for review comments")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Log error but continue processing other reviews
            eprintln!(
                "Warning: Failed to fetch comments for review {}: {}",
                review.id, stderr
            );
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

    Ok(all_comments)
}

/// Format review comments into a prompt for Claude
pub fn format_review_prompt(issue_num: u64, pr_number: &str, comments: &[ReviewComment]) -> String {
    let mut prompt = format!(
        "You previously implemented a fix for issue #{}. A reviewer has left feedback \
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

/// Fetch check runs for a given commit SHA
async fn get_check_runs(owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner}/{repo}/commits/{sha}/check-runs"),
        ])
        .output()
        .await
        .context("Failed to execute gh api command")?;

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
}
