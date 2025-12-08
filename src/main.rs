mod commands;
mod git;
mod github;
mod logger;
mod minion;
mod progress;
mod stream;
mod url_utils;
mod workspace;
mod worktree_scanner;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use commands::{clean, fix, path, review};

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
    #[command(about = "Get the filesystem path to a Minion's worktree")]
    Path {
        #[arg(help = "Minion ID (e.g., M42 or 42)", conflicts_with_all = ["issue", "pr"])]
        minion_id: Option<String>,

        #[arg(long, help = "Resolve from issue number", conflicts_with_all = ["minion_id", "pr"])]
        issue: Option<u64>,

        #[arg(long, help = "Resolve from PR number", conflicts_with_all = ["minion_id", "issue"])]
        pr: Option<u64>,
    },
    #[command(about = "Clean up merged/closed worktrees")]
    Clean {
        #[arg(long, help = "Show what would be cleaned without removing")]
        dry_run: bool,
        #[arg(long, help = "Force removal without confirmation")]
        force: bool,
        #[arg(long, default_value = "main", help = "Base branch to check for merges")]
        base_branch: String,
    },
    #[command(about = "List active Minions")]
    Status,
}

/// Represents the status of a single Minion
#[derive(Debug)]
struct MinionStatus {
    minion_id: String,
    issue_number: String,
    repo_name: String,
    branch: String,
    status: String,
    uptime: String,
}

/// Scans the ~/.gru/work directory for active Minion worktrees
fn scan_worktrees() -> Result<Vec<MinionStatus>> {
    let workspace = workspace::Workspace::new().context("Failed to initialize workspace")?;
    let work_path = workspace.work();

    if !work_path.exists() {
        return Ok(Vec::new());
    }

    let mut minions = Vec::new();

    // Iterate over owner directories
    for owner_entry in std::fs::read_dir(work_path)? {
        let owner_entry = owner_entry?;
        if !owner_entry.path().is_dir() {
            continue;
        }

        // Iterate over repo directories
        for repo_entry in std::fs::read_dir(owner_entry.path())? {
            let repo_entry = repo_entry?;
            if !repo_entry.path().is_dir() {
                continue;
            }

            // Iterate over minion directories (should start with 'M')
            for minion_entry in std::fs::read_dir(repo_entry.path())? {
                let minion_entry = minion_entry?;
                let minion_path = minion_entry.path();

                if !minion_path.is_dir() {
                    continue;
                }

                let minion_id = minion_entry.file_name().to_string_lossy().to_string();

                // Check if this is a valid git worktree
                let git_dir = minion_path.join(".git");
                if !git_dir.exists() {
                    continue;
                }

                // Get the branch name from git
                let branch_output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&minion_path)
                    .arg("branch")
                    .arg("--show-current")
                    .output()?;

                let branch = String::from_utf8_lossy(&branch_output.stdout)
                    .trim()
                    .to_string();

                // Parse issue number from branch name (format: minion/issue-<num>-<id>)
                let issue_number = parse_issue_from_branch(&branch);

                // Determine status (Active or Idle) based on git index modification time
                let status = determine_status(&minion_path)?;

                // Calculate uptime from worktree creation time
                let uptime = calculate_uptime(&minion_path)?;

                // Build repo name from path components
                let owner = owner_entry.file_name().to_string_lossy().to_string();
                let repo = repo_entry.file_name().to_string_lossy().to_string();
                let repo_name = format!("{}/{}", owner, repo);

                minions.push(MinionStatus {
                    minion_id,
                    issue_number,
                    repo_name,
                    branch,
                    status,
                    uptime,
                });
            }
        }
    }

    // Sort by minion ID
    minions.sort_by(|a, b| a.minion_id.cmp(&b.minion_id));

    Ok(minions)
}

/// Parses the issue number from a branch name
/// Expected format: minion/issue-<num>-<id>
fn parse_issue_from_branch(branch: &str) -> String {
    if let Some(issue_part) = branch.strip_prefix("minion/issue-") {
        // Extract the number before the next hyphen
        if let Some(pos) = issue_part.find('-') {
            return issue_part[..pos].to_string();
        }
    }
    "?".to_string()
}

