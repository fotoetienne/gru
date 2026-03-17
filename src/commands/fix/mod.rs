mod agent;
mod helpers;
mod monitor;
mod pr;
mod resolve;
mod types;
mod worktree;

// Re-export public API used by other modules (e.g., resume.rs)
pub(crate) use helpers::update_orchestration_phase;
pub(crate) use pr::handle_pr_creation;
pub use types::FixOptions;
pub(crate) use types::{IssueContext, WorktreeContext};

use agent::{resume_agent_session, run_agent_session};
pub(crate) use helpers::{try_mark_issue_blocked, try_mark_issue_failed};
pub(crate) use monitor::monitor_ci_after_fix;
pub(crate) use monitor::monitor_pr_lifecycle;
use resolve::{check_existing_minions, claim_issue, resolve_issue};
use types::ExistingMinionCheck;
use worktree::setup_worktree;

use crate::agent_registry;
use crate::agent_runner::{is_stuck_or_timeout_error, parse_timeout, EXIT_CODE_SIGNAL_TERMINATED};
use crate::minion_registry::{with_registry, MinionMode, OrchestrationPhase};
use crate::pr_monitor;
use crate::tmux::TmuxGuard;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use tokio::time::Duration;
use uuid::Uuid;

/// Spawns the current binary as a background worker process.
///
/// The worker runs with `--worker <minion_id>` and inherits all other flags.
/// Stdout/stderr are redirected to `gru.log` in the minion directory.
/// Returns the child PID on success.
async fn spawn_worker(
    issue: &str,
    minion_id: &str,
    minion_dir: &Path,
    opts: &FixOptions,
) -> Result<u32> {
    let gru_exe = std::env::current_exe().context("Failed to determine gru executable path")?;

    let log_path = minion_dir.join("gru.log");
    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("Failed to create log file: {}", log_path.display()))?;
    let stderr_file = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    let mut cmd = std::process::Command::new(gru_exe);
    cmd.arg("do").arg(issue);
    cmd.arg("--worker").arg(minion_id);

    // Forward relevant flags
    if let Some(ref t) = opts.timeout {
        cmd.arg("--timeout").arg(t);
    }
    if let Some(ref t) = opts.review_timeout {
        cmd.arg("--review-timeout").arg(t);
    }
    if let Some(ref t) = opts.monitor_timeout {
        cmd.arg("--monitor-timeout").arg(t);
    }
    if opts.force_new {
        cmd.arg("--force-new");
    }
    if opts.agent_name != crate::agent_registry::DEFAULT_AGENT {
        cmd.arg("--agent").arg(&opts.agent_name);
    }
    if opts.no_watch {
        cmd.arg("--no-watch");
    }
    if opts.auto_merge {
        cmd.arg("--auto-merge");
    }
    if opts.quiet {
        cmd.arg("--quiet");
    }
    if opts.ignore_deps {
        cmd.arg("--ignore-deps");
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::from(log_file));
    cmd.stderr(std::process::Stdio::from(stderr_file));

    // Create a new session so the worker survives terminal close / SIGHUP
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = cmd
        .spawn()
        .context("Failed to spawn background worker process")?;
    let pid = child.id();

    // Reap child in background to prevent zombie processes
    std::thread::spawn(move || {
        let _ = child.wait_with_output();
    });

    // Record worker PID in registry
    let mid = minion_id.to_string();
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.pid = Some(pid);
            info.pid_start_time = crate::minion_registry::get_process_start_time(pid);
            info.mode = MinionMode::Autonomous;
            info.last_activity = Utc::now();
        })
    })
    .await
    {
        log::warn!("Failed to record worker PID in registry: {}", e);
    }

    Ok(pid)
}

