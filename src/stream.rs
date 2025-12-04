use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};

/// Represents the different types of events that can be emitted by Claude Code
/// in stream-json mode. These follow the Anthropic Messages API streaming format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
    /// Start of a new message
    #[serde(rename = "message_start")]
    MessageStart {
        #[serde(default)]
        message: serde_json::Value,
    },

    /// Start of a content block (text or tool_use)
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        #[serde(default)]
        index: usize,
        #[serde(default)]
        content_block: serde_json::Value,
    },

    /// Delta/update to a content block
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[serde(default)]
        index: usize,
        #[serde(default)]
        delta: serde_json::Value,
    },

    /// End of a content block
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        index: usize,
    },

    /// Delta/update to the message (e.g., stop_reason)
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        delta: serde_json::Value,
    },

    /// End of the message stream
    #[serde(rename = "message_stop")]
    MessageStop,

    /// Error event
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        error: serde_json::Value,
    },

    /// Ping event (keepalive)
    #[serde(rename = "ping")]
    Ping,
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
        // First check if it's a stream_event wrapper from Claude Code
        if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if wrapper.get("type").and_then(|t| t.as_str()) == Some("stream_event") {
                // Extract the inner event and try to parse it
                if let Some(event_value) = wrapper.get("event") {
                    if let Ok(event) = serde_json::from_value::<ClaudeEvent>(event_value.clone()) {
                        return Ok(Some(StreamOutput::Event(event)));
                    }
                }
            }
        }

        // Not a stream_event, treat as raw line
        Ok(Some(StreamOutput::RawLine(line)))
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
    async fn test_parse_message_start_event() {
        let json = r#"{"type":"message_start","message":{"id":"msg_123","role":"assistant"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::MessageStart { .. }));
    }

    #[tokio::test]
    async fn test_parse_content_block_start_event() {
        let json = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Bash"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 0);
                assert_eq!(
                    content_block.get("type").and_then(|t| t.as_str()),
                    Some("tool_use")
                );
                assert_eq!(
                    content_block.get("name").and_then(|n| n.as_str()),
                    Some("Bash")
                );
            }
            _ => panic!("Expected ContentBlockStart event"),
        }
    }

    #[tokio::test]
    async fn test_parse_content_block_delta_event() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(
                    delta.get("type").and_then(|t| t.as_str()),
                    Some("text_delta")
                );
                assert_eq!(delta.get("text").and_then(|t| t.as_str()), Some("Hello"));
            }
            _ => panic!("Expected ContentBlockDelta event"),
        }
    }

    #[tokio::test]
    async fn test_parse_message_delta_event() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::MessageDelta { delta } => {
                assert_eq!(
                    delta.get("stop_reason").and_then(|r| r.as_str()),
                    Some("end_turn")
                );
            }
            _ => panic!("Expected MessageDelta event"),
        }
    }

    #[tokio::test]
    async fn test_parse_message_stop_event() {
        let json = r#"{"type":"message_stop"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::MessageStop));
    }

    #[tokio::test]
    async fn test_parse_error_event() {
        let json =
            r#"{"type":"error","error":{"type":"api_error","message":"Something went wrong"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::Error { error } => {
                assert_eq!(
                    error.get("message").and_then(|m| m.as_str()),
                    Some("Something went wrong")
                );
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[tokio::test]
    async fn test_parse_ping_event() {
        let json = r#"{"type":"ping"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Ping));
    }

    #[tokio::test]
    async fn test_stream_reader_with_json() {
        // Using Claude Code wrapper format
        let input = r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}
{"type":"stream_event","event":{"type":"message_stop"}}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 2);

        match &outputs[0] {
            StreamOutput::Event(ClaudeEvent::MessageStart { .. }) => {}
            _ => panic!("Expected MessageStart event"),
        }

        match &outputs[1] {
            StreamOutput::Event(ClaudeEvent::MessageStop) => {}
            _ => panic!("Expected MessageStop event"),
        }
    }

    #[tokio::test]
    async fn test_stream_reader_with_mixed_content() {
        // Using Claude Code wrapper format
        let input = "Regular output line\n{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{}}}\nAnother line\n";
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
        let input = "\n\n{\"type\":\"message_stop\"}\n\n";
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        // Empty lines are preserved as RawLine
        assert_eq!(outputs.len(), 4);
    }

    #[tokio::test]
    async fn test_stream_reader_with_claude_code_wrapper() {
        // Test the actual Claude Code stream_event wrapper format
        let input = r#"{"type":"stream_event","event":{"type":"message_start","message":{}},"session_id":"test","uuid":"test"}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}},"session_id":"test","uuid":"test"}
{"type":"stream_event","event":{"type":"message_stop"},"session_id":"test","uuid":"test"}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 3);

        // All should be parsed as events, not raw lines
        match &outputs[0] {
            StreamOutput::Event(ClaudeEvent::MessageStart { .. }) => {}
            _ => panic!("Expected MessageStart event"),
        }

        match &outputs[1] {
            StreamOutput::Event(ClaudeEvent::ContentBlockDelta { .. }) => {}
            _ => panic!("Expected ContentBlockDelta event"),
        }

        match &outputs[2] {
            StreamOutput::Event(ClaudeEvent::MessageStop) => {}
            _ => panic!("Expected MessageStop event"),
        }
    }
}
