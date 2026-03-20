//! Unified deterministic merge-readiness check for GitHub PRs.
//!
//! Replaces the previous dead-code implementation and the inline
//! `evaluate_merge_readiness` in `pr_monitor.rs` with a single source of truth.
//!
//! **Deterministic checks (all must pass):**
//! 1. Not draft — fast bail-out
//! 2. CI passing — Check Runs API + Combined Status API (legacy)
//! 3. Review approved — last-state-per-reviewer, DISMISSED clears prior state
//! 4. No merge conflicts — `mergeable` field from GitHub API

use crate::github;
use crate::github::ReviewUser;
use crate::github::DEFAULT_MAX_RETRIES;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt;

/// Result of a deterministic merge-readiness check for a PR.
///
/// Each field represents one prerequisite for merging. The PR is ready
/// to merge only when all fields are `true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeReadiness {
    /// PR is not a draft.
    pub(crate) not_draft: bool,
    /// All CI check runs passed (success, skipped, or neutral), none pending/in-progress.
    pub(crate) ci_passing: bool,
    /// Review gate satisfied: at least one APPROVED review with no outstanding
    /// CHANGES_REQUESTED, or the PR author left a self-review comment (GitHub prevents
    /// self-approval) and no reviewer has blocked.
    pub(crate) review_approved: bool,
    /// No merge conflicts. `false` when GitHub's `mergeable` is `Some(false)` or `None`.
    pub(crate) no_conflicts: bool,
}

impl MergeReadiness {
    /// Returns `true` if all merge prerequisites are satisfied.
    pub(crate) fn is_ready(&self) -> bool {
        self.not_draft && self.ci_passing && self.review_approved && self.no_conflicts
    }

    /// Returns human-readable reasons for any failing checks.
    pub(crate) fn failure_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.not_draft {
            reasons.push("PR is still a draft".to_string());
        }
        if !self.ci_passing {
            reasons.push("CI checks not passing".to_string());
        }
        if !self.review_approved {
            reasons.push("No approving review or outstanding changes requested".to_string());
        }
        if !self.no_conflicts {
            reasons.push("Merge conflicts or mergeability unknown".to_string());
        }
        reasons
    }
}

impl fmt::Display for MergeReadiness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let check = |ok: bool| if ok { "pass" } else { "FAIL" };
        write!(
            f,
            "draft={} ci={} reviews={} conflicts={}",
            check(self.not_draft),
            check(self.ci_passing),
            check(self.review_approved),
            check(self.no_conflicts),
        )
    }
}

/// Check whether a PR is ready to merge by querying GitHub API.
///
/// This is a pure query function — it reads GitHub state and returns a
/// deterministic result. Same API state always produces the same output.
pub(crate) async fn check_merge_readiness(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<MergeReadiness> {
    let pr_str = pr_number.to_string();

    // Fetch PR details (for draft, mergeable, head SHA) and reviews concurrently
    let (pr, reviews) = tokio::try_join!(
        get_pr_details(host, owner, repo, &pr_str),
        get_reviews(host, owner, repo, &pr_str),
    )?;

    // Fast bail-out: draft PRs are never ready
    if pr.draft {
        return Ok(MergeReadiness {
            not_draft: false,
            ci_passing: false,
            review_approved: false,
            no_conflicts: false,
        });
    }

    // Sequential: needs head SHA from get_pr_details above
    let (check_runs, combined_status) = tokio::try_join!(
        get_check_runs(host, owner, repo, &pr.head_sha),
        get_combined_status(host, owner, repo, &pr.head_sha),
    )?;

    let ci_passing = evaluate_ci(&check_runs) && evaluate_combined_status(&combined_status);
    let review_approved = evaluate_reviews(&reviews, &pr.author_login);
    let no_conflicts = pr.mergeable == Some(true);

    Ok(MergeReadiness {
        not_draft: true,
        ci_passing,
        review_approved,
        no_conflicts,
    })
}

// --- Internal types ---

#[derive(Debug)]
struct PrDetails {
    head_sha: String,
    draft: bool,
    /// `None` means GitHub hasn't computed mergeability yet.
    mergeable: Option<bool>,
    /// Login of the PR author.
    author_login: String,
}

#[derive(Debug, Deserialize)]
struct PrApiResponse {
    head: HeadRef,
    mergeable: Option<bool>,
    #[serde(default)]
    draft: bool,
    user: PrUser,
}

#[derive(Debug, Deserialize)]
struct PrUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct HeadRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewApiResponse {
    pub(crate) state: String,
    pub(crate) user: ReviewUser,
}

#[derive(Debug, Deserialize)]
struct CheckRun {
    conclusion: Option<String>,
    status: Option<String>,
}

/// Combined commit status from the legacy Statuses API.
#[derive(Debug, Deserialize)]
struct CombinedStatus {
    /// One of: success, pending, failure, error
    state: String,
    total_count: u64,
}

// --- API helpers ---

/// Wrapper around the shared retry helper that bails on non-success output.
///
/// merge_readiness callers expect the function to return `Err` on API failure
/// (not just a non-zero exit status), so this wrapper adds the bail check.
async fn gh_api_with_retry(
    host: &str,
    args: &[&str],
    max_retries: u32,
) -> Result<std::process::Output> {
    let output = github::gh_api_with_retry(host, args, max_retries).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let args_str = args.join(" ");
        anyhow::bail!(
            "GitHub API call failed: gh {} — {}",
            args_str,
            stderr.trim()
        );
    }
    Ok(output)
}

