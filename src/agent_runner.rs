//! Generic agent runner that works with any `AgentBackend` implementation.
//!
//! This module provides `run_agent_with_stream_monitoring`, the backend-agnostic
//! replacement for `run_claude_with_stream_monitoring`. It reads lines from the
//! agent process's stdout, delegates parsing to `AgentBackend::parse_events()`,
//! and passes normalized `AgentEvent` values to the caller's callback.

use crate::agent::{AgentBackend, AgentEvent, TimestampedEventRef, TokenUsage};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

/// Timeout in seconds for each line read from the agent's output stream.
/// Set to 5 minutes to accommodate long-running LLM operations.
pub(crate) const STREAM_TIMEOUT_SECS: u64 = 300;

/// Duration of inactivity before warning the user.
pub(crate) const INACTIVITY_WARNING_SECS: u64 = 300; // 5 minutes

/// Duration of inactivity before considering the task stuck.
pub(crate) const INACTIVITY_STUCK_SECS: u64 = 900; // 15 minutes

/// Exit code returned when a process is terminated by a signal (shell convention).
pub(crate) const EXIT_CODE_SIGNAL_TERMINATED: i32 = 128;

/// Classification of inactivity state based on elapsed time since last event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InactivityState {
    /// Activity is within normal bounds.
    Normal,
    /// Inactivity has reached the warning threshold but not the stuck threshold.
    Warning,
    /// Inactivity has reached the stuck threshold — the task should be terminated.
    Stuck,
}

/// Classifies the inactivity state given the elapsed seconds since the last event.
pub(crate) fn classify_inactivity(elapsed_secs: u64) -> InactivityState {
    if elapsed_secs >= INACTIVITY_STUCK_SECS {
        InactivityState::Stuck
    } else if elapsed_secs >= INACTIVITY_WARNING_SECS {
        InactivityState::Warning
    } else {
        InactivityState::Normal
    }
}

/// Errors from the agent runner that indicate the task is stuck or timed out.
///
/// These are typed errors so callers can reliably detect blocked states via
/// `downcast_ref::<AgentRunnerError>()` rather than fragile string matching.
#[derive(Debug)]
pub(crate) enum AgentRunnerError {
    /// The task exceeded its configured maximum timeout (--timeout flag).
    MaxTimeout(Duration),
    /// No activity (no stream events) for INACTIVITY_STUCK_SECS.
    InactivityStuck { minutes: u64 },
    /// No output from the agent process for STREAM_TIMEOUT_SECS.
    StreamTimeout { seconds: u64 },
}

impl std::fmt::Display for AgentRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentRunnerError::MaxTimeout(d) => {
                write!(f, "Task exceeded maximum timeout of {:?}", d)
            }
            AgentRunnerError::InactivityStuck { minutes } => {
                write!(
                    f,
                    "No activity for {} minutes - task appears stuck",
                    minutes
                )
            }
            AgentRunnerError::StreamTimeout { seconds } => {
                write!(
                    f,
                    "Timeout: agent process hasn't produced output in {} seconds",
                    seconds
                )
            }
        }
    }
}

impl std::error::Error for AgentRunnerError {}

/// Returns true if the error indicates the task is stuck or timed out,
/// meaning it should be labeled `gru:blocked` instead of `gru:failed`.
pub(crate) fn is_stuck_or_timeout_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AgentRunnerError>().is_some()
}

/// Result of running an agent session, including exit status and token usage.
#[derive(Debug)]
pub(crate) struct AgentRunResult {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) token_usage: TokenUsage,
}

