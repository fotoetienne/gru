//! Event log viewer for replaying and tailing `events.jsonl`.
//!
//! Provides functionality to replay historical events and follow live events
//! in real-time (poll-based, no external dependencies). Used by `gru logs`
//! and the auto-tail feature of `gru do`.

use crate::agent::AgentEvent;
use crate::minion_registry::{is_process_alive, with_registry};
use crate::progress::{ProgressConfig, ProgressDisplay};
use anyhow::{Context, Result};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::signal;

/// Poll interval for checking new events in tail mode.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Replays all existing events from an events.jsonl file through a ProgressDisplay.
/// Returns the file position after replay (for subsequent tailing).
fn replay_events(events_path: &Path, progress: &ProgressDisplay) -> Result<u64> {
    let file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut position = 0u64;

    for line in reader.lines() {
        let line = line.context("Failed to read line from events.jsonl")?;
        position += line.len() as u64 + 1; // +1 for newline
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            progress.handle_event(&event);
        }
    }

    Ok(position)
}

/// Tails an events.jsonl file, replaying history then following live events.
///
/// Exits when:
/// - The minion process is no longer alive (checked via PID in registry)
/// - Ctrl+C is pressed (clean exit, does not affect the worker)
/// - The events file is fully consumed and the minion is done
pub async fn tail_events(
    events_path: PathBuf,
    minion_id: &str,
    issue_num: &str,
    quiet: bool,
) -> Result<()> {
    let config = ProgressConfig {
        minion_id: minion_id.to_string(),
        issue: issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Wait for events file to be created (worker may not have written yet)
    let mut waited = Duration::ZERO;
    let max_wait = Duration::from_secs(30);
    while !events_path.exists() {
        if waited >= max_wait {
            anyhow::bail!(
                "Timed out waiting for events file: {}",
                events_path.display()
            );
        }
        tokio::time::sleep(TAIL_POLL_INTERVAL).await;
        waited += TAIL_POLL_INTERVAL;
    }

    // Replay existing events
    let mut position = replay_events(&events_path, &progress)?;

    // Follow new events with poll-based tailing
    let mid = minion_id.to_string();
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                progress.finish_with_message("Detached from minion (worker continues in background)");
                break;
            }
            _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {
                // Read new events from the file
                position = read_new_events(&events_path, position, &progress)?;

                // Check if worker is still alive
                if !is_worker_alive(&mid).await {
                    // Read any final events
                    let _ = read_new_events(&events_path, position, &progress);
                    progress.finish_with_message("Minion has finished");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Reads new events from a file starting at the given byte position.
/// Returns the new position after reading.
fn read_new_events(events_path: &Path, position: u64, progress: &ProgressDisplay) -> Result<u64> {
    let mut file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;

    // Check if file has grown
    let metadata = file.metadata().context("Failed to get file metadata")?;
    if metadata.len() <= position {
        return Ok(position);
    }

    file.seek(SeekFrom::Start(position))
        .context("Failed to seek in events file")?;

    let reader = std::io::BufReader::new(file);
    let mut new_position = position;

    for line in reader.lines() {
        let line = line.context("Failed to read line")?;
        new_position += line.len() as u64 + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            progress.handle_event(&event);
        }
    }

    Ok(new_position)
}

/// Checks if the worker process for a minion is still alive.
async fn is_worker_alive(minion_id: &str) -> bool {
    let mid = minion_id.to_string();
    with_registry(move |reg| {
        Ok(reg
            .get(&mid)
            .and_then(|info| info.pid)
            .map(is_process_alive)
            .unwrap_or(false))
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_replay_events_empty_file() {
        let tmp = NamedTempFile::new().unwrap();
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };
        let progress = ProgressDisplay::new(config);
        let pos = replay_events(tmp.path(), &progress).unwrap();
        assert_eq!(pos, 0);
    }

    #[test]
    fn test_replay_events_with_events() {
        let mut tmp = NamedTempFile::new().unwrap();
        let event = AgentEvent::Started { usage: None };
        let json = serde_json::to_string(&event).unwrap();
        writeln!(tmp, "{}", json).unwrap();
        writeln!(tmp, "{}", json).unwrap();
        tmp.flush().unwrap();

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };
        let progress = ProgressDisplay::new(config);
        let pos = replay_events(tmp.path(), &progress).unwrap();
        assert!(pos > 0);
    }

    #[test]
    fn test_replay_events_skips_invalid_json() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "not json").unwrap();
        let event = AgentEvent::Ping;
        let json = serde_json::to_string(&event).unwrap();
        writeln!(tmp, "{}", json).unwrap();
        tmp.flush().unwrap();

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };
        let progress = ProgressDisplay::new(config);
        // Should not error on invalid JSON lines
        let pos = replay_events(tmp.path(), &progress).unwrap();
        assert!(pos > 0);
    }

    #[test]
    fn test_read_new_events_no_new_data() {
        let mut tmp = NamedTempFile::new().unwrap();
        let event = AgentEvent::Ping;
        let json = serde_json::to_string(&event).unwrap();
        writeln!(tmp, "{}", json).unwrap();
        tmp.flush().unwrap();

        let file_len = tmp.as_file().metadata().unwrap().len();
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };
        let progress = ProgressDisplay::new(config);

        // Position at end of file - should return same position
        let pos = read_new_events(tmp.path(), file_len, &progress).unwrap();
        assert_eq!(pos, file_len);
    }
}
