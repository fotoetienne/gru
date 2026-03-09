use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;

/// Token usage information from stream events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

/// Information about a message in a MessageStart event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MessageInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// A content block within a message (text or tool_use)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
    #[default]
    Unknown,
}

/// A delta update within a content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type")]
pub enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
    /// Catch-all for unknown delta types
    #[serde(other)]
    #[default]
    Unknown,
}

/// The body of a MessageDelta event (e.g., stop_reason)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// Usage information that appears alongside MessageDelta events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MessageDeltaUsage {
    #[serde(default)]
    pub output_tokens: u64,
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
        #[serde(default)]
        content_block: ContentBlock,
    },

    /// Delta/update to a content block
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[serde(default)]
        index: usize,
        #[serde(default)]
        delta: ContentDelta,
    },

    /// End of a content block
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        index: usize,
    },

    /// Delta/update to the message (e.g., stop_reason, usage)
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<MessageDeltaUsage>,
    },

    /// End of the message stream
    #[serde(rename = "message_stop")]
    MessageStop,

    /// Error event
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        error: ErrorInfo,
    },

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

/// Parse a trimmed line of Claude Code output into a structured `StreamOutput`.
///
/// Returns `None` if the line is empty or doesn't match any recognized format.
/// This is the shared parsing logic used by both `EventStream::next_line()` and
/// `ClaudeBackend::parse_event()`.
pub(crate) fn parse_line(trimmed: &str) -> Option<StreamOutput> {
    if trimmed.is_empty() {
        return None;
    }

    // Try stream_event wrapper from Claude Code
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if wrapper.get("type").and_then(|t| t.as_str()) == Some("stream_event") {
            if let Some(event_value) = wrapper.get("event") {
                if let Ok(event) = serde_json::from_value::<ClaudeEvent>(event_value.clone()) {
                    return Some(StreamOutput::Event(event));
                }
            }
        }
    }

    // Try conversation message (verbose output with tool results)
    if let Ok(conv_msg) = serde_json::from_str::<ConversationMessage>(trimmed) {
        if conv_msg.message_type == "user" && !conv_msg.message.content.is_empty() {
            return Some(StreamOutput::ToolResult(
                conv_msg.message.content[0].clone(),
            ));
        }
    }

    None
}

/// Accumulated token usage across a session
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Update from a message_start event's usage field.
    /// Note: output_tokens in message_start is always 0 per Anthropic's streaming spec;
    /// actual output token counts come from message_delta events.
    pub fn add_message_start(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens;
        if let Some(cache_creation) = usage.cache_creation_input_tokens {
            self.cache_creation_input_tokens += cache_creation;
        }
        if let Some(cache_read) = usage.cache_read_input_tokens {
            self.cache_read_input_tokens += cache_read;
        }
    }

    /// Update from a message_delta event's usage field
    pub fn add_message_delta(&mut self, usage: &MessageDeltaUsage) {
        self.output_tokens += usage.output_tokens;
    }

    /// Returns total tokens (input + output)
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Format as a compact display string (e.g., "12.3k in / 4.5k out")
    pub fn display_compact(&self) -> String {
        format!(
            "{} in / {} out",
            format_token_count(self.input_tokens),
            format_token_count(self.output_tokens)
        )
    }
}

/// Format a token count in a human-readable way (e.g., 1234 -> "1.2k", 1234567 -> "1.2M")
fn format_token_count(count: u64) -> String {
    if count >= 999_950 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{}", count)
    }
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

impl<R: tokio::io::AsyncRead + Unpin> EventStream<R> {
    /// Creates a new event stream from an async reader
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

        // Delegate to shared parsing logic
        if let Some(output) = parse_line(trimmed) {
            return Ok(Some(output));
        }

