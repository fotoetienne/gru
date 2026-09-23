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
    /// Reports which provider/model actually served a turn.
    ///
    /// Emitted by backends (e.g. Pi) that let the underlying CLI resolve its
    /// own model rather than having Gru pin one — see `pi_backend.rs` for the
    /// rationale. This is purely informational so `events.jsonl` records
    /// which model did the work; it has no effect on execution.
    ModelInfo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
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
    /// Accumulated dollar cost, when the backend reports it (currently only
    /// Pi). `None` means the backend does not report cost, not zero cost —
    /// callers must not render `$0.00` for backends that omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<f64>,
}

impl TokenUsage {
    /// Returns total tokens (input + output).
    pub(crate) fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Format as a compact display string (e.g., "12.3k in / 4.5k out").
    /// Appends accumulated cost (e.g., "$0.0088") when the backend reports it.
    pub(crate) fn display_compact(&self) -> String {
        let base = format!(
            "{} in / {} out",
            format_token_count(self.input_tokens),
            format_token_count(self.output_tokens)
        );
        match self.cost {
            Some(cost) => format!("{} ({})", base, format_cost(cost)),
            None => base,
        }
    }
}

/// Format a dollar cost, widening precision beyond the usual 4 decimals so a
/// small-but-nonzero cost (Pi can report category costs as low as
/// `0.000012`) never rounds down to a misleading "$0.0000".
fn format_cost(cost: f64) -> String {
    let mut decimals = 4;
    loop {
        let formatted = format!("{:.*}", decimals, cost);
        let rounds_to_zero = formatted.parse::<f64>().unwrap_or(0.0) == 0.0;
        if cost == 0.0 || !rounds_to_zero || decimals >= 10 {
            return format!("${}", formatted);
        }
        decimals += 2;
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

/// Builds an actionable message for a failed `TokioCommand::spawn()` on an
/// agent backend's command, naming the exact binary path that was attempted
/// and the `[agent.<name>]` config key that controls it — a bare OS error
/// ("No such file or directory") gives no hint that the culprit is a
/// misconfigured binary override (e.g. a relative or nonexistent path)
/// rather than a missing PATH install.
///
/// Must be called with `cmd` *before* `spawn()` consumes it. `context_suffix`
/// is appended to the failure headline (e.g. `"for merge judge"`); pass `""`
/// for the plain "Failed to start ... agent binary '...'." form.
///
/// `process_names()[0]` doubles as the config section name for every
/// built-in backend (claude/pi/codex) — unlike `backend.name()`, which for
/// Claude is the display name "claude-code", not the config key "claude".
///
/// When the backend supplies an `install_url()`, it is appended as an install
/// pointer: the most likely reason a first-run user hits this is that the CLI
/// isn't installed at all, and a config-key hint alone doesn't help them.
pub(crate) fn spawn_error_context(
    backend: &dyn AgentBackend,
    cmd: &TokioCommand,
    context_suffix: &str,
) -> String {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let config_key = backend
        .process_names()
        .first()
        .copied()
        .unwrap_or(backend.name());
    let suffix = if context_suffix.is_empty() {
        String::new()
    } else {
        format!(" {context_suffix}")
    };
    let install_hint = match backend.install_url() {
        Some(url) => format!(" If it isn't installed yet, see {url}."),
        None => String::new(),
    };
    format!(
        "Failed to start {} agent binary '{}'{}. Check that it exists, is executable, \
         and (if relative) is resolvable from the current directory — see [agent.{}] \
         binary in config.toml if you've overridden it.{}",
        backend.name(),
        program,
        suffix,
        config_key,
        install_hint
    )
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

    /// Returns the process name(s) to look for when scanning for this backend's
    /// running processes (e.g., `["claude"]`, `["codex"]`).
    ///
    /// Used by `gru stop`'s process-scan fallback to build a `pgrep -f` pattern
    /// that covers every registered backend rather than a hardcoded alternation.
    fn process_names(&self) -> &[&str];

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

    /// Build the command to start a *fresh* interactive session with a custom
    /// system prompt.
    ///
    /// Unlike `build_interactive_resume_command` (which reattaches to an existing
    /// session), this starts a new session for the user-facing REPL commands
    /// `gru chat`, `gru pm`, and `gru tpm`. The command must use inherited stdio
    /// and must not request headless/streaming output (no `--print`, no
    /// stream-json), since the agent's own TUI takes over the terminal.
    ///
    /// `system_prompt` carries the role/project context. `initial_prompt`, when
    /// `Some`, becomes the session's first user message and must be passed after
    /// an argument terminator (`--`) so prompts that look like flags (e.g. `-h`)
    /// aren't parsed as CLI options.
    ///
    /// `github_host`, when `Some`, must be exported as `GH_HOST` (as the resume
    /// variant does) so `gh` calls the agent makes during the session target the
    /// right GitHub Enterprise instance instead of defaulting to github.com. A
    /// `None` means no host could be resolved and the variable must be left
    /// untouched, so an inherited `GH_HOST` survives rather than being
    /// overridden by a guess.
    ///
    /// Returns `None` if the backend has no interactive entry point (Codex).
    fn build_interactive_command(
        &self,
        cwd: &Path,
        system_prompt: &str,
        initial_prompt: Option<&str>,
        github_host: Option<&str>,
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
    ///
    /// `github_host` is forwarded as `GH_HOST` so `gh` CLI calls inside the
    /// agent target the correct GitHub Enterprise host.
    fn build_oneshot_command(
        &self,
        worktree_path: &Path,
        prompt_arg: &str,
        github_host: &str,
    ) -> TokioCommand;

    /// Builds a streaming command for a CI fix invocation.
    ///
    /// Unlike `build_oneshot_command`, this command must produce a stream-json
    /// event stream on stdout (the same format as `build_command`) so that
    /// `run_agent_with_stream_monitoring` can capture tool calls and text to
    /// `events.jsonl`. It does not require a session ID because CI fix
    /// invocations are stateless one-shots.
    ///
    /// `github_host` is forwarded as `GH_HOST` so `gh` CLI calls inside the
    /// agent target the correct GitHub Enterprise host.
    fn build_ci_fix_command(
        &self,
        worktree_path: &Path,
        prompt: &str,
        github_host: &str,
    ) -> TokioCommand;

    /// Returns the CLI args this backend uses to bypass interactive permission
    /// prompts (e.g. `["--dangerously-skip-permissions"]` for Claude Code).
    ///
    /// Used by `gru attach --yolo` to append the backend-appropriate flag
    /// instead of hardcoding a Claude-specific one. Backends that have no
    /// such flag (or execute autonomously by default) should return an empty
    /// `Vec`, which is the default.
    fn yolo_args(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// (Optional) Where a user can install this backend's CLI.
    ///
    /// Appended to `spawn_error_context`'s message so a "binary not found"
    /// failure points somewhere useful — this matters most on the first-run
    /// paths (`gru chat`, `gru init`) where the CLI may simply be absent.
    /// Defaults to `None` (no install pointer).
    fn install_url(&self) -> Option<&'static str> {
        None
    }

    /// Returns any session usage totals accumulated internally by the
    /// backend that were never surfaced through a `Finished` event.
    ///
    /// Backends that report input/cache token usage per-turn (rather than
    /// once via `Started`) accumulate those totals internally and normally
    /// flush them in a `Finished` event tied to a backend-specific
    /// session-end marker (e.g. Codex's inferred `thread.completed`). If
    /// the stream ends (EOF) without that marker ever appearing — because
    /// the real CLI doesn't emit it, or emits something else — the caller
    /// has no other way to recover those totals. `run_agent_with_stream_monitoring`
    /// calls this once the stream loop exits, and folds the result into the
    /// final token usage if no `Finished { usage: Some(_) }` was already
    /// observed, so a wrong session-end event name degrades to "still
    /// accurate, just recovered a different way" instead of "silently zero".
    ///
    /// Returns `None` by default; backends without per-turn accumulation
    /// (e.g. Claude Code) don't need to override this.
    fn final_usage(&self) -> Option<TokenUsage> {
        None
    }

    /// Clears any usage totals accumulated internally by the backend from a
    /// prior invocation.
    ///
    /// `run_agent_with_stream_monitoring` calls this once, unconditionally,
    /// before spawning the process for a new invocation. A backend instance
    /// is reused across independent invocations (e.g. `src/ci.rs`'s CI-fix
    /// retry loop drives multiple attempts through the same
    /// `&dyn AgentBackend`), so relying solely on a stream-start event
    /// (e.g. Pi's `session`/`agent_start`, Codex's `thread.started`) to
    /// reset state is not safe: if a new process exits before ever emitting
    /// that event (a startup or auth failure with no JSON stdout), the
    /// previous invocation's totals would otherwise leak into
    /// `final_usage()`'s result for this one.
    ///
    /// No-op by default; backends without per-turn accumulation (e.g.
    /// Claude Code) don't need to override this.
    fn reset_usage(&self) {}

    /// Sanitizes raw stdout from `build_oneshot_command` before a consumer
    /// (e.g. `merge_judge.rs`) treats it as the agent's plain-text answer.
    ///
    /// `build_oneshot_command` documents plain-text stdout, but a backend
    /// invoked through a launcher/wrapper may have bootstrap chatter written
    /// to stdout ahead of the real output (see `PiBackend`, which overrides
    /// this to strip it). Identity by default — Claude and Codex are invoked
    /// directly and their stdout is already clean.
    fn sanitize_oneshot_output(&self, raw: &str) -> String {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_error_context_uses_config_key_not_display_name() {
        // ClaudeBackend::name() returns "claude-code", but the config
        // section is [agent.claude] — the message must use the latter.
        let backend = crate::claude_backend::ClaudeBackend::new(
            None,
            Some("/opt/tools/does-not-exist".to_string()),
            None,
        );
        let cmd = backend.build_oneshot_command(
            std::path::Path::new("/tmp/worktree"),
            "prompt",
            "github.com",
        );
        let msg = spawn_error_context(&backend, &cmd, "");
        assert!(msg.contains("claude-code"), "{msg}");
        assert!(msg.contains("/opt/tools/does-not-exist"), "{msg}");
        assert!(msg.contains("[agent.claude] binary"), "{msg}");
    }

    #[test]
    fn test_spawn_error_context_appends_suffix() {
        let backend = crate::codex_backend::CodexBackend::new(None);
        let cmd = backend.build_oneshot_command(
            std::path::Path::new("/tmp/worktree"),
            "prompt",
            "github.com",
        );
        let msg = spawn_error_context(&backend, &cmd, "for merge judge");
        assert!(msg.contains("for merge judge"), "{msg}");
        assert!(msg.contains("[agent.codex] binary"), "{msg}");
    }

    #[test]
    fn test_spawn_error_context_includes_install_url() {
        // `gru chat` is the first-run path: a missing binary most often means
        // the CLI was never installed, so the message must point somewhere.
        let backend = crate::claude_backend::ClaudeBackend::new(None, None, None);
        let cmd = backend.build_oneshot_command(
            std::path::Path::new("/tmp/project"),
            "prompt",
            "github.com",
        );
        let msg = spawn_error_context(&backend, &cmd, "for gru chat");
        assert!(msg.contains("https://claude.com/claude-code"), "{msg}");

        let codex = crate::codex_backend::CodexBackend::new(None);
        let cmd = codex.build_oneshot_command(
            std::path::Path::new("/tmp/worktree"),
            "prompt",
            "github.com",
        );
        let msg = spawn_error_context(&codex, &cmd, "");
        assert!(msg.contains("https://github.com/openai/codex"), "{msg}");
    }

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
                cost: None,
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
                cost: None,
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
    fn test_agent_event_model_info_roundtrip() {
        let event = AgentEvent::ModelInfo {
            provider: Some("nflx-openai".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_agent_event_model_info_none_fields_omitted() {
        let event = AgentEvent::ModelInfo {
            provider: None,
            model: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "model_info");
        assert!(value.get("provider").is_none());
        assert!(value.get("model").is_none());

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
            cost: Some(0.0088),
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
        assert_eq!(usage.cost, None);
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
        assert!(value.get("cost").is_none());
    }

    #[test]
    fn test_token_usage_display_compact_with_cost() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cost: Some(0.0088216),
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "1.0k in / 500 out ($0.0088)");
    }

    #[test]
    fn test_token_usage_display_compact_without_cost() {
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "1.0k in / 500 out");
    }

    #[test]
    fn test_token_usage_display_compact_small_nonzero_cost_not_rounded_to_zero() {
        // A cost this small would round to "$0.0000" at the usual 4 decimals,
        // making a real non-zero session cost look like zero.
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cost: Some(0.000012),
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "10 in / 5 out ($0.000012)");
    }

    #[test]
    fn test_token_usage_display_compact_zero_cost() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cost: Some(0.0),
            ..Default::default()
        };
        assert_eq!(usage.display_compact(), "10 in / 5 out ($0.0000)");
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