/// Worker entry point: runs phases 3-5 for a previously-registered minion.
///
/// Looks up the minion in the registry by ID, resolves the agent backend,
/// and runs the agent session, PR creation, and monitoring phases.
async fn run_worker(minion_id: &str, issue: &str, opts: FixOptions) -> Result<i32> {
    let FixOptions {
        timeout: timeout_opt,
        review_timeout: review_timeout_opt,
        monitor_timeout: monitor_timeout_opt,
        quiet,
        agent_name,
        no_watch,
        auto_merge,
        ..
    } = opts;

    // Parse review/monitor timeouts
    let review_timeout = review_timeout_opt
        .map(|s| parse_timeout(&s))
        .transpose()
        .context("Invalid --review-timeout value")?;

    let monitor_timeout = match monitor_timeout_opt {
        Some(s) => {
            let d = parse_timeout(&s).context("Invalid --monitor-timeout value")?;
            if d.is_zero() {
                anyhow::bail!("--monitor-timeout must be greater than zero");
            }
            d
        }
        None => Duration::from_secs(24 * 3600),
    };

    // Look up minion in registry
    let mid = minion_id.to_string();
    let registry_info = with_registry(move |reg| Ok(reg.get(&mid).cloned())).await?;

    let info =
        registry_info.with_context(|| format!("Minion {} not found in registry", minion_id))?;

    // Validate repo format
    if !info.repo.contains('/') {
        anyhow::bail!("Invalid repo format in registry: '{}'", info.repo);
    }

    let session_id =
        Uuid::parse_str(&info.session_id).context("Failed to parse session ID from registry")?;

    let checkout_path = info.checkout_path();
    let wt_ctx = WorktreeContext {
        minion_id: minion_id.to_string(),
        branch_name: info.branch.clone(),
        minion_dir: info.worktree.clone(),
        checkout_path,
        session_id,
    };

    // Resolve backend and issue context
    let backend = agent_registry::resolve_backend(&agent_name)?;

    // Fetch fresh issue details for the worker
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let issue_ctx = resolve_issue(issue, &github_hosts).await?;

    // Determine resume phase from registry
    let start_phase = info.orchestration_phase.clone();

    // Phase 3: Run agent
    let agent_result = if start_phase <= OrchestrationPhase::RunningAgent {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::RunningAgent).await;

        // Use --resume only if the agent has already run (session ID was used).
        // If interrupted during Setup, the session was never started.
        let use_resume = start_phase > OrchestrationPhase::Setup;
        let result = if use_resume {
            resume_agent_session(
                &*backend,
                &issue_ctx,
                &wt_ctx,
                quiet,
                timeout_opt.as_deref(),
            )
            .await
        } else {
            run_agent_session(
                &*backend,
                &issue_ctx,
                &wt_ctx,
                quiet,
                timeout_opt.as_deref(),
            )
            .await
        };

        match result {
            Ok(result) => Some(result),
            Err(e) if is_stuck_or_timeout_error(&e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                log::error!("🚨 {:#}", e);
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
                return Ok(1);
            }
            Err(e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        }
    } else {
        println!("⏭️  Skipping agent session (already completed)");
        None
    };

    // Check agent result
    if let Some(ref result) = agent_result {
        if !result.status.success() {
            update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;

            try_mark_issue_failed(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
                issue_ctx.issue_num,
            )
            .await;

            return Ok(result.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
        }
    }

    // Phase 4: Create PR
    let pr_number = if start_phase <= OrchestrationPhase::CreatingPr {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::CreatingPr).await;
        match handle_pr_creation(&issue_ctx, &wt_ctx).await {
            Ok(pr) => pr,
            Err(e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        }
    } else {
        println!("⏭️  Skipping PR creation (already completed)");
        let minion_id_owned = wt_ctx.minion_id.clone();
        let existing_pr = match with_registry(move |registry| {
            Ok(registry
                .get(&minion_id_owned)
                .and_then(|info| info.pr.clone()))
        })
        .await
        {
            Ok(pr) => pr,
            Err(e) => {
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                return Err(e);
            }
        };

        if existing_pr.is_some() {
            existing_pr
        } else {
            log::info!("ℹ️  PR not found in registry, retrying PR creation");
            match handle_pr_creation(&issue_ctx, &wt_ctx).await {
                Ok(pr) => pr,
                Err(e) => {
                    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                    return Err(e);
                }
            }
        }
    };

    // Add gru:auto-merge label if --auto-merge flag was set
    if auto_merge {
        if let Some(ref pr_num) = pr_number {
            if let Err(e) = pr_monitor::ensure_auto_merge_label(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
            )
            .await
            {
                log::warn!("⚠️  Failed to ensure gru:auto-merge label: {}", e);
            }
            match pr_monitor::add_auto_merge_label(
                &issue_ctx.host,
                &issue_ctx.owner,
                &issue_ctx.repo,
                pr_num,
            )
            .await
            {
                Ok(()) => println!("🏷️  Added gru:auto-merge label to PR #{}", pr_num),
                Err(e) => log::warn!("⚠️  Failed to add gru:auto-merge label: {}", e),
            }
        }
    }

    let agent_exit_code = || {
        agent_result
            .as_ref()
            .map(|r| r.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
            .unwrap_or(0)
    };

    if no_watch {
        if let Some(ref pr_num) = pr_number {
            println!(
                "PR #{} created. Skipping lifecycle monitoring (--no-watch).",
                pr_num
            );
        }
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;
        return Ok(agent_exit_code());
    }

    // Phase 5: Monitor PR lifecycle
    if let Some(ref pr_num) = pr_number {
        update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::MonitoringPr).await;
        monitor_pr_lifecycle(
            &*backend,
            &issue_ctx,
            &wt_ctx,
            pr_num,
            timeout_opt.as_deref(),
            review_timeout,
            monitor_timeout,
        )
        .await;
    } else {
        log::warn!(
            "⚠️  No PR number available — skipping PR lifecycle monitoring. \
             Branch may not have been pushed, or PR lookup failed."
        );
    }

    // CI monitoring — only when PR creation failed (no PR number), since
    // monitor_pr_lifecycle handles CI internally when a PR exists.
    // When no_watch is true, we have already returned early above.
    if pr_number.is_none() {
        let ci_passed = monitor_ci_after_fix(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            &wt_ctx.branch_name,
            &wt_ctx.checkout_path,
            &wt_ctx.minion_id,
        )
        .await;
        match ci_passed {
            Ok(true) => log::info!("✅ CI checks passed"),
            Ok(false) => {
                log::warn!("⚠️  CI checks failed or were escalated");
                update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Failed).await;
                try_mark_issue_blocked(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    issue_ctx.issue_num,
                )
                .await;
                return Ok(1);
            }
            Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {}", e),
        }
    }

    update_orchestration_phase(&wt_ctx.minion_id, OrchestrationPhase::Completed).await;

    // Clear PID from registry on clean exit
    let mid_cleanup = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid_cleanup, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
        })
    })
    .await;

    Ok(agent_exit_code())
}

