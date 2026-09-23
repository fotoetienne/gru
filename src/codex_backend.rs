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
//! - `thread.completed` → `AgentEvent::Finished` (with accumulated session usage)
//! - `error` → `AgentEvent::Error`

use crate::agent::{AgentBackend, AgentEvent, TokenUsage};
use crate::display_utils::{shorten_path, truncate_string};
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// OpenAI Codex CLI backend.
///
/// Implements `AgentBackend` by spawning `codex exec --json --full-auto`
/// and parsing the resulting JSONL event stream.
///
/// Codex reports input and cache token counts per-turn on `turn.completed`
/// rather than once at session start, so this backend accumulates them
/// across the session and reports the totals in the `Finished` event at
/// `thread.completed` (output tokens are already accumulated by the caller
/// from each turn's `MessageComplete`, so `Finished` reports
/// `output_tokens: 0` to avoid double-counting).
pub(crate) struct CodexBackend {
    accumulated_usage: Mutex<TokenUsage>,
    /// Path or name of the Codex CLI binary to invoke (`agent.codex.binary` in config).
    binary: String,
}

impl Default for CodexBackend {
    fn default() -> Self {
        Self {
            accumulated_usage: Mutex::default(),
            binary: "codex".to_string(),
        }
    }
}

impl CodexBackend {
    pub(crate) fn new(binary: Option<String>) -> Self {
        Self {
            accumulated_usage: Mutex::default(),
            binary: binary.unwrap_or_else(|| "codex".to_string()),
        }
    }
}

impl AgentBackend for CodexBackend {
    fn name(&self) -> &str {
        "codex"
    }

    fn process_names(&self) -> &[&str] {
        &["codex"]
    }

