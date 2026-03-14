use anyhow::{anyhow, Context, Result};
use octocrab::{models, Octocrab};
use std::env;
use tokio::process::Command;

use crate::labels;

// ============================================================================
// Token Extraction Helpers
// ============================================================================

/// Try to extract GitHub token from gh/ghe CLI
///
/// # Arguments
/// * `host` - GitHub hostname (e.g., "github.com" or "ghe.netflix.net")
///
/// Returns the token if successfully extracted, or None if gh/ghe is not available
async fn try_get_token_from_cli_for_host(host: &str) -> Option<String> {
    let gh_cmd = gh_command_for_host(host);

    // Try to get token from CLI
    let output = Command::new(gh_cmd)
        .args(["auth", "token", "--hostname", host])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim().to_string();

    if token.is_empty() {
        return None;
    }

    Some(token)
}

/// Infer GitHub hostname from repository owner.
///
/// This is a fallback heuristic used when the host isn't known from a URL.
/// Prefer using the host from `parse_github_remote` or `parse_github_url` when available,
/// or passing the host explicitly via `from_env_with_host` / `try_from_env_with_host`.
///
/// Checks `daemon.repos` config entries for an owner match with an explicit GHE host.
/// Falls back to a substring heuristic for "netflix" owners.
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
///
/// Returns the appropriate GitHub hostname
pub(crate) fn infer_github_host(owner: &str) -> String {
    // Check daemon.repos config for an explicit host for this owner
    let cfg = crate::config::try_load_config();
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

    // Fallback: substring heuristic
    if owner == "netflix" || owner.contains("netflix") {
        "git.netflix.net".to_string()
    } else {
        "github.com".to_string()
    }
}

/// Returns the correct `gh` CLI command for a given GitHub host.
///
/// Returns `"ghe"` for non-`github.com` hosts, `"gh"` otherwise.
pub fn gh_command_for_host(host: &str) -> &'static str {
    if host == "github.com" {
        "gh"
    } else {
        "ghe"
    }
}

/// Creates a pre-configured `tokio::process::Command` for the `gh` CLI.
///
/// Always uses the `gh` binary and sets `GH_HOST` for non-`github.com`
/// hosts so authentication targets the correct server.
pub fn gh_cli_command(host: &str) -> Command {
    let mut cmd = Command::new("gh");
    if host != "github.com" {
        cmd.env("GH_HOST", host);
    }
    cmd
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

/// Determine the correct `gh` CLI command for a repository.
///
/// Returns `"ghe"` for non-`github.com` hosts, `"gh"` otherwise.
///
/// # Arguments
/// * `repo` - Repository identifier in "owner/repo" format
pub fn gh_command_for_repo(repo: &str) -> &'static str {
    let owner = repo.split('/').next().unwrap_or("");
    let host = infer_github_host(owner);
    gh_command_for_host(&host)
}