// --- Data fetching ---

async fn get_pr_details(host: &str, owner: &str, repo: &str, pr_number: &str) -> Result<PrDetails> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    let pr: PrApiResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON")?;

    Ok(PrDetails {
        head_sha: pr.head.sha,
        draft: pr.draft,
        mergeable: pr.mergeable,
        author_login: pr.user.login,
    })
}

async fn get_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<ReviewApiResponse>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
    // --paginate with --jq '.[]' streams one JSON object per line across pages
    let output = gh_api_with_retry(
        host,
        &["api", "--paginate", &endpoint, "--jq", ".[]"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    let stdout =
        std::str::from_utf8(&output.stdout).context("Failed to decode reviews stdout as UTF-8")?;

    let mut reviews = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let review: ReviewApiResponse =
            serde_json::from_str(line).context("Failed to parse review JSON line")?;
        reviews.push(review);
    }

    Ok(reviews)
}

async fn get_check_runs(host: &str, owner: &str, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/commits/{sha}/check-runs");
    // --paginate with --jq streams individual check run objects, one per line
    let output = gh_api_with_retry(
        host,
        &["api", "--paginate", &endpoint, "--jq", ".check_runs[]"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    let stdout = std::str::from_utf8(&output.stdout)
        .context("Failed to decode check runs stdout as UTF-8")?;

    let mut check_runs = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let check_run: CheckRun =
            serde_json::from_str(line).context("Failed to parse check run JSON line")?;
        check_runs.push(check_run);
    }

    Ok(check_runs)
}

/// Fetch the combined commit status (legacy Statuses API) for a given SHA.
async fn get_combined_status(
    host: &str,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<CombinedStatus> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/commits/{sha}/status");
    let output = gh_api_with_retry(host, &["api", &endpoint], DEFAULT_MAX_RETRIES).await?;

    let status: CombinedStatus =
        serde_json::from_slice(&output.stdout).context("Failed to parse combined status JSON")?;

    Ok(status)
}

// --- Evaluation logic (pure functions, easy to test) ---

/// Check Runs API: passes when every check run is complete with `success`, `skipped`, or `neutral`.
/// `neutral` is treated as passing to match GitHub branch protection behavior.
/// Any pending/in-progress run or a failing conclusion means CI is not passing.
/// Returns `true` when no check runs exist (repo may use only legacy statuses or no CI).
fn evaluate_ci(check_runs: &[CheckRun]) -> bool {
    if check_runs.is_empty() {
        return true;
    }

    check_runs.iter().all(|cr| {
        if cr.status.as_deref() != Some("completed") {
            return false;
        }
        matches!(
            cr.conclusion.as_deref(),
            Some("success") | Some("skipped") | Some("neutral")
        )
    })
}

/// Legacy Statuses API: passes when the combined state is `success`, or when
/// no statuses exist (total_count == 0). Repos using only Check Runs will have
/// total_count == 0 here, which is fine since `evaluate_ci` covers those.
fn evaluate_combined_status(status: &CombinedStatus) -> bool {
    if status.total_count == 0 {
        return true;
    }
    status.state == "success"
}

/// Reviews pass when there is at least one APPROVED review and no outstanding
/// CHANGES_REQUESTED from any reviewer. A reviewer who first requested changes
/// then later approved is considered approved (last state per reviewer wins).
///
/// DISMISSED reviews clear the reviewer's prior state entirely, fixing a bug
/// where a dismissed CHANGES_REQUESTED would still block.
///
/// **Self-review exception:** GitHub prevents users from approving their own PRs,
/// so self-reviews can only produce COMMENTED state. When the PR author has left
/// a COMMENTED review (self-review) and no other reviewer has blocked, the review
/// gate is satisfied. This allows autonomous minions to pass the merge-readiness
/// check without requiring an external reviewer.
fn evaluate_reviews(reviews: &[ReviewApiResponse], pr_author: &str) -> bool {
    use std::collections::HashMap;

    // Build per-reviewer last-state map (reviews come chronologically from API)
    let mut reviewer_state: HashMap<&str, &str> = HashMap::new();
    let mut author_commented = false;

    for review in reviews {
        let state = review.state.as_str();
        match state {
            "APPROVED" | "CHANGES_REQUESTED" => {
                reviewer_state.insert(&review.user.login, state);
            }
            "DISMISSED" => {
                // DISMISSED clears the reviewer's prior state
                reviewer_state.remove(review.user.login.as_str());
                if review.user.login == pr_author {
                    author_commented = false;
                }
            }
            "COMMENTED" => {
                if review.user.login == pr_author {
                    author_commented = true;
                }
            }
            _ => {
                // PENDING — don't change approval state
            }
        }
    }

    let has_approval = reviewer_state.values().any(|&s| s == "APPROVED");
    let has_blocking = reviewer_state.values().any(|&s| s == "CHANGES_REQUESTED");

    if has_blocking {
        return false;
    }

    // Standard path: at least one external APPROVED review
    if has_approval {
        return true;
    }

    // Self-review exception: the author left a COMMENTED review (GitHub prevents
    // self-approval) and no other reviewer has blocked.
    author_commented
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
    fn test_ci_with_neutral_passing() {
        // neutral is treated as passing (matches GitHub branch protection)
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
    fn test_ci_no_status_no_conclusion() {
        let runs = vec![CheckRun {
            conclusion: None,
            status: None,
        }];
        assert!(!evaluate_ci(&runs));
    }

    #[test]
    fn test_ci_no_checks() {
        assert!(evaluate_ci(&[]));
    }

    // --- evaluate_combined_status tests ---

    #[test]
    fn test_combined_status_success() {
        let status = CombinedStatus {
            state: "success".into(),
            total_count: 2,
        };
        assert!(evaluate_combined_status(&status));
    }

    #[test]
    fn test_combined_status_pending() {
        let status = CombinedStatus {
            state: "pending".into(),
            total_count: 1,
        };
        assert!(!evaluate_combined_status(&status));
    }

    #[test]
    fn test_combined_status_failure() {
        let status = CombinedStatus {
            state: "failure".into(),
            total_count: 3,
        };
        assert!(!evaluate_combined_status(&status));
    }

    #[test]
    fn test_combined_status_error() {
        let status = CombinedStatus {
            state: "error".into(),
            total_count: 1,
        };
        assert!(!evaluate_combined_status(&status));
    }

    #[test]
    fn test_combined_status_no_statuses() {
        let status = CombinedStatus {
            state: "pending".into(),
            total_count: 0,
        };
        assert!(evaluate_combined_status(&status));
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
        assert!(evaluate_reviews(&reviews, "author"));
    }

    #[test]
    fn test_reviews_no_reviews() {
        assert!(!evaluate_reviews(&[], "author"));
    }

    #[test]
    fn test_reviews_only_comments() {
        let reviews = vec![ReviewApiResponse {
            state: "COMMENTED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews, "author"));
    }

    #[test]
    fn test_reviews_changes_requested() {
        let reviews = vec![ReviewApiResponse {
            state: "CHANGES_REQUESTED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews, "author"));
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
        assert!(!evaluate_reviews(&reviews, "author"));
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
        assert!(evaluate_reviews(&reviews, "author"));
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
        assert!(evaluate_reviews(&reviews, "author"));
    }

    #[test]
    fn test_reviews_dismissed_clears_prior_state() {
        // DISMISSED should clear the reviewer's prior CHANGES_REQUESTED
        let reviews = vec![
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "DISMISSED".into(),
                user: ReviewUser {
                    login: "bob".into(),
                },
            },
        ];
        // Bob's CHANGES_REQUESTED was dismissed, Alice approved → ready
        assert!(evaluate_reviews(&reviews, "author"));
    }

    #[test]
    fn test_reviews_dismissed_alone_not_sufficient() {
        // DISMISSED alone doesn't count as approval
        let reviews = vec![ReviewApiResponse {
            state: "DISMISSED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews, "author"));
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
        assert!(evaluate_reviews(&reviews, "author"));
    }

    // --- Self-review tests ---

    #[test]
    fn test_reviews_self_review_comment_satisfies_gate() {
        // PR author left a COMMENTED review (self-review). GitHub prevents
        // self-approval, so COMMENTED is the best the author can do.
        let reviews = vec![ReviewApiResponse {
            state: "COMMENTED".into(),
            user: ReviewUser {
                login: "minion-bot".into(),
            },
        }];
        assert!(evaluate_reviews(&reviews, "minion-bot"));
    }

    #[test]
    fn test_reviews_self_review_comment_blocked_by_changes_requested() {
        // Author self-reviewed, but another reviewer requested changes — blocked.
        let reviews = vec![
            ReviewApiResponse {
                state: "COMMENTED".into(),
                user: ReviewUser {
                    login: "minion-bot".into(),
                },
            },
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
        ];
        assert!(!evaluate_reviews(&reviews, "minion-bot"));
    }

    #[test]
    fn test_reviews_non_author_comment_does_not_satisfy_gate() {
        // COMMENTED review from someone other than the author should NOT satisfy gate.
        let reviews = vec![ReviewApiResponse {
            state: "COMMENTED".into(),
            user: ReviewUser {
                login: "alice".into(),
            },
        }];
        assert!(!evaluate_reviews(&reviews, "minion-bot"));
    }

    #[test]
    fn test_reviews_self_review_with_external_approval() {
        // Both self-review and external approval — should pass.
        let reviews = vec![
            ReviewApiResponse {
                state: "COMMENTED".into(),
                user: ReviewUser {
                    login: "minion-bot".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews, "minion-bot"));
    }

    #[test]
    fn test_reviews_self_review_after_external_approval_dismissed() {
        // External approval was dismissed; only author self-comment remains.
        let reviews = vec![
            ReviewApiResponse {
                state: "COMMENTED".into(),
                user: ReviewUser {
                    login: "minion-bot".into(),
                },
            },
            ReviewApiResponse {
                state: "APPROVED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "DISMISSED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews, "minion-bot"));
    }

    #[test]
    fn test_reviews_self_review_after_blocker_dismissed() {
        // Blocker dismissed, then author self-reviews — should pass.
        let reviews = vec![
            ReviewApiResponse {
                state: "CHANGES_REQUESTED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "DISMISSED".into(),
                user: ReviewUser {
                    login: "alice".into(),
                },
            },
            ReviewApiResponse {
                state: "COMMENTED".into(),
                user: ReviewUser {
                    login: "minion-bot".into(),
                },
            },
        ];
        assert!(evaluate_reviews(&reviews, "minion-bot"));
    }

    // --- MergeReadiness tests ---

    #[test]
    fn test_merge_readiness_all_passing() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: true,
            review_approved: true,
            no_conflicts: true,
        };
        assert!(mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_draft() {
        let mr = MergeReadiness {
            not_draft: false,
            ci_passing: true,
            review_approved: true,
            no_conflicts: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_ci_failing() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: false,
            review_approved: true,
            no_conflicts: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_not_approved() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: true,
            review_approved: false,
            no_conflicts: true,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_has_conflicts() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: true,
            review_approved: true,
            no_conflicts: false,
        };
        assert!(!mr.is_ready());
    }

    #[test]
    fn test_merge_readiness_display() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: true,
            review_approved: false,
            no_conflicts: true,
        };
        let s = mr.to_string();
        assert!(s.contains("ci=pass"));
        assert!(s.contains("reviews=FAIL"));
        assert!(s.contains("conflicts=pass"));
        assert!(s.contains("draft=pass"));
    }

    #[test]
    fn test_merge_readiness_all_failing_display() {
        let mr = MergeReadiness {
            not_draft: false,
            ci_passing: false,
            review_approved: false,
            no_conflicts: false,
        };
        assert!(!mr.is_ready());
        let s = mr.to_string();
        assert!(s.contains("draft=FAIL"));
        assert!(s.contains("ci=FAIL"));
        assert!(s.contains("reviews=FAIL"));
        assert!(s.contains("conflicts=FAIL"));
    }

    #[test]
    fn test_merge_readiness_failure_reasons() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: false,
            review_approved: true,
            no_conflicts: false,
        };
        let reasons = mr.failure_reasons();
        assert_eq!(reasons.len(), 2);
        assert!(reasons.iter().any(|r| r.contains("CI")));
        assert!(reasons.iter().any(|r| r.contains("conflicts")));
    }

    #[test]
    fn test_merge_readiness_no_failure_reasons_when_ready() {
        let mr = MergeReadiness {
            not_draft: true,
            ci_passing: true,
            review_approved: true,
            no_conflicts: true,
        };
        assert!(mr.failure_reasons().is_empty());
    }
}
