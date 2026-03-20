//! Claude Code backend implementation for the `AgentBackend` trait.
//!
//! Wraps the existing Claude Code CLI integration (`stream.rs` parsing,
//! `claude_runner.rs` command building) behind the normalized `AgentBackend`
//! interface. This is the first concrete backend and should produce identical
//! behavior to the pre-refactor code paths.
//!
//! Now consumed by `agent_runner.rs` and the command modules.

use crate::agent::{AgentBackend, AgentEvent, TokenUsage as AgentTokenUsage};
use crate::claude_runner;
use crate::display_utils::{shorten_path, truncate_string};
use crate::stream::{self, ClaudeEvent, ContentBlock, ContentDelta, StreamOutput};
use std::path::Path;
use std::sync::Mutex;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Buffered state for a tool invocation whose input JSON is still arriving.
struct ToolBuffer {
    tool_name: String,
    tool_use_id: String,
    input_json: String,
}

/// Claude Code CLI backend.
///
/// Implements `AgentBackend` by delegating to the existing Claude CLI invocation
/// flags and Anthropic Messages API stream parsing.
///
/// The backend buffers `ContentBlockStart(ToolUse)` and `InputJsonDelta` events
/// internally, emitting a single `AgentEvent::ToolUse` with a populated
/// `input_summary` when `ContentBlockStop` arrives. This eliminates the UX
/// regression of showing "Tool: Bash" instead of "Run: git status".
#[derive(Default)]
pub struct ClaudeBackend {
    tool_buffer: Mutex<Option<ToolBuffer>>,
}

impl ClaudeBackend {
    /// Map a `ClaudeEvent` to an `AgentEvent`, using internal state to buffer
    /// tool invocations until their input JSON is complete.
    fn map_event(&self, event: &ClaudeEvent) -> Option<AgentEvent> {
        match event {
            ClaudeEvent::MessageStart { message } => {
                let agent_usage = message.usage.as_ref().map(|u| AgentTokenUsage {
                    input_tokens: u.input_tokens,
                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                    cache_read_input_tokens: u.cache_read_input_tokens,
                    ..Default::default()
                });
                Some(AgentEvent::Started { usage: agent_usage })
            }

            ClaudeEvent::ContentBlockStart { content_block, .. } => match content_block {
                ContentBlock::ToolUse { name, id } => {
                    // Buffer the tool — don't emit yet. We'll emit on ContentBlockStop
                    // once we have the full input JSON for a meaningful summary.
                    let mut buf = self.tool_buffer.lock().unwrap_or_else(|e| e.into_inner());
                    *buf = Some(ToolBuffer {
                        tool_name: name.clone(),
                        tool_use_id: id.clone(),
                        input_json: String::new(),
                    });
                    None
                }
                ContentBlock::Text { .. } => None,
                ContentBlock::Unknown => None,
            },

            ClaudeEvent::ContentBlockDelta { delta, .. } => match delta {
                ContentDelta::TextDelta { text } => {
                    Some(AgentEvent::TextDelta { text: text.clone() })
                }
                ContentDelta::InputJsonDelta { partial_json } => {
                    // Accumulate into the tool buffer
                    let mut buf = self.tool_buffer.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ref mut tb) = *buf {
                        tb.input_json.push_str(partial_json);
                    }
                    None
                }
                ContentDelta::Unknown => None,
            },

            ClaudeEvent::ContentBlockStop { .. } => {
                // If we have a buffered tool, emit it now with a formatted summary
                let mut buf = self.tool_buffer.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(tb) = buf.take() {
                    let summary = format_tool_summary(&tb.tool_name, &tb.input_json);
                    Some(AgentEvent::ToolUse {
                        tool_name: tb.tool_name,
                        tool_use_id: tb.tool_use_id,
                        input_summary: Some(summary),
                    })
                } else {
                    // Text block ended — no event needed (text was already emitted via TextDelta)
                    None
                }
            }

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

            ClaudeEvent::MessageStop => Some(AgentEvent::MessageComplete {
                stop_reason: None,
                usage: None,
            }),

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

    fn build_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        claude_runner::build_claude_command(worktree_path, session_id, prompt, github_host)
    }

    fn parse_events(&self, line: &str) -> Vec<AgentEvent> {
        let stream_outputs = stream::parse_line(line.trim());
        stream_outputs
            .into_iter()
            .filter_map(|output| match output {
                StreamOutput::Event(ref claude_event) => self.map_event(claude_event),
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
            })
            .collect()
    }

    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> Option<TokioCommand> {
        Some(claude_runner::build_claude_resume_command(
            worktree_path,
            session_id,
            prompt,
            github_host,
        ))
    }

