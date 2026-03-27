use crate::config::{parse_repo_entry_with_hosts, LabConfig};
use crate::github::{self, list_ready_issues_via_cli};
use crate::labels;
use crate::minion_registry::{with_registry, MinionInfo, MinionMode, OrchestrationPhase};
use crate::pr_monitor;
use crate::retry_queue::RetryQueue;
use crate::tmux::TmuxGuard;
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::Notify;
use tokio::time::sleep;

/// Prints a timestamped message to stdout in `[HH:MM:SS] <message>` format.
macro_rules! tprintln {
    () => { println!() };
    ($($arg:tt)*) => {
        println!("[{}] {}", Local::now().format("%H:%M:%S"), format_args!($($arg)*))
    };
}

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

/// Metadata needed to restore labels when a spawned process exits early
/// and to enqueue retries for non-early failures.
struct SpawnMeta {
    host: String,
    owner: String,
    repo: String,
    issue_number: u64,
    ready_label: String,
    spawned_at: Instant,
    /// Retry attempt counter — carried through from RetryEntry so the retry
    /// queue can track attempts without depending on the registry.
    retry_attempt: u32,
}

/// Determines whether a failed spawn qualifies for label restoration.
/// Returns true if the process exited within the early-exit threshold, meaning it
/// likely never started meaningful work and the issue should be returned to the ready state.
fn should_restore_label(spawned_at: Instant) -> bool {
    spawned_at.elapsed() <= EARLY_EXIT_THRESHOLD
}

