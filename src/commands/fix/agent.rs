use super::helpers::try_post_progress_comment;
use super::types::{AgentResult, IssueContext, WorktreeContext};
use crate::agent::{AgentBackend, AgentEvent};
use crate::agent_runner::{run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED};
use crate::minion_registry::{with_registry, MinionMode, MinionRegistry};
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::progress_comments::{MinionPhase, ProgressCommentTracker};
use crate::prompt_loader;
use crate::prompt_renderer::{render_template, PromptContext};
use anyhow::{Context, Result};
use tokio::process::Command as TokioCommand;
use uuid::Uuid;

/// Filename for extra context appended via `--discuss`.
/// Written by `handle_fix` on user input, read by `run_agent_session` at launch.
pub(super) const EXTRA_CONTEXT_FILENAME: &str = "extra_context.txt";

/// Builds the prompt string from issue context using the prompt template system.
///
/// Loads the "do" prompt template (built-in or overridden via `.gru/prompts/do.md`
/// or legacy `.gru/prompts/fix.md`), builds a `PromptContext` from the issue
/// details, and renders the template.
/// Falls back to `/do <issue_num>` when issue details are unavailable.
pub(super) fn build_fix_prompt(ctx: &IssueContext, wt_ctx: &WorktreeContext) -> String {
    let Some(ref details) = ctx.details else {
        return format!(
            "/do {}",
            ctx.issue_num.map_or("?".to_string(), |n| n.to_string())
        );
    };

    // Try to load the prompt through the template system (allows overrides).
    // Use the worktree path as the repo root so `.gru/prompts/do.md` is found.
    let prompt_template = match prompt_loader::resolve_prompt("do", Some(&wt_ctx.checkout_path)) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Failed to load do prompt: {e}, using /do fallback");
            None
        }
    };

    let template_content = match prompt_template {
        Some(ref p) => &p.content,
        None => {
            log::warn!("No 'do' prompt found (built-in or override), using /do fallback");
            return format!(
                "/do {}",
                ctx.issue_num.map_or("?".to_string(), |n| n.to_string())
            );
        }
    };

    // Build the context for rendering
    let labels_value = if details.labels.is_empty() {
        String::new()
    } else {
        format!("Labels: {}", details.labels.join(", "))
    };

    let mut prompt_ctx = PromptContext::new();
    prompt_ctx.issue_number = ctx.issue_num;
    prompt_ctx.issue_title = Some(details.title.clone());
    prompt_ctx.issue_body = Some(details.body.clone());
    prompt_ctx.repo_owner = Some(ctx.owner.clone());
    prompt_ctx.repo_name = Some(ctx.repo.clone());
    prompt_ctx.worktree_path = Some(wt_ctx.checkout_path.clone());
    prompt_ctx.minion_dir = Some(wt_ctx.minion_dir.clone());
    prompt_ctx.branch_name = Some(wt_ctx.branch_name.clone());
    prompt_ctx.minion_id = Some(wt_ctx.minion_id.clone());

    let mut variables = prompt_ctx.to_variables();
    // Add the labels variable (fix-specific, not in the standard PromptContext).
    // Value is "Labels: x, y" when present or empty string when none.
    // The template places {{ labels }} on its own line to handle both cases.
    variables.insert("labels".to_string(), labels_value);

    render_template(template_content, &variables)
}

/// Builds the full prompt including any extra context from `--discuss`.
///
/// Calls `build_fix_prompt` for the base prompt, then appends the contents
/// of `extra_context.txt` in the minion directory if present and non-empty.
pub(super) fn build_full_prompt(issue_ctx: &IssueContext, wt_ctx: &WorktreeContext) -> String {
    let mut prompt = build_fix_prompt(issue_ctx, wt_ctx);

    let extra_path = wt_ctx.minion_dir.join(EXTRA_CONTEXT_FILENAME);
    match std::fs::read_to_string(&extra_path) {
        Ok(extra) => {
            if !extra.trim().is_empty() {
                prompt.push_str("\n\n## Additional Context from User\n\n");
                prompt.push_str(extra.trim());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Normal case: no extra context file (--discuss was not used)
        }
        Err(e) => {
            log::warn!(
                "⚠️  Failed to read extra context from {}: {}",
                extra_path.display(),
                e
            );
        }
    }

    prompt
}

/// Runs an agent session with stream monitoring and progress tracking.
///
/// Spawns the agent CLI, tracks progress, records PID in registry,
/// and cleans up on exit (success or failure).
pub(super) async fn run_agent_session(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    println!("🤖 Launching {}...\n", backend.name());
    let prompt = build_full_prompt(issue_ctx, wt_ctx);

    let mut cmd = backend.build_command(
        &wt_ctx.checkout_path,
        &wt_ctx.session_id,
        &prompt,
        &issue_ctx.host,
    );
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);
    run_agent_session_inner(backend, issue_ctx, wt_ctx, cmd, quiet, timeout_opt).await
}

