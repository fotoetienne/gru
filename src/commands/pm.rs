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
pub async fn handle_pm(verbose: bool) -> Result<i32> {
    launch_skill_session("product manager", PM_SKILL, verbose).await
}

/// Handles the `gru tpm` command — interactive TPM session.
pub async fn handle_tpm(verbose: bool) -> Result<i32> {
    launch_skill_session("technical project manager", TPM_SKILL, verbose).await
}

/// Launches an interactive Claude session with the given skill as the system prompt.
///
/// Requires being inside a git repo — returns an error otherwise.
async fn launch_skill_session(role_name: &str, skill_content: &str, verbose: bool) -> Result<i32> {
    let repo_root = git::detect_git_repo()
        .await
        .context("Not inside a git repository. Run this command from within a project.")?;

    if verbose {
        eprintln!("Working directory: {}", repo_root.display());
        eprintln!("Starting {role_name} session...");
    }

    // Strip YAML frontmatter from the skill content (delimited by --- lines)
    let system_prompt = strip_frontmatter(skill_content);

    let mut cmd = Command::new("claude");
    cmd.arg("--system-prompt").arg(system_prompt);
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

/// Strips YAML frontmatter (delimited by `---` lines) from skill content.
fn strip_frontmatter(content: &str) -> &str {
    // Frontmatter must start at the very beginning with "---"
    if !content.starts_with("---") {
        return content;
    }
    // Find the closing "---" on its own line (after the first line)
    let rest = &content[3..];
    let closing = rest
        .find("\n---\n")
        .map(|i| (i, 5)) // skip "\n---\n"
        .or_else(|| {
            // Handle "---" as the very last line (no trailing newline)
            if rest.ends_with("\n---") {
                Some((rest.len() - 4, rest.len() - rest.rfind('\n').unwrap_or(0)))
            } else {
                None
            }
        });
    if let Some((end, skip)) = closing {
        let after_frontmatter = 3 + end + skip;
        if after_frontmatter < content.len() {
            content[after_frontmatter..].trim_start_matches('\n')
        } else {
            // Frontmatter only, no content after it
            ""
        }
    } else {
        content
    }
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
        assert_eq!(result, "Actual content here.");
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
}
