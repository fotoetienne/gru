---
description: Run a Code Quality Improvement Plan audit (read-only) and file issues for findings
allowed-tools: Bash, Read, Glob, Grep, Agent, Write, Edit, TodoWrite, AskUserQuestion
---

Run a Code Quality Improvement Plan (CQIP) — a structured codebase audit that identifies test gaps, dead code, duplication, testability issues, and code complexity, then files actionable GitHub issues for all findings.

**No source code is modified during this process.**

**Instructions:**

## Phase 0: Detect Language & Check for Existing CQIP Issues

1. Detect the project language by checking for `Cargo.toml` (Rust), `package.json` (JS/TS), `pyproject.toml`/`setup.py` (Python), `build.gradle` (Java/Kotlin), etc. Adapt the audit instructions in Phase 1 to use language-appropriate constructs (e.g., `#[test]` for Rust, `describe/it` for JS, `def test_` for Python).

2. Check for existing open CQIP issues:
   ```bash
   gh issue list --label cqip --state open --json number,title
   ```
   If open CQIP issues exist, note them. In Phase 4, skip filing issues that duplicate existing open ones.

## Phase 1: Audit (parallel agents)

Spawn **two auditor agents in parallel** using the Agent tool. Each agent should write its findings to a temp file, then send a summary message back.

### Agent 1: Test Auditor
Read every test module and test function across the codebase. For each:
- **Classify** each test as high/medium/low value
- **Identify tests to delete**: trivial tests, tests that test mocks not behavior, tests with no assertions
- **Identify tests testing the wrong thing**: tests that re-implement production logic locally, tests that inline production format strings
- **Identify dead code kept alive by tests**: production code with zero non-test callers
- **Identify critical missing coverage**: untested code paths most likely to break in production (error handling, state transitions, async loops, timeout machinery)
- **Identify tests to improve**: tests with no assertions, tests using real CLI tools instead of mocks

Report findings with specific file paths and line numbers, grouped by module.

### Agent 2: Architecture Auditor
Read all source files and audit for:
- **Testability**: Where interfaces/traits should be added for dependency injection, functions mixing business logic with I/O, global state blocking parallel tests
- **Long functions**: Functions >80 lines with multiple responsibilities — list file:line, length, suggested splits
- **Duplication**: Repeated patterns across modules that should be extracted into helpers
- **Dead code**: Unused items, unused public API surface, commented-out code
- **API surface**: Overly broad visibility (e.g., `pub` items that should be `pub(crate)` in a binary crate)

Report findings with specific file paths and line numbers, grouped by category.

## Phase 2: Synthesize

After both agents report back, synthesize their findings into a CQIP document at `plans/CQIP_<today's date>.md` with these sections:

### Section 1: Test Audit
- Tests to remove/simplify (with file:line references)
- Tests to add (prioritized by risk)
- Tests to improve
- If no findings in a subsection, include the header with "No findings."

### Section 2: Testability Improvements
- Dependency injection opportunities (what to change, why, effort S/M/L)
- Function splitting opportunities
- Global state issues

### Section 3: Code Simplification
- Long functions to split (table: file:line, length, suggested split)
- Duplication to extract (specific patterns with all locations)
- Dead code to remove (itemized with line numbers)
- API surface reduction candidates

### Section 4: Prioritized Execution Plan
Group findings into phases, ordered by dependency:
- **Phase 0**: Bug fixes (ship immediately)
- **Phase 1**: Test cleanup (delete low-value tests, dead code)
- **Phase 2**: Quick wins (extract helpers, reduce API surface)
- **Phase 3**: Add critical tests (close coverage gaps)
- **Phase 4**: Testability refactors (split monoliths, add traits)
- **Phase 5**: Behavioral fixes (correctness improvements)

Each phase must be independently shippable. Note dependencies between items.

**For each finding, list the files affected** (e.g., "Files: `src/foo.rs`, `src/bar.rs`") — this is used later for dependency analysis.

## Phase 3: Review against main

Before finalizing, verify findings against the latest code:
```bash
git fetch origin main
```
- Compare findings against `origin/main` to check if any items were already fixed
- Update line numbers that shifted
- Mark resolved items with ~~strikethrough~~
- Add any new issues discovered

## Phase 4: File issues

First, cross-reference findings against any existing open CQIP issues from Phase 0. Skip duplicates.

For each outstanding item, create a GitHub issue with `gh issue create`:
- Title: clear, imperative description of the change
- Body (use heredoc): description, specific file:line references, acceptance criteria, reference to CQIP doc
- Labels: `cqip`, `gru:todo`, plus relevant labels (`bug`, `tests`, `dead-code`, `duplication`, `refactor`, `testability`, `api-surface`)
- If the item has dependencies on other items being filed, include `**Blocked by:** #X, #Y` in the body at creation time (not as a separate edit step)

After all issues are created, analyze dependencies using the "Files affected" data from Phase 2:
- Which issues touch the same files and would cause merge conflicts if done in parallel?
- Which issues must be done in a specific order (e.g., delete dead code before refactoring the same module)?

For each dependency, set the native GitHub dependency:
```bash
BLOCKER_ID=$(gh api /repos/OWNER/REPO/issues/BLOCKER_NUMBER --jq .id)
gh api /repos/OWNER/REPO/issues/BLOCKED_NUMBER/dependencies/blocked_by \
  -f issue_id="$BLOCKER_ID"
```

If the blocked issue was created without `**Blocked by:**` in the body (because the blocker issue hadn't been created yet), prepend it now:
```bash
BODY=$(gh issue view BLOCKED_NUMBER --json body -q .body)
gh issue edit BLOCKED_NUMBER --body "**Blocked by:** #BLOCKER_NUMBER

$BODY"
```

Handle errors: 404 from the native API means GHES without dependency support — log a warning and continue, the body text is sufficient.

**NEVER use the sub-issues/addSubIssue GraphQL API for dependencies.**

## Phase 5: Summary

Report to the user:
- Total issues filed with issue number range
- Dependency list: each blocked issue as `#N blocked by #M, #P`
- How many items per phase
- Which items are immediately actionable (no blockers)

## Notes
- **No source code is modified** — this is a read-only audit followed by issue creation
- All findings must reference specific file paths and line numbers
- Use `gru:todo` label on all issues so they're picked up by lab daemon
- Previous CQIPs are in `plans/` for reference (e.g., `plans/CQIP_2026-03-17.md`)
- Use parallel agents wherever possible to minimize wall-clock time
