//! Claude Code backend implementation for the `AgentBackend` trait.
//!
//! Wraps the existing Claude Code CLI integration (`stream.rs` parsing,
//! `claude_runner.rs` command building) behind the normalized `AgentBackend`
//! interface. This is the first concrete backend and should produce identical
//! behavior to the pre-refactor code paths.
//!
//! These types are not yet consumed by the rest of the codebase — they will be
//! integrated when `fix.rs` and `resume.rs` are migrated to use `AgentBackend`.
//! The `allow(dead_code)` will be removed once consumers exist.
#![allow(dead_code)]

use crate::agent::{AgentBackend, AgentEvent, TokenUsage as AgentTokenUsage};
use crate::stream::{ClaudeEvent, ContentBlock, ContentDelta, ConversationMessage, StreamOutput};
use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Claude Code CLI backend.
///
/// Implements `AgentBackend` by delegating to the existing Claude CLI invocation
/// flags and Anthropic Messages API stream parsing.
pub struct ClaudeBackend;

impl ClaudeBackend {
    pub fn new() -> Self {
        Self
    }

    /// Parse a raw line into a `StreamOutput` (internal helper).
    ///
    /// This mirrors the logic in `EventStream::next_line()` but operates on
    /// an already-read `&str` instead of an async reader, so that `parse_event`
    /// can satisfy the synchronous `AgentBackend` trait method.
    fn parse_stream_output(line: &str) -> Option<StreamOutput> {
        let trimmed = line.trim();
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

    /// Map a `ClaudeEvent` to an `AgentEvent`.
    fn map_claude_event(event: &ClaudeEvent) -> Option<AgentEvent> {
        match event {
            ClaudeEvent::MessageStart { .. } => Some(AgentEvent::Started),

            ClaudeEvent::ContentBlockStart { content_block, .. } => match content_block {
                ContentBlock::ToolUse { name, id } => Some(AgentEvent::ToolUse {
                    tool_name: name.clone(),
                    tool_use_id: id.clone(),
                }),
                ContentBlock::Text { .. } => None,
                ContentBlock::Unknown => None,
            },

            ClaudeEvent::ContentBlockDelta { delta, .. } => match delta {
                ContentDelta::TextDelta { text } => {
                    Some(AgentEvent::TextDelta { text: text.clone() })
                }
                ContentDelta::InputJsonDelta { .. } => None,
                ContentDelta::Unknown => None,
            },

            ClaudeEvent::ContentBlockStop { .. } => None,

            ClaudeEvent::MessageDelta { delta, usage } => {
                let agent_usage = usage.as_ref().map(|u| AgentTokenUsage {
                    output_tokens: u.output_tokens,
                    ..Default::default()
                });
                Some(AgentEvent::MessageComplete {
                    stop_reason: delta.stop_reason.clone(),
                    usage: agent_usage,
                })
            }

            ClaudeEvent::MessageStop => None,

            ClaudeEvent::Error { error } => Some(AgentEvent::Error {
                message: error.message.clone(),
            }),

            ClaudeEvent::Ping => Some(AgentEvent::Ping),
        }
    }
}

impl AgentBackend for ClaudeBackend {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn build_command(&self, worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand {
        let mut cmd = TokioCommand::new("claude");
        cmd.arg("--print")
            .arg("--verbose")
            .arg("--session-id")
            .arg(session_id.to_string())
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--dangerously-skip-permissions")
            .arg(prompt)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .current_dir(worktree_path);
        cmd
    }

    fn parse_event(&self, line: &str) -> Option<AgentEvent> {
        let stream_output = Self::parse_stream_output(line)?;
        match stream_output {
            StreamOutput::Event(ref claude_event) => Self::map_claude_event(claude_event),
            StreamOutput::ToolResult(ref tool_result) => {
                let content = match &tool_result.content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                Some(AgentEvent::ToolResult {
                    tool_use_id: tool_result.tool_use_id.clone(),
                    content,
                    is_error: tool_result.is_error,
                })
            }
            StreamOutput::RawLine(_) => None,
        }
    }

    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
    ) -> Option<TokioCommand> {
        let mut cmd = TokioCommand::new("claude");
        cmd.arg("--print")
            .arg("--verbose")
            .arg("--resume")
            .arg(session_id.to_string())
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--dangerously-skip-permissions")
            .arg(prompt)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .current_dir(worktree_path);
        Some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> ClaudeBackend {
        ClaudeBackend::new()
    }

    #[test]
    fn test_name() {
        assert_eq!(backend().name(), "claude-code");
    }

    #[test]
    fn test_build_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "claude");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--print".as_ref()));
        assert!(args.contains(&"--verbose".as_ref()));
        assert!(args.contains(&"--session-id".as_ref()));
        assert!(args.contains(&"--output-format".as_ref()));
        assert!(args.contains(&"stream-json".as_ref()));
        assert!(args.contains(&"--include-partial-messages".as_ref()));
        assert!(args.contains(&"--dangerously-skip-permissions".as_ref()));
        assert!(args.contains(&"fix the bug".as_ref()));
        // Should NOT contain --resume
        assert!(!args.contains(&"--resume".as_ref()));
    }

