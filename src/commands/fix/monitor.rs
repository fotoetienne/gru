use super::agent::invoke_agent_for_reviews;
use super::types::{IssueContext, WorktreeContext, MAX_REBASE_ATTEMPTS, MAX_REVIEW_ROUNDS};
use crate::agent::AgentBackend;
use crate::ci;
use crate::config::LabConfig;
use crate::github;
use crate::merge_judge::{self, JudgeAction, JudgeState};
use crate::minion_registry;
use crate::pr_monitor::{self, MonitorResult};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

/// Persists a review-check timestamp to the minion registry (best-effort).
/// Errors are logged as warnings since this is non-critical metadata.
async fn save_review_check_time(minion_id: &str, ts: DateTime<Utc>) {
    let mid = minion_id.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.last_review_check_time = Some(ts);
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to save last_review_check_time: {}", e);
    }
}

/// Resets `attempt_count` to 0 after a successful review response (best-effort).
///
/// The cap should only trigger on *consecutive* failures. Resetting here ensures a
/// minion that successfully addresses reviews isn't penalised for past resume cycles.
async fn reset_attempt_count(minion_id: &str) {
    let mid = minion_id.to_string();
    if let Err(e) = minion_registry::with_registry(move |registry| {
        registry.update(&mid, |info| {
            info.attempt_count = 0;
        })
    })
    .await
    {
        log::warn!("⚠️  Failed to reset attempt_count: {}", e);
    }
}

/// Attempts to auto-rebase the worktree branch onto its base branch.
///
/// Returns `Ok(true)` if the rebase succeeded (clean or Claude resolved conflicts),
/// `Ok(false)` if Claude couldn't resolve conflicts, or `Err` on unexpected failures.
async fn auto_rebase_pr(worktree_path: &Path) -> Result<bool> {
    use super::super::rebase::{
        abort_rebase, attempt_rebase, check_clean_worktree, detect_base_branch, fetch_origin,
        force_push, run_agent_rebase, RebaseOutcome,
    };

    // Bail early if worktree has uncommitted changes (e.g., agent crashed mid-edit)
    check_clean_worktree(worktree_path)
        .await
        .context("Cannot auto-rebase: worktree has uncommitted changes")?;

    // Fetch latest from origin
    println!("📡 Fetching latest changes from origin...");
    fetch_origin(worktree_path).await?;

    // Detect the base branch
    let base_branch = detect_base_branch(worktree_path).await?;
    println!("🔄 Rebasing onto origin/{}...", base_branch);

    // Attempt the rebase
    match attempt_rebase(worktree_path, &base_branch).await? {
        RebaseOutcome::Clean { commit_count } => {
            println!(
                "✅ Clean rebase: {} commit{} replayed",
                commit_count,
                if commit_count == 1 { "" } else { "s" }
            );
            force_push(worktree_path).await?;
            println!("🚀 Force-pushed rebased branch");
            Ok(true)
        }
        RebaseOutcome::Conflicts => {
            println!("⚠️  Conflicts detected, launching agent to resolve...");
            abort_rebase(worktree_path).await?;

            let exit_code = run_agent_rebase(worktree_path).await?;
            if exit_code == 0 {
                // Defensively force push in case the /rebase skill didn't push
                force_push(worktree_path).await?;
                println!("🚀 Force-pushed rebased branch");
                Ok(true)
            } else {
                log::warn!("Agent rebase exited with code {}", exit_code);
                Ok(false)
            }
        }
    }
}

/// Format the body of a monitoring-paused notification comment.
///
/// Includes YAML frontmatter for machine discoverability and a human-readable
/// section with the minion ID and resume command.
fn format_exit_notification_comment(minion_id: &str, unaddressed_count: usize) -> String {
    let review_word = if unaddressed_count == 1 {
        "review"
    } else {
        "reviews"
    };
    format!(
        "---\ntype: monitoring-paused\n---\n\n\
        ⏸️ This PR's automated agent has paused. \
        There are {} {} that haven't been addressed yet. \
        Resume automated responses with:\n`gru resume {}`{}",
        unaddressed_count,
        review_word,
        minion_id,
        crate::progress_comments::minion_signature(minion_id),
    )
}

