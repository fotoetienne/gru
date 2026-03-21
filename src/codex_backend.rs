//! OpenAI Codex CLI backend implementation for the `AgentBackend` trait.
//!
//! Implements the `AgentBackend` interface for the OpenAI Codex CLI, mapping
//! its JSONL streaming output (`codex exec --json`) to normalized `AgentEvent`s.
//!
//! Codex event types:
//! - `thread.started` → `AgentEvent::Started`
//! - `turn.started` → `AgentEvent::Thinking`
//! - `turn.completed` → `AgentEvent::MessageComplete` (with token usage)
//! - `turn.failed` → `AgentEvent::Error`
//! - `item.started` / `item.completed` → `AgentEvent::ToolUse` / `AgentEvent::ToolResult`
//! - `error` → `AgentEvent::Error`

use crate::agent::{AgentBackend, AgentEvent, TokenUsage};
use crate::display_utils::{shorten_path, truncate_string};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// OpenAI Codex CLI backend.
///
/// Implements `AgentBackend` by spawning `codex exec --json --full-auto`
/// and parsing the resulting JSONL event stream.
#[derive(Default)]
pub(crate) struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn name(&self) -> &str {
        "codex"
    }

    fn build_command(
        &self,
        worktree_path: &Path,
        _session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = build_codex_command(worktree_path, prompt);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn parse_events(&self, line: &str) -> Vec<AgentEvent> {
        parse_codex_event(line.trim()).into_iter().collect()
    }

    fn build_resume_command(
        &self,
        worktree_path: &Path,
        _session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> Option<TokioCommand> {
        // Codex supports resume via `codex exec resume --last "prompt"`
        // but it relies on its own session persistence, not Gru's session ID.
        let mut cmd = build_codex_resume_command(worktree_path, prompt);
        cmd.env("GH_HOST", github_host);
        Some(cmd)
    }

    fn build_interactive_resume_command(
        &self,
        _worktree_path: &Path,
        _session_id: &Uuid,
        _github_host: &str,
    ) -> Option<TokioCommand> {
        // Codex CLI does not support interactive resume mode
        None
    }

    fn build_oneshot_command(&self, worktree_path: &Path, prompt_arg: &str) -> TokioCommand {
        let mut cmd = TokioCommand::new("codex");
        cmd.arg("exec").arg("--full-auto");

        // When prompt_arg is "-", callers stream the actual prompt via stdin.
        if prompt_arg == "-" {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.arg(prompt_arg);
            cmd.stdin(std::process::Stdio::null());
        }

        cmd.current_dir(worktree_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// Builds a Codex command for a new session.
///
/// Uses `codex exec --json --full-auto` for autonomous headless execution
/// with JSONL streaming output.
fn build_codex_command(worktree_path: &Path, prompt: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new("codex");
    cmd.arg("exec")
        .arg("--json")
        .arg("--full-auto")
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path);
    cmd
}

/// Builds a Codex command to resume the most recent session.
fn build_codex_resume_command(worktree_path: &Path, prompt: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new("codex");
    cmd.arg("exec")
        .arg("resume")
        .arg("--last")
        .arg("--json")
        .arg("--full-auto")
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path);
    cmd
}

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

/// Top-level Codex JSONL event envelope.
#[derive(Debug, Deserialize)]
struct CodexEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// Present on `item.started` and `item.completed` events.
    #[serde(default)]
    item: Option<CodexItem>,
    /// Present on `turn.completed` events.
    #[serde(default)]
    usage: Option<CodexUsage>,
    /// Present on `error` events.
    #[serde(default)]
    error: Option<CodexError>,
    // Note: thread_id and other unrecognized fields are silently ignored
    // by serde since we don't deny_unknown_fields.
}

/// A Codex item (command execution, message, file change, etc.)
#[derive(Debug, Deserialize)]
struct CodexItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    item_type: Option<String>,
    /// Command string for `command_execution` items.
    #[serde(default)]
    command: Option<String>,
    /// Status: "in_progress", "completed", "failed", etc.
    #[serde(default)]
    status: Option<String>,
    /// Output/content for completed items.
    #[serde(default)]
    output: Option<String>,
    /// For message items, the text content.
    #[serde(default)]
    content: Option<serde_json::Value>,
    /// For file change items, the file path.
    #[serde(default)]
    file_path: Option<String>,
}

/// Codex token usage from `turn.completed` events.
#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
}

/// Error detail from Codex `error` events.
#[derive(Debug, Deserialize)]
struct CodexError {
    #[serde(default)]
    message: Option<String>,
    // Note: error_type and other unrecognized fields are silently ignored by serde.
}

