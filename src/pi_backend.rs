//! Pi CLI backend implementation for the `AgentBackend` trait.
//!
//! Implements the `AgentBackend` interface for [Pi](https://github.com/earendil-works/pi-mono)
//! (`pi`, npm `@earendil-works/pi-coding-agent`), mapping its JSONL streaming
//! output (`pi -p --mode json`) to normalized `AgentEvent`s.
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
use std::sync::Mutex;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Pi CLI backend.
///
/// Implements `AgentBackend` by spawning `pi -p --mode json` and parsing the
/// resulting JSONL event stream.
///
/// Pi reports input and cache token counts per-turn on `turn_end` rather
/// than once at session start, so this backend accumulates them across the
/// session and reports the totals in the `Finished` event at `agent_end`
/// (output tokens are already accumulated by the caller from each turn's
/// `MessageComplete`, so `Finished` reports `output_tokens: 0` to avoid
/// double-counting).
pub(crate) struct PiBackend {
    /// Path or name of the Pi CLI binary to invoke (`agent.pi.binary` in config).
    binary: String,
    /// Model to pass via `--model` (`agent.pi.model` in config). `None` lets
    /// Pi use its own configured default.
    model: Option<String>,
    /// Thinking effort to pass via `--thinking` (`agent.pi.thinking` in config).
    thinking: Option<String>,
    accumulated_usage: Mutex<TokenUsage>,
}

impl Default for PiBackend {
    fn default() -> Self {
        Self {
            binary: "pi".to_string(),
            model: None,
            thinking: None,
            accumulated_usage: Mutex::new(TokenUsage::default()),
        }
    }
}

impl PiBackend {
    pub(crate) fn new(
        binary: Option<String>,
        model: Option<String>,
        thinking: Option<String>,
    ) -> Self {
        Self {
            binary: binary.unwrap_or_else(|| "pi".to_string()),
            model,
            thinking,
            accumulated_usage: Mutex::new(TokenUsage::default()),
        }
    }

    /// Appends `--model`/`--thinking` flags (in that order) to `cmd` when
    /// configured. Must be called *before* the prompt argument is added —
    /// Pi's `-p <prompt>` positional could otherwise swallow trailing flags
    /// as part of the prompt text.
    fn apply_model_flags(&self, cmd: &mut TokioCommand) {
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(thinking) = &self.thinking {
            cmd.arg("--thinking").arg(thinking);
        }
    }
}

impl AgentBackend for PiBackend {
    fn name(&self) -> &str {
        "pi"
    }

    fn process_names(&self) -> &[&str] {
        &["pi"]
    }

    /// Uses `pi -p --mode json --session-id <uuid> [--model ...] [--thinking ...]
    /// <prompt>` for autonomous headless execution with JSONL streaming output.
    /// There is no `--dangerously-skip-permissions` equivalent for Pi; `bash`
    /// and `edit` tools run without approval prompts by default under `-p`.
    fn build_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.binary);
        cmd.arg("-p")
            .arg("--mode")
            .arg("json")
            .arg("--session-id")
            .arg(session_id.to_string());
        self.apply_model_flags(&mut cmd);
        apply_pi_stdio(cmd.arg(prompt), worktree_path);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn parse_events(&self, line: &str) -> Vec<AgentEvent> {
        parse_pi_event(line.trim(), &self.accumulated_usage)
    }

    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> Option<TokioCommand> {
        // Pi resumes a session by passing the same --session-id with a new prompt.
        Some(self.build_command(worktree_path, session_id, prompt, github_host))
    }

    fn build_interactive_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        github_host: &str,
    ) -> Option<TokioCommand> {
        // Pi supports interactive resume: drop -p, keep --session-id.
        let mut cmd = TokioCommand::new(&self.binary);
        cmd.arg("--session-id").arg(session_id.to_string());
        self.apply_model_flags(&mut cmd);
        cmd.current_dir(worktree_path)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
            .env_remove(crate::labels::GRU_CONFIG_PATH_ENV);
        cmd.env("GH_HOST", github_host);
        Some(cmd)
    }

    /// When `prompt_arg` is `"-"`, the prompt argument is omitted and stdin is
    /// piped instead — `pi -p -` emits nothing, so the sentinel must not be
    /// passed as a literal argument.
    fn build_oneshot_command(
        &self,
        worktree_path: &Path,
        prompt_arg: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.binary);
        cmd.arg("-p");
        self.apply_model_flags(&mut cmd);

        if prompt_arg == "-" {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.arg(prompt_arg);
            cmd.stdin(std::process::Stdio::null());
        }

        cmd.current_dir(worktree_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
            .env_remove(crate::labels::GRU_CONFIG_PATH_ENV);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    /// Omits `--session-id` entirely (unlike `build_command`) so repeated CI
    /// fixes in the same worktree never share Pi conversation history —
    /// CI-fix invocations must be stateless one-shots per the `AgentBackend`
    /// contract.
    fn build_ci_fix_command(
        &self,
        worktree_path: &Path,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.binary);
        cmd.arg("-p").arg("--mode").arg("json");
        self.apply_model_flags(&mut cmd);
        apply_pi_stdio(cmd.arg(prompt), worktree_path);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn final_usage(&self) -> Option<TokenUsage> {
        Some(self.accumulated_usage.lock().unwrap().clone())
    }
}

