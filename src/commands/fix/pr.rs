use super::helpers::commits_ahead_of_base;
use super::types::{IssueContext, WorktreeContext};
use crate::agent::{AgentEvent, TimestampedEvent};
use crate::minion_registry::with_registry;
use crate::pr_state::PrState;
use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;

/// Typed errors produced by the PR creation phase.
///
/// Used so the worker can distinguish "already handled with `gru:blocked`"
/// from generic failures that need the `gru:failed` cleanup path.
#[derive(Debug)]
pub(crate) enum PrCreationError {
    /// The agent exited cleanly, produced no commits, and did not push a branch.
    /// The issue has already been labeled `gru:blocked` and a comment posted
    /// with the agent's final message.
    NoCommitsCleanExit,
}

impl std::fmt::Display for PrCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrCreationError::NoCommitsCleanExit => write!(
                f,
                "Agent exited cleanly with no commits and no pushed branch; \
                 issue marked `gru:blocked`"
            ),
        }
    }
}

impl std::error::Error for PrCreationError {}

/// Returns true if the error indicates the PR phase already applied `gru:blocked`.
pub(crate) fn is_no_commits_clean_exit(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<PrCreationError>(),
        Some(PrCreationError::NoCommitsCleanExit)
    )
}

/// Maximum characters of the agent's final message to include in the issue comment.
/// Long messages are truncated by appending `_[message truncated]_` to avoid
/// oversized comments.
const MAX_FINAL_MESSAGE_CHARS: usize = 4000;

/// Extracts the agent's final assistant message from an `events.jsonl` file.
///
/// Streams the file line-by-line, accumulating `TextDelta` text into a buffer
/// that is latched to `last_completed` whenever a `MessageComplete` is
/// encountered. The final non-empty completed message is what the agent said
/// before ending its turn. Returns `None` if the file cannot be opened or no
/// assistant text was produced.
///
/// Uses a `BufReader` so memory stays bounded on long sessions dominated by
/// tool-use events.
pub(super) async fn extract_final_assistant_message(events_path: &Path) -> Option<String> {
    let file = tokio::fs::File::open(events_path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();

    let mut buffer = String::new();
    let mut last_completed: Option<String> = None;

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "Failed to read {}: {}. Returning partial final message.",
                    events_path.display(),
                    e
                );
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let te: TimestampedEvent = match serde_json::from_str(trimmed) {
            Ok(te) => te,
            Err(_) => continue,
        };
        match te.event {
            AgentEvent::TextDelta { text } => buffer.push_str(&text),
            AgentEvent::MessageComplete { .. } => {
                let trimmed_buf = buffer.trim();
                if !trimmed_buf.is_empty() {
                    last_completed = Some(trimmed_buf.to_string());
                }
                buffer.clear();
            }
            _ => {}
        }
    }

    // Recovery path: session ended without any MessageComplete (e.g., agent
    // killed mid-stream). Any accumulated TextDelta is the best available
    // signal of what the agent was trying to say.
    if last_completed.is_none() {
        let trimmed_buf = buffer.trim();
        if !trimmed_buf.is_empty() {
            last_completed = Some(trimmed_buf.to_string());
        }
    }

    last_completed.map(|m| truncate_message(&m, MAX_FINAL_MESSAGE_CHARS))
}

/// Truncates `message` to at most `limit` chars, appending an ellipsis marker
/// when truncation occurs. Operates on characters (not bytes) to avoid slicing
/// inside a multi-byte codepoint.
fn truncate_message(message: &str, limit: usize) -> String {
    if message.chars().count() <= limit {
        return message.to_string();
    }
    let truncated: String = message.chars().take(limit).collect();
    format!("{truncated}\n\n_[message truncated]_")
}

