use crate::ci::{self, CheckRun};
use crate::github;
use crate::github::DEFAULT_MAX_RETRIES;
use crate::labels;
use crate::merge_readiness;
use crate::progress_comments::{extract_minion_id_from_signature, has_minion_signature_for};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::process::Output;
use tokio::time::{sleep, Duration, Instant};

const POLL_INTERVAL_SECS: u64 = 30;
const MAX_POLL_INTERVAL_SECS: u64 = 300; // 5 minutes

/// Re-export from `github.rs` so callers (e.g. `monitor.rs`) can use
/// `pr_monitor::is_rate_limit_error` without reaching into `github` directly.
pub(crate) use crate::github::is_rate_limit_error;

/// Alias for the shared retry helper in `github.rs`.
async fn gh_api_with_retry(host: &str, args: &[&str], max_retries: u32) -> Result<Output> {
    github::gh_api_with_retry(host, args, max_retries).await
}

#[derive(Debug, Deserialize)]
struct PrLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    state: String,
    merged: bool,
    head: Head,
    #[allow(dead_code)] // deserialized from GitHub API; used in tests
    user: User,
    /// GitHub's mergeable field: true, false, or null (still computing).
    mergeable: Option<bool>,
    /// When the PR was created on GitHub.
    created_at: DateTime<Utc>,
    /// Labels attached to this PR (included in GitHub's PR API response).
    #[serde(default)]
    labels: Vec<PrLabel>,
    /// Whether the PR is a draft.
    #[serde(default)]
    draft: bool,
}

impl PullRequest {
    /// Check whether the PR carries a label with the given name.
    fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l.name == name)
    }
}

#[derive(Debug, Deserialize)]
struct Head {
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Review {
    id: u64,
    pub(crate) submitted_at: DateTime<Utc>,
    #[allow(dead_code)] // deserialized from GitHub API; used in tests
    pub(crate) user: User,
    #[serde(default)]
    pub(crate) body: String,
    /// Review state (APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING).
    /// Used by merge-readiness evaluation to avoid re-fetching reviews.
    #[serde(default)]
    pub(crate) state: String,
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
    /// The commenter's GitHub login (username).
    pub(crate) reviewer: String,
    /// The commenter's display name, or their login if no display name is set.
    pub(crate) reviewer_display_name: String,
    pub(crate) comment_id: u64,
}

/// A review-level body (not tied to a specific file/line)
#[derive(Debug, Clone)]
pub(crate) struct ReviewBody {
    pub body: String,
    pub reviewer: String,
    /// Display name used when addressing the reviewer: Minion ID when the body
    /// carries a Minion signature, otherwise the reviewer's GitHub display name
    /// (or login fallback when no display name is set).
    pub reviewer_display_name: String,
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

#[derive(Debug, Clone, Deserialize)]
struct ApiReviewComment {
    id: u64,
    path: String,
    line: Option<u64>,
    body: String,
    user: User,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    #[serde(default = "default_api_review_comment_created_at")]
    created_at: DateTime<Utc>,
}

/// Default used when `created_at` is missing from the API payload (which
/// GitHub should always include for a real comment, but may be omitted in
/// test fixtures). The UNIX epoch is load-bearing for dedup safety: any
/// comment we can't date is classified as pre-`since` by
/// `identify_duplicate_minion_replies` and therefore never deleted.
fn default_api_review_comment_created_at() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("UNIX epoch is a valid DateTime")
}

/// A general PR conversation comment (issue comment, not a formal review)
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IssueComment {
    #[allow(dead_code)] // deserialized from GitHub API; available for future deduplication
    pub(crate) id: u64,
    pub(crate) body: String,
    pub(crate) user: User,
    pub(crate) created_at: DateTime<Utc>,
    /// The commenter's display name, or login fallback. Not from GitHub JSON;
    /// populated after deserialization during enrichment, either from a
    /// Minion signature via `extract_minion_id_from_signature` or from
    /// `get_user_display_name` (with login fallback).
    #[serde(skip)]
    pub(crate) display_name: String,
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
///
/// When `prefetched` is provided, uses pre-fetched data from the poll cycle to
/// avoid duplicate API calls. Falls back to fetching everything when `None`.
async fn update_readiness_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    pr_number_u64: u64,
    was_ready: &mut bool,
    prefetched: Option<&merge_readiness::PreFetchedData>,
) -> Option<merge_readiness::MergeReadiness> {
    let readiness = match prefetched {
        Some(data) => {
            match merge_readiness::check_merge_readiness_with_data(host, owner, repo, data).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("Failed to check merge readiness: {:#}", e);
                    return None;
                }
            }
        }
        None => {
            match merge_readiness::check_merge_readiness(host, owner, repo, pr_number_u64).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("Failed to check merge readiness: {}", e);
                    return None;
                }
            }
        }
    };

    let is_ready = readiness.is_ready();

    if is_ready && !*was_ready {
        // Transition: not ready → ready
        match add_ready_to_merge_label(host, owner, repo, pr_number).await {
            Ok(()) => println!("✅ PR #{} is ready to merge", pr_number),
            Err(e) => log::warn!("Failed to add ready-to-merge label: {:#}", e),
        }
    } else if !is_ready && *was_ready {
        // Transition: ready → not ready
        let reason = readiness.failure_reasons().join(", ");
        match remove_ready_to_merge_label(host, owner, repo, pr_number).await {
            Ok(()) => println!(
                "⚠️  PR #{} is no longer ready to merge ({})",
                pr_number, reason
            ),
            Err(e) => log::warn!("Failed to remove ready-to-merge label: {:#}", e),
        }
    }

    *was_ready = is_ready;

    if is_ready {
        Some(readiness)
    } else {
        None
    }
}

/// Local check-run struct for raw GitHub API responses.
///
/// The raw API returns `output` as a JSON object with `title`, `summary`,
/// and `text` sub-fields. This struct deserializes it as `serde_json::Value`
/// and extracts those sub-fields when converting to `ci::CheckRun`.
#[derive(Debug, Deserialize)]
struct RawCheckRun {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: ci::CheckStatus,
    conclusion: Option<ci::CheckConclusion>,
    duration: Option<String>,
    output: Option<serde_json::Value>,
}

