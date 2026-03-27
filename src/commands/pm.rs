use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::commands::child_process;
use crate::git;

/// Product manager skill content, embedded at compile time.
const PM_SKILL: &str = include_str!("../../.claude/skills/product-manager/SKILL.md");

/// Project manager (TPM) skill content, embedded at compile time.
const TPM_SKILL: &str = include_str!("../../.claude/skills/project-manager/SKILL.md");

/// Handles the `gru pm` command — interactive PM session.
pub async fn handle_pm(prompt: Option<String>, verbose: bool) -> Result<i32> {
    launch_skill_session("pm", PM_SKILL, prompt, verbose).await
}

/// Handles the `gru tpm` command — interactive TPM session.
pub async fn handle_tpm(prompt: Option<String>, verbose: bool) -> Result<i32> {
    launch_skill_session("tpm", TPM_SKILL, prompt, verbose).await
}

/// Launches an interactive Claude session with the given skill as the system prompt.
///
/// When `prompt` is `Some`, passes it as a positional argument to claude so it
/// becomes the first message in the interactive session.
async fn launch_skill_session(
    role_name: &str,
    skill_content: &str,
    prompt: Option<String>,
    verbose: bool,
) -> Result<i32> {
    let repo_root = git::detect_git_repo().await.map_err(|_| {
        anyhow::anyhow!("Not inside a git repository. Run `gru {role_name}` from within a project.")
    })?;

    if verbose {
        eprintln!("Working directory: {}", repo_root.display());
        eprintln!("Starting {role_name} session...");
    }

    // Strip YAML frontmatter from the skill content (delimited by --- lines)
    let system_prompt = strip_frontmatter(skill_content);

    let mut cmd = Command::new("claude");
    cmd.arg("--system-prompt").arg(system_prompt);

    if let Some(ref p) = prompt {
        // Use argument terminator so prompts like "-h" are not treated as CLI flags
        cmd.arg("--").arg(p);
    }

    cmd.current_dir(&repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context(
        "Failed to start claude. Is Claude CLI installed and in your PATH?\n\
         See: https://claude.com/claude-code",
    )?;

    let status = child_process::wait_with_ctrlc_handling(&mut child).await?;

    Ok(if status.success() { 0 } else { 1 })
}

/// Strips YAML frontmatter (delimited by `---`) from the beginning of a string.
///
/// Both the opening and closing `---` must appear on their own lines. Handles
/// both LF (`\n`) and CRLF (`\r\n`) line endings. This prevents horizontal rules
/// or dashes embedded in values from being misinterpreted as frontmatter delimiters.
fn strip_frontmatter(content: &str) -> &str {
    // Opening delimiter must be exactly "---" followed by a newline
    let after_opening = if let Some(rest) = content.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = content.strip_prefix("---\r\n") {
        rest
    } else {
        return content;
    };

    // Find the closing "---" on its own line
    for (pos, _) in after_opening.match_indices("\n---") {
        let rest = &after_opening[pos + 4..]; // skip "\n---"
        if rest.is_empty() || rest.starts_with('\n') {
            return rest.strip_prefix('\n').unwrap_or(rest);
        }
        if let Some(after_crlf) = rest.strip_prefix("\r\n") {
            return after_crlf;
        }
    }
    content // No valid closing delimiter found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pm_skill_embedded() {
        assert!(!PM_SKILL.is_empty());
        assert!(PM_SKILL.contains("product manager"));
    }

    #[test]
    fn test_tpm_skill_embedded() {
        assert!(!TPM_SKILL.is_empty());
        assert!(TPM_SKILL.contains("project management"));
    }

    #[test]
    fn test_strip_frontmatter_with_yaml() {
        let content = "---\nname: test\ntype: skill\n---\n\nActual content here.";
        let result = strip_frontmatter(content);
        assert_eq!(result, "\nActual content here.");
    }

    #[test]
    fn test_strip_frontmatter_without_yaml() {
        let content = "No frontmatter here.\nJust content.";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_strip_frontmatter_closing_requires_own_line() {
        // "--- extra" must NOT be treated as a closing delimiter
        let content = "---\nname: test\n--- extra\n\nContent.";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_strip_frontmatter_closing_with_trailing_content() {
        // "--- extra" must NOT be treated as closing delimiter
        let input = "---\nkey: val\n--- extra\n---\nBody";
        assert_eq!(strip_frontmatter(input), "Body");
    }

    #[test]
    fn test_strip_frontmatter_no_trailing_newline() {
        let content = "---\nname: test\n---";
        let result = strip_frontmatter(content);
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_frontmatter_only_opening() {
        let content = "---\nno closing delimiter\nstill going";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_strip_frontmatter_opening_not_own_line() {
        // "--- something" at the start is NOT valid opening frontmatter
        let content = "--- something\nname: test\n---\nBody";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn test_strip_frontmatter_crlf() {
        let content = "---\r\nname: test\r\n---\r\nBody here.";
        assert_eq!(strip_frontmatter(content), "Body here.");
    }
}
