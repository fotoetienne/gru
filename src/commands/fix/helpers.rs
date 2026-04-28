use crate::minion_registry::{with_registry, MinionMode, OrchestrationPhase};
use std::path::Path;
use tokio::process::Command as TokioCommand;

/// Updates the orchestration phase for a minion in the registry.
/// Logs a warning if the update fails, since phase tracking is important for resume correctness.
pub(crate) async fn update_orchestration_phase(minion_id: &str, phase: OrchestrationPhase) {
    let minion_id_owned = minion_id.to_string();
    let phase_name = format!("{:?}", phase);
    if let Err(e) = with_registry(move |registry| {
        registry.update(&minion_id_owned, |info| {
            info.orchestration_phase = phase;
        })
    })
    .await
    {
        log::warn!(
            "⚠️  Failed to update orchestration phase for {} to {}: {}",
            minion_id,
            phase_name,
            e
        );
    }
}

/// Posts a comment on an issue and attempts to mark it as blocked via CLI (fire-and-forget).
/// The comment is posted before the label; the label is still applied even if the comment fails.
pub(crate) async fn try_mark_issue_blocked(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    reason: &str,
) {
    if let Err(e) = crate::github::post_comment_via_cli(host, owner, repo, issue_num, reason).await
    {
        log::warn!("⚠️  Failed to post blocked comment on issue: {}", e);
    }
    match crate::github::mark_issue_blocked_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {:#}", e);
        }
    }
}

/// Removes `gru:blocked` from the PR and restores `gru:in-progress` on the issue.
/// Fire-and-forget: logs on failure but does not propagate errors.
/// Idempotent: safe to call even if the label is not present.
pub(crate) async fn try_remove_blocked_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    issue_num: u64,
) {
    match crate::github::remove_blocked_label(host, owner, repo, pr_number, issue_num).await {
        Ok(()) => {
            println!("🏷️  Removed '{}' label", crate::labels::BLOCKED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to remove blocked label: {:#}", e);
        }
    }
}

/// Environment variable set by `gru lab` on spawned `gru do` children.
/// Presence (with value `GRU_RETRY_PARENT_VALUE`) signals that lab owns retry/give-up
/// policy, so the worker should defer `gru:failed` labeling.
pub(crate) const GRU_RETRY_PARENT_ENV: &str = "GRU_RETRY_PARENT";

/// Expected value of `GRU_RETRY_PARENT` when set by lab.
pub(crate) const GRU_RETRY_PARENT_VALUE: &str = "lab";

/// Returns `true` when the worker should eagerly apply `gru:failed` on agent failure.
///
/// Standalone `gru do` invocations always label eagerly. When spawned by `gru lab`
/// (`GRU_RETRY_PARENT=lab` in the environment), lab owns retry/give-up policy and
/// the label is deferred so the retry queue can fire.
pub(crate) fn label_eagerly_on_failure() -> bool {
    std::env::var(GRU_RETRY_PARENT_ENV).as_deref() != Ok(GRU_RETRY_PARENT_VALUE)
}

/// Attempts to mark an issue as failed via CLI (fire-and-forget).
/// Logs success/failure but does not propagate errors.
pub(crate) async fn try_mark_issue_failed(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::mark_issue_failed_via_cli(host, owner, repo, issue_num).await {
        Ok(()) => {
            println!("🏷️  Updated issue label to '{}'", crate::labels::FAILED);
        }
        Err(e) => {
            log::warn!("⚠️  Failed to update issue label: {:#}", e);
        }
    }
}

/// Resolves a base ref and counts how many commits HEAD is ahead of it.
/// Returns `None` when no base ref can be resolved so callers can distinguish
/// "unknown" from "legitimately zero" and avoid acting on an unreliable signal.
/// Shared between the crash-path preservation flow and the clean-exit
/// detection in `pr.rs`.
pub(super) async fn commits_ahead_of_base(
    checkout_path: &Path,
    host: &str,
    owner: &str,
    repo: &str,
) -> Option<usize> {
    let candidates = base_branch_candidates(checkout_path, host, owner, repo).await;
    for base in &candidates {
        if let Some(base_ref) = resolve_base_ref(checkout_path, base).await {
            return Some(count_commits_ahead(checkout_path, &base_ref).await);
        }
    }
    log::warn!(
        "Could not resolve any base ref for {} (tried: {:?}); commit count unavailable",
        checkout_path.display(),
        candidates
    );
    None
}

/// Resolves candidate base-branch names in preference order: local
/// `origin/HEAD`, then the GitHub API result, then `main` / `master`.
/// Callers pick the first candidate whose `origin/<branch>` ref exists locally.
async fn base_branch_candidates(
    checkout_path: &Path,
    host: &str,
    owner: &str,
    repo: &str,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    if let Ok(out) = TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .await
    {
        if out.status.success() {
            let raw = String::from_utf8_lossy(&out.stdout);
            if let Some(branch) = raw.trim().strip_prefix("refs/remotes/origin/") {
                candidates.push(branch.to_string());
            }
        }
    }

    match crate::github::get_default_branch(host, owner, repo).await {
        Ok(branch) => {
            if !candidates.iter().any(|c| c == &branch) {
                candidates.push(branch);
            }
        }
        Err(e) => {
            log::warn!(
                "Could not determine default branch from GitHub API: {}. \
                 Falling back to 'main' / 'master' guesses.",
                e
            );
        }
    }

    for guess in ["main", "master"] {
        if !candidates.iter().any(|c| c == guess) {
            candidates.push(guess.to_string());
        }
    }

    candidates
}

/// Checks whether a git ref exists in the worktree.
async fn ref_exists(checkout_path: &Path, refname: &str) -> bool {
    TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["show-ref", "--verify", "--quiet", refname])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolves a usable base ref for `branch`, preferring the remote-tracking
/// ref and falling back to the local head. Gru's bare-repo worktrees typically
/// fetch the default branch into `refs/heads/<base>`, not
/// `refs/remotes/origin/<base>`, so the local fallback keeps the rescue from
/// silently giving up on otherwise-standard setups.
async fn resolve_base_ref(checkout_path: &Path, branch: &str) -> Option<String> {
    let remote = format!("refs/remotes/origin/{}", branch);
    if ref_exists(checkout_path, &remote).await {
        return Some(remote);
    }
    let local = format!("refs/heads/{}", branch);
    if ref_exists(checkout_path, &local).await {
        return Some(local);
    }
    None
}

/// Counts commits on HEAD that are not on `base_ref`.
/// Returns 0 on any error — callers treat 0 as "nothing to preserve".
async fn count_commits_ahead(checkout_path: &Path, base_ref: &str) -> usize {
    let output = match TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["rev-list", "--count", &format!("{}..HEAD", base_ref)])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            log::warn!("Failed to count commits ahead of base: {}", e);
            return 0;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!("git rev-list --count failed: {}", stderr.trim());
        return 0;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0)
}