/// Handles the lab daemon command
pub(crate) async fn handle_lab(
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

    // Spawn non-blocking version check in background
    let mut version_rx = crate::version_check::spawn_version_check();

    tprintln!("🚀 Starting Gru Lab daemon");
    tprintln!(
        "📋 Monitoring {} repository(ies)",
        config.daemon.repos.len()
    );
    tprintln!("🔄 Poll interval: {}s", config.daemon.poll_interval_secs);
    tprintln!("🎰 Max concurrent slots: {}", config.daemon.max_slots);
    tprintln!("🏷️  Watching for label: {}", config.daemon.label);
    tprintln!();

    for repo in &config.daemon.repos {
        tprintln!("  • {}", repo);
    }

    // Print version notification if already available — never waits.
    crate::version_check::print_if_ready(&mut version_rx);

    tprintln!();
    tprintln!("Press Ctrl-C to stop...");
    tprintln!();

    // Track child processes for graceful shutdown
    let mut children: Vec<SpawnedChild> = Vec::new();

    // In-memory retry queue for failed/incomplete Minion runs
    let mut retry_queue = RetryQueue::new(
        config.daemon.max_retry_attempts,
        config.daemon.max_retry_backoff_secs,
    );

    // Safety net: track Minion IDs we've already attempted to resume this session
    // to prevent resume loops even if phase updates are missed.
    let mut resumed_this_session: HashSet<String> = HashSet::new();

    // Track the last time we polled each Completed minion for new reviews.
    // Not persisted — resets on daemon restart, which is fine since the cooldown
    // only serves to rate-limit GitHub API calls within a session.
    let mut wake_check_times: HashMap<String, DateTime<Utc>> = HashMap::new();

    if no_resume {
        tprintln!("⏭️  Auto-resume disabled (--no-resume)");
        tprintln!();
    }

    // Cooperative shutdown flag — set by background signal handlers so that
    // long-running loops (e.g. find_minions_needing_wake) can bail out
    // promptly instead of issuing API calls that will fail during teardown.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(Notify::new());

    // Background signal handlers that set the flag and wake the main loop.
    // Uses notify_one() so the permit is stored even if no one is waiting yet
    // (e.g. signal arrives during poll_and_spawn).
    {
        let flag = shutdown_flag.clone();
        let notify = shutdown_notify.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    flag.store(true, Ordering::Release);
                    notify.notify_one();
                }
                Err(e) => {
                    log::warn!("Failed to register SIGINT handler: {e} — CTRL-C will not trigger graceful shutdown");
                }
            }
        });
    }
    {
        let flag = shutdown_flag.clone();
        let notify = shutdown_notify.clone();
        tokio::spawn(async move {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    if sigterm.recv().await.is_some() {
                        flag.store(true, Ordering::Release);
                        notify.notify_one();
                    } else {
                        log::warn!("SIGTERM signal stream closed without receiving SIGTERM; not triggering graceful shutdown");
                    }
                }
                Err(e) => {
                    log::warn!("Failed to register SIGTERM handler: {e} — SIGTERM will not trigger graceful shutdown");
                }
            }
        });
    }

    // Perform initial poll immediately for faster feedback
    if let Err(e) = poll_and_spawn(
        &config,
        &mut children,
        &mut retry_queue,
        no_resume,
        &mut resumed_this_session,
        &mut wake_check_times,
        &shutdown_flag,
    )
    .await
    {
        log::warn!("⚠️  Initial polling error: {:#}", e);
        log::warn!("   Continuing to poll...");
    }

    // Adaptive backoff state
    let base_interval = config.poll_interval();
    let max_interval = config.poll_interval_max();
    let mut consecutive_idle_cycles: u32 = 0;

    // Compute backoff interval: base * 2^idle_cycles, capped at max.
    let backoff_interval = |cycles: u32| -> Duration {
        let multiplier = 2u32.saturating_pow(cycles);
        std::cmp::min(base_interval.saturating_mul(multiplier), max_interval)
    };

    // Main polling loop
    loop {
        if shutdown_flag.load(Ordering::Acquire) {
            tprintln!();
            tprintln!("🛑 Received shutdown signal, stopping daemon...");
            shutdown_children(&mut children, stop_minions).await;
            break;
        }

        let current_interval = backoff_interval(consecutive_idle_cycles);

        tokio::select! {
            biased;
            _ = shutdown_notify.notified() => {
                // Flag was already set by the background handler; fall through
                // to the top-of-loop check which prints the message and breaks.
                continue;
            }
            _ = sleep(current_interval) => {
                // Clean up finished child processes and enqueue retries for failures
                reap_children(&mut children, &mut retry_queue).await;

                match poll_and_spawn(
                    &config,
                    &mut children,
                    &mut retry_queue,
                    no_resume,
                    &mut resumed_this_session,
                    &mut wake_check_times,
                    &shutdown_flag,
                )
                .await
                {
                    Ok(result) if result.spawned > 0 => {
                        let prev = current_interval.as_secs();
                        consecutive_idle_cycles = 0;
                        log::debug!(
                            "Lab poll interval: {}s → {}s (activity detected, spawned {})",
                            prev,
                            base_interval.as_secs(),
                            result.spawned,
                        );
                    }
                    Ok(result) if result.slots_full => {
                        // All slots occupied — reset to base interval so we
                        // pick up new issues promptly when a slot frees up.
                        let prev = current_interval.as_secs();
                        consecutive_idle_cycles = 0;
                        log::debug!(
                            "Lab poll interval: {}s → {}s (slots full, reset backoff)",
                            prev,
                            base_interval.as_secs(),
                        );
                    }
                    Ok(_) => {
                        let prev = current_interval.as_secs();
                        consecutive_idle_cycles = consecutive_idle_cycles.saturating_add(1);
                        let next = backoff_interval(consecutive_idle_cycles);
                        log::debug!(
                            "Lab poll interval: {}s → {}s (idle for {} cycle{})",
                            prev,
                            next.as_secs(),
                            consecutive_idle_cycles,
                            if consecutive_idle_cycles == 1 { "" } else { "s" },
                        );
                    }
                    Err(e) => {
                        log::warn!("⚠️  Polling error: {:#}", e);
                        log::warn!("   Continuing to poll...");
                    }
                }

                // Log retry queue status when non-empty
                if !retry_queue.is_empty() {
                    log_retry_queue_status(&retry_queue);
                }

                // Check if a signal arrived during poll_and_spawn
                if shutdown_flag.load(Ordering::Acquire) {
                    tprintln!();
                    tprintln!("🛑 Received shutdown signal during polling, stopping daemon...");
                    shutdown_children(&mut children, stop_minions).await;
                    break;
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
async fn reap_children(children: &mut Vec<SpawnedChild>, retry_queue: &mut RetryQueue) {
    let mut i = 0;
    while i < children.len() {
        match children[i].child.try_wait() {
            Ok(Some(status)) => {
                log::info!("Minion process exited with status: {}", status);

                if let Some(meta) = &children[i].spawn_meta {
                    let elapsed = meta.spawned_at.elapsed();

                    if !status.success() {
                        if should_restore_label(meta.spawned_at) {
                            // Check if the issue already has a terminal label before
                            // restoring gru:todo — the minion may have finished
                            // (done or failed) before the process exited.
                            let terminal_label = match github::has_any_label_via_cli(
                                &meta.host,
                                &meta.owner,
                                &meta.repo,
                                meta.issue_number,
                                &[labels::DONE, labels::FAILED],
                            )
                            .await
                            {
                                Ok(label) => label,
                                Err(e) => {
                                    // Fail-open: proceed with restoration rather than
                                    // risk leaving the issue stuck in gru:in-progress.
                                    log::warn!(
                                        "⚠️  Failed to check labels on issue #{}: {} \
                                         — proceeding with label restoration (fail-open)",
                                        meta.issue_number,
                                        e
                                    );
                                    None
                                }
                            };

                            if let Some(label) = terminal_label {
                                log::info!(
                                    "⏭️  Issue #{} already has {} — skipping gru:todo restoration, \
                                     removing gru:in-progress only",
                                    meta.issue_number,
                                    label
                                );
                                // Still remove gru:in-progress so the issue doesn't
                                // end up with both a terminal label and in-progress.
                                if let Err(e) = github::edit_labels_via_cli(
                                    &meta.host,
                                    &meta.owner,
                                    &meta.repo,
                                    meta.issue_number,
                                    &[],
                                    &[labels::IN_PROGRESS],
                                )
                                .await
                                {
                                    log::warn!(
                                        "⚠️  Failed to remove gru:in-progress from issue #{}: {}",
                                        meta.issue_number,
                                        e
                                    );
                                }
                            } else {
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
                            }
                        } else {
                            // Non-early failure: enqueue for retry with backoff
                            let reason = format!(
                                "exited with {} after {:.0}s",
                                status,
                                elapsed.as_secs_f64()
                            );
                            if !retry_queue.enqueue_failure(
                                &meta.host,
                                &meta.owner,
                                &meta.repo,
                                meta.issue_number,
                                meta.retry_attempt,
                                &reason,
                                None,
                                None,
                            ) {
                                log::warn!(
                                    "⚠️  Issue #{} exceeded max retry attempts — not retrying",
                                    meta.issue_number,
                                );
                            }
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
        tprintln!("No running Minion processes.");
        return;
    }

    if !stop_minions {
        // Default: detach from children, let them continue running independently
        tprintln!(
            "👋 {} Minion(s) still running — they will continue independently.",
            running_pids.len()
        );
        tprintln!("   Use `gru status` to check on them, or `gru stop <id>` to stop one.");
        return;
    }

    // --stop-minions: kill all children
    tprintln!(
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
    tprintln!("⏳ Waiting for Minions to exit...");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let all_exited = children
            .iter_mut()
            .all(|sc| matches!(sc.child.try_wait(), Ok(Some(_))));
        if all_exited {
            tprintln!("All Minions exited gracefully.");
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
            Some(github::repo_slug(&owner, &repo))
        })
        .collect()
}

/// Resolve the host for a given `owner/repo` from the Lab config.
/// Returns `None` if the repo is not in the config.
fn host_for_repo(config: &LabConfig, owner_repo: &str) -> Option<String> {
    for spec in &config.daemon.repos {
        if let Some((host, owner, repo)) = parse_repo_entry_with_hosts(spec, &config.github_hosts) {
            if github::repo_slug(&owner, &repo) == owner_repo {
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
/// - Be in the `Completed` phase (this also implicitly prevents double-flip: a minion
///   already flipped to `MonitoringPr` will no longer match `== Completed`)
/// - Have a PR number (no point polling if there's no PR)
/// - Not exceed `max_attempts` (bounded autonomy)
/// - Be a "do" or "fix" command (review/prompt minions are one-shot and lack a PR monitoring lifecycle)
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
                && (info.command == "do" || info.command == "fix")
        })
        .map(|(id, _info)| id.clone())
        .collect()
}

/// Returns true if a Completed minion should be woken up based on PR and review state.
///
/// Conditions (any of the following, when `pr_open` is true):
/// - `unaddressed_reviews > 0`: there are new external reviews to address
/// - `has_merge_conflict`: GitHub reports the PR has merge conflicts (`mergeable == false`)
///
/// Note: cooldown rate-limiting is enforced separately via `within_wake_cooldown` before
/// any GitHub API calls are made. This function encapsulates only the per-minion wake
/// decision after PR info is fetched, keeping it a simple testable predicate.
pub(crate) fn should_wake_minion(
    pr_open: bool,
    unaddressed_reviews: usize,
    has_merge_conflict: bool,
) -> bool {
    pr_open && (unaddressed_reviews > 0 || has_merge_conflict)
}

/// Returns true if a minion is still within the wake cooldown window.
///
/// Used to rate-limit GitHub API calls per minion. `last_check` defaults to
/// `DateTime::UNIX_EPOCH` for minions that have never been checked.
pub(crate) fn within_wake_cooldown(
    last_check: DateTime<Utc>,
    now: DateTime<Utc>,
    cooldown: Duration,
) -> bool {
    let elapsed = now
        .signed_duration_since(last_check)
        .to_std()
        .unwrap_or(Duration::ZERO);
    elapsed < cooldown
}

/// Scan Completed minions for open PRs with new external reviews or merge conflicts,
/// and flip them back to `MonitoringPr` so the resume chain picks them up.
///
/// `wake_check_times` is an in-memory map of minion_id → last GitHub API poll time,
/// used to enforce `WAKE_COOLDOWN` across poll cycles without persisting to disk.
async fn find_minions_needing_wake(
    config: &LabConfig,
    max_attempts: u32,
    wake_check_times: &mut HashMap<String, DateTime<Utc>>,
    resumed_this_session: &mut HashSet<String>,
    shutdown_flag: &AtomicBool,
) -> Result<()> {
    let repos = configured_repos(config);

    let all_minions: Vec<(String, MinionInfo)> = with_registry(|reg| Ok(reg.list())).await?;

    let candidate_ids = find_wake_candidates(&all_minions, max_attempts);
    if candidate_ids.is_empty() {
        return Ok(());
    }

    for minion_id in candidate_ids {
        if shutdown_flag.load(Ordering::Acquire) {
            log::debug!("Shutdown requested, aborting review wake scan");
            return Ok(());
        }

        let info = match all_minions.iter().find(|(id, _)| id == &minion_id) {
            Some((_, info)) => info.clone(),
            None => continue,
        };

        if !repos.contains(&info.repo) {
            continue;
        }

        // NOTE: we intentionally do NOT check `resumed_this_session` here.
        // That set guards the resume chain (active-phase minions) from being
        // resumed twice in one session. Completed minions are never inserted
        // into it by the resume chain, so the guard would be a no-op for
        // first-time wake-ups. More critically, a minion that was resumed
        // earlier in the session, completed its work, and then received new
        // reviews would be incorrectly blocked from a second wake-up.
        // The resume chain's own `resumed_this_session.remove()` below handles
        // clearing the entry after the phase flip.

        let last_check = wake_check_times
            .get(&minion_id)
            .copied()
            .unwrap_or(DateTime::UNIX_EPOCH);

        // Skip if the cooldown window hasn't elapsed (avoids burning GitHub API quota).
        let now = Utc::now();
        if within_wake_cooldown(last_check, now, WAKE_COOLDOWN) {
            let elapsed = now
                .signed_duration_since(last_check)
                .to_std()
                .unwrap_or(Duration::ZERO);
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

        // Record the check time immediately so the cooldown applies even when API calls
        // fail — prevents hammering the GitHub API during transient outages.
        wake_check_times.insert(minion_id.clone(), Utc::now());

        // Fetch PR open/author/mergeable info and all reviews.
        let pr_info =
            match pr_monitor::get_pr_info_for_wake_check(&host, &owner, &repo_name, &pr_number)
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
        let (pr_open, _pr_author, mergeable) = pr_info;

        // Skip closed/merged PRs early to avoid unnecessary review API calls.
        if !pr_open {
            log::debug!(
                "Skipping {} — PR #{} is closed/merged",
                minion_id,
                pr_number
            );
            continue;
        }

        let has_merge_conflict = mergeable == Some(false);

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

        let since = info.last_review_check_time.unwrap_or(info.started_at);
        let unaddressed = pr_monitor::count_unaddressed_reviews(&reviews, since);

        if !should_wake_minion(pr_open, unaddressed, has_merge_conflict) {
            log::debug!(
                "No wake needed for {} (pr_open={}, unaddressed={}, conflict={})",
                minion_id,
                pr_open,
                unaddressed,
                has_merge_conflict
            );
            continue;
        }

        // Build a wake_reason that captures all triggers. This string is stored as
        // registry metadata for observability (visible in `gru status` and the JSON).
        // It is NOT sent to the agent as a prompt — conflict resolution is handled by
        // monitor_pr_lifecycle's MergeConflict re-detection path, and review handling
        // is driven by last_review_check_time.
        let issue_display = info.issue.map_or("?".to_string(), |n| n.to_string());
        let wake_reason = match (unaddressed > 0, has_merge_conflict) {
            (true, true) => {
                tprintln!(
                    "🔔 Waking up {} (issue #{}, {}): {} new review(s) + merge conflict on PR #{}",
                    minion_id,
                    issue_display,
                    info.repo,
                    unaddressed,
                    pr_number
                );
                format!(
                    "Address review comments and resolve merge conflicts on PR #{}",
                    pr_number
                )
            }
            (true, false) => {
                tprintln!(
                    "🔔 Waking up {} (issue #{}, {}): {} new external review(s) on PR #{}",
                    minion_id,
                    issue_display,
                    info.repo,
                    unaddressed,
                    pr_number
                );
                format!("Address the review comments on PR #{}", pr_number)
            }
            (false, true) => {
                tprintln!(
                    "🔔 Waking up {} (issue #{}, {}): merge conflict detected on PR #{}",
                    minion_id,
                    issue_display,
                    info.repo,
                    pr_number
                );
                format!("Rebase PR #{} to resolve merge conflicts", pr_number)
            }
            (false, false) => {
                // should_wake_minion returned true, so at least one trigger must be set.
                unreachable!("should_wake_minion returned true with no trigger");
            }
        };
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

        // Remove from resumed_this_session so the resume chain picks up the re-flipped
        // minion.  In practice a Completed minion should never be in this set (the resume
        // chain only inserts active-phase minions), but removing defensively ensures the
        // guard doesn't block a woken minion if that invariant ever breaks.
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
                // Require pid.is_some() for the non-Stopped path: is_running() returns false
                // when pid is None, which would incorrectly flag a minion as dead during the
                // transient startup window where check_and_claim_session has set mode =
                // Autonomous but the lab hasn't written the PID yet.
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

/// Mark a minion as Completed in the registry.
/// Used when a minion's issue is closed or its PR is merged/closed.
async fn mark_minion_completed(minion_id: &str) {
    let mid = minion_id.to_string();
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.orchestration_phase = OrchestrationPhase::Completed;
        })
    })
    .await
    {
        log::warn!("Failed to mark {} as completed: {:#}", minion_id, e);
    }
}

/// Decision returned by [`check_pr_merge_state`] indicating whether a minion
/// should be resumed, skipped (PR merged/closed), or retried next poll.
#[derive(Debug, PartialEq)]
enum PrResumeDecision {
    /// PR is still open (or no PR exists) — proceed with resume.
    Resume,
    /// PR is merged or closed — mark minion as completed and skip.
    SkipCompleted { pr_num: u64 },
    /// Transient error checking PR state — skip this cycle, retry next poll.
    RetryNextPoll { pr_num: u64, error: String },
}

/// Check whether a minion's associated PR has been merged or closed.
///
/// This is a pure decision function that takes the result of the GitHub API call
/// as a parameter, making it easy to test without mocking.
fn check_pr_merge_state(
    pr_field: Option<&str>,
    pr_open_result: impl FnOnce(u64) -> Result<bool>,
) -> PrResumeDecision {
    let pr_str = match pr_field {
        Some(s) => s,
        None => return PrResumeDecision::Resume,
    };
    let pr_num = match pr_str.parse::<u64>() {
        Ok(n) => n,
        Err(_) => {
            log::warn!("Unparseable PR value '{}', skipping PR check", pr_str,);
            return PrResumeDecision::Resume;
        }
    };
    match pr_open_result(pr_num) {
        Ok(true) => PrResumeDecision::Resume,
        Ok(false) => PrResumeDecision::SkipCompleted { pr_num },
        Err(e) => PrResumeDecision::RetryNextPoll {
            pr_num,
            error: e.to_string(),
        },
    }
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
    if let Some(issue_num) = info.issue {
        let _ = github::post_comment_via_cli(host, owner, repo_name, issue_num, &comment).await;
        let _ = github::mark_issue_failed_via_cli(host, owner, repo_name, issue_num).await;
    }
}

/// Sort candidates by minion ID descending (most recent first) and deduplicate by
/// `(repo, issue_number)`, keeping only the first (highest-ID) candidate per issue.
///
/// IDs are zero-padded base36 strings — `(length DESC, lowercased-string DESC)` gives the
/// correct numeric order and handles legacy uppercase IDs produced by older gru versions.
fn sort_and_dedup_resumable(mut candidates: Vec<ResumableMinion>) -> Vec<ResumableMinion> {
    candidates.sort_by_cached_key(|c| {
        let key = c.minion_id.to_ascii_lowercase();
        std::cmp::Reverse((c.minion_id.len(), key))
    });

    let mut seen: HashSet<(String, Option<u64>)> = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert((c.info.repo.clone(), c.info.issue)))
        .collect()
}

/// Determine whether a resume candidate should be skipped, and handle side-effects
/// (marking closed issues as completed, marking exhausted minions as failed).
///
/// Returns `true` if the candidate should be resumed, `false` to skip.
///
/// All errors (registry access, GitHub API) are handled internally by logging
/// and returning `false` — a single candidate failure should never abort the
/// entire resume loop.
async fn should_resume_candidate(
    candidate: &ResumableMinion,
    host: &str,
    max_attempts: u32,
) -> bool {
    let issue_display = candidate
        .info
        .issue
        .map_or("?".to_string(), |n| n.to_string());

    // Cross-cycle guard: skip if another minion for this issue is already running
    match is_issue_claimed(&candidate.info.repo, candidate.info.issue).await {
        Ok(true) => {
            log::info!(
                "Skipping {} (issue #{}, {}): another minion for this issue is already running",
                candidate.minion_id,
                issue_display,
                candidate.info.repo,
            );
            return false;
        }
        Ok(false) => {}
        Err(e) => {
            log::warn!(
                "⚠️  Failed to check if issue #{} is claimed: {} — skipping to be safe",
                issue_display,
                e,
            );
            return false;
        }
    }

    // Skip minions whose issue is already closed (PR merged or issue resolved)
    let (owner, repo_name) = match candidate.info.repo.split_once('/') {
        Some(parts) => parts,
        None => return false,
    };
    // When no issue number is set, skip the closed-issue check entirely
    let issue_closed_result = if let Some(issue_num) = candidate.info.issue {
        github::is_issue_closed_via_cli(owner, repo_name, host, issue_num).await
    } else {
        Ok(false)
    };
    match issue_closed_result {
        Ok(true) => {
            tprintln!(
                "⏭️  Skipping {} (issue #{}, {}): issue is closed",
                candidate.minion_id,
                issue_display,
                candidate.info.repo,
            );
            mark_minion_completed(&candidate.minion_id).await;
            return false;
        }
        Ok(false) => {
            // Issue is still open — but the PR may already be merged
            // (e.g. PR body lacked "Closes #N" so the issue wasn't auto-closed).
            let pr_field = candidate.info.pr.as_deref();
            let decision = check_pr_merge_state(pr_field, |pr_num| {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(github::is_pr_open_via_cli(owner, repo_name, host, pr_num))
                })
            });
            match decision {
                PrResumeDecision::Resume => {} // proceed with resume
                PrResumeDecision::SkipCompleted { pr_num } => {
                    tprintln!(
                        "⏭️  Skipping {} (issue #{}, {}): PR #{} is merged/closed",
                        candidate.minion_id,
                        issue_display,
                        candidate.info.repo,
                        pr_num,
                    );
                    mark_minion_completed(&candidate.minion_id).await;
                    return false;
                }
                PrResumeDecision::RetryNextPoll { pr_num, error } => {
                    log::warn!(
                        "⚠️  Failed to check PR #{} state for {} (issue #{}): {} — will retry next poll",
                        pr_num,
                        candidate.minion_id,
                        issue_display,
                        error,
                    );
                    return false;
                }
            }
        }
        Err(e) => {
            log::warn!(
                "⚠️  Failed to check issue state for {} (issue #{}): {} — will retry next poll",
                candidate.minion_id,
                issue_display,
                e,
            );
            return false;
        }
    }

    // Skip minions whose timeout_deadline has passed
    if let Some(deadline) = candidate.info.timeout_deadline {
        if Utc::now() >= deadline {
            tprintln!(
                "⏭️  Skipping {} (issue #{}, {}): timeout_deadline has passed",
                candidate.minion_id,
                issue_display,
                candidate.info.repo,
            );
            mark_exhausted_minion(
                &candidate.minion_id,
                &candidate.info,
                host,
                "timeout deadline has passed",
            )
            .await;
            return false;
        }
    }

    // Skip minions that have exceeded max attempts
    if candidate.info.attempt_count > max_attempts {
        tprintln!(
            "⏭️  Skipping {} (issue #{}, {}): attempt_count {} > max {}",
            candidate.minion_id,
            issue_display,
            candidate.info.repo,
            candidate.info.attempt_count,
            max_attempts,
        );
        let reason = format!("exceeded maximum resume attempts ({})", max_attempts);
        mark_exhausted_minion(&candidate.minion_id, &candidate.info, host, &reason).await;
        return false;
    }

    true
}

/// Spawn a resume process for a candidate and write its PID to the registry.
///
/// Returns `true` if the resume was successful and the child was added to `children`.
async fn try_resume_candidate(
    candidate: &ResumableMinion,
    children: &mut Vec<SpawnedChild>,
) -> bool {
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
                        info.pid_start_time = crate::minion_registry::get_process_start_time(pid);
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
            true
        }
        Err(e) => {
            log::warn!(
                "⚠️  Failed to resume {} for issue #{}: {}",
                candidate.minion_id,
                candidate
                    .info
                    .issue
                    .map_or("?".to_string(), |n| n.to_string()),
                e
            );
            false
        }
    }
}

/// Returns `true` if the session-level guard should block this minion from being
/// re-resumed. MonitoringPr minions are exempted because they are long-lived and
/// expected to be re-entered across poll cycles (e.g. review agent exits or crashes).
/// CreatingPr and RunningAgent are transient phases that should complete in a single
/// session; re-entering them suggests a persistent failure the guard should contain.
/// The `attempt_count > max_attempts` check in `should_resume_candidate` is the
/// ultimate circuit breaker — `attempt_count` is incremented by `resume.rs` on every
/// re-entry, so crash-looping MonitoringPr minions still get terminated.
fn is_blocked_by_session_guard(
    minion_id: &str,
    phase: &OrchestrationPhase,
    resumed_this_session: &HashSet<String>,
) -> bool {
    resumed_this_session.contains(minion_id) && phase != &OrchestrationPhase::MonitoringPr
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
            let blocked = is_blocked_by_session_guard(
                &c.minion_id,
                &c.info.orchestration_phase,
                resumed_this_session,
            );
            if blocked {
                log::debug!(
                    "Skipping {} (issue #{}, {}): already resumed this session",
                    c.minion_id,
                    c.info.issue.map_or("?".to_string(), |n| n.to_string()),
                    c.info.repo,
                );
            }
            !blocked
        })
        .collect();
    if resumable.is_empty() {
        return Ok(0);
    }

    // Sort by recency and deduplicate per (repo, issue): only the most recent minion
    // per issue is retained, preventing concurrent resumes for the same issue.
    let resumable = sort_and_dedup_resumable(resumable);

    tprintln!(
        "🔄 Found {} resumable Minion(s) from previous session",
        resumable.len()
    );

    let mut resumed = 0;

    for candidate in &resumable {
        if *available == 0 {
            break;
        }

        let host = match host_for_repo(config, &candidate.info.repo) {
            Some(h) => h,
            None => continue,
        };

        if !should_resume_candidate(candidate, &host, max_attempts).await {
            continue;
        }

        tprintln!(
            "♻️  Resuming {} (issue #{}, {}, phase: {:?})",
            candidate.minion_id,
            candidate
                .info
                .issue
                .map_or("?".to_string(), |n| n.to_string()),
            candidate.info.repo,
            candidate.info.orchestration_phase,
        );

        // Record this minion as attempted regardless of outcome
        resumed_this_session.insert(candidate.minion_id.clone());

        if try_resume_candidate(candidate, children).await {
            resumed += 1;
            *available -= 1;
        }
    }

    if resumed > 0 {
        tprintln!("✅ Resumed {} Minion(s)", resumed);
    }

    Ok(resumed)
}

/// Fetch candidate issues for a single repository, with fallback on failure.
async fn fetch_candidate_issues(
    owner: &str,
    repo: &str,
    host: &str,
    label: &str,
    repo_spec: &str,
) -> Option<Vec<github::CandidateIssue>> {
    match list_ready_issues_via_cli(owner, repo, host, label).await {
        Ok(issues) => Some(issues),
        Err(cli_err) => {
            log::warn!(
                "⚠️  CLI issue fetch failed for {}: {}, trying basic CLI fallback",
                repo_spec,
                cli_err
            );
            match fallback_list_issues(owner, repo, host, label).await {
                Ok(issues) => Some(issues),
                Err(e) => {
                    log::warn!("⚠️  Fallback also failed for {}: {:#}", repo_spec, e);
                    None
                }
            }
        }
    }
}

/// Parsed repository identity for a single poll cycle iteration.
struct RepoContext<'a> {
    host: String,
    owner: String,
    repo: String,
    /// Canonical `owner/repo` slug.
    full: String,
    /// Original repo spec string from config (for log messages).
    spec: &'a str,
}

/// Try to claim and spawn a Minion for a single issue.
///
/// Assumes the caller has already checked for dependency blockers and that the
/// issue is not already claimed. Returns `Ok(true)` if a minion was successfully
/// spawned, `Ok(false)` if the claim or spawn failed.
async fn try_spawn_for_issue(
    ctx: &RepoContext<'_>,
    issue_number: u64,
    label: &str,
    children: &mut Vec<SpawnedChild>,
) -> Result<bool> {
    // Try to claim the issue via CLI
    if let Err(e) =
        github::claim_issue_via_cli(&ctx.host, &ctx.owner, &ctx.repo, issue_number, label).await
    {
        log::warn!(
            "⚠️  Failed to claim issue {}/issues/{}: {}",
            ctx.spec,
            issue_number,
            e
        );
        return Ok(false);
    }

    // Successfully claimed, spawn Minion
    match spawn_minion(&ctx.full, &ctx.host, issue_number).await {
        Ok(child) => {
            // Write PID to registry immediately (if the subprocess has
            // already created the entry) to prevent duplicate spawns.
            if let Some(pid) = child.id() {
                let repo_cl = ctx.full.clone();
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
                    host: ctx.host.clone(),
                    owner: ctx.owner.clone(),
                    repo: ctx.repo.clone(),

                    issue_number,
                    ready_label: label.to_string(),
                    spawned_at: Instant::now(),
                    retry_attempt: 0, // first attempt, not a retry
                }),
            });
            tprintln!("✨ Spawned Minion for {}/issues/{}", ctx.spec, issue_number);
            Ok(true)
        }
        Err(e) => {
            log::warn!(
                "⚠️  Failed to spawn Minion for {}/issues/{}: {}",
                ctx.spec,
                issue_number,
                e
            );
            // Unclaim the issue since we failed to spawn: remove in-progress, restore ready label
            if let Err(e) = github::edit_labels_via_cli(
                &ctx.host,
                &ctx.owner,
                &ctx.repo,
                issue_number,
                &[label],
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
            Ok(false)
        }
    }
}