/// Runs a resumed agent session, continuing from a previous interrupted session.
///
/// Uses the backend's resume command to continue the existing conversation.
/// If `resume_prompt` is provided, it is used instead of the default continuation prompt.
pub(super) async fn resume_agent_session(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
    resume_prompt: Option<&str>,
) -> Result<AgentResult> {
    println!("🔄 Resuming {} session...\n", backend.name());
    let prompt = resume_prompt
        .map(|p| p.to_string())
        .unwrap_or_else(|| {
            format!(
                "Continue working on issue #{}. Pick up where you left off. \
                 If you've already completed the implementation, proceed to push and write PR_DESCRIPTION.md.",
                issue_ctx.issue_num.map_or("?".to_string(), |n| n.to_string())
            )
        });
    let mut cmd = backend
        .build_resume_command(
            &wt_ctx.checkout_path,
            &wt_ctx.session_id,
            &prompt,
            &issue_ctx.host,
        )
        .context("Agent backend does not support resume")?;
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);
    run_agent_session_inner(backend, issue_ctx, wt_ctx, cmd, quiet, timeout_opt).await
}

/// Shared implementation for running an agent session (new or resumed).
///
/// Handles stream monitoring, progress tracking, PID registration, and cleanup.
async fn run_agent_session_inner(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    cmd: TokioCommand,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    let config = ProgressConfig {
        minion_id: wt_ctx.minion_id.clone(),
        issue: issue_ctx
            .issue_num
            .map_or("?".to_string(), |n| n.to_string()),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let mut progress_tracker = ProgressCommentTracker::new(wt_ctx.minion_id.clone());

    struct CallbackState<'a> {
        progress: &'a ProgressDisplay,
        progress_tracker: &'a mut ProgressCommentTracker,
    }

    let callback_state = CallbackState {
        progress: &progress,
        progress_tracker: &mut progress_tracker,
    };

    let callback = |event: &AgentEvent| {
        // Detect phase transitions from structured tool-use events rather than
        // raw text, avoiding false positives on code snippets or file names.
        let previous_phase = callback_state.progress_tracker.current_phase();
        if let AgentEvent::ToolUse {
            ref tool_name,
            ref input_summary,
            ..
        } = event
        {
            match tool_name.as_str() {
                // Edit/Write tools signal the implementing phase.
                // "file_change" is the Codex backend equivalent of Edit/Write.
                "Edit" | "Write" | "NotebookEdit" | "file_change"
                    if previous_phase != MinionPhase::Implementing
                        && previous_phase != MinionPhase::Testing =>
                {
                    callback_state
                        .progress_tracker
                        .set_phase(MinionPhase::Implementing);
                }
                // Bash tool with test-related commands signals the testing phase.
                // "command" is the Codex backend equivalent of Bash.
                // Only transition from Implementing (not Planning) to keep phases sequential.
                "Bash" | "command" if previous_phase == MinionPhase::Implementing => {
                    if let Some(ref summary) = input_summary {
                        if is_test_command(summary) {
                            callback_state
                                .progress_tracker
                                .set_phase(MinionPhase::Testing);
                        }
                    }
                }
                _ => {}
            }
        }

        callback_state.progress.handle_event(event);
    };

    let on_spawn =
        MinionRegistry::pid_callback(wt_ctx.minion_id.clone(), Some(MinionMode::Autonomous));

    let run_result = run_agent_with_stream_monitoring(
        cmd,
        backend,
        &wt_ctx.minion_dir,
        timeout_opt,
        Some(callback),
        Some(on_spawn),
    )
    .await;

    // Best-effort cleanup: clear PID, set mode to Stopped, and save token usage.
    // Token usage is persisted regardless of exit status (Ok with non-zero exit) because
    // cost data is valuable even for failed tasks. Only stream-level errors (timeout, stuck)
    // result in Err, in which case partial usage is not saved.
    let token_usage = run_result.as_ref().ok().map(|r| r.token_usage.clone());
    let exit_minion_id = wt_ctx.minion_id.clone();
    let _ = with_registry(move |registry| {
        registry.update(&exit_minion_id, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
            if let Some(usage) = token_usage {
                info.token_usage = Some(usage);
            }
        })
    })
    .await;

    let agent_run = run_result?;

    // Post final completion comment
    {
        progress_tracker.set_phase(MinionPhase::Completed);

        let final_message = if agent_run.status.success() {
            if agent_run.token_usage.total_tokens() > 0 {
                format!(
                    "✅ Task completed successfully! (tokens: {})",
                    agent_run.token_usage.display_compact()
                )
            } else {
                "✅ Task completed successfully!".to_string()
            }
        } else {
            "❌ Task failed.".to_string()
        };

        let update = progress_tracker.create_update(final_message);
        let comment_body = update.format_comment();

        if let Some(issue_num) = issue_ctx.issue_num {
            try_post_progress_comment(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
                issue_num,
                &comment_body,
            )
            .await;
        }
    }

    // Finish the progress display
    if agent_run.status.success() {
        progress.finish_with_message(&format!(
            "✅ Completed issue {}",
            issue_ctx
                .issue_num
                .map_or("?".to_string(), |n| n.to_string())
        ));
    } else {
        progress.finish_with_message(&format!(
            "❌ Failed to fix issue {}",
            issue_ctx
                .issue_num
                .map_or("?".to_string(), |n| n.to_string())
        ));
    }

    Ok(AgentResult {
        status: agent_run.status,
    })
}

