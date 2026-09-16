//! Netflix Pi CLI backend implementation for the `AgentBackend` trait.
//!
//! Implements the `AgentBackend` interface for Pi (`pi`, npm
//! `@netflix-internal/pi-agent`), mapping its JSONL streaming output
//! (`pi -p --mode json`) to normalized `AgentEvent`s.
//!
//! Pi event types:
//! - `session` / `agent_start` → `AgentEvent::Started`
//! - `turn_start` → `AgentEvent::Thinking`
//! - `message_update` (`assistantMessageEvent.type == "text_delta"`) → `AgentEvent::TextDelta`
//! - `tool_execution_start` → `AgentEvent::ToolUse`
//! - `tool_execution_end` → `AgentEvent::ToolResult`
//! - `turn_end` → `AgentEvent::MessageComplete`
//! - `agent_end` → `AgentEvent::Finished`
//! - `turn_failed` / `error` → `AgentEvent::Error`
//!
//! Unlike Codex, Pi supports interactive session resume (needed by `gru attach`)
//! and has no `--dangerously-skip-permissions` equivalent — autonomous tool use
//! is the default under `-p`.

use crate::agent::{AgentBackend, AgentEvent, TokenUsage};
use crate::display_utils::{shorten_path, truncate_string};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Netflix Pi CLI backend.
///
/// Implements `AgentBackend` by spawning `pi -p --mode json` and parsing the
/// resulting JSONL event stream.
#[derive(Default)]
pub(crate) struct PiBackend;

impl AgentBackend for PiBackend {
    fn name(&self) -> &str {
        "pi"
    }

    fn process_names(&self) -> &[&str] {
        &["pi"]
    }

    fn build_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = build_pi_command(worktree_path, session_id, prompt);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn parse_events(&self, line: &str) -> Vec<AgentEvent> {
        parse_pi_event(line.trim())
    }

    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> Option<TokioCommand> {
        // Pi resumes a session by passing the same --session-id with a new prompt.
        let mut cmd = build_pi_command(worktree_path, session_id, prompt);
        cmd.env("GH_HOST", github_host);
        Some(cmd)
    }

    fn build_interactive_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        github_host: &str,
    ) -> Option<TokioCommand> {
        // Pi supports interactive resume: drop -p, keep --session-id.
        let mut cmd = build_pi_interactive_command(worktree_path, session_id);
        cmd.env("GH_HOST", github_host);
        Some(cmd)
    }

    fn build_oneshot_command(&self, worktree_path: &Path, prompt_arg: &str) -> TokioCommand {
        build_pi_oneshot_command(worktree_path, prompt_arg)
    }

    fn build_ci_fix_command(
        &self,
        worktree_path: &Path,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        self.build_command(worktree_path, &Uuid::nil(), prompt, github_host)
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// Builds a Pi command for a new or resumed session.
///
/// Uses `pi -p --mode json --session-id <uuid> <prompt>` for autonomous
/// headless execution with JSONL streaming output. There is no
/// `--dangerously-skip-permissions` equivalent for Pi; `bash` and `edit`
/// tools run without approval prompts by default under `-p`.
fn build_pi_command(worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new("pi");
    cmd.arg("-p")
        .arg("--mode")
        .arg("json")
        .arg("--session-id")
        .arg(session_id.to_string())
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path);
    cmd
}

/// Builds a Pi command for interactive session resume (used by `gru attach`).
///
/// Drops `-p` and `--mode json` in favor of Pi's interactive TUI, which shows
/// prior history for the given session ID.
fn build_pi_interactive_command(worktree_path: &Path, session_id: &Uuid) -> TokioCommand {
    let mut cmd = TokioCommand::new("pi");
    cmd.arg("--session-id")
        .arg(session_id.to_string())
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    cmd
}