/// Poll all configured repositories for ready issues and spawn Minions for them.
///
/// Returns the number of minions successfully spawned.
async fn spawn_for_candidate_issues(
    config: &LabConfig,
    children: &mut Vec<SpawnedChild>,
    available: &mut usize,
    label: &str,
    shutdown_flag: &AtomicBool,
) -> Result<usize> {
    let mut spawned = 0usize;

    for repo_spec in &config.daemon.repos {
        if *available == 0 || shutdown_flag.load(Ordering::Acquire) {
            break;
        }

        let (host, owner, repo) = match parse_repo_entry_with_hosts(repo_spec, &config.github_hosts)
        {
            Some(parsed) => parsed,
            None => {
                log::warn!("⚠️  Invalid repo format: '{}', skipping", repo_spec);
                continue;
            }
        };
        let ctx = RepoContext {
            full: github::repo_slug(&owner, &repo),
            host,
            owner,
            repo,
            spec: repo_spec,
        };

        let mut candidates =
            match fetch_candidate_issues(&ctx.owner, &ctx.repo, &ctx.host, label, ctx.spec).await {
                Some(c) => c,
                None => continue,
            };

        // Sort candidates by priority label (stable sort preserves GitHub's default order within tiers)
        candidates.sort_by_key(|c| github::priority_sort_key(&c.labels));

        let candidate_count = candidates.len();
        let mut blocked_count = 0usize;
        let mut spawned_this_repo = 0usize;

        for candidate in &candidates {
            if *available == 0 || shutdown_flag.load(Ordering::Acquire) {
                break;
            }

            // Check if issue is already being worked on (by a live process)
            if is_issue_claimed(&ctx.full, Some(candidate.number)).await? {
                continue;
            }

            // Check if issue has unresolved dependencies (body parsing + API verify)
            let body = candidate.body.as_deref().unwrap_or("");
            let blockers = crate::dependencies::get_blockers(
                &ctx.host,
                &ctx.owner,
                &ctx.repo,
                candidate.number,
                body,
            )
            .await;
            if !blockers.is_empty() {
                let blocker_list: Vec<String> = blockers.iter().map(|n| format!("#{n}")).collect();
                log::info!(
                    "⏭️  Skipping issue #{}: blocked by {}",
                    candidate.number,
                    blocker_list.join(", ")
                );
                blocked_count += 1;
                continue;
            }

            // Revalidate issue state to prevent TOCTOU races between poll and dispatch
            if !github::is_issue_still_eligible(&ctx.owner, &ctx.repo, &ctx.host, candidate.number)
                .await
            {
                continue;
            }

            if try_spawn_for_issue(&ctx, candidate.number, label, children).await? {
                spawned += 1;
                spawned_this_repo += 1;
                *available -= 1;
            }
        }

        if candidate_count > 0 && spawned_this_repo == 0 && blocked_count > 0 {
            log::warn!(
                "🚫 {}/{} candidate issue(s) in {} blocked by dependencies — nothing spawned this cycle",
                blocked_count,
                candidate_count,
                ctx.spec
            );
        }
    }

    Ok(spawned)
}

