//! Claude CLI command builders.
//!
//! Contains the low-level functions for constructing `TokioCommand` values
//! that invoke the Claude Code CLI. These are used exclusively by
//! `ClaudeBackend` (via the `AgentBackend` trait) and should not be called
//! directly from orchestration modules.

use std::path::Path;
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Builds a standard Claude command with common flags.
///
/// Creates a TokioCommand configured for non-interactive stream-json output.
pub(crate) fn build_claude_command(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
    github_host: &str,
) -> TokioCommand {
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
        .current_dir(worktree_path)
        .env("GH_HOST", github_host)
        // Prevent GRU_RETRY_PARENT from leaking into Claude Code and any tools
        // it spawns — the guard is meant for the direct gru do/resume process only.
        .env_remove(crate::labels::GRU_RETRY_PARENT_ENV);
    cmd
}

/// Builds a Claude command to resume an existing session.
///
/// Uses --resume instead of --session-id to avoid "session already in use" errors.
pub(crate) fn build_claude_resume_command(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
    github_host: &str,
) -> TokioCommand {
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
        .current_dir(worktree_path)
        .env("GH_HOST", github_host)
        .env_remove(crate::labels::GRU_RETRY_PARENT_ENV);
    cmd
}
