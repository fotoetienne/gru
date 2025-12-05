use crate::git;
use anyhow::{Context, Result};
use tokio::process::Command;

/// Validates that the issue argument is either a number or a valid GitHub URL
pub fn validate_issue_format(issue: &str) -> Result<()> {
    // Check if it's a number
    if issue.parse::<u32>().is_ok() {
        return Ok(());
    }

    // Check if it's a GitHub URL with proper format
    // Expected: https://github.com/owner/repo/issues/123
    if issue.starts_with("https://github.com/") {
        // Strip query parameters and fragments
        let url = issue
            .split('?')
            .next()
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .trim_end_matches('/');

        let parts: Vec<&str> = url
            .strip_prefix("https://github.com/")
            .unwrap()
            .split('/')
            .collect();

        if parts.len() == 4
            && !parts[0].is_empty() // owner
            && !parts[1].is_empty() // repo
            && parts[2] == "issues"
            && parts[3].parse::<u32>().is_ok()
        // issue number
        {
            return Ok(());
        }
    }

    anyhow::bail!(
        "Invalid issue format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru fix 42\n\
         - gru fix https://github.com/owner/repo/issues/42"
    );
}

/// Validates that the PR argument is either a number or a valid GitHub URL
pub fn validate_pr_format(pr: &str) -> Result<()> {
    // Check if it's a number
    if pr.parse::<u32>().is_ok() {
        return Ok(());
    }

    // Check if it's a GitHub URL with proper format
    // Expected: https://github.com/owner/repo/pull/123
    if pr.starts_with("https://github.com/") {
        // Strip query parameters and fragments
        let url = pr
            .split('?')
            .next()
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .trim_end_matches('/');

        let parts: Vec<&str> = url
            .strip_prefix("https://github.com/")
            .unwrap()
            .split('/')
            .collect();

        if parts.len() == 4
            && !parts[0].is_empty() // owner
            && !parts[1].is_empty() // repo
            && parts[2] == "pull"
            && parts[3].parse::<u32>().is_ok()
        // PR number
        {
            return Ok(());
        }
    }

    anyhow::bail!(
        "Invalid PR format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru review 42\n\
         - gru review https://github.com/owner/repo/pull/42"
    );
}

/// Extracts owner, repo, and issue number from an issue argument
/// Supports both plain issue numbers (auto-detects from current directory) and GitHub URLs
pub fn parse_issue_info(issue: &str) -> Result<(String, String, String)> {
    // First validate the format
    validate_issue_format(issue)?;

    // Check if it's a GitHub URL
    if issue.starts_with("https://github.com/") {
        // Strip query parameters and fragments
        let url = issue
            .split('?')
            .next()
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .trim_end_matches('/');

        let parts: Vec<&str> = url
            .strip_prefix("https://github.com/")
            .unwrap()
            .split('/')
            .collect();

        // parts[0] = owner, parts[1] = repo, parts[2] = "issues", parts[3] = number
        let owner = parts[0].to_string();
        let repo = parts[1].to_string();
        let issue_num = parts[3].to_string();

        Ok((owner, repo, issue_num))
    } else {
        // Plain issue number - auto-detect repository from current directory
        git::detect_git_repo().context("Failed to detect git repository")?;

        let remote_url = git::get_github_remote().context("Failed to get GitHub remote")?;

        let (owner, repo) =
            git::parse_github_remote(&remote_url).context("Failed to parse GitHub remote URL")?;

        Ok((owner, repo, issue.to_string()))
    }
}

