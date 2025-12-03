use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};

/// Represents the different types of events that can be emitted by Claude Code
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
    #[serde(rename = "thinking")]
    Thinking { content: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        /// The `#[serde(default)]` attribute allows deserialization to succeed
        /// even when the `input` field is missing from the JSON. This is required
        /// because some tool_use events may not include input parameters.
        /// Removing this attribute will cause deserialization failures for such events.
        #[serde(default)]
        input: serde_json::Value,
    },

    #[serde(rename = "message")]
    Message { content: String },

    #[serde(rename = "complete")]
    Complete,

    #[serde(rename = "error")]
    Error { message: String },
}

/// Represents the output from parsing a stream line
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutput {
    /// A parsed Claude event
    Event(ClaudeEvent),
    /// A raw line that wasn't a JSON event
    RawLine(String),
}

/// Async event stream reader for Claude Code output
pub struct EventStream<R> {
    reader: BufReader<R>,
}

impl EventStream<ChildStdout> {
    /// Create a new EventStream from a child process's stdout
    pub fn from_stdout(stdout: ChildStdout) -> Self {
        Self::new(stdout)
    }
}

impl EventStream<ChildStderr> {
    /// Create a new EventStream from a child process's stderr
    #[allow(dead_code)]
    pub fn from_stderr(stderr: ChildStderr) -> Self {
        Self::new(stderr)
    }
}

impl<R: tokio::io::AsyncRead + Unpin> EventStream<R> {
    /// Creates a new event stream from an async reader
    #[allow(dead_code)]
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// Reads the next line from the stream and attempts to parse it as a ClaudeEvent
    /// Returns None when the stream ends
    pub async fn next_line(&mut self) -> anyhow::Result<Option<StreamOutput>> {
        let mut line = String::new();
        let bytes_read = self
            .reader
            .read_line(&mut line)
            .await
            .context("Failed to read line from stream")?;

        if bytes_read == 0 {
            return Ok(None);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Return empty string for truly empty lines to avoid unnecessary whitespace
            return Ok(Some(StreamOutput::RawLine(String::new())));
        }

        // Try to parse as JSON event
        match serde_json::from_str::<ClaudeEvent>(trimmed) {
            Ok(event) => Ok(Some(StreamOutput::Event(event))),
            Err(_) => Ok(Some(StreamOutput::RawLine(line))),
        }
    }

    /// Read all lines from the stream
    #[allow(dead_code)]
    pub async fn read_all(&mut self) -> anyhow::Result<Vec<StreamOutput>> {
        let mut outputs = Vec::new();
        while let Some(output) = self.next_line().await? {
            outputs.push(output);
        }
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_thinking_event() {
        let json = r#"{"type":"thinking","content":"Analyzing the codebase..."}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Thinking { content } => {
                assert_eq!(content, "Analyzing the codebase...");
            }
            _ => panic!("Expected Thinking event"),
        }
    }

    #[tokio::test]
    async fn test_parse_tool_use_event() {
        let json = r#"{"type":"tool_use","name":"bash","input":{"command":"ls"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ToolUse { name, input } => {
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            _ => panic!("Expected ToolUse event"),
        }
    }

    #[tokio::test]
    async fn test_parse_message_event() {
        let json = r#"{"type":"message","content":"Task completed"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Message { content } => {
                assert_eq!(content, "Task completed");
            }
            _ => panic!("Expected Message event"),
        }
    }

    #[tokio::test]
    async fn test_parse_complete_event() {
        let json = r#"{"type":"complete"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Complete));
    }

    #[tokio::test]
    async fn test_parse_error_event() {
        let json = r#"{"type":"error","message":"Something went wrong"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Error { message } => {
                assert_eq!(message, "Something went wrong");
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[tokio::test]
    async fn test_stream_reader_with_json() {
        let input = r#"{"type":"thinking","content":"test"}
{"type":"complete"}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 2);

        match &outputs[0] {
            StreamOutput::Event(ClaudeEvent::Thinking { content }) => {
                assert_eq!(content, "test");
            }
            _ => panic!("Expected Thinking event"),
        }

        match &outputs[1] {
            StreamOutput::Event(ClaudeEvent::Complete) => {}
            _ => panic!("Expected Complete event"),
        }
    }

    #[tokio::test]
    async fn test_stream_reader_with_mixed_content() {
        let input =
            "Regular output line\n{\"type\":\"thinking\",\"content\":\"test\"}\nAnother line\n";
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 3);

        assert!(matches!(outputs[0], StreamOutput::RawLine(_)));
        assert!(matches!(outputs[1], StreamOutput::Event(_)));
        assert!(matches!(outputs[2], StreamOutput::RawLine(_)));
    }

    #[tokio::test]
    async fn test_stream_reader_with_malformed_json() {
        let input = "{\"type\":\"invalid\"\nNot JSON\n";
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 2);

        // Both should be treated as raw lines since they're malformed/invalid
        assert!(matches!(outputs[0], StreamOutput::RawLine(_)));
        assert!(matches!(outputs[1], StreamOutput::RawLine(_)));
    }

    #[tokio::test]
    async fn test_stream_reader_with_empty_lines() {
        let input = "\n\n{\"type\":\"complete\"}\n\n";
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        // Empty lines are preserved as RawLine
        assert_eq!(outputs.len(), 4);
    }
}
