use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::process::Output;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

use crate::labels;

/// Default maximum number of retry attempts for GitHub API calls.
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 5;
const BASE_DELAY_SECS: u64 = 2;
const MAX_DELAY_SECS: u64 = 60;

/// Default timeout for `gh` CLI commands (seconds).
/// Prevents `gru status` and other commands from hanging indefinitely
/// when `gh` calls fail slowly (e.g., querying non-existent issues).
pub(crate) const GH_TIMEOUT_SECS: u64 = 30;

/// Build a `"owner/repo"` slug from separate components.
pub(crate) fn repo_slug(owner: &str, repo: &str) -> String {
    format!("{}/{}", owner, repo)
}

/// Infer GitHub hostname from repository owner.
///
/// This is a fallback heuristic used when the host isn't known from a URL.
/// Prefer using the host from `parse_github_remote` or `parse_github_url` when available,
/// or passing the host explicitly.
///
/// Checks `daemon.repos` config entries for an owner match with an explicit GHE host.
/// Falls back to `github.com` for unknown owners.
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `config` - Optional pre-loaded config. When `None`, falls back to
///   [`crate::config::try_load_config`].
///
/// Returns the appropriate GitHub hostname
pub(crate) fn infer_github_host(owner: &str, config: Option<&crate::config::LabConfig>) -> String {
    // Check daemon.repos config for an explicit host for this owner
    let loaded = if config.is_none() {
        crate::config::try_load_config()
    } else {
        None
    };
    let cfg = config.or(loaded.as_ref());
    if let Some(cfg) = cfg {
        for repo_spec in &cfg.daemon.repos {
            if let Some((host, repo_owner, _repo)) =
                crate::config::parse_repo_entry_with_hosts(repo_spec, &cfg.github_hosts)
            {
                if repo_owner == owner && host != "github.com" {
                    return host;
                }
            }
        }
    }

    "github.com".to_string()
}

/// Creates a pre-configured `tokio::process::Command` for the `gh` CLI.
///
/// Always uses the `gh` binary and sets `GH_HOST` to the provided host
/// so authentication targets the correct server. This ensures deterministic
/// host selection even when the parent process has `GH_HOST` set.
pub(crate) fn gh_cli_command(host: &str) -> Command {
    let mut cmd = Command::new("gh");
    cmd.env("GH_HOST", host);
    cmd
}

/// Run a `gh` CLI command and return its stdout on success.
///
/// This is the standard helper for executing `gh` commands. It handles the
/// common boilerplate of running the command, checking the exit status, and
/// extracting stdout.
///
/// # Arguments
/// * `host` - GitHub hostname (sets `GH_HOST` env var)
/// * `args` - Arguments to pass to `gh` (e.g., `&["pr", "view", "123"]`)
///
/// # Returns
/// The command's stdout as a String on success.
///
/// # Errors
/// Returns an error if the command fails to execute or exits with a non-zero status.
pub(crate) async fn run_gh(host: &str, args: &[&str]) -> Result<String> {
    // Truncate long args (e.g., --body content) to keep error messages readable
    let args_display: String = args
        .iter()
        .map(|a| if a.len() > 80 { &a[..80] } else { a })
        .collect::<Vec<_>>()
        .join(" ");
    let child = gh_cli_command(host).args(args).kill_on_drop(true).output();
    let output = tokio::time::timeout(Duration::from_secs(GH_TIMEOUT_SECS), child)
        .await
        .with_context(|| format!("gh {} timed out after {}s", args_display, GH_TIMEOUT_SECS))?
        .with_context(|| format!("Failed to execute: gh {}", args_display))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh {} failed: {}", args_display, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Check if an error message indicates a transient failure that should be retried.
///
/// Transient failures include network issues and temporary server errors.
/// Rate limit errors are **excluded** — they are handled separately via
/// [`is_rate_limit_error`] and [`sleep_until_rate_limit_reset`].
pub(crate) fn is_retryable_error(stderr: &str) -> bool {
    // Exclude rate limit errors first — they get separate handling.
    // Pass the original stderr so it is only lowercased once inside
    // is_rate_limit_error, avoiding a redundant allocation here.
    if is_rate_limit_error(stderr) {
        return false;
    }

    // All patterns must be lowercase for case-insensitive matching
    let retryable_patterns = [
        // HTTP status codes
        "502",
        "503",
        "504",
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
    std::cmp::min(BASE_DELAY_SECS.saturating_pow(attempt), MAX_DELAY_SECS)
}

/// Execute a gh API command with retry logic and exponential backoff.
///
/// Rate limit errors are handled separately: instead of exponential backoff,
/// the function sleeps until the GitHub rate limit reset window and retries once.
/// Transient errors use exponential backoff up to `max_retries` times.
///
/// Returns the raw [`Output`] regardless of exit status — both successful
/// responses and non-retryable failures are returned as `Ok(Output)`.
/// Callers **must** check `output.status.success()` to distinguish the two.
///
/// # Arguments
/// * `host` - GitHub hostname (e.g., "github.com" or "ghe.example.com")
/// * `args` - The arguments to pass to the gh command
/// * `max_retries` - Maximum number of retry attempts for transient errors
pub(crate) async fn gh_api_with_retry(
    host: &str,
    args: &[&str],
    max_retries: u32,
) -> Result<Output> {
    let mut attempts = 0;
    let mut rate_limit_retried = false;
    let args_str = args.join(" ");

    loop {
        let output = gh_cli_command(host)
            .args(args)
            .output()
            .await
            .with_context(|| format!("Failed to execute: gh {}", args_str))?;

        if output.status.success() {
            if attempts > 0 || rate_limit_retried {
                log::info!("GitHub API call succeeded after retries: gh {}", args_str);
            }
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);

        // Rate limit errors: sleep until reset, retry once
        if is_rate_limit_error(&stderr) {
            if rate_limit_retried {
                return Ok(output);
            }
            rate_limit_retried = true;
            sleep_until_rate_limit_reset(host).await;
            continue;
        }

        // Transient errors: exponential backoff
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
            // Either not retryable or max retries exceeded — return output for caller to handle
            return Ok(output);
        }
    }
}

/// Queries the GitHub API for the repository's default branch.
///
/// Uses `gh api repos/OWNER/REPO --jq .default_branch`.
pub(crate) async fn get_default_branch(host: &str, owner: &str, repo: &str) -> Result<String> {
    let endpoint = format!("repos/{}", repo_slug(owner, repo));
    let stdout = run_gh(host, &["api", &endpoint, "--jq", ".default_branch"]).await?;
    let branch = stdout.trim().to_string();
    if branch.is_empty() {
        anyhow::bail!(
            "GitHub API returned an empty default_branch for {}/{}",
            owner,
            repo
        );
    }
    Ok(branch)
}

// ============================================================================
// Rate Limit Handling
// ============================================================================

/// Default fallback sleep duration (seconds) when the rate limit reset time
/// cannot be determined (e.g., `gh api rate_limit` itself is rate-limited).
const RATE_LIMIT_FALLBACK_SLEEP_SECS: u64 = 60;

/// Small jitter added after the reset timestamp to avoid thundering herd.
const RATE_LIMIT_JITTER_SECS: u64 = 5;

/// Check if a `gh` CLI error indicates a GitHub rate limit (as opposed to a
/// transient server error).  Rate limit errors are never worth retrying with
/// exponential backoff — the caller should sleep until the reset window.
pub(crate) fn is_rate_limit_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("too many requests")
        || contains_standalone_429(&lower)
}