/// Check for unaddressed reviews and post a notification comment if warranted.
///
/// Skips silently when the PR is merged/closed or when there are no unaddressed
/// external reviews (i.e. reviews from someone other than the PR author).
async fn post_exit_notification_if_needed(
    owner: &str,
    repo: &str,
    host: &str,
    pr_number: &str,
    minion_id: &str,
    review_baseline: DateTime<Utc>,
) {
    // Check PR state and author in one API call.
    let (is_open, pr_author) =
        match pr_monitor::get_pr_info_for_exit_notification(host, owner, repo, pr_number).await {
            Ok(info) => info,
            Err(e) => {
                log::warn!("⚠️  Could not check PR state for exit notification: {}", e);
                return;
            }
        };

    if !is_open {
        return;
    }

    let reviews = match pr_monitor::get_all_reviews(host, owner, repo, pr_number).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("⚠️  Could not fetch reviews for exit notification: {}", e);
            return;
        }
    };

    let count = pr_monitor::has_unaddressed_reviews(&reviews, &pr_author, review_baseline);

    if !pr_monitor::should_post_exit_notification(is_open, count) {
        return;
    }

    let body = format_exit_notification_comment(minion_id, count);
    let repo_full = format!("{}/{}", owner, repo);
    let result = crate::github::gh_cli_command(host)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted monitoring-paused notification on PR #{}", pr_number);
            println!(
                "⏸️  Monitoring paused. {} review(s) pending. Resume: gru resume {}",
                count, minion_id
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post exit notification on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment for exit notification: {}", e);
        }
    }
}

/// Posts an escalation comment on a PR when auto-rebase fails.
async fn post_escalation_comment(
    owner: &str,
    repo: &str,
    host: &str,
    pr_number: &str,
    message: &str,
    minion_id: &str,
) {
    let repo_full = format!("{}/{}", owner, repo);
    let body = format!(
        "🤖 **Minion Escalation**\n\n{}{}",
        message,
        crate::progress_comments::minion_signature(minion_id)
    );

    let result = crate::github::gh_cli_command(host)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted escalation comment on PR #{}", pr_number);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post escalation comment on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment: {}", e);
        }
    }
}

/// Trigger a PR review as a separate process.
/// If `review_timeout` is `Some`, the review is killed after that duration.
/// If `None`, the review runs without a timeout (Claude's built-in stuck detection applies).
async fn trigger_pr_review(
    pr_number: &str,
    worktree_path: &Path,
    review_timeout: Option<Duration>,
) -> Result<i32> {
    // Validate PR number format (defense in depth)
    pr_number
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_number))?;

    let mut child = TokioCommand::new("gru")
        .arg("review")
        .arg(pr_number)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn gru review command for PR #{}. Is gru in your PATH?",
                pr_number
            )
        })?;

    match review_timeout {
        Some(timeout_duration) => match timeout(timeout_duration, child.wait()).await {
            Ok(status) => {
                let status = status.with_context(|| {
                    format!("Failed to wait for review process for PR #{}", pr_number)
                })?;
                Ok(status
                    .code()
                    .unwrap_or(crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED))
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let elapsed_secs = timeout_duration.as_secs();
                let time_display = if elapsed_secs >= 60 {
                    let minutes = elapsed_secs / 60;
                    let seconds = elapsed_secs % 60;
                    if seconds == 0 {
                        format!("{} minute{}", minutes, if minutes == 1 { "" } else { "s" })
                    } else {
                        format!(
                            "{} minute{} {} second{}",
                            minutes,
                            if minutes == 1 { "" } else { "s" },
                            seconds,
                            if seconds == 1 { "" } else { "s" }
                        )
                    }
                } else {
                    format!(
                        "{} second{}",
                        elapsed_secs,
                        if elapsed_secs == 1 { "" } else { "s" }
                    )
                };
                Err(anyhow::anyhow!(
                    "Review process timed out after {}. PR #{} review may be stuck.",
                    time_display,
                    pr_number
                ))
            }
        },
        None => {
            let status = child.wait().await.with_context(|| {
                format!("Failed to wait for review process for PR #{}", pr_number)
            })?;
            Ok(status
                .code()
                .unwrap_or(crate::agent_runner::EXIT_CODE_SIGNAL_TERMINATED))
        }
    }
}

