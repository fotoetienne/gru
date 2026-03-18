use crate::config::{parse_repo_entry_with_hosts, LabConfig};
use crate::github::{self, list_ready_issues_via_cli};
use crate::labels;
use crate::minion_registry::{with_registry, MinionInfo, MinionMode, OrchestrationPhase};
use crate::pr_monitor;
use crate::tmux::TmuxGuard;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::time::sleep;

/// Maximum age of a spawn to be considered an "early exit" eligible for label restoration.
/// Processes that fail within this window likely never started meaningful work, so the
/// issue label should be restored to the ready state rather than left as in-progress.
const EARLY_EXIT_THRESHOLD: Duration = Duration::from_secs(30);

/// A child process tracked by the lab, with optional metadata for label restoration.
struct SpawnedChild {
    child: Child,
    /// Set for newly claimed issues so we can restore labels on early exit.
    /// None for resumed minions (they were already in-progress).
    spawn_meta: Option<SpawnMeta>,
}

/// Metadata needed to restore labels when a spawned process exits early.
struct SpawnMeta {
    host: String,
    owner: String,
    repo: String,
    issue_number: u64,
    ready_label: String,
    spawned_at: Instant,
}

/// Determines whether a failed spawn qualifies for label restoration.
/// Returns true if the process exited within the early-exit threshold, meaning it
/// likely never started meaningful work and the issue should be returned to the ready state.
fn should_restore_label(spawned_at: Instant) -> bool {
    spawned_at.elapsed() <= EARLY_EXIT_THRESHOLD
}

/// Handles the lab daemon command
pub async fn handle_lab(
    config_path: Option<PathBuf>,
    repos: Option<Vec<String>>,
    poll_interval: Option<u64>,
    max_slots: Option<usize>,
    no_resume: bool,
    stop_minions: bool,
) -> Result<i32> {
    // Load configuration
    let config = if let Some(path) = config_path {
        LabConfig::load(&path)?
    } else {
        let default_path = LabConfig::default_path()?;
        if default_path.exists() {
            LabConfig::load(&default_path)?
        } else if repos.is_none() {
            log::warn!("⚠️  No config file found at {}", default_path.display());
            log::warn!("   Use --repos flag or create a config file");
            log::warn!("");
            log::warn!("Example:");
            log::warn!("  gru lab --repos owner/repo1,owner/repo2 --slots 2");
            log::warn!("");
            anyhow::bail!("No repositories configured");
        } else {
            LabConfig::default()
        }
    };

    // Apply CLI overrides
    let config = config.with_overrides(repos, poll_interval, max_slots);

    // Validate final configuration
    config.validate()?;

    // Rename tmux window for the lab daemon
    let _tmux_guard = TmuxGuard::new("gru:lab");

    println!("🚀 Starting Gru Lab daemon");
    println!(
        "📋 Monitoring {} repository(ies)",
        config.daemon.repos.len()
    );
    println!("🔄 Poll interval: {}s", config.daemon.poll_interval_secs);
    println!("🎰 Max concurrent slots: {}", config.daemon.max_slots);
    println!("🏷️  Watching for label: {}", config.daemon.label);
    println!();

    for repo in &config.daemon.repos {
        println!("  • {}", repo);
    }

    println!();
    println!("Press Ctrl-C to stop...");
    println!();

    // Track child processes for graceful shutdown
    let mut children: Vec<SpawnedChild> = Vec::new();

    // Safety net: track Minion IDs we've already attempted to resume this session
    // to prevent resume loops even if phase updates are missed.
    let mut resumed_this_session: HashSet<String> = HashSet::new();

    // Track the last time we polled each Completed minion for new reviews.
    // Not persisted — resets on daemon restart, which is fine since the cooldown
    // only serves to rate-limit GitHub API calls within a session.
    let mut wake_check_times: HashMap<String, DateTime<Utc>> = HashMap::new();

    if no_resume {
        println!("⏭️  Auto-resume disabled (--no-resume)");
        println!();
    }

    // Perform initial poll immediately for faster feedback
    if let Err(e) = poll_and_spawn(
        &config,
        &mut children,
        no_resume,
        &mut resumed_this_session,
        &mut wake_check_times,
    )
    .await
    {
        log::warn!("⚠️  Initial polling error: {}", e);
        log::warn!("   Continuing to poll...");
    }

    // Listen for both SIGINT (Ctrl-C) and SIGTERM (kill, systemd, docker)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("Failed to register SIGTERM handler")?;

    // Main polling loop
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n🛑 Received shutdown signal (SIGINT), stopping daemon...");
                shutdown_children(&mut children, stop_minions).await;
                break;
            }
            _ = sigterm.recv() => {
                println!("\n🛑 Received shutdown signal (SIGTERM), stopping daemon...");
                shutdown_children(&mut children, stop_minions).await;
                break;
            }
            _ = sleep(config.poll_interval()) => {
                // Clean up finished child processes
                reap_children(&mut children).await;

                if let Err(e) = poll_and_spawn(
                    &config,
                    &mut children,
                    no_resume,
                    &mut resumed_this_session,
                    &mut wake_check_times,
                )
                .await
                {
                    log::warn!("⚠️  Polling error: {}", e);
                    log::warn!("   Continuing to poll...");
                }
            }
        }
    }

    Ok(0)
}

