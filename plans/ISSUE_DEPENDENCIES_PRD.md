# Issue Dependencies for Gru

**Date:** 2026-03-17
**Status:** Final Draft
**Author:** Product Manager (assisted by research agents)
**Reviews:** 5 independent reviews (2x technical, 1x product/UX, 1x PM, 1x TPM)

---

## Problem Statement

**Who:** Developers using Gru to autonomously work through GitHub issue backlogs.

**Pain:** Minions don't understand issue ordering. They'll happily claim an issue that depends on unfinished work, waste tokens attempting it, and produce a PR that can't be merged because prerequisite changes don't exist yet. This is especially painful when running `gru lab` in daemon mode — the whole point is hands-off operation, but dependency-unaware scheduling forces human intervention.

**Impact:**
- Wasted compute and tokens on doomed work
- Broken PRs that reference code/APIs that don't exist yet
- Human must manually sequence issues or babysit the queue
- Undermines trust in autonomous operation

---

## Context: Current State

Gru already has **partial** dependency support:

| Capability | Status | Location |
|-----------|--------|----------|
| Dependency convention in issue bodies | ✅ In use | `**Blocked by:** #X, #Y` |
| Dependency parsing (Python) | ✅ Implemented | `scripts/pm.py` |
| Critical path analysis | ✅ Implemented | `scripts/pm.py` |
| `/next_issue` command | ✅ Delegates to pm.py | `.claude/commands/next_issue.md` |
| Search filter for native deps | ✅ Already in code | `-is:blocked` in `list_ready_issues_via_cli` |
| Runtime dependency enforcement | ❌ Not implemented | — |
| Minion claim-time checking | ❌ Not implemented | — |
| `gru:blocked` label for dependencies | ❌ Used for escalation only | `src/labels.rs` |

The gap is clear: **we can analyze dependencies for humans, but Minions are blind to them.**

### Three Layers of Dependency Checking

Gru has access to three complementary layers for detecting blocked issues:

| Layer | What it checks | Cost | Reliability |
|-------|---------------|------|-------------|
| **Search filter** (`-is:blocked`) | Native GitHub deps via search index | Free (already in list query) | Eventually consistent — can miss recent changes |
| **Body parsing** (`**Blocked by:** #X`) | Text convention in issue bodies | Free (body already fetched) | Only catches body-text deps, can drift if edited |
| **Native REST API** (`GET .../blocked_by`) | GitHub's authoritative dependency state | 1 API call per issue | Strongly consistent — source of truth |

**Layer 1 is already running.** The existing `-is:blocked` search qualifier in `list_ready_issues_via_cli` filters out most natively-blocked issues, but it uses an eventually consistent search index and cannot be trusted as the sole check.

**This PRD adds Layers 2 and 3** to close the gaps.

---

## Proposed Solution

A layered approach: parse issue bodies first (free), then verify unblocked candidates via the native REST API before claiming.

### How the Check Works

```
For each candidate issue:
  1. Parse body for **Blocked by:** #X, #Y  (free — body already fetched)
  2. If body says blocked → skip immediately, no API call needed
  3. If body says unblocked → call native API to verify
     - API returns blockers → skip (caught a dep set via GitHub UI)
     - API returns empty → proceed to claim
     - API returns 404 → no native dep support (GHES), trust body parse, proceed
     - API returns 403/500 → warn, treat as unblocked, proceed (don't block the pipeline on API errors)
```

**Why this order:** Body parsing is free (data already fetched by `resolve_issue()`). The API call only fires for issues that pass the body check — i.e., issues we're about to claim anyway. In a typical backlog where many issues are body-blocked, this avoids most API calls entirely.

### User Stories

- **As a developer running `gru lab`**, I want Minions to skip issues whose dependencies aren't resolved, so that they only work on issues that can actually succeed.
- **As a developer using `gru do`**, I want to see a warning when an issue has open blockers, so that I can decide whether to proceed.
- **As a developer who sets dependencies in GitHub's UI**, I want Gru to respect those native dependencies, not just body-text conventions.
- **As a developer who just closed a blocking issue**, I expect the dependent issue to become claimable on the next lab poll cycle (not immediately — no real-time push).

### What This Does NOT Do (Intentionally)

