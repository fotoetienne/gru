mod git;
mod github;
mod logger;
mod minion;
mod progress;
mod stream;
mod workspace;
mod worktree_scanner;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use once_cell::sync::Lazy;
use progress::{ProgressConfig, ProgressDisplay};
use std::io::Write;
use std::path::PathBuf;
use stream::EventStream;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Timeout in seconds for each line read from Claude's output stream
/// Set to 5 minutes to accommodate long-running LLM operations
const STREAM_TIMEOUT_SECS: u64 = 300;

/// Regex for extracting issue links from PR bodies
static ISSUE_LINK_REGEX: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?i)(?:fixes|closes|resolves)\s+#(\d+)")
        .expect("Failed to compile issue link regex")
});

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
async fn handle_fix(issue: &str, quiet: bool) -> Result<i32> {
    // Parse issue information
    let (owner_opt, repo_opt, issue_num) = parse_issue_info(issue)?;

    // Always generate a unique minion ID
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    // Check if we have full repo information for workspace creation
    let worktree_path_opt = if let (Some(owner), Some(repo)) = (owner_opt.clone(), repo_opt.clone())
    {
        // Full URL provided - create workspace and launch Claude
        println!(
            "🚀 Setting up workspace for {}/{}#{}",
            owner, repo, issue_num
        );

        // Initialize workspace
        let workspace =
            workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

        // Create bare repository path
        let bare_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
        let git_repo = git::GitRepo::new(&owner, &repo, bare_path);

        // Ensure bare repository is cloned/updated
        println!("📦 Ensuring repository is cloned...");
        git_repo
            .ensure_bare_clone()
            .context("Failed to clone or update repository")?;

        // Fetch issue details to check if already claimed
        println!("🔍 Fetching issue details...");
        let issue_data = github::fetch_issue(&owner, &repo, &issue_num)
            .await
            .context("Failed to fetch issue details")?;

        // Check if issue already has in-progress label
        if github::has_in_progress_label(&issue_data) {
            println!(
                "⚠️  Warning: Issue #{} already has 'in-progress' label",
                issue_num
            );
            println!("   This issue may already be claimed by another Minion.");
            println!("   Do you want to continue? (Press Ctrl+C to cancel or Enter to continue)");

            // Wait for user confirmation
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("Failed to read user input")?;
        }

        // Add in-progress label immediately to minimize race condition window
        // This must happen before the expensive worktree creation
        println!("🏷️  Adding 'in-progress' label...");
        github::add_in_progress_label(&owner, &repo, &issue_num)
            .await
            .context("Failed to add in-progress label")?;

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

        // Post claim comment (non-critical, so we can tolerate failure here)
        println!("💬 Posting claim comment...");
        if let Err(e) = github::post_claim_comment(
            &owner,
            &repo,
            &issue_num,
            &minion_id,
            &branch_name,
            &worktree_path.to_string_lossy(),
        )
        .await
        {
            eprintln!("⚠️  Warning: Failed to post claim comment: {}", e);
            eprintln!("   Continuing with the fix anyway...");
        }

        println!("🤖 Launching Claude...\n");

        Some(worktree_path)
    } else {
        // Plain issue number - use simple mode without workspace
        println!("⚠️  No repository URL provided. Using simple mode without workspace management.");
        println!(
            "   For full workspace support, use: gru fix https://github.com/owner/repo/issues/{}\n",
            issue_num
        );
        None
    };

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = Command::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions")
        .arg(format!("/fix {}", issue_num))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    // If we have a worktree, set the working directory and env var
    if let Some(ref path) = worktree_path_opt {
        cmd.current_dir(path);
        cmd.env("GRU_WORKSPACE", &minion_id);
    }

    // Spawn the command
    let mut child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    // Get the stdout handle
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture stdout from claude process")?;

    // Create event stream reader
    let mut stream = EventStream::from_stdout(stdout);

    // Process stream output asynchronously with timeout and error handling
    let stream_result = async {
        loop {
            // Handle timeout first, then flatten the stream result
            let line_result = timeout(Duration::from_secs(STREAM_TIMEOUT_SECS), stream.next_line())
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Timeout: Claude process hasn't produced output in {} seconds",
                        STREAM_TIMEOUT_SECS
                    )
                })?;

            // Now handle the stream result
            match line_result? {
                Some(output) => progress.handle_output(&output),
                None => break, // Stream ended normally
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Always wait for the process, regardless of stream errors
    let status = child.wait().await?;

    // Now check if there was a stream error
    stream_result?;

    // Finish the progress display
    if status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue));
    }

    // Return the exit code from the claude process
    Ok(status.code().unwrap_or(128))
}

