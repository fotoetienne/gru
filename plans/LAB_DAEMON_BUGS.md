# Lab Daemon Bugs

**Date:** 2026-03-16
**Scope:** `gru lab`, `gru status`, `gru clean`, and supporting modules
**Discovered via:** Manual testing of `gru lab` with 3 repos, observing resume behavior and stale state

---

## Summary

The lab daemon has a cluster of related bugs around minion lifecycle tracking. The root problem is that gru has no reliable way to determine if a minion process is truly alive, and it never checks GitHub state (PR merged, issue closed) before resuming or protecting minions. This leads to:

- Merged-PR minions being resumed on every lab startup
- `gru status` showing phantom "running" minions for 20+ hours
- `gru clean` refusing to remove worktrees for merged PRs
- Wasted agent slots on completed work

---

## Bug 1: Lab resumes minions for merged PRs

**Files:** `src/commands/lab.rs` (`find_resumable_minions`, `resume_interrupted_minions`)

`find_resumable_minions()` decides a minion is resumable based on:
- Process is not running (mode is Stopped or PID is dead)
- Orchestration phase is active (RunningAgent, CreatingPr, MonitoringPr)
- Worktree still exists on disk
- Repo is in the lab config

It never checks whether the associated PR was already merged or the issue was closed. Minions for issues #414 and #430 (both with merged PRs) were resumed on every lab startup.

**Observed behavior:**
```
🔄 Found 3 resumable Minion(s) from previous session
♻️  Resuming M0r6 (issue #130, corp/ste-slackdgs, phase: RunningAgent)
...
♻️  Resuming M0r5 (issue #430, fotoetienne/gru, phase: RunningAgent)
...
♻️  Resuming M0r3 (issue #414, fotoetienne/gru, phase: RunningAgent)
```

Both #430 and #414 had merged PRs at this point.

**Fix:** Before resuming, check the issue/PR state via `gh`. If the PR is merged or the issue is closed, mark the minion as `Completed` and skip it. This could be a lightweight `gh issue view --json state` call.

---

## Bug 2: PID liveness check doesn't account for PID reuse

**File:** `src/minion_registry.rs` (`is_process_alive`)

```rust
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid_i32, 0) == 0 }
}
```

`kill(pid, 0)` checks if *any* process with that PID exists. After a minion process exits, the OS can recycle its PID for an unrelated process (browser tab, system daemon, etc.). `is_process_alive` then returns true for a PID that belongs to something completely different.

This is the root cause of bugs 3 and 4 below.

**Observed behavior:**
```
M0r1  running (interactive)  20h
M0r2  running (interactive)  20h
M0r4  running (interactive)  20h
```

These minions have been "running" for 20 hours. Either their processes are genuinely stuck, or (more likely) the PIDs were recycled.

**Possible fixes (in order of robustness):**
1. **Verify process command name** — check `/proc/<pid>/comm` (Linux) or use `sysctl`/`proc_pidpath` (macOS) to confirm the PID belongs to a `gru` or `claude` process.
2. **Store process start time** — record the PID's creation timestamp at spawn time, then compare against the OS-reported start time. If they differ, the PID was recycled.
3. **Staleness check** — if `last_activity` is older than a threshold (e.g., 2 hours) and the minion isn't in `MonitoringPr` phase, consider the process dead regardless of `kill()` result.
4. **PID file with lock** — write a lockfile that the child holds; if the lock is released, the process is dead regardless of PID state.

---

## Bug 3: `gru status` shows phantom running minions

**File:** `src/commands/status.rs` (`format_mode_display`)

Direct consequence of Bug 2. `gru status` calls `is_process_alive(pid)` to determine display mode. With recycled PIDs, dead minions show as "running (interactive)" indefinitely.

The status command does try to fix stale PIDs (status.rs:156-168) by marking dead processes as Stopped, but this detection relies on the same flawed `is_process_alive` check.

**Impact:** Misleading status display. Users see minions as "running" that finished hours ago.

---

## Bug 4: `gru clean` can't remove worktrees for merged PRs

**File:** `src/commands/clean.rs` (lines 262-324, 443)

`gru clean` partitions registry entries into "active" and "stopped" based on PID liveness. Worktrees with "active" minions are unconditionally skipped (line 443):

```rust
if active_minion_worktrees.contains(&canonical_wt_path) {
    skipped_active_minions.push(wt);
    continue;
}
```

Because Bug 2 makes dead minions appear alive, their worktrees are protected from cleanup. Even if PID detection were correct, `gru clean` doesn't check PR/issue state for active minions — a minion whose PR is merged should be cleanable regardless.

**Observed behavior:**
```
Skipped 4 worktree(s) with active minions:

  M0r2 (issue #414) (active minion)  ← PR #429 is merged
  M0r4 (issue #430) (active minion)  ← PR #431 is merged
  ...

No cleanable worktrees found
```

**Fix:** Two layers:
1. Fix PID detection (Bug 2) so these show as stopped.
2. For active minions, check if the PR is merged / issue is closed. If so, kill the process and mark the worktree as cleanable.

---

## Bug 5: "Found N resumable" log is misleading

**File:** `src/commands/lab.rs` (line 390)

On each poll cycle, `find_resumable_minions()` returns all resumable candidates, including ones already resumed this session. The count is printed before filtering:

```
🔄 Found 3 resumable Minion(s) from previous session  ← printed 4 times
```

Then most are skipped:
```
[WARN] ⏭️  Skipping M0r5: already resumed this session
[WARN] ⏭️  Skipping M0r3: already resumed this session
```

**Fix:** Either subtract `resumed_this_session` before printing, or don't print when all candidates are already handled.

---

## Bug 6: `prune_stale_entries` doesn't handle completed work

**File:** `src/commands/lab.rs` (`prune_stale_entries`, lines 711-730)

Registry pruning only removes entries whose worktree path no longer exists on disk. Minions with merged PRs that still have worktrees on disk are never pruned, so they keep appearing as "resumable" on every poll cycle.

**Fix:** Pruning should also check orchestration phase — entries in terminal states (`Completed`, `Failed`) should be pruned regardless of worktree existence. Alternatively, the `gru do` / worker flow should clean up the worktree and mark completion when a PR is merged, which would let the existing prune logic work.

---

## Bug 7: Lab resume uses `gru do` instead of `gru resume`

**File:** `src/commands/lab.rs` (`resume_interrupted_minions`, line 462)

When resuming an interrupted minion, the lab calls `spawn_minion()` which runs `gru do <issue_url>`. This goes through the full `handle_fix` flow: resolve issue, check existing minions, and then (hopefully) auto-resume the stopped minion via `check_existing_minions`.

This is an indirect path. `gru do` could create a *new* minion instead of resuming the existing one if the existing minion's worktree was cleaned up between finding and spawning. A more direct approach would be to invoke `gru resume <minion_id>`.

---

## Dependency Graph

```
Bug 2 (PID reuse)
  ├── Bug 3 (phantom running in status)
  ├── Bug 4 (clean can't remove merged worktrees)
  └── Bug 1 (partially — find_resumable relies on PID check too)

Bug 1 (no GitHub state check on resume)
  └── Bug 6 (stale entries never pruned)

Bug 5 (misleading log) — independent
Bug 7 (gru do vs gru resume) — independent
```

**Recommended fix order:**
1. Bug 2 — fix PID detection (unblocks 3 and 4)
2. Bug 1 — add GitHub state check before resume (the highest-impact user-facing fix)
3. Bug 4 — clean should check PR state for active minions
4. Bugs 5, 6, 7 — smaller improvements
