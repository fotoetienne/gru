use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

use crate::commands::child_process;
use crate::git;
use crate::tmux::TmuxGuard;

/// Maximum bytes to read from CLAUDE.md. We read slightly more than the
/// truncation limit so we can detect whether truncation is needed and still
/// land on a valid UTF-8 char boundary.
const CLAUDE_MD_READ_LIMIT: usize = 8192;

/// Handles the `gru chat` command.
///
/// Spawns an interactive agent session with project context.
/// When run inside a git repo, includes project context (CLAUDE.md, Gru tool descriptions).
/// When run outside a repo, spawns a general Gru onboarding assistant.
pub(crate) async fn handle_chat(
    repo_flag: Option<String>,
    agent_name: &str,
    verbose: bool,
) -> Result<i32> {
    let _tmux_guard = TmuxGuard::new("gru:chat");

    let backend = crate::agent_registry::resolve_backend(agent_name)?;

    let (work_dir, system_prompt, github_host) = match detect_project_context(repo_flag).await {
        Some((repo_root, owner, repo_name, host)) => {
            let prompt = build_in_repo_prompt(&repo_root, &owner, &repo_name).await;
            (repo_root, prompt, host)
        }
        None => {
            let cwd = std::env::current_dir().context("Failed to determine current directory")?;
            let prompt = build_no_repo_prompt(backend.name());
            // No repo and no owner, so there's no signal to resolve a host
            // from. Leave GH_HOST alone: a GHES-only user may have it
            // exported in their shell, and guessing github.com would
            // silently retarget their `gh` calls.
            (cwd, prompt, None)
        }
    };

    if verbose {
        eprintln!("Working directory: {}", work_dir.display());
    }

    let mut cmd = backend
        .build_interactive_command(&work_dir, &system_prompt, None, github_host.as_deref())
        .ok_or_else(|| crate::agent_registry::interactive_unsupported_error("chat", agent_name))?;

    let mut child = cmd.spawn().with_context(|| {
        crate::agent::spawn_error_context(backend.as_ref(), &cmd, "for gru chat")
    })?;

    let status = child_process::wait_with_ctrlc_handling(&mut child).await?;

    Ok(if status.success() { 0 } else { 1 })
}

/// Detects project context: repo root, owner, repo name, and GitHub host.
///
/// The host is resolved from the repo's git remote so `gh` calls the agent
/// makes during the session hit the right GitHub Enterprise instance rather
/// than defaulting to github.com. It is `None` when nothing resolved it, so
/// the caller leaves an inherited `GH_HOST` in place.
///
/// Returns None if not in a git repo or can't determine GitHub remote.
async fn detect_project_context(
    repo_flag: Option<String>,
) -> Option<(PathBuf, String, String, Option<String>)> {
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
                // --repo carries no host, so it has to be inferred — but the
                // flag names a repo that may live on a different instance
                // than the checkout we happen to be standing in. A configured
                // host for the requested owner therefore wins, and the current
                // repo's remote is only a valid signal when it belongs to that
                // same owner (one owner lives on one instance). Both can come
                // up empty, in which case GH_HOST is left untouched.
                let host = host_for_repo_flag(
                    &repo_root,
                    owner,
                    crate::github::configured_host_for_owner(owner, None),
                    super::resume::inherited_gh_host(),
                )
                .await;
                return Some((repo_root, owner.to_string(), name.to_string(), host));
            }
            _ => {
                log::warn!(
                    "Ignoring malformed --repo '{}': expected non-empty 'owner/repo' format",
                    repo
                );
            }
        }
    }

    // Try to detect from current directory.
    let repo_root = git::detect_git_repo().await.ok()?;
    context_from_repo_root(repo_root).await
}

/// Host for a `--repo owner/repo` run: configured host, then the checkout's
/// remotes if they belong to that owner, then an inherited `GH_HOST`.
///
/// Takes the configured and inherited hosts as parameters so it can be tested
/// without the developer's own config or `GH_HOST` leaking in. Only the dead
/// end — nothing configured, no matching remote, nothing inherited — warrants
/// the unrecognised-host warning: an inherited host is what the child session
/// would have used anyway, so reporting it as a configuration gap is noise.
async fn host_for_repo_flag(
    repo_root: &Path,
    owner: &str,
    configured: Option<String>,
    inherited: Option<String>,
) -> Option<String> {
    if let Some(host) = configured {
        return Some(host);
    }
    let (host, unknown) = host_from_matching_remote(repo_root, owner).await;
    let host = super::resume::host_fallback(host, inherited);
    if host.is_none() {
        git::warn_unknown_remotes(&unknown);
    }
    host
}

/// Owner, repo, and host for a checkout, from its remotes.
///
/// Reads the inherited `GH_HOST` for the warning decision; see
/// [`context_from_remotes`].
async fn context_from_repo_root(
    repo_root: PathBuf,
) -> Option<(PathBuf, String, String, Option<String>)> {
    context_from_remotes(repo_root, super::resume::inherited_gh_host()).await
}