/// Pushes the worktree's HEAD to `refs/heads/<branch_name>` on origin.
///
/// Using an explicit `HEAD:refs/heads/<branch>` refspec (rather than
/// `origin <branch>`) makes the rescue robust when the worktree is in a
/// detached-HEAD state (rebase, merge, or mid-cherry-pick), which can
/// happen if the agent was killed at the wrong moment.
async fn push_branch(checkout_path: &Path, branch_name: &str) -> anyhow::Result<()> {
    let refspec = format!("HEAD:refs/heads/{}", branch_name);
    let output = TokioCommand::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["push", "--force-with-lease", "origin", &refspec])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to execute git push: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow::anyhow!("git push failed: {}", stderr.trim()))
    }
}

/// Attempts to preserve committed work when the agent exits unexpectedly.
///
/// If the branch has local commits that aren't on the base, pushes the branch
/// and posts a diagnostic comment on the issue so the user (or a future minion)
/// can recover the work. Fire-and-forget: errors are logged, not propagated.
///
/// Returns `true` if commits were found and a push was attempted (regardless of
/// whether the push itself succeeded).
pub(crate) async fn try_preserve_branch_work(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: Option<u64>,
    checkout_path: &Path,
    branch_name: &str,
    minion_id: &str,
) -> bool {
    let candidates = base_branch_candidates(checkout_path, host, owner, repo).await;
    let mut resolved: Option<(String, String, usize)> = None;
    for base in &candidates {
        if let Some(base_ref) = resolve_base_ref(checkout_path, base).await {
            let n = count_commits_ahead(checkout_path, &base_ref).await;
            resolved = Some((base.clone(), base_ref, n));
            break;
        }
    }

    let (base_branch, base_ref, commits_ahead) = match resolved {
        Some(r) => r,
        None => {
            log::error!(
                "🚨 Could not resolve a base branch for commit preservation on '{}'. \
                 Tried: {:?}. Committed work on the branch remains only in the local \
                 worktree at {}.",
                branch_name,
                candidates,
                checkout_path.display()
            );
            return false;
        }
    };

    if commits_ahead == 0 {
        return false;
    }
    log::info!(
        "Preserving {} commit(s) on '{}' (base: {} via '{}')",
        commits_ahead,
        branch_name,
        base_branch,
        base_ref
    );

    let push_result = push_branch(checkout_path, branch_name).await;
    let push_succeeded = push_result.is_ok();
    if let Err(ref e) = push_result {
        log::warn!(
            "⚠️  Failed to push branch '{}' after agent exit: {:#}",
            branch_name,
            e
        );
    } else {
        println!(
            "🚀 Pushed branch '{}' to preserve agent's work",
            branch_name
        );
    }

    if let Some(num) = issue_num {
        let branch_line = if push_succeeded {
            format!(
                "Branch pushed: [`{}`](https://{}/{}/{}/tree/{})",
                branch_name, host, owner, repo, branch_name
            )
        } else {
            format!(
                "⚠️  Branch push failed — commits remain only in the local worktree for `{}`.",
                branch_name
            )
        };
        let comment = format!(
            "⚠️  Minion `{}` exited unexpectedly during the agent phase with {} \
             commit(s) ahead of the base branch.\n\n\
             {}\n\n\
             Use `gru resume {}` to retry, or inspect the branch to recover the work.",
            minion_id, commits_ahead, branch_line, minion_id
        );
        try_post_issue_comment(host, owner, repo, num, &comment).await;
    }

    true
}