/// Return true if the string contains "429" that is not part of a larger
/// number (i.e., not immediately preceded or followed by a digit). This
/// avoids matching timestamps or request IDs that happen to contain "429".
fn contains_standalone_429(s: &str) -> bool {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len < 3 {
        return false;
    }
    for i in 0..=len - 3 {
        if &bytes[i..i + 3] == b"429" {
            let prev_is_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_is_digit = i + 3 < len && bytes[i + 3].is_ascii_digit();
            if !prev_is_digit && !next_is_digit {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Deserialize)]
struct RateLimitRate {
    remaining: u64,
    reset: u64,
}

#[derive(Debug, Deserialize)]
struct RateLimitResponse {
    rate: RateLimitRate,
}

/// Info extracted from the GitHub rate limit API.
#[derive(Debug)]
struct RateLimitInfo {
    /// Remaining requests in the current window.
    remaining: u64,
    /// Unix epoch when the current window resets.
    reset: u64,
}

/// Query the GitHub rate limit API and return the core-rate reset epoch and
/// remaining quota.
///
/// Returns `None` if the API call fails (chicken-and-egg: we may already be
/// rate-limited), times out, or the response cannot be parsed.
async fn get_rate_limit_reset(host: &str) -> Option<RateLimitInfo> {
    let child = gh_cli_command(host)
        .args(["api", "rate_limit"])
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(Duration::from_secs(GH_TIMEOUT_SECS), child)
        .await
        .ok()? // timeout elapsed
        .ok()?; // spawn/IO error

    if !output.status.success() {
        return None;
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let parsed: RateLimitResponse = serde_json::from_str(&body).ok()?;
    Some(RateLimitInfo {
        remaining: parsed.rate.remaining,
        reset: parsed.rate.reset,
    })
}

/// Handle a rate limit error by sleeping until the reset window.
///
/// 1. Queries `gh api rate_limit` for the reset epoch.
/// 2. If the reset is within [`RATE_LIMIT_FALLBACK_SLEEP_SECS`] of now,
///    sleeps until `reset + jitter`.
/// 3. If the reset is far in the future (likely a secondary/concurrency
///    rate limit where the core quota is still available), falls back to
///    a fixed 60 s sleep to avoid oversleeping.
/// 4. If the rate limit API itself fails, falls back to a fixed 60 s sleep.
pub(crate) async fn sleep_until_rate_limit_reset(host: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if let Some(info) = get_rate_limit_reset(host).await {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // If core quota still has remaining requests, this is likely a
        // secondary (concurrency) rate limit — use the short fallback.
        if info.remaining > 0 {
            log::warn!(
                "Rate limited but core quota has {} remaining (secondary/concurrency limit). \
                 Sleeping {}s as fallback.",
                info.remaining,
                RATE_LIMIT_FALLBACK_SLEEP_SECS,
            );
        } else if info.reset > now {
            let diff_secs = info.reset - now;

            // Only trust the reset epoch for reasonably short waits. When the
            // reset is far in the future, sleeping until reset could cause
            // us to oversleep by a large margin.
            if diff_secs <= RATE_LIMIT_FALLBACK_SLEEP_SECS {
                let sleep_secs = diff_secs + RATE_LIMIT_JITTER_SECS;
                let reset_time = chrono::DateTime::from_timestamp(info.reset as i64, 0)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%l:%M %p")
                            .to_string()
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                let dur_min = sleep_secs / 60;
                let dur_sec = sleep_secs % 60;
                log::warn!(
                    "Rate limited (remaining=0). Pausing until {} ({}m {}s)",
                    reset_time.trim(),
                    dur_min,
                    dur_sec,
                );
                tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                return;
            }

            log::warn!(
                "Rate limited. Reset is {}s in the future (> {}s cap). \
                 Sleeping {}s as fallback to avoid oversleeping.",
                diff_secs,
                RATE_LIMIT_FALLBACK_SLEEP_SECS,
                RATE_LIMIT_FALLBACK_SLEEP_SECS,
            );
        } else {
            // Reset epoch is in the past — likely stale or clock skew
            log::warn!(
                "Rate limited but reset epoch {} is already past (now={}). \
                 Sleeping {}s as fallback.",
                info.reset,
                now,
                RATE_LIMIT_FALLBACK_SLEEP_SECS,
            );
        }
    } else {
        log::warn!(
            "Rate limited (could not query reset time). Sleeping {}s as fallback.",
            RATE_LIMIT_FALLBACK_SLEEP_SECS,
        );
    }

    tokio::time::sleep(std::time::Duration::from_secs(
        RATE_LIMIT_FALLBACK_SLEEP_SECS,
    ))
    .await;
}

/// Build a full GitHub issue URL for a repo in "owner/repo" format, with an explicit host.
///
/// Returns `Some(url)` when `repo` is a valid `owner/repo` string, otherwise `None`.
pub(crate) fn build_issue_url_with_host(
    repo: &str,
    host: &str,
    issue_number: u64,
) -> Option<String> {
    let (owner, repo_name) = repo.split_once('/')?;
    if owner.is_empty() || repo_name.is_empty() || repo_name.contains('/') {
        return None;
    }
    Some(format!(
        "https://{}/{}/{}/issues/{}",
        host, owner, repo_name, issue_number
    ))
}

// ============================================================================
// gh CLI Helper Functions
// ============================================================================
// These functions use the gh CLI directly for all GitHub API operations.

/// Mark a draft PR as ready for review using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `pr_number` - PR number
pub(crate) async fn mark_pr_ready_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = repo_slug(owner, repo);
    run_gh(host, &["pr", "ready", pr_number, "--repo", &repo_full]).await?;
    Ok(())
}

/// List issues with a label, excluding blocked and already-claimed issues, using gh CLI search.
///
/// Uses GitHub's search qualifiers to exclude:
/// - Issues in GitHub's native blocked state (`-is:blocked`)
/// - Issues labeled `gru:blocked`
/// - Issues already claimed (`gru:in-progress`)
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `label` - Label to search for (e.g., "gru:todo")
///
/// # Returns
/// List of `CandidateIssue` values (number + optional body) matching the criteria (capped at 100)
/// Build a GitHub search query that finds issues with the given label while excluding
/// blocked and in-progress issues. Escapes special characters in the label.
fn build_ready_issues_search_query(label: &str) -> String {
    let escaped_label = label.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "label:\"{}\" -is:blocked -label:\"{}\" -label:\"{}\"",
        escaped_label,
        labels::BLOCKED,
        labels::IN_PROGRESS,
    )
}

pub(crate) async fn list_ready_issues_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    label: &str,
) -> Result<Vec<CandidateIssue>> {
    let repo_full = repo_slug(owner, repo);
    let search_query = build_ready_issues_search_query(label);
    let stdout = run_gh(
        host,
        &[
            "issue",
            "list",
            "--repo",
            &repo_full,
            "--search",
            &search_query,
            "--state",
            "open",
            "--json",
            "number,body,labels",
            "--limit",
            "100",
        ],
    )
    .await?;

    let items: Vec<CandidateIssue> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list JSON output")?;

    Ok(items)
}

/// List open issues with the `gru:in-progress` label using gh CLI.
///
/// Returns a list of issue numbers. Used by the lab recovery scan to find
/// orphaned issues that have no live Minion process.
pub(crate) async fn list_in_progress_issues_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
) -> Result<Vec<u64>> {
    let repo_full = repo_slug(owner, repo);
    let stdout = run_gh(
        host,
        &[
            "issue",
            "list",
            "--repo",
            &repo_full,
            "--label",
            labels::IN_PROGRESS,
            "--state",
            "open",
            "--json",
            "number",
            "--limit",
            "100",
        ],
    )
    .await?;

    #[derive(serde::Deserialize)]
    struct NumberOnly {
        number: u64,
    }

    let items: Vec<NumberOnly> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list JSON output")?;

    Ok(items.into_iter().map(|i| i.number).collect())
}

/// Issue candidate returned by list queries, with optional body for dependency checking
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct CandidateIssue {
    pub(crate) number: u64,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) labels: Vec<IssueLabel>,
}

