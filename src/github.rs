use anyhow::{anyhow, Context, Result};
use octocrab::{models, Octocrab};
use std::env;

// ============================================================================
// Octocrab API Client
// ============================================================================

/// GitHub API client wrapper using octocrab
#[derive(Debug)]
#[allow(dead_code)]
pub struct GitHubClient {
    client: Octocrab,
}

#[allow(dead_code)]
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

    /// Initialize a new GitHub client with token from environment
    ///
    /// Reads `GRU_GITHUB_TOKEN` from environment variables.
    /// Returns an error if the token is missing or invalid.
    pub fn from_env() -> Result<Self> {
        let token = env::var("GRU_GITHUB_TOKEN")
            .context("GRU_GITHUB_TOKEN environment variable not set")?;

        Self::new(token)
    }

    /// Try to initialize a new GitHub client with token from environment
    ///
    /// Returns `None` if the token is not set, instead of an error.
    /// This allows graceful fallback to CLI methods.
    pub fn try_from_env() -> Option<Self> {
        let token = env::var("GRU_GITHUB_TOKEN").ok()?;
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
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `issue` - Issue number
    ///
    /// Returns `Ok(true)` if the issue was successfully claimed, `Ok(false)` if
    /// another Minion already claimed it (race condition), or `Err` on failure.
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
    ///
    /// Note: This method uses gh CLI (not the GitHub API) because:
    /// - The gh CLI handles authentication edge cases better
    /// - CLI provides better error messages for PR creation failures
    /// - Consistent with pre-existing implementation pattern
    pub async fn create_draft_pr(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        // Delegate to CLI implementation (same behavior whether API token is set or not)
        create_draft_pr_via_cli(owner, repo, branch, base, title, body).await
    }

    /// Update the body/description of an existing pull request
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `pr_number` - PR number
    /// * `body` - New PR description body (markdown supported)
    pub async fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        pr_number: &str,
        body: &str,
    ) -> Result<()> {
        use tokio::process::Command;

        let output = Command::new("gh")
            .args([
                "pr",
                "edit",
                pr_number,
                "--repo",
                &format!("{}/{}", owner, repo),
                "--body",
                body,
            ])
            .output()
            .await
            .context("Failed to execute gh pr edit command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to update PR #{} in {}/{}: {}",
                pr_number,
                owner,
                repo,
                stderr
            ));
        }

        Ok(())
    }

    /// Mark a draft PR as ready for review
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `pr_number` - PR number
    pub async fn mark_pr_ready(&self, owner: &str, repo: &str, pr_number: &str) -> Result<()> {
        use tokio::process::Command;

        let output = Command::new("gh")
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
}

// ============================================================================
// gh CLI Helper Functions
// ============================================================================
// Note: gh CLI functions for issue claiming have been removed in favor of
// simpler auto-detection from current directory. They may be re-added in
// future phases when implementing multi-Lab coordination.

/// Fetch issue details using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner (user or organization)
/// * `repo` - Repository name
/// * `number` - Issue number
pub async fn get_issue_via_cli(owner: &str, repo: &str, number: u64) -> Result<IssueInfo> {
    use tokio::process::Command;

    let output = Command::new("gh")
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

/// Post a comment on an issue using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `issue` - Issue number
/// * `body` - Comment body (markdown supported)
#[allow(dead_code)] // Part of public API for CLI fallback, will be used in future phases
pub async fn post_comment_via_cli(owner: &str, repo: &str, issue: u64, body: &str) -> Result<()> {
    use tokio::process::Command;

    let output = Command::new("gh")
        .args([
            "issue",
            "comment",
            &issue.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
            "--body",
            body,
        ])
        .output()
        .await
        .context("Failed to execute gh issue comment command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to post comment on issue #{} in {}/{}: {}",
            issue,
            owner,
            repo,
            stderr
        ));
    }

    Ok(())
}

/// Add a label to an issue using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `issue` - Issue number
/// * `label` - Label name to add
#[allow(dead_code)] // Part of public API for CLI fallback, will be used in future phases
pub async fn add_label_via_cli(owner: &str, repo: &str, issue: u64, label: &str) -> Result<()> {
    use tokio::process::Command;

    let output = Command::new("gh")
        .args([
            "issue",
            "edit",
            &issue.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
            "--add-label",
            label,
        ])
        .output()
        .await
        .context("Failed to execute gh issue edit command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to add label '{}' to issue #{} in {}/{}: {}",
            label,
            issue,
            owner,
            repo,
            stderr
        ));
    }

    Ok(())
}

/// Remove a label from an issue using gh CLI
///
/// # Arguments
/// * `owner` - Repository owner
/// * `repo` - Repository name
/// * `issue` - Issue number
/// * `label` - Label name to remove
#[allow(dead_code)] // Part of public API for CLI fallback, will be used in future phases
pub async fn remove_label_via_cli(owner: &str, repo: &str, issue: u64, label: &str) -> Result<()> {
    use tokio::process::Command;

    let output = Command::new("gh")
        .args([
            "issue",
            "edit",
            &issue.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
            "--remove-label",
            label,
        ])
        .output()
        .await
        .context("Failed to execute gh issue edit command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to remove label '{}' from issue #{} in {}/{}: {}",
            label,
            issue,
            owner,
            repo,
            stderr
        ));
    }

    Ok(())
}

/// Simple struct to hold issue information from gh CLI
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // Part of public API, fields will be used in future phases
pub struct IssueInfo {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
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
    use tokio::process::Command;

    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--repo",
            &format!("{}/{}", owner, repo),
            "--head",
            branch,
            "--base",
            base,
            "--title",
            title,
            "--body",
            body,
            "--draft",
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

    #[test]
    #[serial]
    fn test_from_env_without_token() {
        // Save and remove the token
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();
        env::remove_var("GRU_GITHUB_TOKEN");

        // Should fail with missing token
        let result = GitHubClient::from_env();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GRU_GITHUB_TOKEN"));

        // Restore original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        }
    }

    // Integration tests that require a real GitHub token
    // Run with: cargo test github_client -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_get_issue() {
        let client = GitHubClient::from_env().expect("Failed to create client");

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
        let client = GitHubClient::from_env().expect("Failed to create client");

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
        let client = GitHubClient::from_env().expect("Failed to create client");

        // This test requires write access to a repository
        let owner = "your-username";
        let repo = "your-test-repo";
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