- No new labels — native API provides `is:blocked` state; body-text deps don't need labels
- No real-time reactivity — next poll cycle picks up changes
- No circular dependency detection — perpetually skipped issues are visible in logs
- No cross-repo dependencies
- No transitive dependency checking — direct blockers only
- No interactive prompt — `gru do` warns and proceeds (user explicitly chose the issue)

### Conflict Resolution Policy

When both body text and native API provide dependency info:
- **Native API takes precedence** when it returns 200 (even if body says unblocked)
- **Body text is the sole source** when API returns 404 (GHES without the feature)
- **Body text is never combined with API results** — it's one source or the other, not a union

---

## Core Principles Check

| Principle | Assessment |
|-----------|-----------|
| ✅ **Local-first** | No new external service. Uses existing GitHub API dependency, consistent with all other Gru operations. |
| ✅ **One binary** | No new services or binaries. Logic lives in Gru's Rust code. |
| ✅ **GitHub as state** | Dependencies stored in GitHub (native API or issue bodies). No local DB. |
| ✅ **Stateless Tower** | Tower doesn't need to know about dependencies. Labs handle it. |
| ✅ **No inter-lab coordination** | Each Lab independently checks GitHub for dependency status. Duplicate checks are fine — they converge on the same answer. |

---

## MVP Scope

**In scope:**
- [ ] Parse `**Blocked by:** #X, #Y` from issue bodies (body already fetched)
- [ ] Verify unblocked candidates via native REST API (`GET .../dependencies/blocked_by`)
- [ ] Skip blocked issues in `gru lab` polling loop
- [ ] Warn on blocked issues in `gru do` (print warning, proceed — user explicitly chose this issue)
- [ ] `--ignore-deps` flag to suppress warning in `gru do`
- [ ] Log skip reasons clearly in lab mode
- [ ] Log prominently when all candidates are blocked

**Out of scope (future issues):**
- [ ] `gru status --deps` display (use `pm.py blocked` for now)
- [ ] `/decompose` setting native dependencies on created sub-issues
- [ ] Circular dependency detection
- [ ] Cross-repo dependencies
- [ ] Critical path display in `gru lab` output
- [ ] Dependency format migration tooling
- [ ] GHES-specific label (`gru:waiting`) for environments without `is:blocked`

---

## Detailed Design

### Module Structure

Create `src/dependencies.rs` with two layers, cleanly separated:

**Layer 1: Body text parsing (sync, pure, zero dependencies):**
```rust
/// Parses "**Blocked by:** #X, #Y" from issue body text.
/// Returns issue numbers that block this issue. Empty vec = no body-text deps.
/// Skips cross-repo references (owner/repo#123) with a log warning.
pub fn parse_blockers_from_body(body: &str) -> Vec<u64>
```

Hand-rolled parsing with `str::find`/`str::split`. No `regex` crate dependency.
Supports `**Blocked by:** #X, #Y` format only (matching existing `pm.py` convention).

**Layer 2: Native API verification (async, calls `github.rs`):**
```rust
/// Calls GET /repos/{owner}/{repo}/issues/{number}/dependencies/blocked_by
/// Returns issue numbers of open blockers. Empty vec = unblocked.
/// Returns Ok(vec![]) on 404 (GHES without feature) — caller falls back to body parse.
/// Returns Ok(vec![]) on 403/500 — warns, treats as unblocked (don't block pipeline on API errors).
pub async fn get_blockers_via_api(owner: &str, repo: &str, issue_number: u64) -> Result<Vec<u64>>
```

**Expected API response shape** (array of issue objects):
```json
[
  {"id": 123456, "number": 40, "state": "open", "title": "..."},
  {"id": 123457, "number": 41, "state": "closed", "title": "..."}
]
```
Filter to `state == "open"` and extract `number` field. Use `gh api ... --jq '[.[] | select(.state == "open") | .number]'` to do this server-side.

**Combined check (the main entry point):**
```rust
/// Body parse first (free), then API verify if body says unblocked.
/// This is the function callers should use.
pub async fn get_blockers(
    owner: &str, repo: &str, issue_number: u64, body: &str
) -> Result<Vec<u64>> {
    let body_blockers = parse_blockers_from_body(body);
    if !body_blockers.is_empty() {
        return Ok(body_blockers);
    }
    // Body says unblocked — verify via native API
    get_blockers_via_api(owner, repo, issue_number).await
}
```

### Integration Points

