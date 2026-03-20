---
description: Run a Code Quality Improvement Plan audit and file issues for findings
allowed-tools: Bash(gh issue:*), Bash(gh api:*), Bash(git:*), Read, Glob, Grep, Agent, Write, Edit, TodoWrite, AskUserQuestion
---

Run a Code Quality Improvement Plan (CQIP) — a structured codebase audit that identifies test gaps, dead code, duplication, testability issues, and code complexity, then files actionable GitHub issues for all findings.

**Instructions:**

## Phase 1: Audit (parallel agents)

Spawn **two auditor agents in parallel** using the Agent tool:

### Agent 1: Test Auditor
Read every `#[cfg(test)]` module and `#[test]` function across the codebase. For each:
- **Classify** each test as high/medium/low value
- **Identify tests to delete**: trivial tests, tests that test mocks not behavior, tests with no assertions
- **Identify tests testing the wrong thing**: tests that re-implement production logic locally, tests that inline production format strings
- **Identify dead code kept alive by tests**: production code with zero non-test callers
- **Identify critical missing coverage**: untested code paths most likely to break in production (error handling, state transitions, async loops, timeout machinery)
- **Identify tests to improve**: tests with no assertions, tests using real CLI tools instead of mocks

Report findings with specific file paths and line numbers, grouped by module.

### Agent 2: Architecture Auditor
Read all source files and audit for:
- **Testability**: Where traits should be added for dependency injection, functions mixing business logic with I/O, global state blocking parallel tests
- **Long functions**: Functions >80 lines with multiple responsibilities — list file:line, length, suggested splits
- **Duplication**: Repeated patterns across modules that should be extracted into helpers
- **Dead code**: `#[allow(dead_code)]` items, unused public API surface, commented-out code
- **API surface**: `pub` items that should be `pub(crate)` in a binary crate

Report findings with specific file paths and line numbers, grouped by category.

## Phase 2: Synthesize

After both agents report back, synthesize their findings into a CQIP document at `plans/CQIP_<today's date>.md` with these sections:

### Section 1: Test Audit
- Tests to remove/simplify (with file:line references)
- Tests to add (prioritized by risk)
- Tests to improve

### Section 2: Testability Improvements
- Dependency injection opportunities (what to change, why, effort S/M/L)
- Function splitting opportunities
- Global state issues

### Section 3: Code Simplification
- Long functions to split (table: file:line, length, suggested split)
- Duplication to extract (specific patterns with all locations)
- Dead code to remove (itemized with line numbers)
- API surface reduction (pub → pub(crate) candidates)

### Section 4: Prioritized Execution Plan
Group findings into phases, ordered by dependency:
- **Phase 0**: Bug fixes (ship immediately)
- **Phase 1**: Test cleanup (delete low-value tests, dead code)
- **Phase 2**: Quick wins (extract helpers, sweep pub→pub(crate))
- **Phase 3**: Add critical tests (close coverage gaps)
- **Phase 4**: Testability refactors (split monoliths, add traits)
- **Phase 5**: Behavioral fixes (correctness improvements)

Each phase must be independently shippable. Note dependencies between items.

## Phase 3: Review against main

Before finalizing, verify findings against the latest code:
- Check if any referenced items were already fixed
- Update line numbers that shifted
- Mark resolved items with ~~strikethrough~~
- Add any new issues discovered

## Phase 4: File issues

For each outstanding item in the CQIP:
1. Create a GitHub issue with `gh issue create`:
   - Title: clear, imperative description of the change
   - Body: description, specific file:line references, acceptance criteria, reference to CQIP doc
   - Labels: `cqip`, `gru:todo`, plus relevant labels (`bug`, `tests`, `dead-code`, `duplication`, `refactor`, `testability`, `api-surface`)
2. Use heredoc for issue body to preserve formatting

After all issues are created, analyze dependencies between them:
- Which issues touch the same files and would cause merge conflicts if done in parallel?
- Which issues must be done in a specific order (e.g., delete dead code before refactoring the same module)?

For each dependency, set it using **both layers**:
1. Prepend `**Blocked by:** #X, #Y` to the blocked issue's body via `gh issue edit`
2. Set native GitHub dependency:
   ```bash
   BLOCKER_ID=$(gh api /repos/OWNER/REPO/issues/BLOCKER --jq .id)
   gh api /repos/OWNER/REPO/issues/BLOCKED/dependencies/blocked_by -f issue_id="$BLOCKER_ID"
   ```

**NEVER use the sub-issues/addSubIssue GraphQL API for dependencies.**

## Phase 5: Summary

Report to the user:
- Total issues filed with issue number range
- Dependency graph (text diagram)
- How many items per phase
- Which items are immediately actionable (no blockers)

## Notes
- This is a READ-ONLY audit followed by issue creation — no code changes
- All findings must reference specific file paths and line numbers
- Use `gru:todo` label on all issues so they're picked up by lab daemon
- Previous CQIPs are in `plans/` for reference (e.g., `plans/CQIP_2026-03-17.md`)
- Use parallel agents wherever possible to minimize wall-clock time
