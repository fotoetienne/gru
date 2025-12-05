use crate::url_utils::validate_pr_format;
use anyhow::{Context, Result};
use tokio::process::Command;

/// Handles the review command by delegating to the Claude CLI
/// Returns the exit code from the claude process
pub async fn handle_review(pr: &str) -> Result<i32> {
    // Validate the PR format before proceeding
    validate_pr_format(pr)?;

    // Execute the claude CLI with the /pr_review command
    let status = Command::new("claude")
        .arg(format!("/pr_review {}", pr))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code",
        )?;

    // Return the exit code from the claude process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(128))
}