        // Not a recognized format, treat as raw line
        Ok(Some(StreamOutput::RawLine(line)))
    }

    /// Read all lines from the stream
    #[cfg(test)]
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

    #[test]
    fn test_parse_message_start_event() {
        let json = r#"{"type":"message_start","message":{"id":"msg_123","role":"assistant"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::MessageStart { .. }));
    }

    #[test]
    fn test_parse_content_block_start_event() {
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

    #[test]
    fn test_parse_content_block_delta_event() {
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

    #[test]
    fn test_parse_message_delta_event() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
            }
            _ => panic!("Expected MessageDelta event"),
        }
    }

    #[test]
    fn test_parse_message_stop_event() {
        let json = r#"{"type":"message_stop"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::MessageStop));
    }

    #[test]
    fn test_parse_error_event() {
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

    #[test]
    fn test_parse_ping_event() {
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

    #[test]
    fn test_parse_unknown_content_block_type() {
        let json = r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"some thought"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockStart {
                content_block: ContentBlock::Unknown,
                ..
            } => {}
            _ => panic!("Expected Unknown content block"),
        }
    }

    #[test]
    fn test_parse_unknown_content_delta_type() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"more thought"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockDelta {
                delta: ContentDelta::Unknown,
                ..
            } => {}
            _ => panic!("Expected Unknown delta"),
        }
    }

    #[test]
    fn test_parse_input_json_delta() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockDelta { delta, .. } => match delta {
                ContentDelta::InputJsonDelta { partial_json } => {
                    assert_eq!(partial_json, r#"{"key":"#);
                }
                _ => panic!("Expected InputJsonDelta"),
            },
            _ => panic!("Expected ContentBlockDelta event"),
        }
    }

    #[test]
    fn test_parse_text_content_block() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::ContentBlockStart {
                content_block: ContentBlock::Text { text },
                ..
            } => {
                assert_eq!(text, "");
            }
            _ => panic!("Expected Text content block"),
        }
    }

    #[test]
    fn test_parse_message_start_with_usage() {
        let json = r#"{"type":"message_start","message":{"id":"msg_123","role":"assistant","usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":50,"cache_read_input_tokens":25}}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::MessageStart { message } => {
                let usage = message.usage.unwrap();
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 0);
                assert_eq!(usage.cache_creation_input_tokens, Some(50));
                assert_eq!(usage.cache_read_input_tokens, Some(25));
            }
            _ => panic!("Expected MessageStart event"),
        }
    }

    #[test]
    fn test_parse_message_start_without_usage() {
        let json = r#"{"type":"message_start","message":{"id":"msg_123","role":"assistant"}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::MessageStart { message } => {
                assert!(message.usage.is_none());
            }
            _ => panic!("Expected MessageStart event"),
        }
    }

    #[test]
    fn test_parse_message_delta_with_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":500}}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        match event {
            ClaudeEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                let usage = usage.unwrap();
                assert_eq!(usage.output_tokens, 500);
            }
            _ => panic!("Expected MessageDelta event"),
        }
    }

    #[test]
    fn test_token_usage_accumulation() {
        let mut usage = TokenUsage::default();
        assert_eq!(usage.total_tokens(), 0);

        // Simulate message_start with input tokens
        usage.add_message_start(&Usage {
            input_tokens: 1000,
            output_tokens: 0,
            cache_creation_input_tokens: Some(200),
            cache_read_input_tokens: Some(100),
        });
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 100);

        // Simulate message_delta with output tokens
        usage.add_message_delta(&MessageDeltaUsage { output_tokens: 500 });
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens(), 1500);

        // Simulate another round (second message)
        usage.add_message_start(&Usage {
            input_tokens: 2000,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(500),
        });
        usage.add_message_delta(&MessageDeltaUsage { output_tokens: 300 });

        assert_eq!(usage.input_tokens, 3000);
        assert_eq!(usage.output_tokens, 800);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 600);
        assert_eq!(usage.total_tokens(), 3800);
    }

    #[test]
    fn test_token_usage_display_compact() {
        let usage = TokenUsage {
            input_tokens: 1234,
            output_tokens: 567,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        assert_eq!(usage.display_compact(), "1.2k in / 567 out");

        let large_usage = TokenUsage {
            input_tokens: 1_500_000,
            output_tokens: 50_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        assert_eq!(large_usage.display_compact(), "1.5M in / 50.0k out");
    }

    #[test]
    fn test_format_token_count() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(1500), "1.5k");
        assert_eq!(format_token_count(999_949), "999.9k");
        assert_eq!(format_token_count(999_950), "1.0M");
        assert_eq!(format_token_count(999_999), "1.0M");
        assert_eq!(format_token_count(1_000_000), "1.0M");
        assert_eq!(format_token_count(2_500_000), "2.5M");
    }

    #[test]
    fn test_token_usage_serialization() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 100,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(usage, deserialized);
    }

    #[test]
    fn test_token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.total_tokens(), 0);
    }
}
