use crate::git;
use anyhow::{Context, Result};
use tokio::process::Command;

/// The type of resource referenced in a GitHub URL
#[derive(Debug, Clone, PartialEq)]
pub enum GitHubResourceType {
    Issue,
    Pull,
}

/// Parsed components of a GitHub URL
#[derive(Debug, Clone)]
pub struct GitHubUrl {
    pub owner: String,
    pub repo: String,
    pub resource_type: GitHubResourceType,
    pub number: u32,
}

/// Cleans a URL by stripping query parameters, fragments, and trailing slashes.
fn clean_url(url: &str) -> &str {
    url.split('?')
        .next()
        .unwrap()
        .split('#')
        .next()
        .unwrap()
        .trim_end_matches('/')
}

/// Parses a GitHub issue or PR URL into its components.
///
/// Handles URLs like:
/// - `https://github.com/owner/repo/issues/42`
/// - `https://github.com/owner/repo/pull/42`
/// - URLs with query params, fragments, and trailing slashes
///
/// Returns `None` if the URL is not a valid GitHub issue/PR URL.
pub fn parse_github_url(url: &str) -> Option<GitHubUrl> {
    if !url.starts_with("https://github.com/") {
        return None;
    }

    let cleaned = clean_url(url);
    let path = cleaned.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = path.split('/').collect();

    if parts.len() != 4 {
        return None;
    }

    let owner = parts[0];
    let repo = parts[1];
    let resource_type_str = parts[2];
    let number_str = parts[3];

    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let resource_type = match resource_type_str {
        "issues" => GitHubResourceType::Issue,
        "pull" => GitHubResourceType::Pull,
        _ => return None,
    };

    let number = number_str.parse::<u32>().ok()?;

    Some(GitHubUrl {
        owner: owner.to_string(),
        repo: repo.to_string(),
        resource_type,
        number,
    })
}

/// Extracts owner, repo, and issue number from an issue argument.
///
/// Supports both plain issue numbers (auto-detects from current directory) and GitHub URLs.
/// Validates the input format as part of parsing (no separate validation step needed).
pub async fn parse_issue_info(issue: &str) -> Result<(String, String, String)> {
    // Check if it's a plain number
    if let Ok(num) = issue.parse::<u32>() {
        // Auto-detect repository from current directory
        git::detect_git_repo()
            .await
            .context("Failed to detect git repository")?;

        let remote_url = git::get_github_remote()
            .await
            .context("Failed to get GitHub remote")?;

        let (owner, repo) =
            git::parse_github_remote(&remote_url).context("Failed to parse GitHub remote URL")?;

        return Ok((owner, repo, num.to_string()));
    }

    // Try parsing as a GitHub URL
    if let Some(parsed) = parse_github_url(issue) {
        if parsed.resource_type == GitHubResourceType::Issue {
            return Ok((parsed.owner, parsed.repo, parsed.number.to_string()));
        }
    }

    anyhow::bail!(
        "Invalid issue format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru fix 42\n\
         - gru fix https://github.com/owner/repo/issues/42"
    );
}

/// Extracts owner, repo, PR number, and branch name from a PR argument.
///
/// Supports both plain PR numbers and GitHub URLs.
/// For plain numbers, fetches metadata from GitHub to get branch info.
/// Validates the input format as part of parsing (no separate validation step needed).
pub async fn parse_pr_info(pr: &str) -> Result<(String, String, String, String)> {
    // Extract PR number, validating the format along the way
    let pr_num = if pr.parse::<u32>().is_ok() {
        pr.to_string()
    } else if let Some(parsed) = parse_github_url(pr) {
        if parsed.resource_type != GitHubResourceType::Pull {
            anyhow::bail!(
                "Invalid PR format. Expected: <number> or <github-url>\n\
                 Examples:\n\
                 - gru review 42\n\
                 - gru review https://github.com/owner/repo/pull/42"
            );
        }
        parsed.number.to_string()
    } else {
        anyhow::bail!(
            "Invalid PR format. Expected: <number> or <github-url>\n\
             Examples:\n\
             - gru review 42\n\
             - gru review https://github.com/owner/repo/pull/42"
        );
    };

    // Fetch PR metadata from GitHub to get branch and repo info
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_num,
            "--json",
            "headRefName,headRepository,headRepositoryOwner",
        ])
        .output()
        .await
        .context("Failed to execute gh pr view")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR metadata: {}", stderr);
    }

    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .context("Failed to parse PR metadata JSON")?;

    let branch = json["headRefName"]
        .as_str()
        .context("Missing branch name in PR metadata")?
        .to_string();
    let repo = json["headRepository"]["name"]
        .as_str()
        .context("Missing repo name in PR metadata")?
        .to_string();
    let owner = json["headRepositoryOwner"]["login"]
        .as_str()
        .context("Missing owner in PR metadata")?
        .to_string();

    Ok((owner, repo, pr_num, branch))
}

