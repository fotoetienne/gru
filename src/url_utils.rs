use crate::config::HostRegistry;
use crate::git;
use crate::github;
use anyhow::{Context, Result};

/// The type of resource referenced in a GitHub URL
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GitHubResourceType {
    Issue,
    Pull,
}

/// Parsed components of a GitHub URL
#[derive(Debug, Clone)]
pub(crate) struct GitHubUrl {
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) resource_type: GitHubResourceType,
    pub(crate) number: u32,
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
/// Matches against every API host and every configured web UI host in
/// `host_registry`. When the URL uses a web UI host (e.g. the Netflix pattern
/// where `git.netflix.net` is the API host and `github.netflix.net` is the web
/// UI host), the returned `GitHubUrl::host` is the canonical API host so
/// subsequent `gh` CLI calls target the correct endpoint.
///
/// Handles URLs like:
/// - `https://github.com/owner/repo/issues/42`
/// - `https://github.com/owner/repo/pull/42`
/// - `https://<configured-ghe-host>/owner/repo/issues/42`
/// - `https://<configured-web-url-host>/owner/repo/issues/42`
/// - URLs with query params, fragments, and trailing slashes
///
/// Returns `None` if the URL is not a valid GitHub issue/PR URL.
pub(crate) fn parse_github_url(url: &str, host_registry: &HostRegistry) -> Option<GitHubUrl> {
    let cleaned = clean_url(url);

    for host in host_registry.all_url_hosts() {
        let prefix = format!("https://{}/", host);
        if let Some(path) = cleaned.strip_prefix(&prefix) {
            let parts: Vec<&str> = path.split('/').collect();

            if parts.len() != 4 {
                continue;
            }

            let owner = parts[0];
            let repo = parts[1];
            let resource_type_str = parts[2];
            let number_str = parts[3];

            if owner.is_empty() || repo.is_empty() {
                continue;
            }

            let resource_type = match resource_type_str {
                "issues" => GitHubResourceType::Issue,
                "pull" => GitHubResourceType::Pull,
                _ => continue,
            };

            let number = match number_str.parse::<u32>() {
                Ok(n) => n,
                Err(_) => continue,
            };

            // `host` came from `all_url_hosts()`, so `canonical_host` always resolves.
            let canonical = host_registry
                .canonical_host(&host)
                .expect("host from all_url_hosts is always resolvable");

            return Some(GitHubUrl {
                host: canonical,
                owner: owner.to_string(),
                repo: repo.to_string(),
                resource_type,
                number,
            });
        }
    }

    None
}

/// Extracts owner, repo, host, and issue number from an issue argument.
///
/// Supports both plain issue numbers (auto-detects from current directory) and GitHub URLs.
/// `host_registry` defines all recognized hosts. The returned `host` is always
/// the canonical API host, even when a URL used a configured web UI host.
/// Returns `(owner, repo, issue_number, host)`.
pub(crate) async fn parse_issue_info(
    issue: &str,
    host_registry: &HostRegistry,
) -> Result<(String, String, String, String)> {
    // Check if it's a plain number
    if let Ok(num) = issue.parse::<u32>() {
        // Auto-detect repository from current directory
        git::detect_git_repo()
            .await
            .context("Failed to detect git repository")?;

        let remote_url = git::get_github_remote(host_registry)
            .await
            .context("Failed to get GitHub remote")?;

        let (host, owner, repo) = git::parse_github_remote(&remote_url, host_registry)
            .context("Failed to parse GitHub remote URL")?;

        return Ok((owner, repo, num.to_string(), host));
    }

    // Try parsing as a GitHub URL
    if let Some(parsed) = parse_github_url(issue, host_registry) {
        if parsed.resource_type == GitHubResourceType::Issue {
            return Ok((
                parsed.owner,
                parsed.repo,
                parsed.number.to_string(),
                parsed.host,
            ));
        }
        // Parsed successfully but wrong resource type (e.g., PR URL given for issue command)
        anyhow::bail!(
            "Expected a GitHub issue URL, but got a pull request URL.\n\
             Did you mean to use `gru review` instead?"
        );
    }

    anyhow::bail!(
        "Invalid issue format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru do 42\n\
         - gru do https://github.com/owner/repo/issues/42"
    );
}

