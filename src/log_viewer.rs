//! Event log viewer for replaying and tailing `events.jsonl`.
//!
//! Provides functionality to replay historical events and follow live events
//! in real-time (poll-based, no external dependencies). Used by `gru logs`
//! and the auto-tail feature of `gru do`.

use crate::agent::AgentEvent;
use crate::minion_registry::{is_process_alive, with_registry};
use crate::progress::{ProgressConfig, ProgressDisplay};
use anyhow::{Context, Result};
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::signal;

/// Poll interval for checking new events in tail mode.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Replays all existing events from an events.jsonl file through a ProgressDisplay.
/// Returns the byte position after replay (for subsequent tailing).
pub fn replay_events(events_path: &Path, progress: &ProgressDisplay) -> Result<u64> {
    let file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line from events.jsonl")?;
        if bytes_read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            progress.handle_event(&event);
        }
    }

    // Get exact file position from the reader
    let position = reader
        .stream_position()
        .context("Failed to get stream position")?;
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

    // Look up PID once for liveness checks (avoid re-loading registry every poll)
    let mid = minion_id.to_string();
    let worker_pid = with_registry(move |reg| Ok(reg.get(&mid).and_then(|info| info.pid)))
        .await
        .unwrap_or(None);

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
        // Fail fast if worker died before creating events file
        if !is_pid_alive(worker_pid) {
            anyhow::bail!("Worker exited before creating events file. Check gru.log for details.");
        }
        tokio::time::sleep(TAIL_POLL_INTERVAL).await;
        waited += TAIL_POLL_INTERVAL;
    }

    // Replay existing events
    let mut position = replay_events(&events_path, &progress)?;

    // Follow new events with poll-based tailing
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                progress.finish_with_message("Detached from minion (worker continues in background)");
                break;
            }
            _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {
                // Read new events from the file
                position = read_new_events(&events_path, position, &progress)?;

                // Check if worker is still alive (using cached PID)
                if !is_pid_alive(worker_pid) {
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
/// Returns the new byte position after reading.
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

    let mut reader = std::io::BufReader::new(file);
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line")?;
        if bytes_read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            progress.handle_event(&event);
        }
    }

    let new_position = reader
        .stream_position()
        .context("Failed to get stream position")?;
    Ok(new_position)
}

/// Replays the last `n` events from an events.jsonl file through a ProgressDisplay.
/// Returns the byte position after replay (for subsequent tailing).
pub fn replay_last_n_events(
    events_path: &Path,
    n: usize,
    progress: &ProgressDisplay,
) -> Result<u64> {
    let file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;
    let mut reader = std::io::BufReader::new(file);

    // First pass: collect all events
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line from events.jsonl")?;
        if bytes_read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            events.push(event);
        }
    }

    // Replay only the last N events
    let start = events.len().saturating_sub(n);
    for event in &events[start..] {
        progress.handle_event(event);
    }

    let position = reader
        .stream_position()
        .context("Failed to get stream position")?;
    Ok(position)
}

/// Replays all events from an events.jsonl file as raw JSONL to stdout.
/// Returns the byte position after replay.
pub fn replay_events_raw(events_path: &Path) -> Result<u64> {
    let file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line from events.jsonl")?;
        if bytes_read == 0 {
            break;
        }
        stdout.write_all(&line_buf)?;
    }

    let position = reader
        .stream_position()
        .context("Failed to get stream position")?;
    Ok(position)
}

/// Replays the last N lines from an events.jsonl file as raw JSONL to stdout.
/// Returns the byte position after replay.
pub fn replay_last_n_events_raw(events_path: &Path, n: usize) -> Result<u64> {
    let file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line from events.jsonl")?;
        if bytes_read == 0 {
            break;
        }
        lines.push(line_buf.clone());
    }

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        stdout.write_all(line)?;
    }

    let position = reader
        .stream_position()
        .context("Failed to get stream position")?;
    Ok(position)
}

