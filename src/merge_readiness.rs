//! Deterministic merge-readiness check for GitHub PRs.
//!
//! This module is consumed by the PR lifecycle features introduced in Phase 5.
//! The `allow(dead_code)` will be removed once consumers exist.
#![allow(dead_code)]

use crate::github;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt;
use std::process::Output;

const DEFAULT_MAX_RETRIES: u32 = 5;
const BASE_DELAY_SECS: u64 = 2;
const MAX_DELAY_SECS: u64 = 60;

/// Result of a deterministic merge-readiness check for a PR.
///
/// Each field represents one prerequisite for merging. The PR is ready
/// to merge only when all fields are `true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReadiness {
    /// All CI check runs passed (success or skipped), none pending/in-progress.
    pub ci_passing: bool,
    /// At least one APPROVED review, no outstanding CHANGES_REQUESTED.
    pub review_approved: bool,
    /// No merge conflicts (GitHub's `mergeable` field is `true`).
    pub no_conflicts: bool,
    /// No unresolved review threads.
    pub no_unresolved_threads: bool,
}

impl MergeReadiness {
    /// Returns `true` if all merge prerequisites are satisfied.
    pub fn is_ready(&self) -> bool {
        self.ci_passing && self.review_approved && self.no_conflicts && self.no_unresolved_threads
    }
}

impl fmt::Display for MergeReadiness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let check = |ok: bool| if ok { "pass" } else { "FAIL" };
        write!(
            f,
            "ci={} reviews={} conflicts={} threads={}",
            check(self.ci_passing),
            check(self.review_approved),
            check(self.no_conflicts),
            check(self.no_unresolved_threads),
        )
    }
}