/// Returns a sort key for priority labels on a candidate issue.
///
/// Lower values are higher priority:
///   0 = critical, 1 = high, 2 = medium, 3 = unlabeled, 4 = low
pub(crate) fn priority_sort_key(labels: &[IssueLabel]) -> u8 {
    labels
        .iter()
        .filter_map(|l| match l.name.as_str() {
            "priority:critical" => Some(0u8),
            "priority:high" => Some(1),
            "priority:medium" => Some(2),
            "priority:low" => Some(4),
            _ => None,
        })
        .min()
        .unwrap_or(3) // unlabeled = neutral, between medium and low
}

/// Fetch issue details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - Issue number
pub(crate) async fn get_issue_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    number: u64,
) -> Result<IssueInfo> {
    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let stdout = run_gh(
        host,
        &[
            "issue",
            "view",
            &number_str,
            "--repo",
            &repo_full,
            "--json",
            "title,body,labels",
        ],
    )
    .await?;

    let issue: IssueInfo =
        serde_json::from_str(&stdout).context("Failed to parse gh issue view JSON output")?;

    Ok(issue)
}

/// Quick revalidation check for an issue before dispatch.
///
/// Fetches the issue's current state and labels in a single API call to detect
/// TOCTOU races (issue closed or claimed between poll and dispatch).
///
/// Returns `true` if the issue is still eligible: the issue is still open and
/// does not have any ineligible label such as `gru:in-progress`, `gru:done`,
/// or `gru:failed`.
/// Returns `true` (fail-open) if the API call or response parsing fails, so that
/// transient errors don't block dispatch.
pub async fn is_issue_still_eligible(owner: &str, repo: &str, host: &str, number: u64) -> bool {
    #[derive(serde::Deserialize)]
    struct RevalidationInfo {
        state: String,
        #[serde(default)]
        labels: Vec<IssueLabel>,
    }

    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let result = run_gh(
        host,
        &[
            "issue",
            "view",
            &number_str,
            "--repo",
            &repo_full,
            "--json",
            "state,labels",
        ],
    )
    .await;

    match result {
        Ok(stdout) => match serde_json::from_str::<RevalidationInfo>(&stdout) {
            Ok(info) => {
                let eligible = check_issue_eligibility(&info.state, &info.labels);
                if !eligible.0 {
                    log::info!(
                        "⏭️  Issue #{} {}, skipping",
                        number,
                        eligible.1.unwrap_or_default()
                    );
                }
                eligible.0
            }
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to parse revalidation response for issue #{}: {} — proceeding anyway",
                    number,
                    e
                );
                true // fail-open
            }
        },
        Err(e) => {
            log::warn!(
                "⚠️  Revalidation API call failed for issue #{}: {} — proceeding anyway",
                number,
                e
            );
            true // fail-open
        }
    }
}

/// Pure decision logic for issue eligibility based on state and labels.
///
/// Returns `(true, None)` if eligible, or `(false, Some(reason))` if not.
pub(crate) fn check_issue_eligibility(
    state: &str,
    labels: &[IssueLabel],
) -> (bool, Option<String>) {
    if state != "OPEN" {
        return (false, Some(format!("is no longer open (state: {})", state)));
    }
    let ineligible_labels = [
        crate::labels::IN_PROGRESS,
        crate::labels::DONE,
        crate::labels::FAILED,
    ];
    if let Some(label) = labels
        .iter()
        .find(|l| ineligible_labels.contains(&l.name.as_str()))
    {
        return (
            false,
            Some(format!(
                "has ineligible label ({}) since last poll",
                label.name
            )),
        );
    }
    (true, None)
}

/// Check if a GitHub issue is closed.
///
/// Returns `true` if the issue state is `"closed"` (the REST API value).
pub async fn is_issue_closed_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    number: u64,
) -> Result<bool> {
    let endpoint = format!("repos/{owner}/{repo}/issues/{number}");
    let stdout = run_gh(
        host,
        &["api", &endpoint, "--cache", "300s", "--jq", ".state"],
    )
    .await?;

    let state = stdout.trim().to_string();
    if state.is_empty() {
        return Err(anyhow!(
            "gh api returned empty state for issue #{} in {}/{}",
            number,
            owner,
            repo
        ));
    }
    Ok(state == "closed")
}

/// Check whether a PR is still open (i.e., not merged or closed).
///
/// Returns `true` if the PR state is `"open"` (the REST API value), `false` otherwise.
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `host` - GitHub hostname
/// * `number` - PR number
pub async fn is_pr_open_via_cli(owner: &str, repo: &str, host: &str, number: u64) -> Result<bool> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
    let stdout = run_gh(
        host,
        &["api", &endpoint, "--cache", "300s", "--jq", ".state"],
    )
    .await?;

    let state = stdout.trim().to_string();
    if state.is_empty() {
        return Err(anyhow!(
            "gh api returned empty state for PR #{} in {}/{}",
            number,
            owner,
            repo
        ));
    }
    Ok(state == "open")
}

/// Check whether a PR has been merged.
///
/// Returns `true` if the PR state is "MERGED", `false` otherwise.
pub async fn is_pr_merged_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    number: u64,
) -> Result<bool> {
    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let stdout = run_gh(
        host,
        &[
            "pr",
            "view",
            &number_str,
            "--repo",
            &repo_full,
            "--json",
            "state",
            "--jq",
            ".state",
        ],
    )
    .await?;

    let state = stdout.trim().to_string();
    if state.is_empty() {
        return Err(anyhow!(
            "gh pr view returned empty state for PR #{} in {}/{}",
            number,
            owner,
            repo
        ));
    }
    Ok(state == "MERGED")
}

/// Simple struct to hold issue information from gh CLI
#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueInfo {
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    /// Labels attached to the issue (from `gh issue view --json labels`)
    #[serde(default)]
    pub(crate) labels: Vec<IssueLabel>,
}

/// Label info returned by `gh issue view --json labels`
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct IssueLabel {
    pub(crate) name: String,
}

/// Simple struct to hold PR information from gh CLI
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrInfo {
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    pub(crate) head_ref_name: String,
}

/// Fetch PR details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - PR number
pub(crate) async fn get_pr_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    number: u64,
) -> Result<PrInfo> {
    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let stdout = run_gh(
        host,
        &[
            "pr",
            "view",
            &number_str,
            "--repo",
            &repo_full,
            "--json",
            "title,body,headRefName",
        ],
    )
    .await?;

    let pr: PrInfo =
        serde_json::from_str(&stdout).context("Failed to parse gh pr view JSON output")?;

    Ok(pr)
}

/// Create a draft pull request using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `branch` - Head branch name (source)
/// * `base` - Base branch name (target, usually "main")
/// * `title` - PR title
/// * `body` - PR description body (markdown supported)
///
/// Returns the PR number as a string
pub(crate) async fn create_draft_pr_via_cli(
    owner: &str,
    repo: &str,
    host: &str,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let repo_full = repo_slug(owner, repo);
    let stdout = run_gh(
        host,
        &[
            "pr", "create", "--repo", &repo_full, "--head", branch, "--base", base, "--title",
            title, "--body", body, "--draft",
        ],
    )
    .await?;

    let pr_url = stdout.trim();

    // Validate URL format (gh returns URL like https://<host>/owner/repo/pull/123)
    let expected_prefix = format!("https://{}/", host);
    if !pr_url.starts_with(&expected_prefix) {
        return Err(anyhow!(
            "Expected GitHub HTTPS URL starting with {}, got: {}",
            expected_prefix,
            pr_url
        ));
    }

    // Remove any query parameters or fragments before parsing
    let url_path = pr_url
        .trim_end_matches('/')
        .split('?')
        .next()
        .unwrap()
        .split('#')
        .next()
        .unwrap();

    // Parse PR number from path segments
    // Expected format: https://<host>/owner/repo/pull/123
    let segments: Vec<&str> = url_path.split('/').collect();

    // segments should be: ["https:", "", "<host>", "owner", "repo", "pull", "123"]
    if segments.len() < 7 || segments[5] != "pull" {
        return Err(anyhow!(
            "Unexpected GitHub PR URL format: {}. Expected: https://{}/owner/repo/pull/NUMBER",
            pr_url,
            host
        ));
    }

    let pr_number = segments[6];

    // Validate it's actually a number
    pr_number
        .parse::<u64>()
        .context(format!("PR number '{}' is not a valid integer", pr_number))?;

    Ok(pr_number.to_string())
}