/// Determines if a Minion is Active or Idle based on git index modification time
/// A Minion is considered Active if the git index was modified in the last 5 minutes
fn determine_status(worktree_path: &std::path::Path) -> Result<String> {
    // Use git rev-parse to get the actual git directory path
    // In worktrees, .git is a file, not a directory
    let git_dir_output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg("--git-dir")
        .output()?;

    if !git_dir_output.status.success() {
        return Ok("Idle".to_string());
    }

    let git_dir = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();
    let git_index = std::path::PathBuf::from(git_dir).join("index");

    if !git_index.exists() {
        return Ok("Idle".to_string());
    }

    let metadata = std::fs::metadata(&git_index)?;
    let modified = metadata.modified()?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(modified).unwrap_or_default();

    // Consider active if modified within the last 5 minutes
    if elapsed.as_secs() < 300 {
        Ok("Active".to_string())
    } else {
        Ok("Idle".to_string())
    }
}

/// Calculates the uptime of a worktree based on its creation time
fn calculate_uptime(worktree_path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(worktree_path)?;
    let created = metadata.created().or_else(|_| metadata.modified())?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(created).unwrap_or_default();

    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        Ok(format!("{}d", days))
    } else if hours > 0 {
        Ok(format!("{}h", hours))
    } else if minutes > 0 {
        Ok(format!("{}m", minutes))
    } else {
        Ok("< 1m".to_string())
    }
}

/// Handles the status command by displaying active Minions
fn handle_status() -> Result<i32> {
    let minions = scan_worktrees()?;

    if minions.is_empty() {
        println!("No active Minions");
        return Ok(0);
    }

    // Print table header
    println!(
        "{:<8} {:<8} {:<20} {:<30} {:<10} {:<8}",
        "MINION", "ISSUE", "REPO", "BRANCH", "STATUS", "UPTIME"
    );

    // Print each minion
    for minion in &minions {
        println!(
            "{:<8} #{:<7} {:<20} {:<30} {:<10} {:<8}",
            minion.minion_id,
            minion.issue_number,
            minion.repo_name,
            minion.branch,
            minion.status,
            minion.uptime
        );
    }

    println!();
    println!("{} Minion(s) found", minions.len());

    Ok(0)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue } => fix::handle_fix(&issue, cli.quiet).await,
        Commands::Review { pr } => review::handle_review(&pr).await,
        Commands::Path {
            minion_id,
            issue,
            pr,
        } => path::handle_path(minion_id, issue, pr).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => clean::handle_clean(dry_run, force, &base_branch).await,
        Commands::Status => handle_status(),
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

    #[test]
    fn test_parse_issue_from_branch_valid() {
        assert_eq!(parse_issue_from_branch("minion/issue-42-M001"), "42");
        assert_eq!(parse_issue_from_branch("minion/issue-123-M999"), "123");
        assert_eq!(parse_issue_from_branch("minion/issue-1-M0tk"), "1");
    }

    #[test]
    fn test_parse_issue_from_branch_invalid() {
        assert_eq!(parse_issue_from_branch("main"), "?");
        assert_eq!(parse_issue_from_branch("feature/branch"), "?");
        assert_eq!(parse_issue_from_branch(""), "?");
        assert_eq!(parse_issue_from_branch("minion/issue-"), "?");
    }

    #[test]
    fn test_scan_worktrees_returns_valid_vec() {
        // This test verifies that scan_worktrees succeeds and returns a valid vector
        // Note: A more thorough test would use a temporary directory with known contents
        let result = scan_worktrees();
        assert!(result.is_ok());

        // Verify we get a vector and all minions have required fields
        let minions = result.unwrap();
        for minion in &minions {
            assert!(!minion.minion_id.is_empty());
            assert!(!minion.repo_name.is_empty());
            assert!(!minion.status.is_empty());
            assert!(!minion.uptime.is_empty());
        }
    }
}
