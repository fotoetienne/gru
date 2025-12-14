use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::minion_registry::{MinionInfo as RegistryMinionInfo, MinionRegistry};
use crate::pr_monitor::{self, MonitorResult};
use crate::pr_state::PrState;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::progress_comments::{MinionPhase, ProgressCommentTracker};
use crate::stream::{self, EventStream};
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

/// Timeout in seconds for each line read from Claude's output stream
/// Set to 5 minutes to accommodate long-running LLM operations
const STREAM_TIMEOUT_SECS: u64 = 300;

/// Duration of inactivity before warning the user
const INACTIVITY_WARNING_SECS: u64 = 300; // 5 minutes

/// Duration of inactivity before considering the task stuck
const INACTIVITY_STUCK_SECS: u64 = 900; // 15 minutes

/// Maximum size of the output buffer for test detection (in bytes)
const MAX_OUTPUT_BUFFER_SIZE: usize = 10000;

/// Size to trim the output buffer to when it exceeds the maximum (in bytes)
const TRIM_OUTPUT_BUFFER_SIZE: usize = 5000;

/// Exit code returned when a process is terminated by a signal (shell convention)
const EXIT_CODE_SIGNAL_TERMINATED: i32 = 128;

/// Default timeout for review process in seconds (30 minutes)
/// Reviews can take longer than fixes due to analysis depth
const DEFAULT_REVIEW_TIMEOUT_SECS: u64 = 1800;

/// Maximum number of review rounds to handle automatically
/// After this limit, the user must handle additional reviews manually
const MAX_REVIEW_ROUNDS: usize = 5;

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