/// Parse a single line of Codex JSONL output into an `AgentEvent`.
fn parse_codex_event(line: &str) -> Option<AgentEvent> {
    if line.is_empty() {
        return None;
    }

    let event: CodexEvent = serde_json::from_str(line).ok()?;

    match event.event_type.as_str() {
        "thread.started" => Some(AgentEvent::Started { usage: None }),

        "turn.started" => Some(AgentEvent::Thinking { text: None }),

        "turn.completed" => {
            let usage = event.usage.map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_input_tokens: u.cached_input_tokens,
                ..Default::default()
            });
            Some(AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage,
            })
        }

        "turn.failed" => {
            // Codex may nest error info in the top-level `error` field (same
            // shape as `"type":"error"` events) or in a turn-specific field.
            // We try `error.message` first; if absent, fall back to a generic message.
            // TODO: verify against real Codex output once available.
            let message = event
                .error
                .and_then(|e| e.message)
                .unwrap_or_else(|| "Turn failed".to_string());
            Some(AgentEvent::Error { message })
        }

        "item.started" => {
            let item = event.item?;
            let item_type = item.item_type.as_deref().unwrap_or("unknown");
            let item_id = item.id.unwrap_or_else(|| Uuid::new_v4().to_string());

            match item_type {
                "command_execution" => {
                    let summary = item.command.as_deref().map(format_codex_command_summary);
                    Some(AgentEvent::ToolUse {
                        tool_name: "command".to_string(),
                        tool_use_id: item_id,
                        input_summary: summary,
                    })
                }
                "file_change" => {
                    let summary = item
                        .file_path
                        .as_deref()
                        .map(|p| format!("Edit: {}", shorten_path(p)));
                    Some(AgentEvent::ToolUse {
                        tool_name: "file_change".to_string(),
                        tool_use_id: item_id,
                        input_summary: summary,
                    })
                }
                "message" => {
                    let text = extract_message_text(&item.content);
                    text.map(|t| AgentEvent::TextDelta { text: t })
                }
                _ => Some(AgentEvent::ToolUse {
                    tool_name: item_type.to_string(),
                    tool_use_id: item_id,
                    input_summary: None,
                }),
            }
        }

        "item.completed" => {
            let item = event.item?;
            let item_type = item.item_type.as_deref().unwrap_or("unknown");
            let item_id = item.id.unwrap_or_else(|| Uuid::new_v4().to_string());

            match item_type {
                "command_execution" => {
                    let output = item.output.unwrap_or_default();
                    let is_error = item.status.as_deref() == Some("failed");
                    Some(AgentEvent::ToolResult {
                        tool_use_id: item_id,
                        content: output,
                        is_error,
                    })
                }
                "file_change" => {
                    let content = item.file_path.unwrap_or_else(|| "file changed".to_string());
                    let is_error = item.status.as_deref() == Some("failed");
                    Some(AgentEvent::ToolResult {
                        tool_use_id: item_id,
                        content,
                        is_error,
                    })
                }
                "message" => {
                    let text = extract_message_text(&item.content);
                    text.map(|t| AgentEvent::TextDelta { text: t })
                }
                _ => Some(AgentEvent::ToolResult {
                    tool_use_id: item_id,
                    content: String::new(),
                    is_error: false,
                }),
            }
        }

        "error" => {
            let message = event
                .error
                .and_then(|e| e.message)
                .unwrap_or_else(|| "Unknown Codex error".to_string());
            Some(AgentEvent::Error { message })
        }

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a command execution summary for display.
fn format_codex_command_summary(command: &str) -> String {
    // Strip the `bash -lc ` prefix that Codex adds
    let cmd = command
        .strip_prefix("bash -lc ")
        .or_else(|| command.strip_prefix("bash -c "))
        .unwrap_or(command);

    let truncated = truncate_string(cmd, 60);
    format!("Run: {}", truncated)
}