impl From<RawCheckRun> for CheckRun {
    fn from(raw: RawCheckRun) -> Self {
        let output = raw.output.and_then(|v| {
            // Extract title/summary/text from the API object, mirroring
            // the --jq filter in ci::fetch_check_runs.
            let mut parts = Vec::new();
            for key in &["title", "summary", "text"] {
                if let Some(s) = v.get(key).and_then(|t| t.as_str()) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        });
        CheckRun {
            name: raw.name,
            status: raw.status,
            conclusion: raw.conclusion,
            duration: raw.duration,
            output,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<RawCheckRun>,
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
    minion_id: &str,
) -> Result<(MonitorResult, DateTime<Utc>)> {
    let start_time = Instant::now();

    let mut last_check_time = baseline.unwrap_or_else(Utc::now);

    // Track merge-readiness state across polls to detect transitions.
    // Seed from the current label state so we don't add/remove on first poll.
    // Uses get_pr (which has retry logic) rather than a separate labels endpoint;
    // poll_once will re-fetch the PR on its first iteration, but the one-time
    // startup cost is negligible compared to simplifying poll_once's interface.
    let seed_pr = get_pr(host, owner, repo, pr_number).await?;
    let mut was_ready = seed_pr.has_label(READY_TO_MERGE_LABEL);

    // Register the Ctrl+C listener once to avoid signal loss between iterations
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut consecutive_idle_cycles: u32 = 0;

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
            result = poll_once(host, owner, repo, pr_number, &mut last_check_time, &mut was_ready, minion_id) => {
                if let Some(monitor_result) = result? {
                    return Ok((monitor_result, last_check_time));
                }
            }
            _ = &mut ctrl_c => {
                return Ok((MonitorResult::Interrupted, last_check_time));
            }
        }

        // Adaptive backoff: double interval for each idle cycle, capped at max.
        // Increment first so the first idle sleep is 2x base (the initial poll
        // already ran without delay, so the base interval is "free").
        consecutive_idle_cycles = consecutive_idle_cycles.saturating_add(1);
        let multiplier = 2u64.saturating_pow(consecutive_idle_cycles);
        let current_interval = std::cmp::min(
            POLL_INTERVAL_SECS.saturating_mul(multiplier),
            MAX_POLL_INTERVAL_SECS,
        );
        log::debug!(
            "PR poll interval: {}s (idle for {} cycle{})",
            current_interval,
            consecutive_idle_cycles,
            if consecutive_idle_cycles == 1 {
                ""
            } else {
                "s"
            },
        );

        // Sleep between polls, still responding to Ctrl+C
        tokio::select! {
            _ = sleep(Duration::from_secs(current_interval)) => {}
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

/// Filter reviews to only include those submitted at or after `since` that were
/// not authored by the current Minion.
///
/// A review is considered authored by the current Minion when its body ends with
/// the Minion's own signature (`<sub>🤖 {minion_id}</sub>`).  Using the
/// signature rather than `user.login` correctly handles the case where multiple
/// Minions operate as the same GitHub user: a review posted by Minion M1bz will
/// carry M1bz's signature and must pass through as external feedback for M1by,
/// even though both share the same `user.login`.
///
/// Reviews with an empty body (e.g. GitHub's implicit review objects created
/// when a Minion posts inline reply comments) also pass through; their inline
/// comments are subsequently filtered by `get_review_feedback`.
///
/// Uses inclusive `>=` so that a review landing exactly at `since` is captured;
/// contrast with `count_unaddressed_reviews` which uses exclusive `>`.
pub(crate) fn filter_new_external_reviews(
    reviews: &[Review],
    since: DateTime<Utc>,
    minion_id: &str,
) -> Vec<Review> {
    reviews
        .iter()
        .filter(|r| r.submitted_at >= since && !has_minion_signature_for(&r.body, minion_id))
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
    minion_id: &str,
) -> Result<Option<MonitorResult>> {
    // Fetch PR state
    let pr = get_pr(host, owner, repo, pr_number).await?;

    // Check terminal states - merged PRs are also in "closed" state
    // Must check merged flag first to distinguish merged from just closed
    if let Some(result) = determine_pr_terminal_state(&pr.state, pr.merged) {
        return Ok(Some(result));
    }

    // Capture the poll time before any API calls so that any event submitted
    // while the API calls are in flight is not silently dropped: its timestamp
    // will be >= `poll_time` and will therefore be visible on the next poll cycle.
    let poll_time = Utc::now();
    // Fetch all reviews once, then filter for new ones to avoid a double API call.
    // The full list is reused below for merge-readiness evaluation.
    // Check for new reviews BEFORE merge conflicts so that reviewer feedback
    // is never silently dropped when conflicts and reviews overlap.
    let all_reviews = get_all_reviews(host, owner, repo, pr_number).await?;
    let new_reviews = filter_new_external_reviews(&all_reviews, *last_check_time, minion_id);
    // Track whether review feedback fetch had partial failures. When true, the
    // baseline must not advance — otherwise the unfetched inline comments in this
    // window would be permanently lost (same intent as the issue_comments_fetch_failed
    // guard below).
    let mut review_fetch_failed = false;
    if !new_reviews.is_empty() {
        let feedback =
            get_review_feedback(host, owner, repo, pr_number, &new_reviews, minion_id).await?;
        // Only advance the baseline when we successfully fetched all reviews.
        // If some fetches failed we leave last_check_time unchanged so the
        // reviews are retried on the next poll cycle.
        if !feedback.had_fetch_failures {
            *last_check_time = poll_time;
        } else {
            review_fetch_failed = true;
        }
        // Only emit NewReviews if there is actual feedback to act on.
        // DISMISSED reviews or reviews with empty bodies and no inline
        // comments can produce an empty ReviewFeedback.
        if !feedback.is_empty() {
            return Ok(Some(MonitorResult::NewReviews(feedback)));
        }
    }

    // Check for new general PR conversation comments (issue comments).
    // These are distinct from formal reviews and are invisible to the review polling path.
    // Degrade gracefully on fetch failure — a secondary signal should not kill a healthy session.
    // Track the failure so we can skip the baseline advance below: advancing past a failed
    // fetch would permanently lose any human comments posted in that window (they would
    // never be retried), undermining the monitor's ability to respond to PR conversation.
    let (all_issue_comments, issue_comments_fetch_failed) =
        match fetch_issue_comments(host, owner, repo, pr_number, *last_check_time).await {
            Ok(c) => (c, false),
            Err(e) => {
                log::warn!(
                    "Could not fetch issue comments (will retry next cycle): {:#}",
                    e
                );
                (vec![], true)
            }
        };
    let mut new_issue_comments =
        filter_new_issue_comments(&all_issue_comments, *last_check_time, minion_id);
    if !new_issue_comments.is_empty() {
        // Fetch display names for unique authors of new comments.
        // Minion-signed comments use the Minion ID; human comments use the
        // GitHub display name (or login fallback), matching the inline comment path.
        let mut display_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for comment in &new_issue_comments {
            let login = &comment.user.login;
            if !display_names.contains_key(login)
                && extract_minion_id_from_signature(&comment.body).is_none()
            {
                let name = github::get_user_display_name(host, login).await;
                display_names.insert(login.clone(), name);
            }
        }
        for comment in &mut new_issue_comments {
            comment.display_name = extract_minion_id_from_signature(&comment.body)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    display_names
                        .get(&comment.user.login)
                        .cloned()
                        .unwrap_or_else(|| comment.user.login.clone())
                });
        }

        // Only advance if review fetches also succeeded: if review_fetch_failed is true,
        // the review inline-comment window must not be skipped by this advance.
        if !review_fetch_failed {
            *last_check_time = poll_time;
        }
        return Ok(Some(MonitorResult::NewIssueComments(new_issue_comments)));
    }

    // Check merge conflict status.
    // mergeable == Some(false) means GitHub detected conflicts.
    // mergeable == None means GitHub is still computing — skip and re-check next cycle.
    //
    // ORDERING NOTE: reviews are checked before merge conflicts (above).
    // When review fetching succeeded, handle_merge_conflict in monitor.rs can rely on
    // last_check_time having already been advanced past any reviews (including empty-body
    // reply reviews) seen in this poll iteration before MergeConflict is returned.
    // If review fetching failed, poll_once intentionally leaves last_check_time unchanged
    // so review events are retried on the next cycle; in that case this advance is not
    // guaranteed before returning MergeConflict.
    if pr.mergeable == Some(false) {
        return Ok(Some(MonitorResult::MergeConflict));
    }

    // Check for failed CI runs - only report failures when all checks have completed.
    // If any checks are still queued or in progress, skip and re-check next cycle.
    let check_runs = get_check_runs(host, owner, repo, &pr.head.sha).await?;
    if let Some(failed_checks) = count_completed_failures(&check_runs) {
        // Advance the baseline so reviews are not missed when monitor_pr
        // is re-entered after CI handling in the lifecycle loop.
        *last_check_time = poll_time;
        return Ok(Some(MonitorResult::FailedChecks(failed_checks)));
    }

    // Check merge readiness (via unified module) and update label on transitions.
    // Build PreFetchedData from the PR, reviews, and check runs already fetched above
    // to avoid duplicate API calls in check_merge_readiness.
    let pr_number_u64: u64 = match pr_number.parse() {
        Ok(n) => n,
        Err(_) => {
            log::warn!(
                "Could not parse PR number '{}', skipping readiness check",
                pr_number
            );
            *last_check_time = poll_time;
            return Ok(None);
        }
    };

    let prefetched = merge_readiness::PreFetchedData {
        pr: merge_readiness::PreFetchedPr {
            head_sha: pr.head.sha.clone(),
            draft: pr.draft,
            mergeable: pr.mergeable,
            author_login: pr.user.login.clone(),
        },
        reviews: all_reviews
            .iter()
            .map(|r| merge_readiness::ReviewApiResponse {
                state: r.state.clone(),
                user: github::ReviewUser {
                    login: r.user.login.clone(),
                },
            })
            .collect(),
        // Convert ci::CheckRun to PreFetchedCheckRun strings matching the raw
        // GitHub API format that evaluate_ci expects. Uses Display impls on
        // CheckConclusion/CheckStatus to stay in sync with serde rename_all.
        // Unknown conclusions become "unknown" (rejected by evaluate_ci).
        // Unknown status becomes "unknown" (also rejected, since evaluate_ci
        // requires status == "completed").
        check_runs: check_runs
            .iter()
            .map(|cr| merge_readiness::PreFetchedCheckRun {
                conclusion: cr.conclusion.as_ref().map(|c| c.to_string()),
                status: Some(cr.status.to_string()),
            })
            .collect(),
    };

    if update_readiness_label(
        host,
        owner,
        repo,
        pr_number,
        pr_number_u64,
        was_ready,
        Some(&prefetched),
    )
    .await
    .is_some()
    {
        // PR is ready — check if gru:auto-merge label is present (from PR data already fetched)
        if pr.has_label(AUTO_MERGE_LABEL) {
            *last_check_time = poll_time;
            return Ok(Some(MonitorResult::ReadyToMerge));
        }
    }

    // Only advance the baseline when all fetches succeeded.
    // If either issue comment fetching or review inline-comment fetching failed,
    // leave last_check_time unchanged so this window is retried on the next cycle
    // instead of being permanently skipped.
    if !issue_comments_fetch_failed && !review_fetch_failed {
        *last_check_time = poll_time;
    }
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
    /// New general PR conversation comments (issue comments) detected
    NewIssueComments(Vec<IssueComment>),
    /// Monitoring timed out after the configured duration
    Timeout,
    /// Monitoring was interrupted by the user (e.g., Ctrl+C)
    Interrupted,
}

/// Fetch PR details using gh CLI with retry logic for transient failures
async fn get_pr(host: &str, owner: &str, repo: &str, pr_number: &str) -> Result<PullRequest> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}");
    let output = gh_api_with_retry(
        host,
        &["api", &endpoint, "--cache", "20s"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR: {}", stderr);
    }

    let pr: PullRequest =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR JSON response")?;

    Ok(pr)
}

/// Fetch all reviews for a PR with retry logic for transient failures.
///
/// Uses `--jq ".[]"` to extract individual review objects line-by-line.
/// The reviews endpoint returns a bare JSON array (unlike check-runs which
/// wraps results in a `.check_runs` field).
pub(crate) async fn get_all_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<Review>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
    // --paginate with --jq streams individual review objects, one per line
    let output = gh_api_with_retry(
        host,
        &["api", "--paginate", &endpoint, "--jq", ".[]"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch reviews for PR #{}: {}", pr_number, stderr);
    }

    let stdout =
        std::str::from_utf8(&output.stdout).context("Failed to decode reviews stdout as UTF-8")?;

    let mut reviews = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let review: Review =
            serde_json::from_str(line).context("Failed to parse review JSON line")?;
        reviews.push(review);
    }

    Ok(reviews)
}

