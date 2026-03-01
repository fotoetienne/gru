use crate::stream::{self, ClaudeEvent, EventStream, TokenUsage};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

/// Timeout in seconds for each line read from Claude's output stream
/// Set to 5 minutes to accommodate long-running LLM operations
pub const STREAM_TIMEOUT_SECS: u64 = 300;

/// Duration of inactivity before warning the user
pub const INACTIVITY_WARNING_SECS: u64 = 300; // 5 minutes

/// Duration of inactivity before considering the task stuck
pub const INACTIVITY_STUCK_SECS: u64 = 900; // 15 minutes

/// Exit code returned when a process is terminated by a signal (shell convention)
pub const EXIT_CODE_SIGNAL_TERMINATED: i32 = 128;

/// Errors from the Claude runner that indicate the task is stuck or timed out.
///
/// These are typed errors so callers can reliably detect blocked states via
/// `downcast_ref::<ClaudeRunnerError>()` rather than fragile string matching.
#[derive(Debug)]
pub enum ClaudeRunnerError {
    /// The task exceeded its configured maximum timeout (--timeout flag).
    MaxTimeout(Duration),
    /// No activity (no stream events) for INACTIVITY_STUCK_SECS.
    InactivityStuck { minutes: u64 },
    /// No output from the Claude process for STREAM_TIMEOUT_SECS.
    StreamTimeout { seconds: u64 },
}

impl std::fmt::Display for ClaudeRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeRunnerError::MaxTimeout(d) => {
                write!(f, "Task exceeded maximum timeout of {:?}", d)
            }
            ClaudeRunnerError::InactivityStuck { minutes } => {
                write!(
                    f,
                    "No activity for {} minutes - task appears stuck",
                    minutes
                )
            }
            ClaudeRunnerError::StreamTimeout { seconds } => {
                write!(
                    f,
                    "Timeout: Claude process hasn't produced output in {} seconds",
                    seconds
                )
            }
        }
    }
}

impl std::error::Error for ClaudeRunnerError {}

/// Result of running a Claude session, including exit status and token usage
#[derive(Debug)]
pub struct ClaudeRunResult {
    pub status: std::process::ExitStatus,
    pub token_usage: TokenUsage,
}

/// Logs an event to events.jsonl in the given directory
async fn log_event(dir: &Path, event: &stream::StreamOutput) -> Result<()> {
    let events_file = dir.join("events.jsonl");
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
pub fn parse_timeout(timeout_str: &str) -> Result<Duration> {
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

/// Builds a standard Claude command with common flags
///
/// This helper creates a TokioCommand configured for non-interactive stream-json output.
/// Callers can further customize the command before passing it to run_claude_with_stream_monitoring.
pub fn build_claude_command(worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand {
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
pub fn build_claude_resume_command(
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
/// `on_spawn` is called synchronously with the child PID immediately after the process
/// is spawned, before stream processing begins. This ensures the PID is recorded before
/// the process can exit, avoiding a race condition. The callback should be fast (e.g.,
/// writing a PID to a registry file); it runs on the async executor thread.
pub async fn run_claude_with_stream_monitoring<F>(
    mut cmd: TokioCommand,
    worktree_path: &Path,
    timeout_opt: Option<&str>,
    mut output_callback: Option<F>,
    on_spawn: Option<Box<dyn FnOnce(u32) + Send>>,
) -> Result<ClaudeRunResult>
where
    F: FnMut(&stream::StreamOutput),
{
    // Spawn the command
    let mut child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    // Report the child PID to the caller if a callback was provided.
    // The callback runs synchronously on the current thread (see doc comment above).
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

    // Accumulate token usage across the session
    let mut token_usage = TokenUsage::default();

    // Process stream output asynchronously with timeout and error handling
    let stream_result: Result<()> = async {
        let mut last_event_time = Instant::now();
        let mut inactivity_warning_shown = false;

        loop {
            // Check overall task timeout
            if let Some(max_duration) = max_timeout {
                let elapsed = task_start.elapsed();
                if elapsed >= max_duration {
                    log::info!("⏱️  Task timeout reached ({:?})", max_duration);
                    log::info!("📝 Events saved to events.jsonl");
                    return Err(ClaudeRunnerError::MaxTimeout(max_duration).into());
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
                return Err(ClaudeRunnerError::InactivityStuck {
                    minutes: INACTIVITY_STUCK_SECS / 60,
                }
                .into());
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
                .map_err(|_| -> anyhow::Error {
                    ClaudeRunnerError::StreamTimeout {
                        seconds: STREAM_TIMEOUT_SECS,
                    }
                    .into()
                })?;

            // Handle the stream result
            match line_result? {
                Some(output) => {
                    // Log the event to events.jsonl
                    log_event(worktree_path, &output).await?;

                    // Update last event time for any output
                    last_event_time = Instant::now();

                    // Accumulate token usage from stream events
                    if let stream::StreamOutput::Event(ref event) = output {
                        match event {
                            ClaudeEvent::MessageStart { message } => {
                                if let Some(ref usage) = message.usage {
                                    token_usage.add_message_start(usage);
                                }
                            }
                            ClaudeEvent::MessageDelta {
                                usage: Some(ref usage),
                                ..
                            } => {
                                token_usage.add_message_delta(usage);
                            }
                            _ => {}
                        }
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

    // Return the exit status and token usage for caller to handle
    Ok(ClaudeRunResult {
        status,
        token_usage,
    })
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

    #[test]
    fn test_parse_timeout_invalid_unit() {
        assert!(parse_timeout("10d").is_err());
    }

    #[test]
    fn test_parse_timeout_invalid_number() {
        assert!(parse_timeout("12.5m").is_err());
    }
}