    fn build_command(
        &self,
        worktree_path: &Path,
        _session_id: &Uuid,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = build_codex_command(&self.binary, worktree_path, prompt);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn parse_events(&self, line: &str) -> Vec<AgentEvent> {
        parse_codex_event(line.trim(), &self.accumulated_usage)
            .into_iter()
            .collect()
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
        let mut cmd = build_codex_resume_command(&self.binary, worktree_path, prompt);
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

    fn install_url(&self) -> Option<&'static str> {
        Some("https://github.com/openai/codex")
    }

    fn build_interactive_command(
        &self,
        _cwd: &Path,
        _system_prompt: &str,
        _initial_prompt: Option<&str>,
        _github_host: &str,
    ) -> Option<TokioCommand> {
        // Codex CLI has no interactive entry point that accepts a custom
        // system prompt, so `gru chat`/`pm`/`tpm` are unsupported here.
        None
    }

    fn build_oneshot_command(
        &self,
        worktree_path: &Path,
        prompt_arg: &str,
        github_host: &str,
    ) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.binary);
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
            .stderr(std::process::Stdio::inherit())
            .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
            .env_remove(crate::labels::GRU_CONFIG_PATH_ENV)
            .env("GH_HOST", github_host);
        cmd
    }

    fn build_ci_fix_command(
        &self,
        worktree_path: &Path,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand {
        self.build_command(worktree_path, &Uuid::nil(), prompt, github_host)
    }

    fn final_usage(&self) -> Option<TokenUsage> {
        Some(self.accumulated_usage.lock().unwrap().clone())
    }

    fn reset_usage(&self) {
        *self.accumulated_usage.lock().unwrap() = TokenUsage::default();
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// Builds a Codex command for a new session.
///
/// Uses `codex exec --json --full-auto` for autonomous headless execution
/// with JSONL streaming output.
fn build_codex_command(binary: &str, worktree_path: &Path, prompt: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new(binary);
    cmd.arg("exec")
        .arg("--json")
        .arg("--full-auto")
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path)
        // Prevent GRU_RETRY_PARENT and GRU_CONFIG_PATH from leaking into Codex
        // and any tools it spawns — these guards are meant for the direct gru
        // do/resume process only (mirrors claude_runner.rs).
        .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
        .env_remove(crate::labels::GRU_CONFIG_PATH_ENV);
    cmd
}

/// Builds a Codex command to resume the most recent session.
fn build_codex_resume_command(binary: &str, worktree_path: &Path, prompt: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new(binary);
    cmd.arg("exec")
        .arg("resume")
        .arg("--last")
        .arg("--json")
        .arg("--full-auto")
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(worktree_path)
        .env_remove(crate::labels::GRU_RETRY_PARENT_ENV)
        .env_remove(crate::labels::GRU_CONFIG_PATH_ENV);
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
fn parse_codex_event(line: &str, accumulated_usage: &Mutex<TokenUsage>) -> Option<AgentEvent> {
    if line.is_empty() {
        return None;
    }

    let event: CodexEvent = serde_json::from_str(line).ok()?;

    match event.event_type.as_str() {
        // Usage accumulation is reset once per invocation via
        // `AgentBackend::reset_usage()` (called by the runner before the
        // process is spawned), not here — a startup failure could exit
        // before this event ever arrives.
        "thread.started" => Some(AgentEvent::Started { usage: None }),

        "turn.started" => Some(AgentEvent::Thinking { text: None }),

        "turn.completed" => {
            let usage = event.usage.map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_input_tokens: u.cached_input_tokens,
                ..Default::default()
            });
            if let Some(u) = &usage {
                let mut accumulated = accumulated_usage.lock().unwrap();
                accumulated.input_tokens += u.input_tokens;
                if let Some(cache_read) = u.cache_read_input_tokens {
                    *accumulated.cache_read_input_tokens.get_or_insert(0) += cache_read;
                }
            }
            Some(AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage,
            })
        }

        // TODO: verify `thread.completed` against real Codex CLI output once
        // available — inferred as the terminal counterpart to `thread.started`
        // but not yet confirmed against an actual `codex exec --json` session
        // (see `turn.failed` above for the same class of open verification).
        // If the real event name/shape differs, Codex input/cache totals
        // silently stay at zero since unrecognized types fall through to `_ => None`.
        "thread.completed" => {
            let totals = accumulated_usage.lock().unwrap().clone();
            Some(AgentEvent::Finished {
                usage: Some(totals),
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
        CodexBackend::default()
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
    fn test_binary_override_used_for_all_commands() {
        let b = CodexBackend::new(Some("/opt/tools/codex".to_string()));
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();

        assert_eq!(
            b.build_command(&path, &session_id, "p", "github.com")
                .as_std()
                .get_program(),
            "/opt/tools/codex"
        );
        assert_eq!(
            b.build_resume_command(&path, &session_id, "p", "github.com")
                .unwrap()
                .as_std()
                .get_program(),
            "/opt/tools/codex"
        );
        assert_eq!(
            b.build_oneshot_command(&path, "p", "github.com")
                .as_std()
                .get_program(),
            "/opt/tools/codex"
        );
        assert_eq!(
            b.build_ci_fix_command(&path, "p", "github.com")
                .as_std()
                .get_program(),
            "/opt/tools/codex"
        );
    }

    #[test]
    fn test_binary_falls_back_to_codex_when_unset() {
        let b = CodexBackend::new(None);
        let path = std::path::PathBuf::from("/tmp/worktree");
        let session_id = Uuid::nil();
        assert_eq!(
            b.build_command(&path, &session_id, "p", "github.com")
                .as_std()
                .get_program(),
            "codex"
        );
    }

    #[test]
    fn test_yolo_args_empty() {
        assert!(backend().yolo_args().is_empty());
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
    fn test_build_interactive_command_unsupported() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/project");
        assert!(b
            .build_interactive_command(&path, "you are a PM", Some("hi"), "github.com")
            .is_none());
    }

    #[test]
    fn test_build_oneshot_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_oneshot_command(&path, "fix the tests", "github.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
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

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
        // "-" should NOT appear as an argument when using stdin sentinel
        assert!(!args.contains(&"-".as_ref()));
    }

    #[test]
    fn test_build_ci_fix_command_produces_expected_args() {
        let b = backend();
        let path = std::path::PathBuf::from("/tmp/worktree");
        let cmd = b.build_ci_fix_command(&path, "fix the CI", "github.example.com");
        let inner = cmd.as_std();

        assert_eq!(inner.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"exec".as_ref()));
        assert!(args.contains(&"--json".as_ref()));
        assert!(args.contains(&"--full-auto".as_ref()));
        assert!(args.contains(&"fix the CI".as_ref()));
        // GH_HOST must be set for GitHub Enterprise compatibility
        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.example.com".as_ref())),
            "GH_HOST should be set on CI fix command"
        );
    }

    #[test]
    fn test_all_command_builders_remove_gru_worker_env_vars() {
        // GRU_RETRY_PARENT and GRU_CONFIG_PATH must not leak from the worker
        // process into Codex or its tool subprocesses, matching the
        // env_remove calls in claude_backend.rs and pi_backend.rs.
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

        assert_removed(&b.build_command(&path, &session_id, "fix the bug", "github.com"));
        assert_removed(
            &b.build_resume_command(&path, &session_id, "continue", "github.com")
                .unwrap(),
        );
        assert_removed(&b.build_oneshot_command(&path, "fix the tests", "github.com"));
        assert_removed(&b.build_ci_fix_command(&path, "fix the CI", "github.com"));
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
    fn test_parse_event_thread_completed_no_turns() {
        let b = backend();
        let line = r#"{"type":"thread.completed"}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::Finished { usage } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 0);
                assert_eq!(u.output_tokens, 0);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_thread_completed_accumulates_turn_usage() {
        // Exercise the full parse_events -> accumulate_token_usage path (not
        // just the raw parser output) so a mismatch between what the parser
        // emits and what the runner accumulates would be caught here.
        use crate::agent_runner::accumulate_token_usage;

        let b = backend();
        let mut total = TokenUsage::default();

        for line in [
            r#"{"type":"thread.started","thread_id":"thread_abc123"}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":2000,"output_tokens":300,"cached_input_tokens":100}}"#,
            r#"{"type":"thread.completed"}"#,
        ] {
            for event in b.parse_events(line) {
                accumulate_token_usage(&mut total, &event);
            }
        }

        assert_eq!(total.input_tokens, 3000);
        assert_eq!(total.output_tokens, 800);
        assert_eq!(total.cache_read_input_tokens, Some(300));
    }

    #[test]
    fn test_reset_usage_clears_accumulated_usage_across_invocations() {
        // The backend instance is reused across independent invocations
        // (e.g. multiple CI-fix attempts share one `&dyn AgentBackend`).
        // `run_agent_with_stream_monitoring` calls `reset_usage()` before
        // spawning each new invocation's process — not on `thread.started`,
        // since a startup/auth failure could exit before that event ever
        // arrives — so this must clear totals left over from a prior
        // invocation regardless of what that invocation emitted.
        let b = backend();

        b.parse_events(r#"{"type":"thread.started","thread_id":"thread_1"}"#);
        b.parse_events(
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200}}"#,
        );
        b.parse_events(r#"{"type":"thread.completed"}"#);

        // New invocation reusing the same backend instance.
        b.reset_usage();
        b.parse_events(r#"{"type":"thread.started","thread_id":"thread_2"}"#);
        let event = single(b.parse_events(r#"{"type":"thread.completed"}"#));
        match event {
            AgentEvent::Finished { usage } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 0, "must not leak prior invocation's totals");
                assert_eq!(u.cache_read_input_tokens, None);
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }

    #[test]
    fn test_reset_usage_clears_state_even_if_prior_invocation_never_started() {
        // If a process from a prior invocation exited before ever emitting
        // `thread.started` (e.g. a startup/auth failure with no JSON
        // stdout), its accumulated usage from a still-earlier invocation
        // could linger. `reset_usage()` must clear it regardless, since the
        // runner calls it unconditionally before spawning — it cannot rely
        // on `thread.started` having fired for the invocation being reset.
        let b = backend();

        b.parse_events(r#"{"type":"thread.started","thread_id":"thread_1"}"#);
        b.parse_events(
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200}}"#,
        );
        // Invocation 1 ends here (crashed before "thread.completed").

        // Invocation 2 starts: runner resets, but this process fails before
        // ever emitting "thread.started" or any usage-bearing event.
        b.reset_usage();

        // Invocation 3 starts: runner resets again.
        b.reset_usage();
        let usage = b.final_usage().unwrap();
        assert_eq!(usage.input_tokens, 0, "must not leak invocation 1's totals");
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    #[test]
    fn test_final_usage_recovers_totals_without_thread_completed() {
        // If the real Codex CLI never emits `thread.completed` (unverified
        // — see the TODO on that match arm), the stream ends (EOF) without
        // a `Finished` event to read totals from. `final_usage()` is the
        // fallback `run_agent_with_stream_monitoring` calls in that case so
        // input/cache totals aren't silently lost.
        let b = backend();
        b.parse_events(r#"{"type":"thread.started","thread_id":"thread_abc123"}"#);
        b.parse_events(
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200}}"#,
        );
        b.parse_events(
            r#"{"type":"turn.completed","usage":{"input_tokens":2000,"output_tokens":300,"cached_input_tokens":100}}"#,
        );
        // No "thread.completed" line.

        let usage = b.final_usage().unwrap();
        assert_eq!(usage.input_tokens, 3000);
        assert_eq!(
            usage.output_tokens, 0,
            "output already covered by MessageComplete"
        );
        assert_eq!(usage.cache_read_input_tokens, Some(300));
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