    #[test]
    fn test_build_resume_command_uses_resume_flag() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_resume_command(&path, &session_id, "continue")
            .expect("resume should be supported");
        let inner = cmd.as_std();

        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--resume".as_ref()));
        // Should NOT contain --session-id
        assert!(!args.contains(&"--session-id".as_ref()));
    }

    #[test]
    fn test_resume_is_supported() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp");
        let id = Uuid::nil();
        assert!(b.build_resume_command(&path, &id, "p").is_some());
    }

    // ---- parse_event tests ----

    #[test]
    fn test_parse_event_message_start() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(event, AgentEvent::Started);
    }

    #[test]
    fn test_parse_event_tool_use() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Bash","id":"toolu_1"}}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::ToolUse {
                tool_name: "Bash".to_string(),
                tool_use_id: "toolu_1".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_text_delta() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello world"}}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "Hello world".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_message_delta() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}}"#;
        let event = b.parse_event(line).unwrap();
        match event {
            AgentEvent::MessageComplete { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(usage.unwrap().output_tokens, 42);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_error() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"error","error":{"type":"api_error","message":"rate limited"}}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "rate limited".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_ping() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"ping"}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(event, AgentEvent::Ping);
    }

    #[test]
    fn test_parse_event_tool_result_from_verbose() {
        let b = backend();
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"file contents","is_error":false}]}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::ToolResult {
                tool_use_id: "toolu_123".to_string(),
                content: "file contents".to_string(),
                is_error: false,
            }
        );
    }

    #[test]
    fn test_parse_event_tool_result_error() {
        let b = backend();
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_456","content":"command not found","is_error":true}]}}"#;
        let event = b.parse_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::ToolResult {
                tool_use_id: "toolu_456".to_string(),
                content: "command not found".to_string(),
                is_error: true,
            }
        );
    }

    #[test]
    fn test_parse_event_raw_line_returns_none() {
        let b = backend();
        assert!(b.parse_event("some random output").is_none());
    }

    #[test]
    fn test_parse_event_empty_line_returns_none() {
        let b = backend();
        assert!(b.parse_event("").is_none());
        assert!(b.parse_event("   ").is_none());
    }

    #[test]
    fn test_parse_event_content_block_stop_returns_none() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#;
        assert!(b.parse_event(line).is_none());
    }

    #[test]
    fn test_parse_event_message_stop_returns_none() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
        assert!(b.parse_event(line).is_none());
    }

    #[test]
    fn test_parse_event_input_json_delta_returns_none() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}}"#;
        assert!(b.parse_event(line).is_none());
    }

    #[test]
    fn test_parse_event_text_content_block_start_returns_none() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert!(b.parse_event(line).is_none());
    }

    #[test]
    fn test_parse_event_tool_names_preserved() {
        let b = backend();
        // Verify various Claude tool names pass through correctly
        for tool in &["Bash", "Read", "Write", "Edit", "Glob", "Grep", "Agent"] {
            let line = format!(
                r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","name":"{}","id":"t1"}}}}}}"#,
                tool
            );
            let event = b.parse_event(&line).unwrap();
            match event {
                AgentEvent::ToolUse { tool_name, .. } => {
                    assert_eq!(tool_name, *tool);
                }
                other => panic!("Expected ToolUse for {}, got {:?}", tool, other),
            }
        }
    }
}