/// Owner, repo, and host for a checkout, from its remotes.
async fn context_from_remotes(
    repo_root: PathBuf,
    inherited: Option<String>,
) -> Option<(PathBuf, String, String, Option<String>)> {
    let host_registry = crate::config::load_host_registry();
    let (candidates, unknown) =
        git::github_repo_candidates_from_remotes(&repo_root, &host_registry).await;
    if let Some(resolved) = candidates.into_iter().next() {
        return Some((
            repo_root,
            resolved.owner,
            resolved.repo,
            Some(resolved.host),
        ));
    }

    // Nothing resolved to a host, but a repo-shaped remote on an unconfigured
    // GHES still names the project. Keep that context — the owner/repo is all
    // the in-repo prompt needs — rather than dropping the user into the no-repo
    // onboarding prompt. An inherited `GH_HOST` settles the routing (the child
    // would have inherited it regardless), so only its absence is a genuine
    // configuration gap worth warning about.
    let host = super::resume::host_fallback(None, inherited);
    if host.is_none() {
        git::warn_unknown_remotes(&unknown);
    }
    let fallback = unknown.into_iter().next()?;
    Some((repo_root, fallback.owner, fallback.repo, host))
}

/// Host from the current checkout's remotes, but only if they point at `owner`.
///
/// Guards the `--repo owner/repo` path: running `gru chat --repo corp/project`
/// from inside a github.com checkout must not resolve `GH_HOST=github.com`.
///
/// Returns the owner's unrecognised-host remotes too; the caller warns about
/// them only after its own fallbacks have come up empty.
async fn host_from_matching_remote(
    repo_root: &Path,
    owner: &str,
) -> (Option<String>, Vec<git::UnknownRemote>) {
    let host_registry = crate::config::load_host_registry();
    git::resolve_github_host_for_owner(repo_root, &host_registry, owner).await
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
///
/// Takes the backend's name so the onboarding explanation describes the agent
/// the user actually chose: `gru chat --agent pi` spawns Pi sessions, and
/// telling that user their Minions are Claude Code sessions is simply wrong.
fn build_no_repo_prompt(agent_name: &str) -> String {
    format!(
    "You are a Gru assistant. The user is not currently in a project directory.\n\
     \n\
     Help them get started with Gru:\n\
     - Explain what Gru does (autonomous coding agents for GitHub issues)\n\
     - Help them initialize a repo: `gru init <owner/repo>` or `gru init .` in an existing checkout\n\
     - Help them configure Gru: `~/.gru/config.toml`\n\
     - Walk them through their first task: `gru do <issue#>`\n\
     \n\
     Key concepts:\n\
     - Gru spawns \"Minions\" — autonomous {agent_name} sessions that work on GitHub issues\n\
     - Each Minion works in an isolated git worktree\n\
     - Minions claim issues, implement fixes, create PRs, and respond to reviews\n\
     - GitHub labels drive the workflow: `gru:todo` → `gru:in-progress` → `gru:done`\n\
     \n\
     Be friendly and helpful. This may be their first time using Gru."
    )
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

    /// Git repo with the given `(name, url)` remotes, for host resolution tests.
    fn repo_with_remotes(remotes: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: Vec<&str>| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                // Keep an inherited GIT_DIR (e.g. running under a git hook)
                // from redirecting these commands at the real repo.
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .expect("git")
        };
        run(vec!["init", "--quiet"]);
        for (name, url) in remotes {
            run(vec!["remote", "add", name, url]);
        }
        dir
    }

    #[tokio::test]
    async fn test_host_from_matching_remote_ignores_other_owners_repo() {
        // Regression: `gru chat --repo corp/project` from inside a github.com
        // checkout must not resolve that checkout's host for `corp`.
        let dir = repo_with_remotes(&[("origin", "https://github.com/someone/other.git")]);
        assert_eq!(host_from_matching_remote(dir.path(), "corp").await.0, None);
    }

    #[tokio::test]
    async fn test_host_from_matching_remote_scans_all_remotes() {
        // The globally ranked winner (`origin`) belongs to another owner, but
        // `upstream` points at the requested owner's instance and must win.
        let dir = repo_with_remotes(&[
            ("origin", "https://github.com/someone/other.git"),
            (
                "upstream",
                "https://github.corp.example.com/corp/project.git",
            ),
        ]);
        assert_eq!(
            host_from_matching_remote(dir.path(), "corp").await.0,
            Some("github.corp.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn test_host_from_matching_remote_uses_same_owners_remote() {
        // Same owner means the same instance, so the remote's host applies
        // even when --repo names a different repo under it.
        let dir =
            repo_with_remotes(&[("origin", "https://github.corp.example.com/corp/other.git")]);
        assert_eq!(
            host_from_matching_remote(dir.path(), "corp").await.0,
            Some("github.corp.example.com".to_string())
        );
        // Owner comparison is case-insensitive, as GitHub owners are.
        assert_eq!(
            host_from_matching_remote(dir.path(), "Corp").await.0,
            Some("github.corp.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn test_host_from_matching_remote_without_remotes() {
        let dir = repo_with_remotes(&[]);
        assert_eq!(host_from_matching_remote(dir.path(), "corp").await.0, None);
    }

    #[test]
    fn test_build_no_repo_prompt_contains_key_info() {
        let prompt = build_no_repo_prompt("claude-code");
        assert!(prompt.contains("Gru assistant"));
        assert!(prompt.contains("gru init"));
        assert!(prompt.contains("gru do"));
        assert!(prompt.contains("config.toml"));
    }

    #[test]
    fn test_build_no_repo_prompt_names_selected_agent() {
        // The onboarding prompt explains what a Minion is, so it has to name
        // the agent the user picked rather than assuming Claude Code.
        let prompt = build_no_repo_prompt("pi");
        assert!(prompt.contains("autonomous pi sessions"));
        assert!(!prompt.contains("Claude Code"));
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
    async fn test_context_from_repo_root_keeps_unconfigured_ghes_context() {
        // Regression: an unconfigured GHES remote used to resolve no host and
        // therefore no context at all, dropping `gru chat` into the no-repo
        // onboarding prompt. The project is still named; only the host is
        // unknown, so GH_HOST stays inherited.
        let dir =
            repo_with_remotes(&[("origin", "https://code.corp.example.com/acme/widgets.git")]);
        let (_, owner, repo, host) = context_from_remotes(dir.path().to_path_buf(), None)
            .await
            .expect("an unconfigured GHES remote should still name the project");
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
        assert_eq!(host, None);
    }

    #[tokio::test]
    async fn test_context_from_repo_root_resolves_known_host() {
        let dir = repo_with_remotes(&[("origin", "https://github.com/acme/widgets.git")]);
        let (_, owner, repo, host) = context_from_remotes(dir.path().to_path_buf(), None)
            .await
            .expect("a github.com remote should resolve");
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
        assert_eq!(host, Some("github.com".to_string()));
    }

    #[tokio::test]
    async fn test_context_from_remotes_keeps_inherited_host() {
        // An unconfigured GHES remote with GH_HOST already exported: the child
        // would inherit that host anyway, so it is reported as the resolved
        // host rather than warned about as a configuration gap.
        let dir =
            repo_with_remotes(&[("origin", "https://code.corp.example.com/acme/widgets.git")]);
        let (_, owner, repo, host) = context_from_remotes(
            dir.path().to_path_buf(),
            Some("code.corp.example.com".to_string()),
        )
        .await
        .expect("an unconfigured GHES remote should still name the project");
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
        assert_eq!(host, Some("code.corp.example.com".to_string()));
    }

    #[tokio::test]
    async fn test_host_for_repo_flag_uses_inherited_host() {
        // --repo names an owner with no configured host and no matching
        // remote; the inherited GH_HOST is what the session would use, so it
        // wins over leaving the host unresolved.
        let dir = repo_with_remotes(&[("origin", "https://github.com/someone/other.git")]);
        assert_eq!(
            host_for_repo_flag(
                dir.path(),
                "corp",
                None,
                Some("ghe.example.com".to_string())
            )
            .await,
            Some("ghe.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn test_host_for_repo_flag_prefers_matching_remote_over_inherited() {
        // A remote belonging to the requested owner is a stronger signal than
        // whatever the shell happens to export.
        let dir =
            repo_with_remotes(&[("origin", "https://github.corp.example.com/corp/other.git")]);
        assert_eq!(
            host_for_repo_flag(
                dir.path(),
                "corp",
                None,
                Some("ghe.example.com".to_string())
            )
            .await,
            Some("github.corp.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn test_host_for_repo_flag_without_any_source() {
        let dir = repo_with_remotes(&[("origin", "https://github.com/someone/other.git")]);
        assert_eq!(
            host_for_repo_flag(dir.path(), "corp", None, None).await,
            None
        );
        // A blank GH_HOST is not a host.
        assert_eq!(
            host_for_repo_flag(dir.path(), "corp", None, Some("  ".to_string())).await,
            None
        );
    }

    #[tokio::test]
    async fn test_context_from_repo_root_without_remotes() {
        let dir = repo_with_remotes(&[]);
        assert!(context_from_remotes(dir.path().to_path_buf(), None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_detect_project_context_with_repo_flag() {
        // When run inside a git repo, --repo should resolve the git root
        // and override the owner/name.
        let result = detect_project_context(Some("myowner/myrepo".to_string())).await;
        let (_, owner, repo, host) = result.expect("--repo flag should produce context");
        assert_eq!(owner, "myowner");
        assert_eq!(repo, "myrepo");
        // Host resolution isn't asserted here: it legitimately depends on the
        // ambient config and `GH_HOST`, which this test can't control. The
        // `host_for_repo_flag` tests cover it with both supplied explicitly.
        let _ = host;
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
