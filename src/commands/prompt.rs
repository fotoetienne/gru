use crate::minion;
use crate::minion_registry::{MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry};
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::stream::{self, EventStream};
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

/// Exit code returned when a process is terminated by a signal (shell convention)
const EXIT_CODE_SIGNAL_TERMINATED: i32 = 128;

/// Logs an event to events.jsonl in the workspace directory
async fn log_event(workspace_path: &Path, event: &stream::StreamOutput) -> Result<()> {
    let events_file = workspace_path.join("events.jsonl");
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

/// Handles the prompt command by launching Claude with an ad-hoc prompt
/// Returns the exit code from the claude process
pub async fn handle_prompt(prompt: &str, timeout_opt: Option<String>, quiet: bool) -> Result<i32> {
    // Validate prompt doesn't start with flags (security check)
    let trimmed_prompt = prompt.trim();
    if trimmed_prompt.starts_with('-') {
        anyhow::bail!(
            "Prompt cannot start with '-' (looks like a command flag). \
             Use quotes around your prompt: gru prompt \"your prompt here\""
        );
    }

    if trimmed_prompt.is_empty() {
        anyhow::bail!("Prompt cannot be empty");
    }

    // Generate a unique minion ID for session tracking
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("🆔 Session: {}", minion_id);

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create workspace path for ad-hoc prompts: ~/.gru/work/ad-hoc/<minion-id>/
    // Note: We use "ad-hoc" as a pseudo-repo name and minion_id as the branch name
    // to leverage the existing work_dir validation (path traversal protection, etc.)
    let workspace_path = workspace
        .work_dir("ad-hoc", &minion_id)
        .context("Failed to compute workspace path")?;

    // Create the workspace directory
    tokio::fs::create_dir_all(&workspace_path)
        .await
        .context("Failed to create workspace directory")?;

    // Save prompt to file for debugging and audit trail
    let prompt_file = workspace_path.join("prompt.txt");
    tokio::fs::write(&prompt_file, prompt)
        .await
        .context("Failed to save prompt to workspace")?;

    println!("📂 Workspace: {}", workspace_path.display());

    // Register minion in registry
    // The "ad-hoc" repo name is a special reserved value used in the MinionRegistry
    // to represent prompt minions that are not associated with any real repository.
    // This allows us to leverage the existing registry and workspace mechanisms for
    // tracking, displaying, and managing prompt-based minions, while clearly
    // distinguishing them from repo-based minions. Any code that filters, displays,
    // or processes minions by repo should be aware that "ad-hoc" is a special case
    // and may require different handling (e.g., prompts have no issues, branches, or PRs).
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: "ad-hoc".to_string(), // Special reserved value for prompt minions
        issue: 0,                   // Prompts don't have issues
        command: "prompt".to_string(),
        prompt: prompt.to_string(),
        started_at: now,
        branch: String::new(), // Prompts don't have branches
        worktree: workspace_path.clone(),
        status: "active".to_string(),
        pr: None,                      // Prompts don't have PRs
        session_id: minion_id.clone(), // Prompts use minion_id as session_id
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
    };

    let mut registry = MinionRegistry::load(None).context("Failed to load Minion registry")?;
    registry
        .register(minion_id.clone(), registry_info)
        .context("Failed to register prompt Minion in registry")?;

    // Generate a unique session ID (UUID) for Claude's --session-id flag
    let session_id = Uuid::new_v4();

    println!("🤖 Launching Claude...\n");

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: format!("ad-hoc: {}", prompt),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Get current working directory to pass to Claude
    let cwd = std::env::current_dir().context("Failed to get current working directory")?;

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = TokioCommand::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--session-id")
        .arg(session_id.to_string()) // Claude requires a valid UUID for --session-id
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions")
        .arg(prompt)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(&cwd) // Run in user's current directory, not workspace
        .env("GRU_WORKSPACE", &minion_id);

    // Spawn the command
    let mut child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    // Record child PID in the registry for status tracking (spawn_blocking to avoid
    // blocking the async executor with file lock + disk I/O)
    if let Some(pid) = child.id() {
        let pid_minion_id = minion_id.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(mut registry) = MinionRegistry::load(None) {
                let _ = registry.update(&pid_minion_id, |info| {
                    info.pid = Some(pid);
                    info.last_activity = Utc::now();
                });
            }
        })
        .await
        .context("Failed to update registry with PID")?;
    }

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
                log::info!(
                    "❌ Task appears stuck (no activity for {} minutes)",
                    INACTIVITY_STUCK_SECS / 60
                );
                log::info!("📝 Events saved to events.jsonl");
                return Err(anyhow::anyhow!(
                    "No activity for {} minutes - task appears stuck",
                    INACTIVITY_STUCK_SECS / 60
                ));
            } else if inactivity.as_secs() >= INACTIVITY_WARNING_SECS && !warned_at_5min {
                log::info!(
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
                    // Log the event to events.jsonl in workspace
                    log_event(&workspace_path, &output).await?;

                    // Update last event time for any output
                    last_event_time = Instant::now();

                    // Reset warning flag only on actual events
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

    // Remove minion from registry (best effort - don't fail if this errors).
    // No need to update PID/mode first since the entry is being deleted.
    if let Ok(mut registry) = MinionRegistry::load(None) {
        if let Err(e) = registry.remove(&minion_id) {
            log::info!(
                "Warning: Failed to remove minion {} from registry: {}",
                minion_id,
                e
            );
        }
    }

    // Now check if there was a stream error (after cleanup)
    stream_result?;

    // Finish the progress display and return appropriate exit code
    if status.success() {
        progress.finish_with_message("✅ Task completed");
        println!("\n📁 Session workspace: {}", workspace_path.display());
        println!("💡 To resume this session, use: gru resume {}", minion_id);
        Ok(0)
    } else {
        let exit_code = status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        progress.finish_with_message(&format!("❌ Task failed (exit code: {})", exit_code));
        println!(
            "\n📝 Events saved to: {}",
            workspace_path.join("events.jsonl").display()
        );
        println!(
            "📄 Prompt saved to: {}",
            workspace_path.join("prompt.txt").display()
        );
        Ok(exit_code)
    }
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
    fn test_parse_timeout_invalid_unit() {
        assert!(parse_timeout("10x").is_err());
        assert!(parse_timeout("10d").is_err());
    }

    #[test]
    fn test_parse_timeout_invalid_number() {
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("12.5m").is_err());
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_flag_like_input() {
        let result = handle_prompt("--help", None, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot start with '-'"));
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_empty_input() {
        let result = handle_prompt("", None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));

        let result = handle_prompt("   ", None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }
}
