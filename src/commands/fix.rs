use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::minion_registry::{MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry};
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

/// Builds a standard Claude command with common flags
///
/// This helper creates a TokioCommand configured for non-interactive stream-json output.
/// Callers can further customize the command before passing it to run_claude_with_stream_monitoring.
fn build_claude_command(worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand {
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
    cmd
}

/// Builds a Claude command to resume an existing session
///
/// This is used when continuing a conversation from a previous session,
/// such as when addressing review comments after the initial fix.
/// Uses --resume instead of --session-id to avoid "session already in use" errors.
fn build_claude_resume_command(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
) -> TokioCommand {
    let mut cmd = TokioCommand::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--resume")
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
    cmd
}

/// Runs Claude with stream monitoring and timeout detection
///
/// Returns the exit status for the caller to inspect. Caller is responsible for
/// checking if the process succeeded and handling errors appropriately.
///
/// `on_spawn` is called with the child PID immediately after the process is spawned,
/// allowing callers to record the PID (e.g., in the Minion registry) for status tracking.
async fn run_claude_with_stream_monitoring<F>(
    mut cmd: TokioCommand,
    worktree_path: &Path,
    timeout_opt: Option<&str>,
    mut output_callback: Option<F>,
    on_spawn: Option<Box<dyn FnOnce(u32) + Send>>,
) -> Result<std::process::ExitStatus>
where
    F: FnMut(&stream::StreamOutput),
{
    // Spawn the command
    let mut child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    // Report the child PID to the caller if a callback was provided.
    // The callback fires a spawn_blocking task for registry I/O.
    if let Some(callback) = on_spawn {
        if let Some(pid) = child.id() {
            callback(pid);
        }
    }

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
                    log::info!("⏱️  Task timeout reached ({:?})", max_duration);
                    log::info!("📝 Events saved to events.jsonl");
                    return Err(anyhow::anyhow!(
                        "Task exceeded maximum timeout of {:?}",
                        max_duration
                    ));
                }
            }

            // Check inactivity - time since last event
            let inactivity = last_event_time.elapsed();

            if inactivity.as_secs() >= INACTIVITY_STUCK_SECS {
                log::error!(
                    "❌ Task appears stuck (no activity for {} minutes)",
                    INACTIVITY_STUCK_SECS / 60
                );
                log::info!("📝 Events saved to events.jsonl");
                return Err(anyhow::anyhow!(
                    "No activity for {} minutes - task appears stuck",
                    INACTIVITY_STUCK_SECS / 60
                ));
            } else if inactivity.as_secs() >= INACTIVITY_WARNING_SECS && !inactivity_warning_shown {
                log::warn!(
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

                    // Call custom callback if provided
                    if let Some(ref mut callback) = output_callback {
                        callback(&output);
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

    // Always wait for the process
    let status = child.wait().await.context("Failed to wait for child")?;

    // Now check if there was a stream error
    stream_result?;

    // Return the exit status for caller to handle
    Ok(status)
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
    issue_title_opt: Option<&str>,
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
        log::warn!("⚠️  Could not detect default branch, using 'main'");
        "main".to_string() // Fallback to main
    };

    println!("📌 Creating PR targeting base branch: {}", base_branch);

    // Get issue title - use provided title if available, otherwise fetch
    let issue_title = if let Some(title) = issue_title_opt {
        title.to_string()
    } else {
        // Fetch issue title if not provided
        let issue_number: u64 = issue_num.parse().context("Failed to parse issue number")?;

        if let Some(github_client) = GitHubClient::try_from_env(owner, repo).await {
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
        }
    };

    // Check if work is complete (description file exists)
    let description_path = worktree_path.join("PR_DESCRIPTION.md");
    let should_mark_ready = match tokio::fs::try_exists(&description_path).await {
        Ok(exists) => exists,
        Err(e) => {
            log::warn!(
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
                log::warn!(
                    "⚠️  Warning: PR_DESCRIPTION.md exists but couldn't be read or is empty"
                );
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
                    log::warn!("⚠️  Warning: Failed to mark PR ready: {}", e);
                    log::warn!(
                        "   PR #{} created as draft - you can mark it ready manually",
                        pr_number
                    );
                }
            }
        } else {
            log::warn!(
                "⚠️  Warning: No GitHub authentication found - cannot mark PR ready automatically"
            );
            log::warn!(
                "   You can mark PR #{} ready with: gh pr ready {}",
                pr_number,
                pr_number
            );
        }

        // Clean up description file
        if let Err(e) = tokio::fs::remove_file(&description_path).await {
            log::warn!("⚠️  Warning: Failed to remove PR_DESCRIPTION.md: {}", e);
            log::warn!("   File will be cleaned up by 'gru clean'");
        }
    }

    Ok(pr_number)
}

