use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::commands::child_process;
use crate::git;
use crate::tmux::TmuxGuard;

/// Maximum bytes to read from CLAUDE.md. We read slightly more than the
/// truncation limit so we can detect whether truncation is needed and still
/// land on a valid UTF-8 char boundary.
const CLAUDE_MD_READ_LIMIT: usize = 8192;

/// Handles the `gru chat` command.
///
/// Spawns an interactive Claude session with project context.
/// When run inside a git repo, includes project context (CLAUDE.md, Gru tool descriptions).
/// When run outside a repo, spawns a general Gru onboarding assistant.
pub(crate) async fn handle_chat(repo_flag: Option<String>, verbose: bool) -> Result<i32> {
    let _tmux_guard = TmuxGuard::new("gru:chat");

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

    let status = child_process::wait_with_ctrlc_handling(&mut child).await?;

    Ok(if status.success() { 0 } else { 1 })
}

/// Detects project context: repo root, owner, and repo name.
/// Returns None if not in a git repo or can't determine GitHub remote.
async fn detect_project_context(repo_flag: Option<String>) -> Option<(PathBuf, String, String)> {
    // If --repo flag provided as owner/repo, override owner/name but still
    // resolve the repo root from the current git repository when possible.
    if let Some(repo) = repo_flag {
        match repo.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() => {
                // Prefer the actual git repo root; fall back to CWD if not in a git repo.
                let repo_root = match git::detect_git_repo().await {
                    Ok(root) => root,
                    Err(_) => std::env::current_dir().ok()?,
                };
                return Some((repo_root, owner.to_string(), name.to_string()));
            }
            _ => {
                log::warn!(
                    "Ignoring malformed --repo '{}': expected non-empty 'owner/repo' format",
                    repo
                );
            }
        }
    }

    // Try to detect from current directory
    let repo_root = git::detect_git_repo().await.ok()?;
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let remote_url = git::get_github_remote(&github_hosts).await.ok()?;
    let (_host, owner, repo_name) = git::parse_github_remote(&remote_url, &github_hosts).ok()?;
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

    if let Some((claude_md_content, was_truncated)) = claude_md {
        prompt.push_str("\n\nProject context (from CLAUDE.md):\n");
        prompt.push_str(&claude_md_content);
        if was_truncated {
            prompt.push_str("\n\n[CLAUDE.md truncated — read the full file for more details]");
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
     - GitHub labels drive the workflow: `gru:todo` → `gru:in-progress` → `gru:done`\n\
     \n\
     Be friendly and helpful. This may be their first time using Gru."
        .to_string()
}

/// Loads up to `CLAUDE_MD_READ_LIMIT` bytes of CLAUDE.md from the repo root.
///
/// Returns `(content, was_truncated)`. Only reads the bytes we actually need
/// so that very large CLAUDE.md files don't consume excess memory.
async fn load_claude_md(repo_root: &Path) -> Option<(String, bool)> {
    let claude_md_path = repo_root.join("CLAUDE.md");
    let mut file = tokio::fs::File::open(&claude_md_path).await.ok()?;

    let mut buf = vec![0u8; CLAUDE_MD_READ_LIMIT + 4]; // +4 for UTF-8 boundary detection
    let mut total = 0;
    loop {
        let n = file.read(&mut buf[total..]).await.ok()?;
        if n == 0 {
            break;
        }
        total += n;
        if total >= buf.len() {
            break;
        }
    }
    buf.truncate(total);

    let was_truncated = total > CLAUDE_MD_READ_LIMIT;
    if was_truncated {
        // Trim to CLAUDE_MD_READ_LIMIT on a valid char boundary.
        let mut boundary = CLAUDE_MD_READ_LIMIT;
        while boundary > 0 && !is_utf8_char_boundary(buf[boundary]) {
            boundary -= 1;
        }
        buf.truncate(boundary);
    }

    let content = String::from_utf8(buf).ok()?;
    Some((content, was_truncated))
}

/// Returns true if the byte is the start of a UTF-8 character (or ASCII).
fn is_utf8_char_boundary(b: u8) -> bool {
    // In UTF-8, continuation bytes have the pattern 10xxxxxx (0x80..0xBF).
    // Everything else is a char boundary.
    (b as i8) >= -0x40 // equivalent to: b < 0x80 || b >= 0xC0
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
        // Use multi-byte UTF-8 characters (emoji) to verify char-boundary-safe truncation
        let large_content = "🦀".repeat(3000); // 3000 × 4 bytes = 12000 bytes
        tokio::fs::write(&claude_md, &large_content).await.unwrap();

        let prompt = build_in_repo_prompt(&tmp, "owner", "repo").await;
        assert!(prompt.contains("[CLAUDE.md truncated"));
        // The prompt should contain at most ~8KB of CLAUDE.md content plus the base
        // prompt text (~600 bytes) and truncation notice
        assert!(prompt.len() < CLAUDE_MD_READ_LIMIT + 1000);

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
    async fn test_load_claude_md_returns_truncation_flag() {
        let tmp = std::env::temp_dir().join("gru-chat-test-trunc-flag");
        let _ = tokio::fs::create_dir_all(&tmp).await;

        // Small file: not truncated
        let claude_md = tmp.join("CLAUDE.md");
        tokio::fs::write(&claude_md, "small content").await.unwrap();
        let (_, was_truncated) = load_claude_md(&tmp).await.unwrap();
        assert!(!was_truncated);

        // Large file: truncated
        let large = "x".repeat(CLAUDE_MD_READ_LIMIT + 100);
        tokio::fs::write(&claude_md, &large).await.unwrap();
        let (content, was_truncated) = load_claude_md(&tmp).await.unwrap();
        assert!(was_truncated);
        assert!(content.len() <= CLAUDE_MD_READ_LIMIT);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn test_detect_project_context_with_repo_flag() {
        // When run inside a git repo, --repo should resolve the git root
        // and override the owner/name.
        let result = detect_project_context(Some("myowner/myrepo".to_string())).await;
        let (_, owner, repo) = result.expect("--repo flag should produce context");
        assert_eq!(owner, "myowner");
        assert_eq!(repo, "myrepo");
    }

    #[tokio::test]
    async fn test_detect_project_context_rejects_empty_segments() {
        // "owner/" and "/repo" should be treated as malformed
        let _ = detect_project_context(Some("owner/".to_string())).await;
        let _ = detect_project_context(Some("/repo".to_string())).await;
        // No panic — just falls through to git detection
    }

    #[tokio::test]
    async fn test_detect_project_context_invalid_repo_flag() {
        // No slash at all should fall through to git detection
        let _ = detect_project_context(Some("noslash".to_string())).await;
    }

    #[test]
    fn test_is_utf8_char_boundary() {
        // ASCII byte is always a boundary
        assert!(is_utf8_char_boundary(b'A'));
        assert!(is_utf8_char_boundary(b'\0'));
        // Multi-byte start bytes are boundaries
        assert!(is_utf8_char_boundary(0xC0)); // 2-byte start
        assert!(is_utf8_char_boundary(0xE0)); // 3-byte start
        assert!(is_utf8_char_boundary(0xF0)); // 4-byte start
                                              // Continuation bytes are NOT boundaries
        assert!(!is_utf8_char_boundary(0x80));
        assert!(!is_utf8_char_boundary(0xBF));
    }
}
