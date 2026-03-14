use super::helpers::try_post_progress_comment;
use super::types::{
    AgentResult, IssueContext, WorktreeContext, MAX_OUTPUT_BUFFER_SIZE, TRIM_OUTPUT_BUFFER_SIZE,
};
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

/// Builds the prompt string from issue context using the prompt template system.
///
/// Loads the "do" prompt template (built-in or overridden via `.gru/prompts/do.md`
/// or legacy `.gru/prompts/fix.md`), builds a `PromptContext` from the issue
/// details, and renders the template.
/// Falls back to `/do <issue_num>` when issue details are unavailable.
pub(super) fn build_fix_prompt(ctx: &IssueContext, wt_ctx: &WorktreeContext) -> String {
    let Some(ref details) = ctx.details else {
        return format!("/do {}", ctx.issue_num);
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
            return format!("/do {}", ctx.issue_num);
        }
    };

    // Build the context for rendering
    let labels_value = if details.labels.is_empty() {
        String::new()
    } else {
        format!("Labels: {}", details.labels)
    };

    let mut prompt_ctx = PromptContext::new();
    prompt_ctx.issue_number = Some(ctx.issue_num);
    prompt_ctx.issue_title = Some(details.title.clone());
    prompt_ctx.issue_body = Some(details.body.clone());
    prompt_ctx.repo_owner = Some(ctx.owner.clone());
    prompt_ctx.repo_name = Some(ctx.repo.clone());
    prompt_ctx.worktree_path = Some(wt_ctx.checkout_path.clone());
    prompt_ctx.minion_dir = Some(wt_ctx.minion_dir.clone());
    prompt_ctx.branch_name = Some(wt_ctx.branch_name.clone());

    let mut variables = prompt_ctx.to_variables();
    // Add the labels variable (fix-specific, not in the standard PromptContext).
    // Value is "Labels: x, y" when present or empty string when none.
    // The template places {{ labels }} on its own line to handle both cases.
    variables.insert("labels".to_string(), labels_value);

    render_template(template_content, &variables)
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
    let prompt = build_fix_prompt(issue_ctx, wt_ctx);
    let mut cmd = backend.build_command(&wt_ctx.checkout_path, &wt_ctx.session_id, &prompt);
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);
    run_agent_session_inner(backend, issue_ctx, wt_ctx, cmd, quiet, timeout_opt).await
}

/// Runs a resumed agent session, continuing from a previous interrupted session.
///
/// Uses the backend's resume command to continue the existing conversation.
pub(super) async fn resume_agent_session(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<AgentResult> {
    println!("🔄 Resuming {} session...\n", backend.name());
    let prompt = format!(
        "Continue working on issue #{}. Pick up where you left off. \
         If you've already completed the implementation, proceed to push and write PR_DESCRIPTION.md.",
        issue_ctx.issue_num
    );
    let mut cmd = backend
        .build_resume_command(&wt_ctx.checkout_path, &wt_ctx.session_id, &prompt)
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
        issue: issue_ctx.issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let mut progress_tracker = ProgressCommentTracker::new(wt_ctx.minion_id.clone());

    struct CallbackState<'a> {
        text_output_buffer: String,
        progress: &'a ProgressDisplay,
        progress_tracker: &'a mut ProgressCommentTracker,
    }

    let mut callback_state = CallbackState {
        text_output_buffer: String::new(),
        progress: &progress,
        progress_tracker: &mut progress_tracker,
    };

    let callback = |event: &AgentEvent| {
        // Accumulate text output for phase detection
        if let AgentEvent::TextDelta { ref text } = event {
            callback_state.text_output_buffer.push_str(text);
            if callback_state.text_output_buffer.len() > MAX_OUTPUT_BUFFER_SIZE {
                let mut trim_pos = TRIM_OUTPUT_BUFFER_SIZE;
                while trim_pos > 0 && !callback_state.text_output_buffer.is_char_boundary(trim_pos)
                {
                    trim_pos -= 1;
                }
                callback_state.text_output_buffer =
                    callback_state.text_output_buffer.split_off(trim_pos);
            }

            // Detect phase transitions from a rolling window of the accumulated
            // buffer (last 200 chars). Using the buffer instead of just the current
            // chunk ensures we catch markers split across multiple TextDelta events.
            let buf = &callback_state.text_output_buffer;
            let window_start = buf.len().saturating_sub(200);
            // Align to a char boundary
            let window_start = (window_start..buf.len())
                .find(|&i| buf.is_char_boundary(i))
                .unwrap_or(buf.len());
            let window = &buf[window_start..];

            let previous_phase = callback_state.progress_tracker.current_phase();
            if window.contains("Plan") || window.contains("plan") {
                if previous_phase != MinionPhase::Planning {
                    callback_state
                        .progress_tracker
                        .set_phase(MinionPhase::Planning);
                }
            } else if (window.contains("Implement") || window.contains("Writing"))
                && previous_phase != MinionPhase::Implementing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Implementing);
            } else if (window.contains("test") || window.contains("Test"))
                && previous_phase != MinionPhase::Testing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Testing);
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
            info.pid = None;
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

        try_post_progress_comment(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_ctx.issue_num,
            &comment_body,
        )
        .await;
    }

    // Finish the progress display
    if agent_run.status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue_ctx.issue_num));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue_ctx.issue_num));
    }

    Ok(AgentResult {
        status: agent_run.status,
    })
}

/// Invokes the agent to address review comments using the same session
pub(super) async fn invoke_agent_for_reviews(
    backend: &dyn AgentBackend,
    checkout_path: &std::path::Path,
    minion_dir: &std::path::Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
) -> Result<()> {
    let cmd = backend
        .build_resume_command(checkout_path, session_id, prompt)
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