/// Post a comment on an issue or PR using gh CLI
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue or PR number
/// * `body` - Comment body (markdown supported)
pub(crate) async fn post_comment_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
    body: &str,
) -> Result<()> {
    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    run_gh(
        host,
        &[
            "issue",
            "comment",
            &number_str,
            "--repo",
            &repo_full,
            "--body",
            body,
        ],
    )
    .await?;

    Ok(())
}

/// Check whether any of the given labels are present on an issue.
///
/// Returns the name of the first matching label, or `None` if none match.
pub async fn has_any_label_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
    labels: &[&str],
) -> Result<Option<String>> {
    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let stdout = run_gh(
        host,
        &[
            "issue",
            "view",
            &number_str,
            "--repo",
            &repo_full,
            "--json",
            "labels",
        ],
    )
    .await?;

    #[derive(serde::Deserialize)]
    struct LabelsOnly {
        #[serde(default)]
        labels: Vec<IssueLabel>,
    }

    let info: LabelsOnly =
        serde_json::from_str(&stdout).context("Failed to parse gh issue view labels JSON")?;

    Ok(info
        .labels
        .iter()
        .find(|l| labels.contains(&l.name.as_str()))
        .map(|l| l.name.clone()))
}

/// Edit labels on an issue using gh CLI (add and/or remove in a single call).
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
/// * `add` - Labels to add
/// * `remove` - Labels to remove
pub(crate) async fn edit_labels_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
    add: &[&str],
    remove: &[&str],
) -> Result<()> {
    if add.is_empty() && remove.is_empty() {
        return Ok(());
    }

    let repo_full = repo_slug(owner, repo);
    let number_str = number.to_string();
    let mut args: Vec<&str> = vec!["issue", "edit", &number_str, "--repo", &repo_full];

    for label in add {
        args.push("--add-label");
        args.push(label);
    }
    for label in remove {
        args.push("--remove-label");
        args.push(label);
    }

    run_gh(host, &args).await?;

    Ok(())
}

/// Create a label in a repository using gh CLI
///
/// Uses `--force` for idempotent behavior (updates if exists).
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `name` - Label name
/// * `color` - Hex color code (without # prefix)
/// * `description` - Label description
pub(crate) async fn create_label_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    name: &str,
    color: &str,
    description: &str,
) -> Result<()> {
    let repo_full = repo_slug(owner, repo);
    run_gh(
        host,
        &[
            "label",
            "create",
            name,
            "--repo",
            &repo_full,
            "--color",
            color,
            "-d",
            description,
            "--force",
        ],
    )
    .await?;

    Ok(())
}

/// Check gh CLI authentication status for a host
///
/// # Arguments
/// * `host` - GitHub hostname to check auth for
///
/// # Returns
/// * `Ok(())` if authenticated
/// * `Err(_)` if not authenticated or check failed
pub(crate) async fn check_auth_via_cli(host: &str) -> Result<()> {
    run_gh(host, &["auth", "status", "--hostname", host]).await?;
    Ok(())
}

/// Claim an issue by transitioning labels: remove the ready label, add gru:in-progress.
///
/// Note: This function does not check whether the issue is already in-progress
/// before claiming it (no race-condition guard). Callers should verify the
/// issue state beforehand if multi-instance deployments are a concern.
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
/// * `ready_label` - The label to remove when claiming (e.g., `labels::TODO` or a custom daemon label)
pub(crate) async fn claim_issue_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
    ready_label: &str,
) -> Result<()> {
    edit_labels_via_cli(
        host,
        owner,
        repo,
        number,
        &[labels::IN_PROGRESS],
        &[ready_label],
    )
    .await
}

/// Mark an issue as done: remove gru:in-progress, add gru:done.
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
pub(crate) async fn mark_issue_done_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<()> {
    edit_labels_via_cli(
        host,
        owner,
        repo,
        number,
        &[labels::DONE],
        &[labels::IN_PROGRESS],
    )
    .await
}

/// Mark an issue as failed: remove gru:in-progress, add gru:failed.
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
pub(crate) async fn mark_issue_failed_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<()> {
    edit_labels_via_cli(
        host,
        owner,
        repo,
        number,
        &[labels::FAILED],
        &[labels::IN_PROGRESS],
    )
    .await
}

/// Remove `gru:blocked` label from a PR and restore `gru:in-progress` on the issue.
///
/// The CI escalation path adds `gru:blocked` to the **PR** via `gh pr edit`,
/// so removal must also target the PR. The issue gets `gru:in-progress`
/// restored since `mark_issue_blocked_via_cli` removed it.
///
/// Idempotent: silently succeeds if the label is not present.
pub(crate) async fn remove_blocked_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    issue_number: u64,
) -> Result<()> {
    let repo_full = repo_slug(owner, repo);
    let pr_str = pr_number.to_string();

    // Remove gru:blocked from the PR (where ci.rs adds it)
    // Ignore "not found" errors — label may not be present
    if let Err(e) = run_gh(
        host,
        &[
            "pr",
            "edit",
            &pr_str,
            "--repo",
            &repo_full,
            "--remove-label",
            labels::BLOCKED,
        ],
    )
    .await
    {
        let msg = e.to_string();
        if !msg.contains("404") && !msg.contains("not found") {
            return Err(e);
        }
    }

    // Restore gru:in-progress on the issue (blocking removed it)
    let _ = edit_labels_via_cli(
        host,
        owner,
        repo,
        issue_number,
        &[labels::IN_PROGRESS],
        &[labels::BLOCKED],
    )
    .await;

    Ok(())
}

/// Mark an issue as blocked: add gru:blocked, remove in-progress/done/failed.
///
/// Removes all state labels to ensure a clean transition regardless of
/// which phase triggered the block.
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
pub(crate) async fn mark_issue_blocked_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<()> {
    edit_labels_via_cli(
        host,
        owner,
        repo,
        number,
        &[labels::BLOCKED],
        &[labels::IN_PROGRESS, labels::DONE, labels::FAILED],
    )
    .await
}

// ============================================================================
// PR Review Deduplication
// ============================================================================

/// A single PR review returned by the GitHub reviews API.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrReview {
    pub(crate) user: ReviewUser,
    pub(crate) commit_id: String,
    pub(crate) state: String,
}

/// The user portion of a PR review response.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ReviewUser {
    pub(crate) login: String,
}

/// Sanitize a GitHub display name for safe embedding in a prompt.
///
/// Strips control characters and backticks (to prevent code-span breakage),
/// caps the result at 100 characters, and returns the login if the name
/// is empty after sanitization.
pub(crate) fn sanitize_display_name(name: &str, login: &str) -> String {
    let sanitized: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '`')
        .take(100)
        .collect();
    if sanitized.is_empty() {
        login.to_string()
    } else {
        sanitized
    }
}