/// Extract text content from a Codex message item's content field.
fn extract_message_text(content: &Option<serde_json::Value>) -> Option<String> {
    match content.as_ref()? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            // Content can be an array of content blocks
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|block| {
                    if block.get("type")?.as_str()? == "text" {
                        block.get("text")?.as_str().map(String::from)
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join(""))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> CodexBackend {
        CodexBackend
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
        assert_eq!(backend().name(), "codex");
    }

    #[test]
    fn test_build_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--json".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
        assert!(args.contains(&"fix the bug".as_ref()));
        assert_eq!(*args.last().unwrap(), std::ffi::OsStr::new("fix the bug"));

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
    fn test_build_resume_command_uses_resume() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_resume_command(&path, &session_id, "continue", "github.com")
            .expect("resume should be supported");
        let inner = cmd.as_std();

        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"resume".as_ref()));
        assert!(args.contains(&"--last".as_ref()));
        assert!(args.contains(&"--json".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));

        // Verify GH_HOST is set
        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())),
            "GH_HOST should be set on resume command"
        );
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
    fn test_interactive_resume_not_supported() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp");
        let id = Uuid::nil();
        assert!(b
            .build_interactive_resume_command(&path, &id, "github.com")
            .is_none());
    }

    #[test]
    fn test_build_oneshot_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "fix the tests");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
        assert!(args.contains(&"fix the tests".as_ref()));
    }

    #[test]
    fn test_build_oneshot_command_stdin_sentinel_omits_prompt_arg() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "-");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
        // "-" should NOT appear as an argument when using stdin sentinel
        assert!(!args.contains(&"-".as_ref()));
    }

    // ---- parse_event tests ----

    #[test]
    fn test_parse_event_thread_started() {
        let b = backend();
        let line = r#"{"type":"thread.started","thread_id":"thread_abc123"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Started { usage: None }));
    }

    #[test]
    fn test_parse_event_turn_started() {
        let b = backend();
        let line = r#"{"type":"turn.started"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Thinking { text: None }));
    }

    #[test]
    fn test_parse_event_turn_completed_with_usage() {
        let b = backend();
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 1000);
                assert_eq!(u.output_tokens, 500);
                assert_eq!(u.cache_read_input_tokens, Some(200));
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_turn_completed_no_usage() {
        let b = backend();
        let line = r#"{"type":"turn.completed"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(
            event,
            AgentEvent::MessageComplete {
                stop_reason: Some(_),
                usage: None,
            }
        ));
    }

    #[test]
    fn test_parse_event_turn_failed() {
        let b = backend();
        let line = r#"{"type":"turn.failed"}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "Turn failed".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_turn_failed_with_error() {
        let b = backend();
        let line = r#"{"type":"turn.failed","error":{"type":"api_error","message":"context length exceeded"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "context length exceeded".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_item_started_command() {
        let b = backend();
        let line = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc git status","status":"in_progress"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            } => {
                assert_eq!(tool_name, "command");
                assert_eq!(tool_use_id, "item_1");
                assert_eq!(input_summary, Some("Run: git status".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_item_started_file_change() {
        let b = backend();
        let line = r#"{"type":"item.started","item":{"id":"item_2","type":"file_change","file_path":"src/main.rs","status":"in_progress"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                input_summary,
                ..
            } => {
                assert_eq!(tool_name, "file_change");
                assert_eq!(input_summary, Some("Edit: src/main.rs".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_item_started_message() {
        let b = backend();
        let line = r#"{"type":"item.started","item":{"id":"item_3","type":"message","content":"I'll fix the bug now."}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "I'll fix the bug now.".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_item_completed_command() {
        let b = backend();
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc git status","status":"completed","output":"On branch main\nnothing to commit"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "item_1");
                assert_eq!(content, "On branch main\nnothing to commit");
                assert!(!is_error);
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_item_completed_command_failed() {
        let b = backend();
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","status":"failed","output":"command not found"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolResult {
                is_error, content, ..
            } => {
                assert!(is_error);
                assert_eq!(content, "command not found");
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_item_completed_file_change() {
        let b = backend();
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"file_change","file_path":"src/lib.rs","status":"completed"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "item_2");
                assert_eq!(content, "src/lib.rs");
                assert!(!is_error);
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_item_completed_file_change_failed() {
        let b = backend();
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","file_path":"src/lib.rs","status":"failed"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolResult {
                is_error, content, ..
            } => {
                assert!(is_error);
                assert_eq!(content, "src/lib.rs");
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_error() {
        let b = backend();
        let line = r#"{"type":"error","error":{"type":"api_error","message":"rate limited"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "rate limited".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_error_no_message() {
        let b = backend();
        let line = r#"{"type":"error","error":{"type":"unknown"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "Unknown Codex error".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_empty_line() {
        let b = backend();
        assert!(b.parse_events("").is_empty());
        assert!(b.parse_events("   ").is_empty());
    }

    #[test]
    fn test_parse_event_raw_text() {
        let b = backend();
        assert!(b.parse_events("some random output").is_empty());
    }

    #[test]
    fn test_parse_event_unknown_type() {
        let b = backend();
        let line = r#"{"type":"some.unknown.event"}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_message_with_array_content() {
        let b = backend();
        let line = r#"{"type":"item.started","item":{"id":"item_4","type":"message","content":[{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "Hello world".to_string(),
            }
        );
    }

    // ---- helper tests ----

    #[test]
    fn test_format_codex_command_summary_strips_bash_prefix() {
        assert_eq!(
            format_codex_command_summary("bash -lc git status"),
            "Run: git status"
        );
        assert_eq!(
            format_codex_command_summary("bash -c ls -la"),
            "Run: ls -la"
        );
    }

    #[test]
    fn test_format_codex_command_summary_no_prefix() {
        assert_eq!(
            format_codex_command_summary("git status"),
            "Run: git status"
        );
    }

    #[test]
    fn test_format_codex_command_summary_long_command() {
        let long_cmd = "bash -lc ".to_string() + &"a".repeat(100);
        let result = format_codex_command_summary(&long_cmd);
        assert!(result.ends_with("..."));
        assert!(result.starts_with("Run: "));
    }

    #[test]
    fn test_extract_message_text_string() {
        let content = Some(serde_json::Value::String("hello".to_string()));
        assert_eq!(extract_message_text(&content), Some("hello".to_string()));
    }

    #[test]
    fn test_extract_message_text_none() {
        assert_eq!(extract_message_text(&None), None);
    }
}