/// Tails an events.jsonl file in raw JSONL mode.
/// Replays history (optionally last N lines) then follows live events.
pub async fn tail_events_raw(
    events_path: PathBuf,
    minion_id: &str,
    last_n: Option<usize>,
) -> Result<()> {
    let mid = minion_id.to_string();
    let worker_pid = with_registry(move |reg| Ok(reg.get(&mid).and_then(|info| info.pid)))
        .await
        .unwrap_or(None);

    // Wait for events file
    let mut waited = Duration::ZERO;
    let max_wait = Duration::from_secs(30);
    while !events_path.exists() {
        if waited >= max_wait {
            anyhow::bail!(
                "Timed out waiting for events file: {}",
                events_path.display()
            );
        }
        if !is_pid_alive(worker_pid) {
            anyhow::bail!("Worker exited before creating events file.");
        }
        tokio::time::sleep(TAIL_POLL_INTERVAL).await;
        waited += TAIL_POLL_INTERVAL;
    }

    // Replay existing events
    let mut position = match last_n {
        Some(n) => replay_last_n_events_raw(&events_path, n)?,
        None => replay_events_raw(&events_path)?,
    };

    // Follow new events
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                break;
            }
            _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {
                position = read_new_events_raw(&events_path, position)?;
                if !is_pid_alive(worker_pid) {
                    let _ = read_new_events_raw(&events_path, position);
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Reads new raw events from a file starting at the given byte position.
fn read_new_events_raw(events_path: &Path, position: u64) -> Result<u64> {
    let mut file = std::fs::File::open(events_path)
        .with_context(|| format!("Failed to open {}", events_path.display()))?;

    let metadata = file.metadata().context("Failed to get file metadata")?;
    if metadata.len() <= position {
        return Ok(position);
    }

    file.seek(SeekFrom::Start(position))
        .context("Failed to seek in events file")?;

    let mut reader = std::io::BufReader::new(file);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read line")?;
        if bytes_read == 0 {
            break;
        }
        stdout.write_all(&line_buf)?;
    }

    let new_position = reader
        .stream_position()
        .context("Failed to get stream position")?;
    Ok(new_position)
}

/// Tails events with last-N support (formatted mode).
pub async fn tail_events_last_n(
    events_path: PathBuf,
    minion_id: &str,
    issue_num: &str,
    quiet: bool,
    last_n: Option<usize>,
) -> Result<()> {
    let config = ProgressConfig {
        minion_id: minion_id.to_string(),
        issue: issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let mid = minion_id.to_string();
    let worker_pid = with_registry(move |reg| Ok(reg.get(&mid).and_then(|info| info.pid)))
        .await
        .unwrap_or(None);

    // Wait for events file
    let mut waited = Duration::ZERO;
    let max_wait = Duration::from_secs(30);
    while !events_path.exists() {
        if waited >= max_wait {
            anyhow::bail!(
                "Timed out waiting for events file: {}",
                events_path.display()
            );
        }
        if !is_pid_alive(worker_pid) {
            anyhow::bail!("Worker exited before creating events file.");
        }
        tokio::time::sleep(TAIL_POLL_INTERVAL).await;
        waited += TAIL_POLL_INTERVAL;
    }

    // Replay existing events (last N or all)
    let mut position = match last_n {
        Some(n) => replay_last_n_events(&events_path, n, &progress)?,
        None => replay_events(&events_path, &progress)?,
    };

    // Follow new events
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                progress.finish_with_message("Detached from minion (worker continues in background)");
                break;
            }
            _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {
                position = read_new_events(&events_path, position, &progress)?;
                if !is_pid_alive(worker_pid) {
                    let _ = read_new_events(&events_path, position, &progress);
                    progress.finish_with_message("Minion has finished");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Checks if a worker process is alive using a cached PID.
fn is_pid_alive(pid: Option<u32>) -> bool {
    pid.map(is_process_alive).unwrap_or(false)
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
        // Position should match the actual file size
        let file_len = tmp.as_file().metadata().unwrap().len();
        assert_eq!(pos, file_len);
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
        let pos = replay_events(tmp.path(), &progress).unwrap();
        let file_len = tmp.as_file().metadata().unwrap().len();
        assert_eq!(pos, file_len);
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

    #[test]
    fn test_is_pid_alive_none() {
        assert!(!is_pid_alive(None));
    }

    #[test]
    fn test_is_pid_alive_current_process() {
        assert!(is_pid_alive(Some(std::process::id())));
    }
}