/// Remove finished child processes from the tracking vec.
/// For early exits (non-zero within threshold), restore labels to prevent orphaning.
///
/// Note: This is called once per poll interval, so the effective early-exit window
/// is `EARLY_EXIT_THRESHOLD` minus any delay before the next reap cycle. With typical
/// poll intervals (≤30s) this is fine, but very long poll intervals could cause
/// early exits to be detected after the threshold has passed.
async fn reap_children(children: &mut Vec<SpawnedChild>) {
    let mut i = 0;
    while i < children.len() {
        match children[i].child.try_wait() {
            Ok(Some(status)) => {
                log::info!("Minion process exited with status: {}", status);

                // If the process failed quickly, restore the issue label
                if !status.success() {
                    if let Some(meta) = &children[i].spawn_meta {
                        let elapsed = meta.spawned_at.elapsed();
                        if should_restore_label(meta.spawned_at) {
                            log::warn!(
                                "⚠️  Spawned gru do for issue #{} exited early with {} (after {:.1}s) — restoring label",
                                meta.issue_number,
                                status,
                                elapsed.as_secs_f64()
                            );
                            if let Err(e) = github::edit_labels_via_cli(
                                &meta.host,
                                &meta.owner,
                                &meta.repo,
                                meta.issue_number,
                                &[&meta.ready_label],
                                &[labels::IN_PROGRESS],
                            )
                            .await
                            {
                                log::warn!(
                                    "⚠️  Failed to restore labels on issue #{}: {} \
                                     — issue may need manual label fix",
                                    meta.issue_number,
                                    e
                                );
                            }
                        } else {
                            log::warn!(
                                "⚠️  Spawned gru do for issue #{} exited with {} (after {:.1}s) — \
                                 not restoring label (exceeded early-exit threshold of {}s)",
                                meta.issue_number,
                                status,
                                elapsed.as_secs_f64(),
                                EARLY_EXIT_THRESHOLD.as_secs()
                            );
                        }
                    }
                }

                children.swap_remove(i);
            }
            Ok(None) => {
                i += 1; // Still running
            }
            Err(e) => {
                log::warn!("Failed to check child process status: {}", e);
                i += 1;
            }
        }
    }
}

