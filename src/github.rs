use anyhow::{anyhow, Context, Result};
use octocrab::{models, Octocrab};
use std::env;
use tokio::process::Command;

// ============================================================================
// Token Extraction Helpers
// ============================================================================

/// Try to extract GitHub token from gh/ghe CLI
///
/// # Arguments
/// * `owner` - Repository owner (used to infer hostname)
/// * `repo` - Repository name (currently unused, but available for future enhancements)
///
/// Returns the token if successfully extracted, or None if gh/ghe is not available
async fn try_get_token_from_cli(owner: &str, _repo: &str) -> Option<String> {
    // Infer which CLI to use based on hostname
    let host = infer_github_host(owner);
    let gh_cmd = if host.contains("ghe.") { "ghe" } else { "gh" };

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

/// Infer GitHub hostname from repository owner
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
///
/// Returns the appropriate GitHub hostname (github.com or ghe.netflix.net)
fn infer_github_host(owner: &str) -> &'static str {
    // Future enhancement: Parse from git remote URL
    // For now, use simple heuristic
    if owner == "netflix" || owner.contains("netflix") {
        "ghe.netflix.net"
    } else {
        "github.com"
    }
}

/// Determine the correct `gh` CLI command for a repository.
///
/// Returns `"ghe"` for Netflix repos, `"gh"` otherwise.
///
/// # Arguments
/// * `repo` - Repository identifier in "owner/repo" format
pub fn gh_command_for_repo(repo: &str) -> &'static str {
    let owner = repo.split('/').next().unwrap_or("");
    let host = infer_github_host(owner);
    if host.contains("ghe.") {
        "ghe"
    } else {
        "gh"
    }
}