/// Extracts owner, repo, PR number, and branch name from a PR argument
/// Supports both plain PR numbers and GitHub URLs
/// For plain numbers, fetches metadata from GitHub to get branch info
pub async fn parse_pr_info(pr: &str) -> Result<(String, String, String, String)> {
    // First validate the format
    validate_pr_format(pr)?;

    // Extract PR number
    let pr_num = if pr.parse::<u32>().is_ok() {
        pr.to_string()
    } else if pr.starts_with("https://github.com/") {
        // Strip query parameters and fragments
        let url = pr
            .split('?')
            .next()
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .trim_end_matches('/');

        let parts: Vec<&str> = url
            .strip_prefix("https://github.com/")
            .unwrap()
            .split('/')
            .collect();

        // parts[0] = owner, parts[1] = repo, parts[2] = "pull", parts[3] = number
        parts[3].to_string()
    } else {
        anyhow::bail!("Invalid PR format");
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

    #[test]
    fn test_validate_issue_format_with_number() {
        assert!(validate_issue_format("42").is_ok());
        assert!(validate_issue_format("1").is_ok());
        assert!(validate_issue_format("999999").is_ok());
    }

    #[test]
    fn test_validate_issue_format_with_valid_url() {
        assert!(validate_issue_format("https://github.com/fotoetienne/gru/issues/42").is_ok());
        assert!(validate_issue_format("https://github.com/owner/repo-name/issues/123").is_ok());
    }

    #[test]
    fn test_validate_issue_format_rejects_invalid_input() {
        assert!(validate_issue_format("not-a-number").is_err());
        assert!(validate_issue_format("https://example.com/issues/42").is_err());
        assert!(validate_issue_format("https://github.com/issues/").is_err());
        assert!(validate_issue_format("https://github.com/owner/issues/").is_err());
        assert!(validate_issue_format("https://github.com/owner/repo/issues/").is_err());
        assert!(validate_issue_format("").is_err());
    }

    #[test]
    fn test_validate_issue_format_rejects_negative_numbers() {
        assert!(validate_issue_format("-42").is_err());
    }

    #[test]
    fn test_validate_issue_format_handles_edge_cases() {
        // Trailing slashes should be handled
        assert!(validate_issue_format("https://github.com/owner/repo/issues/42/").is_ok());
        // Query parameters should be ignored
        assert!(validate_issue_format("https://github.com/owner/repo/issues/42?foo=bar").is_ok());
        // Fragments should be ignored
        assert!(
            validate_issue_format("https://github.com/owner/repo/issues/42#comment-123").is_ok()
        );
        // Combined edge cases
        assert!(
            validate_issue_format("https://github.com/owner/repo/issues/42/?foo=bar#comment")
                .is_ok()
        );
    }

    #[test]
    fn test_validate_issue_format_rejects_empty_owner_or_repo() {
        // Empty owner
        assert!(validate_issue_format("https://github.com//repo/issues/42").is_err());
        // Empty repo
        assert!(validate_issue_format("https://github.com/owner//issues/42").is_err());
        // Both empty
        assert!(validate_issue_format("https://github.com///issues/42").is_err());
    }

    #[test]
    fn test_validate_pr_format_with_number() {
        assert!(validate_pr_format("42").is_ok());
        assert!(validate_pr_format("1").is_ok());
        assert!(validate_pr_format("999999").is_ok());
    }

    #[test]
    fn test_validate_pr_format_with_valid_url() {
        assert!(validate_pr_format("https://github.com/fotoetienne/gru/pull/42").is_ok());
        assert!(validate_pr_format("https://github.com/owner/repo-name/pull/123").is_ok());
    }

    #[test]
    fn test_validate_pr_format_rejects_invalid_input() {
        assert!(validate_pr_format("not-a-number").is_err());
        assert!(validate_pr_format("https://example.com/pull/42").is_err());
        assert!(validate_pr_format("https://github.com/pull/").is_err());
        assert!(validate_pr_format("https://github.com/owner/pull/").is_err());
        assert!(validate_pr_format("https://github.com/owner/repo/pull/").is_err());
        assert!(validate_pr_format("").is_err());
    }

    #[test]
    fn test_validate_pr_format_rejects_negative_numbers() {
        assert!(validate_pr_format("-42").is_err());
    }

    #[test]
    fn test_validate_pr_format_handles_edge_cases() {
        // Trailing slashes should be handled
        assert!(validate_pr_format("https://github.com/owner/repo/pull/42/").is_ok());
        // Query parameters should be ignored
        assert!(validate_pr_format("https://github.com/owner/repo/pull/42?foo=bar").is_ok());
        // Fragments should be ignored
        assert!(validate_pr_format("https://github.com/owner/repo/pull/42#comment-123").is_ok());
        // Combined edge cases
        assert!(
            validate_pr_format("https://github.com/owner/repo/pull/42/?foo=bar#comment").is_ok()
        );
    }

    #[test]
    fn test_validate_pr_format_rejects_empty_owner_or_repo() {
        // Empty owner
        assert!(validate_pr_format("https://github.com//repo/pull/42").is_err());
        // Empty repo
        assert!(validate_pr_format("https://github.com/owner//pull/42").is_err());
        // Both empty
        assert!(validate_pr_format("https://github.com///pull/42").is_err());
    }

    #[test]
    fn test_parse_issue_info_with_url() {
        let result = parse_issue_info("https://github.com/fotoetienne/gru/issues/42").unwrap();
        assert_eq!(result.0, "fotoetienne".to_string());
        assert_eq!(result.1, "gru".to_string());
        assert_eq!(result.2, "42".to_string());
    }

    #[test]
    fn test_parse_issue_info_with_url_and_query_params() {
        let result = parse_issue_info("https://github.com/owner/repo/issues/123?foo=bar").unwrap();
        assert_eq!(result.0, "owner".to_string());
        assert_eq!(result.1, "repo".to_string());
        assert_eq!(result.2, "123".to_string());
    }

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