/// Result of a poll cycle.
struct PollResult {
    /// Number of Minions spawned or resumed.
    spawned: usize,
    /// True when no slots were available and issue polling was skipped.
    slots_full: bool,
}

/// Poll GitHub for ready issues and spawn Minions if slots are available.
///
/// Dispatch order: due retries (continuation first) → resumed minions → new issues.
///
/// Checks between major phases for cooperative cancellation.
/// When the flag is set (signal received), returns early at the next safe checkpoint
/// instead of continuing to the next API call.
async fn poll_and_spawn(
    config: &LabConfig,
    children: &mut Vec<SpawnedChild>,
    retry_queue: &mut RetryQueue,
    no_resume: bool,
    resumed_this_session: &mut HashSet<String>,
    wake_check_times: &mut HashMap<String, DateTime<Utc>>,
    shutdown_flag: &AtomicBool,
) -> Result<PollResult> {
    // Prune stale registry entries (worktrees that no longer exist, checking PR status)
    prune_stale_entries().await?;

    if shutdown_flag.load(Ordering::Acquire) {
        return Ok(PollResult {
            spawned: 0,
            slots_full: false,
        });
    }

    let max_attempts = config.daemon.max_resume_attempts;

    // Wake up Completed minions with new external reviews or merge conflicts so they re-enter MonitoringPr.
    // Runs after prune_stale_entries so stale entries don't generate spurious wake-ups.
    if !no_resume {
        if let Err(e) = find_minions_needing_wake(
            config,
            max_attempts,
            wake_check_times,
            resumed_this_session,
            shutdown_flag,
        )
        .await
        {
            log::warn!("⚠️  Review wake scan error: {:#}", e);
        }
    }

    if shutdown_flag.load(Ordering::Acquire) {
        return Ok(PollResult {
            spawned: 0,
            slots_full: false,
        });
    }

    // Calculate available slots using PID liveness (not registry status string)
    let mut available = available_slots(config.daemon.max_slots).await?;

    if available == 0 {
        // All slots occupied, skip issue polling
        return Ok(PollResult {
            spawned: 0,
            slots_full: true,
        });
    }

    // Dispatch due retries first (continuation retries get priority over failure retries)
    let mut spawned = dispatch_due_retries(retry_queue, children, &mut available).await?;

    if available == 0 {
        if spawned > 0 {
            tprintln!();
        }
        return Ok(PollResult {
            spawned,
            slots_full: false,
        });
    }

    // Resume interrupted minions, before claiming new issues
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
    spawned += resumed;

    if shutdown_flag.load(Ordering::Acquire) {
        return Ok(PollResult {
            spawned,
            slots_full: false,
        });
    }

    if available == 0 {
        if spawned > 0 {
            tprintln!();
        }
        return Ok(PollResult {
            spawned,
            slots_full: false,
        });
    }

    // Poll each configured repository and spawn minions for ready issues
    spawned += spawn_for_candidate_issues(
        config,
        children,
        &mut available,
        &config.daemon.label,
        shutdown_flag,
    )
    .await?;

    if spawned > 0 {
        tprintln!();
    }

    Ok(PollResult {
        spawned,
        slots_full: false,
    })
}