/// Helper function to create a WIP PR template
fn create_wip_template(minion_id: &str, issue_num: &str, issue_title: &str) -> (String, String) {
    let pr_title = format!("[WIP] Fixes #{}: {}", issue_num, issue_title);
    let pr_body = format!(
        r#"This PR is being worked on by Minion {}

## Status
Work in progress - I'll update this when ready for review.

## Changes
- [ ] Initial implementation
- [ ] Writing tests
- [ ] Documentation

Fixes #{}"#,
        minion_id, issue_num
    );
    (pr_title, pr_body)
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

    // Get issue title from GitHub API or CLI
    let issue_number: u64 = issue_num.parse().context("Failed to parse issue number")?;

    let issue_title = if let Some(github_client) = GitHubClient::try_from_env(owner, repo).await {
        // Use API if token is available
        let issue = github_client
            .get_issue(owner, repo, issue_number)
            .await
            .context("Failed to fetch issue from GitHub API")?;
        issue.title
    } else {
        // Fall back to gh CLI
        println!("ℹ️  No GitHub authentication found, using gh CLI for GitHub operations");
        let issue = crate::github::get_issue_via_cli(owner, repo, issue_number)
            .await
            .context(
                "Failed to fetch issue using gh CLI. Make sure gh is installed and authenticated.",
            )?;
        issue.title
    };

    // Check if work is complete (description file exists)
    let description_path = worktree_path.join("PR_DESCRIPTION.md");
    let should_mark_ready = match tokio::fs::try_exists(&description_path).await {
        Ok(exists) => exists,
        Err(e) => {
            eprintln!(
                "⚠️  Warning: Failed to check if PR_DESCRIPTION.md exists: {}",
                e
            );
            false
        }
    };

    let (pr_title, pr_body) = if should_mark_ready {
        // Read the description file
        match tokio::fs::read_to_string(&description_path).await {
            Ok(content) if !content.trim().is_empty() => {
                // Work is complete - use description and mark ready
                let pr_title = format!("Fixes #{}: {}", issue_num, issue_title);
                let pr_body = format!("{}\n\nFixes #{}", content.trim(), issue_num);
                (pr_title, pr_body)
            }
            _ => {
                // File exists but couldn't be read or is empty - treat as WIP
                eprintln!("⚠️  Warning: PR_DESCRIPTION.md exists but couldn't be read or is empty");
                create_wip_template(minion_id, issue_num, &issue_title)
            }
        }
    } else {
        // No description file - work in progress
        create_wip_template(minion_id, issue_num, &issue_title)
    };

    // Create the draft PR using gh CLI (PR operations always use CLI)
    let pr_number = crate::github::create_draft_pr_via_cli(
        owner,
        repo,
        branch_name,
        &base_branch,
        &pr_title,
        &pr_body,
    )
    .await
    .context("Failed to create draft PR using gh CLI")?;

    // Mark ready if description was provided
    if should_mark_ready {
        // Use GitHub client to mark PR ready
        if let Some(github_client) = GitHubClient::try_from_env(owner, repo).await {
            match github_client.mark_pr_ready(owner, repo, &pr_number).await {
                Ok(_) => {
                    println!("✅ PR #{} marked ready for review", pr_number);
                }
                Err(e) => {
                    eprintln!("⚠️  Warning: Failed to mark PR ready: {}", e);
                    eprintln!(
                        "   PR #{} created as draft - you can mark it ready manually",
                        pr_number
                    );
                }
            }
        } else {
            eprintln!(
                "⚠️  Warning: No GitHub authentication found - cannot mark PR ready automatically"
            );
            eprintln!(
                "   You can mark PR #{} ready with: gh pr ready {}",
                pr_number, pr_number
            );
        }

        // Clean up description file
        if let Err(e) = tokio::fs::remove_file(&description_path).await {
            eprintln!("⚠️  Warning: Failed to remove PR_DESCRIPTION.md: {}", e);
            eprintln!("   File will be cleaned up by 'gru clean'");
        }
    }

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

/// Invokes Claude to address review comments
///
/// This function spawns Claude with a custom prompt containing review comments.
/// It reuses the same session ID to maintain context from the original implementation.
///
/// This function handles timeout detection and inactivity monitoring. If a timeout is specified
/// via the `timeout_opt` parameter, the function will terminate if the operation exceeds the
/// given duration. The function also monitors for inactivity (no output) and will fail if there
/// is no activity for an extended period (15 minutes by default).
async fn invoke_claude_for_reviews(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
) -> Result<()> {
    // Build the command with flags for non-interactive stream-json output
    let mut cmd = TokioCommand::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--session-id")
        .arg(session_id.to_string())
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions")
        .arg(prompt)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path);

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
    let max_timeout = if let Some(timeout_str) = timeout_opt {
        Some(parse_timeout(timeout_str)?)
    } else {
        None
    };

    // Track task start time for overall timeout
    let task_start = Instant::now();

    // Process stream output asynchronously with timeout and error handling
    let stream_result = async {
        let mut last_event_time = Instant::now();
        let mut inactivity_warning_shown = false;

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

            // Check inactivity - time since last event
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
            } else if inactivity.as_secs() >= INACTIVITY_WARNING_SECS && !inactivity_warning_shown {
                eprintln!(
                    "⚠️  No activity for {} minutes",
                    INACTIVITY_WARNING_SECS / 60
                );
                inactivity_warning_shown = true;
            }

            // Try to read next line with timeout
            let line_result = timeout(Duration::from_secs(STREAM_TIMEOUT_SECS), stream.next_line())
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Timeout: Process hasn't produced output in {} seconds",
                        STREAM_TIMEOUT_SECS
                    )
                })?;

            // Handle the stream result
            match line_result? {
                Some(output) => {
                    // Log the event to events.jsonl
                    log_event(worktree_path, &output).await?;

                    // Update last event time for any output
                    last_event_time = Instant::now();

                    // Reset warning flag only on actual events (not raw output lines)
                    if matches!(output, stream::StreamOutput::Event(_)) {
                        inactivity_warning_shown = false;
                    }
                }
                None => {
                    // Stream ended normally
                    break;
                }
            }
        }

        Ok(())
    }
    .await;

    // Handle stream errors
    if let Err(e) = stream_result {
        let _ = child.kill().await;
        return Err(e);
    }

    // Wait for the child process to finish
    let status = child.wait().await.context("Failed to wait for child")?;

    if !status.success() {
        let exit_code = status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        return Err(anyhow::anyhow!(
            "Review response process exited with code {}",
            exit_code
        ));
    }

    Ok(())
}

/// Triggers an automated PR review by spawning a separate gru review command
///
/// This function spawns `gru review` as a completely separate process with a fresh
/// Claude session context. This ensures the review is unbiased and not influenced
/// by the implementation process.
///
/// # Arguments
/// * `pr_number` - The PR number to review (must be a valid number)
/// * `worktree_path` - Path to the worktree where the review should run
/// * `review_timeout` - Optional timeout duration for the review process
///
/// # Returns
/// The exit code from the review process (0 for success, non-zero for failure)
///
/// # Errors
/// Returns an error if:
/// - The PR number is not a valid numeric value
/// - The gru command fails to spawn (e.g., not in PATH)
/// - The process cannot be waited on
/// - The review process exceeds the timeout
async fn trigger_pr_review(
    pr_number: &str,
    worktree_path: &Path,
    review_timeout: Option<Duration>,
) -> Result<i32> {
    // Validate PR number format (defense in depth)
    pr_number
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_number))?;

    // Use provided timeout or default
    let timeout_duration =
        review_timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_REVIEW_TIMEOUT_SECS));

    // Spawn gru review as a separate process
    let mut child = TokioCommand::new("gru")
        .arg("review")
        .arg(pr_number)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn gru review command for PR #{}. Is gru in your PATH?",
                pr_number
            )
        })?;

    // Wait for the process with timeout
    match timeout(timeout_duration, child.wait()).await {
        Ok(status) => {
            let status = status.with_context(|| {
                format!("Failed to wait for review process for PR #{}", pr_number)
            })?;
            Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
        }
        Err(_) => {
            // Timeout occurred - kill the child process to prevent orphaned process
            let _ = child.kill().await; // Ignore kill errors, already timing out
            Err(anyhow::anyhow!(
                "Review process timed out after {} minutes. PR #{} review may be stuck.",
                timeout_duration.as_secs() / 60,
                pr_number
            ))
        }
    }
}