/// Parses a timeout string into a Duration.
/// Supports formats like "10s", "5m", "1h", "30".
pub(crate) fn parse_timeout(timeout_str: &str) -> Result<Duration> {
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

/// Logs an `AgentEvent` to events.jsonl in the given directory.
///
/// Each event is wrapped in a `TimestampedEvent` so the wall-clock time
/// is persisted alongside the event data.
async fn log_event(dir: &Path, event: &AgentEvent) -> Result<()> {
    let events_file = dir.join("events.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_file)
        .await
        .context("Failed to open events.jsonl")?;

    let ts = chrono::Utc::now().to_rfc3339();
    let timestamped = TimestampedEventRef { ts: &ts, event };
    let json = serde_json::to_string(&timestamped)?;
    file.write_all(json.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

/// Runs an agent with stream monitoring and timeout detection.
///
/// Spawns the pre-built command, reads stdout line by line, and delegates
/// parsing to `backend.parse_events()`. Normalized `AgentEvent` values are
/// passed to `output_callback` and logged to `events.jsonl`.
///
/// `events_dir` is the directory where `events.jsonl` will be written.
/// This is typically the minion directory, separate from the git checkout
/// where the agent runs (which is set via `cmd.current_dir()` by the caller).
///
/// `on_spawn` is called synchronously with the child PID immediately after the
/// process is spawned, before stream processing begins. This ensures the PID
/// is recorded before the process can exit.
pub(crate) async fn run_agent_with_stream_monitoring<F>(
    mut cmd: TokioCommand,
    backend: &dyn AgentBackend,
    events_dir: &Path,
    timeout_opt: Option<&str>,
    mut output_callback: Option<F>,
    on_spawn: Option<Box<dyn FnOnce(u32) + Send>>,
) -> Result<AgentRunResult>
where
    F: FnMut(&AgentEvent),
{
    // Ensure stdout is piped so we can read the agent's event stream.
    // This is defensive — backends should already set this in build_command(),
    // but forcing it here prevents silent failures from misconfigured commands.
    cmd.stdout(std::process::Stdio::piped());

    // Spawn the command
    let mut child = cmd
        .spawn()
        .with_context(|| format!("{} command not found or failed to start", backend.name()))?;

    // Report the child PID to the caller if a callback was provided.
    if let Some(callback) = on_spawn {
        if let Some(pid) = child.id() {
            callback(pid);
        }
    }

    // Get the stdout handle
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture stdout from agent process")?;

    let mut reader = BufReader::new(stdout);

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
        let mut line = String::new();

        loop {
            // Check overall task timeout
            if let Some(max_duration) = max_timeout {
                let elapsed = task_start.elapsed();
                if elapsed >= max_duration {
                    log::info!("⏱️  Task timeout reached ({:?})", max_duration);
                    log::info!("📝 Events saved to events.jsonl");
                    return Err(AgentRunnerError::MaxTimeout(max_duration).into());
                }
            }

            // Check inactivity - time since last event
            let inactivity = last_event_time.elapsed();

            match classify_inactivity(inactivity.as_secs()) {
                InactivityState::Stuck => {
                    log::error!(
                        "❌ Task appears stuck (no activity for {} minutes)",
                        INACTIVITY_STUCK_SECS / 60
                    );
                    log::info!("📝 Events saved to events.jsonl");
                    return Err(AgentRunnerError::InactivityStuck {
                        minutes: INACTIVITY_STUCK_SECS / 60,
                    }
                    .into());
                }
                InactivityState::Warning if !inactivity_warning_shown => {
                    log::warn!(
                        "⚠️  No activity for {} minutes",
                        INACTIVITY_WARNING_SECS / 60
                    );
                    inactivity_warning_shown = true;
                }
                _ => {}
            }

            // Try to read next line with timeout
            line.clear();
            let bytes_read = timeout(
                Duration::from_secs(STREAM_TIMEOUT_SECS),
                reader.read_line(&mut line),
            )
            .await
            .map_err(|_| -> anyhow::Error {
                AgentRunnerError::StreamTimeout {
                    seconds: STREAM_TIMEOUT_SECS,
                }
                .into()
            })?
            .context("Failed to read line from agent stream")?;

            if bytes_read == 0 {
                // Stream ended normally
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Any non-empty line counts as activity for inactivity detection.
            // This ensures we don't treat the task as stuck when the backend
            // chooses to skip certain lines (e.g., logs, banners, non-JSON).
            last_event_time = Instant::now();
            inactivity_warning_shown = false;

            // Parse the line through the backend
            for event in backend.parse_events(trimmed) {
                // Log the event to events.jsonl
                log_event(events_dir, &event).await?;

                // Accumulate token usage from events
                accumulate_token_usage(&mut token_usage, &event);

                // Call custom callback if provided
                if let Some(ref mut callback) = output_callback {
                    callback(&event);
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
    Ok(AgentRunResult {
        status,
        token_usage,
    })
}

/// Accumulates token usage from an `AgentEvent` into a running total.
fn accumulate_token_usage(total: &mut TokenUsage, event: &AgentEvent) {
    match event {
        AgentEvent::Started { usage: Some(usage) } => {
            total.input_tokens += usage.input_tokens;
            if let Some(cache_creation) = usage.cache_creation_input_tokens {
                *total.cache_creation_input_tokens.get_or_insert(0) += cache_creation;
            }
            if let Some(cache_read) = usage.cache_read_input_tokens {
                *total.cache_read_input_tokens.get_or_insert(0) += cache_read;
            }
        }
        AgentEvent::MessageComplete {
            usage: Some(usage), ..
        } => {
            total.output_tokens += usage.output_tokens;
        }
        AgentEvent::Finished {
            usage: Some(usage), ..
        } => {
            // Some backends report aggregate usage only at session end.
            // Accumulate all fields so we don't silently drop final totals.
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            if let Some(cache_creation) = usage.cache_creation_input_tokens {
                *total.cache_creation_input_tokens.get_or_insert(0) += cache_creation;
            }
            if let Some(cache_read) = usage.cache_read_input_tokens {
                *total.cache_read_input_tokens.get_or_insert(0) += cache_read;
            }
        }
        _ => {}
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

    #[test]
    fn test_accumulate_token_usage_started() {
        let mut total = TokenUsage::default();
        let event = AgentEvent::Started {
            usage: Some(TokenUsage {
                input_tokens: 1000,
                output_tokens: 0,
                cache_creation_input_tokens: Some(200),
                cache_read_input_tokens: Some(100),
            }),
        };
        accumulate_token_usage(&mut total, &event);
        assert_eq!(total.input_tokens, 1000);
        assert_eq!(total.cache_creation_input_tokens, Some(200));
        assert_eq!(total.cache_read_input_tokens, Some(100));
    }

    #[test]
    fn test_accumulate_token_usage_message_complete() {
        let mut total = TokenUsage::default();
        let event = AgentEvent::MessageComplete {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 500,
                ..Default::default()
            }),
        };
        accumulate_token_usage(&mut total, &event);
        assert_eq!(total.output_tokens, 500);
    }

    #[test]
    fn test_accumulate_token_usage_multiple_turns() {
        let mut total = TokenUsage::default();

        // First turn: input tokens from Started
        accumulate_token_usage(
            &mut total,
            &AgentEvent::Started {
                usage: Some(TokenUsage {
                    input_tokens: 1000,
                    cache_creation_input_tokens: Some(200),
                    cache_read_input_tokens: Some(100),
                    ..Default::default()
                }),
            },
        );

        // First turn: output tokens from MessageComplete
        accumulate_token_usage(
            &mut total,
            &AgentEvent::MessageComplete {
                stop_reason: Some("tool_use".to_string()),
                usage: Some(TokenUsage {
                    output_tokens: 500,
                    ..Default::default()
                }),
            },
        );

        // Second turn: more input tokens
        accumulate_token_usage(
            &mut total,
            &AgentEvent::Started {
                usage: Some(TokenUsage {
                    input_tokens: 2000,
                    cache_read_input_tokens: Some(500),
                    ..Default::default()
                }),
            },
        );

        // Second turn: more output tokens
        accumulate_token_usage(
            &mut total,
            &AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: Some(TokenUsage {
                    output_tokens: 300,
                    ..Default::default()
                }),
            },
        );

        assert_eq!(total.input_tokens, 3000);
        assert_eq!(total.output_tokens, 800);
        assert_eq!(total.cache_creation_input_tokens, Some(200));
        assert_eq!(total.cache_read_input_tokens, Some(600));
        assert_eq!(total.total_tokens(), 3800);
    }

    #[test]
    fn test_accumulate_token_usage_finished() {
        let mut total = TokenUsage::default();
        accumulate_token_usage(
            &mut total,
            &AgentEvent::Finished {
                usage: Some(TokenUsage {
                    input_tokens: 5000,
                    output_tokens: 2000,
                    cache_creation_input_tokens: Some(300),
                    cache_read_input_tokens: Some(100),
                }),
            },
        );
        assert_eq!(total.input_tokens, 5000);
        assert_eq!(total.output_tokens, 2000);
        assert_eq!(total.cache_creation_input_tokens, Some(300));
        assert_eq!(total.cache_read_input_tokens, Some(100));
    }

    #[test]
    fn test_accumulate_ignores_no_usage() {
        let mut total = TokenUsage::default();
        accumulate_token_usage(&mut total, &AgentEvent::Started { usage: None });
        accumulate_token_usage(&mut total, &AgentEvent::Ping);
        accumulate_token_usage(
            &mut total,
            &AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
        );
        assert_eq!(total.total_tokens(), 0);
    }

    #[test]
    fn test_is_stuck_or_timeout_error() {
        let err: anyhow::Error = AgentRunnerError::InactivityStuck { minutes: 15 }.into();
        assert!(is_stuck_or_timeout_error(&err));

        let err: anyhow::Error = AgentRunnerError::StreamTimeout { seconds: 300 }.into();
        assert!(is_stuck_or_timeout_error(&err));

        let err: anyhow::Error = AgentRunnerError::MaxTimeout(Duration::from_secs(600)).into();
        assert!(is_stuck_or_timeout_error(&err));

        let err = anyhow::anyhow!("some other error");
        assert!(!is_stuck_or_timeout_error(&err));
    }

    // --- T8: Inactivity classification tests ---

    #[test]
    fn test_classify_inactivity_normal() {
        assert_eq!(classify_inactivity(0), InactivityState::Normal);
        assert_eq!(classify_inactivity(60), InactivityState::Normal);
        assert_eq!(classify_inactivity(299), InactivityState::Normal);
    }

    #[test]
    fn test_classify_inactivity_warning_at_threshold() {
        assert_eq!(
            classify_inactivity(INACTIVITY_WARNING_SECS),
            InactivityState::Warning
        );
    }

    #[test]
    fn test_classify_inactivity_warning_between_thresholds() {
        assert_eq!(classify_inactivity(600), InactivityState::Warning);
        assert_eq!(classify_inactivity(899), InactivityState::Warning);
    }

    #[test]
    fn test_classify_inactivity_stuck_at_threshold() {
        assert_eq!(
            classify_inactivity(INACTIVITY_STUCK_SECS),
            InactivityState::Stuck
        );
    }

    #[test]
    fn test_classify_inactivity_stuck_above_threshold() {
        assert_eq!(classify_inactivity(1000), InactivityState::Stuck);
        assert_eq!(classify_inactivity(3600), InactivityState::Stuck);
    }

    #[test]
    fn test_classify_inactivity_boundary_between_warning_and_stuck() {
        // 899 seconds should be Warning (< 900)
        assert_eq!(classify_inactivity(899), InactivityState::Warning);
        // 900 seconds should be Stuck (>= 900)
        assert_eq!(classify_inactivity(900), InactivityState::Stuck);
    }
}