/// Handles the result of fetching issue details via CLI
/// Returns Some with (title, body, empty labels) on success, None on failure
async fn handle_cli_fetch_result(
    result: Result<crate::github::IssueInfo>,
) -> Option<(String, String, String)> {
    match result {
        Ok(issue_info) => {
            // CLI version doesn't include labels, but we can still provide title and body
            let body = issue_info.body.unwrap_or_default();
            Some((issue_info.title, body, String::new()))
        }
        Err(e) => {
            eprintln!("⚠️  Failed to fetch issue details via CLI: {}", e);
            eprintln!("   Continuing with basic prompt format");
            None
        }
    }
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
async fn invoke_claude_for_reviews(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
) -> Result<()> {
    let cmd = build_claude_resume_command(worktree_path, session_id, prompt);
    let status = run_claude_with_stream_monitoring(
        cmd,
        worktree_path,
        timeout_opt,
        None::<fn(&stream::StreamOutput)>,
        None,
    )
    .await?;

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
                    log::warn!("⚠️  Failed to post progress comment: {}", e);
                    false
                }
            }
        }
        Err(_) => {
            log::warn!("⚠️  Invalid issue number format: {}", issue_num);
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

    // Generate a unique session ID for conversation continuity
    let session_id = Uuid::new_v4();

    // Register the Minion in the registry
    let issue_num_u64: u64 = issue_num.parse().context("Failed to parse issue number")?;
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: repo_name.clone(),
        issue: issue_num_u64,
        command: "fix".to_string(),
        prompt: format!("/fix {}", issue_num),
        started_at: now,
        branch: branch_name.clone(),
        worktree: worktree_path.clone(),
        status: "active".to_string(),
        pr: None,
        session_id: session_id.to_string(),
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
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
        issue: issue_num.to_string(),
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
                    log::warn!(
                        "⚠️  Issue #{} is already claimed by another Minion",
                        issue_num
                    );
                    log::warn!("   This may indicate a race condition or multiple gru instances.");
                    log::warn!("   Continuing anyway (worktree already created)...");
                }
                Err(e) => {
                    log::warn!("⚠️  Failed to add label to issue: {}", e);
                    log::warn!("   Continuing anyway...");
                }
            }
        } else {
            log::warn!(
                "⚠️  Invalid issue number '{}', cannot update labels",
                issue_num
            );
        }
    }

    // Parse issue number once for reuse
    let issue_number_opt = issue_num.parse::<u64>().ok();

    // Fetch issue details to include in the prompt
    let issue_details = if let Some(issue_number) = issue_number_opt {
        if let Some(ref client) = github_client {
            // Try to fetch using API
            match client.get_issue(&owner, &repo, issue_number).await {
                Ok(issue) => {
                    let labels = issue
                        .labels
                        .iter()
                        .map(|l| l.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let body = issue.body.unwrap_or_default();
                    Some((issue.title, body, labels))
                }
                Err(e) => {
                    eprintln!("⚠️  Failed to fetch issue details via API: {}", e);
                    eprintln!("   Falling back to gh CLI...");
                    // Try CLI fallback
                    let cli_result =
                        crate::github::get_issue_via_cli(&owner, &repo, issue_number).await;
                    let result = handle_cli_fetch_result(cli_result).await;

                    // Only show auth guidance if both API and CLI failed
                    if result.is_none() {
                        eprintln!("   Fix authentication with: gh auth login");
                    }
                    result
                }
            }
        } else {
            // No API client - try CLI directly
            let cli_result = crate::github::get_issue_via_cli(&owner, &repo, issue_number).await;
            let result = handle_cli_fetch_result(cli_result).await;

            // Show auth guidance when CLI fails and we have no API client
            if result.is_none() {
                eprintln!("   Fix authentication with: gh auth login");
            }
            result
        }
    } else {
        None
    };

    // Format the prompt with issue details
    let prompt = if let Some((title, body, labels)) = issue_details.as_ref() {
        let labels_section = if labels.is_empty() {
            String::new()
        } else {
            format!("\nLabels: {}", labels)
        };

        format!(
            r#"
# Issue #{}: {}

URL: https://github.com/{}/{}/issues/{}{}

## Description:
{}

# Instructions

## 1. Check if Decomposition is Needed
- Assess the issue's complexity:
  - Does it involve multiple distinct components or systems?
  - Does it have multiple acceptance criteria?
  - Would it take more than a few hours to complete?
  - Does it mix different types of work (backend + frontend + docs)?

- **If the issue is complex and should be broken down:**
  - Recommend to the user: "This issue seems complex. Run `/decompose $ARGUMENTS` to break it into smaller sub-issues first."
  - Stop the fix workflow here - wait for user to decompose

- **If the issue is focused and ready to fix:**
  - Proceed to the next step

## 2. Plan the Fix
- Explore the codebase to understand the relevant code
- Create a detailed plan using TodoWrite with specific steps to fix the issue
- Consider tests that need to be added or updated

## 3. Implement the Fix
- Work through each todo item
- Write clean, minimal code changes
- Add or update tests as needed
- Check CLAUDE.md for project-specific build/test commands
- Run tests to verify the fix

## 4. Code Review
- Make a commit with the changes
- Use the Task tool with `subagent_type='code-reviewer'` to perform an autonomous code review
- The code-reviewer agent will analyze the changes for:
  - Code correctness and logic errors
  - Security vulnerabilities
  - Error handling gaps
  - Edge cases
  - Adherence to project conventions (check CLAUDE.md)
  - Test coverage
- Address any issues raised by the code-reviewer before proceeding
- If the review identifies significant problems, iterate on the implementation

## 5. Finish Your Work

When your implementation is complete and ready for human review:

1. **Commit your implementation changes** with a descriptive commit message
2. **Push the branch** to the remote repository
3. Write `PR_DESCRIPTION.md` in the root of the repository with this format:
   ```markdown
   ## Summary
   - Key change 1
   - Key change 2

   ## Test plan
   - How you tested this
   - Commands run: cargo test, just check, etc.

   ## Notes
   - Context reviewers should know
   - Follow-up work if any
   ```

**DO NOT commit PR_DESCRIPTION.md** - Gru will read this file locally from your worktree, use it to create the PR description, mark the PR ready, and then delete it automatically.

**IMPORTANT:** Only write `PR_DESCRIPTION.md` when work is truly complete and ready for human review. If work is still in progress, don't create this file - Gru will create a draft PR instead.

## 6. Iterate on Feedback
- Look at CI check results
- Address any issues raised by the CI checks
- Read review comments
- Determine which comments require changes. Sometimes reviewers are wrong!
- Make the necessary changes
- For any comments that you've determined don't require changes, acknowledge them
- Make a reply that addresses each comment and includes a summary of the changes made
- Repeat until the PR is ready to merge
"#,
            issue_num, title, owner, repo, issue_num, labels_section, body
        )
    } else {
        // Fall back to simple prompt if we couldn't fetch issue details
        format!("/fix {}", issue_num)
    };

    // Build the command with custom environment variable for main fix flow
    let mut cmd = build_claude_command(&worktree_path, &session_id, &prompt);
    cmd.env("GRU_WORKSPACE", &minion_id);

    // Create state for the callback
    // Note: GitHub comment posting is omitted from the callback because it requires async operations
    // which aren't easily supported in the synchronous callback. The refactoring trades
    // this feature for code simplification and DRY principles.
    struct CallbackState<'a> {
        raw_output_buffer: String,
        progress: &'a ProgressDisplay,
        progress_tracker: &'a mut ProgressCommentTracker,
    }

    let mut callback_state = CallbackState {
        raw_output_buffer: String::new(),
        progress: &progress,
        progress_tracker: &mut progress_tracker,
    };

    let callback = |output: &stream::StreamOutput| {
        // Buffer raw output for test detection
        if let stream::StreamOutput::RawLine(ref line) = output {
            callback_state.raw_output_buffer.push_str(line);
            // Keep buffer size reasonable
            if callback_state.raw_output_buffer.len() > MAX_OUTPUT_BUFFER_SIZE {
                // Ensure we split at a valid UTF-8 character boundary to avoid panics
                // Find the largest valid UTF-8 boundary at or before TRIM_OUTPUT_BUFFER_SIZE
                let mut trim_pos = TRIM_OUTPUT_BUFFER_SIZE;
                while trim_pos > 0 && !callback_state.raw_output_buffer.is_char_boundary(trim_pos) {
                    trim_pos -= 1;
                }
                callback_state.raw_output_buffer =
                    callback_state.raw_output_buffer.split_off(trim_pos);
            }
        }

        // Display progress
        callback_state.progress.handle_output(output);

        // Check for milestones and update tracker
        if let stream::StreamOutput::RawLine(ref line) = output {
            // Detect phase transitions from output
            let previous_phase = callback_state.progress_tracker.current_phase();

            if line.contains("Plan") || line.contains("plan") {
                if previous_phase != MinionPhase::Planning {
                    callback_state
                        .progress_tracker
                        .set_phase(MinionPhase::Planning);
                }
            } else if (line.contains("Implement") || line.contains("Writing"))
                && previous_phase != MinionPhase::Implementing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Implementing);
            } else if (line.contains("test") || line.contains("Test"))
                && previous_phase != MinionPhase::Testing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Testing);
            }
        }
    };

    // Build on_spawn callback to record the child PID in the registry.
    // Uses spawn_blocking (fire-and-forget) to avoid blocking the async executor.
    let pid_minion_id = minion_id.clone();
    let on_spawn: Box<dyn FnOnce(u32) + Send> = Box::new(move |pid: u32| {
        tokio::task::spawn_blocking(move || {
            if let Ok(mut registry) = MinionRegistry::load(None) {
                let _ = registry.update(&pid_minion_id, |info| {
                    info.pid = Some(pid);
                    info.last_activity = Utc::now();
                });
            }
        });
    });

    // Run Claude with stream monitoring and get the exit status.
    // Store the result to ensure cleanup runs even on error paths.
    let run_result = run_claude_with_stream_monitoring(
        cmd,
        &worktree_path,
        timeout_opt.as_deref(),
        Some(callback),
        Some(on_spawn),
    )
    .await;

    // Always clear PID and set mode to Stopped, regardless of success or error.
    // This prevents stale PIDs from lingering in the registry after timeouts/crashes.
    let exit_minion_id = minion_id.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(mut registry) = MinionRegistry::load(None) {
            let _ = registry.update(&exit_minion_id, |info| {
                info.pid = None;
                info.mode = MinionMode::Stopped;
            });
        }
    })
    .await
    .context("Failed to update registry after process exit")?;

    // Now propagate the original error if the stream monitoring failed
    let status = run_result?;

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

            // Pass the cached issue title to avoid duplicate fetching
            let issue_title_cached = issue_details.as_ref().map(|(title, _, _)| title.as_str());

            match create_pr_for_issue(
                &owner,
                &repo,
                &branch_name,
                &issue_num,
                &minion_id,
                &worktree_path,
                issue_title_cached,
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
                                    log::warn!("⚠️  Failed to update issue label: {}", e);
                                }
                            }
                        } else {
                            log::warn!(
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
                                log::warn!(
                                    "⚠️  PR review completed with exit code: {}",
                                    review_exit_code
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("⚠️  Failed to run PR review: {}", e);
                            log::warn!("   You can review manually with: gru review {}", pr_number);
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
                                        log::warn!(
                                            "⚠️  Failed to parse issue number '{}': {}",
                                            issue_num,
                                            e
                                        );
                                        log::warn!("   Cannot format review prompt without a valid issue number");
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
                                        log::warn!("⚠️  Failed to address review comments: {}", e);
                                        log::warn!("   You can address them manually");
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
                                log::warn!("⚠️  PR monitoring failed: {}", e);
                                log::warn!(
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
                        log::info!("ℹ️  A PR already exists for branch '{}'", branch_name);
                        log::info!("   Check: https://github.com/{}/{}/pulls", owner, repo);
                    } else if err_msg.contains("branch not found")
                        || err_msg.contains("does not exist")
                    {
                        log::warn!("⚠️  Branch was pushed but is no longer available.");
                        log::warn!("   It may have been deleted or force-pushed.");
                    } else {
                        log::warn!("⚠️  Failed to create PR: {}", e);
                    }
                    log::warn!("   You can create the PR manually if needed.");
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
                        log::warn!("⚠️  Failed to update issue label: {}", e);
                    }
                }
            } else {
                log::warn!(
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