/// Handles the "clean exit, zero commits, no push" outcome described in #850:
/// posts the agent's final message as an issue comment and applies `gru:blocked`
/// so auto-recovery does not re-queue the issue.
async fn handle_no_commits_clean_exit(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Result<()> {
    let events_path = wt_ctx.minion_dir.join("events.jsonl");
    let final_message = extract_final_assistant_message(&events_path).await;

    let Some(issue_num) = issue_ctx.issue_num else {
        log::warn!(
            "⚠️  Minion {} exited cleanly with no commits, but has no issue to label.",
            wt_ctx.minion_id
        );
        return Ok(());
    };

    let message_block = match final_message.as_deref() {
        Some(msg) => {
            // Blockquote every line so long multi-paragraph messages stay
            // visually distinct from the minion's preamble.
            msg.lines()
                .map(|line| {
                    if line.is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        None => "_The agent did not produce a final message. Check the minion logs for details._"
            .to_string(),
    };

    let comment = format!(
        "🤖 Minion `{}` ended its turn without making any commits or pushing a branch.\n\n\
         Marking this issue `gru:blocked` so it is not re-queued automatically. \
         Re-label to `gru:todo` to retry after addressing the minion's feedback below.\n\n\
         ---\n\n\
         **Agent's final message:**\n\n\
         {}",
        wt_ctx.minion_id, message_block
    );

    if let Err(e) = crate::github::post_comment_via_cli(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        issue_num,
        &comment,
    )
    .await
    {
        log::warn!(
            "⚠️  Failed to post no-commits comment on issue #{}: {:#}",
            issue_num,
            e
        );
    }

    // Propagate label failure to the caller so `run_worker` can fall back to
    // the `gru:failed` cleanup path instead of swallowing the label error and
    // returning `NoCommitsCleanExit`. Otherwise a labeling failure would leave
    // the issue stuck at `gru:in-progress` with no follow-up.
    crate::github::mark_issue_blocked_via_cli(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        issue_num,
    )
    .await
    .with_context(|| {
        format!(
            "failed to apply '{}' label to issue #{}",
            crate::labels::BLOCKED,
            issue_num
        )
    })?;

    println!(
        "🏷️  Updated issue #{} label to '{}'",
        issue_num,
        crate::labels::BLOCKED
    );

    Ok(())
}

/// Checks if a branch has been pushed to the remote by querying GitHub's API.
///
/// Uses the `gh`/`ghe` CLI instead of local git tracking refs, because gru
/// worktrees are backed by bare repos whose `origin` remote points to the
/// local bare repo — not to GitHub.
pub(crate) async fn is_branch_pushed(
    owner: &str,
    repo: &str,
    host: &str,
    branch_name: &str,
) -> Result<bool> {
    let repo_full = crate::github::repo_slug(owner, repo);
    let endpoint = format!("repos/{}/git/ref/heads/{}", repo_full, branch_name);
    let output = crate::github::gh_cli_command(host)
        .args(["api", &endpoint, "--silent"])
        .output()
        .await
        .context("Failed to run gh api to check if branch is pushed")?;

    if output.status.success() {
        return Ok(true);
    }

    // 404 means the branch doesn't exist on the remote
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("404") || stderr.contains("Not Found") {
        return Ok(false);
    }

    // Any other failure (auth, network, rate limit) is a real error
    Err(anyhow::anyhow!(
        "gh api failed while checking if branch '{}' is pushed: {}",
        branch_name,
        stderr.trim()
    ))
}

/// Creates a WIP PR title and body template
fn create_wip_template(
    minion_id: &str,
    issue_num: Option<u64>,
    issue_title: &str,
) -> (String, String) {
    let title = match issue_num {
        Some(n) => format!("[{}] Fixes #{}: {}", minion_id, n, issue_title),
        None => format!("[{}] {}", minion_id, issue_title),
    };
    let body = match issue_num {
        Some(n) => format!(
            "## Summary\nAutomated fix for #{} by Minion {}\n\n\
             ## Status\n- [ ] Implementation\n- [ ] Tests\n- [ ] Review\n\n\
             Fixes #{}{}",
            n,
            minion_id,
            n,
            crate::progress_comments::minion_signature(minion_id),
        ),
        None => format!(
            "## Summary\nAutomated work by Minion {}\n\n\
             ## Status\n- [ ] Implementation\n- [ ] Tests\n- [ ] Review{}",
            minion_id,
            crate::progress_comments::minion_signature(minion_id),
        ),
    };
    (title, body)
}

/// Creates a PR for the given issue, returning the PR number
#[allow(clippy::too_many_arguments)]
async fn create_pr_for_issue(
    owner: &str,
    repo: &str,
    host: &str,
    branch_name: &str,
    issue_num: Option<u64>,
    minion_id: &str,
    checkout_path: &Path,
    minion_dir: &Path,
    issue_title_opt: Option<&str>,
) -> Result<String> {
    // Detect base branch
    let base_output = TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()
        .await
        .context("Failed to detect base branch")?;

    let base_branch = if base_output.status.success() {
        let raw = String::from_utf8_lossy(&base_output.stdout);
        raw.trim()
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("main")
            .to_string()
    } else {
        // symbolic-ref failed (common in bare-repo worktrees); query GitHub API
        match crate::github::get_default_branch(host, owner, repo).await {
            Ok(branch) => branch,
            Err(e) => {
                log::warn!(
                    "Could not determine default branch from GitHub API: {}. Falling back to 'main'.",
                    e
                );
                "main".to_string()
            }
        }
    };

    // Get issue title - use provided title if available, otherwise fetch
    let issue_title = if let Some(title) = issue_title_opt {
        title.to_string()
    } else if let Some(num) = issue_num {
        match crate::github::get_issue_via_cli(owner, repo, host, num).await {
            Ok(info) => info.title,
            Err(_) => "Fix issue".to_string(),
        }
    } else {
        "Fix issue".to_string()
    };

    // Check if work is complete (description file exists in minion_dir)
    let description_path = minion_dir.join("PR_DESCRIPTION.md");
    let should_mark_ready = match tokio::fs::try_exists(&description_path).await {
        Ok(exists) => exists,
        Err(e) => {
            log::warn!(
                "⚠️  Warning: Failed to check if PR_DESCRIPTION.md exists: {}",
                e
            );
            false
        }
    };

    let (pr_title, pr_body) = if should_mark_ready {
        // Read the description file
        match tokio::fs::read_to_string(&description_path).await {
            Ok(content) if !content.trim().is_empty() => {
                // Work is complete - use description and mark ready
                let pr_title = match issue_num {
                    Some(n) => format!("Fixes #{}: {}", n, issue_title),
                    None => issue_title.clone(),
                };
                let mut pr_body = content.trim().to_string();
                // Append closing keyword if not already present
                if let Some(n) = issue_num {
                    let closing_line = format!("Fixes #{}", n);
                    if !pr_body.contains(&closing_line) {
                        if !pr_body.ends_with('\n') {
                            pr_body.push('\n');
                        }
                        pr_body.push('\n');
                        pr_body.push_str(&closing_line);
                    }
                }
                if !pr_body.contains("<sub>🤖") {
                    pr_body.push_str(&crate::progress_comments::minion_signature(minion_id));
                }
                (pr_title, pr_body)
            }
            Ok(_) => {
                // File exists but is empty - treat as WIP
                log::warn!("⚠️  Warning: PR_DESCRIPTION.md exists but is empty");
                create_wip_template(minion_id, issue_num, &issue_title)
            }
            Err(e) => {
                // File couldn't be read - treat as WIP
                log::warn!("⚠️  Failed to read PR_DESCRIPTION.md: {}", e);
                create_wip_template(minion_id, issue_num, &issue_title)
            }
        }
    } else {
        // No description file - work in progress
        create_wip_template(minion_id, issue_num, &issue_title)
    };

    // Create the draft PR using gh CLI (with URL validation)
    let pr_number = crate::github::create_draft_pr_via_cli(
        owner,
        repo,
        host,
        branch_name,
        &base_branch,
        &pr_title,
        &pr_body,
    )
    .await
    .context("Failed to create draft PR using gh CLI")?;

    // Mark ready if description was provided
    if should_mark_ready {
        match crate::github::mark_pr_ready_via_cli(owner, repo, host, &pr_number).await {
            Ok(_) => {
                println!("✅ PR #{} marked ready for review", pr_number);
            }
            Err(e) => {
                log::warn!("⚠️  Warning: Failed to mark PR ready: {:#}", e);
                log::warn!(
                    "   PR #{} created as draft - you can mark it ready manually",
                    pr_number
                );
            }
        }

        // Clean up description file
        if let Err(e) = tokio::fs::remove_file(&description_path).await {
            log::warn!("⚠️  Warning: Failed to remove PR_DESCRIPTION.md: {}", e);
            log::warn!("   File will be cleaned up by 'gru clean'");
        }
    }

    Ok(pr_number)
}

/// Saves PR state, updates the minion registry, and marks the issue done.
/// Used by both the normal PR creation path and the "already exists" recovery path.
///
/// Registry updates and label changes are best-effort (warnings on failure).
/// Only PR state file errors are propagated.
async fn finalize_pr(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &str,
) -> Result<()> {
    // Save PR state to minion_dir (metadata)
    let pr_state = PrState::new(
        pr_number.to_string(),
        issue_ctx
            .issue_num
            .map_or("0".to_string(), |n| n.to_string()),
    );
    pr_state
        .save(&wt_ctx.minion_dir)
        .context("Failed to save PR state")?;

    // Update registry with PR number (best-effort — registry parse errors
    // must not kill the worker after the PR was already created successfully)
    let minion_id_clone = wt_ctx.minion_id.clone();
    let pr_number_clone = pr_number.to_string();
    if let Err(e) = with_registry(move |registry| {
        registry.update(&minion_id_clone, |info| {
            info.pr = Some(pr_number_clone);
            info.status = "idle".to_string();
        })
    })
    .await
    {
        log::warn!(
            "⚠️  Failed to update registry with PR number for {}: {:#}",
            wt_ctx.minion_id,
            e
        );
    }

    // Mark issue as done (best-effort: errors logged, not propagated)
    if let Some(issue_num) = issue_ctx.issue_num {
        match crate::github::mark_issue_done_via_cli(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_num,
        )
        .await
        {
            Ok(()) => {
                println!("🏷️  Updated issue label to '{}'", crate::labels::DONE);
            }
            Err(e) => {
                log::warn!("⚠️  Failed to update issue label: {:#}", e);
            }
        }
    }

    Ok(())
}

/// Attempts to recover an existing PR for the given branch.
///
/// Looks up the PR via `gh pr list --head <branch>`, finalizes state if found,
/// and returns the PR number. Used by error-recovery paths in `handle_pr_creation`.
///
/// This function always returns `Ok(_)`: internal lookup errors are logged as
/// warnings and mapped to `Ok(None)`, so callers can use `?` safely.
async fn recover_existing_pr(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Result<Option<String>> {
    match crate::ci::get_pr_number(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        None,
    )
    .await
    {
        Ok(Some(pr_num)) => {
            let pr_number = pr_num.to_string();

            // Best-effort: log warnings instead of propagating errors,
            // since losing the recovered PR number would be worse than
            // missing metadata (which can be recovered on next resume).
            if let Err(e) = finalize_pr(issue_ctx, wt_ctx, &pr_number).await {
                log::warn!("⚠️  Failed to finalize recovered PR state: {:#}", e);
            }

            Ok(Some(pr_number))
        }
        Ok(None) => Ok(None),
        Err(lookup_err) => {
            log::warn!("⚠️  Failed to look up existing PR: {:#}", lookup_err);
            Ok(None)
        }
    }
}

/// Creates a PR for the branch and updates labels/registry.
/// Returns the PR number if successful.
pub(crate) async fn handle_pr_creation(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Result<Option<String>> {
    println!("\n🔍 Checking if branch was pushed...");
    let branch_pushed = is_branch_pushed(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &issue_ctx.host,
        &wt_ctx.branch_name,
    )
    .await?;

    if !branch_pushed {
        println!("ℹ️  Branch was not pushed. No PR will be created.");

        // If the agent produced no commits (#850), surface its final message
        // to the issue and apply `gru:blocked` so auto-recovery does not
        // re-queue the issue endlessly.
        //
        // Invariant: `handle_pr_creation` only runs after the agent phase
        // succeeded — `run_worker` early-returns on non-success before
        // reaching `create_pr_phase`, and `create_pr_phase` only skips the
        // agent phase when a prior run already advanced past `RunningAgent`
        // (which requires a successful agent exit). So reaching this block
        // means the agent exited cleanly and simply produced nothing.
        let commits_ahead = commits_ahead_of_base(
            &wt_ctx.checkout_path,
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
        )
        .await;

        // Only block when the count is *known* to be zero. An unresolved count
        // (e.g., a broken remote setup returning `None`) falls through to the
        // normal "branch not pushed" error so we never falsely block an issue
        // on an unreliable signal.
        if matches!(commits_ahead, Some(0)) {
            handle_no_commits_clean_exit(issue_ctx, wt_ctx).await?;
            return Err(PrCreationError::NoCommitsCleanExit.into());
        }

        println!(
            "   Push your changes with: git push origin {}",
            wt_ctx.branch_name
        );
        return Err(anyhow::anyhow!(
            "Branch '{}' was not pushed — push it and retry with `gru resume`",
            wt_ctx.branch_name
        ));
    }

    // Check if a PR (open, closed, or merged) already exists for this branch
    if let Ok(Some(existing_pr)) = crate::ci::get_pr_number(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        Some("all"),
    )
    .await
    {
        let pr_number = existing_pr.to_string();
        println!(
            "ℹ️  PR #{} already exists for branch '{}', skipping creation.",
            pr_number, wt_ctx.branch_name
        );

        if let Err(e) = finalize_pr(issue_ctx, wt_ctx, &pr_number).await {
            log::warn!("⚠️  Failed to finalize existing PR state: {:#}", e);
        }

        return Ok(Some(pr_number));
    }

    println!("📋 Branch was pushed, creating pull request...");

    let issue_title_cached = issue_ctx.details.as_ref().map(|d| d.title.as_str());

    match create_pr_for_issue(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &issue_ctx.host,
        &wt_ctx.branch_name,
        issue_ctx.issue_num,
        &wt_ctx.minion_id,
        &wt_ctx.checkout_path,
        &wt_ctx.minion_dir,
        issue_title_cached,
    )
    .await
    {
        Ok(pr_number) => {
            // Best-effort: the PR already exists on GitHub, so losing
            // local metadata is recoverable on next resume.
            if let Err(e) = finalize_pr(issue_ctx, wt_ctx, &pr_number).await {
                log::warn!("⚠️  Failed to finalize new PR state: {}", e);
            }

            println!("✅ Draft PR created: #{}", pr_number);
            println!(
                "🔗 View PR at: https://{}/{}/{}/pull/{}",
                issue_ctx.host, issue_ctx.owner, issue_ctx.repo, pr_number
            );

            Ok(Some(pr_number))
        }
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("already exists") || err_msg.contains("A pull request for branch") {
                log::info!(
                    "ℹ️  A PR already exists for branch '{}', recovering PR number...",
                    wt_ctx.branch_name
                );
                match recover_existing_pr(issue_ctx, wt_ctx).await? {
                    Some(pr_number) => {
                        println!("✅ Recovered existing PR #{}", pr_number);
                        Ok(Some(pr_number))
                    }
                    None => Err(e.context(format!(
                        "PR exists for branch '{}' but `gh pr list --head` returned no results. \
                         This may be a transient GitHub API issue or auth problem; retry with 'gru resume'.",
                        wt_ctx.branch_name
                    ))),
                }
            } else if err_msg.contains("branch not found") || err_msg.contains("does not exist") {
                log::warn!("⚠️  Branch was pushed but is no longer available.");
                log::warn!("   It may have been deleted or force-pushed.");
                log::warn!(
                    "   You can create the PR manually at: https://{}/{}/{}/compare/{}",
                    issue_ctx.host,
                    issue_ctx.owner,
                    issue_ctx.repo,
                    wt_ctx.branch_name
                );
                Err(anyhow::anyhow!(
                    "Branch '{}' was pushed but is no longer available (deleted or force-pushed)",
                    wt_ctx.branch_name
                ))
            } else {
                log::warn!("⚠️  Failed to create PR: {:#}", e);

                // Fallback: a PR may already exist from a previous attempt or
                // manual creation.  Try to recover it the same way the
                // "already exists" path does.
                let manual_link = format!(
                    "https://{}/{}/{}/compare/{}",
                    issue_ctx.host, issue_ctx.owner, issue_ctx.repo, wt_ctx.branch_name
                );
                match recover_existing_pr(issue_ctx, wt_ctx).await? {
                    Some(pr_number) => {
                        println!(
                            "✅ Recovered existing PR #{} after creation failure",
                            pr_number
                        );
                        Ok(Some(pr_number))
                    }
                    None => Err(e.context(format!(
                        "PR creation failed and no existing PR found for branch '{}'. \
                         You can create the PR manually at: {}",
                        wt_ctx.branch_name, manual_link
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_is_branch_pushed_nonexistent() {
        // Test with a nonexistent branch — gh api should return 404 → Ok(false)
        let result = is_branch_pushed(
            "fotoetienne",
            "gru",
            "github.com",
            "nonexistent-branch-xyz-12345",
        )
        .await;

        // gh api returns 404 for nonexistent branches, which we map to Ok(false)
        // Skip assertion if gh CLI is not available (e.g., CI without auth)
        match result {
            Ok(pushed) => assert!(!pushed),
            Err(e) => {
                let msg = e.to_string();
                // Acceptable failures: gh not installed, not authenticated
                assert!(
                    msg.contains("gh api failed") || msg.contains("Failed to run gh api"),
                    "Unexpected error: {}",
                    msg
                );
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_pr_number_state_all_nonexistent_branch() {
        // A branch that has never had a PR should return Ok(None)
        let result = crate::ci::get_pr_number(
            "github.com",
            "fotoetienne",
            "gru",
            "nonexistent-branch-xyz-12345",
            Some("all"),
        )
        .await;

        match result {
            Ok(pr) => assert!(pr.is_none(), "Expected no PR for nonexistent branch"),
            Err(e) => {
                let msg = e.to_string();
                // Acceptable: gh not installed or not authenticated
                assert!(
                    msg.contains("Failed to list") || msg.contains("gh pr list"),
                    "Unexpected error: {}",
                    msg
                );
            }
        }
    }

    #[test]
    fn test_create_wip_template() {
        let (title, body) = create_wip_template("M042", Some(123), "Fix login bug");
        assert_eq!(title, "[M042] Fixes #123: Fix login bug");
        assert!(body.contains("Automated fix for #123 by Minion M042"));
        assert!(body.contains("- [ ] Implementation"));
        assert!(body.contains("Fixes #123"));
        assert!(body.contains("<sub>🤖 M042</sub>"));
    }

    /// Verifies that `finalize_pr` returns Ok even when the registry update
    /// fails. Regression test for issue #699: registry errors in finalize_pr
    /// should be non-fatal so the worker continues to the monitoring phase.
    ///
    /// Note: `with_registry` uses `spawn_blocking` so the thread-local
    /// `set_test_workspace` override is not visible. The registry update fails
    /// because the minion ID doesn't exist in whatever registry is loaded.
    /// The companion test `test_registry_parse_error_is_non_fatal` below
    /// exercises the actual parse-error path directly.
    #[tokio::test]
    async fn test_finalize_pr_survives_registry_failure() {
        use super::super::types::{IssueContext, WorktreeContext};
        use uuid::Uuid;

        let tmp = tempfile::tempdir().unwrap();
        let minion_dir = tmp.path().to_path_buf();
        let checkout_path = minion_dir.join("checkout");
        std::fs::create_dir_all(&checkout_path).unwrap();

        let issue_ctx = IssueContext {
            owner: "test-owner".to_string(),
            repo: "test-repo".to_string(),
            host: "github.com".to_string(),
            issue_num: Some(99999),
            details: None,
        };

        let wt_ctx = WorktreeContext {
            minion_id: "M_NONEXISTENT".to_string(),
            branch_name: "minion/issue-99999-M_NONEXISTENT".to_string(),
            minion_dir,
            checkout_path,
            session_id: Uuid::new_v4(),
        };

        // finalize_pr should return Ok even though:
        // - The minion doesn't exist in the registry (registry update fails)
        // - The gh CLI call to update labels will fail (best-effort)
        let result = finalize_pr(&issue_ctx, &wt_ctx, "12345").await;
        assert!(
            result.is_ok(),
            "finalize_pr should not propagate registry errors: {:?}",
            result.err()
        );
    }

    /// Directly exercises the registry parse-error path from issue #699.
    /// Writes an intentionally-invalid `minions.json` to a temp workspace
    /// and verifies that `MinionRegistry::load` fails with a parse error,
    /// proving that the `if let Err` guard in `finalize_pr` would catch it.
    #[test]
    fn test_is_no_commits_clean_exit_detects_typed_error() {
        let err: anyhow::Error = PrCreationError::NoCommitsCleanExit.into();
        assert!(is_no_commits_clean_exit(&err));
    }

    #[test]
    fn test_is_no_commits_clean_exit_wrapped_in_context() {
        let err: anyhow::Error = PrCreationError::NoCommitsCleanExit.into();
        let wrapped = err.context("PR creation failed");
        assert!(is_no_commits_clean_exit(&wrapped));
    }

    #[test]
    fn test_is_no_commits_clean_exit_rejects_other_errors() {
        let err = anyhow::anyhow!("Branch 'feature' was not pushed — push it and retry");
        assert!(!is_no_commits_clean_exit(&err));
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_single_turn() {
        use crate::agent::{AgentEvent, TimestampedEventRef};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let events = [
            AgentEvent::Started { usage: None },
            AgentEvent::TextDelta {
                text: "Recommendation: run ".to_string(),
            },
            AgentEvent::TextDelta {
                text: "/decompose 2620.".to_string(),
            },
            AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            },
        ];
        let ts = "2026-04-21T12:00:00Z".to_string();
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(&TimestampedEventRef { ts: &ts, event: e }).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();

        let msg = extract_final_assistant_message(&path)
            .await
            .expect("message present");
        assert_eq!(msg, "Recommendation: run /decompose 2620.");
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_prefers_last_turn() {
        // Simulate a multi-turn session where an earlier turn used a tool
        // and the final turn produced the recommendation text.
        use crate::agent::{AgentEvent, TimestampedEventRef};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let events = [
            AgentEvent::TextDelta {
                text: "Let me look at the code.".to_string(),
            },
            AgentEvent::MessageComplete {
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            },
            AgentEvent::ToolUse {
                tool_name: "Read".to_string(),
                tool_use_id: "t1".to_string(),
                input_summary: None,
            },
            AgentEvent::TextDelta {
                text: "This issue is too large.".to_string(),
            },
            AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            },
        ];
        let ts = "2026-04-21T12:00:00Z".to_string();
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(&TimestampedEventRef { ts: &ts, event: e }).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();

        let msg = extract_final_assistant_message(&path)
            .await
            .expect("message present");
        assert_eq!(msg, "This issue is too large.");
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.jsonl");
        assert!(extract_final_assistant_message(&path).await.is_none());
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(extract_final_assistant_message(&path).await.is_none());
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_no_text_events() {
        // A session that only ran tools and never produced assistant text.
        use crate::agent::{AgentEvent, TimestampedEventRef};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let events = [
            AgentEvent::ToolUse {
                tool_name: "Bash".to_string(),
                tool_use_id: "t1".to_string(),
                input_summary: None,
            },
            AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            },
        ];
        let ts = "2026-04-21T12:00:00Z".to_string();
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(&TimestampedEventRef { ts: &ts, event: e }).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();

        assert!(extract_final_assistant_message(&path).await.is_none());
    }

    #[tokio::test]
    async fn test_extract_final_assistant_message_skips_malformed_lines() {
        // A corrupt line in the middle should not prevent extraction.
        use crate::agent::{AgentEvent, TimestampedEventRef};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let ts = "2026-04-21T12:00:00Z".to_string();
        let good_1 = serde_json::to_string(&TimestampedEventRef {
            ts: &ts,
            event: &AgentEvent::TextDelta {
                text: "hello ".to_string(),
            },
        })
        .unwrap();
        let good_2 = serde_json::to_string(&TimestampedEventRef {
            ts: &ts,
            event: &AgentEvent::TextDelta {
                text: "world".to_string(),
            },
        })
        .unwrap();
        let good_3 = serde_json::to_string(&TimestampedEventRef {
            ts: &ts,
            event: &AgentEvent::MessageComplete {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            },
        })
        .unwrap();
        let contents = format!("{good_1}\n{{not valid json}}\n{good_2}\n{good_3}\n");
        std::fs::write(&path, contents).unwrap();

        let msg = extract_final_assistant_message(&path)
            .await
            .expect("message present");
        assert_eq!(msg, "hello world");
    }

    #[test]
    fn test_truncate_message_below_limit() {
        assert_eq!(truncate_message("short", 100), "short");
    }

    #[test]
    fn test_truncate_message_above_limit() {
        let long = "a".repeat(500);
        let truncated = truncate_message(&long, 100);
        let expected = format!("{}\n\n_[message truncated]_", "a".repeat(100));
        assert_eq!(truncated, expected);
    }

    #[test]
    fn test_truncate_message_multibyte_safe() {
        // Ensure the truncator operates on chars, not bytes, so it never
        // panics in the middle of a multi-byte codepoint.
        let s = "漢字".repeat(100);
        let truncated = truncate_message(&s, 50);
        assert!(truncated.ends_with("_[message truncated]_"));
    }

    #[test]
    fn test_registry_parse_error_is_non_fatal() {
        use crate::minion_registry::MinionRegistry;

        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // Write a corrupt minions.json — simulates the schema divergence
        // that caused the M14g incident (e.g., null where u64 was expected)
        std::fs::write(
            state_dir.join("minions.json"),
            b"{ this is not valid json }",
        )
        .unwrap();

        // MinionRegistry::load should fail with a parse error
        let result = MinionRegistry::load(Some(&state_dir));
        assert!(
            result.is_err(),
            "Expected parse error from corrupt minions.json"
        );
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("parse")
                || err_msg.contains("deserialize")
                || err_msg.contains("expected"),
            "Error should be a parse/deserialize error, got: {}",
            err_msg
        );
    }
}