/// Applies the stdio/cwd/env settings shared by `-p --mode json` invocations
/// that pass a prompt as a positional argument (`build_command` and
/// `build_ci_fix_command`).
fn apply_pi_stdio(cmd: &mut TokioCommand, worktree_path: &Path) {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path)
        // Prevent GRU_RETRY_PARENT and GRU_CONFIG_PATH from leaking into Pi
        // and its tool subprocesses, which would let a `gru` command Pi
        // invokes incorrectly defer failure labeling or load the lab's
        // worker config instead of behaving as a standalone invocation.
        .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
        .env_remove(crate::labels::GRU_CONFIG_PATH_ENV);
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
    #[serde(default, rename = "toolCallId")]
    tool_call_id: Option<String>,
    /// Present on `tool_execution_start` events.
    #[serde(default, rename = "toolName")]
    tool_name: Option<String>,
    /// Present on `tool_execution_start` events; already-complete tool arguments.
    #[serde(default)]
    args: Option<serde_json::Value>,
    /// Present on `tool_execution_end` events.
    #[serde(default)]
    result: Option<PiToolResult>,
    /// Present on `tool_execution_end` events; whether the tool call failed.
    /// Lives on the event envelope, not nested inside `result`.
    #[serde(default, rename = "isError")]
    is_error: bool,
    /// Present on `turn_end` events. Deserialized as a raw `Value` rather
    /// than directly into `PiUsage` so a malformed/schema-drifting usage
    /// object can't fail parsing of the whole event and drop the `turn_end`
    /// signal — see `parse_usage`.
    #[serde(default)]
    usage: Option<serde_json::Value>,
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
///
/// Note: `isError` lives on the `tool_execution_end` event envelope
/// (`PiEvent::is_error`), not on this nested result.
#[derive(Debug, Deserialize)]
struct PiToolResult {
    #[serde(default)]
    content: Vec<PiContentBlock>,
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

/// Parses a raw `usage` JSON value into `PiUsage`, tolerating malformed or
/// schema-drifting shapes by returning `None` instead of propagating an
/// error. This keeps a bad `usage` object from failing deserialization of
/// the whole `turn_end` line — which would otherwise drop the
/// `MessageComplete` signal the monitor/PR state logic uses as its
/// turn-completion latch, leaving a successfully finished Pi turn looking
/// incomplete.
fn parse_usage(usage: Option<serde_json::Value>) -> Option<PiUsage> {
    serde_json::from_value(usage?).ok()
}

/// Parse a single line of Pi JSONL output into normalized events.
///
/// Silently ignores lines that aren't recognized JSON events. This matters
/// when Pi is invoked through a launcher or wrapper that prints its own
/// non-JSON preamble to stdout ahead of the event stream.
fn parse_pi_event(line: &str, accumulated_usage: &Mutex<TokenUsage>) -> Vec<AgentEvent> {
    if line.is_empty() {
        return Vec::new();
    }

    let event: PiEvent = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    match event.event_type.as_str() {
        "session" | "agent_start" => {
            // Reset accumulated usage at the start of each stream. The
            // backend instance is reused across independent invocations
            // (e.g. multiple CI-fix attempts driven by the same backend
            // reference), so stale totals from a prior session/attempt must
            // not leak into this one's `Finished` usage.
            *accumulated_usage.lock().unwrap() = TokenUsage::default();
            vec![AgentEvent::Started { usage: None }]
        }

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
            let tool_name = event.tool_name.unwrap_or_else(|| "unknown".to_string());
            let tool_use_id = event
                .tool_call_id
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let input_summary = Some(format_pi_tool_summary(&tool_name, event.args.as_ref()));
            vec![AgentEvent::ToolUse {
                tool_name,
                tool_use_id,
                input_summary,
            }]
        }

        "tool_execution_end" => {
            let tool_use_id = event
                .tool_call_id
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let content = event
                .result
                .map(|result| {
                    result
                        .content
                        .iter()
                        .filter_map(|block| block.text.as_deref())
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            let is_error = event.is_error;
            vec![AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            }]
        }

        "turn_end" => {
            let usage = parse_usage(event.usage).map(|u| TokenUsage {
                input_tokens: u.input,
                output_tokens: u.output,
                cache_read_input_tokens: u.cache_read,
                cache_creation_input_tokens: u.cache_write,
            });
            if let Some(u) = &usage {
                let mut accumulated = accumulated_usage.lock().unwrap();
                accumulated.input_tokens += u.input_tokens;
                if let Some(cache_creation) = u.cache_creation_input_tokens {
                    *accumulated.cache_creation_input_tokens.get_or_insert(0) += cache_creation;
                }
                if let Some(cache_read) = u.cache_read_input_tokens {
                    *accumulated.cache_read_input_tokens.get_or_insert(0) += cache_read;
                }
            }
            vec![AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage,
            }]
        }

        "agent_end" => {
            let totals = accumulated_usage.lock().unwrap().clone();
            vec![AgentEvent::Finished {
                usage: Some(totals),
            }]
        }

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
    // A missing `args` payload falls through to each arm's own "not
    // present" branch (e.g. "Run: bash command") via `Value::get` returning
    // `None` on `Value::Null`, rather than short-circuiting every known
    // tool to the generic "Tool: {name}" fallback.
    let empty = serde_json::Value::Null;
    let args = args.unwrap_or(&empty);

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
        PiBackend::default()
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
    fn test_configured_binary_is_used() {
        let b = PiBackend::new(Some("/opt/tools/pi".to_string()), None, None);
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
        assert_eq!(cmd.as_std().get_program(), "/opt/tools/pi");
    }

    #[test]
    fn test_configured_model_and_thinking_are_appended() {
        let b = PiBackend::new(
            None,
            Some("anthropic/claude-sonnet-5".to_string()),
            Some("high".to_string()),
        );
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
        let inner = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--model".as_ref()));
        assert!(args.contains(&"anthropic/claude-sonnet-5".as_ref()));
        assert!(args.contains(&"--thinking".as_ref()));
        assert!(args.contains(&"high".as_ref()));

        // --model/--thinking must precede the positional prompt: if Pi's `-p`
        // takes a greedy/trailing positional, flags placed after the prompt
        // would be silently absorbed into the prompt text instead of parsed.
        assert_eq!(*args.last().unwrap(), std::ffi::OsStr::new("fix the bug"));
        let model_pos = args
            .iter()
            .position(|a| *a == std::ffi::OsStr::new("--model"))
            .unwrap();
        let prompt_pos = args.len() - 1;
        assert!(
            model_pos < prompt_pos,
            "--model must come before the prompt argument"
        );
    }

    #[test]
    fn test_no_model_flags_when_unconfigured() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        let cmd = b.build_command(&path, &session_id, "fix the bug", "github.com");
        let inner = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(!args.contains(&"--model".as_ref()));
        assert!(!args.contains(&"--thinking".as_ref()));
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
        let cmd = b.build_oneshot_command(&path, "fix the tests", "github.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        assert!(args.contains(&"fix the tests".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())),
            "GH_HOST should be set on the oneshot command"
        );
    }