/// Monitors a PR for reviews, CI failures, and merge/close events.
/// Handles automatic review rounds up to MAX_REVIEW_ROUNDS.
pub(crate) async fn monitor_pr_lifecycle(
    backend: &dyn AgentBackend,
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &str,
    timeout_opt: Option<&str>,
    review_timeout: Option<Duration>,
    monitor_timeout: Duration,
) {
    // Ensure the gru:ready-to-merge label exists in the repo
    if let Err(e) =
        pr_monitor::ensure_ready_to_merge_label(&issue_ctx.host, &issue_ctx.owner, &issue_ctx.repo)
            .await
    {
        log::warn!(
            "⚠️  Failed to ensure {} label: {}",
            crate::labels::READY_TO_MERGE,
            e
        );
    }

    // Capture timestamp before self-review so that any reviews submitted
    // during the review are not missed when the monitoring loop starts.
    let pre_review_time = chrono::Utc::now();

    // Guard: skip self-review if the authenticated gh user already posted a
    // non-dismissed review for the current HEAD SHA on this PR.
    //
    // Uses the GitHub API so the check is cross-minion — a review posted by a
    // previous minion session is visible here even though each minion has its
    // own minion_dir.  Fails open: if the API is unreachable we allow the
    // review to proceed (a duplicate is less harmful than a missing review).
    //
    // Race condition: two minions entering simultaneously may both see false
    // and both post a review.  This window is narrow and not worth a
    // distributed lock for V1.
    let head_sha = ci::get_head_sha(&wt_ctx.checkout_path).await.ok();
    let already_reviewed = if let Some(ref sha) = head_sha {
        github::has_gru_review_for_sha(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            pr_number,
            sha,
        )
        .await
    } else {
        false
    };

    if already_reviewed {
        println!(
            "\n⏭️  Skipping self-review: already posted for current HEAD ({})",
            head_sha.as_deref().unwrap_or("unknown")
        );
    } else {
        // Auto-trigger review for Minion-created PRs
        println!("\n🔍 Starting automated PR review...");
        // Persist the pre-review timestamp as the initial review baseline.
        // Done here (not before the already_reviewed guard) so that on re-entry
        // we don't overwrite a stored baseline with the current time, which would
        // cause reviews posted while the minion was stopped to be skipped.
        save_review_check_time(&wt_ctx.minion_id, pre_review_time).await;
        match trigger_pr_review(pr_number, &wt_ctx.checkout_path, review_timeout).await {
            Ok(review_exit_code) => {
                if review_exit_code == 0 {
                    println!("✅ PR review completed successfully");
                } else {
                    log::warn!(
                        "⚠️  PR review completed with exit code: {}",
                        review_exit_code
                    );
                }
            }
            Err(e) => {
                log::warn!("⚠️  Failed to run PR review: {}", e);
                log::warn!("   You can review manually with: gru review {}", pr_number);
            }
        }
    }

    // Start monitoring the PR for review comments, CI failures, and merge/close events
    println!("\n👀 Monitoring PR for updates (polling every 30s)...");
    println!("   Press Ctrl+C to stop monitoring\n");

    let monitor_start = tokio::time::Instant::now();
    let mut review_round = 0;
    let mut ci_escalated = false;
    let mut issue_was_blocked = false;
    let mut rebase_attempts = 0;
    let mut judge_state = JudgeState::new();
    let mut judge_label_ensured = false;
    let mut consecutive_errors: u32 = 0;
    // 10 consecutive monitor_pr invocation failures before giving up.
    // Wall-clock time per failure varies since monitor_pr does its own internal
    // polling and retries before returning an error.
    const MAX_CONSECUTIVE_ERRORS: u32 = 10;
    // Load merge confidence threshold from config (falls back to default).
    // Uses load_partial to avoid requiring [daemon].repos for non-daemon commands.
    let confidence_threshold = LabConfig::default_path()
        .ok()
        .and_then(|p| {
            LabConfig::load_partial(&p)
                .map_err(|e| {
                    log::warn!("Failed to load config for merge threshold: {e}, using default");
                    e
                })
                .ok()
        })
        .map(|c| c.merge.confidence_threshold.clamp(1, 10))
        .unwrap_or(merge_judge::DEFAULT_CONFIDENCE_THRESHOLD);
    // Track review baseline across monitor_pr re-entries so reviews posted
    // before/during event handling (e.g. rebase) are not silently dropped.
    // On a fresh run, initialize to pre_review_time so reviews submitted
    // during the self-review are detected on the first poll.
    // On re-entry (already_reviewed), load the stored baseline from the
    // registry so reviews posted while the minion was stopped are not skipped.
    let initial_baseline = if already_reviewed {
        let mid = wt_ctx.minion_id.clone();
        minion_registry::with_registry(move |registry| {
            Ok(registry
                .get(&mid)
                .map(|info| info.last_review_check_time.unwrap_or(info.started_at)))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(pre_review_time)
    } else {
        pre_review_time
    };
    let mut review_baseline: Option<chrono::DateTime<chrono::Utc>> = Some(initial_baseline);
    loop {
        // Compute remaining time so the timeout spans the entire lifecycle,
        // not just a single monitor_pr invocation.
        let remaining = monitor_timeout.checked_sub(monitor_start.elapsed());
        if remaining.is_none() || remaining == Some(Duration::ZERO) {
            let elapsed = monitor_start.elapsed();
            let total_secs = elapsed.as_secs();
            let hours = total_secs / 3600;
            let minutes = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            let display = if hours > 0 {
                format!("{}h{}m", hours, minutes)
            } else if minutes > 0 {
                format!("{}m", minutes)
            } else {
                format!("{}s", secs)
            };
            println!("⏰ PR monitoring timed out after {}", display);
            println!(
                "   PR is still open: https://github.com/{}/{}/pull/{}",
                issue_ctx.owner, issue_ctx.repo, pr_number
            );
            break;
        }

        let monitor_result = pr_monitor::monitor_pr(
            &issue_ctx.host,
            &issue_ctx.owner,
            &issue_ctx.repo,
            pr_number,
            &wt_ctx.checkout_path,
            remaining,
            review_baseline,
        )
        .await;

        if monitor_result.is_ok() {
            consecutive_errors = 0;
        }

        match monitor_result {
            Ok((MonitorResult::Merged, _)) => {
                println!("✅ PR #{} was merged successfully!", pr_number);
                println!("🎉 Issue {} is complete!", issue_ctx.issue_num);
                break;
            }
            Ok((MonitorResult::Closed, _)) => {
                println!("⚠️  PR #{} was closed without merging", pr_number);
                println!("   The issue may need to be reopened or addressed differently");
                break;
            }
            Ok((MonitorResult::ReadyToMerge, _)) => {
                // All merge gates pass — remove gru:blocked if it was stale
                if issue_was_blocked {
                    if let Ok(pr_num) = pr_number.parse::<u64>() {
                        super::helpers::try_remove_blocked_label(
                            &issue_ctx.host,
                            &issue_ctx.owner,
                            &issue_ctx.repo,
                            pr_num,
                            issue_ctx.issue_num,
                        )
                        .await;
                    }
                    issue_was_blocked = false;
                    ci_escalated = false;
                }

                // Lazily ensure the gru:needs-human-review label exists on
                // first ReadyToMerge, rather than unconditionally at startup.
                if !judge_label_ensured {
                    if let Err(e) = merge_judge::ensure_needs_human_review_label(
                        &issue_ctx.host,
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                    )
                    .await
                    {
                        log::warn!("⚠️  Failed to ensure gru:needs-human-review label: {}", e);
                    }
                    judge_label_ensured = true;
                }

                // Check if gru:needs-human-review was previously applied and
                // not yet cleared — skip judge until human removes it.
                // On API failure, be conservative and skip (don't proceed).
                match merge_judge::has_needs_human_review_label(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    pr_number,
                )
                .await
                {
                    Ok(true) => {
                        println!(
                            "⏸️  PR #{} has gru:needs-human-review — waiting for human to remove it",
                            pr_number
                        );
                        continue;
                    }
                    Ok(false) => {
                        // Only clear escalation if we previously confirmed the
                        // label was applied. This prevents premature clearing if
                        // the label add previously failed.
                        if judge_state.label_was_applied() {
                            judge_state.mark_escalation_cleared();
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to check needs-human-review label: {} — skipping judge",
                            e
                        );
                        continue;
                    }
                }

                // Invoke the merge-readiness judge.
                match merge_judge::evaluate(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    pr_number,
                    &wt_ctx.checkout_path,
                    &mut judge_state,
                    confidence_threshold,
                )
                .await
                {
                    Ok(Some(response)) => match &response.action {
                        JudgeAction::Merge => {
                            println!(
                                "🚀 Judge approved merge for PR #{} (confidence: {}/10)",
                                pr_number, response.confidence
                            );
                            let repo_full = format!("{}/{}", issue_ctx.owner, issue_ctx.repo);
                            match crate::github::gh_cli_command(&issue_ctx.host)
                                .args([
                                    "pr", "merge", pr_number, "--squash", "--auto", "-R",
                                    &repo_full,
                                ])
                                .output()
                                .await
                            {
                                Ok(output) if output.status.success() => {
                                    println!("✅ Auto-merge queued for PR #{}!", pr_number);
                                    println!("🎉 Issue {} is complete!", issue_ctx.issue_num);
                                    break;
                                }
                                Ok(output) => {
                                    let stderr = String::from_utf8_lossy(&output.stderr);
                                    log::warn!(
                                        "⚠️  Auto-merge failed for PR #{}: {}",
                                        pr_number,
                                        stderr.trim()
                                    );
                                    println!("🔄 Will retry on next poll cycle...");
                                }
                                Err(e) => {
                                    log::warn!("⚠️  Failed to run merge command: {}", e);
                                    println!("🔄 Will retry on next poll cycle...");
                                }
                            }
                        }
                        JudgeAction::Wait(duration) => {
                            println!(
                                "⏳ Judge says wait {}m before re-evaluating PR #{}",
                                duration.as_secs() / 60,
                                pr_number
                            );
                            println!("🔄 Continuing to monitor PR...\n");
                        }
                        JudgeAction::Escalate => {
                            println!(
                                "🚨 Judge escalated PR #{} for human review (confidence: {}/10)",
                                pr_number, response.confidence
                            );
                            // Apply label and post comment.
                            match merge_judge::add_needs_human_review_label(
                                &issue_ctx.host,
                                &issue_ctx.owner,
                                &issue_ctx.repo,
                                pr_number,
                            )
                            .await
                            {
                                Ok(()) => judge_state.mark_label_applied(),
                                Err(e) => {
                                    log::warn!("Failed to add needs-human-review label: {}", e);
                                }
                            }
                            merge_judge::post_judge_escalation_comment(
                                &issue_ctx.host,
                                &issue_ctx.owner,
                                &issue_ctx.repo,
                                pr_number,
                                &response,
                            )
                            .await;
                            println!("🔄 Continuing to monitor PR...\n");
                        }
                    },
                    Ok(None) => {
                        // Judge invocation skipped (same state, no timer expired).
                        log::debug!("Judge invocation skipped — PR state unchanged");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Merge judge failed: {}", e);
                        println!("🔄 Will retry on next poll cycle...");
                    }
                }
            }
            Ok((MonitorResult::NewReviews(comments), check_time)) => {
                review_round += 1;
                let count = comments.len();
                println!(
                    "💬 Detected {} new review comment(s) on PR #{} (review round {}/{})",
                    count, pr_number, review_round, MAX_REVIEW_ROUNDS
                );

                if review_round > MAX_REVIEW_ROUNDS {
                    println!(
                        "⚠️  Reached maximum review rounds limit ({})",
                        MAX_REVIEW_ROUNDS
                    );
                    println!("   Additional reviews will need manual handling");
                    println!(
                        "   View PR: https://github.com/{}/{}/pull/{}",
                        issue_ctx.owner, issue_ctx.repo, pr_number
                    );
                    break;
                }

                let review_prompt =
                    pr_monitor::format_review_prompt(issue_ctx.issue_num, pr_number, &comments);

                println!("🔄 Re-invoking to address review feedback...\n");

                match invoke_agent_for_reviews(
                    backend,
                    &wt_ctx.checkout_path,
                    &wt_ctx.minion_dir,
                    &wt_ctx.session_id,
                    &review_prompt,
                    timeout_opt,
                    &issue_ctx.host,
                )
                .await
                {
                    Ok(()) => {
                        println!("\n✅ Finished addressing review comments");
                        println!("🔄 Continuing to monitor PR...\n");
                        // Use the check_time returned by monitor_pr, which was
                        // advanced past the reviews we just handled. This ensures
                        // those reviews aren't re-fetched while still catching
                        // any new reviews posted during handling.
                        review_baseline = Some(check_time);
                        // Persist updated baseline after successfully handling reviews.
                        save_review_check_time(&wt_ctx.minion_id, check_time).await;
                        // Reset attempt_count so the cap only triggers on consecutive
                        // failures, not cumulative resume cycles across successful rounds.
                        reset_attempt_count(&wt_ctx.minion_id).await;
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to address review comments: {}", e);
                        log::warn!("   You can address them manually");
                        break;
                    }
                }
            }
            Ok((MonitorResult::FailedChecks(count), _)) => {
                if ci_escalated {
                    // Already escalated — wait for human intervention
                    println!(
                        "ℹ️  CI still failing ({} check(s)) on PR #{}, waiting for human fix",
                        count, pr_number
                    );
                    // Continue monitoring for merge/close/review events
                    continue;
                }

                println!(
                    "❌ Detected {} failed CI check(s) on PR #{}, attempting auto-fix...",
                    count, pr_number
                );

                // Parse pr_number for the CI fix API
                let pr_num_u64 = match pr_number.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => {
                        println!("⚠️  Could not parse PR number, skipping CI auto-fix");
                        println!("🔄 Continuing to monitor PR for other events...\n");
                        continue;
                    }
                };

                match ci::monitor_and_fix_ci(
                    &issue_ctx.host,
                    &issue_ctx.owner,
                    &issue_ctx.repo,
                    pr_num_u64,
                    &wt_ctx.branch_name,
                    &wt_ctx.checkout_path,
                    &wt_ctx.minion_id,
                )
                .await
                {
                    Ok(true) => {
                        println!("✅ CI checks now pass after auto-fix");
                        // Remove gru:blocked label if it was added during escalation
                        if issue_was_blocked {
                            super::helpers::try_remove_blocked_label(
                                &issue_ctx.host,
                                &issue_ctx.owner,
                                &issue_ctx.repo,
                                pr_num_u64,
                                issue_ctx.issue_num,
                            )
                            .await;
                            issue_was_blocked = false;
                        }
                        ci_escalated = false;
                        println!("🔄 Continuing to monitor PR...\n");
                    }
                    Ok(false) => {
                        ci_escalated = true;
                        issue_was_blocked = true;
                        println!("⚠️  CI auto-fix escalated to human after max attempts");
                        println!(
                            "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                            issue_ctx.owner, issue_ctx.repo, pr_number
                        );
                        println!("🔄 Continuing to monitor PR for other events...\n");
                    }
                    Err(e) => {
                        println!("⚠️  CI auto-fix error: {}", e);
                        println!(
                            "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                            issue_ctx.owner, issue_ctx.repo, pr_number
                        );
                        println!("🔄 Will retry CI auto-fix on subsequent monitoring cycles...\n");
                    }
                }
            }
            Ok((MonitorResult::MergeConflict, check_time)) => {
                if rebase_attempts >= MAX_REBASE_ATTEMPTS {
                    println!(
                        "❌ Reached maximum rebase attempts ({}), escalating",
                        MAX_REBASE_ATTEMPTS
                    );
                    post_escalation_comment(
                        &issue_ctx.owner,
                        &issue_ctx.repo,
                        &issue_ctx.host,
                        pr_number,
                        "Auto-rebase failed after multiple attempts. Manual conflict resolution required.",
                        &wt_ctx.minion_id,
                    )
                    .await;
                    break;
                }

                rebase_attempts += 1;
                println!(
                    "⚠️  Merge conflict detected on PR #{} (rebase attempt {}/{})",
                    pr_number, rebase_attempts, MAX_REBASE_ATTEMPTS
                );

                match auto_rebase_pr(&wt_ctx.checkout_path).await {
                    Ok(true) => {
                        // Reset counter on success — GitHub may still report
                        // mergeable: false for a few poll cycles after force-push
                        // while it recomputes. We don't want stale signals to
                        // exhaust the attempt budget.
                        rebase_attempts = 0;
                        // Use the check_time from just before the conflict was
                        // detected. Reviews posted during the rebase will have
                        // submitted_at > check_time and be caught on the next poll.
                        review_baseline = Some(check_time);
                        // Note: save_review_check_time is intentionally not called here.
                        // check_time marks the start of the conflict window, not a point
                        // where reviews were processed. The exit-time save will persist it.
                        println!("✅ Rebase succeeded, continuing to monitor PR...\n");
                    }
                    Ok(false) => {
                        // Agent couldn't resolve conflicts
                        println!("❌ Could not resolve merge conflicts automatically");
                        post_escalation_comment(
                            &issue_ctx.owner,
                            &issue_ctx.repo,
                            &issue_ctx.host,
                            pr_number,
                            "Auto-rebase failed: could not resolve merge conflicts automatically. Manual intervention required.",
                            &wt_ctx.minion_id,
                        )
                        .await;
                        break;
                    }
                    Err(e) => {
                        log::warn!("⚠️  Auto-rebase error: {:#}", e);
                        post_escalation_comment(
                            &issue_ctx.owner,
                            &issue_ctx.repo,
                            &issue_ctx.host,
                            pr_number,
                            "Auto-rebase encountered an unexpected error. Check Minion logs for details.",
                            &wt_ctx.minion_id,
                        )
                        .await;
                        break;
                    }
                }
            }
            Ok((MonitorResult::Timeout, _)) => {
                // Use the lifecycle-level start time for an accurate total elapsed display
                let total_secs = monitor_start.elapsed().as_secs();
                let hours = total_secs / 3600;
                let minutes = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let display = if hours > 0 {
                    format!("{}h{}m", hours, minutes)
                } else if minutes > 0 {
                    format!("{}m", minutes)
                } else {
                    format!("{}s", secs)
                };
                println!("⏰ PR monitoring timed out after {}", display);
                println!(
                    "   PR is still open: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner, issue_ctx.repo, pr_number
                );
                break;
            }
            Ok((MonitorResult::Interrupted, _)) => {
                println!("\n⚠️  Monitoring interrupted by user");
                println!(
                    "   PR is still open: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner, issue_ctx.repo, pr_number
                );
                break;
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    log::warn!(
                        "⚠️  PR monitoring failed {} consecutive times, giving up: {}",
                        consecutive_errors,
                        e
                    );
                    log::warn!(
                        "   You can monitor manually at: https://github.com/{}/{}/pull/{}",
                        issue_ctx.owner,
                        issue_ctx.repo,
                        pr_number
                    );
                    break;
                }
                log::warn!(
                    "⚠️  PR monitoring error ({}/{}): {}",
                    consecutive_errors,
                    MAX_CONSECUTIVE_ERRORS,
                    e
                );
                // Sleep before retrying to avoid hammering the API if monitor_pr
                // fails before its internal poll sleep. Cap at remaining timeout
                // so we don't overshoot the configured monitor_timeout.
                let backoff = Duration::from_secs(30);
                let remaining = monitor_timeout.checked_sub(monitor_start.elapsed());
                match remaining {
                    Some(r) if r > Duration::ZERO => {
                        tokio::select! {
                            _ = tokio::time::sleep(backoff.min(r)) => {}
                            _ = tokio::signal::ctrl_c() => {
                                println!("\n⚠️  Monitoring interrupted by user");
                                println!(
                                    "   PR is still open: https://github.com/{}/{}/pull/{}",
                                    issue_ctx.owner, issue_ctx.repo, pr_number
                                );
                                break;
                            }
                        }
                    }
                    _ => {
                        // Timeout already expired, let the loop's timeout check handle it.
                    }
                }
                continue;
            }
        }
    }

    // Persist the final review baseline on monitor exit so the lab wake-up
    // scan knows where to resume without re-processing already-seen reviews.
    // Also check if the PR is still open with unaddressed external reviews and
    // post a notification if so.
    if let Some(baseline) = review_baseline {
        save_review_check_time(&wt_ctx.minion_id, baseline).await;
        post_exit_notification_if_needed(
            &issue_ctx.owner,
            &issue_ctx.repo,
            &issue_ctx.host,
            pr_number,
            &wt_ctx.minion_id,
            baseline,
        )
        .await;
    }
}

