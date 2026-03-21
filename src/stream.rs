//! Claude Code stream parsing module.
//!
//! Contains Claude-specific event types and stream parsing logic.
//! The public API is consumed exclusively by `ClaudeBackend::parse_events()`.

use serde::{Deserialize, Serialize};

/// Token usage information from stream events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) input_tokens: u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u64>,
}

/// Information about a message in a MessageStart event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub(crate) struct MessageInfo {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

/// A content block within a message (text or tool_use)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type")]
pub(crate) enum ContentBlock {
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
pub(crate) enum ContentDelta {
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
pub(crate) struct MessageDeltaBody {
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
}

/// Usage information that appears alongside MessageDelta events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub(crate) struct MessageDeltaUsage {
    #[serde(default)]
    pub(crate) output_tokens: u64,
}

/// Error information from the API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub(crate) struct ErrorInfo {
    #[serde(rename = "type", default)]
    pub(crate) error_type: String,
    #[serde(default)]
    pub(crate) message: String,
}

/// Represents the different types of events that can be emitted by Claude Code
/// in stream-json mode. These follow the Anthropic Messages API streaming format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub(crate) enum ClaudeEvent {
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
pub(crate) struct ToolResult {
    #[serde(rename = "type")]
    pub(crate) result_type: String,
    pub(crate) tool_use_id: String,
    pub(crate) content: serde_json::Value,
    #[serde(default)]
    pub(crate) is_error: bool,
}

/// Represents a conversation message from the Messages API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ConversationMessage {
    #[serde(rename = "type")]
    pub(crate) message_type: String,
    pub(crate) message: MessageContent,
}

/// Represents the content of a conversation message.
/// Content is parsed as raw JSON values to tolerate mixed content types
/// (e.g., text + tool_result items) without failing deserialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MessageContent {
    pub(crate) role: String,
    pub(crate) content: Vec<serde_json::Value>,
}

/// Represents the output from parsing a stream line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StreamOutput {
    /// A parsed Claude event
    Event(ClaudeEvent),
    /// A parsed tool result message
    ToolResult(ToolResult),
}

/// Parse a trimmed line of Claude Code output into structured `StreamOutput` values.
///
/// Returns an empty `Vec` if the line is empty or doesn't match any recognized format.
/// Called exclusively by `ClaudeBackend::parse_events()`.
pub(crate) fn parse_line(trimmed: &str) -> Vec<StreamOutput> {
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Try stream_event wrapper from Claude Code
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if wrapper.get("type").and_then(|t| t.as_str()) == Some("stream_event") {
            if let Some(event_value) = wrapper.get("event") {
                if let Ok(event) = serde_json::from_value::<ClaudeEvent>(event_value.clone()) {
                    return vec![StreamOutput::Event(event)];
                }
            }
        }
    }

    // Try conversation message (verbose output with tool results).
    // Content is parsed as raw JSON to tolerate mixed content types (e.g., text + tool_result).
    if let Ok(conv_msg) = serde_json::from_str::<ConversationMessage>(trimmed) {
        if conv_msg.message_type == "user" {
            let results: Vec<StreamOutput> = conv_msg
                .message
                .content
                .into_iter()
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|v| serde_json::from_value::<ToolResult>(v).ok())
                .map(StreamOutput::ToolResult)
                .collect();
            if !results.is_empty() {
                return results;
            }
        }
    }

    Vec::new()
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

    #[test]
    fn test_parse_line_multiple_tool_results() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"output 1","is_error":false},{"type":"tool_result","tool_use_id":"toolu_b","content":"output 2","is_error":false}]}}"#;
        let outputs = parse_line(line);
        assert_eq!(outputs.len(), 2);
        match &outputs[0] {
            StreamOutput::ToolResult(tr) => assert_eq!(tr.tool_use_id, "toolu_a"),
            _ => panic!("Expected ToolResult"),
        }
        match &outputs[1] {
            StreamOutput::ToolResult(tr) => assert_eq!(tr.tool_use_id, "toolu_b"),
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn test_parse_line_single_tool_result() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":"data","is_error":false}]}}"#;
        let outputs = parse_line(line);
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    fn test_parse_line_empty() {
        assert!(parse_line("").is_empty());
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
}