/// Check whether a PR is ready to merge by querying GitHub API.
///
/// This is a pure query function — it reads GitHub state and returns a
/// deterministic result. Same API state always produces the same output.
pub async fn check_merge_readiness(
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<MergeReadiness> {
    let pr_str = pr_number.to_string();

    // Fetch PR details (for mergeable + head SHA), reviews, and review threads concurrently
    let (pr, reviews, unresolved_count) = tokio::try_join!(
        get_pr_details(owner, repo, &pr_str),
        get_reviews(owner, repo, &pr_str),
        get_unresolved_thread_count(owner, repo, pr_number),
    )?;

    let check_runs = get_check_runs(owner, repo, &pr.head_sha).await?;

    let ci_passing = evaluate_ci(&check_runs);
    let review_approved = evaluate_reviews(&reviews);
    let no_conflicts = pr.mergeable == Some(true);
    let no_unresolved_threads = unresolved_count == 0;

    Ok(MergeReadiness {
        ci_passing,
        review_approved,
        no_conflicts,
        no_unresolved_threads,
    })
}

// --- Internal types ---

#[derive(Debug)]
struct PrDetails {
    head_sha: String,
    /// `None` means GitHub hasn't computed mergeability yet.
    mergeable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PrApiResponse {
    head: HeadRef,
    mergeable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct HeadRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ReviewApiResponse {
    state: String,
    user: ReviewUser,
}

#[derive(Debug, Deserialize)]
struct ReviewUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct CheckRun {
    conclusion: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckRunsApiResponse {
    check_runs: Vec<CheckRun>,
}

// GraphQL response types for review threads
#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlData {
    repository: Option<GraphQlRepository>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRepository {
    pull_request: Option<GraphQlPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlPullRequest {
    review_threads: GraphQlThreadConnection,
}

#[derive(Debug, Deserialize)]
struct GraphQlThreadConnection {
    nodes: Vec<GraphQlThread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlThread {
    is_resolved: bool,
}

// --- API helpers ---

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
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        attempts += 1;

        if attempts >= max_retries || !is_retryable_error(&stderr) {
            anyhow::bail!(
                "GitHub API call failed after {} attempt(s): {} {} — {}",
                attempts,
                gh_cmd,
                args_str,
                stderr.trim()
            );
        }

        let delay = std::cmp::min(BASE_DELAY_SECS.pow(attempts), MAX_DELAY_SECS);
        log::warn!(
            "Retrying ({}/{}) after {}s: {} {}",
            attempts,
            max_retries,
            delay,
            gh_cmd,
            args_str,
        );
        tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
    }
}

fn is_retryable_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    [
        "502",
        "503",
        "504",
        "429",
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "rate limit",
        "rate-limit",
        "too many requests",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "temporary",
        "try again",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

// --- Data fetching ---

async fn get_pr_details(owner: &str, repo: &str, pr_number: &str) -> Result<PrDetails> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    let pr: PrApiResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON")?;

    Ok(PrDetails {
        head_sha: pr.head.sha,
        mergeable: pr.mergeable,
    })
}

async fn get_reviews(owner: &str, repo: &str, pr_number: &str) -> Result<Vec<ReviewApiResponse>> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    let reviews: Vec<ReviewApiResponse> =
        serde_json::from_slice(&output.stdout).context("Failed to parse reviews JSON")?;

    Ok(reviews)
}

async fn get_check_runs(owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let repo_full = format!("{owner}/{repo}");
    let endpoint = format!("repos/{repo_full}/commits/{sha}/check-runs");
    let output = gh_api_with_retry(&repo_full, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    let response: CheckRunsApiResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse check runs JSON")?;

    Ok(response.check_runs)
}

async fn get_unresolved_thread_count(owner: &str, repo: &str, pr_number: u64) -> Result<usize> {
    let repo_full = format!("{owner}/{repo}");
    let query = format!(
        r#"query {{ repository(owner: "{owner}", name: "{repo}") {{ pullRequest(number: {pr_number}) {{ reviewThreads(first: 100) {{ nodes {{ isResolved }} }} }} }} }}"#,
    );
    let output = gh_api_with_retry(
        &repo_full,
        &["api", "graphql", "-f", &format!("query={query}")],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    let response: GraphQlResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse GraphQL response")?;

    let count = response
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .map(|pr| {
            pr.review_threads
                .nodes
                .iter()
                .filter(|t| !t.is_resolved)
                .count()
        })
        .unwrap_or(0);

    Ok(count)
}

// --- Evaluation logic (pure functions, easy to test) ---

/// CI passes when every check run is complete with `success` or `skipped`.
/// Any pending/in-progress run or a failing conclusion means CI is not passing.
fn evaluate_ci(check_runs: &[CheckRun]) -> bool {
    if check_runs.is_empty() {
        // No checks configured — treat as passing
        return true;
    }

    check_runs.iter().all(|cr| {
        // A run that hasn't completed yet is not passing
        if cr.status.as_deref() != Some("completed") {
            return false;
        }
        matches!(
            cr.conclusion.as_deref(),
            Some("success") | Some("skipped") | Some("neutral")
        )
    })
}

/// Reviews pass when there is at least one APPROVED review and no outstanding
/// CHANGES_REQUESTED from any reviewer. A reviewer who first requested changes
/// then later approved is considered approved (last state per reviewer wins).
fn evaluate_reviews(reviews: &[ReviewApiResponse]) -> bool {
    use std::collections::HashMap;

    // Build per-reviewer last-state map (reviews come chronologically from API)
    let mut reviewer_state: HashMap<&str, &str> = HashMap::new();
    for review in reviews {
        let state = review.state.as_str();
        // Only track meaningful states; COMMENTED/PENDING/DISMISSED don't change approval
        if state == "APPROVED" || state == "CHANGES_REQUESTED" {
            reviewer_state.insert(&review.user.login, state);
        }
    }

    let has_approval = reviewer_state.values().any(|&s| s == "APPROVED");
    let has_blocking = reviewer_state.values().any(|&s| s == "CHANGES_REQUESTED");

    has_approval && !has_blocking
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- evaluate_ci tests ---

    #[test]
    fn test_ci_all_success() {
        let runs = vec![
            CheckRun {
                conclusion: Some("success".into()),
                status: Some("completed".into()),
            },
            CheckRun {
                conclusion: Some("success".into()),
                status: Some("completed".into()),
            },
        ];
        assert!(evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_skipped() {
        let runs = vec![
            CheckRun {
                conclusion: Some("success".into()),
                status: Some("completed".into()),
            },
            CheckRun {
                conclusion: Some("skipped".into()),
                status: Some("completed".into()),
            },
        ];
        assert!(evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_neutral() {
        let runs = vec![
            CheckRun {
                conclusion: Some("success".into()),
                status: Some("completed".into()),
            },
            CheckRun {
                conclusion: Some("neutral".into()),
                status: Some("completed".into()),
            },
        ];
        assert!(evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_failure() {
        let runs = vec![
            CheckRun {
                conclusion: Some("success".into()),
                status: Some("completed".into()),
            },
            CheckRun {
                conclusion: Some("failure".into()),
                status: Some("completed".into()),
            },
        ];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_cancelled() {
        let runs = vec![CheckRun {
            conclusion: Some("cancelled".into()),
            status: Some("completed".into()),
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_timed_out() {
        let runs = vec![CheckRun {
            conclusion: Some("timed_out".into()),
            status: Some("completed".into()),
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_with_action_required() {
        let runs = vec![CheckRun {
            conclusion: Some("action_required".into()),
            status: Some("completed".into()),
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_pending_run() {
        let runs = vec![CheckRun {
            conclusion: None,
            status: Some("in_progress".into()),
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_queued_run() {
        let runs = vec![CheckRun {
            conclusion: None,
            status: Some("queued".into()),
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_no_checks() {
        assert!(evaluate_ci(&[]));
    }

    // --- evaluate_reviews tests ---

    #[test]
    fn test_reviews_single_approval() {
        let reviews = vec![ReviewApiResponse {
            state: "APPROVED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_no_reviews() {
        assert!(!evaluate_reviews(&[]));
    }

    #[test]
    fn test_reviews_only_comments() {
        let reviews = vec![ReviewApiResponse {
            state: "COMMENTED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_changes_requested() {
        let reviews = vec![ReviewApiResponse {
            state: "CHANGES_REQUESTED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_approved_then_changes_requested_by_different_reviewer() {
        let reviews = vec![
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
        ];
        assert!(!evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_changes_requested_then_approved_same_reviewer() {
        let reviews = vec![
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_approved_by_one_changes_by_another_then_other_approves() {
        let reviews = vec![
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_dismissed_does_not_count() {
        // DISMISSED state should be ignored - only APPROVED and CHANGES_REQUESTED matter
        let reviews = vec![ReviewApiResponse {
            state: "DISMISSED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews));
    }

    #[test]
    fn test_reviews_pending_ignored() {
        let reviews = vec![
            ReviewApiResponse {
                state: "PENDING".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews));
    }

    // --- MergeReadiness tests ---

    #[test]
    fn test_merge_readiness_all_passing() {
        let mr = MergeReadiness {
            ci_passing: true,
            review_approved: true,
            no_conflicts: true,
            no_unresolved_threads: true,
        };
        assert!(mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_ci_failing() {
        let mr = MergeReadiness {
            ci_passing: false,
            review_approved: true,
            no_conflicts: true,
            no_unresolved_threads: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_not_approved() {
        let mr = MergeReadiness {
            ci_passing: true,
            review_approved: false,
            no_conflicts: true,
            no_unresolved_threads: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_has_conflicts() {
        let mr = MergeReadiness {
            ci_passing: true,
            review_approved: true,
            no_conflicts: false,
            no_unresolved_threads: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_unresolved_threads() {
        let mr = MergeReadiness {
            ci_passing: true,
            review_approved: true,
            no_conflicts: true,
            no_unresolved_threads: false,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_display() {
        let mr = MergeReadiness {
            ci_passing: true,
            review_approved: false,
            no_conflicts: true,
            no_unresolved_threads: true,
        };
        let s = mr.to_string();
        assert!(s.contains("ci=pass"));
        assert!(s.contains("reviews=FAIL"));
        assert!(s.contains("conflicts=pass"));
        assert!(s.contains("threads=pass"));
    }

    #[test]
    fn test_merge_readiness_all_failing_display() {
        let mr = MergeReadiness {
            ci_passing: false,
            review_approved: false,
            no_conflicts: false,
            no_unresolved_threads: false,
        };
        assert!(!mr.is_ready());
        let s = mr.to_string();
        assert!(s.contains("ci=FAIL"));
        assert!(s.contains("reviews=FAIL"));
        assert!(s.contains("conflicts=FAIL"));
        assert!(s.contains("threads=FAIL"));
    }

    // --- mergeable field edge cases ---

    #[test]
    fn test_mergeable_null_treated_as_not_ready() {
        // When GitHub hasn't computed mergeability yet, mergeable is null/None
        let pr = PrDetails {
            head_sha: "abc123".into(),
            mergeable: None,
        };
        // None != Some(true), so no_conflicts should be false
        assert!(pr.mergeable != Some(true));
    }

    #[test]
    fn test_mergeable_false_treated_as_not_ready() {
        let pr = PrDetails {
            head_sha: "abc123".into(),
            mergeable: Some(false),
        };
        assert!(pr.mergeable != Some(true));
    }

    #[test]
    fn test_mergeable_true_is_ready() {
        let pr = PrDetails {
            head_sha: "abc123".into(),
            mergeable: Some(true),
        };
        assert!(pr.mergeable == Some(true));
    }
}