/// Handle child processes on lab shutdown.
///
/// When `stop_minions` is false (default), detaches from running children and lets them
/// continue independently. When `stop_minions` is true, sends SIGTERM then SIGKILL.
async fn shutdown_children(children: &mut [SpawnedChild], stop_minions: bool) {
    if children.is_empty() {
        return;
    }

    // Reap already-exited children first for an accurate running count
    let mut running_pids = Vec::new();
    for sc in children.iter_mut() {
        match sc.child.try_wait() {
            Ok(Some(status)) => {
                log::info!("Minion process already exited with status: {}", status);
            }
            Ok(None) => {
                if let Some(pid) = sc.child.id() {
                    running_pids.push(pid);
                }
            }
            Err(e) => {
                log::warn!("Failed to check child process status: {}", e);
            }
        }
    }

    if running_pids.is_empty() {
        println!("No running Minion processes.");
        return;
    }

    if !stop_minions {
        // Default: detach from children, let them continue running independently
        println!(
            "👋 {} Minion(s) still running — they will continue independently.",
            running_pids.len()
        );
        println!("   Use `gru status` to check on them, or `gru stop <id>` to stop one.");
        return;
    }

    // --stop-minions: kill all children
    println!(
        "🔪 Signaling {} running Minion(s) to shut down...",
        running_pids.len()
    );

    // Send SIGTERM to all still-running children
    for pid in &running_pids {
        #[cfg(unix)]
        {
            // SAFETY: kill with SIGTERM is safe - it requests graceful termination.
            // The PID is valid because we just obtained it from the child handle.
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
    }

    // Wait up to 5 seconds for graceful shutdown, polling every 100ms
    println!("⏳ Waiting for Minions to exit...");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let all_exited = children
            .iter_mut()
            .all(|sc| matches!(sc.child.try_wait(), Ok(Some(_))));
        if all_exited {
            println!("All Minions exited gracefully.");
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    // Force-kill any remaining processes and reap to avoid zombies
    for sc in children.iter_mut() {
        match sc.child.try_wait() {
            Ok(Some(_)) => {} // Already exited
            _ => {
                log::warn!("Force-killing Minion process that didn't exit gracefully");
                let _ = sc.child.kill().await;
                // Reap the child process to avoid leaving a zombie
                let _ = sc.child.wait().await;
            }
        }
    }
}

/// Information about a resumable minion found in the registry
struct ResumableMinion {
    minion_id: String,
    info: MinionInfo,
}

/// Build the set of `owner/repo` strings that this Lab instance monitors.
fn configured_repos(config: &LabConfig) -> HashSet<String> {
    config
        .daemon
        .repos
        .iter()
        .filter_map(|spec| {
            let (_host, owner, repo) = parse_repo_entry_with_hosts(spec, &config.github_hosts)?;
            Some(format!("{}/{}", owner, repo))
        })
        .collect()
}

/// Resolve the host for a given `owner/repo` from the Lab config.
/// Returns `None` if the repo is not in the config.
fn host_for_repo(config: &LabConfig, owner_repo: &str) -> Option<String> {
    for spec in &config.daemon.repos {
        if let Some((host, owner, repo)) = parse_repo_entry_with_hosts(spec, &config.github_hosts) {
            if format!("{}/{}", owner, repo) == owner_repo {
                return Some(host);
            }
        }
    }
    None
}

/// Minimum time between GitHub API calls for the review wake-up scan per minion.
const WAKE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Returns the IDs of Completed minions eligible for a review wake-up check.
///
/// A candidate must:
/// - Be in the `Completed` phase
/// - Have a PR number (no point polling if there's no PR)
/// - Not exceed `max_attempts` (bounded autonomy)
/// - Not already be in `MonitoringPr` (prevents double-flip)
pub(crate) fn find_wake_candidates(
    minions: &[(String, MinionInfo)],
    max_attempts: u32,
) -> Vec<String> {
    minions
        .iter()
        .filter(|(_id, info)| {
            info.orchestration_phase == OrchestrationPhase::Completed
                && info.pr.is_some()
                && info.attempt_count < max_attempts
        })
        .map(|(id, _info)| id.clone())
        .collect()
}

/// Returns true if a Completed minion should be woken up based on PR and review state.
///
/// All conditions must hold:
/// - `pr_open`: the PR is still open (not merged or closed)
/// - `unaddressed_reviews > 0`: there are new external reviews to address
/// - cooldown has elapsed since `last_check` (rate-limits GitHub API calls)
pub(crate) fn should_wake_minion(
    pr_open: bool,
    unaddressed_reviews: usize,
    last_check: DateTime<Utc>,
    cooldown: Duration,
) -> bool {
    if !pr_open || unaddressed_reviews == 0 {
        return false;
    }
    Utc::now()
        .signed_duration_since(last_check)
        .to_std()
        .map(|elapsed| elapsed >= cooldown)
        .unwrap_or(false)
}

/// Scan Completed minions for open PRs with new external reviews, and flip them back
/// to `MonitoringPr` so the resume chain picks them up.
///
/// `wake_check_times` is an in-memory map of minion_id → last GitHub API poll time,
/// used to enforce `WAKE_COOLDOWN` across poll cycles without persisting to disk.
async fn find_minions_needing_review_wake(
    config: &LabConfig,
    max_attempts: u32,
    wake_check_times: &mut HashMap<String, DateTime<Utc>>,
    resumed_this_session: &mut HashSet<String>,
) -> Result<()> {
    let repos = configured_repos(config);

    let all_minions: Vec<(String, MinionInfo)> = with_registry(|reg| Ok(reg.list())).await?;

    let candidate_ids = find_wake_candidates(&all_minions, max_attempts);
    if candidate_ids.is_empty() {
        return Ok(());
    }

    for minion_id in candidate_ids {
        let info = match all_minions.iter().find(|(id, _)| id == &minion_id) {
            Some((_, info)) => info.clone(),
            None => continue,
        };

        if !repos.contains(&info.repo) {
            continue;
        }

        if resumed_this_session.contains(&minion_id) {
            continue;
        }

        let last_check = wake_check_times
            .get(&minion_id)
            .copied()
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default());

        // Skip if the cooldown window hasn't elapsed (avoids burning GitHub API quota).
        let elapsed = Utc::now()
            .signed_duration_since(last_check)
            .to_std()
            .unwrap_or(Duration::ZERO);
        if elapsed < WAKE_COOLDOWN {
            log::debug!(
                "Skipping wake check for {} (cooldown: {:.0?} remaining)",
                minion_id,
                WAKE_COOLDOWN.saturating_sub(elapsed)
            );
            continue;
        }

        let pr_number = match &info.pr {
            Some(p) => p.clone(),
            None => continue,
        };

        let host = match host_for_repo(config, &info.repo) {
            Some(h) => h,
            None => continue,
        };

        let (owner, repo_name) = match info.repo.split_once('/') {
            Some((o, r)) => (o.to_string(), r.to_string()),
            None => continue,
        };

        // Fetch PR open/author info and all reviews in parallel-ish sequence.
        let pr_info = match pr_monitor::get_pr_info_for_exit_notification(
            &host, &owner, &repo_name, &pr_number,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Failed to get PR info for {} (PR #{}): {}",
                    minion_id,
                    pr_number,
                    e
                );
                continue;
            }
        };
        let (pr_open, pr_author) = pr_info;

        let reviews = match pr_monitor::get_all_reviews(&host, &owner, &repo_name, &pr_number).await
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Failed to fetch reviews for {} (PR #{}): {}",
                    minion_id,
                    pr_number,
                    e
                );
                continue;
            }
        };

        // Record that we performed a check for this minion, regardless of outcome.
        wake_check_times.insert(minion_id.clone(), Utc::now());

        let since = info.last_review_check_time.unwrap_or(info.started_at);
        let unaddressed = pr_monitor::has_unaddressed_reviews(&reviews, &pr_author, since);

        if !should_wake_minion(pr_open, unaddressed, last_check, WAKE_COOLDOWN) {
            log::debug!(
                "No wake needed for {} (pr_open={}, unaddressed={})",
                minion_id,
                pr_open,
                unaddressed
            );
            continue;
        }

        println!(
            "🔔 Waking up {} (issue #{}, {}): {} new external review(s) on PR #{}",
            minion_id, info.issue, info.repo, unaddressed, pr_number
        );

        let wake_reason = format!("Address the review comments on PR #{}", pr_number);
        let mid = minion_id.clone();
        if let Err(e) = with_registry(move |reg| {
            reg.update(&mid, |i| {
                i.orchestration_phase = OrchestrationPhase::MonitoringPr;
                i.wake_reason = Some(wake_reason.clone());
            })
        })
        .await
        {
            log::warn!(
                "Failed to update registry for wake-up of {}: {}",
                minion_id,
                e
            );
            continue;
        }

        // Clear from resumed_this_session so the existing resume chain picks it up.
        resumed_this_session.remove(&minion_id);
    }

    Ok(())
}

/// Scan the registry for minions that can be resumed.
///
/// A minion is resumable if:
/// - Its process is not running. Two cases are distinguished:
///
///   1. `mode == Stopped` — the minion was cleanly stopped; no PID is present.
///   2. Running-mode (`Autonomous`/`Interactive`) with a recorded PID that is now
///      dead — e.g. after SIGKILL before cleanup could run.
///
///   We require a PID to be present for case 2 to avoid a false positive during the
///   transient startup window: `check_and_claim_session` sets `mode = Autonomous` but
///   the outer lab hasn't written the PID yet. In that window `is_running()` returns
///   false (no PID ⟹ false), which would incorrectly flag the minion as dead.
///   Gating on `pid.is_some()` excludes that window.
/// - Its orchestration phase is active (RunningAgent, CreatingPr, or MonitoringPr)
/// - Its worktree still exists on disk
/// - Its repo is in the Lab config
async fn find_resumable_minions(config: &LabConfig) -> Result<Vec<ResumableMinion>> {
    let repos = configured_repos(config);
    with_registry(move |registry| {
        let resumable = registry
            .list()
            .into_iter()
            .filter(|(_id, info)| {
                let process_dead =
                    info.mode == MinionMode::Stopped || (info.pid.is_some() && !info.is_running());
                process_dead
                    && info.orchestration_phase.is_active()
                    && info.worktree.exists()
                    && repos.contains(&info.repo)
            })
            .map(|(minion_id, info)| ResumableMinion { minion_id, info })
            .collect();
        Ok(resumable)
    })
    .await
}