/// Fetch the display name for a GitHub user, falling back to their login.
///
/// Fetches the full user JSON from `gh api /users/{login}` and extracts the
/// `name` field. A JSON-null `name` is treated as absent (falls back to the
/// login); a non-null string value (including a literal "null" display name)
/// is used as-is after sanitization. On API error or parse failure, logs a
/// warning and returns the login.
///
/// Results are cached by the `gh` CLI for 5 minutes to reduce rate-limit
/// pressure during PR monitoring cycles.
pub(crate) async fn get_user_display_name(host: &str, login: &str) -> String {
    let endpoint = format!("users/{login}");
    match run_gh(host, &["api", &endpoint, "--cache", "300s"]).await {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => {
                let name = value["name"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_default();
                sanitize_display_name(&name, login)
            }
            Err(e) => {
                log::warn!("Failed to parse display name response for {}: {}", login, e);
                login.to_string()
            }
        },
        Err(e) => {
            log::warn!("Failed to fetch display name for {}: {}", login, e);
            login.to_string()
        }
    }
}

/// Returns the login of the currently authenticated `gh` CLI user.
///
/// Uses `gh api user` to fetch the authenticated account. Returns an error
/// if `gh` is not authenticated or the API call fails.
pub(crate) async fn get_authenticated_user(host: &str) -> Result<String> {
    let stdout = run_gh(host, &["api", "user", "--cache", "60s", "--jq", ".login"]).await?;

    let login = stdout.trim().to_string();
    if login.is_empty() {
        return Err(anyhow!("gh api user returned empty login"));
    }
    Ok(login)
}

/// Fetch all reviews for a PR from the GitHub API (paginated).
///
/// Uses `--paginate --jq '.[]'` to handle PRs with more than 30 reviews,
/// producing a newline-delimited JSON stream that is then collected into a Vec.
pub(crate) async fn list_pr_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<PrReview>> {
    let endpoint = format!("repos/{}/{}/pulls/{}/reviews", owner, repo, pr_number);
    // No --cache: this is called by has_gru_review_for_sha as a pre-write
    // dedup guard, so it must see the latest reviews to avoid duplicates.
    let stdout = run_gh(host, &["api", &endpoint, "--paginate", "--jq", ".[]"]).await?;

    // --paginate --jq '.[]' outputs one JSON object per line (NDJSON).
    parse_pr_reviews_ndjson(&stdout)
}

/// Parse a newline-delimited JSON stream of PR review objects.
///
/// `--paginate --jq '.[]'` emits one JSON object per line; this helper
/// is extracted for unit testing without network access.
pub(crate) fn parse_pr_reviews_ndjson(ndjson: &str) -> Result<Vec<PrReview>> {
    let mut reviews = Vec::new();
    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let review: PrReview =
            serde_json::from_str(line).context("Failed to parse PR review JSON line")?;
        reviews.push(review);
    }
    Ok(reviews)
}

/// Check whether the authenticated gh user has already posted a non-dismissed,
/// non-pending review for the given HEAD SHA on the specified PR.
///
/// Returns `true` if a review already exists (skip posting another).
/// Returns `false` if no review found or on API error (fail open).
///
/// # Race condition note
/// If two minions enter this check simultaneously, both may see `false` and both
/// proceed to post a review. This is a narrow window with low consequence (one
/// extra review comment) and is not worth a distributed lock for V1.
pub(crate) async fn has_gru_review_for_sha(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    head_sha: &str,
) -> bool {
    // Get the authenticated user login; fail open on error.
    // TODO: the authenticated user is stable for the lifetime of a process; a
    // OnceLock<String> or a caller-supplied parameter could avoid this extra
    // `gh api user` call on each invocation. Not worth the complexity at V1
    // call frequency (once per monitor_pr_lifecycle entry), but revisit if
    // has_gru_review_for_sha ever gets additional hot-path callers.
    let gh_user = match get_authenticated_user(host).await {
        Ok(u) => u,
        Err(e) => {
            log::warn!(
                "⚠️  Could not get authenticated user for review dedup check: {}",
                e
            );
            return false;
        }
    };

    // Fetch all reviews; fail open on error.
    let reviews = match list_pr_reviews(host, owner, repo, pr_number).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "⚠️  Could not fetch PR reviews for dedup check (proceeding): {}",
                e
            );
            return false;
        }
    };

    review_exists_for_sha(&reviews, &gh_user, head_sha)
}

