mod minion;
mod workspace;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::process::Command;

mod git;

/// CLI structure for the Gru agent orchestrator
#[derive(Parser)]
#[command(name = "gru")]
#[command(version)]
#[command(about = "Local-First LLM Agent Orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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

/// Handles the fix command by delegating to the Claude CLI
/// Returns the exit code from the claude process
fn handle_fix(issue: &str) -> Result<i32> {
    // Validate the issue format before proceeding
    validate_issue_format(issue)?;

    // Execute the claude CLI with the /fix command
    let status = Command::new("claude")
        .arg(format!("/fix {}", issue))
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
        Commands::Fix { issue } => handle_fix(&issue),
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
}
