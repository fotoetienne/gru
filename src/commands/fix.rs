use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::pr_state::PrState;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::stream::{self, EventStream};
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

/// Timeout in seconds for each line read from Claude's output stream
/// Set to 5 minutes to accommodate long-running LLM operations
const STREAM_TIMEOUT_SECS: u64 = 300;

/// Duration of inactivity before warning the user
const INACTIVITY_WARNING_SECS: u64 = 300; // 5 minutes

/// Duration of inactivity before considering the task stuck
const INACTIVITY_STUCK_SECS: u64 = 900; // 15 minutes

/// Logs an event to events.jsonl in the worktree directory
async fn log_event(worktree_path: &Path, event: &stream::StreamOutput) -> Result<()> {
    let events_file = worktree_path.join("events.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_file)
        .await
        .context("Failed to open events.jsonl")?;

    // Only log actual events, not raw lines
    if let stream::StreamOutput::Event(claude_event) = event {
        let json = serde_json::to_string(claude_event)?;
        file.write_all(json.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
    }
    Ok(())
}

/// Checks if a branch has been pushed to the remote
async fn is_branch_pushed(worktree_path: &Path, branch_name: &str) -> Result<bool> {
    let output = TokioCommand::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg(format!("origin/{}", branch_name))
        .output()
        .await
        .context("Failed to check if branch is pushed")?;

    Ok(output.status.success())
}

/// Creates a draft PR for the given branch
async fn create_pr_for_issue(
    owner: &str,
    repo: &str,
    branch_name: &str,
    issue_num: &str,
    minion_id: &str,
    worktree_path: &Path,
) -> Result<String> {
    // Get the default branch (usually main or master)
    let base_branch_output = TokioCommand::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()
        .await
        .context("Failed to get default branch")?;

    let base_branch = if base_branch_output.status.success() {
        String::from_utf8_lossy(&base_branch_output.stdout)
            .trim()
            .trim_start_matches("refs/remotes/origin/")
            .to_string()
    } else {
        eprintln!("⚠️  Could not detect default branch, using 'main'");
        "main".to_string() // Fallback to main
    };

    println!("📌 Creating PR targeting base branch: {}", base_branch);

    // Get issue title from GitHub API
    let github_client = GitHubClient::from_env().context(
        "Failed to initialize GitHub client. Set GRU_GITHUB_TOKEN environment variable.",
    )?;

    let issue_number: u64 = issue_num.parse().context("Failed to parse issue number")?;

    let issue = github_client
        .get_issue(owner, repo, issue_number)
        .await
        .context("Failed to fetch issue from GitHub")?;

    let issue_title = issue.title;

    // Create PR title and body
    let pr_title = format!("[WIP] Fixes #{}: {}", issue_num, issue_title);
    let pr_body = format!(
        r#"🤖 This PR is being worked on by Minion {}

## Status
Work in progress - I'll update this when ready for review.

## Changes
- ✅ Initial implementation
- ⏳ Writing tests
- ⏳ Documentation

Fixes #{}"#,
        minion_id, issue_num
    );

    // Create the draft PR
    let pr_number = github_client
        .create_draft_pr(owner, repo, branch_name, &base_branch, &pr_title, &pr_body)
        .await
        .context("Failed to create draft PR")?;

    Ok(pr_number)
}

/// Parses a timeout string into a Duration
/// Supports formats like "10s", "5m", "1h", "30"
fn parse_timeout(timeout_str: &str) -> Result<Duration> {
    let timeout_str = timeout_str.trim();

    // Try to parse as plain seconds first
    if let Ok(secs) = timeout_str.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }

    // Parse with unit suffix
    if timeout_str.len() < 2 {
        anyhow::bail!(
            "Invalid timeout format: '{}'. Expected format: <number>[s|m|h]",
            timeout_str
        );
    }

    let (num_str, unit) = timeout_str.split_at(timeout_str.len() - 1);
    let num: u64 = num_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "Invalid timeout format: '{}'. Expected format: <number>[s|m|h]",
            timeout_str
        )
    })?;

    match unit {
        "s" => Ok(Duration::from_secs(num)),
        "m" => {
            let secs = num
                .checked_mul(60)
                .ok_or_else(|| anyhow::anyhow!("Timeout value too large"))?;
            Ok(Duration::from_secs(secs))
        }
        "h" => {
            let secs = num
                .checked_mul(3600)
                .ok_or_else(|| anyhow::anyhow!("Timeout value too large"))?;
            Ok(Duration::from_secs(secs))
        }
        _ => anyhow::bail!(
            "Invalid timeout unit: '{}'. Supported units: s (seconds), m (minutes), h (hours)",
            unit
        ),
    }
}