/// Pure helper: given a list of reviews, a user login, and a SHA, determine
/// whether the user already has a submitted, non-dismissed review for that commit.
///
/// `PENDING` reviews (drafted but not yet submitted) and `DISMISSED` reviews do not
/// count — both should allow a new review to be posted.
///
/// Extracted for unit testing without network access.
pub(crate) fn review_exists_for_sha(reviews: &[PrReview], user_login: &str, sha: &str) -> bool {
    reviews.iter().any(|r| {
        r.user.login == user_login
            && r.commit_id == sha
            && r.state != "DISMISSED"
            && r.state != "PENDING"
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- review_exists_for_sha tests ---

    fn make_review(login: &str, commit_id: &str, state: &str) -> PrReview {
        PrReview {
            user: ReviewUser {
                login: login.to_string(),
            },
            commit_id: commit_id.to_string(),
            state: state.to_string(),
        }
    }

    #[test]
    fn test_review_exists_matching_review() {
        let reviews = vec![make_review("gru-bot", "abc123", "APPROVED")];
        assert!(review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_dismissed_is_ignored() {
        let reviews = vec![make_review("gru-bot", "abc123", "DISMISSED")];
        assert!(!review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_different_sha() {
        let reviews = vec![make_review("gru-bot", "abc123", "APPROVED")];
        assert!(!review_exists_for_sha(&reviews, "gru-bot", "deadbeef"));
    }

    #[test]
    fn test_review_exists_different_user() {
        let reviews = vec![make_review("other-bot", "abc123", "APPROVED")];
        assert!(!review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_empty_list() {
        assert!(!review_exists_for_sha(&[], "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_multiple_reviews_one_matches() {
        let reviews = vec![
            make_review("human-reviewer", "abc123", "APPROVED"),
            make_review("gru-bot", "abc123", "COMMENTED"),
            make_review("gru-bot", "oldsha", "APPROVED"),
        ];
        assert!(review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_pending_is_ignored() {
        // A PENDING review was never submitted; should not block re-review
        let reviews = vec![make_review("gru-bot", "abc123", "PENDING")];
        assert!(!review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_commented_state_counts() {
        let reviews = vec![make_review("gru-bot", "abc123", "COMMENTED")];
        assert!(review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_review_exists_changes_requested_counts() {
        let reviews = vec![make_review("gru-bot", "abc123", "CHANGES_REQUESTED")];
        assert!(review_exists_for_sha(&reviews, "gru-bot", "abc123"));
    }

    #[test]
    fn test_pr_review_deserialize() {
        let json = r#"[
            {"user": {"login": "gru-bot"}, "commit_id": "abc123", "state": "APPROVED"},
            {"user": {"login": "human"}, "commit_id": "abc123", "state": "CHANGES_REQUESTED"}
        ]"#;
        let reviews: Vec<PrReview> = serde_json::from_str(json).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].user.login, "gru-bot");
        assert_eq!(reviews[0].commit_id, "abc123");
        assert_eq!(reviews[0].state, "APPROVED");
    }

    #[test]
    fn test_parse_pr_reviews_ndjson() {
        // Exercises the actual parsing path used by list_pr_reviews
        let ndjson = concat!(
            "{\"user\":{\"login\":\"gru-bot\"},\"commit_id\":\"abc123\",\"state\":\"APPROVED\"}\n",
            "{\"user\":{\"login\":\"human\"},\"commit_id\":\"abc123\",\"state\":\"CHANGES_REQUESTED\"}\n",
            "\n", // blank line should be skipped
        );
        let reviews = parse_pr_reviews_ndjson(ndjson).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].user.login, "gru-bot");
        assert_eq!(reviews[1].state, "CHANGES_REQUESTED");
    }

    #[test]
    fn test_parse_pr_reviews_ndjson_empty() {
        let reviews = parse_pr_reviews_ndjson("").unwrap();
        assert!(reviews.is_empty());
    }

    // --- infer_github_host tests ---

    #[test]
    fn test_infer_github_host_public_owner() {
        assert_eq!(infer_github_host("octocat", None), "github.com");
    }

    #[test]
    fn test_infer_github_host_empty() {
        assert_eq!(infer_github_host("", None), "github.com");
    }

    #[test]
    fn test_infer_github_host_with_injected_config() {
        use crate::config::{GhHostConfig, LabConfig};
        use std::collections::HashMap;

        let mut hosts = HashMap::new();
        hosts.insert(
            "corp".to_string(),
            GhHostConfig {
                host: "github.corp.example.com".to_string(),
                web_url: None,
            },
        );

        let cfg = LabConfig {
            github_hosts: hosts,
            daemon: crate::config::DaemonConfig {
                repos: vec!["corp:acme/widgets".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };

        // Owner present in config -> returns GHE host
        assert_eq!(
            infer_github_host("acme", Some(&cfg)),
            "github.corp.example.com"
        );
        // Owner not in config -> falls back to github.com
        assert_eq!(infer_github_host("unknown", Some(&cfg)), "github.com");
    }

    // --- IssueInfo deserialization tests ---

    #[test]
    fn test_issue_info_deserialize_full() {
        let json = r#"{"number": 42, "title": "Fix the bug", "body": "Details here"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.title, "Fix the bug");
        assert_eq!(info.body.as_deref(), Some("Details here"));
    }

    #[test]
    fn test_issue_info_deserialize_null_body() {
        let json = r#"{"number": 1, "title": "No body", "body": null}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.title, "No body");
        assert!(info.body.is_none());
    }

    #[test]
    fn test_issue_info_deserialize_missing_body() {
        // serde treats a missing Option<T> field as None by default
        let json = r#"{"number": 5, "title": "Minimal"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert!(info.body.is_none());
    }

    #[test]
    fn test_issue_info_deserialize_extra_fields() {
        let json = r#"{"number": 10, "title": "Has extras", "body": "body", "labels": [], "url": "https://example.com"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.title, "Has extras");
    }

    #[test]
    fn test_issue_info_deserialize_missing_title() {
        let json = r#"{"number": 5}"#;
        let result: Result<IssueInfo, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // --- CandidateIssue deserialization tests ---

    #[test]
    fn test_candidate_issue_deserialize() {
        let json = r#"[{"number": 1}, {"number": 42}, {"number": 100}]"#;
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].number, 1);
        assert_eq!(items[1].number, 42);
        assert_eq!(items[2].number, 100);
    }

    #[test]
    fn test_candidate_issue_deserialize_empty() {
        let json = "[]";
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_candidate_issue_deserialize_extra_fields() {
        let json = r#"[{"number": 5, "title": "ignored", "url": "https://example.com"}]"#;
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 5);
    }

    #[test]
    fn test_candidate_issue_deserialize_missing_number() {
        let json = r#"[{"title": "no number"}]"#;
        let result: Result<Vec<CandidateIssue>, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_candidate_issue_with_body() {
        let json = r#"[{"number": 7, "body": "**Blocked by:** #3, #5"}]"#;
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(items[0].number, 7);
        assert_eq!(items[0].body.as_deref(), Some("**Blocked by:** #3, #5"));
    }

    #[test]
    fn test_candidate_issue_null_body() {
        let json = r#"[{"number": 8, "body": null}]"#;
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(items[0].number, 8);
        assert!(items[0].body.is_none());
    }

    #[test]
    fn test_candidate_issue_missing_body() {
        let json = r#"[{"number": 9}]"#;
        let items: Vec<CandidateIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(items[0].number, 9);
        assert!(items[0].body.is_none());
    }

    // --- Search query construction tests ---

    #[test]
    fn test_build_ready_issues_search_query_simple() {
        let query = build_ready_issues_search_query("gru:todo");
        assert_eq!(
            query,
            "label:\"gru:todo\" -is:blocked -label:\"gru:blocked\" -label:\"gru:in-progress\""
        );
    }

    #[test]
    fn test_build_ready_issues_search_query_with_spaces() {
        let query = build_ready_issues_search_query("ready for minion");
        assert!(query.starts_with("label:\"ready for minion\""));
    }

    #[test]
    fn test_build_ready_issues_search_query_escapes_quotes() {
        let query = build_ready_issues_search_query(r#"label"with"quotes"#);
        // Input quotes are escaped: label"with"quotes → label\"with\"quotes
        assert!(query.starts_with(r#"label:"label\"with\"quotes""#));
    }

    #[test]
    fn test_build_ready_issues_search_query_escapes_backslashes() {
        let query = build_ready_issues_search_query(r"back\slash");
        // Input backslash is escaped: back\slash → back\\slash
        assert!(query.starts_with(r#"label:"back\\slash""#));
    }

    // --- gh_cli_command tests ---

    #[test]
    fn test_gh_cli_command_github_com() {
        let cmd = gh_cli_command("github.com");
        // Should use "gh" binary
        assert_eq!(cmd.as_std().get_program(), "gh");
        // Should always set GH_HOST for deterministic host selection
        let gh_host = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "GH_HOST")
            .and_then(|(_, v)| v)
            .map(|v| v.to_str().unwrap().to_string());
        assert_eq!(gh_host.as_deref(), Some("github.com"));
    }

    #[test]
    fn test_gh_cli_command_ghe_host() {
        let cmd = gh_cli_command("git.example.com");
        // Should still use "gh" binary (not "ghe")
        assert_eq!(cmd.as_std().get_program(), "gh");
        // Should set GH_HOST for non-github.com hosts
        let gh_host = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "GH_HOST")
            .and_then(|(_, v)| v)
            .map(|v| v.to_str().unwrap().to_string());
        assert_eq!(gh_host.as_deref(), Some("git.example.com"));
    }

    // --- CLI function integration tests ---
    // These require real gh CLI auth. Run with: cargo test cli_via -- --ignored

    #[tokio::test]
    #[ignore]
    async fn test_check_auth_via_cli_github_com() {
        let result = check_auth_via_cli("github.com").await;
        // Will pass if gh is authenticated for github.com
        assert!(result.is_ok(), "gh auth status failed: {:?}", result.err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_default_branch_github_com() {
        // Requires gh auth for github.com
        let result = get_default_branch("github.com", "fotoetienne", "gru").await;
        match result {
            Ok(branch) => {
                assert!(!branch.is_empty(), "Default branch should not be empty");
                // fotoetienne/gru uses "main"
                assert_eq!(branch, "main");
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Failed to") || msg.contains("gh"),
                    "Unexpected error: {}",
                    msg
                );
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_post_comment_via_cli() {
        // Requires write access to a test repo
        let result = post_comment_via_cli(
            "github.com",
            "your-username",
            "your-test-repo",
            1,
            "Test comment from CLI",
        )
        .await;
        assert!(
            result.is_ok(),
            "post_comment_via_cli failed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_edit_labels_via_cli() {
        // Requires write access to a test repo
        let result = edit_labels_via_cli(
            "github.com",
            "your-username",
            "your-test-repo",
            1,
            &["test-label"],
            &[],
        )
        .await;
        assert!(
            result.is_ok(),
            "edit_labels_via_cli failed: {:?}",
            result.err()
        );
    }

    // --- check_issue_eligibility tests ---

    fn make_labels(names: &[&str]) -> Vec<IssueLabel> {
        names
            .iter()
            .map(|n| IssueLabel {
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_eligibility_open_no_labels() {
        let (eligible, reason) = check_issue_eligibility("OPEN", &[]);
        assert!(eligible);
        assert!(reason.is_none());
    }

    #[test]
    fn test_eligibility_open_with_todo_label() {
        let labels = make_labels(&["gru:todo"]);
        let (eligible, _) = check_issue_eligibility("OPEN", &labels);
        assert!(eligible);
    }

    #[test]
    fn test_eligibility_closed_state() {
        let (eligible, reason) = check_issue_eligibility("CLOSED", &[]);
        assert!(!eligible);
        assert!(reason.unwrap().contains("no longer open"));
    }

    #[test]
    fn test_eligibility_unexpected_state() {
        let (eligible, reason) = check_issue_eligibility("MERGED", &[]);
        assert!(!eligible);
        assert!(reason.unwrap().contains("MERGED"));
    }

    #[test]
    fn test_eligibility_in_progress_label() {
        let labels = make_labels(&["gru:in-progress"]);
        let (eligible, reason) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible);
        assert!(reason.unwrap().contains("gru:in-progress"));
    }

    #[test]
    fn test_eligibility_done_label() {
        let labels = make_labels(&["gru:done"]);
        let (eligible, reason) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible);
        assert!(reason.unwrap().contains("gru:done"));
    }

    #[test]
    fn test_eligibility_failed_label() {
        let labels = make_labels(&["gru:failed"]);
        let (eligible, reason) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible);
        assert!(reason.unwrap().contains("gru:failed"));
    }

    #[test]
    fn test_eligibility_mixed_labels_one_ineligible() {
        let labels = make_labels(&["bug", "gru:done", "priority:high"]);
        let (eligible, _) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible);
    }

    #[test]
    fn test_eligibility_state_checked_before_labels() {
        // Closed + ineligible label: should report state, not label
        let labels = make_labels(&["gru:in-progress"]);
        let (eligible, reason) = check_issue_eligibility("CLOSED", &labels);
        assert!(!eligible);
        assert!(reason.unwrap().contains("no longer open"));
    }

    #[test]
    fn test_sanitize_display_name_normal() {
        assert_eq!(
            sanitize_display_name("Alice Johnson", "alicej"),
            "Alice Johnson"
        );
    }

    #[test]
    fn test_sanitize_display_name_empty_falls_back_to_login() {
        // JSON null from the API becomes an empty string before reaching this function
        assert_eq!(sanitize_display_name("", "alicej"), "alicej");
    }

    #[test]
    fn test_sanitize_display_name_literal_null_string_is_valid() {
        // A user whose display name really is "null" should not be mistaken for absent
        assert_eq!(sanitize_display_name("null", "alicej"), "null");
    }

    #[test]
    fn test_sanitize_display_name_strips_control_chars() {
        // Embedded control characters (e.g., newlines) should be removed
        assert_eq!(
            sanitize_display_name("Evil\nInjection", "bad-actor"),
            "EvilInjection"
        );
    }

    #[test]
    fn test_sanitize_display_name_strips_backticks() {
        // Backticks would break the code-span used in the prompt
        assert_eq!(
            sanitize_display_name("Alice`s Name", "alicej"),
            "Alices Name"
        );
    }

    #[test]
    fn test_sanitize_display_name_truncates_at_100() {
        let long_name = "A".repeat(200);
        let result = sanitize_display_name(&long_name, "u");
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_has_any_label_parses_present_label() {
        // Simulate the JSON returned by `gh issue view --json labels`
        #[derive(serde::Deserialize)]
        struct LabelsOnly {
            #[serde(default)]
            labels: Vec<IssueLabel>,
        }
        let json = r#"{"labels":[{"name":"gru:done"},{"name":"bug"}]}"#;
        let info: LabelsOnly = serde_json::from_str(json).unwrap();
        let targets = ["gru:done", "gru:failed"];
        let found = info
            .labels
            .iter()
            .find(|l| targets.contains(&l.name.as_str()));
        assert_eq!(found.unwrap().name, "gru:done");
    }

    #[test]
    fn test_has_any_label_finds_failed() {
        #[derive(serde::Deserialize)]
        struct LabelsOnly {
            #[serde(default)]
            labels: Vec<IssueLabel>,
        }
        let json = r#"{"labels":[{"name":"gru:failed"},{"name":"bug"}]}"#;
        let info: LabelsOnly = serde_json::from_str(json).unwrap();
        let targets = ["gru:done", "gru:failed"];
        let found = info
            .labels
            .iter()
            .find(|l| targets.contains(&l.name.as_str()));
        assert_eq!(found.unwrap().name, "gru:failed");
    }

    #[test]
    fn test_has_any_label_parses_absent_label() {
        #[derive(serde::Deserialize)]
        struct LabelsOnly {
            #[serde(default)]
            labels: Vec<IssueLabel>,
        }
        let json = r#"{"labels":[{"name":"gru:in-progress"},{"name":"bug"}]}"#;
        let info: LabelsOnly = serde_json::from_str(json).unwrap();
        let targets = ["gru:done", "gru:failed"];
        let found = info
            .labels
            .iter()
            .find(|l| targets.contains(&l.name.as_str()));
        assert!(found.is_none());
    }

    #[test]
    fn test_has_any_label_parses_empty_labels() {
        #[derive(serde::Deserialize)]
        struct LabelsOnly {
            #[serde(default)]
            labels: Vec<IssueLabel>,
        }
        let json = r#"{"labels":[]}"#;
        let info: LabelsOnly = serde_json::from_str(json).unwrap();
        let targets = ["gru:done", "gru:failed"];
        let found = info
            .labels
            .iter()
            .find(|l| targets.contains(&l.name.as_str()));
        assert!(found.is_none());
    }

    // --- is_rate_limit_error tests ---

    #[test]
    fn test_is_rate_limit_error_matches() {
        assert!(is_rate_limit_error("API rate limit exceeded"));
        assert!(is_rate_limit_error("rate limit exceeded for user"));
        assert!(is_rate_limit_error("secondary rate-limit hit"));
        assert!(is_rate_limit_error("HTTP 429 Too Many Requests"));
        assert!(is_rate_limit_error("too many requests"));
        // Bare HTTP status code
        assert!(is_rate_limit_error("HTTP 429"));
        assert!(is_rate_limit_error("status: 429"));
    }

    #[test]
    fn test_is_rate_limit_error_case_insensitive() {
        assert!(is_rate_limit_error("RATE LIMIT exceeded"));
        assert!(is_rate_limit_error("Rate Limit"));
        assert!(is_rate_limit_error("TOO MANY REQUESTS"));
    }

    #[test]
    fn test_is_rate_limit_error_non_matching() {
        assert!(!is_rate_limit_error("HTTP 502 Bad Gateway"));
        assert!(!is_rate_limit_error("connection timed out"));
        assert!(!is_rate_limit_error("not found"));
        assert!(!is_rate_limit_error("unauthorized"));
        assert!(!is_rate_limit_error(""));
        // Digits embedded in larger numbers should not match
        assert!(!is_rate_limit_error("request_id=142938"));
        assert!(!is_rate_limit_error("timestamp: 14291234"));
    }

    // --- contains_standalone_429 tests ---

    #[test]
    fn test_contains_standalone_429_matches() {
        assert!(contains_standalone_429("429"));
        assert!(contains_standalone_429("HTTP 429"));
        assert!(contains_standalone_429("status: 429"));
        assert!(contains_standalone_429("error 429 rate"));
        assert!(contains_standalone_429("429 Too Many"));
    }

    #[test]
    fn test_contains_standalone_429_rejects_embedded_digits() {
        assert!(!contains_standalone_429("14291234"));
        assert!(!contains_standalone_429("id=142938"));
        assert!(!contains_standalone_429("4290"));
        assert!(!contains_standalone_429("1429"));
    }

    #[test]
    fn test_contains_standalone_429_edge_cases() {
        assert!(!contains_standalone_429(""));
        assert!(!contains_standalone_429("42"));
        assert!(contains_standalone_429("x429x"));
        assert!(!contains_standalone_429("x4290"));
    }

    #[test]
    fn test_is_rate_limit_error_429_not_embedded_in_numbers() {
        // "429" embedded in a larger number should NOT match
        assert!(!is_rate_limit_error("request-id: 14291"));
        assert!(!is_rate_limit_error("timestamp 1714290000"));
        assert!(!is_rate_limit_error("error code 5429"));
        // But standalone 429 should match
        assert!(is_rate_limit_error("error 429"));
        assert!(is_rate_limit_error("429 rate limited"));
        assert!(is_rate_limit_error("status=429;"));
    }

    #[test]
    fn test_contains_standalone_429() {
        assert!(contains_standalone_429("429"));
        assert!(contains_standalone_429("HTTP 429"));
        assert!(contains_standalone_429("429 error"));
        assert!(contains_standalone_429("status:429"));
        assert!(!contains_standalone_429("1429"));
        assert!(!contains_standalone_429("4290"));
        assert!(!contains_standalone_429("14290"));
        assert!(!contains_standalone_429("42"));
        assert!(!contains_standalone_429(""));
    }

    // --- RateLimitResponse deserialization test ---

    #[test]
    fn test_rate_limit_response_deserialize() {
        let json = r#"{"rate": {"limit": 5000, "remaining": 0, "reset": 1700000000}}"#;
        let parsed: RateLimitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.rate.reset, 1700000000);
        assert_eq!(parsed.rate.remaining, 0);
    }

    #[test]
    fn test_rate_limit_response_deserialize_with_remaining() {
        let json = r#"{"rate": {"limit": 5000, "remaining": 4200, "reset": 1700003600}}"#;
        let parsed: RateLimitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.rate.remaining, 4200);
        assert_eq!(parsed.rate.reset, 1700003600);
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_label_via_cli() {
        // Requires write access to a test repo
        let result = create_label_via_cli(
            "github.com",
            "your-username",
            "your-test-repo",
            "test-cli-label",
            "0E8A16",
            "Test label created via CLI",
        )
        .await;
        assert!(
            result.is_ok(),
            "create_label_via_cli failed: {:?}",
            result.err()
        );
    }

    // --- is_retryable_error tests ---

    #[test]
    fn test_is_retryable_error_http_status_codes() {
        assert!(is_retryable_error("HTTP 502 Bad Gateway"));
        assert!(is_retryable_error("error: 503 Service Unavailable"));
        assert!(is_retryable_error("status: 504 Gateway Timeout"));
        // 429 is a rate limit error, handled separately
        assert!(!is_retryable_error("HTTP 429 Too Many Requests"));
    }

    #[test]
    fn test_is_retryable_error_network_errors() {
        assert!(is_retryable_error("connection timed out"));
        assert!(is_retryable_error("ETIMEDOUT"));
        assert!(is_retryable_error("connection reset by peer"));
        assert!(is_retryable_error("ECONNRESET"));
        assert!(is_retryable_error("connection refused"));
        assert!(is_retryable_error("ECONNREFUSED"));
        assert!(is_retryable_error("network unreachable"));
    }

    #[test]
    fn test_is_retryable_error_dns_errors() {
        assert!(is_retryable_error("could not resolve host"));
        assert!(is_retryable_error("name resolution failed"));
    }

    #[test]
    fn test_is_retryable_error_server_errors() {
        assert!(is_retryable_error("Internal Server Error"));
        assert!(is_retryable_error("Service Unavailable"));
        assert!(is_retryable_error("Bad Gateway"));
        assert!(is_retryable_error("Gateway Timeout"));
    }

    #[test]
    fn test_is_retryable_error_generic_transient() {
        assert!(is_retryable_error("temporary failure"));
        assert!(is_retryable_error("please try again later"));
        // Rate limit errors are handled separately, not retryable via backoff
        assert!(!is_retryable_error("rate limit exceeded"));
        assert!(!is_retryable_error("rate-limit"));
    }

    #[test]
    fn test_is_retryable_error_non_retryable() {
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
        assert!(is_retryable_error("TIMEOUT"));
        assert!(is_retryable_error("Timeout"));
        assert!(is_retryable_error("SERVICE UNAVAILABLE"));
    }

    // --- calculate_retry_delay tests ---

    #[test]
    fn test_retry_delay_progression() {
        assert_eq!(calculate_retry_delay(1), 2);
        assert_eq!(calculate_retry_delay(2), 4);
        assert_eq!(calculate_retry_delay(3), 8);
        assert_eq!(calculate_retry_delay(4), 16);
        assert_eq!(calculate_retry_delay(5), 32);
    }

    #[test]
    fn test_retry_delay_caps_at_max() {
        assert_eq!(calculate_retry_delay(6), MAX_DELAY_SECS);
        assert_eq!(calculate_retry_delay(7), MAX_DELAY_SECS);
        assert_eq!(calculate_retry_delay(10), MAX_DELAY_SECS);
    }

    // --- priority_sort_key tests ---

    #[test]
    fn test_priority_sort_key_critical() {
        assert_eq!(priority_sort_key(&make_labels(&["priority:critical"])), 0);
    }

    #[test]
    fn test_priority_sort_key_high() {
        assert_eq!(priority_sort_key(&make_labels(&["priority:high"])), 1);
    }

    #[test]
    fn test_priority_sort_key_medium() {
        assert_eq!(priority_sort_key(&make_labels(&["priority:medium"])), 2);
    }

    #[test]
    fn test_priority_sort_key_unlabeled() {
        assert_eq!(priority_sort_key(&[]), 3);
    }

    #[test]
    fn test_priority_sort_key_low() {
        assert_eq!(priority_sort_key(&make_labels(&["priority:low"])), 4);
    }

    #[test]
    fn test_priority_sort_key_non_priority_labels_treated_as_unlabeled() {
        assert_eq!(priority_sort_key(&make_labels(&["bug", "enhancement"])), 3);
    }

    #[test]
    fn test_priority_sort_key_mixed_labels_picks_priority() {
        assert_eq!(
            priority_sort_key(&make_labels(&["bug", "priority:high", "enhancement"])),
            1
        );
    }

    #[test]
    fn test_priority_sort_key_multiple_labels_highest_wins() {
        // If multiple priority labels exist, the highest priority wins
        assert_eq!(
            priority_sort_key(&make_labels(&["priority:low", "priority:critical"])),
            0
        );
    }

    #[test]
    fn test_candidate_issues_sort_by_priority() {
        let mut candidates = [
            CandidateIssue {
                number: 1,
                body: None,
                labels: make_labels(&["priority:low"]),
            },
            CandidateIssue {
                number: 2,
                body: None,
                labels: vec![],
            },
            CandidateIssue {
                number: 3,
                body: None,
                labels: make_labels(&["priority:critical"]),
            },
            CandidateIssue {
                number: 4,
                body: None,
                labels: make_labels(&["priority:high"]),
            },
        ];

        candidates.sort_by_key(|c| priority_sort_key(&c.labels));

        let order: Vec<u64> = candidates.iter().map(|c| c.number).collect();
        assert_eq!(order, vec![3, 4, 2, 1]);
    }

    #[test]
    fn test_stable_sort_preserves_order_within_tier() {
        let mut candidates = [
            CandidateIssue {
                number: 10,
                body: None,
                labels: make_labels(&["priority:high"]),
            },
            CandidateIssue {
                number: 20,
                body: None,
                labels: make_labels(&["priority:high"]),
            },
            CandidateIssue {
                number: 30,
                body: None,
                labels: make_labels(&["priority:high"]),
            },
        ];

        candidates.sort_by_key(|c| priority_sort_key(&c.labels));

        let order: Vec<u64> = candidates.iter().map(|c| c.number).collect();
        // Stable sort preserves original order within same priority tier
        assert_eq!(order, vec![10, 20, 30]);
    }
}