/// Get GitHub token with automatic fallback logic
///
/// Priority order:
/// 1. Try gh/ghe CLI (respects existing authentication)
/// 2. Fall back to GRU_GITHUB_TOKEN environment variable
/// 3. Return error with helpful message
///
/// # Arguments
/// * `host` - GitHub hostname (e.g., "github.com" or "ghe.netflix.net")
async fn get_github_token_for_host(host: &str) -> Result<String> {
    // Try CLI first
    if let Some(token) = try_get_token_from_cli_for_host(host).await {
        return Ok(token);
    }

    // Fall back to environment variable
    if let Ok(token) = env::var("GRU_GITHUB_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }

    // Provide helpful error message
    let gh_cmd = gh_command_for_host(host);

    Err(anyhow!(
        "No GitHub authentication found.\n\n\
         To authenticate, choose one option:\n\n\
         1. Use {} CLI (recommended):\n   \
            {} auth login\n\n\
         2. Set environment variable:\n   \
            export GRU_GITHUB_TOKEN=\"ghp_xxxx\"\n\n\
         Need help? https://cli.github.com/manual/gh_auth_login",
        gh_cmd,
        gh_cmd
    ))
}

// ============================================================================
// Octocrab API Client
// ============================================================================

/// GitHub API client wrapper using octocrab
#[derive(Debug)]
pub struct GitHubClient {
    client: Octocrab,
}

impl GitHubClient {
    /// Initialize a new GitHub client targeting a specific host.
    ///
    /// For non-`github.com` hosts, sets the octocrab `base_uri` to
    /// `https://{host}/api/v3` for GitHub Enterprise compatibility.
    pub fn new_with_host(token: String, host: &str) -> Result<Self> {
        if token.is_empty() {
            return Err(anyhow!("GitHub token is empty"));
        }

        let mut builder = Octocrab::builder().personal_token(token);

        if host != "github.com" {
            let base_uri = format!("https://{}/api/v3", host);
            builder = builder.base_uri(base_uri).context("Invalid GHE base URI")?;
        }

        let client = builder.build().context("Failed to build GitHub client")?;

        Ok(Self { client })
    }

    /// Initialize a new GitHub client for a specific host.
    ///
    /// For non-`github.com` hosts, sets the octocrab `base_uri` to
    /// `https://{host}/api/v3` for GitHub Enterprise compatibility.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `host` - GitHub hostname (e.g., "github.com" or "ghe.netflix.net")
    pub async fn from_env_with_host(owner: &str, repo: &str, host: &str) -> Result<Self> {
        let _ = (owner, repo); // reserved for future per-repo logic
        let token = get_github_token_for_host(host).await?;
        Self::new_with_host(token, host)
    }

    /// Initialize a new GitHub client, inferring the host from the owner.
    ///
    /// This is a convenience wrapper around `from_env_with_host` for callers
    /// that don't have an explicit host (e.g., ad-hoc commands working from
    /// a local git checkout). Prefer `from_env_with_host` when the host is known.
    #[cfg(test)]
    pub async fn from_env(owner: &str, repo: &str) -> Result<Self> {
        let host = infer_github_host(owner);
        Self::from_env_with_host(owner, repo, &host).await
    }

    /// Try to initialize a new GitHub client with token from environment or gh/ghe CLI
    ///
    /// Returns `None` if no authentication is found, instead of an error.
    /// This allows graceful fallback to CLI methods.
    ///
    /// # Arguments
    /// * `host` - GitHub hostname (e.g., "github.com" or "git.netflix.net")
    pub async fn try_from_env_with_host(host: &str) -> Option<Self> {
        let token = get_github_token_for_host(host).await.ok()?;
        Self::new_with_host(token, host).ok()
    }

    /// Fetch issue details
    ///
    /// # Arguments
    /// * `owner` - Repository owner (user or organization)
    /// * `repo` - Repository name
    /// * `number` - Issue number
    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<models::issues::Issue> {
        self.client
            .issues(owner, repo)
            .get(number)
            .await
            .context(format!(
                "Failed to fetch issue #{} from {}/{}",
                number, owner, repo
            ))
    }

    /// Fetch pull request details
    ///
    /// # Arguments
    /// * `owner` - Repository owner (user or organization)
    /// * `repo` - Repository name
    /// * `number` - PR number
    pub async fn get_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<models::pulls::PullRequest> {
        self.client
            .pulls(owner, repo)
            .get(number)
            .await
            .context(format!(
                "Failed to fetch PR #{} from {}/{}",
                number, owner, repo
            ))
    }

    /// Post a comment on an issue
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    /// * `body` - Comment body (markdown supported)
    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
        body: &str,
    ) -> Result<models::issues::Comment> {
        self.client
            .issues(owner, repo)
            .create_comment(issue, body)
            .await
            .context(format!(
                "Failed to post comment on issue #{} in {}/{}",
                issue, owner, repo
            ))
    }

    /// Add a label to an issue
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    /// * `label` - Label name to add
    pub async fn add_label(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
        label: &str,
    ) -> Result<Vec<models::Label>> {
        self.client
            .issues(owner, repo)
            .add_labels(issue, &[label.to_string()])
            .await
            .context(format!(
                "Failed to add label '{}' to issue #{} in {}/{}",
                label, issue, owner, repo
            ))
    }

    /// Remove a label from an issue
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    /// * `label` - Label name to remove
    pub async fn remove_label(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
        label: &str,
    ) -> Result<()> {
        self.client
            .issues(owner, repo)
            .remove_label(issue, label)
            .await
            .context(format!(
                "Failed to remove label '{}' from issue #{} in {}/{}",
                label, issue, owner, repo
            ))?;
        Ok(())
    }

    /// Claim an issue by transitioning from gru:todo to gru:in-progress.
    ///
    /// This operation is designed for fire-and-forget usage. While it returns a Result,
    /// callers typically log errors but don't block the main workflow.
    ///
    /// # Race Conditions
    /// This method attempts to detect if another Minion already claimed the issue
    /// by checking for the in-progress label. However, there is a TOCTOU window
    /// between the check and the label addition. Multiple Minions could pass the
    /// check simultaneously and both claim the issue. In V1, we accept this limitation.
    /// For V2+, consider using GitHub issue assignment or comment-based coordination.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    ///
    /// # Returns
    /// * `Ok(true)` - Successfully claimed (no race detected)
    /// * `Ok(false)` - Already claimed by another Minion (race detected)
    /// * `Err(_)` - API call failed (network error, auth error, etc.)
    pub async fn claim_issue(&self, owner: &str, repo: &str, issue: u64) -> Result<bool> {
        // First, check current labels to detect race conditions
        let issue_info = self.get_issue(owner, repo, issue).await?;
        let current_labels: Vec<String> =
            issue_info.labels.iter().map(|l| l.name.clone()).collect();

        // If already in-progress, another Minion claimed it
        if labels::has_label(&current_labels, labels::IN_PROGRESS) {
            return Ok(false);
        }

        // Remove todo label if present
        let _ = self.remove_label(owner, repo, issue, labels::TODO).await;

        // Add in-progress label
        self.add_label(owner, repo, issue, labels::IN_PROGRESS)
            .await?;

        Ok(true)
    }

    /// Mark an issue as completed by transitioning from gru:in-progress to gru:done.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    #[allow(dead_code)] // Currently unused — retained for future lab.rs migration
    pub async fn mark_issue_done(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove in-progress label (ignore errors - may not exist)
        let _ = self
            .remove_label(owner, repo, issue, labels::IN_PROGRESS)
            .await;

        self.add_label(owner, repo, issue, labels::DONE).await?;

        Ok(())
    }

    /// Mark an issue as failed by transitioning from gru:in-progress to gru:failed.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    pub async fn mark_issue_failed(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove in-progress label (ignore errors - may not exist)
        let _ = self
            .remove_label(owner, repo, issue, labels::IN_PROGRESS)
            .await;

        self.add_label(owner, repo, issue, labels::FAILED).await?;

        Ok(())
    }

    /// Mark an issue as blocked by transitioning to gru:blocked.
    ///
    /// Used when a minion is stuck (inactivity timeout), the task times out,
    /// or CI fix attempts are exhausted. Signals that human intervention is needed.
    ///
    /// Removes in-progress, done, and failed labels if present
    /// to ensure a clean state transition regardless of which phase triggered the block.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    #[allow(dead_code)] // Currently unused — retained for future lab.rs migration
    pub async fn mark_issue_blocked(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove any existing state labels (ignore errors)
        let _ = self
            .remove_label(owner, repo, issue, labels::IN_PROGRESS)
            .await;
        let _ = self.remove_label(owner, repo, issue, labels::DONE).await;
        let _ = self.remove_label(owner, repo, issue, labels::FAILED).await;

        self.add_label(owner, repo, issue, labels::BLOCKED).await?;

        Ok(())
    }

    /// Get the authenticated user information
    ///
    /// Used for validating that the GitHub token is valid and has appropriate access.
    ///
    /// # Returns
    /// The authenticated user's information
    pub async fn get_authenticated_user(&self) -> Result<models::Author> {
        self.client
            .current()
            .user()
            .await
            .context("Failed to fetch authenticated user information")
    }

    /// Create a label in a repository
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `name` - Label name
    /// * `color` - Hex color code (without # prefix)
    /// * `description` - Label description
    ///
    /// # Returns
    /// * `Ok(true)` - Label was created
    /// * `Ok(false)` - Label already exists (idempotent)
    /// * `Err(_)` - Failed to create label
    pub async fn create_label(
        &self,
        owner: &str,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<bool> {
        // Try to create the label
        match self
            .client
            .issues(owner, repo)
            .create_label(name, color, description)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                // Check if the error is because the label already exists
                let err_str = e.to_string();
                if err_str.contains("already_exists") || err_str.contains("already exists") {
                    Ok(false)
                } else {
                    Err(anyhow!(
                        "Failed to create label '{}' in {}/{}: {}",
                        name,
                        owner,
                        repo,
                        e
                    ))
                }
            }
        }
    }

    /// List all issues with a specific label
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `label` - Label name to filter by
    ///
    /// # Returns
    /// List of issues with the specified label
    pub async fn list_issues_with_label(
        &self,
        owner: &str,
        repo: &str,
        label: &str,
    ) -> Result<Vec<models::issues::Issue>> {
        let mut issues = Vec::new();
        let mut page = 1u32;

        loop {
            let page_result = self
                .client
                .issues(owner, repo)
                .list()
                .labels(&[label.to_string()])
                .state(octocrab::params::State::Open)
                .per_page(100)
                .page(page)
                .send()
                .await
                .context(format!(
                    "Failed to list issues with label '{}' in {}/{}",
                    label, owner, repo
                ))?;

            if page_result.items.is_empty() {
                break;
            }

            issues.extend(page_result.items);

            if page_result.next.is_none() {
                break;
            }

            page += 1;
        }

        Ok(issues)
    }
}