/// Cleans up orchestration state after a post-agent failure. Without this,
/// a failure between the agent exiting and the worker exiting leaves the
/// issue with `gru:in-progress` and no live process — an "orphaned label"
/// recoverable only by the multi-hour auto-recovery scan.
///
/// This helper:
/// 1. Posts an explanatory comment on the issue (always)
/// 2. Transitions the issue label `gru:in-progress` → `gru:failed` (only when
///    `label_eagerly_on_failure()` returns true — deferred when lab spawned this process)
/// 3. Clears the PID and marks the minion as `Stopped` in the registry
///
/// Currently wired only to PR creation failures in `run_worker`. The PR
/// lifecycle monitoring phase (`monitor_pr_phase`) handles its own label
/// transitions (to `gru:blocked`), so invoking this helper there would
/// double-label the issue.
///
/// Fire-and-forget: logs on failure but does not propagate errors, since the
/// caller is already returning an error of its own.
pub(crate) async fn cleanup_post_agent_failure(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: Option<u64>,
    minion_id: &str,
    reason: &str,
) {
    if let Some(num) = issue_num {
        let comment = if label_eagerly_on_failure() {
            format!(
                "⚠️  Minion `{}` failed after the agent phase: {}\n\n\
                 Use `gru resume {}` to retry.",
                minion_id, reason, minion_id
            )
        } else {
            format!(
                "⚠️  Minion `{}` failed after the agent phase: {}\n\n\
                 `gru lab` will retry automatically. To retry manually instead, \
                 use `gru resume {}`.",
                minion_id, reason, minion_id
            )
        };
        try_post_issue_comment(host, owner, repo, num, &comment).await;
        if label_eagerly_on_failure() {
            try_mark_issue_failed(host, owner, repo, num).await;
        } else {
            log::debug!(
                "GRU_RETRY_PARENT=lab: deferring gru:failed for issue #{} — \
                 lab's retry queue will decide",
                num
            );
        }
    }

    let mid = minion_id.to_string();
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
        })
    })
    .await
    {
        log::warn!(
            "⚠️  Failed to clear registry state for {}: {:#}",
            minion_id,
            e
        );
    }
}

/// Attempts to unclaim an issue by restoring `gru:todo` and removing `gru:in-progress`.
/// Fire-and-forget: logs on failure but does not propagate errors.
/// Used when `--discuss` abort needs to reverse a `claim_issue` call.
pub(super) async fn try_unclaim_issue(host: &str, owner: &str, repo: &str, issue_num: u64) {
    match crate::github::edit_labels_via_cli(
        host,
        owner,
        repo,
        issue_num,
        &[crate::labels::TODO],
        &[crate::labels::IN_PROGRESS],
    )
    .await
    {
        Ok(()) => {
            println!(
                "🏷️  Restored '{}' label on issue #{}",
                crate::labels::TODO,
                issue_num
            );
        }
        Err(e) => {
            log::warn!("⚠️  Failed to unclaim issue #{}: {}", issue_num, e);
        }
    }
}