/// Monitors CI after the initial fix and attempts auto-fixes if checks fail.
/// Returns Ok(true) if CI passed, Ok(false) if escalated/failed.
pub(crate) async fn monitor_ci_after_fix(
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    worktree_path: &Path,
    minion_id: &str,
) -> Result<bool> {
    let pr_number = match ci::get_pr_number(host, owner, repo, branch, None).await? {
        Some(num) => num,
        None => {
            eprintln!(
                "ℹ️  No PR found for branch {}, skipping CI monitoring",
                branch
            );
            return Ok(true);
        }
    };

    // Backfill the minion registry if it has pr: null but we discovered a PR.
    // Read first to avoid an unnecessary save when pr is already set.
    let mid = minion_id.to_string();
    let needs_backfill = crate::minion_registry::with_registry({
        let mid = mid.clone();
        move |registry| Ok(registry.get(&mid).is_some_and(|info| info.pr.is_none()))
    })
    .await
    .unwrap_or(false);

    if needs_backfill {
        let pr_num_for_backfill = pr_number;
        if let Err(e) = crate::minion_registry::with_registry(move |registry| {
            registry.update(&mid, |info| {
                if info.pr.is_none() {
                    log::info!(
                        "📝 Backfilling registry: minion now linked to PR #{}",
                        pr_num_for_backfill
                    );
                    info.pr = Some(pr_num_for_backfill.to_string());
                }
            })
        })
        .await
        {
            log::warn!("⚠️  Failed to backfill PR in registry: {}", e);
        }
    }

    eprintln!("🔍 Monitoring CI for PR #{}", pr_number);
    ci::monitor_and_fix_ci(
        host,
        owner,
        repo,
        pr_number,
        branch,
        worktree_path,
        minion_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_notification_format_contains_minion_id_and_resume_command() {
        let body = format_exit_notification_comment("M042", 2);
        assert!(body.contains("M042"), "comment must contain minion ID");
        assert!(
            body.contains("gru resume M042"),
            "comment must contain resume command"
        );
        assert!(body.contains("2 reviews"), "comment must mention the count");
        assert!(
            body.contains("type: monitoring-paused"),
            "comment must include YAML frontmatter type"
        );
    }
}
