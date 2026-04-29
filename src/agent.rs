//! Agent backend abstraction for multi-agent support.
//!
//! Defines the `AgentBackend` trait and `AgentEvent` normalized event model
//! that decouple core orchestration from any specific agent CLI implementation.
//!
//! These types are consumed by `agent_runner.rs`, `progress.rs`, and the
//! command modules (`fix.rs`, `review.rs`, `prompt.rs`, `resume.rs`).

use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Normalized event emitted by any agent backend.
///
/// This is the common event type that the `progress` and `fix` commands
/// consume, regardless of the underlying agent implementation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AgentEvent {
    /// Agent session has started (or a new message turn began).
    Started {
        /// Token usage from the initial message (e.g., input tokens, cache tokens).
        /// Backends that report per-message input usage populate this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// Agent is thinking / processing.
    Thinking {
        /// Optional thinking text, if exposed by the backend.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Agent is invoking a tool.
    ToolUse {
        /// Name of the tool being invoked
        tool_name: String,
        /// Unique identifier for this tool invocation
        tool_use_id: String,
        /// Human-readable summary of the tool call (e.g., "Run: git status").
        /// Populated by backends that can determine tool input before emitting
        /// the event. `None` when input is unknown or not applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_summary: Option<String>,
    },
    /// Result of a tool invocation.
    ToolResult {
        /// Identifier of the tool invocation this result belongs to.
        tool_use_id: String,
        /// Tool output content, if available.
        #[serde(default)]
        content: String,
        /// Whether the tool invocation was an error.
        #[serde(default)]
        is_error: bool,
    },
    /// Incremental text output from the agent.
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

/// An `AgentEvent` with an optional timestamp for persistence.
///
/// When written to `events.jsonl`, events are wrapped with a `ts` field
/// recording the wall-clock time (RFC 3339). Legacy events without `ts`
/// deserialize with `ts: None`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct TimestampedEvent {
    /// Wall-clock timestamp when the event was recorded (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ts: Option<String>,

    /// The underlying agent event.
    #[serde(flatten)]
    pub(crate) event: AgentEvent,
}

impl TimestampedEvent {
    /// Wraps an `AgentEvent` with the current UTC time.
    #[cfg(test)]
    pub(crate) fn now(event: AgentEvent) -> Self {
        Self {
            ts: Some(chrono::Utc::now().to_rfc3339()),
            event,
        }
    }
}

/// Borrowing wrapper for serializing an `AgentEvent` with a timestamp
/// without cloning the event.
#[derive(Serialize)]
pub(crate) struct TimestampedEventRef<'a> {
    pub(crate) ts: &'a str,
    #[serde(flatten)]
    pub(crate) event: &'a AgentEvent,
}

/// Agent-agnostic accumulated token usage.
///
/// Tracks input and output token counts across an entire agent session.
/// Cache token fields are `Option<u64>` since not all backends support prompt
/// caching — `None` means the backend does not report cache metrics, while
/// `Some(0)` means caching is supported but no tokens were cached.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_read_input_tokens: Option<u64>,
}