/// Normalizes a Minion ID by adding the 'M' prefix if missing
/// Validates that the ID contains only alphanumeric characters to prevent path traversal
#[allow(dead_code)]
pub fn normalize_minion_id(id: &str) -> Result<String> {
    // Validate against path traversal and invalid characters
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        anyhow::bail!(
            "Invalid Minion ID '{}': contains path separators or parent directory references",
            id
        );
    }

    if id.contains('\0') {
        anyhow::bail!("Invalid Minion ID '{}': contains null bytes", id);
    }

    let normalized = if id.starts_with('M') {
        id.to_string()
    } else {
        format!("M{}", id)
    };

    // Additional validation: ensure only alphanumeric characters
    if !normalized.chars().all(|c| c.is_alphanumeric()) {
        anyhow::bail!(
            "Invalid Minion ID '{}': must contain only alphanumeric characters",
            id
        );
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_github_url tests ---

    #[test]
    fn test_parse_github_url_issue() {
        let result = parse_github_url("https://github.com/fotoetienne/gru/issues/42").unwrap();
        assert_eq!(result.owner, "fotoetienne");
        assert_eq!(result.repo, "gru");
        assert_eq!(result.resource_type, GitHubResourceType::Issue);
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_pull() {
        let result = parse_github_url("https://github.com/owner/repo-name/pull/123").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo-name");
        assert_eq!(result.resource_type, GitHubResourceType::Pull);
        assert_eq!(result.number, 123);
    }

    #[test]
    fn test_parse_github_url_strips_query_params() {
        let result = parse_github_url("https://github.com/owner/repo/issues/42?foo=bar").unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_strips_fragments() {
        let result =
            parse_github_url("https://github.com/owner/repo/issues/42#comment-123").unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_strips_trailing_slash() {
        let result = parse_github_url("https://github.com/owner/repo/pull/42/").unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_combined_edge_cases() {
        let result =
            parse_github_url("https://github.com/owner/repo/issues/42/?foo=bar#comment").unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_rejects_non_github() {
        assert!(parse_github_url("https://example.com/issues/42").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_empty_owner() {
        assert!(parse_github_url("https://github.com//repo/issues/42").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_empty_repo() {
        assert!(parse_github_url("https://github.com/owner//issues/42").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_missing_number() {
        assert!(parse_github_url("https://github.com/owner/repo/issues/").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_non_numeric_number() {
        assert!(parse_github_url("https://github.com/owner/repo/issues/abc").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_unknown_resource_type() {
        assert!(parse_github_url("https://github.com/owner/repo/wiki/42").is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_incomplete_path() {
        assert!(parse_github_url("https://github.com/owner/repo").is_none());
        assert!(parse_github_url("https://github.com/owner").is_none());
        assert!(parse_github_url("https://github.com/issues/").is_none());
    }

    // --- parse_issue_info tests (URL paths only; plain numbers need git context) ---

    #[tokio::test]
    async fn test_parse_issue_info_with_url() {
        let result = parse_issue_info("https://github.com/fotoetienne/gru/issues/42")
            .await
            .unwrap();
        assert_eq!(result.0, "fotoetienne");
        assert_eq!(result.1, "gru");
        assert_eq!(result.2, "42");
    }

    #[tokio::test]
    async fn test_parse_issue_info_with_url_and_query_params() {
        let result = parse_issue_info("https://github.com/owner/repo/issues/123?foo=bar")
            .await
            .unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
        assert_eq!(result.2, "123");
    }

    #[tokio::test]
    async fn test_parse_issue_info_rejects_invalid() {
        assert!(parse_issue_info("not-a-number").await.is_err());
        assert!(parse_issue_info("").await.is_err());
        assert!(parse_issue_info("-42").await.is_err());
        // PR URL should not parse as issue
        assert!(parse_issue_info("https://github.com/owner/repo/pull/42")
            .await
            .is_err());
    }

    // --- parse_pr_info validation tests (only format validation; gh calls need network) ---

    #[tokio::test]
    async fn test_parse_pr_info_rejects_invalid() {
        assert!(parse_pr_info("not-a-number").await.is_err());
        assert!(parse_pr_info("").await.is_err());
        assert!(parse_pr_info("-42").await.is_err());
        // Issue URL should not parse as PR
        assert!(parse_pr_info("https://github.com/owner/repo/issues/42")
            .await
            .is_err());
    }

    // --- normalize_minion_id tests ---

    #[test]
    fn test_normalize_minion_id_with_prefix() {
        assert_eq!(normalize_minion_id("M42").unwrap(), "M42");
        assert_eq!(normalize_minion_id("M001").unwrap(), "M001");
        assert_eq!(normalize_minion_id("M0ZZ").unwrap(), "M0ZZ");
    }

    #[test]
    fn test_normalize_minion_id_without_prefix() {
        assert_eq!(normalize_minion_id("42").unwrap(), "M42");
        assert_eq!(normalize_minion_id("001").unwrap(), "M001");
        assert_eq!(normalize_minion_id("0ZZ").unwrap(), "M0ZZ");
    }

    #[test]
    fn test_normalize_minion_id_rejects_path_traversal() {
        // Test parent directory references
        assert!(normalize_minion_id("M../../etc").is_err());
        assert!(normalize_minion_id("M42/../evil").is_err());
        assert!(normalize_minion_id("../M42").is_err());

        // Test path separators
        assert!(normalize_minion_id("M/etc/passwd").is_err());
        assert!(normalize_minion_id("M42/subdir").is_err());
        assert!(normalize_minion_id(r"M42\subdir").is_err());

        // Test null bytes
        assert!(normalize_minion_id("M42\0").is_err());

        // Test non-alphanumeric characters
        assert!(normalize_minion_id("M42!").is_err());
        assert!(normalize_minion_id("M42@evil").is_err());
        assert!(normalize_minion_id("M42-test").is_err());
    }
}
