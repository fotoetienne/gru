use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use octocrab::{models, Octocrab};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::process::Command;

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
    /// Initialize a new GitHub client with token from environment
    ///
    /// Reads `GRU_GITHUB_TOKEN` from environment variables.
    /// Returns an error if the token is missing or invalid.
    pub fn new() -> Result<Self> {
        let token = env::var("GRU_GITHUB_TOKEN")
            .context("GRU_GITHUB_TOKEN environment variable not set")?;

        if token.is_empty() {
            return Err(anyhow!("GRU_GITHUB_TOKEN is empty"));
        }

        let client = Octocrab::builder()
            .personal_token(token)
            .build()
            .context("Failed to build GitHub client")?;

        Ok(Self { client })
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
}

// ============================================================================
// gh CLI Helper Functions
// ============================================================================

/// Represents a GitHub issue with essential metadata
#[derive(Debug, Serialize, Deserialize)]
pub struct Issue {
    pub number: u32,
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<Label>,
}

/// Represents a GitHub label
#[derive(Debug, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
}

/// Validates that owner/repo names are safe to use in commands
fn validate_github_identifier(s: &str, field_name: &str) -> Result<()> {
    if s.is_empty() {
        anyhow::bail!("{} cannot be empty", field_name);
    }

    // GitHub usernames and repo names can only contain alphanumeric, hyphens, underscores, and dots
    if !s
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        anyhow::bail!(
            "{} contains invalid characters (only alphanumeric, hyphens, underscores, and dots allowed)",
            field_name
        );
    }

    Ok(())
}

/// Validates that an issue number is numeric only
fn validate_issue_number(issue_num: &str) -> Result<()> {
    if issue_num.is_empty() {
        anyhow::bail!("Issue number cannot be empty");
    }

    if !issue_num.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("Issue number must contain only digits");
    }

    Ok(())
}

/// Fetches issue details from GitHub using the gh CLI
pub async fn fetch_issue(owner: &str, repo: &str, issue_num: &str) -> Result<Issue> {
    // Validate inputs to prevent command injection
    validate_github_identifier(owner, "owner")?;
    validate_github_identifier(repo, "repo")?;
    validate_issue_number(issue_num)?;

    let output = Command::new("gh")
        .args([
            "issue",
            "view",
            issue_num,
            "--repo",
            &format!("{}/{}", owner, repo),
            "--json",
            "number,title,body,labels",
        ])
        .output()
        .await
        .context("Failed to execute gh command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Parse common error scenarios for better user guidance
        if stderr.contains("not found") || stderr.contains("404") {
            anyhow::bail!(
                "Issue #{} not found in {}/{}. Please verify the issue number and repository.",
                issue_num,
                owner,
                repo
            );
        } else if stderr.contains("authentication")
            || stderr.contains("401")
            || stderr.contains("403")
        {
            anyhow::bail!(
                "GitHub authentication failed. Please run 'gh auth login' or set GRU_GITHUB_TOKEN.\nError: {}",
                stderr
            );
        } else {
            anyhow::bail!(
                "Failed to fetch issue #{} from {}/{}: {}",
                issue_num,
                owner,
                repo,
                stderr
            );
        }
    }

    let issue: Issue = serde_json::from_slice(&output.stdout)
        .context("Failed to parse issue JSON from gh output")?;

    Ok(issue)
}

/// Checks if an issue has the "in-progress" label
pub fn has_in_progress_label(issue: &Issue) -> bool {
    issue.labels.iter().any(|label| label.name == "in-progress")
}

/// Adds the "in-progress" label to an issue
pub async fn add_in_progress_label(owner: &str, repo: &str, issue_num: &str) -> Result<()> {
    // Validate inputs to prevent command injection
    validate_github_identifier(owner, "owner")?;
    validate_github_identifier(repo, "repo")?;
    validate_issue_number(issue_num)?;

    let output = Command::new("gh")
        .args([
            "issue",
            "edit",
            issue_num,
            "--repo",
            &format!("{}/{}", owner, repo),
            "--add-label",
            "in-progress",
        ])
        .output()
        .await
        .context("Failed to execute gh command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Parse common error scenarios for better user guidance
        if stderr.contains("authentication") || stderr.contains("401") || stderr.contains("403") {
            anyhow::bail!(
                "GitHub authentication failed. Please run 'gh auth login' or set GRU_GITHUB_TOKEN.\nError: {}",
                stderr
            );
        } else {
            anyhow::bail!(
                "Failed to add in-progress label to issue #{} in {}/{}: {}",
                issue_num,
                owner,
                repo,
                stderr
            );
        }
    }

    Ok(())
}

