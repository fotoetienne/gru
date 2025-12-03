use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Represents the different types of events that can be emitted by Claude Code
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type")]
#[allow(dead_code)]
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

/// Represents either a parsed Claude event or raw output
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum StreamOutput {
    Event(ClaudeEvent),
    RawLine(String),
}

/// Async event stream reader that reads from stdout line-by-line
#[allow(dead_code)]
pub struct EventStream<R> {
    reader: BufReader<R>,
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
    #[allow(dead_code)]
    pub async fn next_line(&mut self) -> std::io::Result<Option<StreamOutput>> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            return Ok(None);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Some(StreamOutput::RawLine(line)));
        }

        // Try to parse as JSON
        match serde_json::from_str::<ClaudeEvent>(trimmed) {
            Ok(event) => Ok(Some(StreamOutput::Event(event))),
            Err(_) => Ok(Some(StreamOutput::RawLine(line))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_thinking() {
        let json = r#"{"type":"thinking","content":"Analyzing..."}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Thinking { .. }));
        if let ClaudeEvent::Thinking { content } = event {
            assert_eq!(content, "Analyzing...");
        }
    }

    #[tokio::test]
    async fn test_parse_tool_use() {
        let json = r#"{"type":"tool_use","name":"grep","input":{"pattern":"test"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::ToolUse { .. }));
        if let ClaudeEvent::ToolUse { name, input } = event {
            assert_eq!(name, "grep");
            assert_eq!(input["pattern"], "test");
        }
    }

    #[tokio::test]
    async fn test_parse_message() {
        let json = r#"{"type":"message","content":"Hello, world!"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Message { .. }));
        if let ClaudeEvent::Message { content } = event {
            assert_eq!(content, "Hello, world!");
        }
    }

    #[tokio::test]
    async fn test_parse_complete() {
        let json = r#"{"type":"complete"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Complete));
    }

    #[tokio::test]
    async fn test_parse_error() {
        let json = r#"{"type":"error","message":"Something went wrong"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Error { .. }));
        if let ClaudeEvent::Error { message } = event {
            assert_eq!(message, "Something went wrong");
        }
    }

    #[tokio::test]
    async fn test_event_stream_parses_valid_json() {
        let input = r#"{"type":"thinking","content":"test"}
"#;
        let mut stream = EventStream::new(input.as_bytes());
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(
            output,
            StreamOutput::Event(ClaudeEvent::Thinking { .. })
        ));
    }

    #[tokio::test]
    async fn test_event_stream_handles_raw_lines() {
        let input = "This is not JSON\n";
        let mut stream = EventStream::new(input.as_bytes());
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::RawLine(_)));
    }

    #[tokio::test]
    async fn test_event_stream_handles_empty_lines() {
        let input = "\n";
        let mut stream = EventStream::new(input.as_bytes());
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::RawLine(_)));
    }

    #[tokio::test]
    async fn test_event_stream_handles_malformed_json() {
        let input = r#"{"type":"thinking","content":"incomplete"#;
        let mut stream = EventStream::new(input.as_bytes());
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::RawLine(_)));
    }

    #[tokio::test]
    async fn test_event_stream_mixed_content() {
        let input = r#"Some raw text
{"type":"message","content":"Hello"}
More raw text
{"type":"complete"}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        // First line: raw text
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::RawLine(_)));

        // Second line: valid message event
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(
            output,
            StreamOutput::Event(ClaudeEvent::Message { .. })
        ));

        // Third line: raw text
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::RawLine(_)));

        // Fourth line: complete event
        let output = stream.next_line().await.unwrap().unwrap();
        assert!(matches!(output, StreamOutput::Event(ClaudeEvent::Complete)));

        // End of stream
        let output = stream.next_line().await.unwrap();
        assert!(output.is_none());
    }
}
