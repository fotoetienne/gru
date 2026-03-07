//! Agent backend abstraction for multi-agent support.
//!
//! Defines the `AgentBackend` trait and `AgentEvent` normalized event model
//! that decouple core orchestration from any specific agent CLI implementation.
//!
//! These types are not yet consumed by the rest of the codebase — they will be
//! integrated in subsequent Phase 1 issues. The `allow(dead_code)` will be
//! removed once consumers exist.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Normalized event emitted by any agent backend.
///
/// This is the common event type that `progress.rs` and `fix.rs` consume,
/// regardless of the underlying agent implementation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Agent session has started
    Started,
    /// Agent is thinking / processing
    Thinking,
    /// Agent is invoking a tool
    ToolUse {
        /// Name of the tool being invoked
        tool_name: String,
        /// Unique identifier for this tool invocation
        tool_use_id: String,
    },
    /// Incremental text output from the agent
    TextDelta {
        /// The text fragment
        text: String,
    },
    /// A complete message has been produced
    MessageComplete {
        /// Reason the message ended (e.g., "end_turn", "tool_use")
        stop_reason: Option<String>,
        /// Token usage for this message, if available
        usage: Option<TokenUsage>,
    },
    /// Agent has finished execution
    Finished {
        /// Token usage for the entire session, if available
        usage: Option<TokenUsage>,
    },
    /// An error occurred
    Error {
        /// Error message
        message: String,
    },
    /// Keepalive / heartbeat signal
    Ping,
}

/// Agent-agnostic accumulated token usage.
///
/// Tracks input and output token counts across an entire agent session.
/// Cache token fields are optional since not all backends support prompt caching.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Returns total tokens (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Format as a compact display string (e.g., "12.3k in / 4.5k out").
    pub fn display_compact(&self) -> String {
        format!(
            "{} in / {} out",
            format_token_count(self.input_tokens),
            format_token_count(self.output_tokens)
        )
    }
}

/// Format a token count in a human-readable way (e.g., 1234 -> "1.2k", 1234567 -> "1.2M").
fn format_token_count(count: u64) -> String {
    if count >= 999_950 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{count}")
    }
}

/// Trait abstracting agent backend interaction.
///
/// Implementations of this trait allow Gru to work with different agent CLIs
/// (e.g., Claude Code, Aider, Codex) without changes to core orchestration code.
///
/// The trait is `Send + Sync` for async compatibility.
pub trait AgentBackend: Send + Sync {
    /// Returns the human-readable name of this agent backend (e.g., "claude-code").
    fn name(&self) -> &str;

    /// Build the command to start a new agent session.
    fn build_command(&self, worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand;

    /// Parse a single line of agent output into a normalized event.
    ///
    /// Returns `None` for lines that don't represent a recognized event
    /// (e.g., raw log output, blank lines). Backends should silently skip
    /// unrecognized lines rather than returning errors, since agent output
    /// commonly includes non-event lines.
    fn parse_event(&self, line: &str) -> Option<AgentEvent>;

    /// Whether this backend supports resuming a previous session.
    fn supports_resume(&self) -> bool;

    /// Build the command to resume an existing agent session.
    ///
    /// Returns `None` if the backend does not support resume.
    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
    ) -> Option<TokioCommand>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_event_started_roundtrip() {
        let event = AgentEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_thinking_roundtrip() {
        let event = AgentEvent::Thinking;
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_tool_use_roundtrip() {
        let event = AgentEvent::ToolUse {
            tool_name: "Bash".to_string(),
            tool_use_id: "tool_123".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_text_delta_roundtrip() {
        let event = AgentEvent::TextDelta {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_message_complete_roundtrip() {
        let event = AgentEvent::MessageComplete {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(TokenUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_input_tokens: 100,
                cache_read_input_tokens: 200,
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_message_complete_no_usage() {
        let event = AgentEvent::MessageComplete {
            stop_reason: None,
            usage: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_finished_roundtrip() {
        let event = AgentEvent::Finished {
            usage: Some(TokenUsage {
                input_tokens: 5000,
                output_tokens: 2000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_error_roundtrip() {
        let event = AgentEvent::Error {
            message: "Something went wrong".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_ping_roundtrip() {
        let event = AgentEvent::Ping;
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_json_format() {
        // Verify the tagged enum produces the expected JSON structure
        let event = AgentEvent::ToolUse {
            tool_name: "Read".to_string(),
            tool_use_id: "abc".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "tool_use");
        assert_eq!(value["tool_name"], "Read");
        assert_eq!(value["tool_use_id"], "abc");
    }

    #[test]
    fn test_agent_event_deserialize_from_json_object() {
        let json = r#"{"type": "text_delta", "text": "hello"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "hello".to_string()
            }
        );
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

    #[test]
    fn test_token_usage_total() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        assert_eq!(usage.total_tokens(), 1500);
    }

    #[test]
    fn test_token_usage_display_compact() {
        let usage = TokenUsage {
            input_tokens: 12345,
            output_tokens: 4567,
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "12.3k in / 4.6k out");
    }

    #[test]
    fn test_token_usage_display_compact_millions() {
        let usage = TokenUsage {
            input_tokens: 1_500_000,
            output_tokens: 750_000,
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "1.5M in / 750.0k out");
    }

    #[test]
    fn test_token_usage_display_compact_small() {
        let usage = TokenUsage {
            input_tokens: 42,
            output_tokens: 7,
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "42 in / 7 out");
    }

    #[test]
    fn test_token_usage_roundtrip() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(usage, deserialized);
    }

    #[test]
    fn test_token_usage_deserialize_missing_cache_fields() {
        // Cache fields should default to 0 when missing
        let json = r#"{"input_tokens": 100, "output_tokens": 50}"#;
        let usage: TokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
    }
}