/// Builds a Pi command for a one-shot utility task (no session tracking).
///
/// When `prompt_arg` is `"-"`, the prompt argument is omitted and stdin is
/// piped instead — `pi -p -` emits nothing, so the sentinel must not be
/// passed as a literal argument.
fn build_pi_oneshot_command(worktree_path: &Path, prompt_arg: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new("pi");
    cmd.arg("-p");

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

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

/// Top-level Pi JSONL event envelope.
#[derive(Debug, Deserialize)]
struct PiEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// Present on `message_update` events.
    #[serde(default, rename = "assistantMessageEvent")]
    assistant_message_event: Option<PiAssistantMessageEvent>,
    /// Present on `tool_execution_start` and `tool_execution_end` events.
    #[serde(default)]
    id: Option<String>,
    /// Present on `tool_execution_start` events.
    #[serde(default)]
    tool: Option<String>,
    /// Present on `tool_execution_start` events; already-complete tool arguments.
    #[serde(default)]
    args: Option<serde_json::Value>,
    /// Present on `tool_execution_end` events.
    #[serde(default)]
    result: Option<PiToolResult>,
    /// Present on `turn_end` events.
    #[serde(default)]
    usage: Option<PiUsage>,
    /// Present on `error` / `turn_failed` events.
    #[serde(default)]
    message: Option<String>,
}

/// Nested assistant message event carried by `message_update`.
#[derive(Debug, Deserialize)]
struct PiAssistantMessageEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<String>,
}

/// Result payload carried by `tool_execution_end`.
#[derive(Debug, Deserialize)]
struct PiToolResult {
    #[serde(default)]
    content: Vec<PiContentBlock>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

/// A single content block within a tool result.
#[derive(Debug, Deserialize)]
struct PiContentBlock {
    #[serde(default)]
    text: Option<String>,
}

/// Pi token usage from `turn_end` events.
#[derive(Debug, Deserialize)]
struct PiUsage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: Option<u64>,
    #[serde(default, rename = "cacheWrite")]
    cache_write: Option<u64>,
}

/// Parse a single line of Pi JSONL output into normalized events.
///
/// Silently ignores lines that aren't recognized JSON events, including the
/// non-JSON preamble the newt shim prints to stdout before the event stream
/// (e.g. "Using existing agent-beach…").
fn parse_pi_event(line: &str) -> Vec<AgentEvent> {
    if line.is_empty() {
        return Vec::new();
    }

    let event: PiEvent = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    match event.event_type.as_str() {
        "session" | "agent_start" => vec![AgentEvent::Started { usage: None }],

        "turn_start" => vec![AgentEvent::Thinking { text: None }],

        "message_update" => {
            let Some(ame) = event.assistant_message_event else {
                return Vec::new();
            };
            if ame.event_type != "text_delta" {
                return Vec::new();
            }
            match ame.delta {
                Some(text) => vec![AgentEvent::TextDelta { text }],
                None => Vec::new(),
            }
        }

        "tool_execution_start" => {
            let tool_name = event.tool.unwrap_or_else(|| "unknown".to_string());
            let tool_use_id = event.id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let input_summary = Some(format_pi_tool_summary(&tool_name, event.args.as_ref()));
            vec![AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            }]
        }

        "tool_execution_end" => {
            let tool_use_id = event.id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let (content, is_error) = match event.result {
                Some(result) => {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| block.text.as_deref())
                        .collect::<Vec<_>>()
                        .join("");
                    (text, result.is_error)
                }
                None => (String::new(), false),
            };
            vec![AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            }]
        }

        "turn_end" => {
            let usage = event.usage.map(|u| TokenUsage {
                input_tokens: u.input,
                output_tokens: u.output,
                cache_read_input_tokens: u.cache_read,
                cache_creation_input_tokens: u.cache_write,
            });
            vec![AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage,
            }]
        }

        "agent_end" => vec![AgentEvent::Finished { usage: None }],

        "turn_failed" | "error" => {
            let message = event
                .message
                .unwrap_or_else(|| "Pi agent error".to_string());
            vec![AgentEvent::Error { message }]
        }

        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a tool-call summary for display, mirroring