/// Helper function to post a progress comment to a GitHub issue
/// Returns true if the comment was posted successfully, false otherwise
async fn try_post_progress_comment(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    issue_num: &str,
    comment_body: &str,
) -> bool {
    // Parse issue number to u64
    match issue_num.parse::<u64>() {
        Ok(issue_num_u64) => {
            // Post comment (fire and forget - don't block on errors)
            match client
                .post_comment(owner, repo, issue_num_u64, comment_body)
                .await
            {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("⚠️  Failed to post progress comment: {}", e);
                    false
                }
            }
        }
        Err(_) => {
            eprintln!("⚠️  Invalid issue number format: {}", issue_num);
            false
        }
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

    // Register the Minion in the registry
    let issue_num_u64: u64 = issue_num.parse().context("Failed to parse issue number")?;
    let registry_info = RegistryMinionInfo {
        repo: repo_name.clone(),
        issue: issue_num_u64,
        command: "fix".to_string(),
        prompt: format!("/fix {}", issue_num),
        started_at: Utc::now(),
        branch: branch_name.clone(),
        worktree: worktree_path.clone(),
        status: "active".to_string(),
        pr: None,
    };

    // Load registry and register the Minion (spawn_blocking to avoid blocking the async runtime)
    let minion_id_clone = minion_id.clone();
    tokio::task::spawn_blocking(move || {
        let mut registry = MinionRegistry::load(None)?;
        registry.register(minion_id_clone, registry_info)
    })
    .await
    .context("Failed to spawn blocking task for registry registration")??;

    println!("📝 Registered Minion {} in registry", minion_id);
    println!("🤖 Launching Claude...\n");

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Initialize progress comment tracker
    let mut progress_tracker = ProgressCommentTracker::new(minion_id.clone());

    // Initialize GitHub client (optional - only if token is available)
    let github_client = GitHubClient::from_env(&owner, &repo).await.ok();
    if github_client.is_none() {
        println!("⚠️  No GitHub authentication found - progress comments will not be posted");
    }

    // Claim the issue by adding in-progress label (fire-and-forget)
    if let Some(ref client) = github_client {
        if let Ok(issue_number) = issue_num.parse::<u64>() {
            match client.claim_issue(&owner, &repo, issue_number).await {
                Ok(true) => {
                    println!("🏷️  Added 'in-progress' label to issue #{}", issue_num);
                }
                Ok(false) => {
                    eprintln!(
                        "⚠️  Issue #{} is already claimed by another Minion",
                        issue_num
                    );
                    eprintln!("   This may indicate a race condition or multiple gru instances.");
                    eprintln!("   Continuing anyway (worktree already created)...");
                }
                Err(e) => {
                    eprintln!("⚠️  Failed to add label to issue: {}", e);
                    eprintln!("   Continuing anyway...");
                }
            }
        } else {
            eprintln!(
                "⚠️  Invalid issue number '{}', cannot update labels",
                issue_num
            );
        }
    }

    // Generate a unique session ID for conversation continuity
    let session_id = Uuid::new_v4();

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = TokioCommand::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--session-id")
        .arg(session_id.to_string())
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
        let mut inactivity_warning_shown = false;
        let mut raw_output_buffer = String::new();

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
            } else if inactivity.as_secs() >= INACTIVITY_WARNING_SECS && !inactivity_warning_shown {
                eprintln!(
                    "⚠️  No activity for {} minutes",
                    INACTIVITY_WARNING_SECS / 60
                );
                inactivity_warning_shown = true;
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
                        inactivity_warning_shown = false;
                    }

                    // Buffer raw output for test detection
                    if let stream::StreamOutput::RawLine(ref line) = output {
                        raw_output_buffer.push_str(line);
                        // Keep buffer size reasonable
                        if raw_output_buffer.len() > MAX_OUTPUT_BUFFER_SIZE {
                            // Ensure we split at a valid UTF-8 character boundary to avoid panics
                            // Find the largest valid UTF-8 boundary at or before TRIM_OUTPUT_BUFFER_SIZE
                            let mut trim_pos = TRIM_OUTPUT_BUFFER_SIZE;
                            while trim_pos > 0 && !raw_output_buffer.is_char_boundary(trim_pos) {
                                trim_pos -= 1;
                            }
                            raw_output_buffer = raw_output_buffer.split_off(trim_pos);
                        }
                    }

                    // Display progress
                    progress.handle_output(&output);

                    // Check for milestones and update tracker
                    if let stream::StreamOutput::RawLine(ref line) = output {
                        // Detect phase transitions from output and post progress comments
                        let previous_phase = progress_tracker.current_phase();
                        let mut phase_changed = false;

                        if line.contains("Plan") || line.contains("plan") {
                            if previous_phase != MinionPhase::Planning {
                                progress_tracker.set_phase(MinionPhase::Planning);
                                phase_changed = true;
                            }
                        } else if (line.contains("Implement") || line.contains("Writing"))
                            && previous_phase != MinionPhase::Implementing
                        {
                            progress_tracker.set_phase(MinionPhase::Implementing);
                            phase_changed = true;
                        } else if (line.contains("test") || line.contains("Test"))
                            && previous_phase != MinionPhase::Testing
                        {
                            progress_tracker.set_phase(MinionPhase::Testing);
                            phase_changed = true;
                        }

                        // Post progress comment if phase changed and rate limiting allows
                        if phase_changed {
                            if let Some(ref client) = github_client {
                                if progress_tracker.can_post_comment() {
                                    let message = format!(
                                        "Now in {} phase.",
                                        progress_tracker.current_phase().as_str()
                                    );
                                    let update = progress_tracker.create_update(message);
                                    let comment_body = update.format_comment();

                                    if try_post_progress_comment(
                                        client,
                                        &owner,
                                        &repo,
                                        &issue_num,
                                        &comment_body,
                                    )
                                    .await
                                    {
                                        progress_tracker.mark_comment_posted();
                                    }
                                }
                            }
                        }
                    }
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

    // Post final completion comment
    if let Some(ref client) = github_client {
        progress_tracker.set_phase(MinionPhase::Completed);

        let final_message = if status.success() {
            "✅ Task completed successfully!".to_string()
        } else {
            "❌ Task failed.".to_string()
        };

        let update = progress_tracker.create_update(final_message);
        let comment_body = update.format_comment();

        // Post final comment (ignore rate limiting for completion)
        try_post_progress_comment(client, &owner, &repo, &issue_num, &comment_body).await;
    }

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

                    // Update registry with PR number (spawn_blocking to avoid blocking)
                    let minion_id_clone = minion_id.clone();
                    let pr_number_clone = pr_number.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut registry = MinionRegistry::load(None)?;
                        registry.update(&minion_id_clone, |info| {
                            info.pr = Some(pr_number_clone);
                            info.status = "idle".to_string();
                        })
                    })
                    .await
                    .context("Failed to spawn blocking task for registry update")??;

                    println!("✅ Draft PR created: #{}", pr_number);
                    println!(
                        "🔗 View PR at: https://github.com/{}/{}/pull/{}",
                        owner, repo, pr_number
                    );

                    // Mark issue as done (fire-and-forget)
                    if let Some(ref client) = github_client {
                        if let Ok(issue_number) = issue_num.parse::<u64>() {
                            match client.mark_issue_done(&owner, &repo, issue_number).await {
                                Ok(()) => {
                                    println!("🏷️  Updated issue label to 'minion:done'");
                                }
                                Err(e) => {
                                    eprintln!("⚠️  Failed to update issue label: {}", e);
                                }
                            }
                        } else {
                            eprintln!(
                                "⚠️  Invalid issue number '{}', cannot update labels",
                                issue_num
                            );
                        }
                    }

                    // Auto-trigger review for Minion-created PRs
                    println!("\n🔍 Starting automated PR review...");
                    match trigger_pr_review(&pr_number, &worktree_path, None).await {
                        Ok(review_exit_code) => {
                            if review_exit_code == 0 {
                                println!("✅ PR review completed successfully");
                            } else {
                                eprintln!(
                                    "⚠️  PR review completed with exit code: {}",
                                    review_exit_code
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("⚠️  Failed to run PR review: {}", e);
                            eprintln!("   You can review manually with: gru review {}", pr_number);
                        }
                    }

                    // Start monitoring the PR for review comments, CI failures, and merge/close events
                    println!("\n👀 Monitoring PR for updates (polling every 30s)...");
                    println!("   Press Ctrl+C to stop monitoring\n");

                    // Keep monitoring in a loop to handle multiple rounds of reviews
                    let mut review_round = 0;
                    loop {
                        match pr_monitor::monitor_pr(&owner, &repo, &pr_number, &worktree_path)
                            .await
                        {
                            Ok(MonitorResult::Merged) => {
                                println!("✅ PR #{} was merged successfully!", pr_number);
                                println!("🎉 Issue {} is complete!", issue_num);
                                break;
                            }
                            Ok(MonitorResult::Closed) => {
                                println!("⚠️  PR #{} was closed without merging", pr_number);
                                println!(
                                    "   The issue may need to be reopened or addressed differently"
                                );
                                break;
                            }
                            Ok(MonitorResult::NewReviews(comments)) => {
                                review_round += 1;
                                let count = comments.len();
                                println!(
                                    "💬 Detected {} new review comment(s) on PR #{} (review round {}/{})",
                                    count, pr_number, review_round, MAX_REVIEW_ROUNDS
                                );

                                // Check if we've hit the maximum review rounds limit
                                if review_round > MAX_REVIEW_ROUNDS {
                                    println!(
                                        "⚠️  Reached maximum review rounds limit ({})",
                                        MAX_REVIEW_ROUNDS
                                    );
                                    println!("   Additional reviews will need manual handling");
                                    println!(
                                        "   View PR: https://github.com/{}/{}/pull/{}",
                                        owner, repo, pr_number
                                    );
                                    break;
                                }

                                // Format the review prompt with detailed comments
                                let issue_number = match issue_num.parse::<u64>() {
                                    Ok(num) => num,
                                    Err(e) => {
                                        eprintln!(
                                            "⚠️  Failed to parse issue number '{}': {}",
                                            issue_num, e
                                        );
                                        eprintln!("   Cannot format review prompt without a valid issue number");
                                        break;
                                    }
                                };

                                let review_prompt = pr_monitor::format_review_prompt(
                                    issue_number,
                                    &pr_number,
                                    &comments,
                                );

                                println!("🔄 Re-invoking to address review feedback...\n");

                                // Re-invoke Claude with the same session ID to maintain context
                                match invoke_claude_for_reviews(
                                    &worktree_path,
                                    &session_id,
                                    &review_prompt,
                                    timeout_opt.as_deref(),
                                )
                                .await
                                {
                                    Ok(()) => {
                                        println!("\n✅ Finished addressing review comments");
                                        println!("🔄 Continuing to monitor PR...\n");
                                        // Continue monitoring for more reviews
                                        //
                                        // Note: monitor_pr re-initializes last_check_time from existing reviews
                                        // on each call, so it will pick up from the most recent review timestamp.
                                        // This mitigates (but doesn't completely eliminate) the race condition
                                        // where the same reviews could be re-detected. A more robust solution
                                        // would track processed review IDs or return last_check_time from monitor_pr.
                                    }
                                    Err(e) => {
                                        eprintln!("⚠️  Failed to address review comments: {}", e);
                                        eprintln!("   You can address them manually");
                                        break;
                                    }
                                }
                            }
                            Ok(MonitorResult::FailedChecks(count)) => {
                                println!(
                                    "❌ Detected {} failed CI check(s) on PR #{}",
                                    count, pr_number
                                );
                                println!(
                                    "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                                    owner, repo, pr_number
                                );
                                println!("   Fix issues and push updates to the branch");
                                break;
                            }
                            Err(e) => {
                                eprintln!("⚠️  PR monitoring failed: {}", e);
                                eprintln!(
                                    "   You can monitor manually at: https://github.com/{}/{}/pull/{}",
                                    owner, repo, pr_number
                                );
                                break;
                            }
                        }
                    }
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

        // Mark issue as failed (fire-and-forget)
        if let Some(ref client) = github_client {
            if let Ok(issue_number) = issue_num.parse::<u64>() {
                match client.mark_issue_failed(&owner, &repo, issue_number).await {
                    Ok(()) => {
                        println!("🏷️  Updated issue label to 'minion:failed'");
                    }
                    Err(e) => {
                        eprintln!("⚠️  Failed to update issue label: {}", e);
                    }
                }
            } else {
                eprintln!(
                    "⚠️  Invalid issue number '{}', cannot update labels",
                    issue_num
                );
            }
        }
    }

    // Return the exit code from the claude process
    Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
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
