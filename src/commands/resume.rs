use crate::agent::AgentBackend;
use crate::agent_registry;
use crate::agent_runner::{
    is_stuck_or_timeout_error, EXIT_ALREADY_RUNNING, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::commands::fix::{
    agent_exit_code, create_pr_phase, fetch_issue_details, monitor_pr_phase, run_agent_phase,
    update_orchestration_phase, IssueContext, WorktreeContext,
};
use crate::minion_lock::MinionLock;
use crate::minion_registry::{
    mark_minion_failed, revert_to_stopped, with_registry, MinionMode, OrchestrationPhase,
};
use crate::minion_resolver;
use crate::session_claim::{self, SessionClaimError};
use crate::tmux::TmuxGuard;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use tokio::time::Duration;
use uuid::Uuid;

/// Context produced by `check_resumption_preconditions`, consumed by `run_resume_pipeline`.
struct ResumeContext {
    wt_ctx: WorktreeContext,
    issue_ctx: IssueContext,
    backend: Box<dyn AgentBackend>,
    resume_prompt: Option<String>,
    start_phase: OrchestrationPhase,
    effective_timeout: Option<String>,
    no_watch: bool,
    /// The command that originally created this minion (e.g., "do", "prompt", "review").
    command: String,
    /// Advisory lock held for the lifetime of the resume pipeline. Prevents
    /// concurrent `gru resume` / `gru attach` from spawning a second agent
    /// against the same minion (issue #865). Released on drop.
    _minion_lock: MinionLock,
}

/// Handles the resume command: resumes a stopped Minion in autonomous mode.
///
/// Unlike `gru attach` (which runs interactively), `gru resume` runs in
/// autonomous mode with stream-json monitoring and auto-PR creation — the
/// same execution model as `gru do`.
///
/// Flow:
/// 1. Resolve the minion ID and load registry info
/// 2. Check that the minion is stopped (error if already running)
/// 3. Spawn Claude with `--resume` in stream-json mode
/// 4. Monitor output with progress display and timeout detection
/// 5. Auto-create PR if branch was pushed
/// 6. Update registry on exit
pub(crate) async fn handle_resume(
    id: String,
    additional_prompt: Option<String>,
    timeout_opt: Option<String>,
    quiet: bool,
) -> Result<i32> {
    let ctx = match check_resumption_preconditions(id, additional_prompt, timeout_opt).await {
        Ok(ctx) => ctx,
        Err(e) if e.downcast_ref::<SessionClaimError>().is_some() => {
            // `gru do` returns EXIT_ALREADY_RUNNING on the same condition so
            // lab can short-circuit retries; keep resume's contract aligned.
            eprintln!("{:#}", e);
            return Ok(EXIT_ALREADY_RUNNING);
        }
        Err(e) => return Err(e),
    };
    run_resume_pipeline(ctx, quiet).await
}

/// Validates resumption preconditions: resolves the minion, claims the session,
/// checks attempt count and timeout, builds the prompt, and resolves the backend.
///
/// Returns a `ResumeContext` ready for `run_resume_pipeline`.
async fn check_resumption_preconditions(
    id: String,
    additional_prompt: Option<String>,
    timeout_opt: Option<String>,
) -> Result<ResumeContext> {
    // Resolve the minion ID (same smart resolution as gru path/attach)
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Verify minion directory still exists
    if !minion.worktree_path.exists() {
        bail!(
            "Minion directory no longer exists: {}\n\
             The worktree may have been removed. Try 'gru status' to see active minions.",
            minion.worktree_path.display()
        );
    }

    // Acquire the per-minion advisory lock before touching the registry so
    // that if another process already owns this minion we bail out without
    // mutating any shared state. Held for the lifetime of the resume
    // pipeline; released on drop (issue #865).
    //
    // On contention the error is `SessionClaimError::LockContention`;
    // `handle_resume` downcasts any `SessionClaimError` variant (lock or
    // registry) to exit with `EXIT_ALREADY_RUNNING`, keeping the retry
    // contract uniform across both barriers.
    let minion_lock = MinionLock::try_acquire(&minion.minion_id)?;

    // Atomically check registry state and claim as Autonomous with our own
    // PID in a single file-locked write. Passing `claim_pid` here closes the
    // TOCTOU window where a concurrent claimer would otherwise observe
    // `mode=Autonomous, pid=None` and reset + claim the minion, producing two
    // live resumes against the same session (issues #862 and #864).
    let parent_pid = std::process::id();
    let parent_start_time = crate::minion_registry::get_process_start_time(parent_pid);
    let registry_info = session_claim::check_and_claim_session(
        &minion.minion_id,
        MinionMode::Autonomous,
        Some((parent_pid, parent_start_time)),
        false, // not graceful: resume requires registry
    )
    .await?;

    let info = match registry_info {
        Some(info) => info,
        None => {
            bail!(
                "Minion {} is not in the registry. Cannot resume without session context.\n\
                 Use 'gru attach {}' for interactive mode instead.",
                minion.minion_id,
                minion.minion_id
            );
        }
    };

    // Parse owner/repo from "owner/repo" format
    let (owner, repo_name) = info
        .repo
        .split_once('/')
        .map(|(o, r)| (o.to_string(), r.to_string()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid repo format in registry: '{}' (expected 'owner/repo')",
                info.repo
            )
        })?;

    let session_id = info.session_id;
    let issue_num = info.issue;
    let branch_name = info.branch;
    let agent_name = info.agent_name;
    let command = info.command;
    let timeout_deadline: Option<DateTime<Utc>> = info.timeout_deadline;
    let no_watch = info.no_watch;
    let start_phase = info.orchestration_phase.clone();
    let wake_reason = info.wake_reason.clone();

    // Check if timeout_deadline has passed — fail instead of resuming
    if let Some(deadline) = timeout_deadline {
        if Utc::now() >= deadline {
            mark_minion_failed(&minion.minion_id).await;
            bail!(
                "Minion {} has passed its timeout deadline ({}). Marking as failed.",
                minion.minion_id,
                deadline
            );
        }
    }

    // Increment attempt_count for observability.  We do NOT enforce
    // max_resume_attempts here — that limit is enforced by `gru lab`'s
    // `should_resume_candidate()` for daemon-driven retries.  User-initiated
    // paths (`gru resume`, `gru attach` auto-resume) should not be blocked
    // by max_resume_attempts since the human made an explicit decision.
    let mid = minion.minion_id.clone();
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.attempt_count = info.attempt_count.saturating_add(1);
        })
    })
    .await
    {
        log::warn!(
            "Failed to increment attempt_count for {}: {:#}",
            minion.minion_id,
            e
        );
    }

    let session_uuid = match Uuid::parse_str(&session_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            // Revert registry to Stopped since we claimed Autonomous but can't proceed
            revert_to_stopped(&minion.minion_id).await;
            return Err(anyhow::anyhow!(e).context("Failed to parse session ID from registry"));
        }
    };

    // Clear wake_reason unconditionally so it never leaks in the registry,
    // regardless of which prompt branch wins below.
    if wake_reason.is_some() {
        let mid = minion.minion_id.clone();
        if let Err(e) = with_registry(move |reg| {
            reg.update(&mid, |i| {
                i.wake_reason = None;
            })
        })
        .await
        {
            log::warn!(
                "Failed to clear wake_reason for {}: {}",
                minion.minion_id,
                e
            );
        }
    }

    // Build the continuation prompt.
    // Priority: explicit additional_prompt > wake_reason (review-focused) > None (use default).
    //
    // Note: when the lab daemon wakes a minion for new reviews it sets start_phase =
    // MonitoringPr, so the agent-run phase is skipped and the prompt is never passed
    // to the agent directly. In that case `wake_reason` acts as metadata signalling
    // WHY the minion was woken, and the actual review response is handled by
    // `monitor_pr_lifecycle`'s own review-detection loop.
    let resume_prompt = if let Some(ref extra) = additional_prompt {
        Some(format!(
            "Continue working on this issue. Additional instructions: {}",
            extra
        ))
    } else {
        wake_reason
    };

    // Resolve the agent backend from registry (use stored agent name)
    let backend = match agent_registry::resolve_backend(&agent_name) {
        Ok(b) => b,
        Err(e) => {
            // Revert registry to Stopped since we claimed Autonomous but can't proceed
            revert_to_stopped(&minion.minion_id).await;
            return Err(e.context("Failed to resolve agent backend for resume"));
        }
    };

    // Compute effective timeout: use CLI flag if provided, otherwise compute
    // remaining time from timeout_deadline. This ensures resumed minions honor
    // the original timeout budget rather than resetting it.
    let effective_timeout: Option<String> = if timeout_opt.is_some() {
        timeout_opt
    } else if let Some(deadline) = timeout_deadline {
        let remaining = deadline - Utc::now();
        if remaining.num_seconds() > 0 {
            Some(format!("{}s", remaining.num_seconds()))
        } else {
            // Deadline just passed between the check above and here — treat as expired
            mark_minion_failed(&minion.minion_id).await;
            bail!(
                "Minion {} has passed its timeout deadline ({}). Marking as failed.",
                minion.minion_id,
                deadline
            );
        }
    } else {
        None
    };

    // Resolve host from worktree git remote, falling back to config-based inference
    let checkout_path = minion.checkout_path();
    let host = resolve_host_from_worktree(&checkout_path, &owner).await;

    let wt_ctx = WorktreeContext {
        minion_id: minion.minion_id.clone(),
        branch_name,
        minion_dir: minion.worktree_path.clone(),
        checkout_path,
        session_id: session_uuid,
    };

    let details = match issue_num {
        Some(num) => fetch_issue_details(&owner, &repo_name, &host, num).await,
        None => None,
    };
    let issue_ctx = IssueContext {
        owner,
        repo: repo_name,
        host,
        issue_num,
        details,
    };

    Ok(ResumeContext {
        wt_ctx,
        issue_ctx,
        backend,
        resume_prompt,
        start_phase,
        effective_timeout,
        no_watch,
        command,
        _minion_lock: minion_lock,
    })
}