/// Get GitHub token with automatic fallback logic
///
/// Priority order:
/// 1. Try gh/ghe CLI (respects existing authentication)
/// 2. Fall back to GRU_GITHUB_TOKEN environment variable
/// 3. Return error with helpful message
///
/// # Arguments
/// * `owner` - Repository owner (used to infer hostname)
/// * `repo` - Repository name
async fn get_github_token(owner: &str, repo: &str) -> Result<String> {
    // Try CLI first
    if let Some(token) = try_get_token_from_cli(owner, repo).await {
        return Ok(token);
    }

    // Fall back to environment variable
    if let Ok(token) = env::var("GRU_GITHUB_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }

    // Provide helpful error message
    let host = infer_github_host(owner);
    let gh_cmd = if host.contains("ghe.") { "ghe" } else { "gh" };

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
    /// Initialize a new GitHub client with the provided token
    ///
    /// # Arguments
    /// * `token` - GitHub personal access token
    ///
    /// Returns an error if the token is empty or invalid.
    pub fn new(token: String) -> Result<Self> {
        if token.is_empty() {
            return Err(anyhow!("GitHub token is empty"));
        }

        let client = Octocrab::builder()
            .personal_token(token)
            .build()
            .context("Failed to build GitHub client")?;

        Ok(Self { client })
    }

    /// Initialize a new GitHub client with token from environment or gh/ghe CLI
    ///
    /// Priority order:
    /// 1. Try gh/ghe CLI (respects existing authentication)
    /// 2. Fall back to GRU_GITHUB_TOKEN environment variable
    ///
    /// # Arguments
    /// * `owner` - Repository owner (used to infer hostname)
    /// * `repo` - Repository name
    ///
    /// Returns an error if no authentication is found.
    pub async fn from_env(owner: &str, repo: &str) -> Result<Self> {
        let token = get_github_token(owner, repo).await?;
        Self::new(token)
    }

    /// Try to initialize a new GitHub client with token from environment or gh/ghe CLI
    ///
    /// Returns `None` if no authentication is found, instead of an error.
    /// This allows graceful fallback to CLI methods.
    ///
    /// # Arguments
    /// * `owner` - Repository owner (used to infer hostname)
    /// * `repo` - Repository name
    pub async fn try_from_env(owner: &str, repo: &str) -> Option<Self> {
        let token = get_github_token(owner, repo).await.ok()?;
        Self::new(token).ok()
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

    /// Claim an issue by transitioning from ready-for-minion to in-progress
    ///
    /// This operation is designed for fire-and-forget usage. While it returns a Result,
    /// callers typically log errors but don't block the main workflow.
    ///
    /// # Race Conditions
    /// This method attempts to detect if another Minion already claimed the issue
    /// by checking for the `in-progress` label. However, there is a TOCTOU window
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

        // If already has in-progress, another Minion claimed it
        if current_labels.iter().any(|l| l == "in-progress") {
            return Ok(false);
        }

        // Remove ready-for-minion if present (ignore errors - label may not exist)
        let _ = self
            .remove_label(owner, repo, issue, "ready-for-minion")
            .await;

        // Add in-progress label
        self.add_label(owner, repo, issue, "in-progress").await?;

        Ok(true)
    }

    /// Mark an issue as completed by transitioning from in-progress to minion:done
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    pub async fn mark_issue_done(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove in-progress label (ignore errors - may not exist)
        let _ = self.remove_label(owner, repo, issue, "in-progress").await;

        // Add minion:done label
        self.add_label(owner, repo, issue, "minion:done").await?;

        Ok(())
    }

    /// Mark an issue as failed by transitioning from in-progress to minion:failed
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    pub async fn mark_issue_failed(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove in-progress label (ignore errors - may not exist)
        let _ = self.remove_label(owner, repo, issue, "in-progress").await;

        // Add minion:failed label
        self.add_label(owner, repo, issue, "minion:failed").await?;

        Ok(())
    }

    /// Mark an issue as blocked by transitioning to minion:blocked
    ///
    /// Used when a minion is stuck (inactivity timeout), the task times out,
    /// or CI fix attempts are exhausted. Signals that human intervention is needed.
    ///
    /// Removes `in-progress`, `minion:done`, and `minion:failed` labels if present
    /// to ensure a clean state transition regardless of which phase triggered the block.
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    pub async fn mark_issue_blocked(&self, owner: &str, repo: &str, issue: u64) -> Result<()> {
        // Remove any existing state labels (ignore errors - may not exist)
        let _ = self.remove_label(owner, repo, issue, "in-progress").await;
        let _ = self.remove_label(owner, repo, issue, "minion:done").await;
        let _ = self.remove_label(owner, repo, issue, "minion:failed").await;

        // Add minion:blocked label
        self.add_label(owner, repo, issue, "minion:blocked").await?;

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
pub async fn mark_pr_ready_via_cli(owner: &str, repo: &str, pr_number: &str) -> Result<()> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let output = Command::new(gh_cmd)
        .args([
            "pr",
            "ready",
            pr_number,
            "--repo",
            &format!("{}/{}", owner, repo),
        ])
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

/// Fetch issue details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - Issue number
pub async fn get_issue_via_cli(owner: &str, repo: &str, number: u64) -> Result<IssueInfo> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let output = Command::new(gh_cmd)
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
            "--json",
            "number,title,body",
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
    #[allow(dead_code)] // Included for serde completeness; callers use .title and .body
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
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
pub async fn get_pr_via_cli(owner: &str, repo: &str, number: u64) -> Result<PrInfo> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let output = Command::new(gh_cmd)
        .args([
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
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
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = gh_command_for_repo(&repo_full);
    let output = Command::new(gh_cmd)
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

    // Validate URL format (gh returns URL like https://github.com/owner/repo/pull/123)
    // Only accept HTTPS URLs for security
    if !pr_url.starts_with("https://github.com/") {
        return Err(anyhow!("Expected GitHub HTTPS URL, got: {}", pr_url));
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
    // Expected format: https://github.com/owner/repo/pull/123
    let segments: Vec<&str> = url_path.split('/').collect();

    // segments should be: ["https:", "", "github.com", "owner", "repo", "pull", "123"]
    if segments.len() < 7 || segments[5] != "pull" {
        return Err(anyhow!(
            "Unexpected GitHub PR URL format: {}. Expected: https://github.com/owner/repo/pull/NUMBER",
            pr_url
        ));
    }

    let pr_number = segments[6];

    // Validate it's actually a number
    pr_number
        .parse::<u64>()
        .context(format!("PR number '{}' is not a valid integer", pr_number))?;

    Ok(pr_number.to_string())
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
        let result = GitHubClient::new(String::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_new_with_valid_token() {
        // Should succeed with valid token format
        let result = GitHubClient::new("ghp_test123".to_string());
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
        assert_eq!(infer_github_host("netflix"), "ghe.netflix.net");
    }

    #[test]
    fn test_infer_github_host_netflix_substring() {
        assert_eq!(infer_github_host("netflix-oss"), "ghe.netflix.net");
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
}