/// Mark a minion as failed in the registry and post an escalation comment on the issue.
async fn mark_exhausted_minion(minion_id: &str, info: &MinionInfo, host: &str, reason: &str) {
    let mid = minion_id.to_string();
    // Update registry to Failed
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |i| {
            i.orchestration_phase = OrchestrationPhase::Failed;
        })
    })
    .await
    {
        log::warn!(
            "Failed to mark minion {} as exhausted in registry: {}",
            minion_id,
            e
        );
    }

    // Post escalation comment and mark failed on GitHub
    let (owner, repo_name) = match info.repo.split_once('/') {
        Some(parts) => parts,
        None => return,
    };
    let comment = format!(
        "⚠️ **Minion {} failed: {}.**\n\n\
         Phase at failure: `{:?}`\n\n\
         This issue needs human attention.{}",
        minion_id,
        reason,
        info.orchestration_phase,
        crate::progress_comments::minion_signature(minion_id),
    );
    let _ = github::post_comment_via_cli(host, owner, repo_name, info.issue, &comment).await;
    let _ = github::mark_issue_failed_via_cli(host, owner, repo_name, info.issue).await;
}

/// Resume interrupted minions, filling available slots before new issue claims.
///
/// Returns the number of minions successfully resumed.
async fn resume_interrupted_minions(
    config: &LabConfig,
    children: &mut Vec<SpawnedChild>,
    available: &mut usize,
    max_attempts: u32,
    resumed_this_session: &mut HashSet<String>,
) -> Result<usize> {
    if *available == 0 {
        return Ok(0);
    }

    let resumable: Vec<_> = find_resumable_minions(config)
        .await?
        .into_iter()
        .filter(|c| {
            if resumed_this_session.contains(&c.minion_id) {
                log::debug!(
                    "Skipping {} (issue #{}, {}): already resumed this session",
                    c.minion_id,
                    c.info.issue,
                    c.info.repo,
                );
                false
            } else {
                true
            }
        })
        .collect();
    if resumable.is_empty() {
        return Ok(0);
    }

    println!(
        "🔄 Found {} resumable Minion(s) from previous session",
        resumable.len()
    );

    let mut resumed = 0;

    for candidate in resumable {
        if *available == 0 {
            break;
        }

        let host = match host_for_repo(config, &candidate.info.repo) {
            Some(h) => h,
            None => continue, // repo no longer in config
        };

        // Skip minions whose issue is already closed (PR merged or issue resolved)
        let (owner, repo_name) = match candidate.info.repo.split_once('/') {
            Some(parts) => parts,
            None => continue,
        };
        match github::is_issue_closed_via_cli(owner, repo_name, &host, candidate.info.issue).await {
            Ok(true) => {
                println!(
                    "⏭️  Skipping {} (issue #{}, {}): issue is closed",
                    candidate.minion_id, candidate.info.issue, candidate.info.repo,
                );
                // Mark as Completed in the registry
                let mid = candidate.minion_id.clone();
                if let Err(e) = with_registry(move |reg| {
                    reg.update(&mid, |info| {
                        info.orchestration_phase = OrchestrationPhase::Completed;
                    })
                })
                .await
                {
                    log::warn!("Failed to mark {} as completed: {}", candidate.minion_id, e);
                }
                continue;
            }
            Ok(false) => {} // Issue is still open, proceed with resume
            Err(e) => {
                // Transient failure (network, auth, etc.) — skip this cycle;
                // the candidate stays active so it will be retried next poll.
                log::warn!(
                    "⚠️  Failed to check issue state for {} (issue #{}): {} — will retry next poll",
                    candidate.minion_id,
                    candidate.info.issue,
                    e,
                );
                continue;
            }
        }

        // Skip minions whose timeout_deadline has passed
        if let Some(deadline) = candidate.info.timeout_deadline {
            if Utc::now() >= deadline {
                println!(
                    "⏭️  Skipping {} (issue #{}, {}): timeout_deadline has passed",
                    candidate.minion_id, candidate.info.issue, candidate.info.repo,
                );
                mark_exhausted_minion(
                    &candidate.minion_id,
                    &candidate.info,
                    &host,
                    "timeout deadline has passed",
                )
                .await;
                continue;
            }
        }

        // Skip minions that have exceeded max attempts
        if candidate.info.attempt_count > max_attempts {
            println!(
                "⏭️  Skipping {} (issue #{}, {}): attempt_count {} > max {}",
                candidate.minion_id,
                candidate.info.issue,
                candidate.info.repo,
                candidate.info.attempt_count,
                max_attempts,
            );
            let reason = format!("exceeded maximum resume attempts ({})", max_attempts);
            mark_exhausted_minion(&candidate.minion_id, &candidate.info, &host, &reason).await;
            continue;
        }

        println!(
            "♻️  Resuming {} (issue #{}, {}, phase: {:?})",
            candidate.minion_id,
            candidate.info.issue,
            candidate.info.repo,
            candidate.info.orchestration_phase,
        );

        // Record this minion as attempted regardless of outcome
        resumed_this_session.insert(candidate.minion_id.clone());

        match spawn_resume(&candidate.minion_id).await {
            Ok(child) => {
                // Write the `gru resume` PID to registry immediately to prevent
                // duplicate spawns. The inner agent subprocess will later
                // overwrite this with the agent PID via pid_callback.
                if let Some(pid) = child.id() {
                    let mid = candidate.minion_id.clone();
                    if let Err(e) = with_registry(move |registry| {
                        registry.update(&mid, |info| {
                            info.pid = Some(pid);
                            info.pid_start_time =
                                crate::minion_registry::get_process_start_time(pid);
                            info.mode = MinionMode::Autonomous;
                        })
                    })
                    .await
                    {
                        log::warn!(
                            "⚠️  Failed to write PID for resumed {}: {}",
                            candidate.minion_id,
                            e
                        );
                    }
                }
                children.push(SpawnedChild {
                    child,
                    spawn_meta: None, // Resumed minions don't need label restoration
                });
                resumed += 1;
                *available -= 1;
            }
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to resume {} for issue #{}: {}",
                    candidate.minion_id,
                    candidate.info.issue,
                    e
                );
            }
        }
    }

    if resumed > 0 {
        println!("✅ Resumed {} Minion(s)", resumed);
    }

    Ok(resumed)
}

