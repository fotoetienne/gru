use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::Duration;

use crate::git;

/// Grace period for the child process to exit after receiving a signal.
const CTRL_C_GRACE_SECS: u64 = 5;

/// Handles the `gru chat` command.
///
/// Spawns an interactive Claude session with project context.
/// When run inside a git repo, includes project context (CLAUDE.md, Gru tool descriptions).
/// When run outside a repo, spawns a general Gru onboarding assistant.
pub async fn handle_chat(repo_flag: Option<String>, verbose: bool) -> Result<i32> {
    let (work_dir, system_prompt) = match detect_project_context(repo_flag).await {
        Some((repo_root, owner, repo_name)) => {
            let prompt = build_in_repo_prompt(&repo_root, &owner, &repo_name).await;
            (repo_root, prompt)
        }
        None => {
            let cwd = std::env::current_dir().context("Failed to determine current directory")?;
            let prompt = build_no_repo_prompt();
            (cwd, prompt)
        }
    };

    if verbose {
        eprintln!("Working directory: {}", work_dir.display());
    }

    // Build claude command for interactive mode (no --print, no --output-format)
    let mut cmd = Command::new("claude");
    cmd.arg("--system-prompt").arg(&system_prompt);
    cmd.current_dir(&work_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context(
        "Failed to start claude. Is Claude CLI installed and in your PATH?\n\
         See: https://claude.com/claude-code",
    )?;

    // Wait for claude to exit, handling Ctrl-C gracefully.
    let status = tokio::select! {
        result = child.wait() => result.context("Failed to wait for claude process")?,
        _ = tokio::signal::ctrl_c() => {
            signal_child(&mut child);
            match tokio::time::timeout(
                Duration::from_secs(CTRL_C_GRACE_SECS),
                child.wait(),
            ).await {
                Ok(result) => result.context("Failed to wait for claude after interrupt")?,
                Err(_) => {
                    log::warn!("Claude process did not exit within {}s, force-killing", CTRL_C_GRACE_SECS);
                    let _ = child.kill().await;
                    child.wait().await.context("Failed to reap claude after force-kill")?
                }
            }
        }
    };

    Ok(if status.success() { 0 } else { 1 })
}

/// Detects project context: repo root, owner, and repo name.
/// Returns None if not in a git repo or can't determine GitHub remote.
async fn detect_project_context(repo_flag: Option<String>) -> Option<(PathBuf, String, String)> {
    // If --repo flag provided as owner/repo, use CWD with that context
    if let Some(repo) = repo_flag {
        if let Some((owner, name)) = repo.split_once('/') {
            let cwd = std::env::current_dir().ok()?;
            return Some((cwd, owner.to_string(), name.to_string()));
        }
    }

    // Try to detect from current directory
    let repo_root = git::detect_git_repo().await.ok()?;
    let remote_url = git::get_github_remote().await.ok()?;
    let (owner, repo_name) = git::parse_github_remote(&remote_url).ok()?;
    Some((repo_root, owner, repo_name))
}

/// Builds the system prompt for in-repo context.
async fn build_in_repo_prompt(repo_root: &Path, owner: &str, repo_name: &str) -> String {
    let claude_md = load_claude_md(repo_root).await;

    let mut prompt = format!(
        "You are a project assistant for {owner}/{repo_name}.\n\
         \n\
         You have access to these tools for managing the project:\n\
         - `gru status` — list active Minions and their state\n\
         - `gru do <issue#>` — spawn a Minion to work on an issue autonomously\n\
         - `gru clean` — clean up merged/closed worktrees\n\
         - `gru review <pr#>` — review a pull request\n\
         - `gh issue list` — list open issues\n\
         - `gh pr list` — list open PRs\n\
         - `gh issue view <number>` — view issue details\n\
         - `gh pr view <number>` — view PR details\n\
         - `gh issue create` — create a new issue\n\
         \n\
         When the user asks what to work on, use `gh issue list` and `gru status` to \
         find open issues that aren't already being worked on.\n\
         \n\
         When the user asks to start work on an issue, suggest using `gru do <issue#>` \
         to spawn an autonomous Minion."
    );

    if let Some(claude_md_content) = claude_md {
        prompt.push_str("\n\nProject context (from CLAUDE.md):\n");
        // Truncate very large CLAUDE.md to avoid exceeding prompt limits
        let max_len = 8000;
        if claude_md_content.len() > max_len {
            prompt.push_str(&claude_md_content[..max_len]);
            prompt.push_str("\n\n[CLAUDE.md truncated — read the full file for more details]");
        } else {
            prompt.push_str(&claude_md_content);
        }
    }

    prompt
}

/// Builds the system prompt for when no repo is detected.
fn build_no_repo_prompt() -> String {
    "You are a Gru assistant. The user is not currently in a project directory.\n\
     \n\
     Help them get started with Gru:\n\
     - Explain what Gru does (autonomous coding agents for GitHub issues)\n\
     - Help them initialize a repo: `gru init <owner/repo>` or `gru init .` in an existing checkout\n\
     - Help them configure Gru: `~/.gru/config.toml`\n\
     - Walk them through their first task: `gru do <issue#>`\n\
     \n\
     Key concepts:\n\
     - Gru spawns \"Minions\" — autonomous Claude Code sessions that work on GitHub issues\n\
     - Each Minion works in an isolated git worktree\n\
     - Minions claim issues, implement fixes, create PRs, and respond to reviews\n\
     - GitHub labels drive the workflow: `ready-for-minion` → `in-progress` → `minion:done`\n\
     \n\
     Be friendly and helpful. This may be their first time using Gru."
        .to_string()
}

/// Loads CLAUDE.md from the repo root, if it exists.
async fn load_claude_md(repo_root: &Path) -> Option<String> {
    let claude_md_path = repo_root.join("CLAUDE.md");
    tokio::fs::read_to_string(&claude_md_path).await.ok()
}

/// Send a termination signal to the child process.
fn signal_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_no_repo_prompt_contains_key_info() {
        let prompt = build_no_repo_prompt();
        assert!(prompt.contains("Gru assistant"));
        assert!(prompt.contains("gru init"));
        assert!(prompt.contains("gru do"));
        assert!(prompt.contains("config.toml"));
    }

    #[tokio::test]
    async fn test_build_in_repo_prompt_contains_tools() {
        let tmp = std::env::temp_dir().join("gru-chat-test");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        let prompt = build_in_repo_prompt(&tmp, "testowner", "testrepo").await;
        assert!(prompt.contains("testowner/testrepo"));
        assert!(prompt.contains("gru status"));
        assert!(prompt.contains("gru do"));
        assert!(prompt.contains("gh issue list"));
        assert!(prompt.contains("gh pr list"));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn test_build_in_repo_prompt_includes_claude_md() {
        let tmp = std::env::temp_dir().join("gru-chat-test-claudemd");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        let claude_md = tmp.join("CLAUDE.md");
        tokio::fs::write(&claude_md, "# Test Project\nThis is a test.")
            .await
            .unwrap();

        let prompt = build_in_repo_prompt(&tmp, "owner", "repo").await;
        assert!(prompt.contains("# Test Project"));
        assert!(prompt.contains("This is a test."));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn test_build_in_repo_prompt_truncates_large_claude_md() {
        let tmp = std::env::temp_dir().join("gru-chat-test-truncate");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        let claude_md = tmp.join("CLAUDE.md");
        let large_content = "x".repeat(10000);
        tokio::fs::write(&claude_md, &large_content).await.unwrap();

        let prompt = build_in_repo_prompt(&tmp, "owner", "repo").await;
        assert!(prompt.contains("[CLAUDE.md truncated"));
        // Should not contain the full 10000 chars
        assert!(prompt.len() < 10000 + 500); // prompt overhead + truncated content

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn test_load_claude_md_missing_file() {
        let tmp = std::env::temp_dir().join("gru-chat-test-no-claude-md");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        let result = load_claude_md(&tmp).await;
        assert!(result.is_none());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn test_detect_project_context_no_repo() {
        // When run in /tmp (not a git repo), should return None
        let result = detect_project_context(None).await;
        // This may or may not be None depending on whether /tmp is in a git repo
        // Just verify it doesn't panic
        let _ = result;
    }
}
