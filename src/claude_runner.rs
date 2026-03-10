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
pub fn build_claude_command(worktree_path: &Path, session_id: &Uuid, prompt: &str) -> TokioCommand {
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

/// Builds a Claude command to resume an existing session.
///
/// Uses --resume instead of --session-id to avoid "session already in use" errors.
pub fn build_claude_resume_command(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
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
        .current_dir(worktree_path);
    cmd
}