/// Handles the fix command by delegating to the agent backend.
/// Returns the exit code from the agent process.
///
/// In normal (foreground) mode, orchestrates:
/// 1. `resolve_issue` - Parse issue, check duplicates, fetch details
/// 2. `setup_worktree` - Clone repo, create worktree, register minion
/// 3. Spawn background worker process
/// 4. Auto-tail events.jsonl (unless --detach)
///
/// In worker mode (--worker <minion_id>), runs phases 3-5 directly:
/// 3. `run_agent_session` - Build prompt, run agent, track progress
/// 4. `handle_pr_creation` - Push check, create PR, update labels
/// 5. `monitor_pr_lifecycle` - Review, poll for updates, handle feedback
///
/// If a previous session for the same issue was interrupted, it will
/// automatically resume from the last completed phase.
pub async fn handle_fix(issue: &str, opts: FixOptions) -> Result<i32> {
    // Worker mode: run phases 3-5 directly (background process)
    if let Some(ref minion_id) = opts.worker {
        let mid = minion_id.clone();
        return run_worker(&mid, issue, opts).await;
    }

    let quiet = opts.quiet;
    let force_new = opts.force_new;
    let detach = opts.detach;
    let agent_name = &opts.agent_name;

    // Validate agent name early (fail fast on unknown agents)
    let _backend = agent_registry::resolve_backend(agent_name)?;

    let ignore_deps = opts.ignore_deps;

    // Phase 1: Resolve issue
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let issue_ctx = resolve_issue(issue, &github_hosts).await?;

    // Check for unresolved blockers (unless --ignore-deps)
    if !ignore_deps {
        let body = issue_ctx
            .details
            .as_ref()
            .map(|d| d.body.as_str())
            .unwrap_or("");
        let blockers = crate::dependencies::get_blockers(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_ctx.issue_num,
            body,
        )
        .await;

        if !blockers.is_empty() {
            let blocker_list: Vec<String> = blockers.iter().map(|n| format!("#{}", n)).collect();
            println!(
                "⚠️  Issue #{} may have unresolved blockers: {}",
                issue_ctx.issue_num,
                blocker_list.join(", ")
            );
            println!("   Use --ignore-deps to suppress this warning.");
        }
    }

    // Rename tmux window early with the initial `gru:do:#N` name
    let tmux_guard = TmuxGuard::new(&format!("gru:do:#{}", issue_ctx.issue_num));

    // Phase 2: Determine whether to resume or start fresh
    let (wt_ctx, is_fresh) = if force_new {
        (setup_worktree(&issue_ctx, agent_name, &opts).await?, true)
    } else {
        match check_existing_minions(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num).await?
        {
            ExistingMinionCheck::None => {
                (setup_worktree(&issue_ctx, agent_name, &opts).await?, true)
            }
            ExistingMinionCheck::Resumable(minion_id, info) => {
                let phase = info.orchestration_phase.clone();
                println!(
                    "🔄 Resuming interrupted session {} (phase: {:?})",
                    minion_id, phase
                );
                // Increment attempt_count for this auto-resume
                let mid = minion_id.clone();
                with_registry(move |reg| {
                    reg.update(&mid, |info| {
                        info.attempt_count = info.attempt_count.saturating_add(1);
                    })
                })
                .await
                .ok();
                let session_id = Uuid::parse_str(&info.session_id)
                    .context("Failed to parse session ID from registry")?;
                let checkout_path = info.checkout_path();
                (
                    WorktreeContext {
                        minion_id,
                        branch_name: info.branch,
                        minion_dir: info.worktree,
                        checkout_path,
                        session_id,
                    },
                    false,
                )
            }
            ExistingMinionCheck::AlreadyRunning => return Ok(1),
        }
    };

    // Update tmux window name now that we have the Minion ID (gru:do:#N → gru:M042:#N)
    tmux_guard.rename(&format!(
        "gru:{}:#{}",
        wt_ctx.minion_id, issue_ctx.issue_num
    ));

    // Claim the issue on fresh starts (skip on resume — already claimed)
    if is_fresh {
        claim_issue(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_ctx.issue_num,
        )
        .await;
    }

    // Phase 3: Spawn background worker
    let worker_pid = spawn_worker(issue, &wt_ctx.minion_id, &wt_ctx.minion_dir, &opts).await?;

    println!(
        "Minion {} spawned for issue #{} (PID: {})",
        wt_ctx.minion_id, issue_ctx.issue_num, worker_pid
    );

    if detach {
        println!(
            "Detached. Use `gru logs {}` to follow progress, `gru stop {}` to cancel.",
            wt_ctx.minion_id, wt_ctx.minion_id
        );
        return Ok(0);
    }

    // Auto-tail events
    println!(
        "Streaming progress... (Ctrl+C to detach, `gru stop {}` to cancel)\n",
        wt_ctx.minion_id
    );

    let events_path = wt_ctx.minion_dir.join("events.jsonl");
    let issue_str = issue_ctx.issue_num.to_string();
    crate::log_viewer::tail_events(events_path, &wt_ctx.minion_id, &issue_str, quiet).await?;

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::AgentRunnerError;
    use types::IssueDetails;

    /// Creates a test `WorktreeContext` with separate minion_dir and checkout_path.
    fn test_wt_ctx(path: &std::path::Path) -> WorktreeContext {
        let checkout = path.join("checkout");
        // Create checkout dir with .git marker so resolve_checkout_path detects new layout
        let _ = std::fs::create_dir_all(&checkout);
        let _ = std::fs::write(checkout.join(".git"), "gitdir: test");
        WorktreeContext {
            minion_id: "M001".to_string(),
            branch_name: "minion/issue-42-M001".to_string(),
            minion_dir: path.to_path_buf(),
            checkout_path: checkout,
            session_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn test_build_fix_prompt_with_details() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            host: "github.com".to_string(),
            issue_num: 42,
            details: Some(IssueDetails {
                title: "Fix the widget".to_string(),
                body: "The widget is broken".to_string(),
                labels: "bug, priority:high".to_string(),
            }),
        };

        let prompt = agent::build_fix_prompt(&ctx, &wt_ctx);
        assert!(prompt.starts_with("# Issue #42: Fix the widget"));
        assert!(prompt.contains("octocat/hello-world/issues/42"));
        assert!(prompt.contains("The widget is broken"));
        assert!(prompt.contains("Labels: bug, priority:high"));
        assert!(prompt.contains("## 1. Check if Decomposition is Needed"));
    }

    #[test]
    fn test_build_fix_prompt_without_details() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            host: "github.com".to_string(),
            issue_num: 42,
            details: None,
        };

        let prompt = agent::build_fix_prompt(&ctx, &wt_ctx);
        assert_eq!(prompt, "/do 42");
    }

    #[test]
    fn test_build_fix_prompt_empty_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            host: "github.com".to_string(),
            issue_num: 7,
            details: Some(IssueDetails {
                title: "Add feature".to_string(),
                body: "Please add this feature".to_string(),
                labels: String::new(),
            }),
        };

        let prompt = agent::build_fix_prompt(&ctx, &wt_ctx);
        assert!(prompt.contains("# Issue #7: Add feature"));
        assert!(!prompt.contains("Labels:"));
    }

    #[test]
    fn test_build_fix_prompt_uses_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        let ctx = IssueContext {
            owner: "myorg".to_string(),
            repo: "myproject".to_string(),
            host: "github.com".to_string(),
            issue_num: 99,
            details: Some(IssueDetails {
                title: "Template test".to_string(),
                body: "Body content here".to_string(),
                labels: "enhancement".to_string(),
            }),
        };

        let prompt = agent::build_fix_prompt(&ctx, &wt_ctx);

        // Verify template variables were substituted (no {{ }} patterns remaining
        // for known variables)
        assert!(!prompt.contains("{{ issue_number }}"));
        assert!(!prompt.contains("{{ issue_title }}"));
        assert!(!prompt.contains("{{ issue_body }}"));
        assert!(!prompt.contains("{{ repo_owner }}"));
        assert!(!prompt.contains("{{ repo_name }}"));

        // Verify the substituted values are present
        assert!(prompt.contains("99"));
        assert!(prompt.contains("Template test"));
        assert!(prompt.contains("Body content here"));
        assert!(prompt.contains("myorg"));
        assert!(prompt.contains("myproject"));
        assert!(prompt.contains("Labels: enhancement"));
    }

    #[test]
    fn test_build_fix_prompt_repo_override() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_ctx = test_wt_ctx(tmp.path());

        // Create a custom do prompt in the checkout dir (where the repo lives)
        let prompts_dir = wt_ctx.checkout_path.join(".gru").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(
            prompts_dir.join("do.md"),
            r#"---
description: Custom do
requires: [issue]
---
CUSTOM: Fix #{{ issue_number }} - {{ issue_title }}"#,
        )
        .unwrap();

        let ctx = IssueContext {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            host: "github.com".to_string(),
            issue_num: 55,
            details: Some(IssueDetails {
                title: "Custom test".to_string(),
                body: "Custom body".to_string(),
                labels: String::new(),
            }),
        };

        let prompt = agent::build_fix_prompt(&ctx, &wt_ctx);
        assert_eq!(prompt, "CUSTOM: Fix #55 - Custom test");
    }

    #[test]
    fn test_is_stuck_or_timeout_error_stuck() {
        let err: anyhow::Error = AgentRunnerError::InactivityStuck { minutes: 15 }.into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_task_timeout() {
        let err: anyhow::Error =
            AgentRunnerError::MaxTimeout(tokio::time::Duration::from_secs(600)).into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_stream_timeout() {
        let err: anyhow::Error = AgentRunnerError::StreamTimeout { seconds: 300 }.into();
        assert!(is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_other_error() {
        let err = anyhow::anyhow!("Failed to spawn claude process");
        assert!(!is_stuck_or_timeout_error(&err));
    }

    #[test]
    fn test_is_stuck_or_timeout_error_wrapped_in_context() {
        // Typed errors survive context wrapping, unlike string matching
        let err: anyhow::Error = AgentRunnerError::InactivityStuck { minutes: 15 }.into();
        let wrapped = err.context("Claude session failed");
        assert!(is_stuck_or_timeout_error(&wrapped));
    }

    #[test]
    fn test_fix_options_no_watch_default_false() {
        let opts = FixOptions {
            timeout: None,
            review_timeout: None,
            monitor_timeout: None,
            quiet: false,
            force_new: false,
            agent_name: "claude".to_string(),
            no_watch: false,
            auto_merge: false,
            detach: false,
            ignore_deps: false,
            worker: None,
        };
        assert!(!opts.no_watch);
    }

    #[test]
    fn test_fix_options_no_watch_set() {
        let opts = FixOptions {
            timeout: None,
            review_timeout: None,
            monitor_timeout: None,
            quiet: false,
            force_new: false,
            agent_name: "claude".to_string(),
            no_watch: true,
            auto_merge: false,
            detach: false,
            ignore_deps: false,
            worker: None,
        };
        assert!(opts.no_watch);
    }

    #[test]
    fn test_fix_options_auto_merge() {
        let opts = FixOptions {
            timeout: None,
            review_timeout: None,
            monitor_timeout: None,
            quiet: false,
            force_new: false,
            agent_name: "claude".to_string(),
            no_watch: false,
            auto_merge: true,
            detach: false,
            ignore_deps: false,
            worker: None,
        };
        assert!(opts.auto_merge);
    }

    #[test]
    fn test_fix_options_destructuring() {
        // Verify all fields can be destructured as handle_fix does
        let opts = FixOptions {
            timeout: Some("10m".to_string()),
            review_timeout: Some("5m".to_string()),
            monitor_timeout: Some("1h".to_string()),
            quiet: true,
            force_new: true,
            agent_name: "codex".to_string(),
            no_watch: true,
            auto_merge: true,
            detach: true,
            ignore_deps: false,
            worker: Some("M001".to_string()),
        };
        let FixOptions {
            timeout,
            review_timeout,
            monitor_timeout,
            quiet,
            force_new,
            agent_name,
            no_watch,
            auto_merge,
            ..
        } = opts;
        assert_eq!(timeout.as_deref(), Some("10m"));
        assert_eq!(review_timeout.as_deref(), Some("5m"));
        assert_eq!(monitor_timeout.as_deref(), Some("1h"));
        assert!(quiet);
        assert!(force_new);
        assert_eq!(agent_name, "codex");
        assert!(no_watch);
        assert!(auto_merge);
    }
}
