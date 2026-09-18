//! Pi CLI backend implementation for the `AgentBackend` trait.
//!
//! Implements the `AgentBackend` interface for [Pi](https://github.com/earendil-works/pi-mono)
//! (`pi`, npm `@earendil-works/pi-coding-agent`), mapping its JSONL streaming
//! output (`pi -p --mode json`) to normalized `AgentEvent`s.
//!
//! Pi event types:
//! - `session` / `agent_start` → `AgentEvent::Started`
//! - `turn_start` → `AgentEvent::Thinking`
//! - `message_start` → `AgentEvent::ModelInfo`
//! - `message_update` (`assistantMessageEvent.type == "text_delta"`) → `AgentEvent::TextDelta`
//! - `tool_execution_start` → `AgentEvent::ToolUse`
//! - `tool_execution_end` → `AgentEvent::ToolResult`
//! - `turn_end` → `AgentEvent::MessageComplete` (plus `AgentEvent::ModelInfo`
//!   when the event also carries `provider`/`model`)
//! - `agent_end` → `AgentEvent::Finished`
//! - `turn_failed` / `error` → `AgentEvent::Error`
//!
//! Unlike Codex, Pi supports interactive session resume (needed by `gru attach`)
//! and has no `--dangerously-skip-permissions` equivalent — autonomous tool use
//! is the default under `-p`.
//!
//! ## Model selection is intentionally Pi's decision, not Gru's
//!
//! `PiBackend` passes `--model`/`--thinking` only when `[agent.pi]` sets
//! them (see `apply_model_flags`). With no config it passes neither and lets
//! Pi resolve its own default — Pi's provider/model is pluggable, so "the
//! default" varies by machine and Pi version, not a fixed value Gru could
//! usefully pin. Pi users have already configured Pi for their own needs;
//! having Gru override that would be surprising. Do not "fix" this by
//! defaulting `model`/`thinking` to a hardcoded value — instead, the actual
//! provider/model Pi used is captured from `message_start` events (see
//! `AgentEvent::ModelInfo`) so `events.jsonl` still records which model did
//! the work, without Gru forcing a choice.

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
/// Pi reports input and cache token counts, plus dollar cost, per-turn on
/// `turn_end` rather than once at session start, so this backend accumulates
/// them across the session and reports the totals in the `Finished` event at
/// `agent_end` (output tokens are already accumulated by the caller from
/// each turn's `MessageComplete`, so `Finished` reports `output_tokens: 0`
/// to avoid double-counting).
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
            .arg(session_id.to_string())
            // Pi already withholds project-local trust by default, but pass
            // this explicitly so a future change to Pi's default can't
            // silently start executing repo-supplied extensions/skills from
            // a freshly-cloned worktree.
            .arg("--no-approve");
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
        cmd.arg("--session-id")
            .arg(session_id.to_string())
            // Locks in the safe default; see build_command for rationale.
            .arg("--no-approve");
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
        // Locks in the safe default; see build_command for rationale.
        cmd.arg("-p").arg("--no-approve");
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
        // Locks in the safe default; see build_command for rationale.
        cmd.arg("-p").arg("--mode").arg("json").arg("--no-approve");
        self.apply_model_flags(&mut cmd);
        apply_pi_stdio(cmd.arg(prompt), worktree_path);
        cmd.env("GH_HOST", github_host);
        cmd
    }

    fn final_usage(&self) -> Option<TokenUsage> {
        Some(self.accumulated_usage.lock().unwrap().clone())
    }

    fn reset_usage(&self) {
        *self.accumulated_usage.lock().unwrap() = TokenUsage::default();
    }

    /// Some environments invoke Pi through a launcher/wrapper that writes its
    /// own bootstrap lines to stdout before Pi's real output, e.g.:
    ///
    /// ```text
    /// Using existing sandbox at /path/to/sandbox
    /// Using existing distribution package: npm:<some-pi-package>
    /// ```
    ///
    /// These lines have no bearing on Pi's own output format, so only a
    /// fixed set of known launcher-preamble prefixes is stripped, and only
    /// while they appear contiguously at the very start of stdout — this
    /// avoids discarding legitimate Pi output that happens to start with a
    /// similar-looking line further down.
    fn sanitize_oneshot_output(&self, raw: &str) -> String {
        const LAUNCHER_PREAMBLE_PREFIXES: &[&str] = &[
            "Using existing sandbox at ",
            "Using existing distribution package: ",
        ];

        let mut lines: Vec<&str> = raw.lines().collect();
        while let Some(first) = lines.first() {
            if LAUNCHER_PREAMBLE_PREFIXES
                .iter()
                .any(|prefix| first.starts_with(prefix))
            {
                lines.remove(0);
            } else {
                break;
            }
        }
        lines.join("\n")
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
    /// Present on `error` / `turn_failed` events as a plain string, and on
    /// `message_start` events as a `{role, provider, model, ...}` object —
    /// hence `Value` rather than `String`; see `parse_pi_event` for the
    /// per-event-type interpretation.
    #[serde(default)]
    message: Option<serde_json::Value>,
    /// Present at the top level on `turn_end` events (alongside `usage`).
    /// `message_start` instead nests these under `message` — see
    /// `extract_provider_model`.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Extracts `provider`/`model` from wherever a given Pi event puts them:
/// top-level fields (`turn_end`) or nested under `message` (`message_start`).
fn extract_provider_model(event: &PiEvent) -> (Option<String>, Option<String>) {
    if event.provider.is_some() || event.model.is_some() {
        return (event.provider.clone(), event.model.clone());
    }
    let Some(message) = &event.message else {
        return (None, None);
    };
    let provider = message
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let model = message
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (provider, model)
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
    #[serde(default)]
    cost: Option<PiCost>,
}

/// Dollar cost breakdown nested under a Pi `usage` object. Only the `total`
/// is surfaced today; per-category costs aren't tracked separately.
///
/// `total` is `Option` rather than defaulting to `0.0`: a `cost` object
/// present without a valid `total` (schema drift) must not fabricate a real
/// `Some(0.0)` cost — `TokenUsage.cost` should stay `None` in that case, same
/// as when `cost` is absent entirely.
#[derive(Debug, Deserialize)]
struct PiCost {
    #[serde(default)]
    total: Option<f64>,
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
        // Usage accumulation is reset once per invocation via
        // `AgentBackend::reset_usage()` (called by the runner before the
        // process is spawned), not here — a startup failure could exit
        // before this event ever arrives.
        "session" | "agent_start" => vec![AgentEvent::Started { usage: None }],

        "turn_start" => vec![AgentEvent::Thinking { text: None }],

        "message_start" => {
            let (provider, model) = extract_provider_model(&event);
            if provider.is_none() && model.is_none() {
                return Vec::new();
            }
            vec![AgentEvent::ModelInfo { provider, model }]
        }

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
            let (provider, model) = extract_provider_model(&event);
            let usage = parse_usage(event.usage).map(|u| TokenUsage {
                input_tokens: u.input,
                output_tokens: u.output,
                cache_read_input_tokens: u.cache_read,
                cache_creation_input_tokens: u.cache_write,
                cost: u.cost.and_then(|c| c.total),
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
                if let Some(cost) = u.cost {
                    *accumulated.cost.get_or_insert(0.0) += cost;
                }
            }
            let mut events = Vec::with_capacity(2);
            if provider.is_some() || model.is_some() {
                events.push(AgentEvent::ModelInfo { provider, model });
            }
            events.push(AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage,
            });
            events
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
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("Pi agent error")
                .to_string();
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
        assert!(args.contains(&"--no-approve".as_ref()));
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
        assert!(args.contains(&"--no-approve".as_ref()));
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
        assert!(args.contains(&"--no-approve".as_ref()));
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
        assert!(args.contains(&"--no-approve".as_ref()));
        assert!(args.contains(&"fix the tests".as_ref()));

        let envs: Vec<_> = inner.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "GH_HOST" && *v == Some("github.com".as_ref())),
            "GH_HOST should be set on the oneshot command"
        );
    }

    #[test]
    fn test_sanitize_oneshot_output_strips_launcher_preamble() {
        let b = backend();
        let raw = "Using existing sandbox at /path/to/sandbox\n\
                   Using existing distribution package: npm:@earendil-works/pi-coding-agent\n\
                   {\"confidence\": 8, \"action\": \"merge\"}";
        let sanitized = b.sanitize_oneshot_output(raw);
        assert_eq!(sanitized, "{\"confidence\": 8, \"action\": \"merge\"}");
    }

    #[test]
    fn test_sanitize_oneshot_output_preserves_output_without_preamble() {
        let b = backend();
        let raw = "{\"confidence\": 8, \"action\": \"merge\"}";
        assert_eq!(b.sanitize_oneshot_output(raw), raw);
    }

    #[test]
    fn test_sanitize_oneshot_output_only_strips_leading_preamble_lines() {
        let b = backend();
        // A legitimate answer that merely mentions the phrase mid-output
        // must survive untouched once real content has started.
        let raw = "Using existing sandbox at /path/to/sandbox\n\
                   The agent reported: Using existing sandbox at /other/path";
        assert_eq!(
            b.sanitize_oneshot_output(raw),
            "The agent reported: Using existing sandbox at /other/path"
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
        assert!(args.contains(&"--no-approve".as_ref()));
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
        let line = r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50,"cost":{"total":0.01}}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 1000);
                assert_eq!(u.output_tokens, 500);
                assert_eq!(u.cache_read_input_tokens, Some(200));
                assert_eq!(u.cache_creation_input_tokens, Some(50));
                assert_eq!(u.cost, Some(0.01));
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_turn_end_without_cost() {
        // Cost is absent from the usage object on backends/providers that
        // don't report it — must not be conjured as Some(0.0).
        let b = backend();
        let line = r#"{"type":"turn_end","usage":{"input":1000,"output":500}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { usage, .. } => {
                assert_eq!(usage.unwrap().cost, None);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_turn_end_with_malformed_cost_object_degrades_gracefully() {
        // A `cost` object present but missing `total` (schema drift) must
        // not fail parsing of the whole `usage` object and drop input/
        // output/cache tokens along with it. It also must not fabricate a
        // fake `Some(0.0)` cost — `cost` should stay `None`, same as when
        // the `cost` object is absent entirely.
        let b = backend();
        let line = r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cost":{}}}"#;
        let event = single(b.parse_events(line));
        match event {
            AgentEvent::MessageComplete { usage, .. } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 1000);
                assert_eq!(u.output_tokens, 500);
                assert_eq!(u.cost, None);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_turn_end_with_provider_and_model() {
        let b = backend();
        let line = r#"{"type":"turn_end","provider":"nflx-openai","model":"gpt-5.6-sol","usage":{"input":1000,"output":500}}"#;
        let events = b.parse_events(line);
        assert_eq!(
            events[0],
            AgentEvent::ModelInfo {
                provider: Some("nflx-openai".to_string()),
                model: Some("gpt-5.6-sol".to_string()),
            }
        );
        assert!(matches!(events[1], AgentEvent::MessageComplete { .. }));
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
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50,"cost":{"total":0.005}}}"#,
            r#"{"type":"turn_end","usage":{"input":2000,"output":300,"cacheRead":100,"cost":{"total":0.003}}}"#,
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
        // Float accumulation is approximate; don't assert exact equality.
        assert!((total.cost.unwrap() - 0.008).abs() < 1e-9);
    }

    #[test]
    fn test_reset_usage_clears_accumulated_usage_across_invocations() {
        // The backend instance is reused across independent invocations
        // (e.g. multiple CI-fix attempts share one `&dyn AgentBackend`).
        // `run_agent_with_stream_monitoring` calls `reset_usage()` before
        // spawning each new invocation's process — not on `session`/
        // `agent_start`, since a startup failure could exit before that
        // event ever arrives — so this must clear totals left over from a
        // prior invocation regardless of what that invocation emitted.
        let b = backend();

        b.parse_events(r#"{"type":"session"}"#);
        b.parse_events(
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50,"cost":{"total":0.01}}}"#,
        );
        b.parse_events(r#"{"type":"agent_end"}"#);

        // New invocation reusing the same backend instance.
        b.reset_usage();
        b.parse_events(r#"{"type":"agent_start"}"#);
        let event = single(b.parse_events(r#"{"type":"agent_end"}"#));
        match event {
            AgentEvent::Finished { usage } => {
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 0, "must not leak prior invocation's totals");
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
                assert_eq!(u.cost, None, "must not leak prior invocation's cost");
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }

    #[test]
    fn test_reset_usage_clears_state_even_if_prior_invocation_never_started() {
        // If a process from a prior invocation exited before ever emitting
        // `session`/`agent_start` (e.g. a startup failure with no JSON
        // stdout), its accumulated usage from a still-earlier invocation
        // could linger. `reset_usage()` must clear it regardless, since the
        // runner calls it unconditionally before spawning — it cannot rely
        // on `session`/`agent_start` having fired for the invocation being
        // reset.
        let b = backend();

        b.parse_events(r#"{"type":"session"}"#);
        b.parse_events(
            r#"{"type":"turn_end","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":50}}"#,
        );
        // Invocation 1 ends here (crashed before "agent_end").

        // Invocation 2 starts: runner resets, but this process fails before
        // ever emitting "session"/"agent_start" or any usage-bearing event.
        b.reset_usage();

        // Invocation 3 starts: runner resets again.
        b.reset_usage();
        let usage = b.final_usage().unwrap();
        assert_eq!(usage.input_tokens, 0, "must not leak invocation 1's totals");
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, None);
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
    fn test_parse_event_message_start_captures_provider_and_model() {
        let b = backend();
        let line = r#"{"type":"message_start","message":{"role":"assistant","api":"openai-responses","provider":"nflx-openai","model":"gpt-5.6-sol"}}"#;
        let event = single(b.parse_events(line));
        assert_eq!(
            event,
            AgentEvent::ModelInfo {
                provider: Some("nflx-openai".to_string()),
                model: Some("gpt-5.6-sol".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_event_message_start_no_provider_or_model_ignored() {
        let b = backend();
        let line = r#"{"type":"message_start","message":{"role":"assistant"}}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_message_start_missing_message_ignored() {
        let b = backend();
        let line = r#"{"type":"message_start"}"#;
        assert!(b.parse_events(line).is_empty());
    }

    #[test]
    fn test_parse_event_ignored_types() {
        let b = backend();
        for event_type in [
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
