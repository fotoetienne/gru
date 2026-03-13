use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::config::LabConfig;
use crate::git::GitRepo;
use crate::github::GitHubClient;
use crate::labels;
use crate::workspace::Workspace;

/// Repository source type for initialization
#[derive(Debug, Clone)]
pub enum RepoSource {
    /// GitHub repository in "owner/repo" format
    GitHub(String),
    /// Local filesystem path (not yet implemented)
    #[allow(dead_code)]
    LocalPath(PathBuf),
    /// Current directory (detect from git remote)
    CurrentDir,
}

/// Parse repository source from command line argument
pub fn parse_repo_source(arg: &str) -> Result<RepoSource> {
    // Explicit path markers
    if arg.starts_with("./") || arg.starts_with("../") || arg.starts_with('/') {
        return Ok(RepoSource::LocalPath(PathBuf::from(arg)));
    }

    // Current directory
    if arg == "." {
        return Ok(RepoSource::CurrentDir);
    }

    // GitHub owner/repo format (exactly one slash, no dots)
    if arg.matches('/').count() == 1 && !arg.contains("..") {
        // Validate that it looks like owner/repo format
        let parts: Vec<&str> = arg.split('/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Ok(RepoSource::GitHub(arg.to_string()));
        }
    }

    // Ambiguous case
    bail!(
        "Ambiguous repository path: {}\n\n\
        Did you mean:\n\
        • Local path: gru init ./{}\n\
        • GitHub repo: gru init owner/repo\n\n\
        Tip: Use ./ prefix for local paths, or specify owner/repo for GitHub.",
        arg,
        arg
    );
}

/// Initialize a repository for use with Gru
pub async fn handle_init(repo_arg: String) -> Result<i32> {
    // Parse repository source
    let repo_source = parse_repo_source(&repo_arg)?;

    // Resolve to owner/repo/host format
    let (owner, repo, host) = match repo_source {
        RepoSource::GitHub(github_repo) => {
            let parts: Vec<&str> = github_repo.split('/').collect();
            let owner = parts[0].to_string();
            let host = crate::github::infer_github_host(&owner).to_string();
            (owner, parts[1].to_string(), host)
        }
        RepoSource::CurrentDir => {
            println!("🔍 Detecting repository from current directory...");
            detect_current_repo().await?
        }
        RepoSource::LocalPath(_) => {
            bail!("Local path repositories are not yet supported. Please use GitHub repositories in owner/repo format.");
        }
    };

    println!("Initializing repository: {}/{}\n", owner, repo);

    // 1. Verify GitHub access
    println!("🔐 Verifying GitHub access...");
    let github_client = match GitHubClient::from_env(&owner, &repo).await {
        Ok(client) => client,
        Err(_) => {
            log::error!("\n❌ GitHub token not found or invalid\n");
            log::warn!(
                "To use Gru, you need a GitHub personal access token with the following scopes:"
            );
            log::error!("  • repo (Full control of private repositories)");
            log::error!("  • read:org (Read org and team membership)\n");
            log::error!("Create a token at: https://github.com/settings/tokens\n");
            log::error!("Then set it as an environment variable:");
            log::error!("  export GRU_GITHUB_TOKEN=ghp_xxxxxxxxxxxx\n");
            return Ok(1);
        }
    };

    // Validate token by fetching current user
    match github_client.get_authenticated_user().await {
        Ok(user) => {
            println!("✓ Authenticated as: {}", user.login);
        }
        Err(e) => {
            log::error!("\n❌ Failed to authenticate with GitHub: {}", e);
            log::error!("\nPlease check that your GRU_GITHUB_TOKEN is valid.");
            return Ok(1);
        }
    }

    // 2. Initialize workspace
    println!("\n📁 Setting up workspace...");
    let workspace = Workspace::new().context("Failed to initialize workspace")?;

    // 3. Generate default config file
    println!("\n⚙️  Checking configuration...");
    let config_path = LabConfig::default_path()?;
    match LabConfig::write_default_config(&config_path) {
        Ok(true) => {
            println!("✓ Created default config: {}", config_path.display());
        }
        Ok(false) => {
            println!("  • Config exists: {}", config_path.display());
        }
        Err(e) => {
            log::warn!("  ⚠️  Could not create config file: {}", e);
        }
    }

    // 4. Clone/update bare repository
    println!("\n📦 Setting up repository mirror...");
    let bare_repo_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
    let git_repo = GitRepo::new(&owner, &repo, &host, bare_repo_path.clone());

    match git_repo.ensure_bare_clone().await {
        Ok(()) => {
            println!("✓ Bare repository ready: {}", bare_repo_path.display());
        }
        Err(e) => {
            log::error!("\n❌ Failed to clone repository: {:#}", e);
            log::error!("\nPlease check that:");
            log::error!("  • The repository {}/{} exists on GitHub", owner, repo);
            log::error!("  • You have read access to the repository");
            log::error!("  • Your network connection is working");
            return Ok(1);
        }
    }

    // 4. Create all required labels (idempotent)
    println!("\n🏷️  Configuring labels...");
    let mut labels_failed = Vec::new();

    for (name, color, description) in labels::ALL_LABELS {
        match github_client
            .create_label(&owner, &repo, name, color, description)
            .await
        {
            Ok(created) => {
                if created {
                    println!("  ✓ Created: {}", name);
                } else {
                    println!("  • Exists: {}", name);
                }
            }
            Err(e) => {
                labels_failed.push(name.to_string());
                log::error!("  ✗ Failed to create {}: {}", name, e);
            }
        }
    }

    if !labels_failed.is_empty() {
        log::warn!(
            "\n⚠️  Warning: Failed to create {} label(s). You may need write access to the repository.",
            labels_failed.len()
        );
    }

    // 6. Check for ready issues
    println!("\n🔍 Checking for ready issues...");
    match github_client
        .list_issues_with_label(&owner, &repo, labels::TODO)
        .await
    {
        Ok(issues) => {
            if issues.is_empty() {
                println!("  No issues labeled '{}' yet", labels::TODO);
            } else {
                println!(
                    "✓ Found {} issue(s) labeled '{}'",
                    issues.len(),
                    labels::TODO
                );
            }
        }
        Err(e) => {
            log::warn!("  ⚠️  Could not check for issues: {}", e);
        }
    }

    // Summary
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("✓ Repository is ready!\n");
    println!("Next steps:");
    println!("  1. Mark an issue as ready:");
    println!("     gh issue edit 42 --add-label {}", labels::TODO);
    println!("  2. Start a Minion:");
    println!("     gru do {}/{}#42", owner, repo);
    println!("  3. Check status:");
    println!("     gru status");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    Ok(0)
}

