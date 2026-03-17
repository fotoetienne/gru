# Outstanding Bugs — Lab Daemon & PR Monitoring

**Date:** 2026-03-17
**Scope:** `gru lab`, `gru status`, `gru review`, `pr_monitor`, `ci`, and PR creation
**Discovered via:** Investigation of PRs #452, #453, #454 on `fotoetienne/gru` — minions spawned by `gru lab` that disappeared from `gru status` and accumulated redundant code reviews
**Prior report:** `plans/LAB_DAEMON_BUGS.md` (2026-03-16) — all 7 bugs resolved on `main`. Bug 6's fix introduced new Bug 1 below.

---

## Summary

After a `gru lab` session created PRs #452–#454, all three minions vanished from `gru status` despite having open PRs. Two PRs accumulated 5–7 redundant self-reviews. One PR received a false CI failure escalation while CI was actually passing. A fourth issue — duplicate PR creation on an already-merged branch — was also observed.

These bugs fall into four categories:
1. **Registry lifecycle** — minions with open PRs are pruned too aggressively
2. **Review feedback loop** — self-reviews are mistaken for external feedback, and self-review can never satisfy merge-readiness
3. **CI monitoring** — premature failure detection, redundant monitoring passes, and stale labels
4. **PR creation** — no guard against duplicate PRs on already-merged branches

---

## Bug 1: Registry entries lost for minions with open PRs

**Severity:** High
**Files:** `src/commands/lab.rs:792-810` (`prune_stale_entries`), `src/commands/status.rs:131-148` (`handle_status`)

### Problem

Both `prune_stale_entries()` (lab) and `handle_status()` (status) remove registry entries where `!info.worktree.exists()`. Neither checks whether the minion has an **open PR** before deleting.

The exact chain that caused the disappearance is unclear — the worktrees for M0rm, M0ru, and M0rr still exist on disk, so `prune_stale_entries` alone cannot explain the loss. Possible paths:
- A worktree was temporarily inaccessible (filesystem race, NFS issue) when the check ran
- An earlier `gru status` or lab poll ran before the worktrees were fully created
- The lab session exited and a new session's pruning saw stale state

Regardless of the trigger, the core issue is the same: **no pruning path checks `info.pr` before deleting**, so any transient worktree absence permanently destroys the registry entry for a minion with an open PR.

### Evidence