// ============================================================================
// gh CLI Helper Functions
// ============================================================================
// These free functions use the gh CLI directly for operations where octocrab
// doesn't provide good support (PR creation, marking ready) or as fallbacks
// when GitHubClient initialization fails (issue fetching).

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
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args(["pr", "ready", pr_number, "--repo", &repo_full])
        .output()
        .await
        .context("Failed to execute gh pr ready command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to mark PR #{} as ready in {}/{}: {}",
            pr_number,
            owner,
            repo,
            stderr
        ));
    }

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
/// List of issue numbers matching the search criteria (capped at 100)
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
) -> Result<Vec<u64>> {
    let repo_full = format!("{}/{}", owner, repo);
    let search_query = build_ready_issues_search_query(label);
    let output = gh_cli_command(host)
        .args([
            "issue",
            "list",
            "--repo",
            &repo_full,
            "--search",
            &search_query,
            "--state",
            "open",
            "--json",
            "number",
            "--limit",
            "100",
        ])
        .output()
        .await
        .context("Failed to execute gh issue list command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to list ready issues in {}: {}",
            repo_full,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let items: Vec<IssueNumber> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list JSON output")?;

    Ok(items.into_iter().map(|i| i.number).collect())
}

/// Helper struct for deserializing issue number from gh CLI JSON
#[derive(Debug, serde::Deserialize)]
struct IssueNumber {
    number: u64,
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
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo_full,
            "--json",
            "number,title,body,labels",
        ])
        .output()
        .await
        .context("Failed to execute gh issue view command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch issue #{} from {}/{}: {}",
            number,
            owner,
            repo,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let issue: IssueInfo =
        serde_json::from_str(&stdout).context("Failed to parse gh issue view JSON output")?;

    Ok(issue)
}

