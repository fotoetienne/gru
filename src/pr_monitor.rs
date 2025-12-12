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
    submitted_at: DateTime<Utc>,
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
            return Ok(MonitorResult::NewReviews(reviews.len()));
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
    /// New review comments detected (count)
    NewReviews(usize),
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

        let reviews = MonitorResult::NewReviews(3);
        assert_eq!(format!("{:?}", reviews), "NewReviews(3)");

        let checks = MonitorResult::FailedChecks(2);
        assert_eq!(format!("{:?}", checks), "FailedChecks(2)");
    }
}