/// Dispatch retry entries whose backoff has elapsed.
///
/// For each due retry:
/// 1. Validate the issue is still eligible (open, not already claimed)
/// 2. Spawn a new Minion process (failure retries get fresh sessions)
///
/// Note: Currently only failure retries are dispatched. Continuation retries
/// (which would reuse session-id/worktree via `spawn_resume`) are infrastructure
/// for #618 and are not yet enqueued or dispatched. When #618 lands, this function
/// should branch on `entry.kind` and call `spawn_resume` for continuations.
/// Returns the number of retries dispatched.
async fn dispatch_due_retries(
    retry_queue: &mut RetryQueue,
    children: &mut Vec<SpawnedChild>,
    available: &mut usize,
) -> Result<usize> {
    let due = retry_queue.take_due();
    if due.is_empty() {
        return Ok(0);
    }

    let mut dispatched = 0usize;

    for entry in due {
        if *available == 0 {
            // No slots left — re-insert with original due_at preserved
            // so the backoff timer isn't reset on every slot-full poll cycle.
            retry_queue.reinsert(entry);
            continue;
        }

        let full_repo = format!("{}/{}", entry.owner, entry.repo);

        // Skip if issue is already being worked on by a live process
        if is_issue_claimed(&full_repo, Some(entry.issue_number)).await? {
            log::info!(
                "⏭️  Skipping retry for issue #{}: already claimed by live process",
                entry.issue_number
            );
            continue;
        }

        // Validate issue is still open and eligible
        if !github::is_issue_still_eligible(
            &entry.owner,
            &entry.repo,
            &entry.host,
            entry.issue_number,
        )
        .await
        {
            log::info!(
                "⏭️  Skipping retry for issue #{}: no longer eligible (closed or label changed)",
                entry.issue_number
            );
            continue;
        }

        tprintln!(
            "🔄 Retrying issue {}/{}#{} ({} retry, attempt {}): {}",
            entry.owner,
            entry.repo,
            entry.issue_number,
            entry.kind,
            entry.attempt,
            entry.reason
        );

        match spawn_minion(&full_repo, &entry.host, entry.issue_number).await {
            Ok(child) => {
                children.push(SpawnedChild {
                    child,
                    spawn_meta: Some(SpawnMeta {
                        host: entry.host.clone(),
                        owner: entry.owner.clone(),
                        repo: entry.repo.clone(),

                        issue_number: entry.issue_number,
                        ready_label: labels::TODO.to_string(),
                        spawned_at: Instant::now(),
                        retry_attempt: entry.attempt, // carry through for next failure
                    }),
                });
                dispatched += 1;
                *available -= 1;
            }
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to spawn retry for issue #{}: {} — re-enqueuing",
                    entry.issue_number,
                    e
                );
                // Reinsert so a transient spawn failure doesn't permanently drop retry state.
                retry_queue.reinsert(entry);
            }
        }
    }

    Ok(dispatched)
}