/// Poll GitHub for ready issues and spawn Minions if slots are available
async fn poll_and_spawn(
    config: &LabConfig,
    children: &mut Vec<SpawnedChild>,
    no_resume: bool,
    resumed_this_session: &mut HashSet<String>,
    wake_check_times: &mut HashMap<String, DateTime<Utc>>,
) -> Result<()> {
    // Prune stale registry entries (worktrees that no longer exist, checking PR status)
    prune_stale_entries().await?;

    let max_attempts = config.daemon.max_resume_attempts;

    // Wake up Completed minions with new external reviews so they re-enter MonitoringPr.
    // Runs after prune_stale_entries so stale entries don't generate spurious wake-ups.
    if !no_resume {
        if let Err(e) = find_minions_needing_review_wake(
            config,
            max_attempts,
            wake_check_times,
            resumed_this_session,
        )
        .await
        {
            log::warn!("⚠️  Review wake scan error: {}", e);
        }
    }

    // Calculate available slots using PID liveness (not registry status string)
    let mut available = available_slots(config.daemon.max_slots).await?;

    if available == 0 {
        // All slots occupied, skip this poll
        return Ok(());
    }

    // Resume interrupted minions first, before claiming new issues
    let resumed = if no_resume {
        0
    } else {
        resume_interrupted_minions(
            config,
            children,
            &mut available,
            max_attempts,
            resumed_this_session,
        )
        .await?
    };
    let mut spawned = resumed;

    if available == 0 {
        if spawned > 0 {
            println!();
        }
        return Ok(());
    }

    // Poll each configured repository
    for repo_spec in &config.daemon.repos {
        if available == 0 {
            break;
        }

        // Parse owner/repo, host/owner/repo, or name:owner/repo
        let (host, owner, repo) = match parse_repo_entry_with_hosts(repo_spec, &config.github_hosts)
        {
            Some(parsed) => parsed,
            None => {
                log::warn!("⚠️  Invalid repo format: '{}', skipping", repo_spec);
                continue;
            }
        };
        // Canonical owner/repo form for registry lookups and issue URL building
        let repo_full = format!("{}/{}", owner, repo);

        // Fetch ready issues, excluding blocked ones (both GitHub-blocked and gru:blocked).
        // Try CLI first (supports -is:blocked qualifier), fall back to simpler CLI query
        // with client-side filtering.
        let candidates =
            match list_ready_issues_via_cli(&owner, &repo, &host, &config.daemon.label).await {
                Ok(issues) => issues,
                Err(cli_err) => {
                    log::warn!(
                        "⚠️  CLI issue fetch failed for {}: {}, trying basic CLI fallback",
                        repo_spec,
                        cli_err
                    );
                    match fallback_list_issues(&owner, &repo, &host, &config.daemon.label).await {
                        Ok(issues) => issues,
                        Err(e) => {
                            log::warn!("⚠️  Fallback also failed for {}: {}", repo_spec, e);
                            continue;
                        }
                    }
                }
            };

        let candidate_count = candidates.len();
        let mut blocked_count = 0usize;
        let mut spawned_this_repo = 0usize;

        // Try to spawn a Minion for each ready issue
        for candidate in candidates {
            let issue_number = candidate.number;
            if available == 0 {
                break;
            }

            // Check if issue is already being worked on (by a live process)
            if is_issue_claimed(&repo_full, issue_number).await? {
                continue;
            }

            // Check if issue has unresolved dependencies (body parsing + API verify)
            let body = candidate.body.as_deref().unwrap_or("");
            let blockers =
                crate::dependencies::get_blockers(&host, &owner, &repo, issue_number, body).await;
            if !blockers.is_empty() {
                let blocker_list: Vec<String> = blockers.iter().map(|n| format!("#{n}")).collect();
                log::info!(
                    "⏭️  Skipping issue #{}: blocked by {}",
                    issue_number,
                    blocker_list.join(", ")
                );
                blocked_count += 1;
                continue;
            }

            // Try to claim the issue via CLI
            match github::claim_issue_via_cli(
                &host,
                &owner,
                &repo,
                issue_number,
                &config.daemon.label,
            )
            .await
            {
                Ok(()) => {
                    // Successfully claimed, spawn Minion
                    match spawn_minion(&repo_full, &host, issue_number).await {
                        Ok(child) => {
                            // Write PID to registry immediately (if the subprocess has
                            // already created the entry) to prevent duplicate spawns.
                            if let Some(pid) = child.id() {
                                let repo_cl = repo_full.clone();
                                if let Err(e) = with_registry(move |registry| {
                                    let entries = registry.find_by_issue(&repo_cl, issue_number);
                                    if entries.is_empty() {
                                        log::debug!(
                                            "No registry entry yet for issue #{} — \
                                             subprocess will register its own PID",
                                            issue_number
                                        );
                                    }
                                    for (mid, _) in entries {
                                        registry.update(&mid, |info| {
                                            info.pid = Some(pid);
                                            info.pid_start_time =
                                                crate::minion_registry::get_process_start_time(pid);
                                            info.mode = MinionMode::Autonomous;
                                        })?;
                                    }
                                    Ok(())
                                })
                                .await
                                {
                                    log::warn!(
                                        "⚠️  Failed to write PID for new spawn on issue #{}: {}",
                                        issue_number,
                                        e
                                    );
                                }
                            }
                            children.push(SpawnedChild {
                                child,
                                spawn_meta: Some(SpawnMeta {
                                    host: host.clone(),
                                    owner: owner.clone(),
                                    repo: repo.clone(),
                                    issue_number,
                                    ready_label: config.daemon.label.clone(),
                                    spawned_at: Instant::now(),
                                }),
                            });
                            println!(
                                "✨ Spawned Minion for {}/issues/{}",
                                repo_spec, issue_number
                            );
                            spawned += 1;
                            spawned_this_repo += 1;
                            available -= 1; // Decrement available slots after successful spawn
                        }
                        Err(e) => {
                            log::warn!(
                                "⚠️  Failed to spawn Minion for {}/issues/{}: {}",
                                repo_spec,
                                issue_number,
                                e
                            );
                            // Unclaim the issue since we failed to spawn: remove in-progress, restore ready label
                            if let Err(e) = github::edit_labels_via_cli(
                                &host,
                                &owner,
                                &repo,
                                issue_number,
                                &[&config.daemon.label],
                                &[labels::IN_PROGRESS],
                            )
                            .await
                            {
                                log::warn!(
                                    "⚠️  Failed to restore labels on issue #{}: {} \
                                     — issue may need manual label fix",
                                    issue_number,
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "⚠️  Failed to claim issue {}/issues/{}: {}",
                        repo_spec,
                        issue_number,
                        e
                    );
                    continue;
                }
            }
        }

        // Log when no candidates were spawned and some were blocked
        if candidate_count > 0 && spawned_this_repo == 0 && blocked_count > 0 {
            log::warn!(
                "🚫 {}/{} candidate issue(s) in {} blocked by dependencies — nothing spawned this cycle",
                blocked_count,
                candidate_count,
                repo_spec
            );
        }
    }

    if spawned > 0 {
        println!();
    }

    Ok(())
}

/// Prune stale registry entries where worktrees no longer exist.
///
/// Delegates to the shared two-phase pruning in `minion_registry` which
/// checks GitHub PR status before removing entries with open PRs.
async fn prune_stale_entries() -> Result<()> {
    crate::minion_registry::prune_stale_entries().await?;
    Ok(())
}

/// Calculate available slots based on PID liveness of registered Minions
async fn available_slots(max_slots: usize) -> Result<usize> {
    let active_count = with_registry(move |registry| {
        let active = registry
            .list()
            .iter()
            .filter(|(_id, info)| info.is_running())
            .count();
        Ok(active)
    })
    .await?;

    Ok(max_slots.saturating_sub(active_count))
}

/// Check if an issue is already being worked on by a live Minion process
async fn is_issue_claimed(repo: &str, issue_number: u64) -> Result<bool> {
    let repo = repo.to_string();
    with_registry(move |registry| {
        let claimed = registry.list().iter().any(|(_id, info)| {
            info.repo == repo && info.issue == issue_number && info.is_running()
        });
        Ok(claimed)
    })
    .await
}

/// Spawn a Minion to work on an issue using the `gru do` command.
/// Returns the child process handle for lifecycle tracking.
async fn spawn_minion(repo: &str, host: &str, issue_number: u64) -> Result<Child> {
    let issue_ref = crate::github::build_issue_url_with_host(repo, host, issue_number)
        .with_context(|| format!("Invalid repo format: '{}'", repo))?;

    // Get the current executable path
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    // Create log directory and open log file for this minion's output
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    let log_dir = home.join(".gru").join("state").join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    // Include host in log filename to avoid collisions when the same owner/repo
    // exists on different hosts (e.g., github.com/org/svc vs ghe.corp.com/org/svc).
    // Sanitize by replacing any non-alphanumeric characters with hyphens.
    let safe_host = if host == "github.com" {
        String::new()
    } else {
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{}-", sanitized)
    };
    let safe_repo = repo.replace('/', "-");
    let log_path = log_dir.join(format!(
        "{}{}-issue-{}.log",
        safe_host, safe_repo, issue_number
    ));
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?
        .try_into_std()
        .expect("no in-flight I/O immediately after open");
    let stdout_file = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;
    let stderr_file = log_file;

    // Spawn `gru do <issue>` as a background process with output captured to log file.
    // Remove TMUX/TMUX_PANE so the child doesn't inherit the lab's tmux session —
    // otherwise TmuxGuard renames arbitrary windows in the parent's tmux.
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("do")
        .arg(&issue_ref)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    // Give the child its own session so SIGINT from Ctrl-C (sent to the terminal's
    // foreground process group) is not delivered to the child. This allows the lab
    // to shut down without killing running Minions.
    #[cfg(unix)]
    unsafe {
        // SAFETY: setsid() is async-signal-safe.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().context("Failed to spawn gru do command")?;

    // Give the process a moment to fail if there are startup issues
    // This prevents phantom slot occupancy from processes that immediately fail
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!(
            "Spawned process for {} exited immediately with status: {:?}",
            issue_ref,
            status
        );
    }

    println!("📝 Log: {}", log_path.display());

    Ok(child)
}

/// Spawn a resume for an existing Minion using `gru resume <minion_id>`.
/// Returns the child process handle for lifecycle tracking.
async fn spawn_resume(minion_id: &str) -> Result<Child> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    // Create log directory and open log file
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    let log_dir = home.join(".gru").join("state").join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let log_path = log_dir.join(format!("resume-{}.log", minion_id));
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?
        .try_into_std()
        .expect("no in-flight I/O immediately after open");
    let stdout_file = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;
    let stderr_file = log_file;

    let mut child = tokio::process::Command::new(exe)
        .arg("resume")
        .arg(minion_id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("Failed to spawn gru resume command")?;

    // Give the process a moment to fail if there are startup issues
    tokio::time::sleep(Duration::from_millis(100)).await;

    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!(
            "Spawned gru resume for {} exited immediately with status: {:?}",
            minion_id,
            status
        );
    }

    println!("📝 Log: {}", log_path.display());

    Ok(child)
}

/// Fallback issue listing using gh CLI with basic label filter.
/// Used when the primary `list_ready_issues_via_cli` call fails.
/// Uses a simpler gh CLI invocation with just the label filter.
async fn fallback_list_issues(
    owner: &str,
    repo: &str,
    host: &str,
    label: &str,
) -> Result<Vec<github::CandidateIssue>> {
    let repo_full = format!("{}/{}", owner, repo);
    let output = github::gh_cli_command(host)
        .args([
            "issue",
            "list",
            "--repo",
            &repo_full,
            "--label",
            label,
            "--state",
            "open",
            "--json",
            "number,body,labels",
            "--limit",
            "100",
        ])
        .output()
        .await
        .context("Failed to execute gh issue list command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh issue list failed for {}: {}", repo_full, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list output")?;

    let filtered: Vec<github::CandidateIssue> = issues
        .into_iter()
        .filter(|issue| {
            let label_names: Vec<String> = issue["labels"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            !labels::has_label(&label_names, labels::BLOCKED)
                && !labels::has_label(&label_names, labels::IN_PROGRESS)
        })
        .filter_map(|issue| {
            let number = issue["number"].as_u64()?;
            let body = issue["body"].as_str().map(String::from);
            Some(github::CandidateIssue { number, body })
        })
        .collect();

    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minion_registry::is_process_alive;

    #[tokio::test]
    async fn test_reap_children_empty() {
        let mut children: Vec<SpawnedChild> = Vec::new();
        reap_children(&mut children).await;
        assert!(children.is_empty());
    }

    #[test]
    fn test_log_path_github_com() {
        let safe_repo = "owner/repo".replace('/', "-");
        let log_name = format!("{}-issue-{}.log", safe_repo, 42);
        assert_eq!(log_name, "owner-repo-issue-42.log");
    }

    #[test]
    fn test_log_path_ghe_includes_host() {
        let host = "ghe.netflix.net";
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let safe_host = format!("{}-", sanitized);
        let safe_repo = "corp/service".replace('/', "-");
        let log_name = format!("{}{}-issue-{}.log", safe_host, safe_repo, 42);
        assert_eq!(log_name, "ghe-netflix-net-corp-service-issue-42.log");
    }

    #[test]
    fn test_log_path_host_with_port_is_sanitized() {
        let host = "ghe.example.com:8443";
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        assert_eq!(sanitized, "ghe-example-com-8443");
    }

    // --- issue URL construction tests ---

    #[test]
    fn test_issue_ref_builds_full_github_url() {
        let url =
            crate::github::build_issue_url_with_host("fotoetienne/gru", "github.com", 42).unwrap();
        assert_eq!(url, "https://github.com/fotoetienne/gru/issues/42");
    }

    #[test]
    fn test_issue_ref_builds_ghe_url() {
        let url =
            crate::github::build_issue_url_with_host("corp/some-service", "ghe.netflix.net", 99)
                .unwrap();
        assert_eq!(url, "https://ghe.netflix.net/corp/some-service/issues/99");
    }

    #[test]
    fn test_build_issue_url_with_host_rejects_invalid() {
        assert!(crate::github::build_issue_url_with_host("", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("justrepo", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("/repo", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("owner/", "github.com", 1).is_none());
    }

    // --- configured_repos / host_for_repo tests ---

    fn test_lab_config(repos: Vec<&str>) -> LabConfig {
        let mut config = LabConfig::default();
        config.daemon.repos = repos.into_iter().map(String::from).collect();
        config.daemon.max_slots = 2;
        config
    }

    #[test]
    fn test_configured_repos_basic() {
        let config = test_lab_config(vec!["owner/repo1", "owner/repo2"]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains("owner/repo1"));
        assert!(repos.contains("owner/repo2"));
    }

    #[test]
    fn test_configured_repos_with_host() {
        let config = test_lab_config(vec!["ghe.corp.com/owner/repo1", "owner/repo2"]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains("owner/repo1"));
        assert!(repos.contains("owner/repo2"));
    }

    #[test]
    fn test_configured_repos_skips_invalid() {
        let config = test_lab_config(vec!["owner/repo1", "invalid", ""]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 1);
        assert!(repos.contains("owner/repo1"));
    }

    #[test]
    fn test_host_for_repo_github_com() {
        let config = test_lab_config(vec!["owner/repo1"]);
        assert_eq!(
            host_for_repo(&config, "owner/repo1"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_host_for_repo_ghe() {
        let config = test_lab_config(vec!["ghe.corp.com/owner/repo1"]);
        assert_eq!(
            host_for_repo(&config, "owner/repo1"),
            Some("ghe.corp.com".to_string())
        );
    }

    #[test]
    fn test_host_for_repo_not_found() {
        let config = test_lab_config(vec!["owner/repo1"]);
        assert_eq!(host_for_repo(&config, "other/repo"), None);
    }

    #[test]
    fn test_max_resume_attempts_default() {
        let config = LabConfig::default();
        assert_eq!(config.daemon.max_resume_attempts, 3);
    }

    #[tokio::test]
    async fn test_reap_children_removes_exited_process() {
        // Spawn a process that exits immediately with code 1
        let child = tokio::process::Command::new("false")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut children = vec![SpawnedChild {
            child,
            spawn_meta: None,
        }];

        // Wait for process to exit
        tokio::time::sleep(Duration::from_millis(100)).await;

        reap_children(&mut children).await;
        assert!(children.is_empty(), "Exited child should be reaped");
    }

    #[tokio::test]
    async fn test_reap_children_keeps_running_process() {
        // Spawn a process that sleeps for a while
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut children = vec![SpawnedChild {
            child,
            spawn_meta: None,
        }];

        reap_children(&mut children).await;
        assert_eq!(children.len(), 1, "Running child should not be reaped");

        // Clean up
        children[0].child.kill().await.ok();
        children[0].child.wait().await.ok();
    }

    #[test]
    fn test_spawn_meta_tracks_issue_metadata() {
        let meta = SpawnMeta {
            host: "github.com".to_string(),
            owner: "test-owner".to_string(),
            repo: "test-repo".to_string(),
            issue_number: 42,
            ready_label: "gru:todo".to_string(),
            spawned_at: Instant::now(),
        };
        assert_eq!(meta.issue_number, 42);
        assert!(meta.spawned_at.elapsed() <= EARLY_EXIT_THRESHOLD);
    }

    #[test]
    fn test_should_restore_label_within_threshold() {
        // A just-spawned process should qualify for label restoration
        let spawned_at = Instant::now();
        assert!(
            should_restore_label(spawned_at),
            "Process that just spawned should qualify for label restoration"
        );
    }

    #[test]
    fn test_should_restore_label_beyond_threshold() {
        // A process spawned well before the threshold should not qualify
        let spawned_at = Instant::now() - Duration::from_secs(60);
        assert!(
            !should_restore_label(spawned_at),
            "Process spawned 60s ago should not qualify for label restoration"
        );
    }

    #[tokio::test]
    async fn test_shutdown_children_detach_leaves_process_running() {
        // Spawn a process that sleeps
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let pid = child.id().unwrap();

        let mut children = vec![SpawnedChild {
            child,
            spawn_meta: None,
        }];

        // Default shutdown (detach mode) should NOT kill the process
        shutdown_children(&mut children, false).await;

        // Process should still be alive
        assert!(
            is_process_alive(pid),
            "Process should still be running after detach shutdown"
        );

        // Clean up: kill and reap the process to avoid leaving a zombie
        children[0].child.kill().await.ok();
        children[0].child.wait().await.ok();
    }

    #[tokio::test]
    async fn test_shutdown_children_stop_kills_process() {
        // Spawn a process that sleeps
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let pid = child.id().unwrap();

        let mut children = vec![SpawnedChild {
            child,
            spawn_meta: None,
        }];

        // Stop mode should kill the process
        shutdown_children(&mut children, true).await;

        // Give a moment for the process to be reaped
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Process should be dead
        assert!(
            !is_process_alive(pid),
            "Process should be dead after stop shutdown"
        );
    }

    #[test]
    fn test_should_restore_label_just_under_threshold() {
        // A process spawned just under the threshold should still qualify
        let spawned_at = Instant::now() - EARLY_EXIT_THRESHOLD + Duration::from_secs(1);
        assert!(
            should_restore_label(spawned_at),
            "Process at 1s under threshold should still qualify for label restoration"
        );
    }

    // --- find_wake_candidates tests ---

    fn make_completed_minion(pr: Option<&str>, attempt_count: u32) -> MinionInfo {
        use crate::minion_registry::{MinionMode, OrchestrationPhase};
        use std::path::PathBuf;
        MinionInfo {
            repo: "owner/repo".to_string(),
            issue: 42,
            command: "do".to_string(),
            prompt: "test".to_string(),
            started_at: chrono::Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: pr.map(String::from),
            session_id: uuid::Uuid::new_v4().to_string(),
            pid: None,
            pid_start_time: None,
            mode: MinionMode::Stopped,
            last_activity: chrono::Utc::now(),
            orchestration_phase: OrchestrationPhase::Completed,
            token_usage: None,
            agent_name: "claude".to_string(),
            timeout_deadline: None,
            attempt_count,
            no_watch: false,
            last_review_check_time: None,
            wake_reason: None,
        }
    }

    #[test]
    fn test_find_wake_candidates_empty_registry() {
        let minions: Vec<(String, MinionInfo)> = vec![];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(candidates.is_empty(), "Empty registry yields no candidates");
    }

    #[test]
    fn test_find_wake_candidates_skips_minions_without_prs() {
        let info = make_completed_minion(None, 0);
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Minion without a PR must not be a candidate"
        );
    }

    #[test]
    fn test_find_wake_candidates_skips_over_max_attempts() {
        let info = make_completed_minion(Some("10"), 3);
        let minions = vec![("M001".to_string(), info)];
        // max_attempts=3 means attempt_count must be < 3; attempt_count=3 is over limit
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Minion at max_attempts must not be a candidate"
        );
    }

    #[test]
    fn test_find_wake_candidates_skips_non_completed_phases() {
        use crate::minion_registry::OrchestrationPhase;
        let mut info = make_completed_minion(Some("10"), 0);
        // MonitoringPr is an active phase, not Completed — must be skipped
        info.orchestration_phase = OrchestrationPhase::MonitoringPr;
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Minion in MonitoringPr phase must not be a candidate (prevents double-flip)"
        );
    }

    #[test]
    fn test_find_wake_candidates_returns_eligible_minion() {
        let info = make_completed_minion(Some("10"), 0);
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert_eq!(candidates, vec!["M001"]);
    }

    // --- should_wake_minion tests ---

    #[test]
    fn test_should_wake_minion_false_for_closed_pr() {
        let old_check = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(
            !should_wake_minion(false, 2, old_check, Duration::from_secs(300)),
            "Closed/merged PR must never trigger wake-up"
        );
    }

    #[test]
    fn test_should_wake_minion_false_for_no_reviews() {
        let old_check = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(
            !should_wake_minion(true, 0, old_check, Duration::from_secs(300)),
            "Open PR with zero unaddressed reviews must not trigger wake-up"
        );
    }

    #[test]
    fn test_should_wake_minion_respects_cooldown() {
        // last_check is very recent — cooldown not elapsed
        let recent_check = chrono::Utc::now() - chrono::Duration::seconds(10);
        assert!(
            !should_wake_minion(true, 1, recent_check, Duration::from_secs(300)),
            "Wake-up must be suppressed when within cooldown window"
        );
    }

    #[test]
    fn test_should_wake_minion_true_when_all_conditions_met() {
        // PR open, reviews pending, cooldown elapsed
        let old_check = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(
            should_wake_minion(true, 1, old_check, Duration::from_secs(300)),
            "Wake-up must trigger when PR is open, reviews pending, and cooldown elapsed"
        );
    }
}