/// Builds the argument list for `gh pr view` to fetch PR metadata.
///
/// When `repo` is `Some`, includes `--repo owner/repo` so the command targets the
/// correct repository regardless of the current working directory.
fn build_pr_view_args(pr_num: &str, repo: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr_num.to_string(),
        "--json".to_string(),
        "headRefName,headRepository,headRepositoryOwner".to_string(),
    ];
    if let Some(repo) = repo {
        args.push("--repo".to_string());
        args.push(repo.to_string());
    }
    args
}

/// Extracts owner, repo, PR number, and branch name from a PR argument.
///
/// Supports both plain PR numbers and GitHub URLs.
/// `host_registry` defines all recognized hosts. The returned `host` is always
/// the canonical API host, even when a URL used a configured web UI host.
/// For plain numbers, fetches metadata from GitHub to get branch info.
/// Returns `(owner, repo, pr_number, branch, host)`.
pub(crate) async fn parse_pr_info(
    pr: &str,
    host_registry: &HostRegistry,
) -> Result<(String, String, String, String, String)> {
    // Extract PR number, gh command, and optional repo qualifier
    let (pr_num, detected_host, repo_flag) = if pr.parse::<u32>().is_ok() {
        // Plain number: detect repo from current directory to pick gh vs ghe
        git::detect_git_repo()
            .await
            .context("Failed to detect git repository")?;
        let remote_url = git::get_github_remote(host_registry)
            .await
            .context("Failed to get GitHub remote")?;
        let (host, _det_owner, _det_repo) = git::parse_github_remote(&remote_url, host_registry)
            .context("Failed to parse GitHub remote URL")?;
        (pr.to_string(), host, None)
    } else if let Some(parsed) = parse_github_url(pr, host_registry) {
        if parsed.resource_type != GitHubResourceType::Pull {
            // Parsed successfully but wrong resource type (e.g., issue URL given for review command)
            anyhow::bail!(
                "Expected a GitHub pull request URL, but got an issue URL.\n\
                 Did you mean to use `gru do` instead?"
            );
        }
        let repo_full = github::repo_slug(&parsed.owner, &parsed.repo);
        (parsed.number.to_string(), parsed.host, Some(repo_full))
    } else {
        anyhow::bail!(
            "Invalid PR format. Expected: <number> or <github-url>\n\
             Examples:\n\
             - gru review 42\n\
             - gru review https://github.com/owner/repo/pull/42"
        );
    };

    // Fetch PR metadata from GitHub to get branch and repo info
    let args = build_pr_view_args(&pr_num, repo_flag.as_deref());
    let output = github::gh_cli_command(&detected_host)
        .args(&args)
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

    Ok((owner, repo, pr_num, branch, detected_host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GhHostConfig, LabConfig};

    fn default_hosts() -> HostRegistry {
        HostRegistry::from_config(&LabConfig::default())
    }

    fn hosts_with_ghe() -> HostRegistry {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "ghe".to_string(),
            GhHostConfig {
                host: "ghe.netflix.net".to_string(),
                web_url: None,
            },
        );
        HostRegistry::from_config(&config)
    }

    fn hosts_with_web_url() -> HostRegistry {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        HostRegistry::from_config(&config)
    }

    // --- parse_github_url tests ---

    #[test]
    fn test_parse_github_url_issue() {
        let result = parse_github_url(
            "https://github.com/fotoetienne/gru/issues/42",
            &default_hosts(),
        )
        .unwrap();
        assert_eq!(result.host, "github.com");
        assert_eq!(result.owner, "fotoetienne");
        assert_eq!(result.repo, "gru");
        assert_eq!(result.resource_type, GitHubResourceType::Issue);
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_ghe_issue() {
        let result = parse_github_url(
            "https://ghe.netflix.net/netflix/some-service/issues/99",
            &hosts_with_ghe(),
        )
        .unwrap();
        assert_eq!(result.host, "ghe.netflix.net");
        assert_eq!(result.owner, "netflix");
        assert_eq!(result.repo, "some-service");
        assert_eq!(result.resource_type, GitHubResourceType::Issue);
        assert_eq!(result.number, 99);
    }

    #[test]
    fn test_parse_github_url_ghe_pull() {
        let result = parse_github_url(
            "https://ghe.netflix.net/netflix/some-service/pull/10",
            &hosts_with_ghe(),
        )
        .unwrap();
        assert_eq!(result.host, "ghe.netflix.net");
        assert_eq!(result.owner, "netflix");
        assert_eq!(result.repo, "some-service");
        assert_eq!(result.resource_type, GitHubResourceType::Pull);
        assert_eq!(result.number, 10);
    }

    #[test]
    fn test_parse_github_url_ghe_not_recognized_without_config() {
        // GHE URLs should NOT parse if the host isn't configured
        assert!(parse_github_url(
            "https://ghe.netflix.net/netflix/some-service/issues/99",
            &default_hosts(),
        )
        .is_none());
    }

    #[test]
    fn test_parse_github_url_pull() {
        let result = parse_github_url(
            "https://github.com/owner/repo-name/pull/123",
            &default_hosts(),
        )
        .unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo-name");
        assert_eq!(result.resource_type, GitHubResourceType::Pull);
        assert_eq!(result.number, 123);
    }

    #[test]
    fn test_parse_github_url_strips_query_params() {
        let result = parse_github_url(
            "https://github.com/owner/repo/issues/42?foo=bar",
            &default_hosts(),
        )
        .unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_strips_fragments() {
        let result = parse_github_url(
            "https://github.com/owner/repo/issues/42#comment-123",
            &default_hosts(),
        )
        .unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_strips_trailing_slash() {
        let result =
            parse_github_url("https://github.com/owner/repo/pull/42/", &default_hosts()).unwrap();
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_combined_edge_cases() {
        let result = parse_github_url(
            "https://github.com/owner/repo/issues/42/?foo=bar#comment",
            &default_hosts(),
        )
        .unwrap();
        assert_eq!(result.owner, "owner");
        assert_eq!(result.repo, "repo");
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_rejects_non_github() {
        assert!(parse_github_url("https://example.com/issues/42", &default_hosts()).is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_http() {
        // parse_github_url only accepts https:// web URLs;
        // use git::parse_github_remote for git remote URLs (which accept http:// and SSH)
        assert!(
            parse_github_url("http://github.com/owner/repo/issues/42", &default_hosts()).is_none()
        );
    }

    #[test]
    fn test_parse_github_url_rejects_empty_owner() {
        assert!(parse_github_url("https://github.com//repo/issues/42", &default_hosts()).is_none());
    }

    #[test]
    fn test_parse_github_url_rejects_empty_repo() {
        assert!(
            parse_github_url("https://github.com/owner//issues/42", &default_hosts()).is_none()
        );
    }

    #[test]
    fn test_parse_github_url_rejects_missing_number() {
        assert!(
            parse_github_url("https://github.com/owner/repo/issues/", &default_hosts()).is_none()
        );
    }

    #[test]
    fn test_parse_github_url_rejects_non_numeric_number() {
        assert!(
            parse_github_url("https://github.com/owner/repo/issues/abc", &default_hosts())
                .is_none()
        );
    }

    #[test]
    fn test_parse_github_url_rejects_unknown_resource_type() {
        assert!(
            parse_github_url("https://github.com/owner/repo/wiki/42", &default_hosts()).is_none()
        );
    }

    #[test]
    fn test_parse_github_url_rejects_incomplete_path() {
        assert!(parse_github_url("https://github.com/owner/repo", &default_hosts()).is_none());
        assert!(parse_github_url("https://github.com/owner", &default_hosts()).is_none());
        assert!(parse_github_url("https://github.com/issues/", &default_hosts()).is_none());
    }

    // --- parse_issue_info tests (URL paths only; plain numbers need git context) ---

    #[tokio::test]
    async fn test_parse_issue_info_with_url() {
        let result = parse_issue_info(
            "https://github.com/fotoetienne/gru/issues/42",
            &default_hosts(),
        )
        .await
        .unwrap();
        assert_eq!(result.0, "fotoetienne");
        assert_eq!(result.1, "gru");
        assert_eq!(result.2, "42");
        assert_eq!(result.3, "github.com");
    }

    #[tokio::test]
    async fn test_parse_issue_info_url_normalizes_number() {
        // Leading zeros are normalized by parsing through u32
        let result = parse_issue_info("https://github.com/owner/repo/issues/042", &default_hosts())
            .await
            .unwrap();
        assert_eq!(result.2, "42");
    }

    #[tokio::test]
    async fn test_parse_issue_info_with_url_and_query_params() {
        let result = parse_issue_info(
            "https://github.com/owner/repo/issues/123?foo=bar",
            &default_hosts(),
        )
        .await
        .unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
        assert_eq!(result.2, "123");
    }

    #[tokio::test]
    async fn test_parse_issue_info_rejects_invalid() {
        let hosts = default_hosts();
        assert!(parse_issue_info("not-a-number", &hosts).await.is_err());
        assert!(parse_issue_info("", &hosts).await.is_err());
        assert!(parse_issue_info("-42", &hosts).await.is_err());
    }

    #[tokio::test]
    async fn test_parse_issue_info_rejects_pr_url_with_specific_message() {
        let err = parse_issue_info("https://github.com/owner/repo/pull/42", &default_hosts())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pull request URL"),
            "Expected specific error for PR URL given to issue parser, got: {}",
            msg
        );
    }

    // --- build_pr_view_args tests ---

    #[test]
    fn test_build_pr_view_args_without_repo() {
        let args = build_pr_view_args("42", None);
        assert_eq!(
            args,
            [
                "pr",
                "view",
                "42",
                "--json",
                "headRefName,headRepository,headRepositoryOwner"
            ]
        );
        assert!(!args.contains(&"--repo".to_string()));
    }

    #[test]
    fn test_build_pr_view_args_with_repo() {
        let args = build_pr_view_args("99", Some("fotoetienne/gru"));
        assert!(args.contains(&"--repo".to_string()));
        assert!(args.contains(&"fotoetienne/gru".to_string()));
        // --repo should come after the base args
        let repo_idx = args.iter().position(|a| a == "--repo").unwrap();
        assert_eq!(args[repo_idx + 1], "fotoetienne/gru");
    }

    // --- parse_pr_info validation tests (only format validation; gh calls need network) ---

    #[tokio::test]
    async fn test_parse_pr_info_rejects_invalid() {
        let hosts = default_hosts();
        assert!(parse_pr_info("not-a-number", &hosts).await.is_err());
        assert!(parse_pr_info("", &hosts).await.is_err());
        assert!(parse_pr_info("-42", &hosts).await.is_err());
    }

    #[tokio::test]
    async fn test_parse_pr_info_rejects_issue_url_with_specific_message() {
        let err = parse_pr_info("https://github.com/owner/repo/issues/42", &default_hosts())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("issue URL"),
            "Expected specific error for issue URL given to PR parser, got: {}",
            msg
        );
    }

    // --- web_url → API host mapping tests ---

    #[test]
    fn test_parse_github_url_web_url_issue_maps_to_api_host() {
        // URL using the configured web UI hostname is accepted, and the
        // returned host is the canonical API host.
        let result = parse_github_url(
            "https://github.netflix.net/corp/service/issues/42",
            &hosts_with_web_url(),
        )
        .unwrap();
        assert_eq!(result.host, "git.netflix.net");
        assert_eq!(result.owner, "corp");
        assert_eq!(result.repo, "service");
        assert_eq!(result.resource_type, GitHubResourceType::Issue);
        assert_eq!(result.number, 42);
    }

    #[test]
    fn test_parse_github_url_web_url_pull_maps_to_api_host() {
        let result = parse_github_url(
            "https://github.netflix.net/corp/service/pull/99",
            &hosts_with_web_url(),
        )
        .unwrap();
        assert_eq!(result.host, "git.netflix.net");
        assert_eq!(result.resource_type, GitHubResourceType::Pull);
        assert_eq!(result.number, 99);
    }

    #[test]
    fn test_parse_github_url_api_host_still_works_with_web_url_config() {
        // The API host continues to parse correctly when web_url is also configured.
        let result = parse_github_url(
            "https://git.netflix.net/corp/service/issues/7",
            &hosts_with_web_url(),
        )
        .unwrap();
        assert_eq!(result.host, "git.netflix.net");
        assert_eq!(result.number, 7);
    }

    #[tokio::test]
    async fn test_parse_issue_info_web_url_returns_api_host() {
        // End-to-end: web UI URL resolves to API host for subsequent gh CLI calls.
        let (owner, repo, num, host) = parse_issue_info(
            "https://github.netflix.net/corp/service/issues/2612",
            &hosts_with_web_url(),
        )
        .await
        .unwrap();
        assert_eq!(owner, "corp");
        assert_eq!(repo, "service");
        assert_eq!(num, "2612");
        assert_eq!(host, "git.netflix.net");
    }

    #[tokio::test]
    async fn test_parse_pr_info_rejects_issue_url_on_web_url_host() {
        // Issue URL via web UI hostname is recognized (rejected for being a PR
        // parser, not for being unrecognized). The error message references the
        // issue URL type — confirming parsing succeeded before type validation.
        let err = parse_pr_info(
            "https://github.netflix.net/corp/service/issues/42",
            &hosts_with_web_url(),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("issue URL"),
            "Expected specific error for issue URL given to PR parser, got: {}",
            msg
        );
    }
}