/// Posts a claim comment on an issue with YAML frontmatter
pub async fn post_claim_comment(
    owner: &str,
    repo: &str,
    issue_num: &str,
    minion_id: &str,
    branch: &str,
    workspace_path: &str,
) -> Result<()> {
    // Validate inputs to prevent command injection
    validate_github_identifier(owner, "owner")?;
    validate_github_identifier(repo, "repo")?;
    validate_issue_number(issue_num)?;

    let timestamp = Utc::now().to_rfc3339();

    let comment = format!(
        r#"🤖 **Minion {} claimed this issue**

---
event: minion:claim
minion_id: {}
branch: {}
timestamp: {}
---

Starting work on this issue. I'll create a branch and begin implementation.

Workspace: `{}`
"#,
        minion_id, minion_id, branch, timestamp, workspace_path
    );

    let output = Command::new("gh")
        .args([
            "issue",
            "comment",
            issue_num,
            "--repo",
            &format!("{}/{}", owner, repo),
            "--body",
            &comment,
        ])
        .output()
        .await
        .context("Failed to execute gh command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Parse common error scenarios for better user guidance
        if stderr.contains("authentication") || stderr.contains("401") || stderr.contains("403") {
            anyhow::bail!(
                "GitHub authentication failed. Please run 'gh auth login' or set GRU_GITHUB_TOKEN.\nError: {}",
                stderr
            );
        } else {
            anyhow::bail!(
                "Failed to post claim comment on issue #{} in {}/{}: {}",
                issue_num,
                owner,
                repo,
                stderr
            );
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Octocrab API Client Tests
    #[test]
    fn test_new_without_token() {
        // Save original token if present
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();

        // Remove token
        env::remove_var("GRU_GITHUB_TOKEN");

        // Should fail with missing token
        let result = GitHubClient::new();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GRU_GITHUB_TOKEN"));

        // Restore original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        }
    }

    #[test]
    fn test_new_with_empty_token() {
        // Save original token if present
        let original_token = env::var("GRU_GITHUB_TOKEN").ok();

        // Set empty token
        env::set_var("GRU_GITHUB_TOKEN", "");

        // Should fail with empty token
        let result = GitHubClient::new();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        // Restore original token if it existed
        if let Some(token) = original_token {
            env::set_var("GRU_GITHUB_TOKEN", token);
        } else {
            env::remove_var("GRU_GITHUB_TOKEN");
        }
    }

    // Integration tests that require a real GitHub token
    // Run with: cargo test github_client -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_get_issue() {
        let client = GitHubClient::new().expect("Failed to create client");

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
        let client = GitHubClient::new().expect("Failed to create client");

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
        let client = GitHubClient::new().expect("Failed to create client");

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

    // gh CLI Helper Function Tests
    #[test]
    fn test_has_in_progress_label_when_present() {
        let issue = Issue {
            number: 123,
            title: "Test Issue".to_string(),
            body: Some("Test body".to_string()),
            labels: vec![
                Label {
                    name: "bug".to_string(),
                },
                Label {
                    name: "in-progress".to_string(),
                },
            ],
        };

        assert!(has_in_progress_label(&issue));
    }

    #[test]
    fn test_has_in_progress_label_when_absent() {
        let issue = Issue {
            number: 123,
            title: "Test Issue".to_string(),
            body: Some("Test body".to_string()),
            labels: vec![Label {
                name: "bug".to_string(),
            }],
        };

        assert!(!has_in_progress_label(&issue));
    }

    #[test]
    fn test_has_in_progress_label_empty_labels() {
        let issue = Issue {
            number: 123,
            title: "Test Issue".to_string(),
            body: Some("Test body".to_string()),
            labels: vec![],
        };

        assert!(!has_in_progress_label(&issue));
    }

    #[test]
    fn test_validate_github_identifier_accepts_valid_names() {
        assert!(validate_github_identifier("fotoetienne", "owner").is_ok());
        assert!(validate_github_identifier("gru", "repo").is_ok());
        assert!(validate_github_identifier("my-repo", "repo").is_ok());
        assert!(validate_github_identifier("my_repo", "repo").is_ok());
        assert!(validate_github_identifier("repo.name", "repo").is_ok());
        assert!(validate_github_identifier("user-123", "owner").is_ok());
    }

    #[test]
    fn test_validate_github_identifier_rejects_invalid_names() {
        assert!(validate_github_identifier("", "owner").is_err());
        assert!(validate_github_identifier("evil; rm -rf /", "owner").is_err());
        assert!(validate_github_identifier("repo name", "repo").is_err());
        assert!(validate_github_identifier("repo|name", "repo").is_err());
        assert!(validate_github_identifier("repo&name", "repo").is_err());
        assert!(validate_github_identifier("repo$name", "repo").is_err());
    }

    #[test]
    fn test_validate_issue_number_accepts_valid_numbers() {
        assert!(validate_issue_number("1").is_ok());
        assert!(validate_issue_number("42").is_ok());
        assert!(validate_issue_number("123456").is_ok());
    }

    #[test]
    fn test_validate_issue_number_rejects_invalid_numbers() {
        assert!(validate_issue_number("").is_err());
        assert!(validate_issue_number("not-a-number").is_err());
        assert!(validate_issue_number("123; rm -rf /").is_err());
        assert!(validate_issue_number("-42").is_err());
        assert!(validate_issue_number("12.34").is_err());
    }
}