/// Posts an explanatory comment on an issue (fire-and-forget).
/// Logs a warning if posting fails but does not propagate the error.
pub(crate) async fn try_post_issue_comment(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) {
    if let Err(e) = crate::github::post_comment_via_cli(host, owner, repo, issue_num, body).await {
        log::warn!("⚠️  Failed to post comment on issue: {}", e);
    }
}

/// Posts a progress comment to the issue via CLI (fire-and-forget).
pub(super) async fn try_post_progress_comment(
    host: &str,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) -> bool {
    match crate::github::post_comment_via_cli(host, owner, repo, issue_num, body).await {
        Ok(()) => true,
        Err(e) => {
            log::warn!("⚠️  Failed to post progress comment: {:#}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    // These tests verify the exact message strings produced at each blocked/escalation call site.
    // They are format-string snapshot tests, not behavioral tests of the async helpers.
    use crate::agent_runner::AgentRunnerError;
    use std::time::Duration;

    use super::{label_eagerly_on_failure, GRU_RETRY_PARENT_ENV, GRU_RETRY_PARENT_VALUE};

    #[test]
    fn eager_label_when_env_unset() {
        temp_env::with_var_unset(GRU_RETRY_PARENT_ENV, || {
            assert!(label_eagerly_on_failure());
        });
    }

    #[test]
    fn no_eager_label_when_env_set() {
        temp_env::with_var(GRU_RETRY_PARENT_ENV, Some(GRU_RETRY_PARENT_VALUE), || {
            assert!(!label_eagerly_on_failure());
        });
    }

    #[test]
    fn eager_label_when_env_has_unexpected_value() {
        temp_env::with_var(GRU_RETRY_PARENT_ENV, Some("1"), || {
            assert!(label_eagerly_on_failure());
        });
    }

    #[test]
    fn test_blocked_reason_inactivity_stuck() {
        let minion_id = "M042";
        let err = AgentRunnerError::InactivityStuck { minutes: 15 };
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: No activity for 15 minutes - task appears stuck. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_stream_timeout() {
        let minion_id = "M042";
        let err = AgentRunnerError::StreamTimeout { seconds: 300 };
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: Timeout: agent process hasn't produced output in 300 seconds. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_max_timeout() {
        let minion_id = "M042";
        let err = AgentRunnerError::MaxTimeout(Duration::from_secs(600));
        let reason = format!(
            "Minion `{}` stopped: {}. Human intervention required.",
            minion_id, err
        );
        assert_eq!(
            reason,
            "Minion `M042` stopped: Task exceeded maximum timeout of 600s. Human intervention required."
        );
    }

    #[test]
    fn test_blocked_reason_ci_exhausted() {
        let pr_number = "123";
        let reason = format!(
            "CI auto-fix failed after {} attempts. See PR #{} for details. Human intervention required.",
            crate::ci::MAX_CI_FIX_ATTEMPTS,
            pr_number
        );
        assert_eq!(
            reason,
            format!(
                "CI auto-fix failed after {} attempts. See PR #123 for details. Human intervention required.",
                crate::ci::MAX_CI_FIX_ATTEMPTS
            )
        );
    }

    #[test]
    fn test_post_agent_failure_comment_format_standalone() {
        let minion_id = "M1gt";
        let reason = "PR creation failed: no PR was created";
        let comment = format!(
            "⚠️  Minion `{}` failed after the agent phase: {}\n\n\
             Use `gru resume {}` to retry.",
            minion_id, reason, minion_id
        );
        assert_eq!(
            comment,
            "⚠️  Minion `M1gt` failed after the agent phase: PR creation failed: no PR was created\n\n\
             Use `gru resume M1gt` to retry."
        );
    }

    #[test]
    fn test_post_agent_failure_comment_format_lab() {
        let minion_id = "M1gt";
        let reason = "PR creation failed: no PR was created";
        let comment = format!(
            "⚠️  Minion `{}` failed after the agent phase: {}\n\n\
             `gru lab` will retry automatically. To retry manually instead, \
             use `gru resume {}`.",
            minion_id, reason, minion_id
        );
        assert_eq!(
            comment,
            "⚠️  Minion `M1gt` failed after the agent phase: PR creation failed: no PR was created\n\n\
             `gru lab` will retry automatically. To retry manually instead, use `gru resume M1gt`."
        );
    }

    /// Sets up a tiny git repo with `main` as base and a feature branch with
    /// `extra` commits ahead. The `origin` remote points to a bare clone so
    /// that `origin/main` resolves locally.
    fn setup_test_repo_ahead(dir: &std::path::Path, extra: usize) -> std::path::PathBuf {
        use std::process::Command as StdCmd;
        let run = |args: &[&str], cwd: &std::path::Path| {
            let out = StdCmd::new("git")
                .args(args)
                .current_dir(cwd)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .output()
                .expect("git failed");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        let bare = dir.join("origin.git");
        let status = StdCmd::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .expect("git init --bare failed to spawn");
        assert!(status.success(), "git init --bare exited {:?}", status);

        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        run(&["init", "-b", "main"], &wt);
        run(&["config", "user.email", "t@t"], &wt);
        run(&["config", "user.name", "t"], &wt);
        run(&["remote", "add", "origin", bare.to_str().unwrap()], &wt);
        std::fs::write(wt.join("base.txt"), "base").unwrap();
        run(&["add", "."], &wt);
        run(&["commit", "-m", "base"], &wt);
        run(&["push", "-u", "origin", "main"], &wt);
        run(&["checkout", "-b", "feature"], &wt);
        for i in 0..extra {
            std::fs::write(wt.join(format!("f{}.txt", i)), "x").unwrap();
            run(&["add", "."], &wt);
            run(&["commit", "-m", &format!("c{}", i)], &wt);
        }
        wt
    }

    #[tokio::test]
    async fn test_count_commits_ahead_with_new_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 2);
        let n = super::count_commits_ahead(&wt, "refs/remotes/origin/main").await;
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn test_resolve_base_ref_prefers_remote_then_local() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 1);

        // feature branch is checked out; local `main` exists and so does
        // `refs/remotes/origin/main` — remote should win.
        let base = super::resolve_base_ref(&wt, "main").await;
        assert_eq!(base.as_deref(), Some("refs/remotes/origin/main"));

        // Missing branch returns None.
        let missing = super::resolve_base_ref(&wt, "nope").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_resolve_base_ref_falls_back_to_local_head() {
        // Repo with a local `refs/heads/main` but no `refs/remotes/origin/main`
        // — mimics Gru's bare-repo-worktree layout where the default branch
        // is fetched into refs/heads/<base>, not refs/remotes/origin/<base>.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        use std::process::Command as StdCmd;
        let run = |args: &[&str]| {
            let out = StdCmd::new("git")
                .args(args)
                .current_dir(dir)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "a").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "a"]);

        let base = super::resolve_base_ref(dir, "main").await;
        assert_eq!(base.as_deref(), Some("refs/heads/main"));
    }

    #[tokio::test]
    async fn test_push_branch_pushes_to_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 1);
        let result = super::push_branch(&wt, "feature").await;
        assert!(result.is_ok(), "push_branch failed: {:?}", result.err());

        // Verify the ref now exists in the bare origin.
        use std::process::Command as StdCmd;
        let bare = tmp.path().join("origin.git");
        let out = StdCmd::new("git")
            .args([
                "-C",
                bare.to_str().unwrap(),
                "show-ref",
                "--verify",
                "refs/heads/feature",
            ])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "feature ref missing in bare origin");
    }

    #[tokio::test]
    async fn test_push_branch_from_detached_head() {
        // Detach HEAD, then push — the explicit `HEAD:refs/heads/<branch>`
        // refspec should still advance the remote branch.
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 1);
        use std::process::Command as StdCmd;
        let out = StdCmd::new("git")
            .args(["-C", wt.to_str().unwrap(), "checkout", "--detach", "HEAD"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git checkout --detach failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let result = super::push_branch(&wt, "feature").await;
        assert!(result.is_ok(), "push_branch failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_count_commits_ahead_bad_ref_returns_zero() {
        // Bogus base ref → rev-list fails → function returns 0 rather than erroring.
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 1);
        let n = super::count_commits_ahead(&wt, "refs/remotes/origin/does-not-exist").await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn test_count_commits_ahead_no_new_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = setup_test_repo_ahead(tmp.path(), 0);
        let n = super::count_commits_ahead(&wt, "refs/remotes/origin/main").await;
        assert_eq!(n, 0);
    }

    #[test]
    fn test_blocked_reason_judge_escalated() {
        let pr_number = "456";
        let reason = format!(
            "Merge judge escalated PR #{} for human review. See PR for details.",
            pr_number
        );
        assert_eq!(
            reason,
            "Merge judge escalated PR #456 for human review. See PR for details."
        );
    }
}