/// Runs the resume pipeline: agent session, PR creation, and PR monitoring.
///
/// Reuses the phase helpers from `fix/worker.rs` to avoid duplicating
/// orchestration logic.
async fn run_resume_pipeline(ctx: ResumeContext, quiet: bool) -> Result<i32> {
    let ResumeContext {
        wt_ctx,
        issue_ctx,
        backend,
        resume_prompt,
        start_phase,
        effective_timeout,
        no_watch,
        command,
        _minion_lock,
    } = ctx;

    // Non-"do" minions (e.g., "prompt", "review") only run the agent phase —
    // they should not create PRs or enter the monitoring lifecycle.
    let agent_only = !crate::minion_registry::is_pr_monitoring_command(&command);

    // Rename tmux window for the resume session
    let _tmux_guard = TmuxGuard::new(&format!("gru:{}", wt_ctx.minion_id));

    println!(
        "🔄 Resuming Minion {} in autonomous mode...",
        wt_ctx.minion_id
    );
    println!("📂 Workspace: {}", wt_ctx.checkout_path.display());

    // Phase: Run agent
    let agent_result = match run_agent_phase(
        &*backend,
        &issue_ctx,
        &wt_ctx,
        &start_phase,
        quiet,
        effective_timeout.as_deref(),
        resume_prompt.as_deref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) if is_stuck_or_timeout_error(&e) => {
            log::error!("🚨 {:#}", e);
            if agent_only {
                cleanup_registry(&wt_ctx.minion_id).await;
            }
            return Ok(1);
        }
        Err(e) => return Err(e),
    };

    // Check agent result — non-zero exit means failure
    if let Some(ref result) = agent_result {
        if !result.status.success() {
            println!("❌ Agent session exited with non-zero status");
            if agent_only {
                cleanup_registry(&wt_ctx.minion_id).await;
            }
            return Ok(result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
        }
    }

    // Agent-only commands (review, prompt) have no post-agent phases.
    // If the agent already ran in a prior invocation (start_phase past RunningAgent)
    // or just completed, mark as done and exit.  For previously-failed minions,
    // preserve the Failed phase and return a non-zero exit code so the failure
    // signal isn't lost.
    if agent_only && (agent_result.is_some() || start_phase > OrchestrationPhase::RunningAgent) {
        if start_phase == OrchestrationPhase::Failed {
            cleanup_registry(&wt_ctx.minion_id).await;
            println!(
                "❌ Minion {} was previously failed — cleaning up",
                wt_ctx.minion_id
            );
            return Ok(1);
        }
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
        cleanup_registry(&wt_ctx.minion_id).await;
        println!("✅ Resume completed for Minion {}", wt_ctx.minion_id);
        return Ok(agent_exit_code(&agent_result));
    }

    // Phase: Create PR
    let mut pr_number = create_pr_phase(&issue_ctx, &wt_ctx, &start_phase, false).await?;

    // Last resort: discover PR by head branch (handles manual PR creation or missing registry state)
    if pr_number.is_none() {
        pr_number = discover_pr_by_branch(&issue_ctx, &wt_ctx).await;
    }

    // Respect no_watch: skip lifecycle monitoring for fire-and-forget minions
    if no_watch {
        if let Some(ref pr_num) = pr_number {
            println!(
                "PR #{}. Skipping lifecycle monitoring (--no-watch).",
                pr_num
            );
        }
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
        cleanup_registry(&wt_ctx.minion_id).await;
        println!("✅ Resume completed for Minion {}", wt_ctx.minion_id);
        return Ok(agent_exit_code(&agent_result));
    }

    // Phase: Monitor PR lifecycle (reviews, CI, merge).
    // When no PR exists, monitor_pr_phase falls back to standalone CI monitoring —
    // aligning resume behavior with `gru do`.
    let monitor_timeout = Duration::from_secs(24 * 3600);
    let monitor_result = monitor_pr_phase(
        &*backend,
        &issue_ctx,
        &wt_ctx,
        &pr_number,
        effective_timeout.as_deref(),
        None, // review_timeout: use default
        monitor_timeout,
    )
    .await;

    if monitor_result.is_err() {
        cleanup_registry(&wt_ctx.minion_id).await;
        return Ok(1);
    }

    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
    cleanup_registry(&wt_ctx.minion_id).await;
    println!("✅ Resume completed for Minion {}", wt_ctx.minion_id);
    Ok(agent_exit_code(&agent_result))
}