    #[test]
    fn test_build_oneshot_command_stdin_sentinel_omits_prompt_arg() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "-", "github.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"-p".as_ref()));
        // "-" should NOT appear as an argument when using stdin sentinel
        assert!(!args.contains(&"-".as_ref()));
    }

    #[test]
    fn test_build_oneshot_command_sets_ghe_host() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "fix the tests", "github.example.com");
        let inner = cmd.as_std();

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.example.com".as_ref())),
            "GH_HOST should be set to the GHE host"
        );
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
        // Must be stateless — no --session-id, so repeated CI fixes in the
        // same worktree never share Pi conversation history.
        assert!(!args.contains(&"--session-id".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.example.com".as_ref())),
            "GH_HOST should be set on CI fix command"
        );
    }

    #[test]
    fn test_all_command_builders_remove_gru_worker_env_vars() {
        // GRU_RETRY_PARENT and GRU_CONFIG_PATH must not leak from the
        // worker process into Pi (or its tool subprocesses), matching the
        // env_remove calls in claude_backend.rs and codex_backend.rs.
        // Command::env_remove surfaces as (key, None) in get_envs().
        let assert_removed = |cmd: &tokio::process::Command| {
            let envs: Vec<_> = cmd.as_std().get_envs().collect();
            assert!(
                envs.iter()
                    .any(|(k, v)| *k == crate::labels::GRU_RETRY_PARENT_ENV && v.is_none()),
                "GRU_RETRY_PARENT should be removed"
            );
            assert!(
                envs.iter()
                    .any(|(k, v)| *k == crate::labels::GRU_CONFIG_PATH_ENV && v.is_none()),
                "GRU_CONFIG_PATH should be removed"
            );
        };

        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();

        assert_removed(&b.build_command(&path, &session_id, "prompt", "github.com"));
        assert_removed(
            &b.build_resume_command(&path, &session_id, "prompt", "github.com")
                .unwrap(),
        );
        assert_removed(
            &b.build_interactive_resume_command(&path, &session_id, "github.com")
                .unwrap(),
        );
        assert_removed(&b.build_oneshot_command(&path, "prompt", "github.com"));
        assert_removed(&b.build_ci_fix_command(&path, "prompt", "github.com"));
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
        let line = r#"{"type":"tool_execution_start","toolCallId":"tool_1","toolName":"bash","args":{"command":"git status"}}"#;
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
        let line = r#"{"type":"tool_execution_start","toolCallId":"tool_2","toolName":"read","args":{"path":"src/main.rs"}}"#;
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
        let line = r#"{"type":"tool_execution_start","toolCallId":"tool_3","toolName":"write","args":{"path":"out.txt"}}"#;
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
        let line = r#"{"type":"tool_execution_start","toolCallId":"tool_4","toolName":"edit","args":{"path":"src/lib.rs"}}"#;
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
        let line = r#"{"type":"tool_execution_start","toolCallId":"tool_5","toolName":"grep","args":{"pattern":"foo"}}"#;
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
        let line = r#"{"type":"tool_execution_end","toolCallId":"tool_1","result":{"content":[{"type":"text","text":"On branch main"}]},"isError":false}"#;
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
        let line = r#"{"type":"tool_execution_end","toolCallId":"tool_1","result":{"content":[{"type":"text","text":"command not found"}]},"isError":true}"#;
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
    fn test_parse_event_turn_end_malformed_usage_still_completes() {
        // A malformed/schema-drifting usage object (e.g. a negative number
        // where u64 is expected) must not fail deserialization of the whole
        // turn_end line — the MessageComplete signal is the monitor's
        // turn-completion latch, so it must still be emitted, just without
        // usage stats.
        let b = backend();
        let line = r#"{"type":"turn_end","usage":{"input":-5,"output":500}}"#;
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
    fn test_parse_event_turn_end_wrong_type_usage_still_completes() {
        let b = backend();
        let line = r#"{"type":"turn_end","usage":"unexpected string shape"}"#;
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
    fn test_parse_event_agent_end_no_turns() {
        let b = backend();
        let line = r#"{"type":"agent_end"}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::Finished { usage } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 0);
                assert_eq!(u.output_tokens, 0);
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_agent_end_accumulates_turn_usage() {
        // Exercise the full parse_events -> accumulate_token_usage path (not
        // just the raw parser output) so a mismatch between what the parser
        // emits and what the runner accumulates would be caught here.
        use crate::agent_runner::accumulate_token_usage;

        let b = backend();
        let mut total = TokenUsage::default();

        for line in [
            r#"{"type":"session"}"#,
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50}}"#,
            r#"{"type":"turn_end","usage":{"input":2000,"output":300,"cacheRead":100}}"#,
            r#"{"type":"agent_end"}"#,
        ] {
            for event in b.parse_events(line) {
                accumulate_token_usage(&mut total, &event);
            }
        }

        assert_eq!(total.input_tokens, 3000);
        assert_eq!(total.output_tokens, 800);
        assert_eq!(total.cache_creation_input_tokens, Some(50));
        assert_eq!(total.cache_read_input_tokens, Some(300));
    }

    #[test]
    fn test_session_start_resets_accumulated_usage_across_invocations() {
        // The backend instance is reused across independent invocations
        // (e.g. multiple CI-fix attempts share one `&dyn AgentBackend`), so a
        // second stream's `session`/`agent_start` must not let the first
        // stream's totals leak into the second stream's `Finished` usage.
        let b = backend();

        b.parse_events(r#"{"type":"session"}"#);
        b.parse_events(
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50}}"#,
        );
        b.parse_events(r#"{"type":"agent_end"}"#);

        // New invocation reusing the same backend instance.
        b.parse_events(r#"{"type":"agent_start"}"#);
        let event = single(b.parse_events(r#"{"type":"agent_end"}"#));
        match event {
            AgentEvent::Finished { usage } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 0, "must not leak prior invocation's totals");
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }

    #[test]
    fn test_final_usage_recovers_totals_without_agent_end() {
        // If the stream ends (EOF) without an `agent_end` line — e.g. the
        // process is killed by stuck-detection, or crashes mid-session —
        // the runner has no `Finished` event to read totals from.
        // `final_usage()` is the fallback that recovers them.
        let b = backend();
        b.parse_events(r#"{"type":"session"}"#);
        b.parse_events(
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50}}"#,
        );
        b.parse_events(
            r#"{"type":"turn_end","usage":{"input":2000,"output":300,"cacheRead":100}}"#,
        );
        // No "agent_end" line.

        let usage = b.final_usage().unwrap();
        assert_eq!(usage.input_tokens, 3000);
        assert_eq!(
            usage.output_tokens, 0,
            "output already covered by MessageComplete"
        );
        assert_eq!(usage.cache_creation_input_tokens, Some(50));
        assert_eq!(usage.cache_read_input_tokens, Some(300));
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
    fn test_parse_event_wrapper_preamble_lines() {
        // Launchers and wrappers may print their own status lines to stdout
        // before Pi's event stream begins; these must be skipped, not parsed.
        let b = backend();
        assert!(b
            .parse_events("Using existing sandbox at /path/to/sandbox")
            .is_empty());
        assert!(b
            .parse_events("Using existing distribution package: npm:some-pi-package")
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
        // A missing args payload for a known tool still falls back to the
        // tool-specific phrasing rather than the generic "Tool: {name}".
        assert_eq!(format_pi_tool_summary("bash", None), "Run: bash command");
    }

    #[test]
    fn test_format_pi_tool_summary_no_args_unknown_tool() {
        assert_eq!(format_pi_tool_summary("grep", None), "Tool: grep");
    }

    #[test]
    fn test_format_pi_tool_summary_write_no_path_falls_back() {
        let args = serde_json::json!({});
        assert_eq!(format_pi_tool_summary("write", Some(&args)), "Write: file");
    }

    #[test]
    fn test_format_pi_tool_summary_edit_no_path_falls_back() {
        let args = serde_json::json!({});
        assert_eq!(format_pi_tool_summary("edit", Some(&args)), "Edit: file");
    }
}