**1. `gru do` — `handle_fix()` in `src/commands/fix/mod.rs`:**

Insert dependency check between `resolve_issue()` (line ~444) and `check_existing_minions()`. Issue body is available via `issue_ctx.details.body`.

```
1. resolve_issue()              -- fetches issue details including body
2. NEW: get_blockers()          -- body parse + API verify
3. NEW: if blocked →
   - Print warning to stderr: "Warning: #42 is blocked by open issues: #40, #41"
   - Proceed anyway (user explicitly chose this issue)
   - --ignore-deps suppresses the warning
4. check_existing_minions()
5. setup_worktree()
6. claim_issue()
7. spawn_worker()               -- must forward --ignore-deps flag
```

**2. `gru do` worker — `run_worker()` in `src/commands/fix/mod.rs`:**

The worker (line ~133) also calls `resolve_issue()`. The worker should **NOT** re-check dependencies — the foreground process already validated. The `--ignore-deps` flag is forwarded to suppress the check in the worker path.

**3. `gru do` CLI plumbing:**

- Add `ignore_deps: bool` to `FixOptions` in `src/commands/fix/types.rs`
- Add `--ignore-deps` flag to the `Do` command in `src/main.rs` (line ~74)
- Forward `--ignore-deps` in `spawn_worker()` (line ~39 of `mod.rs`) — **this is easy to miss** and would cause a redundant check in the background worker

**4. `gru lab` — `poll_and_spawn()` in `src/commands/lab.rs`:**

Insert `get_blockers()` call inside the existing candidate loop, between `is_issue_claimed()` (line ~677) and `claim_issue_via_cli()` (line ~682).

To get the issue body in the lab path: add `body` to the `--json` fields in `list_ready_issues_via_cli()` in `github.rs`. This is a one-line change — bodies come back for free in the existing list call. No extra API calls needed for body parsing.

The native API verify call (for candidates that pass body parsing) adds one `gh api` subprocess per unblocked candidate (~200-500ms each). For 50 candidates with 10 body-blocked, that's ~40 API calls adding 8-20 seconds to the poll cycle. Acceptable for typical workloads; could parallelize with `tokio::join!` or cache within a poll cycle if needed.

**5. Resume — `resume_interrupted_minions()` in `lab.rs`:**

Resumed Minions should **NOT** re-check dependencies. They already started work. A dependency added after work began is not a reason to abort.

**6. Non-interactive detection:**

Use `std::io::stdin().is_terminal()` (stable since Rust 1.70) to detect whether `gru do` is running interactively. When non-interactive, the warning is printed to stderr but execution proceeds (same as interactive). The `--ignore-deps` flag suppresses the warning entirely.

### No New Labels Needed

For github.com users: the native API provides `is:blocked` state visible in GitHub UI.

For GHES users without the native API: body-text dependencies have no UI representation. If GHES users report that they need visible feedback, a `gru:waiting` label can be added in a future iteration. This is explicitly deferred — not rejected.

---

## Known Risks and Limitations

1. **Search index lag:** `-is:blocked` in GitHub search is eventually consistent. The body-parse + API-verify pattern mitigates this for all cases where the body or native API reflects the dependency. A small race window remains between API check and claim — benign, worst case is a Minion starts work on a just-blocked issue.

2. **API call performance in lab polling:** For N unblocked candidate issues, N sequential `gh api` subprocesses are spawned (~200-500ms each). At 50 candidates, this adds 10-25 seconds per poll cycle. Mitigations if needed: parallelize with `tokio::join!`, cache blocker results within a poll cycle, or use GraphQL to batch.

3. **Body-text fragility:** If a user edits an issue body and removes the `**Blocked by:**` line, Gru falls through to the API check. If native deps are also not set, the issue is treated as unblocked. This is a known limitation of the body-text convention — no reconciliation mechanism.

4. **Cross-repo references:** Not supported. Parser skips `owner/repo#123` with a warning. Does not error.

5. **Circular dependencies:** Not detected. If present, affected issues are perpetually skipped in lab mode (body or API always returns blockers). Visible in skip logs. Future: visited-set check, warn, apply `gru:blocked`.

6. **Transitive dependencies:** MVP checks direct blockers only. If #42 → #41 → #40, Gru only checks #42's direct blocker (#41). If #41 is closed but #40 is still open, Gru will claim #42. In practice this is usually acceptable: if #41 was closed, its work was merged, and #42 can likely build on it. True transitive enforcement is a future enhancement.

