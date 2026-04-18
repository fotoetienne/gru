use anyhow::{bail, Context, Result};

use crate::config::{GhHostConfig, LabConfig};
use crate::git::GitRepo;
use crate::github;
use crate::labels;
use crate::workspace::Workspace;
use std::collections::HashMap;

/// Repository source type for initialization
#[derive(Debug, Clone)]
pub(crate) enum RepoSource {
    /// GitHub repository in "owner/repo" format
    GitHub(String),
    /// Current directory (detect from git remote)
    CurrentDir,
}

/// Parse repository source from command line argument
pub(crate) fn parse_repo_source(arg: &str) -> Result<RepoSource> {
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
        • GitHub repo: gru init owner/repo\n\
        • Current directory: gru init .\n\n\
        Tip: Specify owner/repo for GitHub or use . for current directory.",
        arg,
    );
}

/// Check if a binary is available on PATH.
fn check_binary(name: &str) -> bool {
    which::which(name).is_ok()
}

/// Validate that a `--host` value is a bare hostname (no scheme, path, or special characters).
fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("Invalid --host value: hostname cannot be empty");
    }
    if host.contains(char::is_whitespace) {
        bail!("Invalid --host value: {:?} contains whitespace", host);
    }
    if host.contains("://") {
        bail!(
            "Invalid --host value: {:?} looks like a URL — use just the hostname (e.g. \"ghe.example.com\")",
            host
        );
    }
    if host.contains('/') {
        bail!(
            "Invalid --host value: {:?} contains a path — use just the hostname (e.g. \"ghe.example.com\")",
            host
        );
    }
    if host.contains('@') {
        bail!(
            "Invalid --host value: {:?} contains '@' — use just the hostname (e.g. \"ghe.example.com\")",
            host
        );
    }
    if host.starts_with('.') || host.ends_with('.') {
        bail!(
            "Invalid --host value: {:?} has a leading or trailing dot",
            host
        );
    }
    Ok(())
}

/// Build the `daemon.repos` entry for a repository.
///
/// Resolution order:
/// 1. `github.com` → `owner/repo`
/// 2. Host matches a named `[github_hosts.*]` entry → `name:owner/repo`
/// 3. Host contains a dot → legacy `host/owner/repo`
/// 4. Otherwise → `None` (caller should warn and skip)
fn build_repo_entry(
    host: &str,
    owner: &str,
    repo: &str,
    github_hosts: &HashMap<String, GhHostConfig>,
) -> Option<String> {
    if host.eq_ignore_ascii_case("github.com") {
        return Some(format!("{}/{}", owner, repo));
    }
    // `validate_github_hosts` enforces case-insensitive uniqueness of `host`
    // across entries, so at most one match is possible here.
    if let Some((name, _)) = github_hosts
        .iter()
        .find(|(_, gh)| gh.host.eq_ignore_ascii_case(host))
    {
        return Some(format!("{}:{}/{}", name, owner, repo));
    }
    if host.contains('.') {
        return Some(format!("{}/{}/{}", host, owner, repo));
    }
    None
}

/// Run prerequisite checks before initialization.
/// Returns `Ok(0)` if all critical prerequisites are met, or `Ok(1)` if required tools
/// are missing. The integer value is intended to be used as a process exit code.
fn check_prerequisites() -> Result<i32> {
    println!("Checking prerequisites...\n");
    let mut has_errors = false;

    // 1. Check for gh CLI
    if check_binary("gh") {
        println!("  ✓ gh (GitHub CLI)");
    } else {
        println!("  ✗ gh (GitHub CLI) — not found");
        println!("    Install: https://cli.github.com/");
        has_errors = true;
    }

    // 2. Check for at least one agent backend
    let has_claude = check_binary("claude");
    let has_codex = check_binary("codex");

    if has_claude {
        println!("  ✓ claude (Claude Code CLI)");
    }
    if has_codex {
        println!("  ✓ codex (OpenAI Codex CLI)");
    }
    if !has_claude && !has_codex {
        println!("  ⚠ No agent backend found (claude or codex)");
        println!(
            "    Install Claude Code: https://docs.anthropic.com/en/docs/claude-code/overview"
        );
        println!("    Install Codex: https://github.com/openai/codex");
    }

    if has_errors {
        println!("\n❌ Missing required tools. Install them and try again.");
        return Ok(1);
    }

    println!();
    Ok(0)
}

