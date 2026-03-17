# Outstanding Bugs — Lab Daemon & PR Monitoring

**Date:** 2026-03-17
**Scope:** `gru lab`, `gru status`, `gru review`, `pr_monitor`, `ci`, and PR creation
**Discovered via:** Investigation of PRs #452, #453, #454 on `fotoetienne/gru` — minions spawned by `gru lab` that disappeared from `gru status` and accumulated redundant code reviews
**Prior report:** `plans/LAB_DAEMON_BUGS.md` (2026-03-16) — all 7 bugs resolved on `main`. Bug 6's fix introduced new Bug 1 below.

---

## Summary

After a `gru lab` session created PRs #452–#454, all three minions vanished from `gru status` despite having open PRs. Two PRs accumulated 5–7 redundant self-reviews. One PR received a false CI failure escalation while CI was actually passing. A fourth issue — duplicate PR creation on an already-merged branch — was also observed.

These bugs fall into three categories:
1. **Registry lifecycle** — minions with open PRs are pruned too aggressively
2. **Review feedback loop** — self-reviews are mistaken for external feedback
3. **CI monitoring** — premature failure detection and redundant monitoring passes

---

## Bug 1: Lab prunes completed minions with open PRs from registry

**Severity:** High
**Files:** `src/commands/lab.rs:792-810` (`prune_stale_entries`)

### Problem

`prune_stale_entries()` runs on every lab poll cycle and removes registry entries whose worktrees no longer exist on disk. Separately, completed/failed minions with dead processes are eligible for pruning. However, there is no check for whether the minion has an **open PR** that still needs monitoring, review responses, or CI attention.

When a minion finishes its `monitor_pr_lifecycle` loop (e.g., timeout, max review rounds, or monitoring interrupted) and the process exits, the next lab poll cycle prunes the entry. The worktree survives (protected by `gru clean`'s open-PR check), but the registry entry is gone.

### Evidence

Minions M0rm, M0ru, and M0rr all created PRs (#452, #453, #454) but are absent from `~/.gru/state/minions.json`. Their worktrees still exist under `~/.gru/work/fotoetienne/gru/minion/`.

### Suggested fix

Before pruning a terminal-phase entry, check if `info.pr` is set and whether that PR is still open on GitHub. Only prune if the PR is merged, closed, or absent.

---

## Bug 2: Self-review feedback loop causes redundant reviews

**Severity:** High
**Files:** `src/pr_monitor.rs:562-581` (`poll_once` — review detection), `src/commands/fix/monitor.rs` (review handling)

### Problem

The PR monitor's `poll_once` function detects reviews by checking `r.submitted_at >= *last_check_time`. It does not filter by author — so the minion's own self-review (posted via `gru review` → `gh pr review --comment`) is detected as a new external review on the next poll cycle.

This triggers `MonitorResult::NewReviews`, which resumes the agent to "address" the feedback. The agent sees its own LGTM review, posts another LGTM review, and the cycle repeats.

### Evidence

- PR #453: **7 self-reviews** by `fotoetienne` over ~6 minutes, all saying "LGTM / no issues found"
- PR #454: **5 self-reviews** over ~6 minutes with the same pattern
- All reviews use `COMMENTED` state (not `APPROVED`), so GitHub never considers the PR approved, further preventing the loop from terminating via merge-readiness

### Suggested fix

1. Filter reviews in `poll_once` where `review.user.login` matches the authenticated user (the PR author)
2. Skip `invoke_agent_for_reviews` when the `NewReviews` payload contains zero inline comments (body-only reviews don't require code changes)
3. Add explicit "submit exactly one review" instruction to the review prompt

---

## Bug 3: PR monitor reports CI failure before all checks complete

**Severity:** High
**Files:** `src/pr_monitor.rs:590-596` (`poll_once` — CI check)

### Problem

`poll_once` calls `get_check_runs()` and immediately reports `FailedChecks` if any check has a failed conclusion — even while other checks are still `in_progress` or `queued`. This is premature: a failed check may be superseded by a re-run, or the overall suite may still be running.

Contrast with `ci::wait_for_ci` (`src/ci.rs`) which correctly waits for `checks.iter().all(|c| c.status == CheckStatus::Completed)` before evaluating failures.

### Evidence

PR #452 timeline:
- **03:56:07Z**: Check run 1 started
- **03:56:24Z**: Check run 2 started
- **03:57:05Z**: Check run 1 completed (success)
- **03:57:11Z**: Escalation comment posted — "2/2 attempts failed", check conclusion "(failure)"
- **03:57:23Z**: Check run 2 completed (success)

Both checks passed. The escalation was a false positive.

### Suggested fix

Only report `FailedChecks` when all check runs have `status == Completed`. If any are still `in_progress` or `queued`, return `None` and re-check on the next poll cycle.

---

## Bug 4: Duplicate PR created on already-merged branch

**Severity:** Medium
**Files:** `src/commands/fix/` (PR creation path)

### Problem

Minion M0rm created PR #452 on branch `minion/issue-445-M0rm` at 03:56:18Z — **12 seconds after** PR #450 on the same branch was merged at 03:56:06Z. The PR creation path does not check whether a PR already exists (open or merged) for the head branch.

### Evidence

- PR #450: merged at 03:56:06Z on branch `minion/issue-445-M0rm`
- PR #452: created at 03:56:18Z on the same branch — duplicate, now stuck open with `gru:blocked`

### Suggested fix

Before calling `gh pr create`, check for existing PRs on the head branch: `gh pr list --head <branch> --state all`. If a merged or open PR exists, skip creation.

---

## Bug 5: Redundant CI monitoring causes double escalation

**Severity:** Medium
**Files:** `src/commands/fix/mod.rs:350-394` (`run_worker`)

### Problem

CI is monitored **twice** in the worker flow:

1. **Inside `monitor_pr_lifecycle`** (line 352): When `poll_once` returns `FailedChecks`, it calls `ci::monitor_and_fix_ci` internally
2. **After `monitor_pr_lifecycle` exits** (line 370): `monitor_ci_after_fix` runs the same `ci::monitor_and_fix_ci` flow again on the same commit

This can cause the same CI failure to be escalated twice, wasting fix attempts and posting duplicate escalation comments.

### Suggested fix

Remove the `monitor_ci_after_fix` call at line 370, or gate it so it only runs when `monitor_pr_lifecycle` was skipped (i.e., when `pr_number` is `None`).

---

## Bug 6: `gru status` silently deletes registry entries

**Severity:** Low
**Files:** `src/commands/status.rs:131-148` (`handle_status`)

### Problem

`handle_status` — a read-only command — has a destructive side-effect: it removes registry entries for any minion whose worktree directory no longer exists on disk (lines 131-148). This means simply running `gru status` after a manual `rm -rf` of a worktree permanently destroys the registry entry.

This compounds Bug 1: once lab prunes the worktree reference, the next `gru status` call finishes the job by removing the registry entry entirely.

### Suggested fix

Move stale-entry cleanup out of `handle_status` and into `gru clean` only. Alternatively, log a warning instead of silently deleting, so the user can decide.

---

## Appendix: Immediate PR Cleanup Needed

| PR | Issue | Action |
|----|-------|--------|
| #452 | Duplicate of merged #450 | Close as duplicate |
| #453 | 7 redundant reviews, CI passing | Approve and merge |
| #454 | Stale `gru:blocked` label, CI passing | Remove label, approve and merge |