7. **GHES availability:** Native dependencies API may not be available on all GHES versions. The body-parsing layer provides coverage. The API verify step gracefully degrades (404 → skip verify, trust body parse).

8. **Non-404 API errors:** A 403 (permissions) or 500 (server error) from the dependencies endpoint is NOT treated as "feature unavailable." These are logged as warnings and the issue is treated as unblocked (don't block the entire pipeline on transient API errors). Only 404 triggers the body-parsing fallback.

9. **`--ignore-deps` forwarding:** The flag must be forwarded in `spawn_worker()` to the background worker process. If missed, the worker re-runs the dependency check unnecessarily. Documented here as a known gotcha for implementers.

---

## Resolved Questions

1. ~~**Should `gru:blocked-by-dep` be auto-applied?**~~ **No.** Native API provides `is:blocked` state. For GHES, deferred — add `gru:waiting` label if users request it.
2. ~~**Partial dependency progress?**~~ **No.** Binary: blocked or unblocked. Keep it simple.
3. ~~**PR dependencies?**~~ **No.** Only closed issues count as resolved. A PR can be reverted, fail CI, or get reworked. "Simple before clever."
4. ~~**Dependency format migration?**~~ **Deferred.** Not needed for MVP.
5. ~~**GHES detection:**~~ **Try the API, detect 404, fall back to body parsing.** No need to inspect the host URL.
6. ~~**Lab idle behavior:**~~ **Yes, log prominently.** "All N candidate issues blocked — nothing to claim this cycle."
7. ~~**Rate limit budget:**~~ **No.** Don't make it configurable. Default to no explicit limit. Add a circuit breaker only if someone hits rate limits. Premature optimization otherwise.
8. ~~**Interactive prompt for `gru do`?**~~ **No prompt.** User explicitly chose the issue — warn and proceed. `--ignore-deps` suppresses the warning. Avoids TTY detection complexity and three code paths.
9. ~~**Non-404 error handling?**~~ **Warn and treat as unblocked.** 403/500 are logged but don't block claiming. Only 404 triggers body-parsing fallback.
10. ~~**Native API vs body text conflict?**~~ **Native API wins** when it returns 200. Body text is sole source only on 404.
11. ~~**Resumed Minions re-check deps?**~~ **No.** They already started work.
12. ~~**Worker re-checks deps?**~~ **No.** Foreground already validated. `--ignore-deps` forwarded to suppress.

## Open Questions

None. All questions resolved.

---

## Success Metrics

- **North Star: Minion success rate.** Percentage of Minion runs that result in a mergeable PR, before vs. after this feature ships. This is what we're trying to improve.
- **Blocked-skip count:** Number of issues skipped due to dependencies per day, as a rate of total candidates evaluated. (Directly measurable from logs/events.)
- **False-skip rate:** Number of times a user uses `--ignore-deps` and the work succeeds. (Qualitative signal from user feedback — full automation of this metric is deferred.)
- **Blocked-issue dwell time:** Proxy: number of `gru:todo` issues that have been skippable for >24 hours based on lab poll logs.

---

## Implementation Plan

### Phase 1: Body Parsing + `gru do` Integration (~1 issue)
- Create `src/dependencies.rs` with `parse_blockers_from_body()` (sync, pure)
- Add `get_blockers_via_api()` (async, calls `gh api`)
- Wire `get_blockers()` (combined) into `handle_fix()` between `resolve_issue()` and `check_existing_minions()`
- Add `ignore_deps: bool` to `FixOptions` in `types.rs`
- Add `--ignore-deps` flag to `Do` command in `main.rs`
- Forward `--ignore-deps` in `spawn_worker()`
- Skip dependency check in `run_worker()` path
- Unit tests for body parsing (various formats, empty body, cross-repo refs)
- Unit tests for API response parsing

### Phase 2: Lab Integration (~1-2 issues)
- Add `body` to `--json` fields in `list_ready_issues_via_cli()` (one-line change)
- Add `get_blockers()` call in `poll_and_spawn()` between `is_issue_claimed()` and `claim_issue_via_cli()`
- Skip dependency check in `resume_interrupted_minions()`
- Log prominently when all candidates are blocked
- Integration test for lab skip behavior

> **Note:** Phases 2 and 3 can be parallelized if two developers are available. Phase 2 only depends on Phase 1's `dependencies.rs` module being merged.

### Phase 3: GHES Hardening (~1 issue, can parallelize with Phase 2)
- Verify 404 fallback works correctly end-to-end
- Test on GHES environment if available
- Document body-text convention for GHES users
- Consider `gru:waiting` label for GHES if user feedback warrants it

---

## Appendix A: Background Research — GitHub API Support

### Native GitHub Issue Dependencies

GitHub shipped issue dependencies (blocking/blocked-by) as GA in **August 2025**.

**REST API endpoints:**

| Action | Method | Endpoint |
|--------|--------|----------|
| List blocked-by | `GET` | `/repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by` |
| Add blocked-by | `POST` | `/repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by` |
| Remove blocked-by | `DELETE` | `/repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by/{issue_id}` |
| List blocking | `GET` | `/repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocking` |

**Limits:** Up to 50 issues per relationship direction. Webhook support included.

**Important:** The POST endpoint for adding blocked-by requires the issue's internal `id` (not the issue `number`). Resolve via: `gh api /repos/OWNER/REPO/issues/NUMBER --jq .id`

### Sub-Issues (Related but Distinct)

GitHub also shipped sub-issues (parent/child hierarchy) in **April 2025**.

REST endpoints exist for add, remove, list, and reprioritize sub-issues. GraphQL requires the `GraphQL-Features: sub_issues` header. Limits: 100 sub-issues per parent, 8 levels of nesting.

### `gh` CLI Support

**Neither sub-issues nor dependencies have native `gh` CLI commands yet.**

- Sub-issues CLI request: [cli/cli#10298](https://github.com/cli/cli/issues/10298) (93+ upvotes, `needs-product` label)
- Dependencies CLI request: [cli/cli#11757](https://github.com/cli/cli/issues/11757)

Both require workarounds via `gh api`:

```bash
# Check what blocks issue #42
gh api /repos/OWNER/REPO/issues/42/dependencies/blocked_by

# Add dependency (requires issue internal id, not number)
ISSUE_ID=$(gh api /repos/OWNER/REPO/issues/41 --jq .id)
gh api /repos/OWNER/REPO/issues/42/dependencies/blocked_by \
  -f issue_id="$ISSUE_ID"
```

### GHES Availability

- **Sub-issues + issue types:** Available in GHES 3.18+ (GA October 2025)
- **Issue dependencies:** Not confirmed for GHES yet. Likely requires GHES 3.19 or later.

### Community Extensions

- [`gh-sub-issue`](https://github.com/yahsan2/gh-sub-issue) — manage parent/child relationships
- [`gh-sub-issues`](https://github.com/d-oit/gh-sub-issues) — hierarchical issue management
- [`gh-pm`](https://github.com/yahsan2/gh-pm) — project management with sub-issue decomposition

---

## Appendix B: Alternatives Considered

### Alternative 1: Native API Only (No Body Parsing)

Use GitHub's native dependency API exclusively, with no body-parsing layer.

**Not selected because:**
- Misses dependencies encoded in issue bodies (the existing Gru convention)
- Makes N API calls per poll cycle even for body-blocked issues
- No fallback for GHES without the feature

**Our approach uses body parsing as a free first-pass filter**, reducing API calls to only the candidates that appear unblocked.

### Alternative 2: Body Parsing Only (No Native API)

Use the `**Blocked by:** #X, #Y` body convention as the only source.

**Not selected because:**
- Misses dependencies set via GitHub's UI (not reflected in body text)
- Doesn't fix the search index lag problem — issues blocked via native deps but not body text slip through
- Dependencies aren't visible in GitHub's UI

**Body parsing is our first layer**, but the native API verify catches deps that body text misses.

### Alternative 3: Native API First, Body Parsing as GHES Fallback

Lead with the native API for all candidates; fall back to body parsing only on 404.

**Not selected because:**
- Makes an API call for every candidate, even those body parsing could have caught for free
- Higher latency per poll cycle (N calls vs only unblocked-count calls)

**Our approach is strictly better**: body parsing catches the easy cases for free, API verifies the rest.

### Alternative 4: GitHub Projects Custom Fields

Use GitHub Projects to track dependencies via custom fields.

**Rejected because:**
- Couples Gru to GitHub Projects (not all users use Projects)
- No webhook for Projects field changes — requires polling
- More complex API surface (Projects v2 GraphQL)
- Violates simplicity principle

### Alternative 5: External Dependency Store

Use a local SQLite database or file to track dependencies.

**Rejected because:**
- Violates "GitHub as database" principle
- Creates sync problems between local state and GitHub
- Adds infrastructure dependency
- Goes against Gru's design philosophy

### Alternative 6: Labels for Dependencies

Use labels like `depends-on:42` to encode dependencies.

**Rejected because:**
- Label names are limited in length
- Creates label pollution (one label per dependency relationship)
- Hard to parse and maintain
- Doesn't scale beyond a handful of dependencies

### Alternative 7: Structured Comments

Use hidden HTML comments or YAML frontmatter in issue bodies.

```markdown
<!-- gru:dependencies -->
blocked-by: #42, #43
<!-- /gru:dependencies -->
```

**Rejected because:**
- The `**Blocked by:**` format is already in use and human-readable
- Structured comments add complexity for marginal benefit
- Could revisit if we need machine-writable dependencies

---

## Appendix C: Current Gru Dependency Infrastructure

### `scripts/pm.py` — Project Manager Tool

The Python-based PM tool already parses dependencies:

```python
# Regex pattern (line ~37 of pm.py)
# Parses: **Blocked by:** #X, #Y
```

Capabilities:
- Fetches all issues via `gh issue list --json`
- Extracts `**Blocked by:** #X, #Y` from issue bodies
- Calculates critical path via recursive memoization
- Identifies ready vs. blocked issues
- Commands: `status`, `next`, `blocked`, `critical-path`, `graph`

### `src/labels.rs` — Label Constants

Current labels (dependency-relevant):
- `gru:todo` — Issue ready for claiming
- `gru:in-progress` — Minion working
- `gru:blocked` — Needs human intervention (NOT dependency-related)
- `gru:done` — Completed
- `gru:failed` — Failed

### `docs/DECISIONS.md` — Future Design

The decisions doc outlines a planned dependency format:

```markdown
## Dependencies
- depends-on: #123
- blocks: #456
```

With planned behavior: parse on claim, check status, wait if unresolved, apply `blocked` label. This format is not yet in use; the PRD defers it in favor of the existing `**Blocked by:**` convention.

### `scripts/add_dependencies.sh` — One-Time Setup

A shell script that was used to populate the V0 project dependency graph across 21 issues organized into 5 phases. Demonstrates the body-editing pattern for setting dependencies.

---

## Appendix D: Review History

This PRD went through 5 independent reviews across two rounds.

### Round 1 (2 reviews)
- **Technical implementation review** (senior Rust engineer): Identified correct integration points, flagged lab body-fetching gap, recommended hand-rolled parsing over regex, suggested `--ignore-deps` naming.
- **Product/UX review** (product designer): Flagged feedback loop gap for lab users, missing user stories, need for concrete metrics, silent failure anti-pattern.

### Round 2 (3 reviews)
- **Product manager review**: Flagged missing golden-path user story, conflict resolution policy, non-404 error handling, North Star metric. Suggested dropping `gru status --deps` from MVP and Phase 4 from PRD.
- **TPM review**: Identified missing integration points (`spawn_worker` flag forwarding, `run_worker` skip, `resume_interrupted_minions` skip). Flagged Phase 2 effort underestimate, API response format gap, non-interactive detection need. Noted Phases 2+3 can parallelize.
- **Tech lead review (simplicity)**: Advocated body parsing first (40 lines, zero API calls, ships immediately). Recommended dropping interactive prompt, dropping `gru status --deps`, adding `body` to existing list query. Challenged native-API-first as YAGNI.

### Key Decisions from Reviews
- **Option D adopted**: Body parse first (free), API verify unblocked candidates only. Balances simplicity (tech lead) with correctness (PM).
- **No interactive prompt**: Warn and proceed for `gru do`. User explicitly chose the issue.
- **`gru status --deps` deferred**: `pm.py blocked` covers this. Not needed for MVP.
- **Phase 4 (`/decompose`) removed**: Separate future issue. Not required for core value.
- **All open questions resolved**: Rate limits (YAGNI), error handling (warn+proceed), conflict resolution (API wins), etc.