/// Initialize a repository for use with Gru
pub(crate) async fn handle_init(repo_arg: String, host_override: Option<String>) -> Result<i32> {
    // Validate --host early
    if let Some(ref h) = host_override {
        validate_host(h)?;
    }

    // Run prerequisite checks
    let prereq_result = check_prerequisites()?;
    if prereq_result != 0 {
        return Ok(prereq_result);
    }

    // Parse repository source
    let repo_source = parse_repo_source(&repo_arg)?;

    // Resolve to owner/repo/host format
    let (owner, repo, host) = match repo_source {
        RepoSource::GitHub(github_repo) => {
            let parts: Vec<&str> = github_repo.split('/').collect();
            let owner = parts[0].to_string();
            let host = if let Some(ref h) = host_override {
                h.clone()
            } else {
                github::infer_github_host(&owner, None)
            };
            (owner, parts[1].to_string(), host)
        }
        RepoSource::CurrentDir => {
            println!("🔍 Detecting repository from current directory...");
            let (owner, repo, detected_host) = detect_current_repo().await?;
            let host = host_override.unwrap_or(detected_host);
            (owner, repo, host)
        }
    };

    println!("Initializing repository: {}/{}\n", owner, repo);

    // 1. Verify GitHub access via gh CLI
    println!("🔐 Verifying GitHub access...");
    match github::check_auth_via_cli(&host).await {
        Ok(()) => {
            println!("✓ Authenticated via gh CLI");
        }
        Err(e) => {
            log::debug!("auth check error detail: {:#}", e);
            eprintln!("\n❌ GitHub authentication failed for host: {}", host);
            eprintln!("\nTo authenticate, run:\n");
            if host.eq_ignore_ascii_case("github.com") {
                eprintln!("  gh auth login");
            } else {
                eprintln!("  gh auth login --hostname {}", host);
            }
            eprintln!();
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
            println!("  Edit it to customize Gru's behavior.");
        }
        Ok(false) => {
            println!("  • Config exists: {}", config_path.display());
        }
        Err(e) => {
            log::warn!("  ⚠️  Could not create config file: {:#}", e);
        }
    }

    // 4. Add repo to daemon.repos in config
    let github_hosts = match LabConfig::load_partial(&config_path) {
        Ok(cfg) => cfg.github_hosts,
        Err(e) => {
            log::warn!(
                "  ⚠️  Could not load config for [github_hosts.*] lookup; \
                 proceeding without alias resolution (repo entry will use \
                 owner/repo for github.com, legacy host/owner/repo for other \
                 hosts with a dot, or be skipped): {:#}",
                e
            );
            HashMap::new()
        }
    };
    match build_repo_entry(&host, &owner, &repo, &github_hosts) {
        Some(repo_entry) => match LabConfig::add_repo_to_config(&config_path, &repo_entry) {
            Ok(true) => {
                println!(
                    "✓ Added {} to daemon.repos in {}",
                    repo_entry,
                    config_path.display()
                );
            }
            Ok(false) => {
                println!("  ℹ {} already in daemon.repos", repo_entry);
            }
            Err(e) => {
                log::warn!("  ⚠️  Could not update daemon.repos: {}", e);
            }
        },
        None => {
            // Hosts without dots (e.g., "localhost") can't be represented in
            // host/owner/repo format (config parser requires a dot). Skip and advise.
            log::warn!(
                "  ⚠️  Cannot add {}/{}/{} to daemon.repos: host {:?} has no dot. \
                 Add the repo manually using a [github_hosts.*] named entry.",
                host,
                owner,
                repo,
                host,
            );
        }
    }

    // 5. Clone/update bare repository
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

    // 6. Create all required labels (idempotent via --force)
    println!("\n🏷️  Configuring labels...");
    let mut labels_failed = Vec::new();

    for (name, color, description) in labels::ALL_LABELS {
        match github::create_label_via_cli(&host, &owner, &repo, name, color, description).await {
            Ok(()) => {
                println!("  ✓ Ensured: {}", name);
            }
            Err(e) => {
                labels_failed.push(name.to_string());
                log::error!("  ✗ Failed to create {}: {:#}", name, e);
            }
        }
    }

    if !labels_failed.is_empty() {
        log::warn!(
            "\n⚠️  Warning: Failed to create {} label(s). You may need write access to the repository.",
            labels_failed.len()
        );
    }

    // 7. Check for ready issues
    println!("\n🔍 Checking for ready issues...");
    match github::list_ready_issues_via_cli(&owner, &repo, &host, labels::TODO).await {
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
            log::warn!("  ⚠️  Could not check for issues: {:#}", e);
        }
    }

    // Summary
    let gh_host_prefix = if host.eq_ignore_ascii_case("github.com") {
        String::new()
    } else {
        format!("GH_HOST={} ", host)
    };

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("✓ Repository is ready!\n");
    println!("Next steps:");
    println!("  1. Label an issue for Gru to work on:");
    println!(
        "     {}gh issue edit <number> --add-label {} -R {}/{}",
        gh_host_prefix,
        labels::TODO,
        owner,
        repo,
    );
    println!("  2. Start a Minion:");
    println!("     gru do {}/{}#<number>", owner, repo);
    println!("  3. Or run in daemon mode:");
    println!("     gru lab");
    println!("  4. Check status:");
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
    let host_registry = crate::config::load_host_registry();
    let remote_url = get_github_remote(&host_registry)
        .await
        .context("No GitHub remote found in current repository")?;

    // Parse owner/repo from remote URL
    let (host, owner, repo) = parse_github_remote(&remote_url, &host_registry)
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
    fn test_parse_local_path_now_fails() {
        let result = parse_repo_source("./path/to/repo");
        assert!(result.is_err());

        let result = parse_repo_source("/absolute/path");
        assert!(result.is_err());
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
        assert!(result.is_err());

        let result = parse_repo_source("owner/");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_check_binary_finds_common_tools() {
        assert!(check_binary("sh"));
        assert!(!check_binary("definitely-not-a-real-binary-xyz123"));
    }

    #[test]
    fn test_validate_host_accepts_valid_hostnames() {
        assert!(validate_host("github.com").is_ok());
        assert!(validate_host("ghe.example.com").is_ok());
        assert!(validate_host("my-github.corp.net").is_ok());
        assert!(validate_host("localhost").is_ok());
    }

    #[test]
    fn test_validate_host_rejects_empty() {
        assert!(validate_host("").is_err());
    }

    #[test]
    fn test_validate_host_rejects_whitespace() {
        assert!(validate_host("ghe. example.com").is_err());
        assert!(validate_host(" github.com").is_err());
    }

    #[test]
    fn test_validate_host_rejects_urls() {
        assert!(validate_host("https://ghe.example.com").is_err());
        assert!(validate_host("http://github.com").is_err());
    }

    #[test]
    fn test_validate_host_rejects_paths() {
        assert!(validate_host("ghe.example.com/path").is_err());
    }

    #[test]
    fn test_validate_host_rejects_at_sign() {
        assert!(validate_host("user@ghe.example.com").is_err());
    }

    fn host(h: &str) -> GhHostConfig {
        GhHostConfig {
            host: h.to_string(),
            web_url: None,
        }
    }

    #[test]
    fn test_build_repo_entry_github_com() {
        let hosts = HashMap::new();
        assert_eq!(
            build_repo_entry("github.com", "foo", "bar", &hosts),
            Some("foo/bar".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_prefers_named_host_alias() {
        let mut hosts = HashMap::new();
        hosts.insert("netflix".to_string(), host("github.netflix.net"));
        assert_eq!(
            build_repo_entry("github.netflix.net", "foo", "bar", &hosts),
            Some("netflix:foo/bar".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_named_host_match_is_case_insensitive() {
        let mut hosts = HashMap::new();
        hosts.insert("corp".to_string(), host("GHE.Example.COM"));
        assert_eq!(
            build_repo_entry("ghe.example.com", "o", "r", &hosts),
            Some("corp:o/r".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_non_matching_named_host_falls_through_to_legacy() {
        let mut hosts = HashMap::new();
        hosts.insert("netflix".to_string(), host("github.netflix.net"));
        assert_eq!(
            build_repo_entry("ghe.other.com", "foo", "bar", &hosts),
            Some("ghe.other.com/foo/bar".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_falls_through_when_no_alias_matches() {
        let mut hosts = HashMap::new();
        hosts.insert("netflix".to_string(), host("github.netflix.net"));
        // Host doesn't match the configured alias — should fall through to legacy.
        assert_eq!(
            build_repo_entry("ghe.other.com", "foo", "bar", &hosts),
            Some("ghe.other.com/foo/bar".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_legacy_fallback_for_unnamed_host() {
        let hosts = HashMap::new();
        assert_eq!(
            build_repo_entry("ghe.example.com", "foo", "bar", &hosts),
            Some("ghe.example.com/foo/bar".to_string())
        );
    }

    #[test]
    fn test_build_repo_entry_host_without_dot_returns_none() {
        let hosts = HashMap::new();
        assert_eq!(build_repo_entry("localhost", "foo", "bar", &hosts), None);
    }

    #[test]
    fn test_validate_host_rejects_leading_trailing_dots() {
        assert!(validate_host(".example.com").is_err());
        assert!(validate_host("example.com.").is_err());
    }
}