    fn build_interactive_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        github_host: &str,
    ) -> Option<TokioCommand> {
        let mut cmd = TokioCommand::new("claude");
        cmd.arg("--resume")
            .arg(session_id.to_string())
            .current_dir(worktree_path)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env("GH_HOST", github_host);
        Some(cmd)
    }
}

// ---------------------------------------------------------------------------
// Tool input formatting (Claude-specific tool schemas)
// ---------------------------------------------------------------------------

/// Formats a human-readable summary of a tool call from its name and input JSON.
fn format_tool_summary(tool_name: &str, input_json: &str) -> String {
    let input: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return format!("Tool: {}", tool_name),
    };

    match tool_name {
        "Bash" => {
            if let Some(command) = input.get("command").and_then(|c| c.as_str()) {
                let truncated = truncate_string(command, 60);
                format!("Run: {}", truncated)
            } else {
                "Run: bash command".to_string()
            }
        }
        "Read" => {
            if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                format!("Read: {}", shorten_path(file_path))
            } else {
                "Read: file".to_string()
            }
        }
        "Write" => {
            if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                format!("Write: {}", shorten_path(file_path))
            } else {
                "Write: file".to_string()
            }
        }
        "Edit" => {
            if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                format!("Edit: {}", shorten_path(file_path))
            } else {
                "Edit: file".to_string()
            }
        }
        "Grep" => {
            if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                let truncated = truncate_string(pattern, 40);
                format!("Search: {}", truncated)
            } else {
                "Search: pattern".to_string()
            }
        }
        "Glob" => {
            if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                let truncated = truncate_string(pattern, 40);
                format!("Find: {}", truncated)
            } else {
                "Find: files".to_string()
            }
        }
        "Task" | "Agent" => {
            if let Some(description) = input.get("description").and_then(|d| d.as_str()) {
                let truncated = truncate_string(description, 50);
                format!("Task: {}", truncated)
            } else {
                "Task: running agent".to_string()
            }
        }
        "TodoWrite" => "Update todos".to_string(),
        "AskUserQuestion" => "Asking question...".to_string(),
        _ => format!("Tool: {}", tool_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> ClaudeBackend {
        ClaudeBackend::default()
    }

    /// Assert that parse_events returns exactly one event and return it.
    fn single(events: Vec<AgentEvent>) -> AgentEvent {
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event, got {}",
            events.len()
        );
        events.into_iter().next().unwrap()
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
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
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
        assert_eq!(*args.last().unwrap(), std::ffi::OsStr::new("fix the bug"));
        assert!(!args.contains(&"--resume".as_ref()));

        // Verify GH_HOST is set
        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())),
            "GH_HOST should be set on the command"
        );
    }

    #[test]
    fn test_build_command_sets_ghe_host() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.example.com");
        let inner = cmd.as_std();

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.example.com".as_ref())),
            "GH_HOST should be set to the GHE host"
        );
    }

    #[test]
    fn test_build_resume_command_uses_resume_flag() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_resume_command(&path, &session_id, "continue", "github.com")
            .expect("resume should be supported");
        let inner = cmd.as_std();

        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--resume".as_ref()));
        assert!(!args.contains(&"--session-id".as_ref()));
    }

    #[test]
    fn test_resume_is_supported() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp");
        let id = Uuid::nil();
        assert!(b
            .build_resume_command(&path, &id, "p", "github.com")
            .is_some());
    }

    #[test]
    fn test_build_interactive_resume_command() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_interactive_resume_command(&path, &session_id, "github.com")
            .expect("interactive resume should be supported");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "claude");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--resume".as_ref()));
        assert!(args.contains(&session_id.to_string().as_ref()));
        // Interactive mode should NOT have --print or --output-format
        assert!(!args.contains(&"--print".as_ref()));
        assert!(!args.contains(&"--output-format".as_ref()));
        assert!(!args.contains(&"stream-json".as_ref()));
    }

    // ---- parse_event tests ----

    #[test]
    fn test_parse_event_message_start() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Started { usage: None }));
    }

    #[test]
    fn test_parse_event_tool_use_buffered_until_stop() {
        let b = backend();

        // ContentBlockStart(ToolUse) should NOT emit — it buffers
        let start_line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Bash","id":"toolu_1"}}}"#;
        assert!(b.parse_events(start_line).is_empty());

        // InputJsonDelta should accumulate
        let delta_line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"git status\"}"}}}"#;
        assert!(b.parse_events(delta_line).is_empty());

        // ContentBlockStop should emit ToolUse with summary
        let stop_line =
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#;
        let event = single(b.parse_events(stop_line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            } => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(tool_use_id, "toolu_1");
                assert_eq!(input_summary, Some("Run: git status".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_use_multi_delta() {
        let b = backend();

        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Read","id":"toolu_2"}}}"#;
        assert!(b.parse_events(start).is_empty());

        // Two partial JSON deltas
        let delta1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\""}}}"#;
        assert!(b.parse_events(delta1).is_empty());

        let delta2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"src/main.rs\"}"}}}"#;
        assert!(b.parse_events(delta2).is_empty());

        let stop = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#;
        let event = single(b.parse_events(stop));
        match event {
            AgentEvent::ToolUse { input_summary, .. } => {
                assert_eq!(input_summary, Some("Read: src/main.rs".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_text_delta() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello world"}}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "Hello world".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_text_block_stop_returns_none() {
        let b = backend();
        // ContentBlockStop with no buffered tool = text block end = None
        let line = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_message_delta() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(usage.unwrap().output_tokens, 42);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_message_delta_no_usage() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            }
        );
    }

    #[test]
    fn test_parse_event_error() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"error","error":{"type":"api_error","message":"rate limited"}}}"#;
        let event = single(b.parse_events(line));
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
        let event = single(b.parse_events(line));
        assert_eq!(event, AgentEvent::Ping);
    }

    #[test]
    fn test_parse_event_tool_result_from_verbose() {
        let b = backend();
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"file contents","is_error":false}]}}"#;
        let event = single(b.parse_events(line));
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
        let event = single(b.parse_events(line));
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
    fn test_parse_events_multiple_tool_results() {
        let b = backend();
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"output A","is_error":false},{"type":"tool_result","tool_use_id":"toolu_2","content":"output B","is_error":true}]}}"#;
        let events = b.parse_events(line);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "output A".to_string(),
                is_error: false,
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                tool_use_id: "toolu_2".to_string(),
                content: "output B".to_string(),
                is_error: true,
            }
        );
    }

    #[test]
    fn test_parse_event_raw_line_returns_none() {
        let b = backend();
        assert!(b.parse_events("some random output").is_empty());
    }

    #[test]
    fn test_parse_event_empty_line_returns_none() {
        let b = backend();
        assert!(b.parse_events("").is_empty());
        assert!(b.parse_events("   ").is_empty());
    }

    #[test]
    fn test_parse_event_message_stop_produces_completion() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::MessageComplete {
                stop_reason: None,
                usage: None,
            }
        );
    }

    #[test]
    fn test_parse_event_text_content_block_start_returns_none() {
        let b = backend();
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_tool_names_preserved() {
        let b = backend();
        for tool in &["Bash", "Read", "Write", "Edit", "Glob", "Grep", "Agent"] {
            // Start tool block
            let start = format!(
                r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","name":"{}","id":"t1"}}}}}}"#,
                tool
            );
            assert!(b.parse_events(&start).is_empty()); // buffered

            // Stop block — emits ToolUse
            let stop = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#;
            let event = single(b.parse_events(stop));
            match event {
                AgentEvent::ToolUse { tool_name, .. } => {
                    assert_eq!(tool_name, *tool);
                }
                other => panic!("Expected ToolUse for {}, got {:?}", tool, other),
            }
        }
    }

    // ---- format_tool_summary tests ----

    #[test]
    fn test_format_tool_summary_bash() {
        let result = format_tool_summary("Bash", r#"{"command":"git status"}"#);
        assert_eq!(result, "Run: git status");
    }

    #[test]
    fn test_format_tool_summary_bash_long() {
        let result = format_tool_summary(
            "Bash",
            r#"{"command":"git commit -m 'This is a very long commit message that should be truncated'"}"#,
        );
        assert!(result.starts_with("Run: git commit"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_format_tool_summary_read() {
        let result = format_tool_summary("Read", r#"{"file_path":"src/main.rs"}"#);
        assert_eq!(result, "Read: src/main.rs");
    }

    #[test]
    fn test_format_tool_summary_edit_long_path() {
        let result = format_tool_summary(
            "Edit",
            r#"{"file_path":"/Users/test/projects/gru/src/commands/fix.rs"}"#,
        );
        assert_eq!(result, "Edit: .../src/commands/fix.rs");
    }

    #[test]
    fn test_format_tool_summary_grep() {
        let result = format_tool_summary("Grep", r#"{"pattern":"TODO"}"#);
        assert_eq!(result, "Search: TODO");
    }

    #[test]
    fn test_format_tool_summary_unknown_tool() {
        let result = format_tool_summary("CustomTool", r#"{"foo":"bar"}"#);
        assert_eq!(result, "Tool: CustomTool");
    }

    #[test]
    fn test_format_tool_summary_invalid_json() {
        let result = format_tool_summary("Bash", "not valid json");
        assert_eq!(result, "Tool: Bash");
    }
}