/// Fetch general PR conversation comments (issue comments) for a PR.
///
/// Uses `--paginate` with `--jq ".[]"` to stream individual comment objects.
/// Passes `since` as a server-side pre-filter (`?since=`) to reduce payload size on
/// long-lived PRs with many comments. Note: GitHub filters by `updated_at`, while the
/// local filter in `filter_new_issue_comments` uses `created_at`; the local filter
/// remains authoritative and prevents edited old comments from being treated as new.
async fn fetch_issue_comments(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    since: DateTime<Utc>,
) -> Result<Vec<IssueComment>> {
    let repo_full = github::repo_slug(owner, repo);
    // Embed `since` directly in the URL using the `Z` suffix (no special URL chars).
    // GitHub's `?since=` filters by `updated_at`; the local filter is the authoritative gate.
    let endpoint = format!(
        "repos/{repo_full}/issues/{pr_number}/comments?since={}",
        since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    let output = gh_api_with_retry(
        host,
        &["api", "--paginate", &endpoint, "--jq", ".[]"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to fetch issue comments for PR #{}: {}",
            pr_number,
            stderr
        );
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("Failed to decode issue comments stdout as UTF-8")?;

    let mut comments = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let comment: IssueComment =
            serde_json::from_str(line).context("Failed to parse issue comment JSON")?;
        comments.push(comment);
    }

    Ok(comments)
}

/// Filter issue comments to only those newer than `since` and not authored by this Minion.
fn filter_new_issue_comments(
    comments: &[IssueComment],
    since: DateTime<Utc>,
    minion_id: &str,
) -> Vec<IssueComment> {
    comments
        .iter()
        .filter(|c| c.created_at >= since && !has_minion_signature_for(&c.body, minion_id))
        .cloned()
        .collect()
}

/// Format new issue comments into a prompt for the Minion to respond to.
pub(crate) fn format_issue_comments_prompt(
    issue_num: Option<u64>,
    pr_number: &str,
    comments: &[IssueComment],
    owner: &str,
    repo: &str,
    minion_id: &str,
) -> String {
    let preamble = match issue_num {
        Some(n) => format!(
            "You previously implemented a fix for issue #{}. New comment(s) have been posted \
            on PR #{}.",
            n, pr_number
        ),
        None => format!("New comment(s) have been posted on PR #{}.", pr_number),
    };
    let mut prompt = format!("{} Please read and respond to the following:\n\n", preamble);

    for (i, comment) in comments.iter().enumerate() {
        let display = if comment.display_name.is_empty() {
            &comment.user.login
        } else {
            &comment.display_name
        };
        prompt.push_str(&format!("## Comment {}\n", i + 1));
        prompt.push_str(&format!(
            "**Author:** `{}` (@{})\n",
            display, comment.user.login
        ));
        prompt.push_str(&format!("**Comment:** {}\n\n", comment.body));
    }

    prompt.push_str(
        "Please address any questions or requests in these comments. \
        If code changes are needed, make them, run tests, and commit. \
        Post a reply on the PR thread using the GitHub API:\n\n\
        ```\n\
        gh api --method POST ",
    );
    prompt.push_str(&format!(
        "repos/{owner}/{repo}/issues/{pr_number}/comments \\\n  \
        -f body=$'<reply text>\\n\\n<sub>🤖 {minion_id}</sub>'\n\
        ```\n\n\
        Open by addressing the commenter using their display name shown above \
        (e.g., write `Alice Johnson,` or `M1ab,` — never `@login`). \
        End with the signature: `\\n\\n<sub>🤖 {minion_id}</sub>`"
    ));

    prompt
}

/// Filter a list of raw API review comments down to only those that need a
/// Minion reply, skipping:
/// - The current Minion's own reply comments (identified by its specific signature)
/// - Comments that the current Minion has already directly replied to
///   (identified by appearing as `in_reply_to_id` on a Minion reply comment)
///
/// `candidate_comments` is the set to filter (typically the comments drawn from
/// the new reviews being processed this cycle). `reply_sources` is the pool
/// scanned to build the `already_answered` set — passing the full set of PR
/// inline comments here (see `fetch_all_pr_inline_comments`) makes the filter
/// idempotent against replies from prior sessions or concurrent processes
/// (issue #866): even if a Minion reply lives in an implicit review that is
/// not part of the current review batch, the candidate it answered is still
/// dropped. Callers that don't need the PR-wide view can pass
/// `&candidate_comments` as `reply_sources` (same semantics as before).
///
/// Returns raw `ApiReviewComment`s so that display-name lookups can be
/// deferred until after filtering (avoiding API calls for already-answered or
/// Minion-authored threads).
fn filter_unanswered_comments(
    candidate_comments: Vec<ApiReviewComment>,
    reply_sources: &[ApiReviewComment],
    minion_id: &str,
) -> Vec<ApiReviewComment> {
    // Collect IDs of comments that this Minion has already directly replied to.
    let already_answered: std::collections::HashSet<u64> = reply_sources
        .iter()
        .filter_map(|c| {
            if has_minion_signature_for(&c.body, minion_id) {
                c.in_reply_to_id
            } else {
                None
            }
        })
        .collect();

    candidate_comments
        .into_iter()
        .filter(|c| {
            !has_minion_signature_for(&c.body, minion_id) && !already_answered.contains(&c.id)
        })
        .collect()
}

/// Identify duplicate inline review comment replies posted by this Minion.
///
/// A Minion is instructed (in `format_review_prompt`) to post exactly one
/// reply per inline comment thread, but prompt constraints are best-effort.
/// This backstop inspects the comments returned by the PR-comments endpoint
/// and, for each thread containing more than one Minion-signed reply created
/// at/after `since`, returns the IDs of all duplicates (all but the earliest
/// by `(created_at, id)` — `id` breaks ties when two replies share a
/// timestamp).
///
/// Only comments created at/after `since` are considered — this confines the
/// dedup to replies from the current review-response session and avoids ever
/// deleting replies from prior sessions whose correctness was implicitly
/// accepted by the human reviewer.
///
/// Orphan Minion-signed comments (no `in_reply_to_id`) are ignored here —
/// they don't belong to any inline thread and are out of scope for this
/// dedup.
///
/// Grouping caveat: `in_reply_to_id` is the direct parent comment being
/// replied to, which is not always the thread root. In practice, GitHub
/// normalizes most PR inline-comment replies to point at the root, but two
/// Minion replies pointing at different ancestors within the same thread
/// would land in different groups and not be deduped here. That is an
/// acceptable miss for this backstop — the guarantee is "no duplicates
/// *against the same parent*," which is what the prompt instructs.
fn identify_duplicate_minion_replies(
    api_comments: &[ApiReviewComment],
    minion_id: &str,
    since: DateTime<Utc>,
) -> Vec<u64> {
    use std::collections::HashMap;
    let mut groups: HashMap<u64, Vec<&ApiReviewComment>> = HashMap::new();
    for c in api_comments {
        if c.created_at < since {
            continue;
        }
        if !has_minion_signature_for(&c.body, minion_id) {
            continue;
        }
        let Some(parent) = c.in_reply_to_id else {
            continue;
        };
        groups.entry(parent).or_default().push(c);
    }

    let mut duplicates: Vec<u64> = Vec::new();
    for replies in groups.values_mut() {
        if replies.len() < 2 {
            continue;
        }
        replies.sort_by_key(|c| (c.created_at, c.id));
        // Keep the first; mark the rest for deletion.
        for dup in replies.iter().skip(1) {
            duplicates.push(dup.id);
        }
    }
    duplicates.sort_unstable();
    duplicates
}

/// Fetch every inline review comment on a PR (across all reviews) by
/// paginating `/pulls/{pr_number}/comments`.
///
/// `--paginate` without `--jq` concatenates raw JSON arrays across pages
/// (`[...][...]`), which is not valid JSON. Pair with `--jq ".[]"` to
/// stream one comment per line instead, matching the pattern used by
/// `get_check_runs`.
async fn fetch_all_pr_inline_comments(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<ApiReviewComment>> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/pulls/{pr_number}/comments");
    let output = gh_api_with_retry(
        host,
        &["api", "--paginate", &endpoint, "--jq", ".[]"],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        // `gh_api_with_retry` may return `Ok(output)` with a non-success exit
        // code when the error is not retryable. Convert that into an `Err`
        // here so callers can decide whether to hard-fail or fall back
        // (`get_review_feedback` logs and proceeds with the per-review set;
        // `dedup_minion_inline_replies` propagates the error).
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to fetch inline comments on {repo_full} PR #{pr_number}: {}",
            stderr
        );
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("Failed to decode PR inline comments stdout as UTF-8")?;
    let mut api_comments: Vec<ApiReviewComment> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let comment: ApiReviewComment =
            serde_json::from_str(line).context("Failed to parse PR inline comment JSON line")?;
        api_comments.push(comment);
    }
    Ok(api_comments)
}

/// Fetch all inline review comments on a PR and delete any duplicate replies
/// authored by this Minion in the current review-response session.
///
/// Returns the number of duplicate comments deleted (0 on a healthy run).
/// Errors from individual `DELETE` calls are logged and do not abort the
/// sweep — a partial dedup is strictly better than none, and the next
/// review-response cycle will catch any stragglers.
pub(crate) async fn dedup_minion_inline_replies(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    minion_id: &str,
    since: DateTime<Utc>,
) -> Result<usize> {
    let repo_full = github::repo_slug(owner, repo);
    let api_comments = fetch_all_pr_inline_comments(host, owner, repo, pr_number).await?;

    let duplicate_ids = identify_duplicate_minion_replies(&api_comments, minion_id, since);

    let mut deleted = 0usize;
    for id in &duplicate_ids {
        let delete_endpoint = format!("repos/{repo_full}/pulls/comments/{id}");
        let result = gh_api_with_retry(
            host,
            &["api", "--method", "DELETE", &delete_endpoint],
            DEFAULT_MAX_RETRIES,
        )
        .await;
        match result {
            Ok(out) if out.status.success() => {
                deleted += 1;
                log::info!(
                    "🧹 Deleted duplicate inline reply comment {} on PR #{}",
                    id,
                    pr_number
                );
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::warn!(
                    "⚠️  Failed to delete duplicate inline reply {}: {}",
                    id,
                    stderr.trim()
                );
            }
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to delete duplicate inline reply {}: {:#}",
                    id,
                    e
                );
            }
        }
    }

    Ok(deleted)
}

