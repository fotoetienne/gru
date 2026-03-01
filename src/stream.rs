use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};

/// Information about a message in a MessageStart event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MessageInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

/// A content block within a message (text or tool_use)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "tool_use")]
    ToolUse {
        #[serde(default)]
        name: String,
        #[serde(default)]
        id: String,
    },
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    /// Catch-all for unknown block types
    #[serde(other)]
    Unknown,
}

/// A delta update within a content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    /// Catch-all for unknown delta types
    #[serde(other)]
    Unknown,
}

/// The body of a MessageDelta event (e.g., stop_reason)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// Error information from the API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ErrorInfo {
    #[serde(rename = "type", default)]
    pub error_type: String,
    #[serde(default)]
    pub message: String,
}

/// Represents the different types of events that can be emitted by Claude Code
/// in stream-json mode. These follow the Anthropic Messages API streaming format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
    /// Start of a new message
    #[serde(rename = "message_start")]
    MessageStart {
        #[serde(default)]
        message: MessageInfo,
    },

    /// Start of a content block (text or tool_use)
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        #[serde(default)]
        index: usize,
        content_block: ContentBlock,
    },

    /// Delta/update to a content block
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[serde(default)]
        index: usize,
        delta: ContentDelta,
    },

    /// End of a content block
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        index: usize,
    },

    /// Delta/update to the message (e.g., stop_reason)
    #[serde(rename = "message_delta")]
    MessageDelta { delta: MessageDeltaBody },

    /// End of the message stream
    #[serde(rename = "message_stop")]
    MessageStop,

    /// Error event
    #[serde(rename = "error")]
    Error { error: ErrorInfo },

    /// Ping event (keepalive)
    #[serde(rename = "ping")]
    Ping,
}

/// Represents a tool result from the Messages API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    #[serde(rename = "type")]
    pub result_type: String,
    pub tool_use_id: String,
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
}

/// Represents a conversation message from the Messages API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub message: MessageContent,
}

/// Represents the content of a conversation message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageContent {
    pub role: String,
    pub content: Vec<ToolResult>,
}

/// Represents the output from parsing a stream line
#[derive(Debug, Clone, PartialEq)]
pub enum StreamOutput {
    /// A parsed Claude event
    Event(ClaudeEvent),
    /// A parsed tool result message
    ToolResult(ToolResult),
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

        // Try to parse as a conversation message (verbose output)
        if let Ok(conv_msg) = serde_json::from_str::<ConversationMessage>(trimmed) {
            if conv_msg.message_type == "user" && !conv_msg.message.content.is_empty() {
                // Return the first tool result (typically there's only one per message)
                // The is_empty() guard above ensures that direct indexing is safe here.
                return Ok(Some(StreamOutput::ToolResult(
                    conv_msg.message.content[0].clone(),
                )));
            }
        }

        // Not a recognized format, treat as raw line
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
        let json = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Bash","id":"toolu_1"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 0);
                match content_block {
                    ContentBlock::ToolUse { name, id } => {
                        assert_eq!(name, "Bash");
                        assert_eq!(id, "toolu_1");
                    }
                    _ => panic!("Expected ToolUse content block"),
                }
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
                match delta {
                    ContentDelta::TextDelta { text } => assert_eq!(text, "Hello"),
                    _ => panic!("Expected TextDelta"),
                }
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
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
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
                assert_eq!(error.error_type, "api_error");
                assert_eq!(error.message, "Something went wrong");
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

    #[tokio::test]
    async fn test_stream_reader_with_tool_result() {
        // Test parsing tool result messages from verbose output
        let input = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"output data","is_error":false}]}}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 1);

        match &outputs[0] {
            StreamOutput::ToolResult(tool_result) => {
                assert_eq!(tool_result.result_type, "tool_result");
                assert_eq!(tool_result.tool_use_id, "toolu_123");
                assert!(!tool_result.is_error);
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn test_stream_reader_with_tool_result_error() {
        // Test parsing tool result with error
        let input = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_456","content":"error message","is_error":true}]}}
"#;
        let mut stream = EventStream::new(input.as_bytes());

        let outputs = stream.read_all().await.unwrap();
        assert_eq!(outputs.len(), 1);

        match &outputs[0] {
            StreamOutput::ToolResult(tool_result) => {
                assert_eq!(tool_result.result_type, "tool_result");
                assert_eq!(tool_result.tool_use_id, "toolu_456");
                assert!(tool_result.is_error);
            }
            _ => panic!("Expected ToolResult with error"),
        }
    }
}