/// Best-effort registry cleanup: clear PID and set mode to Stopped.
///
/// Mirrors the cleanup in `fix::run_worker` so the minion isn't left in
/// Autonomous mode after the pipeline finishes.
async fn cleanup_registry(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
        })
    })
    .await;
}

/// Last-resort PR discovery by head branch name.
///
/// Handles cases where the PR was created manually or the registry state is missing.
async fn discover_pr_by_branch(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Option<String> {
    match crate::ci::get_pr_number(
        &issue_ctx.host,
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        None,
    )
    .await
    {
        Ok(Some(num)) => {
            log::info!("Discovered PR #{} by branch name", num);
            Some(num.to_string())
        }
        Ok(None) => None,
        Err(e) => {
            log::warn!("Failed to discover PR by branch: {:#}", e);
            None
        }
    }
}

/// Resolve the GitHub host for a worktree, for Gru's own API calls.
///
/// Order: the repo's remotes (filtered to `owner`), then a configured host for
/// `owner`, then github.com. An inherited `GH_HOST` is deliberately *not*
/// consulted: this host targets Gru's own API calls, and a shell-level
/// `GH_HOST` left over from an unrelated GHES would silently retarget them.
/// Use this where a host is required. Callers that only route a child agent
/// process want [`resolve_child_host_from_worktree`], where inheriting is the
/// right answer because the child would have inherited it anyway.
pub(crate) async fn resolve_host_from_worktree(
    checkout_path: &std::path::Path,
    owner: &str,
) -> String {
    let (resolved, unknown) = resolve_host_from_remotes(checkout_path, owner).await;
    if let Some(host) = resolved {
        return host;
    }
    if let Some(host) = crate::github::configured_host_for_owner(owner, None) {
        return host;
    }
    // Only now is an unrecognised remote actually a problem: nothing else
    // named a host, so github.com is a guess that may be wrong.
    crate::git::warn_unknown_remotes(&unknown);
    "github.com".to_string()
}

/// Resolve the `GH_HOST` to hand a child agent process, or `None`.
///
/// Remotes, then a configured host for `owner`, then an inherited `GH_HOST`.
/// Unlike [`resolve_host_from_worktree`] there is no github.com default: when
/// all three come up empty the repo is on an unconfigured GHES, and sending
/// the child to github.com would silently retarget its `gh` calls. `None`
/// means "leave `GH_HOST` unset" — see [`apply_child_host`] — and only that
/// dead end warrants the unrecognised-host warning.
pub(crate) async fn resolve_child_host_from_worktree(
    checkout_path: &std::path::Path,
    owner: &str,
) -> Option<String> {
    let (resolved, unknown) = resolve_host_from_remotes(checkout_path, owner).await;
    if resolved.is_some() {
        return resolved;
    }
    let fallback = host_fallback(
        crate::github::configured_host_for_owner(owner, None),
        inherited_gh_host(),
    );
    if fallback.is_none() {
        crate::git::warn_unknown_remotes(&unknown);
    }
    fallback
}

/// Fallback order once the higher-priority source yields nothing: `resolved`,
/// then an inherited `GH_HOST`. An owner explicitly configured on public
/// GitHub counts as resolved, so it wins over an inherited `GH_HOST` pointing
/// at some unrelated GHES. A blank inherited value is treated as unset.
///
/// Shared with `gru chat`, and split out from
/// [`resolve_child_host_from_worktree`] so it can be tested without mutating
/// the process-global `GH_HOST`.
pub(crate) fn host_fallback(resolved: Option<String>, inherited: Option<String>) -> Option<String> {
    resolved.or_else(|| inherited.filter(|h| !h.trim().is_empty()))
}

/// The inherited `GH_HOST`, if the environment set one.
///
/// The single read of the variable, so callers stay testable by taking the
/// value as a parameter.
pub(crate) fn inherited_gh_host() -> Option<String> {
    std::env::var("GH_HOST").ok()
}

/// Applies a child-routing host, as resolved by
/// [`resolve_child_host_from_worktree`], to an already-built command.
///
/// `None` means nothing — not the remotes, not config, not the environment —
/// identified a host, so the child is left with no `GH_HOST` rather than one
/// pointing at the wrong instance.
pub(crate) fn apply_child_host(cmd: &mut tokio::process::Command, host: Option<&str>) {
    match host {
        Some(host) => {
            cmd.env("GH_HOST", host);
        }
        None => {
            cmd.env_remove("GH_HOST");
        }
    }
}

/// Resolve the GitHub host for a worktree by inspecting its git remotes.
///
/// Thin wrapper over [`crate::git::resolve_github_host_for_owner`]: candidates
/// are filtered by `owner` (pass `""` when the caller has no repo in mind).
/// The host is `None` when no remote yields a GitHub repo, so callers that set
/// `GH_HOST` on a child process can leave an inherited value alone instead of
/// overriding it with a guess.
///
/// The unrecognised-host remotes come back with it rather than being warned
/// about here: the caller may still resolve a host from config or the
/// environment, and should only report the configuration gap once it has run
/// out of options. See [`crate::git::warn_unknown_remotes`].
pub(crate) async fn resolve_host_from_remotes(
    checkout_path: &std::path::Path,
    owner: &str,
) -> (Option<String>, Vec<crate::git::UnknownRemote>) {
    let host_registry = crate::config::load_host_registry();
    crate::git::resolve_github_host_for_owner(checkout_path, &host_registry, owner).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_fallback_prefers_configured_host() {
        assert_eq!(
            host_fallback(
                Some("ghe.example.com".to_string()),
                Some("github.com".to_string())
            ),
            Some("ghe.example.com".to_string())
        );
    }

    #[test]
    fn test_host_fallback_preserves_inherited_host() {
        // An unconfigured GHES resolves to nothing; the user's own GH_HOST is
        // a better answer than retargeting their agent at github.com.
        assert_eq!(
            host_fallback(None, Some("code.corp.example.com".to_string())),
            Some("code.corp.example.com".to_string())
        );
    }

    #[test]
    fn test_host_fallback_yields_nothing_when_nothing_inherited() {
        // No host anywhere: child routing leaves GH_HOST unset rather than
        // retargeting the agent at github.com.
        assert_eq!(host_fallback(None, None), None);
        assert_eq!(host_fallback(None, Some("   ".to_string())), None);
    }

    /// Init a git repo with the given `(remote_name, url)` pairs.
    async fn repo_with_remotes(remotes: &[(&str, &str)]) -> tempfile::TempDir {
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
    async fn test_resolve_host_prefers_origin() {
        let dir = repo_with_remotes(&[
            ("origin", "https://github.com/owner/repo.git"),
            ("upstream", "https://ghes.example.com/owner/repo.git"),
        ])
        .await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "owner").await,
            "github.com"
        );
    }

    #[tokio::test]
    async fn test_resolve_host_uses_non_origin_remote() {
        // No origin at all: the GitHub remote is named `upstream`.
        let dir = repo_with_remotes(&[("upstream", "git@github.com:owner/repo.git")]).await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "owner").await,
            "github.com"
        );
    }

    #[tokio::test]
    async fn test_resolve_host_ignores_non_github_only_repo() {
        // A GitLab-only repo must not have its host exported as GH_HOST just
        // because the URL shape is valid; fall back to the config heuristic.
        let dir =
            repo_with_remotes(&[("origin", "https://gitlab.example.com/owner/repo.git")]).await;
        assert_eq!(resolve_host_from_remotes(dir.path(), "owner").await.0, None);
    }

    #[tokio::test]
    async fn test_resolve_host_uses_push_url_when_fetch_is_a_mirror() {
        // `origin` fetches from a non-GitHub mirror but pushes to GHES; the
        // push URL must still be considered.
        let dir =
            repo_with_remotes(&[("origin", "https://gitlab.example.com/owner/repo.git")]).await;
        std::process::Command::new("git")
            .args([
                "remote",
                "set-url",
                "--push",
                "origin",
                "https://github.corp.example.com/owner/repo.git",
            ])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git");
        assert_eq!(
            resolve_host_from_remotes(dir.path(), "owner")
                .await
                .0
                .as_deref(),
            Some("github.corp.example.com")
        );
    }

    #[tokio::test]
    async fn test_resolve_host_skips_non_github_origin() {
        // origin is a non-GitHub mirror; the registry-known remote wins.
        let dir = repo_with_remotes(&[
            ("origin", "https://gitlab.example.com/owner/repo.git"),
            ("github", "https://github.com/owner/repo.git"),
        ])
        .await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "owner").await,
            "github.com"
        );
    }

    #[tokio::test]
    async fn test_resolve_host_falls_back_to_unknown_origin_host() {
        // Unconfigured GHES instance: not in the registry, but still better
        // than defaulting to github.com.
        let dir = repo_with_remotes(&[("origin", "https://ghes.example.com/owner/repo.git")]).await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "owner").await,
            "ghes.example.com"
        );
    }

    #[tokio::test]
    async fn test_resolve_host_returns_unknown_remotes_instead_of_warning() {
        // The warning belongs to whoever runs out of options: remotes are only
        // the first source, so an unrecognised host comes back to the caller to
        // report after the configured-host and inherited-GH_HOST fallbacks.
        let dir =
            repo_with_remotes(&[("origin", "https://code.corp.example.com/acme/widgets.git")])
                .await;
        let (host, unknown) = resolve_host_from_remotes(dir.path(), "acme").await;
        assert_eq!(host, None);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].host, "code.corp.example.com");

        // Nothing to report once a host resolves.
        let dir = repo_with_remotes(&[("origin", "https://github.com/acme/widgets.git")]).await;
        let (host, unknown) = resolve_host_from_remotes(dir.path(), "acme").await;
        assert_eq!(host.as_deref(), Some("github.com"));
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_host_no_remotes_resolves_nothing() {
        let dir = repo_with_remotes(&[]).await;
        assert_eq!(resolve_host_from_remotes(dir.path(), "").await.0, None);
    }

    #[tokio::test]
    async fn test_resolve_host_from_worktree_ignores_inherited_gh_host() {
        // This host targets Gru's own API calls, so a shell-level GH_HOST for
        // an unrelated instance must not retarget them. Deterministic whether
        // or not the test machine exports GH_HOST, since it is never read.
        let dir = repo_with_remotes(&[]).await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "unconfigured-owner").await,
            "github.com"
        );
    }

    #[tokio::test]
    async fn test_resolve_host_filters_candidates_by_owner() {
        // The globally ranked winner (`origin`) belongs to another owner, so
        // resuming work for `corp` must follow `upstream` to corp's instance
        // rather than exporting GH_HOST=github.com.
        let dir = repo_with_remotes(&[
            ("origin", "https://github.com/someone/other.git"),
            (
                "upstream",
                "https://github.corp.example.com/corp/project.git",
            ),
        ])
        .await;
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "corp").await,
            "github.corp.example.com"
        );
        // An empty owner means no repo in mind, so the global winner stands.
        assert_eq!(
            resolve_host_from_worktree(dir.path(), "").await,
            "github.com"
        );
    }

    #[test]
    fn test_child_host_prefers_explicit_github_com_over_inherited() {
        // `configured_host_for_owner` reports an explicitly configured public
        // owner as Some("github.com"), so the inherited value must not win.
        assert_eq!(
            host_fallback(
                Some("github.com".to_string()),
                Some("ghes.example.com".to_string())
            ),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_apply_child_host_unsets_when_unresolved() {
        let mut cmd = tokio::process::Command::new("true");
        apply_child_host(&mut cmd, Some("ghe.example.com"));
        let set: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(set.contains(&(
            std::ffi::OsStr::new("GH_HOST"),
            Some(std::ffi::OsStr::new("ghe.example.com"))
        )));

        let mut cmd = tokio::process::Command::new("true");
        apply_child_host(&mut cmd, None);
        let set: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(set.contains(&(std::ffi::OsStr::new("GH_HOST"), None)));
    }

    #[tokio::test]
    async fn test_handle_resume_with_invalid_id() {
        let result = handle_resume("nonexistent-minion-xyz".to_string(), None, None, false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_handle_resume_with_prompt_and_invalid_id() {
        let result = handle_resume(
            "nonexistent-minion-xyz".to_string(),
            Some("Add error handling".to_string()),
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[test]
    fn test_is_pr_monitoring_command_do() {
        assert!(crate::minion_registry::is_pr_monitoring_command("do"));
    }

    #[test]
    fn test_is_pr_monitoring_command_legacy_fix() {
        assert!(crate::minion_registry::is_pr_monitoring_command("fix"));
    }

    #[test]
    fn test_is_pr_monitoring_command_review() {
        assert!(!crate::minion_registry::is_pr_monitoring_command("review"));
    }

    #[test]
    fn test_is_pr_monitoring_command_prompt() {
        assert!(!crate::minion_registry::is_pr_monitoring_command("prompt"));
    }
}