Minions M0rm, M0ru, and M0rr all created PRs (#452, #453, #454) but are absent from `~/.gru/state/minions.json`. Their worktrees still exist under `~/.gru/work/fotoetienne/gru/minion/`.

### Suggested fix

Extract pruning logic into a shared function used by both lab and status. Before removing any entry where `info.pr.is_some()`, check whether that PR is still open on GitHub. Only prune if the PR is merged, closed, or absent. Note: the `with_registry` closure is synchronous, so this requires a two-phase approach (collect candidates → release lock → check GitHub → re-acquire lock → remove confirmed stale entries).

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
- All reviews use `COMMENTED` state (not `APPROVED`), so GitHub never considers the PR approved, further preventing the loop from terminating via merge-readiness (see Bug 7)

### Suggested fix

Filter reviews in `poll_once` where `review.user.login` matches the PR author (available via `pr.user.login` or `gh api user --jq '.login'`). This is the highest-impact single fix. As defense-in-depth, also skip `invoke_agent_for_reviews` when inline comments are empty and the review author is the PR author.

---

## Bug 3: PR monitor reports CI failure before all checks complete

**Severity:** High
**Files:** `src/pr_monitor.rs:590-596` (`poll_once` — CI check)

### Problem

`poll_once` calls `get_check_runs()` and immediately reports `FailedChecks` if any check has a failed conclusion — even while other checks are still `in_progress` or `queued`. The `is_failed_check` function (line 172) checks `conclusion` but ignores `status`, so it cannot distinguish between a definitively failed suite and a partially-completed one.

Contrast with `ci::wait_for_ci` (`src/ci.rs`) which correctly waits for `checks.iter().all(|c| c.status == CheckStatus::Completed)` before evaluating failures. The `CheckRun` struct already has `status: CheckStatus` available — no API change needed.

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
**Files:** `src/commands/fix/pr.rs:59-136` (`create_pr_for_issue`)

### Problem

The PR creation path in `create_pr_for_issue` does not check whether a PR already exists (open or merged) for the head branch before calling `gh pr create`.

### Evidence

- PR #450: merged at 03:56:06Z on branch `minion/issue-445-M0rm`
- PR #452: created at 03:56:18Z on the same branch — duplicate, now stuck open with `gru:blocked`

The 12-second gap suggests the minion's monitor detected the merge, re-entered the PR creation path, and created a new PR on the now-merged branch.

### Suggested fix

Before calling `gh pr create`, check for existing PRs on the head branch: `gh pr list --head <branch> --state all`. If a merged or open PR exists, skip creation.

---

## Bug 5: Redundant CI monitoring causes double escalation

**Severity:** Medium
**Files:** `src/commands/fix/mod.rs:350-394` (`run_worker`), `src/commands/fix/monitor.rs:523` (lifecycle CI handling), `src/commands/fix/monitor.rs:712` (`monitor_ci_after_fix`)

### Problem

CI is monitored **twice** in the worker flow:

1. **Inside `monitor_pr_lifecycle`** (mod.rs line 352): When `poll_once` returns `FailedChecks`, it calls `ci::monitor_and_fix_ci` internally (monitor.rs:523)
2. **After `monitor_pr_lifecycle` exits** (mod.rs line 370): `monitor_ci_after_fix` runs the same `ci::monitor_and_fix_ci` flow again on the same commit (monitor.rs:712)

Combined with Bug 3 (premature failure detection), a single false positive can trigger up to **4 CI fix attempts** (2 per `monitor_and_fix_ci` invocation × 2 invocations).

Note: when `no_watch` is true (mod.rs:338-347), the function returns early before `monitor_pr_lifecycle`, so `monitor_ci_after_fix` is the only CI check — this path is correct. The redundancy only occurs when `no_watch` is false AND a PR exists.

### Suggested fix

Gate `monitor_ci_after_fix` at line 370 so it only runs when `pr_number` is `None` (i.e., `monitor_pr_lifecycle` was skipped). When `monitor_pr_lifecycle` ran, CI was already handled internally.

---

## Bug 6: `gru status` silently deletes registry entries (destructive read)

**Severity:** Medium
**Files:** `src/commands/status.rs:131-148` (`handle_status`)

### Problem

`handle_status` — a read-only command — has a destructive side-effect: it removes registry entries for any minion whose worktree directory no longer exists on disk (lines 131-148). This means simply running `gru status` after a manual `rm -rf` of a worktree permanently destroys the registry entry with no confirmation or recovery path.

Users run `gru status` frequently to check on their minions. A command that destroys state on read violates the principle of least surprise and compounds Bug 1.

### Suggested fix

Move stale-entry cleanup out of `handle_status` and into `gru clean` only. In `handle_status`, display stale entries with a visual indicator (e.g., "(stale)") instead of silently deleting them.

---

## Bug 7: Self-reviews structurally cannot satisfy merge-readiness gate

**Severity:** Medium
**Files:** `src/prompt_loader.rs:162-168` (review prompt), `src/merge_readiness.rs:386` (readiness evaluation)

### Problem

The built-in review prompt detects when the agent is reviewing its own PR and falls back to `gh pr review --comment` (COMMENTED state) instead of `--approve` (APPROVED state). This is correct for GitHub's constraint — you cannot approve your own PR.

However, `merge_readiness.rs:386` explicitly ignores COMMENTED reviews when evaluating the `review_approved` gate. Only APPROVED and CHANGES_REQUESTED reviews change reviewer state. Since the minion is always the PR author, self-review can never produce an APPROVED state, meaning the merge-readiness gate is structurally unsatisfiable for autonomous minions.

### Evidence

PRs #453 and #454 both have passing CI and multiple positive reviews, but none with APPROVED state — the PR is stuck in a state where it can never become merge-ready via self-review alone.

### Suggested fix

Options (not mutually exclusive):
1. Skip the `review_approved` check for minion-authored PRs when a self-review with positive sentiment exists
2. Have the minion bypass self-review entirely and rely on the merge judge (`src/merge_judge.rs`)
3. Use a separate reviewer identity (bot account) that can approve the PR

---

## Bug 8: `gru:blocked` label never removed after CI recovery

**Severity:** Low
**Files:** `src/ci.rs:671-702` (label addition), `src/commands/fix/monitor.rs:533` (CI success path)

### Problem

When CI fails and escalation occurs, `ci.rs:671-702` adds the `gru:blocked` label to the issue. However, when CI subsequently recovers (either via auto-fix or external action), no code path removes the label. In `monitor.rs:533`, after `monitor_and_fix_ci` returns `Ok(true)` (CI fixed), the code logs success and continues monitoring but never calls `github::edit_labels` to remove `gru:blocked`.

Searching the codebase for `gru:blocked` removal yields zero results — the label is only ever added, never removed programmatically.

### Evidence

PR #454 is labeled `gru:blocked` despite all CI checks passing. The label is stale from a transient CI failure that has since resolved.

### Suggested fix

After `ci::monitor_and_fix_ci` returns `Ok(true)`, remove the `gru:blocked` label from the issue. Also consider adding label cleanup to the merge-readiness check — if all gates pass, `gru:blocked` should be removed.

---

## Dependency Graph & Recommended Fix Order

```
Bug 2 (self-review loop) ──────────┐
                                    ├──▶ Bug 7 (COMMENTED can't approve)
Bug 3 (premature CI failure) ──┐   │
                                ├──▶ Bug 5 (double CI monitoring)
                                │   │
                                └──▶ Bug 8 (stale blocked label)
Bug 1 (registry pruning) ──────┐
                                ├──▶ Shared: PR-aware pruning function
Bug 6 (status destructive read) ┘
Bug 4 (duplicate PR) ──────────── Independent
```

**Recommended fix order:**

1. **Bug 2** (self-review loop) — highest user-visible impact, causes review spam on every PR
2. **Bug 3** (premature CI failure) — causes false escalations that block PRs
3. **Bug 5** (double CI) — quick fix, gate on `pr_number.is_none()`, compounds Bug 3
4. **Bug 1 + Bug 6** (registry pruning) — fix together with shared PR-aware pruning function
5. **Bug 7** (merge-readiness gate) — design decision needed on approach
6. **Bug 4** (duplicate PR) — race condition, lower frequency
7. **Bug 8** (stale label) — low severity, quick fix alongside Bug 3

---

## Appendix: Immediate PR Cleanup Needed

| PR | Issue | Action |
|----|-------|--------|
| #452 | Duplicate of merged #450 | Close as duplicate |
| #453 | 7 redundant reviews, CI passing | Approve and merge |
| #454 | Stale `gru:blocked` label, CI passing | Remove label, approve and merge |