impl TokenUsage {
    /// Returns total tokens (input + output).
    pub(crate) fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Format as a compact display string (e.g., "12.3k in / 4.5k out").
    pub(crate) fn display_compact(&self) -> String {
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
pub(crate) trait AgentBackend: Send + Sync {
    /// Returns the human-readable name of this agent backend (e.g., "claude-code").
    fn name(&self) -> &str;

    /// Build the command to start a new agent session.
    ///
    /// `github_host` is set as `GH_HOST` on the spawned process so that
    /// `gh` CLI commands target the correct GitHub instance without discovery.
    fn build_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand;

    /// Parse a single line of agent output into normalized events.
    ///
    /// Returns an empty `Vec` for lines that don't represent recognized events
    /// (e.g., raw log output, blank lines). May return multiple events when a
    /// single line contains batch results (e.g., multi-tool-result messages).
    /// Backends should silently skip unrecognized lines rather than returning
    /// errors, since agent output commonly includes non-event lines.
    fn parse_events(&self, line: &str) -> Vec<AgentEvent>;

    /// Build the command to resume an existing agent session.
    ///
    /// Returns `None` if the backend does not support resume.
    /// Callers can check `is_some()` to test for resume support.
    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> Option<TokioCommand>;

    /// Build the command to interactively resume an existing agent session.
    ///
    /// Unlike `build_resume_command` (which produces a headless/stream-json command
    /// for autonomous mode), this produces an interactive command suitable for
    /// `gru attach` — with inherited stdio, no `--print`, and no `--output-format`.
    ///
    /// Returns `None` if the backend does not support interactive resume.
    fn build_interactive_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        github_host: &str,
    ) -> Option<TokioCommand>;

    /// Build a command for a one-shot utility task (no session tracking, text output).
    ///
    /// Used for fire-and-forget invocations like merge-readiness judge where the
    /// caller just needs a single agent turn and plain-text output.
    ///
    /// `prompt_arg` is passed as a CLI argument to the underlying agent binary.
    /// Callers may either:
    ///
    /// - Pass the full prompt text directly, or
    /// - Pass `"-"` and stream the actual prompt on stdin (the convention used by
    ///   the merge-readiness judge for large prompts that may exceed arg limits).
    ///
    /// Backends must support the `"-"` stdin-sentinel convention.
    /// The command should produce plain-text output on stdout with piped stdio.
    fn build_oneshot_command(&self, worktree_path: &Path, prompt_arg: &str) -> TokioCommand;

    /// Build a command for a CI fix invocation.
    ///
    /// CI fix requires multiple turns (read failure logs → locate offending
    /// code → edit → run tests → commit). Backends intended for single-turn
    /// utility tasks (such as merge-readiness judging) should use
    /// `build_oneshot_command` instead; override this method when the backend
    /// needs a qualitatively different invocation for multi-turn fix cycles.
    ///
    /// The default implementation delegates to `build_oneshot_command`.
    ///
    /// **Implementor note:** If your `build_oneshot_command` imposes a
    /// single-turn limit (e.g., `--max-turns 1`), you **must** override this
    /// method — the default delegation will inherit that limit and CI fix
    /// will always fail to make meaningful progress. Backends whose
    /// `build_oneshot_command` does not impose such a limit may safely rely
    /// on the default.
    fn build_ci_fix_command(&self, worktree_path: &Path, prompt_arg: &str) -> TokioCommand {
        self.build_oneshot_command(worktree_path, prompt_arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_event_started_roundtrip() {
        let event = AgentEvent::Started { usage: None };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_thinking_roundtrip() {
        let event = AgentEvent::Thinking {
            text: Some("Let me analyze this...".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_thinking_no_text() {
        // Thinking without text (backend doesn't expose thinking content)
        let json = r#"{"type": "thinking"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event, AgentEvent::Thinking { text: None });
    }

    #[test]
    fn test_agent_event_thinking_none_omits_text() {
        // Thinking with None text should not serialize the text field
        let event = AgentEvent::Thinking { text: None };
        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("text").is_none());
    }

    #[test]
    fn test_agent_event_tool_result_roundtrip() {
        let event = AgentEvent::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: "file contents here".to_string(),
            is_error: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_tool_result_error() {
        let event = AgentEvent::ToolResult {
            tool_use_id: "tool_456".to_string(),
            content: "command not found".to_string(),
            is_error: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_tool_use_roundtrip() {
        let event = AgentEvent::ToolUse {
            tool_name: "Bash".to_string(),
            tool_use_id: "tool_123".to_string(),
            input_summary: None,
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
                cache_creation_input_tokens: Some(100),
                cache_read_input_tokens: Some(200),
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
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
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
            input_summary: None,
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
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.total_tokens(), 0);
    }

    #[test]
    fn test_token_usage_total() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
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
            cache_creation_input_tokens: Some(100),
            cache_read_input_tokens: Some(200),
        };
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(usage, deserialized);
    }

    #[test]
    fn test_token_usage_deserialize_missing_cache_fields() {
        // Cache fields should default to None when missing
        let json = r#"{"input_tokens": 100, "output_tokens": 50}"#;
        let usage: TokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    #[test]
    fn test_token_usage_none_cache_fields_omitted() {
        // When cache fields are None, they should not appear in serialized JSON
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("cache_creation_input_tokens").is_none());
        assert!(value.get("cache_read_input_tokens").is_none());
    }

    #[test]
    fn test_timestamped_event_roundtrip() {
        let te = TimestampedEvent::now(AgentEvent::Ping);
        assert!(te.ts.is_some());
        let json = serde_json::to_string(&te).unwrap();
        let deserialized: TimestampedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(te, deserialized);
    }

    #[test]
    fn test_timestamped_event_json_has_ts_and_type() {
        let te = TimestampedEvent::now(AgentEvent::Started { usage: None });
        let json = serde_json::to_string(&te).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("ts").is_some());
        assert_eq!(value["type"], "started");
    }

    #[test]
    fn test_timestamped_event_legacy_without_ts() {
        // Legacy events (bare AgentEvent JSON) should deserialize with ts: None
        let json = r#"{"type": "ping"}"#;
        let te: TimestampedEvent = serde_json::from_str(json).unwrap();
        assert_eq!(te.ts, None);
        assert_eq!(te.event, AgentEvent::Ping);
    }

    #[test]
    fn test_timestamped_event_legacy_tool_use_without_ts() {
        let json = r#"{"type":"tool_use","tool_name":"Read","tool_use_id":"abc"}"#;
        let te: TimestampedEvent = serde_json::from_str(json).unwrap();
        assert_eq!(te.ts, None);
        assert_eq!(
            te.event,
            AgentEvent::ToolUse {
                tool_name: "Read".to_string(),
                tool_use_id: "abc".to_string(),
                input_summary: None,
            }
        );
    }
}
