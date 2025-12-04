use anyhow::{anyhow, Context, Result};
use octocrab::{models, Octocrab};
use std::env;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