/// Checks whether a Bash tool input summary looks like a test command.
///
/// Matches common test runners and invocations (e.g., `cargo test`, `just test`,
/// `npm test`, `pytest`). The summary may start with "Run: " as produced by
/// the agent backends; this prefix is optional and will be stripped when present.
fn is_test_command(summary: &str) -> bool {
    // Strip the optional "Run: " prefix that agent backends prepend to Bash summaries
    let cmd = summary.strip_prefix("Run: ").unwrap_or(summary);
    let cmd_lower = cmd.to_lowercase();

    // Common test runner patterns (non-exhaustive; add new runners as needed)
    let test_patterns = [
        "cargo test",
        "cargo nextest",
        "just test",
        "just check", // runs fmt+lint+test+build; close enough to "testing"
        "npm test",
        "npm run test",
        "npx jest",
        "npx vitest",
        "yarn test",
        "pnpm test",
        "pytest",
        "python -m pytest",
        "python -m unittest",
        "go test",
        "make test",
        "gradle test",
        "./gradlew test",
        "mvn test",
        "bundle exec rspec",
        "rspec",
    ];

    test_patterns
        .iter()
        .any(|pattern| cmd_lower.starts_with(pattern))
}

/// Invokes the agent to address review comments using the same session
pub(super) async fn invoke_agent_for_reviews(
    backend: &dyn AgentBackend,
    checkout_path: &std::path::Path,
    minion_dir: &std::path::Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
    github_host: &str,
) -> Result<()> {
    let cmd = backend
        .build_resume_command(checkout_path, session_id, prompt, github_host)
        .context("Agent backend does not support resume")?;

    let result = run_agent_with_stream_monitoring(
        cmd,
        backend,
        minion_dir,
        timeout_opt,
        None::<fn(&AgentEvent)>,
        None::<Box<dyn FnOnce(u32) + Send>>,
    )
    .await?;

    if !result.status.success() {
        let exit_code = result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        anyhow::bail!("Review response process exited with code {}", exit_code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_test_command_cargo_test() {
        assert!(is_test_command("Run: cargo test"));
        assert!(is_test_command("Run: cargo test --lib"));
        assert!(is_test_command("Run: cargo test -- --nocapture"));
    }

    #[test]
    fn test_is_test_command_just_test() {
        assert!(is_test_command("Run: just test"));
        assert!(is_test_command("Run: just test-verbose"));
        assert!(is_test_command("Run: just check"));
    }

    #[test]
    fn test_is_test_command_npm() {
        assert!(is_test_command("Run: npm test"));
        assert!(is_test_command("Run: npm run test"));
        assert!(is_test_command("Run: npx jest"));
        assert!(is_test_command("Run: npx vitest"));
    }

    #[test]
    fn test_is_test_command_python() {
        assert!(is_test_command("Run: pytest"));
        assert!(is_test_command("Run: pytest -v tests/"));
        assert!(is_test_command("Run: python -m pytest"));
        assert!(is_test_command("Run: python -m unittest"));
    }

    #[test]
    fn test_is_test_command_other_runners() {
        assert!(is_test_command("Run: go test ./..."));
        assert!(is_test_command("Run: make test"));
        assert!(is_test_command("Run: gradle test"));
        assert!(is_test_command("Run: ./gradlew test"));
        assert!(is_test_command("Run: mvn test"));
        assert!(is_test_command("Run: bundle exec rspec"));
        assert!(is_test_command("Run: rspec spec/"));
    }

    #[test]
    fn test_is_test_command_without_run_prefix() {
        // Should work without the "Run: " prefix
        assert!(is_test_command("cargo test"));
        assert!(is_test_command("pytest -v"));
    }

    #[test]
    fn test_is_test_command_false_positives() {
        // These should NOT be detected as test commands
        assert!(!is_test_command("Run: cargo build"));
        assert!(!is_test_command("Run: git status"));
        assert!(!is_test_command("Run: echo test"));
        assert!(!is_test_command("Run: cat test_helper.rs"));
        assert!(!is_test_command("Run: ls tests/"));
        assert!(!is_test_command("Run: cargo fmt"));
        assert!(!is_test_command("Run: cargo clippy"));
        assert!(!is_test_command("Run: npm install"));
        assert!(!is_test_command("Run: pip install pytest"));
    }

    #[test]
    fn test_is_test_command_case_insensitive() {
        assert!(is_test_command("Run: Cargo Test"));
        assert!(is_test_command("Run: CARGO TEST"));
        assert!(is_test_command("Run: PYTEST"));
    }

    #[test]
    fn test_is_test_command_nextest() {
        assert!(is_test_command("Run: cargo nextest run"));
        assert!(is_test_command("Run: cargo nextest run --lib"));
    }

    #[test]
    fn test_is_test_command_empty_and_no_match() {
        assert!(!is_test_command(""));
        assert!(!is_test_command("Run: "));
    }
}
