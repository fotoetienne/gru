use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Represents a Claude event that can be logged
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)] // Module will be used in future issues
pub enum ClaudeEvent {
    #[serde(rename = "thinking")]
    Thinking { content: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "message")]
    Message { content: String },

    #[serde(rename = "complete")]
    Complete,

    #[serde(rename = "error")]
    Error { message: String },
}

/// A logged event with timestamp and minion_id
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)] // Module will be used in future issues
pub struct LoggedEvent {
    pub timestamp: DateTime<Utc>,
    pub minion_id: String,
    #[serde(flatten)]
    pub event: ClaudeEvent,
}

/// EventLogger manages writing events to a JSONL file
#[allow(dead_code)] // Module will be used in future issues
pub struct EventLogger {
    minion_id: String,
    log_path: PathBuf,
}

#[allow(dead_code)] // Module will be used in future issues
impl EventLogger {
    /// Creates a new EventLogger for the given minion_id
    /// This creates the archive directory if it doesn't exist
    pub fn new(minion_id: impl Into<String>) -> Result<Self> {
        let minion_id = minion_id.into();
        let log_path = Self::get_log_path(&minion_id)?;

        // Create the parent directory if it doesn't exist
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).context("Failed to create archive directory")?;
        }

        Ok(Self {
            minion_id,
            log_path,
        })
    }

    /// Gets the log path for a given minion_id
    /// Returns ~/.gru/archive/<minion-id>/events.jsonl
    fn get_log_path(minion_id: &str) -> Result<PathBuf> {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        Ok(home_dir
            .join(".gru")
            .join("archive")
            .join(minion_id)
            .join("events.jsonl"))
    }

    /// Logs an event to the JSONL file
    /// Each event is written as a single line with timestamp and minion_id
    /// The file is flushed immediately to ensure durability
    pub fn log_event(&self, event: ClaudeEvent) -> Result<()> {
        let logged_event = LoggedEvent {
            timestamp: Utc::now(),
            minion_id: self.minion_id.clone(),
            event,
        };

        // Open file in append mode, create if it doesn't exist
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .context("Failed to open log file")?;

        let mut writer = BufWriter::new(file);

        // Serialize the event to JSON and write as a single line
        serde_json::to_writer(&mut writer, &logged_event).context("Failed to serialize event")?;

        // Write newline to separate events
        writeln!(writer).context("Failed to write newline")?;

        // Flush to ensure the event is written to disk immediately
        writer.flush().context("Failed to flush log file")?;

        Ok(())
    }

    /// Gets the path to the log file
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Gets the minion_id
    pub fn minion_id(&self) -> &str {
        &self.minion_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_minion_id(prefix: &str) -> String {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!(
            "{}_{}_{}",
            prefix,
            Utc::now().timestamp_nanos_opt().unwrap(),
            counter
        )
    }

    #[test]
    fn test_event_logger_new() {
        let minion_id = unique_minion_id("M_TEST_NEW");
        let logger = EventLogger::new(&minion_id).unwrap();

        assert_eq!(logger.minion_id(), minion_id);
        assert!(logger.log_path().to_string_lossy().contains("archive"));
        assert!(logger.log_path().to_string_lossy().contains("events.jsonl"));
    }

    #[test]
    fn test_log_event_creates_file() {
        let minion_id = unique_minion_id("M_TEST_CREATE");
        let logger = EventLogger::new(&minion_id).unwrap();

        // Log a test event
        let event = ClaudeEvent::Thinking {
            content: "Test thinking".to_string(),
        };
        logger.log_event(event).unwrap();

        // Verify file exists
        assert!(logger.log_path().exists());

        // Clean up - only remove file, leave directory structure
        fs::remove_file(logger.log_path()).ok();
    }

    #[test]
    fn test_log_event_appends_to_file() {
        let minion_id = unique_minion_id("M_TEST_APPEND");
        let logger = EventLogger::new(&minion_id).unwrap();

        // Clean up any existing file from previous test runs
        fs::remove_file(logger.log_path()).ok();

        // Log multiple events
        logger
            .log_event(ClaudeEvent::Thinking {
                content: "First thought".to_string(),
            })
            .unwrap();
        logger
            .log_event(ClaudeEvent::Message {
                content: "First message".to_string(),
            })
            .unwrap();
        logger.log_event(ClaudeEvent::Complete).unwrap();

        // Read the file and verify all events are present
        let file = fs::File::open(logger.log_path()).unwrap();
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();

        assert_eq!(lines.len(), 3);

        // Verify each line is valid JSON
        for line in &lines {
            let logged_event: LoggedEvent = serde_json::from_str(line).unwrap();
            assert_eq!(logged_event.minion_id, minion_id);
            assert!(logged_event.timestamp <= Utc::now());
        }

        // Clean up - only remove file, leave directory structure
        fs::remove_file(logger.log_path()).ok();
    }

    #[test]
    fn test_log_event_includes_timestamp_and_minion_id() {
        let minion_id = unique_minion_id("M_TEST_TIMESTAMP");
        let logger = EventLogger::new(&minion_id).unwrap();

        // Clean up any existing file from previous test runs
        fs::remove_file(logger.log_path()).ok();

        let event = ClaudeEvent::ToolUse {
            name: "bash".to_string(),
            input: serde_json::json!({"command": "ls -la"}),
        };
        logger.log_event(event).unwrap();

        // Read the logged event
        let file = fs::File::open(logger.log_path()).unwrap();
        let reader = BufReader::new(file);
        let line = reader.lines().next().unwrap().unwrap();
        let logged_event: LoggedEvent = serde_json::from_str(&line).unwrap();

        assert_eq!(logged_event.minion_id, minion_id);
        assert!(logged_event.timestamp <= Utc::now());

        match logged_event.event {
            ClaudeEvent::ToolUse { name, .. } => {
                assert_eq!(name, "bash");
            }
            _ => panic!("Expected ToolUse event"),
        }

        // Clean up - only remove file, leave directory structure
        fs::remove_file(logger.log_path()).ok();
    }

    #[test]
    fn test_logged_event_serialization() {
        let event = ClaudeEvent::Error {
            message: "Test error".to_string(),
        };
        let logged_event = LoggedEvent {
            timestamp: Utc::now(),
            minion_id: "M042".to_string(),
            event,
        };

        let json = serde_json::to_string(&logged_event).unwrap();
        let deserialized: LoggedEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.minion_id, "M042");
        match deserialized.event {
            ClaudeEvent::Error { message } => {
                assert_eq!(message, "Test error");
            }
            _ => panic!("Expected Error event"),
        }
    }
}