/// Fetch review bodies and inline comments for specific reviews with retry logic.
///
/// Raw inline comments are accumulated from *all* reviews first, then
/// `filter_unanswered_comments` is called once on the full set (Bug 2 fix).
async fn get_review_feedback(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    reviews: &[Review],
    minion_id: &str,
) -> Result<ReviewFeedback> {
    let repo_full = github::repo_slug(owner, repo);
    let mut raw_comments: Vec<ApiReviewComment> = Vec::new();
    let mut all_bodies = Vec::new();
    let mut failed_reviews = 0;
    // Memoize display-name lookups per login so that repeated reviews from
    // the same author don't each trigger a separate `gh` CLI invocation.
    let mut body_display_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for review in reviews {
        // Collect review body text (non-empty bodies that aren't just whitespace).
        // Skip DISMISSED reviews — their body is the dismissal reason, not
        // actionable feedback for the implementer.
        let state = if review.state.is_empty() {
            "COMMENTED"
        } else {
            &review.state
        };
        if state != "DISMISSED" {
            let trimmed = review.body.trim();
            if !trimmed.is_empty() {
                let reviewer_display_name =
                    if let Some(minion_id) = extract_minion_id_from_signature(trimmed) {
                        minion_id.to_owned()
                    } else {
                        let login = &review.user.login;
                        if !body_display_names.contains_key(login) {
                            let name = github::get_user_display_name(host, login).await;
                            body_display_names.insert(login.clone(), name);
                        }
                        body_display_names[login].clone()
                    };
                all_bodies.push(ReviewBody {
                    body: trimmed.to_string(),
                    reviewer: review.user.login.clone(),
                    reviewer_display_name,
                    state: state.to_string(),
                });
            }
        }

        // Fetch inline comments for this specific review with retry
        let endpoint = format!(
            "repos/{repo_full}/pulls/{pr_number}/reviews/{}/comments",
            review.id
        );
        let output = gh_api_with_retry(
            host,
            &["api", &endpoint, "--cache", "20s"],
            DEFAULT_MAX_RETRIES,
        )
        .await?;

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

        raw_comments.extend(api_comments);
    }

    // Log summary if any reviews failed to fetch
    if failed_reviews > 0 {
        log::warn!(
            "⚠️  Failed to fetch comments from {} out of {} review(s)",
            failed_reviews,
            reviews.len()
        );
    }

    // Pre-emptive idempotency check (issue #866): fetch every inline comment
    // on the PR — not just the ones in the new reviews being processed — so
    // that the `already_answered` set also catches Minion replies posted
    // during prior sessions or by a concurrent agent process. A comment the
    // Minion has already replied to anywhere on the PR is dropped from the
    // candidate set here, so it will not appear in the review prompt and will
    // not trigger a duplicate reply.
    //
    // Degrade gracefully on fetch failure: fall back to scanning only the
    // comments from the new reviews. A transient API failure should not
    // block review handling, and the existing post-hoc dedup
    // (`dedup_minion_inline_replies`) plus the registry lock still prevent
    // duplicates from persisting.
    let pr_wide_replies = match fetch_all_pr_inline_comments(host, owner, repo, pr_number).await {
        Ok(comments) => comments,
        Err(e) => {
            log::warn!(
                "⚠️  Failed to fetch PR-wide inline comments for idempotency check (falling back to per-review scan): {:#}",
                e
            );
            Vec::new()
        }
    };

    // Build the replies source from the PR-wide fetch when available, union'd
    // with `raw_comments` so the filter still works if the fetch returned
    // empty (fallback path above) but the per-review set contains Minion
    // replies from implicit reviews in the batch.
    let mut reply_sources: Vec<ApiReviewComment> = pr_wide_replies;
    // Avoid O(n²) dedup by tracking seen IDs; `raw_comments` may overlap with
    // `pr_wide_replies` when both include the same comment.
    let mut seen: std::collections::HashSet<u64> = reply_sources.iter().map(|c| c.id).collect();
    for c in &raw_comments {
        if seen.insert(c.id) {
            reply_sources.push(c.clone());
        }
    }

    // Filter all accumulated comments at once (Bug 2 fix: cross-review dedup).
    // Filtering happens before display-name lookup so we only call the API for
    // authors of comments that will actually be replied to.
    let unanswered_raw = filter_unanswered_comments(raw_comments, &reply_sources, minion_id);

    // Fetch display names only for unique authors of unanswered comments.
    let mut display_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for comment in &unanswered_raw {
        let login = &comment.user.login;
        if !display_names.contains_key(login) {
            let name = github::get_user_display_name(host, login).await;
            display_names.insert(login.clone(), name);
        }
    }

    // Convert filtered raw comments to ReviewComment with display names.
    let all_comments: Vec<ReviewComment> = unanswered_raw
        .into_iter()
        .map(|c| {
            // If the comment body carries a Minion signature, use that Minion ID
            // as the display name so sibling-Minion reviewers are addressed by
            // their Minion ID rather than their GitHub login.
            let display_name = extract_minion_id_from_signature(&c.body)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    // Every login in unanswered_raw was inserted into display_names
                    // above; the fallback is a defensive guard.
                    display_names
                        .get(&c.user.login)
                        .cloned()
                        .unwrap_or_else(|| c.user.login.clone())
                });
            ReviewComment {
                file: c.path,
                line: c.line,
                body: c.body,
                reviewer: c.user.login,
                reviewer_display_name: display_name,
                comment_id: c.id,
            }
        })
        .collect();

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
        prompt.push_str(&format!(
            "**Reviewer:** `{}` (@{})\n",
            review_body.reviewer_display_name, review_body.reviewer
        ));
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
            prompt.push_str(&format!(
                "**Reviewer:** `{}` (@{})\n",
                comment.reviewer_display_name, comment.reviewer
            ));
            prompt.push_str(&format!("**Comment ID:** {}\n", comment.comment_id));
            prompt.push_str(&format!("**Comment:** {}\n\n", comment.body));
        }
    }

    prompt.push_str(
        "Please make the requested changes, run tests, and commit.\n\n\
When addressing a reviewer in any reply, use the name shown in backticks in the \
**Reviewer:** line (e.g., write `Alice Johnson,` or `M0ab,` — never `@login`).\n\n",
    );

    // Instruct the agent to reply to top-level review bodies
    if !feedback.bodies.is_empty() {
        prompt.push_str(&format!(
            "After committing your changes, post a reply to the PR thread for each review body \
above to explain what you changed. Post EXACTLY ONE reply per review — do not post duplicate \
replies. Post each reply in a separate sequential step — do not batch reply API calls with each \
other or with git operations such as push.\n\n\
For each review body, post a PR comment using the GitHub API:\n\n\
```\n\
gh api --method POST repos/{owner}/{repo}/issues/{pr_number}/comments \\\n  \
-f body=$'<reply text>\\n\\n<sub>🤖 {minion_id}</sub>'\n\
```\n\n\
Each reply must:\n\
- Open by addressing the reviewer by the name shown in backticks in the **Reviewer:** line (e.g., `alice-dev,` — never `@alice-dev`)\n\
- Summarize what was changed to address the feedback\n\
- End with the signature: `\\n\\n<sub>🤖 {minion_id}</sub>`\n"
        ));
    }

    // Instruct the agent to reply to each inline review comment thread
    if !feedback.comments.is_empty() {
        prompt.push_str(&format!(
            "After committing your changes, reply to EACH inline review comment thread to explain what you changed. \
Post EXACTLY ONE reply per comment ID — do not post duplicate replies. \
Reply to each comment in a separate sequential step — do not batch reply API calls with each other \
or with git operations such as push. Reply to the comments in the order they appear above.\n\n\
For each comment, post an inline reply using the GitHub API:\n\n\
```\n\
gh api --method POST repos/{owner}/{repo}/pulls/{pr_number}/comments \\\n  \
-f body=$'<reply text>\\n\\n<sub>🤖 {minion_id}</sub>' \\\n  \
-F in_reply_to=<comment_id>\n\
```\n\n\
Where `<comment_id>` is the Comment ID listed above for each inline review comment. \
Each reply must:\n\
- Open by addressing the reviewer by the name shown in backticks in the **Reviewer:** line (e.g., `Alice Johnson,` or `alice-dev,`)\n\
- Summarize what was changed to address the feedback\n\
- End with the signature: `\\n\\n<sub>🤖 {minion_id}</sub>`\n"
        ));
    }

    prompt
}

/// Fetch check runs for a given commit SHA with retry logic for transient failures.
///
/// Uses `--jq ".check_runs[]"` to extract individual check run objects line-by-line,
/// which is resilient to wrapper structure changes and handles pagination.
/// Note: the check-runs endpoint wraps results in a `.check_runs` field (hence
/// `.check_runs[]`), unlike the reviews endpoint which returns a bare array (`.[]`).
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

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch check runs: {}", stderr);
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("Failed to decode check runs stdout as UTF-8")?;

    let mut check_runs = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: RawCheckRun =
            serde_json::from_str(line).context("Failed to parse check run JSON line")?;
        check_runs.push(CheckRun::from(raw));
    }

    Ok(check_runs)
}