/// Handles the review command by delegating to the Claude CLI
/// Returns the exit code from the claude process
async fn handle_review(pr: &str) -> Result<i32> {
    // Validate the PR format before proceeding
    validate_pr_format(pr)?;

    // Execute the claude CLI with the /pr_review command
    let status = Command::new("claude")
        .arg(format!("/pr_review {}", pr))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code",
        )?;

    // Return the exit code from the claude process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(128))
}

/// Normalizes a Minion ID by adding the 'M' prefix if missing
/// Validates that the ID contains only alphanumeric characters to prevent path traversal
fn normalize_minion_id(id: &str) -> Result<String> {
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

/// Handles the path command to resolve a Minion's worktree path
/// Returns 0 on success, 1 on error
async fn handle_path(
    minion_id: Option<String>,
    issue: Option<u64>,
    pr: Option<u64>,
) -> Result<i32> {
    // Validate that at least one option is provided
    // Note: clap's conflicts_with_all ensures mutual exclusion (at most one)
    if minion_id.is_none() && issue.is_none() && pr.is_none() {
        anyhow::bail!("Must provide either a minion ID, --issue, or --pr");
    }

    // Resolve to a Minion ID
    let resolved_minion_id = if let Some(id) = minion_id {
        // Direct Minion ID resolution
        normalize_minion_id(&id)?
    } else if let Some(issue_num) = issue {
        // Resolve from issue number via GitHub API
        resolve_minion_from_issue(issue_num).await?
    } else {
        // Must be PR number (validated above that at least one is present)
        resolve_minion_from_pr(pr.unwrap()).await?
    };

    // Construct the worktree path
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let worktree_base = home.join(".gru").join("work");

    // Find the Minion worktree (this validates existence)
    let worktree_path = find_minion_worktree(&worktree_base, &resolved_minion_id)?;

    // Output just the path to stdout
    println!("{}", worktree_path.display());
    Ok(0)
}

/// Finds a Minion's worktree by searching the work directory
fn find_minion_worktree(work_dir: &std::path::Path, minion_id: &str) -> Result<PathBuf> {
    use std::fs;

    // The structure is ~/.gru/work/owner/repo/M<id>
    // We need to search for the Minion ID across all owner/repo subdirectories

    if !work_dir.exists() {
        anyhow::bail!("Work directory does not exist. No Minions have been created yet.");
    }

    // Walk through owner directories
    for owner_entry in fs::read_dir(work_dir)? {
        let owner_entry = owner_entry?;
        let owner_path = owner_entry.path();

        if !owner_path.is_dir() {
            continue;
        }

        // Walk through repo directories
        for repo_entry in fs::read_dir(&owner_path)? {
            let repo_entry = repo_entry?;
            let repo_path = repo_entry.path();

            if !repo_path.is_dir() {
                continue;
            }

            // Check if this repo has the Minion worktree
            let minion_path = repo_path.join(minion_id);

            // Defensive check: verify the path stays within the work directory
            if !minion_path.starts_with(work_dir) {
                anyhow::bail!(
                    "Security error: Minion path escapes work directory. This should never happen."
                );
            }

            if minion_path.exists() && minion_path.is_dir() {
                return Ok(minion_path);
            }
        }
    }

    anyhow::bail!(
        "No worktree found for Minion {}. It may not have been created yet.",
        minion_id
    );
}

/// Resolves a Minion ID from a GitHub issue number
async fn resolve_minion_from_issue(issue_num: u64) -> Result<String> {
    // Use gh CLI to get issue labels
    let output = Command::new("gh")
        .args(["issue", "view", &issue_num.to_string(), "--json", "labels"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch issue #{}: {}", issue_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    // Look for in-progress:M<id> label
    let labels = json["labels"]
        .as_array()
        .context("Issue labels field is not an array")?;

    for label in labels {
        let label_name = label["name"]
            .as_str()
            .context("Label name is not a string")?;

        if let Some(minion_id) = label_name.strip_prefix("in-progress:") {
            if minion_id.starts_with('M') {
                return Ok(minion_id.to_string());
            }
        }
    }

    anyhow::bail!(
        "No active Minion found for issue #{}. Issue may not be in progress.",
        issue_num
    );
}

/// Resolves a Minion ID from a GitHub PR number
async fn resolve_minion_from_pr(pr_num: u64) -> Result<String> {
    // Use gh CLI to get linked issue from PR body
    let output = Command::new("gh")
        .args(["pr", "view", &pr_num.to_string(), "--json", "body"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR #{}: {}", pr_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    let body = json["body"]
        .as_str()
        .context("PR body field is not a string")?;

    // Look for "Fixes #<issue>" or "Closes #<issue>" in the PR body
    if let Some(captures) = ISSUE_LINK_REGEX.captures(body) {
        let issue_num = captures[1]
            .parse::<u64>()
            .context("Failed to parse issue number from PR body")?;

        // Now resolve the Minion from that issue
        return resolve_minion_from_issue(issue_num).await;
    }

    anyhow::bail!(
        "No linked issue found for PR #{}. PR must contain 'Fixes #<issue>' in its description.",
        pr_num
    );
}

/// Handles the clean command to remove merged/closed worktrees
fn handle_clean(dry_run: bool, force: bool, base_branch: &str) -> Result<i32> {
    let ws = workspace::Workspace::new().context("Failed to initialize workspace")?;

    println!("Scanning for worktrees in {}...", ws.repos().display());

    // Discover all worktrees
    let worktrees =
        worktree_scanner::discover_worktrees(ws.repos()).context("Failed to discover worktrees")?;

    if worktrees.is_empty() {
        println!("No worktrees found.");
        return Ok(0);
    }

    println!("Found {} worktrees. Checking status...\n", worktrees.len());

    // Check status of each worktree
    let mut cleanable = Vec::new();
    for wt in worktrees {
        let status = wt
            .status(base_branch)
            .with_context(|| format!("Failed to check status of {}", wt.path.display()))?;

        if status != worktree_scanner::WorktreeStatus::Active {
            cleanable.push((wt, status));
        }
    }

    if cleanable.is_empty() {
        println!("No worktrees to clean.");
        return Ok(0);
    }

    // Display cleanable worktrees
    println!("Cleanable worktrees:\n");
    for (wt, status) in &cleanable {
        let reason = match status {
            worktree_scanner::WorktreeStatus::Merged => "branch merged",
            worktree_scanner::WorktreeStatus::IssueClosed => "issue closed",
            worktree_scanner::WorktreeStatus::RemoteDeleted => "remote deleted",
            worktree_scanner::WorktreeStatus::Active => unreachable!(),
        };
        println!("  {} ({})", wt.path.display(), reason);
        println!("    Branch: {}", wt.branch);
        println!("    Repo: {}\n", wt.repo);
    }

    if dry_run {
        println!("Dry run mode - no worktrees were removed.");
        return Ok(0);
    }

    // Confirm removal unless force flag is set
    if !force {
        print!("Remove {} worktrees? [y/N]: ", cleanable.len());
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        if input != "y" && input != "yes" {
            println!("Cancelled.");
            return Ok(0);
        }
    }

    // Remove worktrees
    println!("\nRemoving worktrees...");
    let mut removed = 0;
    let mut failed = 0;

    for (wt, _) in cleanable {
        print!("Removing {}... ", wt.path.display());
        std::io::stdout().flush()?;

        // Remove the worktree
        let status = std::process::Command::new("git")
            .args([
                "-C",
                &wt.bare_repo_path.to_string_lossy(),
                "worktree",
                "remove",
                &wt.path.to_string_lossy(),
            ])
            .output()?;

        if status.status.success() {
            println!("✓");
            removed += 1;

            // Also remove the branch from the bare repository
            let branch_result = std::process::Command::new("git")
                .args([
                    "-C",
                    &wt.bare_repo_path.to_string_lossy(),
                    "branch",
                    "-D",
                    &wt.branch,
                ])
                .output();

            if let Err(e) = branch_result {
                eprintln!("  Warning: Failed to delete branch: {}", e);
            }
        } else {
            println!("✗");
            eprintln!("  Error: {}", String::from_utf8_lossy(&status.stderr));
            failed += 1;
        }
    }

    println!("\nSummary: {} removed, {} failed", removed, failed);

    Ok(if failed > 0 { 1 } else { 0 })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fix { issue } => handle_fix(&issue, cli.quiet).await,
        Commands::Review { pr } => handle_review(&pr).await,
        Commands::Path {
            minion_id,
            issue,
            pr,
        } => handle_path(minion_id, issue, pr).await,
        Commands::Clean {
            dry_run,
            force,
            base_branch,
        } => handle_clean(dry_run, force, &base_branch),
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

    #[test]
    fn test_find_minion_worktree_nonexistent_work_dir() {
        use std::path::PathBuf;
        let nonexistent_dir = PathBuf::from("/tmp/gru_test_nonexistent");
        let result = find_minion_worktree(&nonexistent_dir, "M42");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Work directory does not exist"));
    }
}