/// Simple struct to hold issue information from gh CLI
#[derive(Debug, serde::Deserialize)]
pub struct IssueInfo {
    #[allow(dead_code)] // Included for serde completeness; callers use .title, .body, and .labels
    pub number: u64,
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
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args([
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            &repo_full,
            "--json",
            "title,body,headRefName",
        ])
        .output()
        .await
        .context("Failed to execute gh pr view command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch PR #{} from {}/{}: {}",
            number,
            owner,
            repo,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args([
            "pr", "create", "--repo", &repo_full, "--head", branch, "--base", base, "--title",
            title, "--body", body, "--draft",
        ])
        .output()
        .await
        .context("Failed to execute gh pr create command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to create draft PR for branch '{}' in {}/{}: {}",
            branch,
            owner,
            repo,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args([
            "issue",
            "comment",
            &number.to_string(),
            "--repo",
            &repo_full,
            "--body",
            body,
        ])
        .output()
        .await
        .context("Failed to execute gh issue comment command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to post comment on #{} in {}: {}",
            number,
            repo_full,
            stderr
        ));
    }

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

    let repo_full = format!("{}/{}", owner, repo);
    let mut args = vec![
        "issue".to_string(),
        "edit".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repo_full.clone(),
    ];

    for label in add {
        args.push("--add-label".to_string());
        args.push(label.to_string());
    }
    for label in remove {
        args.push("--remove-label".to_string());
        args.push(label.to_string());
    }