/// Returns the count of external reviews submitted after `since` that are not
/// authored by the current Minion.
///
/// Uses the same signature-based identity check as `filter_new_external_reviews`:
/// a review is considered authored by the current Minion when its body ends with
/// `<sub>🤖 {minion_id}</sub>`.  This ensures that reviews by a sibling Minion
/// (which shares the same `user.login`) are counted as unaddressed feedback,
/// matching the wake-up semantics in the lab scanner.
///
/// Uses exclusive `>` (vs. `filter_new_external_reviews`'s inclusive `>=`) because
/// this function is called against a baseline that was set *at* the last check,
/// and reviews at exactly that timestamp were already processed.
pub(crate) fn count_unaddressed_reviews(
    reviews: &[Review],
    minion_id: &str,
    since: DateTime<Utc>,
) -> usize {
    reviews
        .iter()
        .filter(|r| r.submitted_at > since && !has_minion_signature_for(&r.body, minion_id))
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
/// Returns `true` if the PR is still open.
pub(crate) async fn get_pr_info_for_exit_notification(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<bool> {
    let pr = get_pr(host, owner, repo, pr_number).await?;
    // A merged PR has state="closed" AND merged=true. Check both to guard
    // against the narrow race where state hasn't propagated yet.
    Ok(pr.state != "closed" && !pr.merged)
}

/// Fetch PR info needed for the lab daemon's wake-up scan.
///
/// Returns `(is_open, pr_author_login, mergeable)`.
/// `mergeable` is `Some(false)` when GitHub detects merge conflicts,
/// `Some(true)` when clean, or `None` when GitHub is still computing.
pub(crate) async fn get_pr_info_for_wake_check(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<(bool, String, Option<bool>)> {
    let pr = get_pr(host, owner, repo, pr_number).await?;
    let is_open = pr.state != "closed" && !pr.merged;
    Ok((is_open, pr.user.login, pr.mergeable))
}

/// Fetch the creation timestamp of a PR from the GitHub API.
///
/// Used as a review-baseline fallback when the minion resumes after a crash
/// and no persisted `last_review_check_time` is available.
pub(crate) async fn get_pr_created_at(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<DateTime<Utc>> {
    let pr = get_pr(host, owner, repo, pr_number).await?;
    Ok(pr.created_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pull_request_has_label() {
        let json = r#"{
            "state": "open", "merged": false,
            "head": {"sha": "abc123"}, "user": {"login": "octocat"},
            "created_at": "2024-01-01T00:00:00Z",
            "labels": [{"name": "gru:auto-merge"}, {"name": "bug"}]
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.has_label("gru:auto-merge"));
        assert!(pr.has_label("bug"));
        assert!(!pr.has_label("gru:ready-to-merge"));
    }

    #[test]
    fn test_pull_request_labels_default_empty() {
        let json = r#"{
            "state": "open", "merged": false,
            "head": {"sha": "abc123"}, "user": {"login": "octocat"},
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.labels.is_empty());
        assert!(!pr.has_label("anything"));
    }

    #[test]
    fn test_format_review_prompt_single_comment() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/main.rs".to_string(),
                line: Some(45),
                body: "This function needs error handling for null inputs.".to_string(),
                reviewer: "alice".to_string(),
                reviewer_display_name: "alice".to_string(),
                comment_id: 1001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(
            Some(123),
            "456",
            &feedback,
            "octocat",
            "hello-world",
            "M042",
        );

        assert!(prompt.contains("issue #123"));
        assert!(prompt.contains("PR #456"));
        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**Reviewer:** `alice` (@alice)"));
        assert!(prompt.contains("**Comment ID:** 1001"));
        assert!(prompt.contains("This function needs error handling for null inputs."));
        assert!(prompt.contains("Please make the requested changes, run tests, and commit."));
        assert!(prompt.contains("in_reply_to"));
        assert!(prompt.contains("repos/octocat/hello-world/pulls/456/comments"));
        assert!(prompt.contains("<sub>🤖 M042</sub>"));
        assert!(prompt.contains("Open by addressing the reviewer"));
        // Anti-duplication instructions must be present
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
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
                    reviewer_display_name: "alice".to_string(),
                    comment_id: 2001,
                },
                ReviewComment {
                    file: "tests/test_main.rs".to_string(),
                    line: Some(12),
                    body: "Add a test case for the edge case.".to_string(),
                    reviewer: "bob".to_string(),
                    reviewer_display_name: "bob".to_string(),
                    comment_id: 2002,
                },
            ],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(
            Some(123),
            "456",
            &feedback,
            "octocat",
            "hello-world",
            "M042",
        );

        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("## Inline Comment 2"));
        assert!(prompt.contains("**File:** src/main.rs:45"));
        assert!(prompt.contains("**File:** tests/test_main.rs:12"));
        assert!(prompt.contains("`alice` (@alice)"));
        assert!(prompt.contains("`bob` (@bob)"));
        assert!(prompt.contains("**Comment ID:** 2001"));
        assert!(prompt.contains("**Comment ID:** 2002"));
        assert!(prompt.contains("in_reply_to"));
        assert!(prompt.contains("End with the signature"));
        // Anti-duplication instructions must be present
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
    }

    #[test]
    fn test_format_review_prompt_with_display_name() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/lib.rs".to_string(),
                line: Some(10),
                body: "Please add a screenshot.".to_string(),
                reviewer: "sspalding".to_string(),
                reviewer_display_name: "Stephen Spalding".to_string(),
                comment_id: 4001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt =
            format_review_prompt(Some(786), "42", &feedback, "octocat", "hello-world", "M001");

        // Display name wrapped in backticks with login in parens
        assert!(prompt.contains("**Reviewer:** `Stephen Spalding` (@sspalding)"));
        // Login-only rendering should NOT appear when a distinct display name is available
        assert!(!prompt.contains("**Reviewer:** `sspalding` (@sspalding)"));
        // Reply instruction should tell Claude to address by display name
        assert!(prompt.contains("Open by addressing the reviewer"));
        // Anti-duplication instructions must be present for inline comments
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
    }

    #[test]
    fn test_format_review_prompt_no_line_number() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "README.md".to_string(),
                line: None,
                body: "Update the documentation.".to_string(),
                reviewer: "charlie".to_string(),
                reviewer_display_name: "charlie".to_string(),
                comment_id: 3001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(
            Some(123),
            "456",
            &feedback,
            "octocat",
            "hello-world",
            "M042",
        );

        // Should not have a colon if line is None
        assert!(prompt.contains("**File:** README.md\n"));
        assert!(!prompt.contains("README.md:"));
        // Anti-duplication instructions must be present for inline comments
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
    }

    #[test]
    fn test_format_review_prompt_body_only() {
        let feedback = ReviewFeedback {
            comments: vec![],
            bodies: vec![ReviewBody {
                body: "Consider refactoring the error handling to use Result types.".to_string(),
                reviewer: "dave".to_string(),
                reviewer_display_name: "dave".to_string(),
                state: "COMMENTED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt =
            format_review_prompt(Some(42), "99", &feedback, "octocat", "hello-world", "M001");

        assert!(prompt.contains("issue #42"));
        assert!(prompt.contains("PR #99"));
        assert!(prompt.contains("## Review 1 (COMMENTED)"));
        assert!(prompt.contains("**Reviewer:** `dave` (@dave)"));
        assert!(prompt.contains("Consider refactoring the error handling"));
        assert!(prompt.contains("Please make the requested changes"));
        // Review body present: should include signature instruction and PR comment API
        assert!(prompt.contains("<sub>🤖 M001</sub>"));
        assert!(prompt.contains("repos/octocat/hello-world/issues/99/comments"));
        // No inline comments, so no in_reply_to or inline-specific strings
        assert!(!prompt.contains("in_reply_to"));
        assert!(!prompt.contains("EXACTLY ONE reply per comment ID"));
    }

    #[test]
    fn test_format_review_prompt_body_with_display_name() {
        // When a reviewer has a display name different from their login,
        // review body replies should use the display name.
        let feedback = ReviewFeedback {
            comments: vec![],
            bodies: vec![ReviewBody {
                body: "Please add more error handling.".to_string(),
                reviewer: "sspalding".to_string(),
                reviewer_display_name: "Stephen Spalding".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(10), "20", &feedback, "owner", "repo", "M1e3");

        assert!(prompt.contains("**Reviewer:** `Stephen Spalding` (@sspalding)"));
    }

    #[test]
    fn test_format_review_prompt_body_and_comments() {
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/lib.rs".to_string(),
                line: Some(10),
                body: "Fix this line.".to_string(),
                reviewer: "eve".to_string(),
                reviewer_display_name: "eve".to_string(),
                comment_id: 6001,
            }],
            bodies: vec![ReviewBody {
                body: "Overall looks good but needs some tweaks.".to_string(),
                reviewer: "eve".to_string(),
                reviewer_display_name: "eve".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt =
            format_review_prompt(Some(10), "20", &feedback, "octocat", "hello-world", "M002");

        // Review body appears
        assert!(prompt.contains("## Review 1 (CHANGES_REQUESTED)"));
        assert!(prompt.contains("Overall looks good"));
        // Inline comment appears
        assert!(prompt.contains("## Inline Comment 1"));
        assert!(prompt.contains("**File:** src/lib.rs:10"));
        assert!(prompt.contains("Fix this line."));
        // Body reply instructions present (issues endpoint, signature)
        assert!(prompt.contains("repos/octocat/hello-world/issues/20/comments"));
        assert!(prompt.contains("EXACTLY ONE reply per review"));
        // Inline comment reply instructions present
        assert!(prompt.contains("in_reply_to"));
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
    }

    #[test]
    fn test_format_review_prompt_minion_reviewer_in_body() {
        // When a review body carries a Minion signature, the reviewer_display_name
        // should be the Minion ID and the prompt should address it without `@`.
        let feedback = ReviewFeedback {
            comments: vec![],
            bodies: vec![ReviewBody {
                body: "Looks good overall.\n\n<sub>🤖 M1cu</sub>".to_string(),
                reviewer: "fotoetienne".to_string(),
                reviewer_display_name: "M1cu".to_string(),
                state: "APPROVED".to_string(),
            }],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(10), "20", &feedback, "owner", "repo", "M1cx");

        // Minion ID appears in backticks, not preceded by @
        assert!(prompt.contains("**Reviewer:** `M1cu` (@fotoetienne)"));
        // The login should not appear as the sole name
        assert!(!prompt.contains("**Reviewer:** @fotoetienne"));
        // Addressing instruction should guide Claude to use M1cu, not @login
        assert!(prompt.contains("use the name shown in backticks"));
    }

    #[test]
    fn test_format_review_prompt_minion_reviewer_in_inline_comment() {
        // When an inline comment body carries a Minion signature, reviewer_display_name
        // should be the Minion ID.
        let feedback = ReviewFeedback {
            comments: vec![ReviewComment {
                file: "src/main.rs".to_string(),
                line: Some(5),
                body: "Please rename this variable.\n\n<sub>🤖 M1ab</sub>".to_string(),
                reviewer: "fotoetienne".to_string(),
                reviewer_display_name: "M1ab".to_string(),
                comment_id: 9001,
            }],
            bodies: vec![],
            had_fetch_failures: false,
        };

        let prompt = format_review_prompt(Some(10), "20", &feedback, "owner", "repo", "M1cx");

        assert!(prompt.contains("**Reviewer:** `M1ab` (@fotoetienne)"));
        assert!(!prompt.contains("**Reviewer:** `fotoetienne`"));
        // Anti-duplication instructions must be present for inline comments
        assert!(prompt.contains("EXACTLY ONE reply per comment ID"));
        assert!(prompt.contains("in a separate sequential step"));
        assert!(prompt.contains("do not batch reply API calls"));
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
            "user": {"login": "author"},
            "created_at": "2024-01-01T00:00:00Z"
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
            "user": {"login": "author"},
            "created_at": "2024-01-01T00:00:00Z"
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
            "user": {"login": "author"},
            "created_at": "2024-01-01T00:00:00Z"
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
        assert_eq!(
            pr.created_at,
            "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
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
        assert_eq!(review.body, "");
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
        assert_eq!(review.body, "Please fix this");
    }

    #[test]
    fn test_review_list_deserialize() {
        // Simulates `gh api --paginate ... --jq ".[]"` output: one JSON object
        // per line, with surrounding whitespace and blank lines that should be
        // ignored — matching the actual parsing logic in get_all_reviews.
        let lines = "
            {\"id\": 1, \"submitted_at\": \"2024-06-15T10:30:00Z\", \"user\": {\"login\": \"alice\"}, \"body\": \"first review\"}

              {\"id\": 2, \"submitted_at\": \"2024-06-15T11:30:00Z\", \"user\": {\"login\": \"bob\"}, \"body\": \"second review\"}
        ";

        let reviews: Vec<Review> = lines
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].id, 1);
        assert_eq!(reviews[0].body, "first review");
        assert_eq!(reviews[1].id, 2);
        assert_eq!(reviews[1].body, "second review");
    }

    #[test]
    fn test_review_line_by_line_empty() {
        // Simulates --jq ".[]" output when there are no reviews
        let lines = "";

        let reviews: Vec<Review> = lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

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
    fn test_check_run_line_by_line_deserialize() {
        use crate::ci::CheckConclusion;
        // Simulates --jq ".check_runs[]" output: one JSON object per line
        let lines = r#"{"conclusion": "success"}
{"conclusion": "failure"}
{"conclusion": null}"#;

        let check_runs: Vec<CheckRun> = lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(check_runs.len(), 3);
        assert_eq!(check_runs[0].conclusion, Some(CheckConclusion::Success));
        assert_eq!(check_runs[1].conclusion, Some(CheckConclusion::Failure));
        assert!(check_runs[2].conclusion.is_none());
    }

    #[test]
    fn test_check_run_line_by_line_empty() {
        // Simulates --jq ".check_runs[]" output when there are no check runs
        let lines = "";

        let check_runs: Vec<CheckRun> = lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert!(check_runs.is_empty());
    }

    #[test]
    fn test_check_runs_response_with_output_object() {
        let json = r#"{
            "total_count": 1,
            "check_runs": [{
                "name": "build",
                "status": "completed",
                "conclusion": "failure",
                "output": {
                    "title": "Build failed",
                    "summary": "error[E0433]: failed to resolve",
                    "text": null,
                    "annotations_count": 1,
                    "annotations_url": "https://api.github.com/repos/o/r/check-runs/1/annotations"
                }
            }]
        }"#;

        let response: CheckRunsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.check_runs.len(), 1);
        let check: CheckRun = response.check_runs.into_iter().next().unwrap().into();
        assert_eq!(check.name, "build");
        assert_eq!(check.conclusion, Some(crate::ci::CheckConclusion::Failure));
        assert_eq!(
            check.output.as_deref(),
            Some("Build failed\n\nerror[E0433]: failed to resolve")
        );
    }

    #[test]
    fn test_check_runs_response_with_null_output() {
        let json = r#"{
            "total_count": 1,
            "check_runs": [{"conclusion": "success", "output": null}]
        }"#;

        let response: CheckRunsResponse = serde_json::from_str(json).unwrap();
        let check: CheckRun = response.check_runs.into_iter().next().unwrap().into();
        assert!(check.output.is_none());
    }

    // ========================================================================
    // Real GitHub API Payload Fixture Tests
    //
    // These tests use payloads captured from the live GitHub API to ensure
    // our structs can deserialize real-world responses, not just minimal
    // hand-crafted JSON. This prevents type mismatches like `output` being
    // an object instead of a string from going undetected.
    // ========================================================================

    /// Real payload from `gh api repos/OWNER/REPO/commits/SHA/check-runs`
    /// with fields like `output` (object), `app`, `check_suite`, `pull_requests`.
    const REAL_CHECK_RUN_API_RESPONSE: &str = r#"{
        "total_count": 2,
        "check_runs": [
            {
                "id": 38771234567,
                "name": "Check",
                "node_id": "CR_kwDOExample",
                "head_sha": "abc123def456789",
                "external_id": "",
                "url": "https://api.github.com/repos/owner/repo/check-runs/38771234567",
                "html_url": "https://github.com/owner/repo/runs/38771234567",
                "details_url": "https://github.com/owner/repo/actions/runs/987654321/job/38771234567",
                "status": "completed",
                "conclusion": "success",
                "started_at": "2025-06-15T10:00:00Z",
                "completed_at": "2025-06-15T10:05:34Z",
                "output": {
                    "title": "Build passed",
                    "summary": "All 42 checks passed",
                    "text": null,
                    "annotations_count": 0,
                    "annotations_url": "https://api.github.com/repos/owner/repo/check-runs/38771234567/annotations"
                },
                "app": {
                    "id": 15368,
                    "slug": "github-actions",
                    "node_id": "MDM6QXBwMTUzNjg=",
                    "owner": {
                        "login": "github",
                        "id": 9919,
                        "type": "Organization"
                    },
                    "name": "GitHub Actions",
                    "description": "Automate your workflow from idea to production",
                    "external_url": "https://help.github.com/en/actions",
                    "html_url": "https://github.com/apps/github-actions",
                    "created_at": "2018-07-30T09:30:17Z",
                    "updated_at": "2019-12-10T19:04:12Z"
                },
                "check_suite": {
                    "id": 33344455566,
                    "node_id": "CS_kwDOExample",
                    "head_branch": "feature/new-thing",
                    "head_sha": "abc123def456789",
                    "status": "completed",
                    "conclusion": "success"
                },
                "pull_requests": [
                    {
                        "url": "https://api.github.com/repos/owner/repo/pulls/123",
                        "id": 2012345678,
                        "number": 123,
                        "head": {
                            "ref": "feature/new-thing",
                            "sha": "abc123def456789"
                        },
                        "base": {
                            "ref": "main",
                            "sha": "000111222333444"
                        }
                    }
                ]
            },
            {
                "id": 38771234999,
                "name": "Lint",
                "node_id": "CR_kwDOExample2",
                "head_sha": "abc123def456789",
                "external_id": "",
                "url": "https://api.github.com/repos/owner/repo/check-runs/38771234999",
                "html_url": "https://github.com/owner/repo/runs/38771234999",
                "details_url": "https://github.com/owner/repo/actions/runs/987654321/job/38771234999",
                "status": "completed",
                "conclusion": "failure",
                "started_at": "2025-06-15T10:00:00Z",
                "completed_at": "2025-06-15T10:02:11Z",
                "output": {
                    "title": "Lint check failed",
                    "summary": "Found 3 warnings",
                    "text": "src/main.rs:10: unused variable `x`\nsrc/lib.rs:20: missing docs",
                    "annotations_count": 3,
                    "annotations_url": "https://api.github.com/repos/owner/repo/check-runs/38771234999/annotations"
                },
                "app": {
                    "id": 15368,
                    "slug": "github-actions",
                    "node_id": "MDM6QXBwMTUzNjg=",
                    "owner": {
                        "login": "github",
                        "id": 9919,
                        "type": "Organization"
                    },
                    "name": "GitHub Actions",
                    "description": "Automate your workflow from idea to production",
                    "external_url": "https://help.github.com/en/actions",
                    "html_url": "https://github.com/apps/github-actions",
                    "created_at": "2018-07-30T09:30:17Z",
                    "updated_at": "2019-12-10T19:04:12Z"
                },
                "check_suite": {
                    "id": 33344455566,
                    "node_id": "CS_kwDOExample",
                    "head_branch": "feature/new-thing",
                    "head_sha": "abc123def456789",
                    "status": "completed",
                    "conclusion": "failure"
                },
                "pull_requests": []
            }
        ]
    }"#;

    /// A single raw check run object from the GitHub API (not jq-filtered).
    /// Exercises deserialization of the full API shape including nested `output` object.
    const REAL_CHECK_RUN_RAW_API_OBJECT: &str = r#"{"id":38771234567,"name":"Check","node_id":"CR_kwDOExample","head_sha":"abc123def456789","external_id":"","url":"https://api.github.com/repos/owner/repo/check-runs/38771234567","html_url":"https://github.com/owner/repo/runs/38771234567","details_url":"https://github.com/owner/repo/actions/runs/987654321/job/38771234567","status":"completed","conclusion":"success","started_at":"2025-06-15T10:00:00Z","completed_at":"2025-06-15T10:05:34Z","output":{"title":"Build passed","summary":"All 42 checks passed","text":null,"annotations_count":0,"annotations_url":"https://api.github.com/repos/owner/repo/check-runs/38771234567/annotations"},"app":{"id":15368,"slug":"github-actions","node_id":"MDM6QXBwMTUzNjg=","owner":{"login":"github","id":9919,"type":"Organization"},"name":"GitHub Actions","description":"Automate your workflow from idea to production","external_url":"https://help.github.com/en/actions","html_url":"https://github.com/apps/github-actions","created_at":"2018-07-30T09:30:17Z","updated_at":"2019-12-10T19:04:12Z"},"check_suite":{"id":33344455566,"node_id":"CS_kwDOExample","head_branch":"feature/new-thing","head_sha":"abc123def456789","status":"completed","conclusion":"success"},"pull_requests":[{"url":"https://api.github.com/repos/owner/repo/pulls/123","id":2012345678,"number":123,"head":{"ref":"feature/new-thing","sha":"abc123def456789"},"base":{"ref":"main","sha":"000111222333444"}}]}"#;

    /// Real payload from `gh api repos/OWNER/REPO/pulls/NUMBER/reviews`
    const REAL_REVIEWS_API_RESPONSE: &str = r#"[
        {
            "id": 2456789012,
            "node_id": "PRR_kwDOExample",
            "user": {
                "login": "reviewer-alice",
                "id": 12345678,
                "node_id": "MDQ6VXNlcjEyMzQ1Njc4",
                "avatar_url": "https://avatars.githubusercontent.com/u/12345678?v=4",
                "gravatar_id": "",
                "url": "https://api.github.com/users/reviewer-alice",
                "html_url": "https://github.com/reviewer-alice",
                "type": "User",
                "site_admin": false
            },
            "body": "Looks good overall, just a few nits.",
            "state": "CHANGES_REQUESTED",
            "html_url": "https://github.com/owner/repo/pull/123#pullrequestreview-2456789012",
            "pull_request_url": "https://api.github.com/repos/owner/repo/pulls/123",
            "author_association": "COLLABORATOR",
            "submitted_at": "2025-06-15T14:30:00Z",
            "commit_id": "abc123def456789"
        },
        {
            "id": 2456789999,
            "node_id": "PRR_kwDOExample2",
            "user": {
                "login": "reviewer-bob",
                "id": 87654321,
                "node_id": "MDQ6VXNlcjg3NjU0MzIx",
                "avatar_url": "https://avatars.githubusercontent.com/u/87654321?v=4",
                "gravatar_id": "",
                "url": "https://api.github.com/users/reviewer-bob",
                "html_url": "https://github.com/reviewer-bob",
                "type": "User",
                "site_admin": false
            },
            "body": "",
            "state": "APPROVED",
            "html_url": "https://github.com/owner/repo/pull/123#pullrequestreview-2456789999",
            "pull_request_url": "https://api.github.com/repos/owner/repo/pulls/123",
            "author_association": "MEMBER",
            "submitted_at": "2025-06-16T09:15:00Z",
            "commit_id": "abc123def456789"
        }
    ]"#;

    #[test]
    fn test_check_runs_response_real_api_payload() {
        use crate::ci::{CheckConclusion, CheckStatus};

        let response: CheckRunsResponse =
            serde_json::from_str(REAL_CHECK_RUN_API_RESPONSE).unwrap();

        assert_eq!(response.check_runs.len(), 2);

        // Convert RawCheckRun → CheckRun to test the full deserialization pipeline
        let checks: Vec<CheckRun> = response
            .check_runs
            .into_iter()
            .map(CheckRun::from)
            .collect();

        // First check run: success — output object has title + summary (text is null)
        let check = &checks[0];
        assert_eq!(check.name, "Check");
        assert_eq!(check.status, CheckStatus::Completed);
        assert_eq!(check.conclusion, Some(CheckConclusion::Success));
        assert_eq!(
            check.output,
            Some("Build passed\n\nAll 42 checks passed".to_string())
        );

        // Second check run: failure — output object has title + summary + text
        let check = &checks[1];
        assert_eq!(check.name, "Lint");
        assert_eq!(check.status, CheckStatus::Completed);
        assert_eq!(check.conclusion, Some(CheckConclusion::Failure));
        assert_eq!(
            check.output,
            Some("Lint check failed\n\nFound 3 warnings\n\nsrc/main.rs:10: unused variable `x`\nsrc/lib.rs:20: missing docs".to_string())
        );
    }

    #[test]
    fn test_check_run_raw_api_object_deserialize() {
        use crate::ci::{CheckConclusion, CheckStatus};

        // Deserialize as RawCheckRun then convert, matching the production code path
        let raw: RawCheckRun = serde_json::from_str(REAL_CHECK_RUN_RAW_API_OBJECT).unwrap();
        let check = CheckRun::from(raw);

        assert_eq!(check.name, "Check");
        assert_eq!(check.status, CheckStatus::Completed);
        assert_eq!(check.conclusion, Some(CheckConclusion::Success));
        // output object's title + summary extracted (text is null, so excluded)
        assert_eq!(
            check.output,
            Some("Build passed\n\nAll 42 checks passed".to_string())
        );
    }

    #[test]
    fn test_reviews_real_api_payload() {
        let reviews: Vec<Review> = serde_json::from_str(REAL_REVIEWS_API_RESPONSE).unwrap();

        assert_eq!(reviews.len(), 2);

        assert_eq!(reviews[0].id, 2456789012);
        assert_eq!(reviews[0].user.login, "reviewer-alice");
        assert_eq!(
            reviews[0].submitted_at,
            "2025-06-15T14:30:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        );

        assert_eq!(reviews[1].id, 2456789999);
        assert_eq!(reviews[1].user.login, "reviewer-bob");
    }

    #[test]
    fn test_check_run_output_string_still_works() {
        // Ensure that when output is a string (e.g., set programmatically),
        // deserialization still works correctly.
        let json = r#"{
            "name": "test-check",
            "status": "completed",
            "conclusion": "failure",
            "output": "Build failed: exit code 1"
        }"#;

        let check: CheckRun = serde_json::from_str(json).unwrap();
        assert_eq!(check.output, Some("Build failed: exit code 1".to_string()));
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

    #[test]
    fn test_api_review_comment_deserialize_with_in_reply_to_id() {
        let json = r#"{
            "id": 300,
            "path": "src/lib.rs",
            "line": 10,
            "body": "Great point",
            "user": {"login": "reviewer"},
            "in_reply_to_id": 100
        }"#;

        let comment: ApiReviewComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.in_reply_to_id, Some(100));
    }

    #[test]
    fn test_api_review_comment_deserialize_without_in_reply_to_id() {
        // Root comments don't have in_reply_to_id; field should default to None
        let json = r#"{
            "id": 100,
            "path": "src/lib.rs",
            "line": 10,
            "body": "Please fix this",
            "user": {"login": "reviewer"}
        }"#;

        let comment: ApiReviewComment = serde_json::from_str(json).unwrap();
        assert!(comment.in_reply_to_id.is_none());
    }

    fn make_api_comment(id: u64, body: &str, in_reply_to_id: Option<u64>) -> ApiReviewComment {
        make_api_comment_at(id, body, in_reply_to_id, "2024-01-01T00:00:00Z")
    }

    fn make_api_comment_at(
        id: u64,
        body: &str,
        in_reply_to_id: Option<u64>,
        created_at: &str,
    ) -> ApiReviewComment {
        ApiReviewComment {
            id,
            path: "src/main.rs".to_string(),
            line: Some(1),
            body: body.to_string(),
            user: User {
                login: "reviewer".to_string(),
            },
            in_reply_to_id,
            created_at: created_at.parse().unwrap(),
        }
    }

    #[test]
    fn test_filter_unanswered_comments_thread_fully_answered() {
        // Root comment + Minion reply → nothing returned
        let original = make_api_comment(1, "Please fix this.", None);
        let minion_reply = make_api_comment(2, "Done!\n\n<sub>🤖 M001</sub>", Some(1));

        let sources = vec![original.clone(), minion_reply.clone()];
        let result = filter_unanswered_comments(vec![original, minion_reply], &sources, "M001");
        assert!(
            result.is_empty(),
            "Already-answered comment should be filtered out"
        );
    }

    #[test]
    fn test_filter_unanswered_comments_one_answered_one_not() {
        // Two root comments; only one has a Minion reply → only the unanswered one returned
        let answered_root = make_api_comment(1, "Fix the typo.", None);
        let unanswered_root = make_api_comment(2, "Add error handling.", None);
        let minion_reply = make_api_comment(3, "Fixed!\n\n<sub>🤖 M001</sub>", Some(1));

        let sources = vec![
            answered_root.clone(),
            unanswered_root.clone(),
            minion_reply.clone(),
        ];
        let result = filter_unanswered_comments(
            vec![answered_root, unanswered_root, minion_reply],
            &sources,
            "M001",
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 2);
        assert_eq!(result[0].body, "Add error handling.");
    }

    #[test]
    fn test_filter_unanswered_comments_empty_input() {
        let result = filter_unanswered_comments(vec![], &[], "M001");
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_unanswered_comments_minion_reply_without_in_reply_to_id() {
        // A Minion-signed comment with no in_reply_to_id should be filtered out
        // but should not cause anything to be added to already_answered.
        let orphan_minion = make_api_comment(1, "Done!\n\n<sub>🤖 M001</sub>", None);
        let unrelated = make_api_comment(2, "Please fix this.", None);

        let sources = vec![orphan_minion.clone(), unrelated.clone()];
        let result = filter_unanswered_comments(vec![orphan_minion, unrelated], &sources, "M001");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 2);
    }

    // ========================================================================
    // Idempotency tests (issue #866): replies from a separate PR-wide source
    // ========================================================================

    #[test]
    fn test_filter_unanswered_comments_reply_in_external_source_only() {
        // The candidate set contains only the reviewer's root comment (e.g. a
        // fresh batch of new reviews). The Minion's prior reply lives in the
        // PR-wide sources fetch (an implicit review from a previous session or
        // a concurrent process). The candidate must still be dropped.
        let original = make_api_comment(1, "Please fix this.", None);
        let prior_minion_reply_elsewhere =
            make_api_comment(42, "Done!\n\n<sub>🤖 M001</sub>", Some(1));

        let sources = vec![original.clone(), prior_minion_reply_elsewhere];
        let result = filter_unanswered_comments(vec![original], &sources, "M001");
        assert!(
            result.is_empty(),
            "Comment answered in the PR-wide source must be filtered from candidates"
        );
    }

    #[test]
    fn test_filter_unanswered_comments_sibling_minion_reply_ignored() {
        // A reply signed by a sibling Minion (different ID) does NOT mark the
        // comment as answered for this Minion — each Minion's idempotency
        // check runs against its own signature only.
        let original = make_api_comment(1, "Please fix this.", None);
        let sibling_reply = make_api_comment(2, "Done!\n\n<sub>🤖 M999</sub>", Some(1));

        let sources = vec![original.clone(), sibling_reply];
        let result = filter_unanswered_comments(vec![original], &sources, "M001");
        assert_eq!(result.len(), 1, "Sibling Minion's reply must not suppress");
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn test_filter_unanswered_comments_race_regression_862() {
        // Regression for #862: two agent processes race to respond to the
        // same review thread. The first one posts its reply; by the time the
        // second fetches the PR state, that reply is visible in the PR-wide
        // sources — so the second process must skip this candidate rather
        // than post a duplicate.
        let root = make_api_comment(10, "Needs a test.", None);
        // Raced reply that landed first, posted by the OTHER process (same
        // Minion identity — both processes share the Minion ID).
        let winner_reply = make_api_comment(11, "Added a test.\n\n<sub>🤖 M1jc</sub>", Some(10));

        // Second process's candidate list (freshly fetched from the review)
        // still contains the root comment; its PR-wide fetch contains the
        // winner's reply.
        let sources = vec![root.clone(), winner_reply];
        let result = filter_unanswered_comments(vec![root], &sources, "M1jc");
        assert!(
            result.is_empty(),
            "Second racing process must no-op when the winner has already replied"
        );
    }

    #[test]
    fn test_filter_unanswered_comments_fixture_parses_gh_api_response() {
        // Exercise the actual parse path used by `fetch_all_pr_inline_comments`
        // in production: `gh api --paginate --jq ".[]"` emits one JSON object
        // per line, and the function calls `serde_json::from_str` on each
        // trimmed line. The fixture below matches that shape so the test
        // catches drift in the `ApiReviewComment` per-line deserialization
        // contract, not just top-level array deserialization.
        let fixture = r#"{"id":100,"path":"src/lib.rs","line":10,"body":"Please add a test.","user":{"login":"reviewer"},"created_at":"2024-06-15T10:00:00Z"}
{"id":200,"path":"src/lib.rs","line":10,"body":"Added a test.\n\n<sub>🤖 M1jc</sub>","user":{"login":"minion-bot"},"in_reply_to_id":100,"created_at":"2024-06-15T10:30:00Z"}
{"id":300,"path":"src/main.rs","line":5,"body":"Rename this variable.","user":{"login":"reviewer"},"created_at":"2024-06-15T11:00:00Z"}"#;

        let mut sources: Vec<ApiReviewComment> = Vec::new();
        for line in fixture.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            sources.push(serde_json::from_str(line).unwrap());
        }

        // Candidates include both root comments (100 answered, 300 unanswered).
        let candidates = vec![sources[0].clone(), sources[2].clone()];
        let result = filter_unanswered_comments(candidates, &sources, "M1jc");
        assert_eq!(result.len(), 1, "Only the unanswered root should remain");
        assert_eq!(result[0].id, 300);
    }

    // ========================================================================
    // identify_duplicate_minion_replies tests
    // ========================================================================

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn test_identify_duplicates_empty_no_duplicates() {
        let comments = vec![
            make_api_comment(1, "Please fix this.", None),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert!(dups.is_empty());
    }

    #[test]
    fn test_identify_duplicates_keeps_earliest_by_created_at() {
        // Two Minion replies to the same thread; the later one is a duplicate.
        let comments = vec![
            make_api_comment(1, "Please fix this.", None),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
            make_api_comment_at(
                3,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:05:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert_eq!(dups, vec![3]);
    }

    #[test]
    fn test_identify_duplicates_ignores_pre_since() {
        // A duplicate-looking reply from before `since` should be ignored —
        // it belongs to a prior session.
        let comments = vec![
            make_api_comment(1, "Please fix this.", None),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T10:00:00Z",
            ),
            make_api_comment_at(
                3,
                "Done again!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        // Only one reply is within the session window — no duplicates to delete.
        assert!(dups.is_empty());
    }

    #[test]
    fn test_identify_duplicates_ignores_other_minions() {
        // A sibling Minion's signature must not count toward this Minion's
        // duplicates.
        let comments = vec![
            make_api_comment(1, "Please fix this.", None),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
            make_api_comment_at(
                3,
                "Done!\n\n<sub>🤖 M002</sub>",
                Some(1),
                "2024-06-15T12:05:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert!(dups.is_empty());
    }

    #[test]
    fn test_identify_duplicates_orphan_minion_replies_ignored() {
        // Minion-signed comments without `in_reply_to_id` are not inline
        // thread replies — leave them alone.
        let comments = vec![
            make_api_comment_at(
                1,
                "Done!\n\n<sub>🤖 M001</sub>",
                None,
                "2024-06-15T12:00:00Z",
            ),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                None,
                "2024-06-15T12:01:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert!(dups.is_empty());
    }

    #[test]
    fn test_identify_duplicates_multiple_threads() {
        // Duplicates in two separate threads; all duplicates across threads
        // are returned.
        let comments = vec![
            make_api_comment(1, "A", None),
            make_api_comment(10, "B", None),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
            make_api_comment_at(
                3,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:01:00Z",
            ),
            make_api_comment_at(
                11,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(10),
                "2024-06-15T12:02:00Z",
            ),
            make_api_comment_at(
                12,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(10),
                "2024-06-15T12:03:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert_eq!(dups, vec![3, 12]);
    }

    #[test]
    fn test_identify_duplicates_ties_broken_by_id() {
        // Two replies with identical timestamps: keep the lower id,
        // delete the higher id.
        let comments = vec![
            make_api_comment(1, "A", None),
            make_api_comment_at(
                5,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
            make_api_comment_at(
                2,
                "Done!\n\n<sub>🤖 M001</sub>",
                Some(1),
                "2024-06-15T12:00:00Z",
            ),
        ];
        let dups = identify_duplicate_minion_replies(&comments, "M001", t("2024-06-15T11:00:00Z"));
        assert_eq!(dups, vec![5]);
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
    // Reviews without a Minion signature (body=None) represent human reviews.

    fn make_review(id: u64, timestamp: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: "reviewer".to_string(),
            },
            body: String::new(),
            state: String::new(),
        }
    }

    #[test]
    fn test_reviews_after_timestamp_included() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review(1, "2024-06-15T09:00:00Z"), // Before - excluded
            make_review(2, "2024-06-15T11:00:00Z"), // After - included
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "M001");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 2);
    }

    #[test]
    fn test_reviews_at_exact_timestamp_included() {
        // Edge case: review at exactly the since timestamp should be included
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review(1, "2024-06-15T10:00:00Z")];

        let filtered = filter_new_external_reviews(&reviews, since, "M001");
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

        let filtered = filter_new_external_reviews(&reviews, since, "M001");
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_empty_review_list_returns_empty() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews: Vec<Review> = vec![];

        let filtered = filter_new_external_reviews(&reviews, since, "M001");
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

        let filtered = filter_new_external_reviews(&reviews, since, "M001");
        assert_eq!(filtered.len(), 3);
    }

    // ========================================================================
    // Minion Signature Filtering Tests
    // ========================================================================
    // The excluded_user parameter represents the authenticated GitHub user
    // (the minion identity), NOT the PR author. Reviews from the minion are
    // excluded to prevent feedback loops. Reviews from the PR author (who may
    // be a human) should pass through. See issue #701.

    fn make_review_with_body(id: u64, timestamp: &str, login: &str, body: Option<&str>) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: login.to_string(),
            },
            body: body.unwrap_or("").to_string(),
            state: String::new(),
        }
    }

    fn make_review_by(id: u64, timestamp: &str, login: &str) -> Review {
        Review {
            id,
            submitted_at: timestamp.parse().unwrap(),
            user: User {
                login: login.to_string(),
            },
            body: String::new(),
            state: String::new(),
        }
    }

    #[test]
    fn test_self_review_excluded_from_new_reviews() {
        // A review signed by the current Minion is excluded; other reviews pass.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review_with_body(
                1,
                "2024-06-15T11:00:00Z",
                "fotoetienne",
                Some("Looks good\n\n<sub>🤖 M1by</sub>"),
            ),
            make_review(2, "2024-06-15T11:00:00Z"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 2);
    }

    #[test]
    fn test_same_user_human_review_kept() {
        // Core scenario from issue #751: human and Minion share the same account.
        // A review with no Minion signature passes through regardless of login.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review_with_body(
            1,
            "2024-06-15T11:00:00Z",
            "fotoetienne",
            Some("Three nits on the error handling."),
        )];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_review_with_no_body_kept() {
        // Reviews with no body (just inline comments) should be kept
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review_with_body(
            1,
            "2024-06-15T11:00:00Z",
            "fotoetienne",
            None,
        )];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_only_self_reviews_returns_empty() {
        // All reviews signed by the current Minion → empty result.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            make_review_with_body(
                1,
                "2024-06-15T11:00:00Z",
                "fotoetienne",
                Some("LGTM\n\n<sub>🤖 M1by</sub>"),
            ),
            make_review_with_body(
                2,
                "2024-06-15T12:00:00Z",
                "fotoetienne",
                Some("Fixed.\n\n<sub>🤖 M1by</sub>"),
            ),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_other_minion_review_is_included() {
        // Bug 1: A review by a different Minion (M1bz) on the same GitHub user
        // must be treated as external feedback for M1by, even though both share
        // the same user.login.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![
            // M1bz posts a review — different signature, same GitHub user
            make_review_with_body(
                1,
                "2024-06-15T11:00:00Z",
                "fotoetienne",
                Some("Minor observation\n\n<sub>🤖 M1bz</sub>"),
            ),
            // M1by's own implicit reply review (empty body)
            make_review_by(2, "2024-06-15T11:30:00Z", "fotoetienne"),
        ];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        // M1bz's review passes (different signature)
        // M1by's empty-body reply review also passes (no own signature)
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|r| r.id == 1)); // M1bz's review included
    }

    #[test]
    fn test_empty_body_review_passes_filter() {
        // GitHub creates implicit review objects with empty bodies when a Minion
        // posts inline reply comments.  These must pass the filter so that
        // get_review_feedback can inspect their inline comments for the
        // already_answered set.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review_by(1, "2024-06-15T11:00:00Z", "fotoetienne")];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_external_reviewer_always_kept() {
        // Reviews from other users (e.g. Copilot) are never filtered
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let reviews = vec![make_review_with_body(
            1,
            "2024-06-15T11:00:00Z",
            "copilot-pull-request-reviewer",
            Some("Consider using a constant here."),
        )];

        let filtered = filter_new_external_reviews(&reviews, since, "M1by");
        assert_eq!(filtered.len(), 1);
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
    fn test_check_run_malformed_line_fails() {
        let line = r#"{ this is not valid json }"#;

        let result: Result<CheckRun, _> = serde_json::from_str(line);
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
            "mergeable": true,
            "created_at": "2024-01-01T00:00:00Z"
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
            "mergeable": false,
            "created_at": "2024-01-01T00:00:00Z"
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
            "mergeable": null,
            "created_at": "2024-01-01T00:00:00Z"
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
            "user": {"login": "author"},
            "created_at": "2024-01-01T00:00:00Z"
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

    #[test]
    fn test_count_unaddressed_reviews_filters_own_minion() {
        // Reviews signed by the current Minion are excluded; external reviews
        // (whether by a human or a sibling Minion) are counted as unaddressed.
        let since = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let reviews = vec![
            // M1by's own review (e.g. self-review) — excluded
            make_review_with_body(
                1,
                "2024-01-02T00:00:00Z",
                "reviewer",
                Some("LGTM\n\n<sub>🤖 M1by</sub>"),
            ),
            // External reviewer (no Minion signature) — counted
            make_review_with_body(
                2,
                "2024-01-02T00:00:00Z",
                "reviewer",
                Some("Please fix the typo"),
            ),
        ];
        assert_eq!(count_unaddressed_reviews(&reviews, "M1by", since), 1);
    }

    #[test]
    fn test_count_unaddressed_reviews_sibling_minion_counted() {
        // A review by a sibling Minion (M1bz) carries a different signature and
        // must be counted as unaddressed feedback for M1by.
        let since = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let reviews = vec![make_review_with_body(
            1,
            "2024-01-02T00:00:00Z",
            "reviewer",
            Some("Minor issue\n\n<sub>🤖 M1bz</sub>"),
        )];
        assert_eq!(count_unaddressed_reviews(&reviews, "M1by", since), 1);
    }

    #[test]
    fn test_count_unaddressed_reviews_returns_zero_before_baseline() {
        let since = "2024-01-10T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let reviews = vec![
            make_review_with_body(1, "2024-01-05T00:00:00Z", "reviewer", Some("old comment")), // before baseline
            make_review_with_body(2, "2024-01-10T00:00:00Z", "reviewer", Some("at baseline")), // equal — excluded (uses >)
        ];
        assert_eq!(count_unaddressed_reviews(&reviews, "M1by", since), 0);
    }

    #[test]
    fn test_count_unaddressed_reviews_empty_list() {
        let since = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(count_unaddressed_reviews(&[], "M1by", since), 0);
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

    #[test]
    fn test_filter_new_issue_comments_returns_new_non_minion_comments() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let comments = vec![
            IssueComment {
                id: 1,
                body: "Old comment".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2024-06-15T09:00:00Z".parse().unwrap(),
                display_name: String::new(),
            },
            IssueComment {
                id: 2,
                body: "New comment from human".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: "2024-06-15T11:00:00Z".parse().unwrap(),
                display_name: String::new(),
            },
            IssueComment {
                id: 3,
                body: "New comment from minion\n\n<sub>🤖 M001</sub>".to_string(),
                user: User {
                    login: "bot".to_string(),
                },
                created_at: "2024-06-15T11:30:00Z".parse().unwrap(),
                display_name: String::new(),
            },
        ];

        let new = filter_new_issue_comments(&comments, since, "M001");
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].body, "New comment from human");
    }

    #[test]
    fn test_filter_new_issue_comments_inclusive_boundary() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let before: DateTime<Utc> = "2024-06-15T09:59:59Z".parse().unwrap();
        let comments = vec![
            IssueComment {
                id: 1,
                body: "Exactly at boundary".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
                created_at: since,
                display_name: String::new(),
            },
            IssueComment {
                id: 2,
                body: "One second before — excluded".to_string(),
                user: User {
                    login: "bob".to_string(),
                },
                created_at: before,
                display_name: String::new(),
            },
        ];
        let new = filter_new_issue_comments(&comments, since, "M001");
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, 1);
    }

    #[test]
    fn test_filter_new_issue_comments_empty() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let new = filter_new_issue_comments(&[], since, "M001");
        assert!(new.is_empty());
    }

    #[test]
    fn test_issue_comment_deserialize() {
        let json = r#"{
            "id": 123,
            "body": "Hello world",
            "user": {"login": "alice"},
            "created_at": "2024-06-15T10:30:00Z",
            "html_url": "https://github.com/example/repo/issues/42#issuecomment-123"
        }"#;
        let comment: IssueComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, 123);
        assert_eq!(comment.body, "Hello world");
        assert_eq!(comment.user.login, "alice");
        let expected_time: DateTime<Utc> = "2024-06-15T10:30:00Z".parse().unwrap();
        assert_eq!(comment.created_at, expected_time);
    }

    #[test]
    fn test_format_issue_comments_prompt_single() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let comments = vec![IssueComment {
            id: 1,
            body: "Can you add more tests?".to_string(),
            user: User {
                login: "alice".to_string(),
            },
            created_at: since,
            display_name: "alice".to_string(),
        }];

        let prompt = format_issue_comments_prompt(
            Some(42),
            "99",
            &comments,
            "octocat",
            "hello-world",
            "M001",
        );

        assert!(prompt.contains("issue #42"));
        assert!(prompt.contains("PR #99"));
        assert!(prompt.contains("## Comment 1"));
        assert!(prompt.contains("**Author:** `alice` (@alice)"));
        assert!(prompt.contains("Can you add more tests?"));
        assert!(prompt.contains("repos/octocat/hello-world/issues/99/comments"));
        assert!(prompt.contains("<sub>🤖 M001</sub>"));
    }

    #[test]
    fn test_format_issue_comments_prompt_no_issue_num() {
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let comments = vec![IssueComment {
            id: 1,
            body: "LGTM".to_string(),
            user: User {
                login: "bob".to_string(),
            },
            created_at: since,
            display_name: "bob".to_string(),
        }];

        let prompt = format_issue_comments_prompt(None, "5", &comments, "owner", "repo", "M002");

        assert!(!prompt.contains("issue #"));
        assert!(prompt.contains("PR #5"));
        assert!(prompt.contains("**Author:** `bob` (@bob)"));
        assert!(prompt.contains("LGTM"));
    }

    #[test]
    fn test_format_issue_comments_prompt_minion_commenter() {
        // A comment posted by a sibling Minion (its body contains a Minion signature).
        // The author is shown by login — no special Minion-ID extraction in this path.
        let since: DateTime<Utc> = "2024-06-15T10:00:00Z".parse().unwrap();
        let comments = vec![IssueComment {
            id: 10,
            body: "Looks good to me.\n\n<sub>🤖 M1ab</sub>".to_string(),
            user: User {
                login: "fotoetienne".to_string(),
            },
            created_at: since,
            display_name: String::new(),
        }];

        let prompt = format_issue_comments_prompt(Some(5), "7", &comments, "owner", "repo", "M1ec");

        assert!(prompt.contains("**Author:** `fotoetienne` (@fotoetienne)"));
        assert!(prompt.contains("Looks good to me."));
        assert!(prompt.contains("<sub>🤖 M1ec</sub>"));
        // The body passes through unchanged, including any sibling Minion signature
        assert!(prompt.contains("<sub>🤖 M1ab</sub>"));
    }
}
