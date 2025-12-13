use crate::workspace::Workspace;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Maximum content length for event strings (1MB)
const MAX_CONTENT_LENGTH: usize = 1_000_000;

/// Maximum tool name length
const MAX_TOOL_NAME_LENGTH: usize = 1_000;

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

impl ClaudeEvent {
    /// Validates the event content to prevent disk exhaustion and log injection attacks
    fn validate(&self) -> Result<()> {
        match self {
            ClaudeEvent::Thinking { content }
            | ClaudeEvent::Message { content }
            | ClaudeEvent::Error { message: content } => {
                if content.len() > MAX_CONTENT_LENGTH {
                    anyhow::bail!(
                        "Event content exceeds maximum length of {} bytes",
                        MAX_CONTENT_LENGTH
                    );
                }
            }
            ClaudeEvent::ToolUse { name, input } => {
                if name.len() > MAX_TOOL_NAME_LENGTH {
                    anyhow::bail!(
                        "Tool name exceeds maximum length of {} bytes",
                        MAX_TOOL_NAME_LENGTH
                    );
                }
                let serialized_size = serde_json::to_string(input)
                    .context("Failed to serialize tool input")?
                    .len();
                if serialized_size > MAX_CONTENT_LENGTH {
                    anyhow::bail!(
                        "Tool input exceeds maximum length of {} bytes when serialized",
                        MAX_CONTENT_LENGTH
                    );
                }
            }
            ClaudeEvent::Complete => {}
        }
        Ok(())
    }
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
    ///
    /// # Security
    ///
    /// The minion_id is validated to prevent path traversal attacks.
    /// Only alphanumeric characters, hyphens, underscores, and dots are allowed.
    pub fn new(minion_id: impl Into<String>) -> Result<Self> {
        let minion_id = minion_id.into();
        let log_path = Self::get_log_path(&minion_id)?;

        Ok(Self {
            minion_id,
            log_path,
        })
    }

    /// Creates a new EventLogger with a custom workspace (for testing only).
    ///
    /// This constructor allows tests to use temporary directories instead of
    /// polluting the production `~/.gru/` directory.
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier
    /// * `workspace` - Custom workspace instance (typically with temp directory root)
    #[cfg(test)]
    pub fn new_with_workspace(minion_id: impl Into<String>, workspace: &Workspace) -> Result<Self> {
        let minion_id = minion_id.into();

        let archive_dir = workspace
            .archive_dir(&minion_id)
            .context("Invalid minion_id")?;
        let log_path = archive_dir.join("events.jsonl");

        Ok(Self {
            minion_id,
            log_path,
        })
    }

    /// Gets the log path for a given minion_id
    /// Returns ~/.gru/archive/<minion-id>/events.jsonl
    ///
    /// This method uses Workspace::archive_dir() which validates the minion_id
    /// to prevent path traversal attacks and creates the directory with proper permissions.
    fn get_log_path(minion_id: &str) -> Result<PathBuf> {
        let workspace = Workspace::new().context("Failed to initialize workspace")?;
        let archive_dir = workspace
            .archive_dir(minion_id)
            .context("Invalid minion_id")?;
        Ok(archive_dir.join("events.jsonl"))
    }

    /// Logs an event to the JSONL file
    /// Each event is written as a single line with timestamp and minion_id
    /// The file is flushed immediately to ensure durability
    ///
    /// # Security
    ///
    /// Event content is validated to prevent disk exhaustion attacks.
    /// Content exceeding 1MB will be rejected.
    pub fn log_event(&self, event: ClaudeEvent) -> Result<()> {
        // Validate event content before logging
        event.validate().context("Event validation failed")?;

        let logged_event = LoggedEvent {
            timestamp: Utc::now(),
            minion_id: self.minion_id.clone(),
            event,
        };

        // Ensure parent directory exists
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).context("Failed to create archive directory")?;
        }

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

    #[test]
    fn test_event_logger_new() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_NEW", &workspace).unwrap();

        assert_eq!(logger.minion_id(), "M_TEST_NEW");
        assert!(logger.log_path().to_string_lossy().contains("archive"));
        assert!(logger.log_path().to_string_lossy().contains("events.jsonl"));
    }

    #[test]
    fn test_log_event_creates_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_CREATE", &workspace).unwrap();

        // Log a test event
        let event = ClaudeEvent::Thinking {
            content: "Test thinking".to_string(),
        };
        logger.log_event(event).unwrap();

        // Verify file exists
        assert!(logger.log_path().exists());
    }

    #[test]
    fn test_log_event_appends_to_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_APPEND", &workspace).unwrap();

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
            assert_eq!(logged_event.minion_id, "M_TEST_APPEND");
            assert!(logged_event.timestamp <= Utc::now());
        }
    }

    #[test]
    fn test_log_event_includes_timestamp_and_minion_id() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_TIMESTAMP", &workspace).unwrap();

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

        assert_eq!(logged_event.minion_id, "M_TEST_TIMESTAMP");
        assert!(logged_event.timestamp <= Utc::now());

        match logged_event.event {
            ClaudeEvent::ToolUse { name, .. } => {
                assert_eq!(name, "bash");
            }
            _ => panic!("Expected ToolUse event"),
        }
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

    #[test]
    fn test_rejects_path_traversal_in_minion_id() {
        // Test that path traversal attempts are rejected
        assert!(EventLogger::new("../../../etc/passwd").is_err());
        assert!(EventLogger::new("../../secrets").is_err());
        assert!(EventLogger::new("foo/../bar").is_err());
        assert!(EventLogger::new("foo/bar").is_err());
        assert!(EventLogger::new("foo\\bar").is_err());
    }

    #[test]
    fn test_rejects_oversized_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_SIZE", &workspace).unwrap();

        // Create content that exceeds MAX_CONTENT_LENGTH
        let large_content = "A".repeat(MAX_CONTENT_LENGTH + 1);
        let event = ClaudeEvent::Thinking {
            content: large_content,
        };

        // Should fail validation
        assert!(logger.log_event(event).is_err());
    }

    #[test]
    fn test_rejects_oversized_tool_input() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_TOOL_SIZE", &workspace).unwrap();

        // Create a tool input that's too large when serialized
        // Each string "x" takes about 3 bytes in JSON: "x",
        // So we need about 350,000 strings to exceed 1MB
        let large_array = vec![String::from("x"); 350_000];
        let event = ClaudeEvent::ToolUse {
            name: "test".to_string(),
            input: serde_json::json!(large_array),
        };

        // Should fail validation
        assert!(logger.log_event(event).is_err());
    }

    #[test]
    fn test_rejects_oversized_tool_name() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new_with_root(temp_dir.path().to_path_buf()).unwrap();
        let logger = EventLogger::new_with_workspace("M_TEST_TOOL_NAME", &workspace).unwrap();

        // Create a tool name that's too long
        let long_name = "A".repeat(MAX_TOOL_NAME_LENGTH + 1);
        let event = ClaudeEvent::ToolUse {
            name: long_name,
            input: serde_json::json!({}),
        };

        // Should fail validation
        assert!(logger.log_event(event).is_err());
    }
}