    let output = gh_cli_command(host)
        .args(&args)
        .output()
        .await
        .context("Failed to execute gh issue edit command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to edit labels on #{} in {}: {}",
            number,
            repo_full,
            stderr
        ));
    }

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
#[allow(dead_code)]
pub async fn create_label_via_cli(
    host: &str,
    owner: &str,
    repo: &str,
    name: &str,
    color: &str,
    description: &str,
) -> Result<()> {
    let repo_full = format!("{}/{}", owner, repo);
    let output = gh_cli_command(host)
        .args([
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
        ])
        .output()
        .await
        .context("Failed to execute gh label create command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to create label '{}' in {}: {}",
            name,
            repo_full,
            stderr
        ));
    }

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
#[allow(dead_code)]
pub async fn check_auth_via_cli(host: &str) -> Result<()> {
    let output = gh_cli_command(host)
        .args(["auth", "status", "--hostname", host])
        .output()
        .await
        .context("Failed to execute gh auth status command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Not authenticated with gh CLI for host {}: {}",
            host,
            stderr
        ));
    }

    Ok(())
}

/// Claim an issue by transitioning labels: remove gru:todo, add gru:in-progress.
///
/// Note: Unlike `GitHubClient::claim_issue`, this does not check whether the
/// issue is already in-progress (race condition guard). Callers should add that
/// check if needed.
///
/// # Arguments
/// * `host` - GitHub hostname
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `number` - Issue number
pub async fn claim_issue_via_cli(host: &str, owner: &str, repo: &str, number: u64) -> Result<()> {
    edit_labels_via_cli(
        host,
        owner,
        repo,
        number,
        &[labels::IN_PROGRESS],
        &[labels::TODO],
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

/// Mark an issue as blocked: add gru:blocked, remove in-progress/done/failed.
///
/// Removes all state labels to ensure a clean transition regardless of
/// which phase triggered the block (matches octocrab `mark_issue_blocked`).
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Octocrab API Client Tests
    #[test]
    fn test_new_with_empty_token() {
        // Should fail with empty token
        let result = GitHubClient::new_with_host(String::new(), "github.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_new_with_valid_token() {
        // Should succeed with valid token format
        let result = GitHubClient::new_with_host("ghp_test123".to_string(), "github.com");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_new_with_host_ghe() {
        // Should succeed and configure GHE base URI
        let result = GitHubClient::new_with_host("ghp_test123".to_string(), "ghe.netflix.net");
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_from_env_without_token() {
        // Save and remove the token
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();
        env::remove_var("GRU_GITHUB_TOKEN");

        // Try to get client - will succeed if gh CLI is authenticated, fail otherwise
        let result = GitHubClient::from_env("test-owner", "test-repo").await;

        // If gh CLI is authenticated, the result will succeed
        // If gh CLI is not authenticated, the result should fail with a helpful error
        if result.is_err() {
            let err_msg = result.unwrap_err().to_string();
            assert!(err_msg.contains("No GitHub authentication found") || err_msg.contains("auth"));
        }
        // If result.is_ok(), that's also fine - it means gh CLI provided a token

        // Restore original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        }
    }

    // --- infer_github_host tests ---

    #[test]
    fn test_infer_github_host_netflix_org() {
        let host = infer_github_host("netflix");
        assert_ne!(
            host, "github.com",
            "netflix owner should resolve to a GHE host"
        );
    }

    #[test]
    fn test_infer_github_host_netflix_substring() {
        let host = infer_github_host("netflix-oss");
        assert_ne!(
            host, "github.com",
            "netflix-oss owner should resolve to a GHE host"
        );
    }

    #[test]
    fn test_infer_github_host_public_owner() {
        assert_eq!(infer_github_host("octocat"), "github.com");
    }

    #[test]
    fn test_infer_github_host_empty() {
        assert_eq!(infer_github_host(""), "github.com");
    }

    // --- gh_command_for_repo tests ---

    #[test]
    fn test_gh_command_for_repo_netflix() {
        assert_eq!(gh_command_for_repo("netflix/some-repo"), "ghe");
    }

    #[test]
    fn test_gh_command_for_repo_public() {
        assert_eq!(gh_command_for_repo("octocat/hello-world"), "gh");
    }

    #[test]
    fn test_gh_command_for_repo_no_slash() {
        // Edge case: no owner/repo separator
        assert_eq!(gh_command_for_repo("just-a-string"), "gh");
    }

    #[test]
    fn test_gh_command_for_repo_empty() {
        assert_eq!(gh_command_for_repo(""), "gh");
    }

    // --- gh_command_for_host tests ---

    #[test]
    fn test_gh_command_for_host_github_com() {
        assert_eq!(gh_command_for_host("github.com"), "gh");
    }

    #[test]
    fn test_gh_command_for_host_ghe() {
        assert_eq!(gh_command_for_host("ghe.netflix.net"), "ghe");
    }

    #[test]
    fn test_gh_command_for_host_custom() {
        assert_eq!(gh_command_for_host("git.example.com"), "ghe");
    }

    // --- IssueInfo deserialization tests ---

    #[test]
    fn test_issue_info_deserialize_full() {
        let json = r#"{"number": 42, "title": "Fix the bug", "body": "Details here"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.title, "Fix the bug");
        assert_eq!(info.body.as_deref(), Some("Details here"));
    }

    #[test]
    fn test_issue_info_deserialize_null_body() {
        let json = r#"{"number": 1, "title": "No body", "body": null}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.number, 1);
        assert_eq!(info.title, "No body");
        assert!(info.body.is_none());
    }

    #[test]
    fn test_issue_info_deserialize_missing_body() {
        // serde treats a missing Option<T> field as None by default
        let json = r#"{"number": 5, "title": "Minimal"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.number, 5);
        assert!(info.body.is_none());
    }

    #[test]
    fn test_issue_info_deserialize_extra_fields() {
        let json = r#"{"number": 10, "title": "Has extras", "body": "body", "labels": [], "url": "https://example.com"}"#;
        let info: IssueInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.number, 10);
        assert_eq!(info.title, "Has extras");
    }

    #[test]
    fn test_issue_info_deserialize_missing_required_field() {
        let json = r#"{"title": "No number"}"#;
        let result: Result<IssueInfo, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // Integration tests that require a real GitHub token
    // Run with: cargo test github_client -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_get_issue() {
        let client = GitHubClient::from_env("octocat", "Hello-World")
            .await
            .expect("Failed to create client");

        // Test against a known public issue
        let issue = client
            .get_issue("octocat", "Hello-World", 1)
            .await
            .expect("Failed to fetch issue");

        assert_eq!(issue.number, 1);
    }

    #[tokio::test]
    #[ignore]
    async fn test_post_comment() {
        let client = GitHubClient::from_env("your-username", "your-test-repo")
            .await
            .expect("Failed to create client");

        // This test requires write access to a repository
        // You should replace these with your own test repo details
        let comment = client
            .post_comment(
                "your-username",
                "your-test-repo",
                1,
                "Test comment from Gru GitHub client",
            )
            .await
            .expect("Failed to post comment");

        assert!(!comment.body.unwrap_or_default().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_add_and_remove_label() {
        let owner = "your-username";
        let repo = "your-test-repo";
        let client = GitHubClient::from_env(owner, repo)
            .await
            .expect("Failed to create client");

        // This test requires write access to a repository
        let issue = 1;
        let label = "test-label";

        // Add label
        let labels = client
            .add_label(owner, repo, issue, label)
            .await
            .expect("Failed to add label");

        assert!(labels.iter().any(|l| l.name == label));

        // Remove label
        client
            .remove_label(owner, repo, issue, label)
            .await
            .expect("Failed to remove label");
    }

    // --- IssueNumber deserialization tests ---

    #[test]
    fn test_issue_number_deserialize() {
        let json = r#"[{"number": 1}, {"number": 42}, {"number": 100}]"#;
        let items: Vec<IssueNumber> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].number, 1);
        assert_eq!(items[1].number, 42);
        assert_eq!(items[2].number, 100);
    }

    #[test]
    fn test_issue_number_deserialize_empty() {
        let json = "[]";
        let items: Vec<IssueNumber> = serde_json::from_str(json).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_issue_number_deserialize_extra_fields() {
        let json = r#"[{"number": 5, "title": "ignored", "url": "https://example.com"}]"#;
        let items: Vec<IssueNumber> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 5);
    }

    #[test]
    fn test_issue_number_deserialize_missing_number() {
        let json = r#"[{"title": "no number"}]"#;
        let result: Result<Vec<IssueNumber>, _> = serde_json::from_str(json);
        assert!(result.is_err());
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
        // Should not set GH_HOST for github.com
        let has_gh_host = cmd.as_std().get_envs().any(|(k, _)| k == "GH_HOST");
        assert!(!has_gh_host);
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
