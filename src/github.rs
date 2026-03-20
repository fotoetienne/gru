use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

use crate::labels;

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
    let loaded;
    let cfg = match config {
        Some(c) => Some(c),
        None => {
            loaded = crate::config::try_load_config();
            loaded.as_ref()
        }
    };
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
    let output = gh_cli_command(host)
        .args(args)
        .output()
        .await
        .with_context(|| format!("Failed to execute: gh {}", args_display))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh {} failed: {}", args_display, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Build a full GitHub issue URL for a repo in "owner/repo" format, with an explicit host.
///
/// Returns `Some(url)` when `repo` is a valid `owner/repo` string, otherwise `None`.
pub fn build_issue_url_with_host(repo: &str, host: &str, issue_number: u64) -> Option<String> {
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
pub async fn mark_pr_ready_via_cli(
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

pub async fn list_ready_issues_via_cli(
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
            "number,body",
            "--limit",
            "100",
        ],
    )
    .await?;

    let items: Vec<CandidateIssue> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list JSON output")?;

    Ok(items)
}

/// Issue candidate returned by list queries, with optional body for dependency checking
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CandidateIssue {
    pub number: u64,
    #[serde(default)]
    pub body: Option<String>,
}

/// Fetch issue details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - Issue number
pub async fn get_issue_via_cli(
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

/// Check if a GitHub issue is closed (or has a merged/closed PR).
///
/// Returns `true` if the issue state is `CLOSED` (the GraphQL enum value
/// returned by `gh issue view --json state`).
pub async fn is_issue_closed_via_cli(
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
            "issue",
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
    Ok(state == "CLOSED")
}

/// Check whether a PR is still open (i.e., not merged or closed).
///
/// Returns `true` if the PR state is "OPEN", `false` otherwise.
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `host` - GitHub hostname
/// * `number` - PR number
pub async fn is_pr_open_via_cli(owner: &str, repo: &str, host: &str, number: u64) -> Result<bool> {
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
    Ok(state == "OPEN")
}

/// Simple struct to hold issue information from gh CLI
#[derive(Debug, serde::Deserialize)]
pub struct IssueInfo {
    pub title: String,
    pub body: Option<String>,
    /// Labels attached to the issue (from `gh issue view --json labels`)
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
}

/// Label info returned by `gh issue view --json labels`
#[derive(Debug, serde::Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

/// Simple struct to hold PR information from gh CLI
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    pub title: String,
    pub body: Option<String>,
    pub head_ref_name: String,
}

/// Fetch PR details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - PR number
pub async fn get_pr_via_cli(owner: &str, repo: &str, host: &str, number: u64) -> Result<PrInfo> {
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
pub async fn create_draft_pr_via_cli(
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
pub async fn post_comment_via_cli(
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

/// Edit labels on an issue using gh CLI (add and/or remove in a single call)
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
/// * `add` - Labels to add
/// * `remove` - Labels to remove
pub async fn edit_labels_via_cli(
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
pub async fn create_label_via_cli(
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
pub async fn check_auth_via_cli(host: &str) -> Result<()> {
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
pub async fn claim_issue_via_cli(
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
pub async fn mark_issue_done_via_cli(
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
pub async fn mark_issue_failed_via_cli(
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
pub async fn remove_blocked_label(
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
pub async fn mark_issue_blocked_via_cli(
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
pub struct PrReview {
    pub user: ReviewUser,
    pub commit_id: String,
    pub state: String,
}

/// The user portion of a PR review response.
#[derive(Debug, serde::Deserialize)]
pub struct ReviewUser {
    pub login: String,
}

/// Returns the login of the currently authenticated `gh` CLI user.
///
/// Uses `gh api user` to fetch the authenticated account. Returns an error
/// if `gh` is not authenticated or the API call fails.
pub async fn get_authenticated_user(host: &str) -> Result<String> {
    let stdout = run_gh(host, &["api", "user", "--jq", ".login"]).await?;

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
pub async fn list_pr_reviews(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<Vec<PrReview>> {
    let endpoint = format!("repos/{}/{}/pulls/{}/reviews", owner, repo, pr_number);
    let stdout = run_gh(host, &["api", &endpoint, "--paginate", "--jq", ".[]"]).await?;

    // --paginate --jq '.[]' outputs one JSON object per line (NDJSON).
    parse_pr_reviews_ndjson(&stdout)
}

/// Parse a newline-delimited JSON stream of PR review objects.
///
/// `--paginate --jq '.[]'` emits one JSON object per line; this helper
/// is extracted for unit testing without network access.
pub fn parse_pr_reviews_ndjson(ndjson: &str) -> Result<Vec<PrReview>> {
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
pub async fn has_gru_review_for_sha(
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
pub fn review_exists_for_sha(reviews: &[PrReview], user_login: &str, sha: &str) -> bool {
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
}