/// Log a summary of pending retry queue entries.
fn log_retry_queue_status(retry_queue: &RetryQueue) {
    let entries = retry_queue.pending_entries();
    let now = Instant::now();
    tprintln!("🔄 Retry queue: {} pending", entries.len());
    for entry in entries {
        let remaining = if entry.due_at > now {
            let secs = (entry.due_at - now).as_secs();
            format!("due in {}s", secs)
        } else {
            "due now".to_string()
        };
        tprintln!(
            "   • {}/{}#{} ({} retry, attempt {}) — {}",
            entry.owner,
            entry.repo,
            entry.issue_number,
            entry.kind,
            entry.attempt,
            remaining
        );
    }
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
async fn is_issue_claimed(repo: &str, issue_number: Option<u64>) -> Result<bool> {
    let Some(issue_number) = issue_number else {
        return Ok(false);
    };
    let repo = repo.to_string();
    with_registry(move |registry| {
        let claimed = registry.list().iter().any(|(_id, info)| {
            info.repo == repo && info.issue == Some(issue_number) && info.is_running()
        });
        Ok(claimed)
    })
    .await
}

/// Build a log filename for a minion working on an issue.
///
/// For `github.com` hosts the prefix is omitted; for other hosts (GHE) the
/// hostname is sanitized and prepended to avoid collisions when the same
/// owner/repo exists on multiple hosts.
fn format_log_name(host: &str, repo: &str, issue_number: u64) -> String {
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
    format!("{}{}-issue-{}.log", safe_host, safe_repo, issue_number)
}

/// Create the log directory and open a log file, returning cloned handles for
/// stdout and stderr redirection.
///
/// The returned `PathBuf` is the resolved log file path (for display purposes).
async fn setup_log_file(log_name: &str) -> Result<(std::fs::File, std::fs::File, PathBuf)> {
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    let log_dir = home.join(".gru").join("state").join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let log_path = log_dir.join(log_name);
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

    Ok((stdout_file, stderr_file, log_path))
}

/// Spawn a command as a background process with stdin null and stdout/stderr
/// redirected to the given file handles.
///
/// Gives the child its own session (setsid) so Ctrl-C doesn't propagate, waits
/// briefly for immediate failures, and returns the child handle.
async fn spawn_background_cmd(
    mut cmd: tokio::process::Command,
    stdout_file: std::fs::File,
    stderr_file: std::fs::File,
    description: &str,
) -> Result<Child> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    // Give the child its own session so SIGINT from Ctrl-C (sent to the terminal's
    // foreground process group) is not delivered to the child. This allows the lab
    // to shut down without killing running Minions.
    #[cfg(unix)]
    // SAFETY: pre_exec closures run in the child after fork(). setsid() is
    // async-signal-safe, so this is safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {}", description))?;

    // Give the process a moment to fail if there are startup issues.
    // This prevents phantom slot occupancy from processes that immediately fail.
    tokio::time::sleep(Duration::from_millis(100)).await;

    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!(
            "{} exited immediately with status: {:?}",
            description,
            status
        );
    }

    Ok(child)
}

/// Spawn a Minion to work on an issue using the `gru do` command.
///
/// Returns the child process handle for lifecycle tracking.
async fn spawn_minion(repo: &str, host: &str, issue_number: u64) -> Result<Child> {
    let issue_ref = crate::github::build_issue_url_with_host(repo, host, issue_number)
        .with_context(|| format!("Invalid repo format: '{}'", repo))?;

    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let log_name = format_log_name(host, repo, issue_number);
    let (stdout_file, stderr_file, log_path) = setup_log_file(&log_name).await?;

    // Remove TMUX/TMUX_PANE so the child doesn't inherit the lab's tmux session —
    // otherwise TmuxGuard renames arbitrary windows in the parent's tmux.
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("do")
        .arg(&issue_ref)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");

    let child = spawn_background_cmd(
        cmd,
        stdout_file,
        stderr_file,
        &format!("gru do {}", issue_ref),
    )
    .await?;

    tprintln!("📝 Log: {}", log_path.display());

    Ok(child)
}