/// Detect repository from current directory's git remote
async fn detect_current_repo() -> Result<(String, String, String)> {
    use crate::git::{detect_git_repo, get_github_remote, parse_github_remote};

    // Check if we're in a git repo
    let _git_dir = detect_git_repo().await.context("Not in a git repository")?;

    // Get the remote URL (function doesn't need git_dir - it uses current directory)
    let github_hosts = crate::config::load_github_hosts();
    let remote_url = get_github_remote(&github_hosts)
        .await
        .context("No GitHub remote found in current repository")?;

    // Parse owner/repo from remote URL
    let (host, owner, repo) = parse_github_remote(&remote_url, &github_hosts)
        .context("Could not parse GitHub owner/repo from remote URL")?;

    println!("  Detected: {}/{}", owner, repo);

    Ok((owner, repo, host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_repo() {
        let result = parse_repo_source("owner/repo").unwrap();
        match result {
            RepoSource::GitHub(repo) => assert_eq!(repo, "owner/repo"),
            _ => panic!("Expected GitHub source"),
        }
    }

    #[test]
    fn test_parse_current_dir() {
        let result = parse_repo_source(".").unwrap();
        assert!(matches!(result, RepoSource::CurrentDir));
    }

    #[test]
    fn test_parse_local_path() {
        let result = parse_repo_source("./path/to/repo").unwrap();
        assert!(matches!(result, RepoSource::LocalPath(_)));

        let result = parse_repo_source("/absolute/path").unwrap();
        assert!(matches!(result, RepoSource::LocalPath(_)));
    }

    #[test]
    fn test_parse_ambiguous_fails() {
        let result = parse_repo_source("ambiguous");
        assert!(result.is_err());

        let result = parse_repo_source("path/to/repo/subdir");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_github_format() {
        // No slash
        let result = parse_repo_source("noslash");
        assert!(result.is_err());

        // Too many slashes
        let result = parse_repo_source("owner/repo/extra");
        assert!(result.is_err());

        // Empty parts
        let result = parse_repo_source("/repo");
        matches!(result.unwrap(), RepoSource::LocalPath(_));

        let result = parse_repo_source("owner/");
        assert!(result.is_err());
    }
}
