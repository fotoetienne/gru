use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

use crate::labels;

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

    "github.com".to_string()
}

/// Creates a pre-configured `tokio::process::Command` for the `gh` CLI.
///
/// Always uses the `gh` binary and sets `GH_HOST` to the provided host
/// so authentication targets the correct server. This ensures deterministic
/// host selection even when the parent process has `GH_HOST` set.
pub fn gh_cli_command(host: &str) -> Command {
    let mut cmd = Command::new("gh");
    cmd.env("GH_HOST", host);
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- infer_github_host tests ---

    #[test]
    fn test_infer_github_host_public_owner() {
        assert_eq!(infer_github_host("octocat"), "github.com");
    }

    #[test]
    fn test_infer_github_host_empty() {
        assert_eq!(infer_github_host(""), "github.com");
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
