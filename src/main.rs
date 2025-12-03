mod git;
mod logger;
mod minion;
mod progress;
mod stream;
mod workspace;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use progress::{ProgressConfig, ProgressDisplay};
use std::process::Command;
use stream::EventStream;

/// CLI structure for the Gru agent orchestrator
#[derive(Parser)]
#[command(name = "gru")]
#[command(version)]
#[command(about = "Local-First LLM Agent Orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Run in quiet mode (only show errors)
    #[arg(short, long, global = true)]
    quiet: bool,
}

/// Available commands for Gru
#[derive(Subcommand)]
enum Commands {
    #[command(about = "Fix a GitHub issue")]
    Fix {
        #[arg(help = "Issue number or URL to fix")]
        issue: String,
    },
    #[command(about = "Review a GitHub pull request")]
    Review {
        #[arg(help = "PR number or URL to review")]
        pr: String,
    },
}

/// Validates that the issue argument is either a number or a valid GitHub URL
fn validate_issue_format(issue: &str) -> Result<()> {
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
fn validate_pr_format(pr: &str) -> Result<()> {
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
/// Supports both plain issue numbers and GitHub URLs
fn parse_issue_info(issue: &str) -> Result<(Option<String>, Option<String>, String)> {
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

        Ok((Some(owner), Some(repo), issue_num))
    } else {
        // Plain issue number - no owner/repo info
        Ok((None, None, issue.to_string()))
    }
}

/// Handles the fix command by delegating to the Claude CLI
/// Returns the exit code from the claude process
<<<<<<< HEAD
fn handle_fix(issue: &str) -> Result<i32> {
    // Parse issue information
    let (owner_opt, repo_opt, issue_num) = parse_issue_info(issue)?;

    // Check if we have full repo information for workspace creation
    if let (Some(owner), Some(repo)) = (owner_opt, repo_opt) {
        // Full URL provided - create workspace and launch Claude
        println!(
            "🚀 Setting up workspace for {}/{}#{}",
            owner, repo, issue_num
        );

        // Initialize workspace
        let workspace =
            workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

        // Generate minion ID
        let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;

        println!("📋 Generated Minion ID: {}", minion_id);

        // Create bare repository path
        let bare_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
        let git_repo = git::GitRepo::new(&owner, &repo, bare_path);

        // Ensure bare repository is cloned/updated
        println!("📦 Ensuring repository is cloned...");
        git_repo
            .ensure_bare_clone()
            .context("Failed to clone or update repository")?;

        // Create worktree path
        let repo_name = format!("{}/{}", owner, repo);
        let worktree_path = workspace
            .work_dir(&repo_name, &minion_id)
            .context("Failed to compute worktree path")?;

        // Create worktree with branch name: minion/issue-<num>-<id>
        let branch_name = format!("minion/issue-{}-{}", issue_num, minion_id);
        println!("🌿 Creating worktree with branch: {}", branch_name);

        git_repo
            .create_worktree(&branch_name, &worktree_path)
            .context("Failed to create worktree")?;

        println!("📂 Workspace created at: {}", worktree_path.display());
        println!("🤖 Launching Claude...\n");

        // Launch Claude with environment variable and in the worktree directory
        let status = Command::new("claude")
            .arg(format!("/fix {}", issue_num))
            .current_dir(&worktree_path)
            .env("GRU_WORKSPACE", &minion_id)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context(
                "claude command not found. Install from: https://github.com/anthropics/claude-code",
            )?;

        // Return the exit code from the claude process
        Ok(status.code().unwrap_or(128))
    } else {
        // Plain issue number - fall back to simple delegation
        // This maintains backward compatibility when no URL is provided
        println!("⚠️  No repository URL provided. Using simple mode without workspace management.");
        println!(
            "   For full workspace support, use: gru fix https://github.com/owner/repo/issues/{}\n",
            issue_num
        );

        let status = Command::new("claude")
            .arg(format!("/fix {}", issue_num))
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context(
                "claude command not found. Install from: https://github.com/anthropics/claude-code",
            )?;

        Ok(status.code().unwrap_or(128))
    }
=======
fn handle_fix(issue: &str, quiet: bool) -> Result<i32> {
    // Validate the issue format before proceeding
    validate_issue_format(issue)?;

    // TODO: Extract actual minion ID from workspace
    let minion_id = "M04R".to_string();

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Execute the claude CLI with the /fix command, capturing stdout
    let mut child = Command::new("claude")
        .arg(format!("/fix {}", issue))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code",
        )?;

    // Get the stdout handle
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture stdout from claude process")?;

    // Create event stream reader
    let mut stream = EventStream::from_stdout(stdout);

    // Process stream output
    while let Some(output) = stream.read_line()? {
        progress.handle_output(&output);
    }

    // Wait for the process to finish
    let status = child.wait()?;

    // Finish the progress display
    if status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue));
    }

    // Return the exit code from the claude process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(128))
>>>>>>> b25526d (Implement real-time progress display for Minion work)
}

/// Handles the review command by delegating to the Claude CLI
/// Returns the exit code from the claude process
fn handle_review(pr: &str) -> Result<i32> {
    // Validate the PR format before proceeding
    validate_pr_format(pr)?;

    // Execute the claude CLI with the /pr_review command
    let status = Command::new("claude")
        .arg(format!("/pr_review {}", pr))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code",
        )?;

    // Return the exit code from the claude process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(128))
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue } => handle_fix(&issue, cli.quiet),
        Commands::Review { pr } => handle_review(&pr),
    };

    // Handle any errors that occurred
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
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
        assert_eq!(result.0, Some("fotoetienne".to_string()));
        assert_eq!(result.1, Some("gru".to_string()));
        assert_eq!(result.2, "42".to_string());
    }

    #[test]
    fn test_parse_issue_info_with_plain_number() {
        let result = parse_issue_info("42").unwrap();
        assert_eq!(result.0, None);
        assert_eq!(result.1, None);
        assert_eq!(result.2, "42".to_string());
    }

    #[test]
    fn test_parse_issue_info_with_url_and_query_params() {
        let result = parse_issue_info("https://github.com/owner/repo/issues/123?foo=bar").unwrap();
        assert_eq!(result.0, Some("owner".to_string()));
        assert_eq!(result.1, Some("repo".to_string()));
        assert_eq!(result.2, "123".to_string());
    }
}