/// `format_tool_summary` in `claude_backend.rs` but using Pi's lowercase
/// tool names (`read`, `bash`, `edit`, `write`, …).
fn format_pi_tool_summary(tool_name: &str, args: Option<&serde_json::Value>) -> String {
    let args = match args {
        Some(v) => v,
        None => return format!("Tool: {}", tool_name),
    };

    match tool_name {
        "bash" => {
            if let Some(command) = args.get("command").and_then(|c| c.as_str()) {
                format!("Run: {}", truncate_string(command, 60))
            } else {
                "Run: bash command".to_string()
            }
        }
        "read" => {
            if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
                format!("Read: {}", shorten_path(path))
            } else {
                "Read: file".to_string()
            }
        }
        "write" => {
            if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
                format!("Write: {}", shorten_path(path))
            } else {
                "Write: file".to_string()
            }
        }
        "edit" => {
            if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
                format!("Edit: {}", shorten_path(path))
            } else {
                "Edit: file".to_string()
            }
        }
        _ => format!("Tool: {}", tool_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> PiBackend {
        PiBackend
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
        assert_eq!(backend().name(), "pi");
    }

    #[test]
    fn test_build_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        assert!(args.contains(&"--mode".as_ref()));
        assert!(args.contains(&"json".as_ref()));
        assert!(args.contains(&"--session-id".as_ref()));
        assert!(args.contains(&session_id.to_string().as_ref()));
        assert!(args.contains(&"fix the bug".as_ref()));
        assert_eq!(*args.last().unwrap(), std::ffi::OsStr::new("fix the bug"));

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
    fn test_build_resume_command_uses_same_session_id() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_resume_command(&path, &session_id, "continue", "github.com")
            .expect("resume should be supported");
        let inner = cmd.as_std();

        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--session-id".as_ref()));
        assert!(args.contains(&session_id.to_string().as_ref()));
        assert!(args.contains(&"continue".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())));
    }

    #[test]
    fn test_build_interactive_resume_command_supported() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b
            .build_interactive_resume_command(&path, &session_id, "github.com")
            .expect("interactive resume should be supported");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--session-id".as_ref()));
        assert!(args.contains(&session_id.to_string().as_ref()));
        // Interactive mode should NOT have -p or --mode json
        assert!(!args.contains(&"-p".as_ref()));
        assert!(!args.contains(&"--mode".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())));
    }

    #[test]
    fn test_build_oneshot_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "fix the tests");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        assert!(args.contains(&"fix the tests".as_ref()));
    }

    #[test]
    fn test_build_oneshot_command_stdin_sentinel_omits_prompt_arg() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "-");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        // "-" should NOT appear as an argument when using stdin sentinel
        assert!(!args.contains(&"-".as_ref()));
    }

    #[test]
    fn test_build_ci_fix_command_produces_json_stream_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_ci_fix_command(&path, "fix the CI", "github.example.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        assert!(args.contains(&"--mode".as_ref()));
        assert!(args.contains(&"json".as_ref()));
        assert!(args.contains(&"fix the CI".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.example.com".as_ref())),
            "GH_HOST should be set on CI fix command"
        );
    }

    // ---- parse_event tests ----

    #[test]
    fn test_parse_event_session() {
        let b = backend();
        let line = r#"{"type":"session","sessionId":"abc123"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Started { usage: None }));
    }

    #[test]
    fn test_parse_event_agent_start() {
        let b = backend();
        let line = r#"{"type":"agent_start"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Started { usage: None }));
    }

    #[test]
    fn test_parse_event_turn_start() {
        let b = backend();
        let line = r#"{"type":"turn_start"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Thinking { text: None }));
    }

    #[test]
    fn test_parse_event_message_update_text_delta() {
        let b = backend();
        let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hello world"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::TextDelta {
                text: "Hello world".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_message_update_other_type_ignored() {
        let b = backend();
        let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"reasoning_delta","delta":"thinking..."}}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_tool_execution_start_bash() {
        let b = backend();
        let line = r#"{"type":"tool_execution_start","id":"tool_1","tool":"bash","args":{"command":"git status"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(tool_use_id, "tool_1");
                assert_eq!(input_summary, Some("Run: git status".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_start_read() {
        let b = backend();
        let line = r#"{"type":"tool_execution_start","id":"tool_2","tool":"read","args":{"path":"src/main.rs"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                input_summary,
                ..
            } => {
                assert_eq!(tool_name, "read");
                assert_eq!(input_summary, Some("Read: src/main.rs".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_start_write() {
        let b = backend();
        let line = r#"{"type":"tool_execution_start","id":"tool_3","tool":"write","args":{"path":"out.txt"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse { input_summary, .. } => {
                assert_eq!(input_summary, Some("Write: out.txt".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_start_edit() {
        let b = backend();
        let line = r#"{"type":"tool_execution_start","id":"tool_4","tool":"edit","args":{"path":"src/lib.rs"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse { input_summary, .. } => {
                assert_eq!(input_summary, Some("Edit: src/lib.rs".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_start_unknown_tool() {
        let b = backend();
        let line = r#"{"type":"tool_execution_start","id":"tool_5","tool":"grep","args":{"pattern":"foo"}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolUse {
                tool_name,
                input_summary,
                ..
            } => {
                assert_eq!(tool_name, "grep");
                assert_eq!(input_summary, Some("Tool: grep".to_string()));
            }
            other => panic!("Expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_end_success() {
        let b = backend();
        let line = r#"{"type":"tool_execution_end","id":"tool_1","result":{"content":[{"type":"text","text":"On branch main"}],"isError":false}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tool_1");
                assert_eq!(content, "On branch main");
                assert!(!is_error);
            }
            other => panic!("Expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_tool_execution_end_error() {
        let b = backend();
        let line = r#"{"type":"tool_execution_end","id":"tool_1","result":{"content":[{"type":"text","text":"command not found"}],"isError":true}}"#;
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
    fn test_parse_event_turn_end_with_usage() {
        let b = backend();
        let line = r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50},"cost":{"total":0.01}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 1000);
                assert_eq!(u.output_tokens, 500);
                assert_eq!(u.cache_read_input_tokens, Some(200));
                assert_eq!(u.cache_creation_input_tokens, Some(50));
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_turn_end_no_usage() {
        let b = backend();
        let line = r#"{"type":"turn_end"}"#;
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
    fn test_parse_event_agent_end() {
        let b = backend();
        let line = r#"{"type":"agent_end"}"#;
        let event = single(b.parse_events(line));
        assert!(matches!(event, AgentEvent::Finished { usage: None }));
    }

    #[test]
    fn test_parse_event_turn_failed() {
        let b = backend();
        let line = r#"{"type":"turn_failed","message":"context length exceeded"}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "context length exceeded".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_turn_failed_no_message() {
        let b = backend();
        let line = r#"{"type":"turn_failed"}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "Pi agent error".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_event_error() {
        let b = backend();
        let line = r#"{"type":"error","message":"rate limited"}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::Error {
                message: "rate limited".to_string(),
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
    fn test_parse_event_ignored_types() {
        let b = backend();
        for event_type in [
            "message_start",
            "message_end",
            "entry_appended",
            "agent_settled",
            "tool_execution_update",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
        ] {
            let line = format!(r#"{{"type":"{}"}}"#, event_type);
            assert!(
                b.parse_events(&line).is_empty(),
                "expected {} to be ignored",
                event_type
            );
        }
    }

    #[test]
    fn test_parse_event_newt_shim_preamble_lines() {
        let b = backend();
        assert!(b.parse_events("Using existing agent-beach…").is_empty());
        assert!(b
            .parse_events("Using existing Netflix Pi distribution package…")
            .is_empty());
    }

    #[test]
    fn test_parse_event_unknown_type() {
        let b = backend();
        let line = r#"{"type":"some.unknown.event"}"#;
        assert!(b.parse_events(line).is_empty());
    }

    // ---- helper tests ----

    #[test]
    fn test_format_pi_tool_summary_bash_truncates() {
        let args = serde_json::json!({"command": "a".repeat(100)});
        let result = format_pi_tool_summary("bash", Some(&args));
        assert!(result.starts_with("Run: "));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_format_pi_tool_summary_no_args() {
        assert_eq!(format_pi_tool_summary("bash", None), "Tool: bash");
    }
}