/// Handles the fix command by delegating to the Claude CLI
/// Returns the exit code from the claude process
pub async fn handle_fix(issue: &str, timeout_opt: Option<String>, quiet: bool) -> Result<i32> {
    // Parse issue information (auto-detects repo from current directory if plain number)
    let (owner, repo, issue_num) = parse_issue_info(issue)?;

    // Always generate a unique minion ID
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    // Create workspace and launch Claude
    println!(
        "🚀 Setting up workspace for {}/{}#{}",
        owner, repo, issue_num
    );

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

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

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = TokioCommand::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions")
        .arg(format!("/fix {}", issue_num))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(&worktree_path)
        .env("GRU_WORKSPACE", &minion_id);

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

    // Parse timeout if provided
    let max_timeout = if let Some(ref timeout_str) = timeout_opt {
        Some(parse_timeout(timeout_str)?)
    } else {
        None
    };

    // Track task start time for overall timeout
    let task_start = Instant::now();

    // Process stream output asynchronously with timeout and error handling
    let stream_result = async {
        let mut last_event_time = Instant::now();
        let mut warned_at_5min = false;

        loop {
            // Check overall task timeout
            if let Some(max_duration) = max_timeout {
                let elapsed = task_start.elapsed();
                if elapsed >= max_duration {
                    eprintln!("⏱️  Task timeout reached ({:?})", max_duration);
                    eprintln!("📝 Events saved to events.jsonl");
                    return Err(anyhow::anyhow!(
                        "Task exceeded maximum timeout of {:?}",
                        max_duration
                    ));
                }
            }

            // Check inactivity - time since last event (not overall time)
            let inactivity = last_event_time.elapsed();

            if inactivity.as_secs() >= INACTIVITY_STUCK_SECS {
                eprintln!(
                    "❌ Task appears stuck (no activity for {} minutes)",
                    INACTIVITY_STUCK_SECS / 60
                );
                eprintln!("📝 Events saved to events.jsonl");
                return Err(anyhow::anyhow!(
                    "No activity for {} minutes - task appears stuck",
                    INACTIVITY_STUCK_SECS / 60
                ));
            } else if inactivity.as_secs() >= INACTIVITY_WARNING_SECS && !warned_at_5min {
                eprintln!(
                    "⚠️  No activity for {} minutes",
                    INACTIVITY_WARNING_SECS / 60
                );
                warned_at_5min = true;
            }

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
                Some(output) => {
                    // Log the event to events.jsonl
                    log_event(&worktree_path, &output).await?;

                    // Update last event time for any output
                    last_event_time = Instant::now();

                    // Reset warning flag only on actual events (not raw output lines)
                    if matches!(output, stream::StreamOutput::Event(_)) {
                        warned_at_5min = false;
                    }

                    // Display progress
                    progress.handle_output(&output);
                }
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

        // Check if a PR should be created
        println!("\n🔍 Checking if branch was pushed...");
        let branch_pushed = is_branch_pushed(&worktree_path, &branch_name).await?;

        if branch_pushed {
            println!("📋 Branch was pushed, creating pull request...");

            match create_pr_for_issue(
                &owner,
                &repo,
                &branch_name,
                &issue_num,
                &minion_id,
                &worktree_path,
            )
            .await
            {
                Ok(pr_number) => {
                    // Save PR state
                    let pr_state = PrState::new(pr_number.clone(), issue_num.clone());
                    pr_state
                        .save(&worktree_path)
                        .context("Failed to save PR state")?;

                    println!("✅ Draft PR created: #{}", pr_number);
                    println!(
                        "🔗 View PR at: https://github.com/{}/{}/pull/{}",
                        owner, repo, pr_number
                    );
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("already exists")
                        || err_msg.contains("A pull request for branch")
                    {
                        eprintln!("ℹ️  A PR already exists for branch '{}'", branch_name);
                        eprintln!("   Check: https://github.com/{}/{}/pulls", owner, repo);
                    } else if err_msg.contains("branch not found")
                        || err_msg.contains("does not exist")
                    {
                        eprintln!("⚠️  Branch was pushed but is no longer available.");
                        eprintln!("   It may have been deleted or force-pushed.");
                    } else {
                        eprintln!("⚠️  Failed to create PR: {}", e);
                    }
                    eprintln!("   You can create the PR manually if needed.");
                }
            }
        } else {
            println!("ℹ️  Branch was not pushed. No PR will be created.");
            println!("   Push your changes with: git push origin {}", branch_name);
        }
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue));
    }

    // Return the exit code from the claude process
    Ok(status.code().unwrap_or(128))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeout_with_seconds() {
        assert_eq!(parse_timeout("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("1s").unwrap(), Duration::from_secs(1));
    }

    #[test]
    fn test_parse_timeout_with_minutes() {
        assert_eq!(parse_timeout("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_timeout("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_timeout("15m").unwrap(), Duration::from_secs(900));
    }

    #[test]
    fn test_parse_timeout_with_hours() {
        assert_eq!(parse_timeout("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_timeout("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_timeout_with_plain_number() {
        assert_eq!(parse_timeout("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("300").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_timeout_with_whitespace() {
        assert_eq!(parse_timeout(" 10s ").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_timeout("  5m  ").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_timeout_rejects_invalid_input() {
        assert!(parse_timeout("").is_err());
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("10x").is_err());
        assert!(parse_timeout("-10s").is_err());
        assert!(parse_timeout("s").is_err());
    }

    #[tokio::test]
    async fn test_is_branch_pushed_nonexistent() {
        use std::env;

        // Test with a non-existent directory
        let temp_dir = env::temp_dir().join("gru-test-nonexistent");
        let result = is_branch_pushed(&temp_dir, "test-branch").await;

        // Git command will fail, but we return Ok(false) to indicate branch is not pushed
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }
}