/// Spawn a resume for an existing Minion using `gru resume <minion_id>`.
/// Returns the child process handle for lifecycle tracking.
async fn spawn_resume(minion_id: &str) -> Result<Child> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let log_name = format!("resume-{}.log", minion_id);
    let (stdout_file, stderr_file, log_path) = setup_log_file(&log_name).await?;

    // Remove TMUX/TMUX_PANE so the child doesn't inherit the lab's tmux session —
    // otherwise TmuxGuard renames arbitrary windows in the parent's tmux.
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("resume")
        .arg(minion_id)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");

    let child = spawn_background_cmd(
        cmd,
        stdout_file,
        stderr_file,
        &format!("gru resume {}", minion_id),
    )
    .await?;

    tprintln!("📝 Log: {}", log_path.display());

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
    let repo_full = github::repo_slug(owner, repo);
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
            let labels = issue["labels"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| {
                            l["name"].as_str().map(|n| github::IssueLabel {
                                name: n.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(github::CandidateIssue {
                number,
                body,
                labels,
            })
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
        let mut retry_queue = RetryQueue::new(3, 300);
        reap_children(&mut children, &mut retry_queue).await;
        assert!(children.is_empty());
    }

    #[test]
    fn test_log_path_github_com() {
        assert_eq!(
            format_log_name("github.com", "owner/repo", 42),
            "owner-repo-issue-42.log"
        );
    }

    #[test]
    fn test_log_path_ghe_includes_host() {
        assert_eq!(
            format_log_name("ghe.netflix.net", "corp/service", 42),
            "ghe-netflix-net-corp-service-issue-42.log"
        );
    }

    #[test]
    fn test_log_path_host_with_port_is_sanitized() {
        assert_eq!(
            format_log_name("ghe.example.com:8443", "org/app", 7),
            "ghe-example-com-8443-org-app-issue-7.log"
        );
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

        // Poll until the process has actually exited instead of a fixed sleep,
        // which is flaky on loaded CI machines.
        let mut exited = false;
        for _ in 0..100 {
            if children[0]
                .child
                .try_wait()
                .expect("failed to check child process status")
                .is_some()
            {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(exited, "child process did not exit within 1s timeout");

        let mut retry_queue = RetryQueue::new(3, 300);
        reap_children(&mut children, &mut retry_queue).await;
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

        let mut retry_queue = RetryQueue::new(3, 300);
        reap_children(&mut children, &mut retry_queue).await;
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
            retry_attempt: 0,
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
        make_completed_minion_with_command(pr, attempt_count, "do")
    }

    fn make_completed_minion_with_command(
        pr: Option<&str>,
        attempt_count: u32,
        command: &str,
    ) -> MinionInfo {
        use crate::minion_registry::{MinionMode, OrchestrationPhase};
        use std::path::PathBuf;
        MinionInfo {
            repo: "owner/repo".to_string(),
            issue: Some(42),
            command: command.to_string(),
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
            archived_at: None,
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
    fn test_find_wake_candidates_requires_completed_phase() {
        use crate::minion_registry::OrchestrationPhase;
        let mut info = make_completed_minion(Some("10"), 0);
        // MonitoringPr is an active phase, not Completed — excluded by the == Completed check
        info.orchestration_phase = OrchestrationPhase::MonitoringPr;
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Only Completed-phase minions are candidates; any other phase is excluded"
        );
    }

    #[test]
    fn test_find_wake_candidates_returns_eligible_minion() {
        let info = make_completed_minion(Some("10"), 0);
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert_eq!(candidates, vec!["M001"]);
    }

    #[test]
    fn test_find_wake_candidates_skips_review_minions() {
        let info = make_completed_minion_with_command(Some("10"), 0, "review");
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Review minions are one-shot and must not be wake candidates"
        );
    }

    #[test]
    fn test_find_wake_candidates_skips_prompt_minions() {
        let info = make_completed_minion_with_command(Some("10"), 0, "prompt");
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Prompt minions are one-shot and must not be wake candidates"
        );
    }

    #[test]
    fn test_find_wake_candidates_allows_fix_minions() {
        let info = make_completed_minion_with_command(Some("10"), 0, "fix");
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert_eq!(
            candidates,
            vec!["M001"],
            "Fix minions should be wake candidates"
        );
    }

    #[test]
    fn test_find_wake_candidates_skips_unknown_command() {
        let info = make_completed_minion_with_command(Some("10"), 0, "respond");
        let minions = vec![("M001".to_string(), info)];
        let candidates = find_wake_candidates(&minions, 3);
        assert!(
            candidates.is_empty(),
            "Unknown commands must not be wake candidates until explicitly allowed"
        );
    }

    // --- within_wake_cooldown tests ---

    #[test]
    fn test_within_wake_cooldown_true_when_recently_checked() {
        let now = Utc::now();
        let last_check = now - chrono::Duration::seconds(60); // 1 minute ago
        let cooldown = Duration::from_secs(5 * 60); // 5 minute cooldown
        assert!(
            within_wake_cooldown(last_check, now, cooldown),
            "Should be within cooldown when only 1 minute has elapsed of a 5-minute cooldown"
        );
    }

    #[test]
    fn test_within_wake_cooldown_false_when_cooldown_elapsed() {
        let now = Utc::now();
        let last_check = now - chrono::Duration::seconds(6 * 60); // 6 minutes ago
        let cooldown = Duration::from_secs(5 * 60); // 5 minute cooldown
        assert!(
            !within_wake_cooldown(last_check, now, cooldown),
            "Should not be within cooldown when 6 minutes have elapsed of a 5-minute cooldown"
        );
    }

    #[test]
    fn test_within_wake_cooldown_false_for_epoch_default() {
        // DateTime::UNIX_EPOCH is the default for never-checked minions; cooldown must not block them.
        let now = Utc::now();
        let cooldown = Duration::from_secs(5 * 60);
        assert!(
            !within_wake_cooldown(DateTime::UNIX_EPOCH, now, cooldown),
            "Never-checked minion (epoch default) must pass the cooldown check"
        );
    }

    // --- should_wake_minion tests ---

    #[test]
    fn test_should_wake_minion_false_for_closed_pr() {
        assert!(
            !should_wake_minion(false, 2, false),
            "Closed/merged PR must never trigger wake-up"
        );
    }

    #[test]
    fn test_should_wake_minion_false_for_closed_pr_with_conflict() {
        assert!(
            !should_wake_minion(false, 0, true),
            "Closed/merged PR must never trigger wake-up even with conflict"
        );
    }

    #[test]
    fn test_should_wake_minion_false_for_no_reviews_no_conflict() {
        assert!(
            !should_wake_minion(true, 0, false),
            "Open PR with zero unaddressed reviews and no conflict must not trigger wake-up"
        );
    }

    #[test]
    fn test_should_wake_minion_true_when_reviews_pending() {
        assert!(
            should_wake_minion(true, 1, false),
            "Wake-up must trigger when PR is open and reviews are pending"
        );
    }

    #[test]
    fn test_should_wake_minion_true_when_merge_conflict() {
        assert!(
            should_wake_minion(true, 0, true),
            "Wake-up must trigger when PR is open and has merge conflict"
        );
    }

    #[test]
    fn test_should_wake_minion_true_when_reviews_and_conflict() {
        assert!(
            should_wake_minion(true, 2, true),
            "Wake-up must trigger when PR has both reviews and conflict"
        );
    }

    // --- sort_and_dedup_resumable tests ---

    fn make_resumable(minion_id: &str, repo: &str, issue: Option<u64>) -> ResumableMinion {
        use crate::minion_registry::{MinionInfo, MinionMode, OrchestrationPhase};
        use chrono::Utc;
        use std::path::PathBuf;
        let now = Utc::now();
        ResumableMinion {
            minion_id: minion_id.to_string(),
            info: MinionInfo {
                repo: repo.to_string(),
                issue,
                command: "do".to_string(),
                prompt: String::new(),
                started_at: now,
                branch: format!(
                    "minion/issue-{}-{}",
                    issue.map_or("none".to_string(), |n| n.to_string()),
                    minion_id
                ),
                worktree: PathBuf::from("/tmp/test"),
                status: "active".to_string(),
                pr: None,
                session_id: uuid::Uuid::new_v4().to_string(),
                pid: None,
                pid_start_time: None,
                mode: MinionMode::Stopped,
                last_activity: now,
                orchestration_phase: OrchestrationPhase::RunningAgent,
                token_usage: None,
                agent_name: "claude".to_string(),
                timeout_deadline: None,
                attempt_count: 0,
                no_watch: false,
                last_review_check_time: None,
                wake_reason: None,
                archived_at: None,
            },
        }
    }

    #[test]
    fn test_sort_and_dedup_resumable_picks_most_recent() {
        // Three minions for the same issue; M003 is the most recent and should win.
        let candidates = vec![
            make_resumable("M001", "owner/repo", Some(42)),
            make_resumable("M003", "owner/repo", Some(42)),
            make_resumable("M002", "owner/repo", Some(42)),
        ];
        let result = sort_and_dedup_resumable(candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].minion_id, "M003");
    }

    #[test]
    fn test_sort_and_dedup_resumable_keeps_one_per_issue() {
        // Two issues, two minions each; only the newer one per issue should survive.
        let candidates = vec![
            make_resumable("M001", "owner/repo", Some(10)),
            make_resumable("M004", "owner/repo", Some(10)),
            make_resumable("M002", "owner/repo", Some(20)),
            make_resumable("M003", "owner/repo", Some(20)),
        ];
        let result = sort_and_dedup_resumable(candidates);
        assert_eq!(result.len(), 2);
        let ids: Vec<&str> = result.iter().map(|r| r.minion_id.as_str()).collect();
        assert!(
            ids.contains(&"M004"),
            "issue 10: expected M004, got {:?}",
            ids
        );
        assert!(
            ids.contains(&"M003"),
            "issue 20: expected M003, got {:?}",
            ids
        );
    }

    #[test]
    fn test_sort_and_dedup_resumable_legacy_uppercase_ids() {
        // Legacy IDs used uppercase letters for digits 10-35.
        // M00Z (legacy) and M00a (current) both represent 35; M00a is a current-format
        // ID while M00Z is legacy.  The sort must not incorrectly rank the legacy
        // uppercase ID higher than a current ID with the same numeric value.
        // M010 = 36, which is strictly greater than M00z = 35, so M010 must win.
        let candidates = vec![
            make_resumable("M00Z", "owner/repo", Some(1)), // legacy uppercase, value = 35
            make_resumable("M010", "owner/repo", Some(1)), // current lowercase, value = 36
        ];
        let result = sort_and_dedup_resumable(candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].minion_id, "M010",
            "M010 (36) must rank higher than M00Z (35)"
        );
    }

    #[test]
    fn test_sort_and_dedup_resumable_empty() {
        let result = sort_and_dedup_resumable(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_sort_and_dedup_resumable_single() {
        let candidates = vec![make_resumable("M001", "owner/repo", Some(42))];
        let result = sort_and_dedup_resumable(candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].minion_id, "M001");
    }

    // --- check_pr_merge_state tests ---

    #[test]
    fn test_check_pr_merge_state_no_pr() {
        let decision = check_pr_merge_state(None, |_| unreachable!());
        assert_eq!(decision, PrResumeDecision::Resume);
    }

    #[test]
    fn test_check_pr_merge_state_pr_still_open() {
        let decision = check_pr_merge_state(Some("42"), |num| {
            assert_eq!(num, 42);
            Ok(true) // PR is open
        });
        assert_eq!(decision, PrResumeDecision::Resume);
    }

    #[test]
    fn test_check_pr_merge_state_pr_merged_or_closed() {
        let decision = check_pr_merge_state(Some("99"), |num| {
            assert_eq!(num, 99);
            Ok(false) // PR is merged/closed
        });
        assert_eq!(decision, PrResumeDecision::SkipCompleted { pr_num: 99 });
    }

    #[test]
    fn test_check_pr_merge_state_transient_error() {
        let decision = check_pr_merge_state(Some("7"), |_| Err(anyhow::anyhow!("network timeout")));
        match decision {
            PrResumeDecision::RetryNextPoll { pr_num, error } => {
                assert_eq!(pr_num, 7);
                assert!(error.contains("network timeout"));
            }
            other => panic!("expected RetryNextPoll, got {:?}", other),
        }
    }

    #[test]
    fn test_check_pr_merge_state_unparseable_pr() {
        let decision = check_pr_merge_state(Some("not-a-number"), |_| unreachable!());
        assert_eq!(decision, PrResumeDecision::Resume);
    }

    /// Verifies that an issue with gru:done should not have gru:todo restored.
    /// This is the core logic behind the reap_children guard: if an issue already
    /// has a terminal label, adding the ready label would cause spurious re-spawns.
    #[test]
    fn test_done_label_prevents_eligibility() {
        use crate::github::{check_issue_eligibility, IssueLabel};

        let labels = vec![
            IssueLabel {
                name: "gru:done".to_string(),
            },
            IssueLabel {
                name: "gru:todo".to_string(),
            },
        ];
        let (eligible, reason) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible, "Issue with gru:done should not be eligible");
        assert!(reason.unwrap().contains("gru:done"));
    }

    /// Verifies that gru:failed also prevents label restoration.
    #[test]
    fn test_failed_label_prevents_eligibility() {
        use crate::github::{check_issue_eligibility, IssueLabel};

        let labels = vec![IssueLabel {
            name: "gru:failed".to_string(),
        }];
        let (eligible, reason) = check_issue_eligibility("OPEN", &labels);
        assert!(!eligible, "Issue with gru:failed should not be eligible");
        assert!(reason.unwrap().contains("gru:failed"));
    }

    /// Verifies that an issue without terminal labels IS eligible for
    /// label restoration, confirming the normal early-exit path still works.
    #[test]
    fn test_no_terminal_label_allows_restoration() {
        use crate::github::{check_issue_eligibility, IssueLabel};

        let labels = vec![IssueLabel {
            name: "gru:todo".to_string(),
        }];
        let (eligible, _) = check_issue_eligibility("OPEN", &labels);
        assert!(eligible, "Issue without terminal labels should be eligible");
    }

    /// MonitoringPr minions that die after being resumed should be re-resumed
    /// on the next poll cycle (not blocked by resumed_this_session).
    #[test]
    fn test_monitoring_pr_minion_bypasses_resumed_this_session() {
        use crate::minion_registry::OrchestrationPhase;

        let mut resumed_this_session: HashSet<String> = HashSet::new();
        resumed_this_session.insert("M001".to_string());
        resumed_this_session.insert("M002".to_string());

        // M001 is MonitoringPr — should pass through the filter
        let mut monitoring = make_resumable("M001", "owner/repo", Some(42));
        monitoring.info.orchestration_phase = OrchestrationPhase::MonitoringPr;

        // M002 is RunningAgent — should be blocked by resumed_this_session
        let running = make_resumable("M002", "owner/repo", Some(43));
        assert_eq!(
            running.info.orchestration_phase,
            OrchestrationPhase::RunningAgent
        );

        // M003 is MonitoringPr but NOT in resumed_this_session — should pass
        let mut fresh = make_resumable("M003", "owner/repo", Some(44));
        fresh.info.orchestration_phase = OrchestrationPhase::MonitoringPr;

        let candidates = vec![monitoring, running, fresh];
        let filtered: Vec<_> = candidates
            .into_iter()
            .filter(|c| {
                !is_blocked_by_session_guard(
                    &c.minion_id,
                    &c.info.orchestration_phase,
                    &resumed_this_session,
                )
            })
            .collect();

        let ids: Vec<&str> = filtered.iter().map(|r| r.minion_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["M001", "M003"],
            "MonitoringPr minions must bypass resumed_this_session; RunningAgent must not"
        );
    }

    /// Verifies that resumable minions can carry PR metadata,
    /// which resume_interrupted_minions uses to check merge state.
    #[test]
    fn test_resumable_minion_with_pr_is_deduped_correctly() {
        let mut c1 = make_resumable("M001", "owner/repo", Some(42));
        c1.info.pr = Some("100".to_string());
        let mut c2 = make_resumable("M003", "owner/repo", Some(42));
        c2.info.pr = Some("101".to_string());

        let result = sort_and_dedup_resumable(vec![c1, c2]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].minion_id, "M003");
        // The most recent minion's PR is the one that will be checked
        assert_eq!(result[0].info.pr.as_deref(), Some("101"));
    }
}
