# GitHub Issues for Gru Self-Implementation

This document contains the GitHub issues that Gru will implement on itself after the initial bootstrap (Gru 0.1) is complete.

Each issue follows a standard format optimized for AI agents:
- Clear acceptance criteria
- Links to relevant design docs
- Specific file paths when applicable
- Observable validation steps

## How to Use

1. Complete bootstrap (Gru 0.1) manually
2. Create these issues in your Gru repository
3. Add label `ready-for-minion` to issues you want Gru to work on
4. Start `gru lab` and let it claim and implement

**Recommended order:** Start with Phase 1 issues (infrastructure), then Phase 2 (core features), then Phase 3 (polish).

---

## Phase 1: Infrastructure & Multi-Slot Support

### Issue #1: Add support for multiple concurrent Minions (slots > 1)

**Labels:** `ready-for-minion`, `enhancement`, `priority:high`

**Description:**

Currently Gru 0.1 only supports a single Minion at a time. Add support for configurable concurrent Minions (slots).

**Requirements:**

1. Update Scheduler to track multiple active Minions in a HashMap
2. Check available slots before claiming issues
3. Allow configuring `lab.slots` in config.yaml (default: 2)
4. Each Minion gets unique ID (M001, M002, etc.)
5. `gru minions list` shows all active Minions

**Acceptance Criteria:**

- [ ] Can configure `lab.slots: 3` in config.yaml
- [ ] Lab claims up to 3 issues concurrently
- [ ] Each gets unique Minion ID and tmux session
- [ ] `gru minions list` shows all 3 active Minions
- [ ] No race conditions claiming same issue twice

**Files to modify:**
- `src/scheduler.rs` - Add slot tracking
- `src/lab.rs` - Manage multiple Minions
- `src/config.rs` - Add slots field

**Reference:** See DESIGN.md section "Scheduler"

---

### Issue #2: Implement automatic PR creation after initial commits

**Labels:** `ready-for-minion`, `enhancement`, `priority:high`

**Description:**

Minions should automatically create draft PRs after their first commit, not require manual creation.

**Requirements:**

1. After Minion makes first commit and pushes branch
2. Call GitHub API to create draft PR
3. Title format: `[DRAFT] Fixes #<issue> - <issue title>`
4. Body includes Minion ID, link to issue, auto-generated description
5. Store PR number in Minion struct
6. Post comment on issue with PR link

**Acceptance Criteria:**

- [ ] Minion creates draft PR automatically after first push
- [ ] PR properly links issue with "Fixes #123"
- [ ] PR marked as draft initially
- [ ] Minion knows its own PR number
- [ ] Comment posted to issue: "Draft PR opened: #456"

**Files to modify:**
- `src/minion.rs` - Add PR creation logic after first commit
- `src/github.rs` - Ensure create_draft_pr() works correctly

**Reference:** See DESIGN.md section "Draft PR as Lock Mechanism"

---

### Issue #3: Add proper structured logging with tracing

**Labels:** `ready-for-minion`, `enhancement`, `priority:medium`

**Description:**

Currently minimal logging. Add structured logging with tracing/tracing-subscriber for better observability.

**Requirements:**

1. Add tracing and tracing-subscriber dependencies
2. Configure subscriber in main.rs with RUST_LOG env var
3. Replace println! with appropriate log levels:
   - `info!` for normal operations
   - `debug!` for detailed traces
   - `warn!` for recoverable errors
   - `error!` for failures
4. Include structured fields (minion_id, issue_number, repo)
5. Log to both stdout and file (~/.gru/logs/gru.log)

**Acceptance Criteria:**

- [ ] `RUST_LOG=debug gru lab` shows detailed traces
- [ ] Logs include structured fields (minion_id, issue, etc.)
- [ ] Logs written to ~/.gru/logs/gru.log
- [ ] No more println! in production code
- [ ] Log levels appropriate for each message

**Files to modify:**
- `Cargo.toml` - Add tracing deps
- `src/main.rs` - Configure subscriber
- All modules - Replace println! with tracing macros

**Reference:** See DESIGN.md section "Observability - Structured Logging"

---

### Issue #4: Implement event logging to archive (events.jsonl)

**Labels:** `ready-for-minion`, `enhancement`, `priority:medium`

**Description:**

Add structured event logging to ~/.gru/archive/<MINION_ID>/events.jsonl for audit trail.

**Requirements:**

1. Create MinionEvent enum with variants: Claim, Progress, Done, Failed
2. Each event serializes to JSON line
3. Append to events.jsonl in archive directory
4. Also post formatted events as GitHub comments (YAML frontmatter)
5. Events include timestamps, minion_id, and event-specific data

**Acceptance Criteria:**

- [ ] Events written to ~/.gru/archive/M001/events.jsonl
- [ ] Valid JSONL format (one JSON object per line)
- [ ] Events also posted as GitHub comments with YAML frontmatter
- [ ] Archive directory created when Minion starts
- [ ] Can parse events.jsonl to reconstruct timeline

**Files to create:**
- `src/events.rs` - MinionEvent enum and formatting

**Files to modify:**
- `src/minion.rs` - Log events at key lifecycle points
- `src/github.rs` - Helper to format YAML comments

**Reference:** See DESIGN.md sections "Event Types (YAML Comments)" and "Event Log (events.jsonl)"

---

## Phase 2: Core Features

### Issue #5: Implement CI monitoring and waiting for check runs

**Labels:** `ready-for-minion`, `enhancement`, `priority:high`

**Description:**

After Minion pushes commits, it should wait for GitHub Actions CI to complete and react to results.

**Requirements:**

1. After push, get commit SHA
2. Poll GitHub Checks API every 10 seconds
3. Wait for all check runs to complete (max 30 min timeout)
4. If all pass, proceed
5. If any fail, fetch logs and attempt to fix
6. Track ci_runs metric in MinionMetrics

**Acceptance Criteria:**

- [ ] Minion waits for CI after pushing
- [ ] Detects when all checks complete
- [ ] Distinguishes pass vs fail
- [ ] Fetches failure logs from GitHub
- [ ] Timeout after 30 minutes with clear error

**Files to create:**
- `src/ci.rs` - CI monitoring logic

**Files to modify:**
- `src/github.rs` - Add get_check_runs(), get_check_run_logs()
- `src/minion.rs` - Call CI monitor after push

**Reference:** See DESIGN.md sections "CI/CD Integration" and "Monitoring Check Runs"

---

### Issue #6: Implement retry logic for CI failures

**Labels:** `ready-for-minion`, `enhancement`, `priority:high`

**Description:**

When CI fails, Minion should analyze logs and attempt fixes, with exponential backoff and max retries.

**Requirements:**

1. On CI failure, parse failure logs
2. Classify failure type (test failure, build error, timeout, flaky)
3. Generate fix based on error analysis
4. Apply fix and commit
5. Retry with exponential backoff (5s, 10s, 20s, 40s, 80s...)
6. Max retries configurable (default 10)
7. After max retries, escalate to human

**Acceptance Criteria:**

- [ ] Minion attempts to fix failed tests automatically
- [ ] Uses exponential backoff between attempts
- [ ] Stops after max_ci_retries (default 10)
- [ ] Adds minion:failed label after max retries
- [ ] Posts escalation comment tagging human

**Files to create:**
- `src/retry.rs` - Retry logic with backoff

**Files to modify:**
- `src/minion.rs` - Implement fix_ci_failure()
- `src/config.rs` - Add max_ci_retries field

**Reference:** See DESIGN.md sections "Handling CI Failures" and "Retry Strategy"

---

### Issue #7: Add PR ready-for-review conversion

**Labels:** `ready-for-minion`, `enhancement`, `priority:medium`

**Description:**

When Minion completes implementation and tests pass, convert draft PR to ready-for-review.

**Requirements:**

1. After all planned work complete and CI green
2. Call GitHub API to mark PR ready (convert from draft)
3. Update PR body with summary of changes
4. Post completion comment on issue
5. Change label to minion:done (or keep in-progress for review monitoring)

**Acceptance Criteria:**

- [ ] Draft PR automatically converted when work complete
- [ ] PR body updated with clear summary
- [ ] Issue comment posted: "Ready for review"
- [ ] Label updated appropriately

**Files to modify:**
- `src/github.rs` - Implement mark_pr_ready() (may need GraphQL)
- `src/minion.rs` - Call when implementation phase done

**Reference:** See DESIGN.md section "Review Phase"

**Note:** This may require GraphQL mutation `markPullRequestReadyForReview`. Octocrab might not support it directly - may need raw GraphQL query.

---

### Issue #8: Implement post-PR review monitoring

**Labels:** `ready-for-minion`, `enhancement`, `priority:medium`

**Description:**

After PR is ready for review, Minion should monitor for review comments and respond autonomously.

**Requirements:**

1. After marking PR ready, enter monitoring loop
2. Poll PR for new review comments every 30 seconds
3. Detect inline comments and review submissions
4. For simple changes (typo, rename, refactor), implement immediately
5. For complex/unclear requests, ask clarifying questions
6. Continue until PR merged or closed

**Acceptance Criteria:**

- [ ] Minion polls PR after marking ready
- [ ] Detects new review comments
- [ ] Responds to simple feedback automatically
- [ ] Asks questions when unclear
- [ ] Exits loop when PR merged/closed

**Files to modify:**
- `src/minion.rs` - Add review monitoring loop
- `src/github.rs` - Add get_review_comments(), get_pr_status()

**Reference:** See DESIGN.md section "Review Phase"

---

### Issue #9: Implement graceful cleanup and archiving

**Labels:** `ready-for-minion`, `enhancement`, `priority:medium`

**Description:**

When Minion completes (PR merged or failed), properly archive logs and clean up resources.

**Requirements:**

1. When PR merged, detected via GitHub API
2. Archive session logs, events.jsonl, metrics.json to ~/.gru/archive/<ID>/
3. Kill tmux session
4. Remove worktree with git worktree remove
5. Remove Minion from active set
6. Post final summary comment with metrics

**Acceptance Criteria:**

- [ ] On merge, Minion archives all logs
- [ ] Tmux session terminated
- [ ] Worktree removed cleanly
- [ ] Archive contains: events.jsonl, session.log, metrics.json
- [ ] Final comment includes: duration, tokens used, commits

**Files to modify:**
- `src/minion.rs` - Implement complete() and archive()
- `src/lab.rs` - Remove from active tracking

**Reference:** See DESIGN.md sections "Completion Phase" and "File System Layout"

---

## Phase 3: Polish & Advanced Features

### Issue #10: Add crash recovery on Lab restart

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

When Lab restarts, recover state from GitHub and resume monitoring active Minions.

**Requirements:**

1. On Lab start, enumerate tmux sessions with prefix `gru-minion-`
2. For each session, extract Minion ID
3. Fetch corresponding issue from GitHub (from in-progress label or search)
4. Reconstruct Minion state from GitHub timeline
5. Resume monitoring (check if PR exists, what stage)
6. Gracefully handle orphaned sessions (issue closed while offline)

**Acceptance Criteria:**

- [ ] Lab restart detects existing tmux sessions
- [ ] Reconstructs Minion state from GitHub
- [ ] Resumes monitoring without user intervention
- [ ] Handles edge cases (orphaned sessions, closed issues)

**Files to modify:**
- `src/lab.rs` - Add recovery on startup
- `src/minion.rs` - Add from_existing() constructor

**Reference:** See DESIGN.md section "Crash Recovery"

---

### Issue #11: Implement Prometheus metrics endpoint

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

Expose Prometheus metrics at http://localhost:9090/metrics for monitoring.

**Requirements:**

1. Add prometheus dependency
2. Track metrics:
   - gru_minions_active (gauge by state)
   - gru_issues_claimed_total (counter)
   - gru_prs_merged_total (counter)
   - gru_ci_runs_total (counter by result)
   - gru_tokens_used_total (counter)
   - gru_issue_duration_seconds (histogram)
3. Expose HTTP endpoint on configurable port (default 9090)
4. Add observability.metrics_enabled config option

**Acceptance Criteria:**

- [ ] Metrics endpoint responds at localhost:9090/metrics
- [ ] Metrics in Prometheus exposition format
- [ ] All core metrics tracked and updated
- [ ] Can disable with metrics_enabled: false

**Files to create:**
- `src/metrics.rs` - Metrics definitions and server

**Files to modify:**
- `Cargo.toml` - Add prometheus dependency
- `src/lab.rs` - Start metrics server
- `src/config.rs` - Add metrics config

**Reference:** See DESIGN.md section "Metrics (Prometheus)"

---

### Issue #12: Add branch name generation from issue metadata

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

Generate semantic branch names based on issue type and title.

**Requirements:**

1. Parse issue labels to determine type (bug → fix, enhancement → feat, etc.)
2. Extract slug from issue title (first 4 words, lowercased, hyphenated)
3. Format: `<type>/issue<number>-<slug>-<minion-id>`
4. Examples:
   - `feat/issue123-add-user-auth-M007`
   - `fix/issue456-memory-leak-M00a`
5. Sanitize special characters from slug

**Acceptance Criteria:**

- [ ] Branch names follow semantic convention
- [ ] Type prefix derived from labels correctly
- [ ] Slug generated from issue title
- [ ] Special characters handled safely
- [ ] Examples in tests validate all formats

**Files to modify:**
- `src/minion.rs` - Update generate_branch_name()

**Reference:** See DECISIONS.md section "Branch Naming"

---

### Issue #13: Implement merge conflict auto-resolution

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

When Minion's branch becomes stale or has conflicts, automatically rebase and resolve.

**Requirements:**

1. Detect stale branch (base branch moved ahead)
2. Attempt git rebase from worktree
3. If conflicts, analyze conflict markers
4. Use LLM to generate resolution
5. Run tests after resolution
6. Only push if tests pass
7. If tests fail, escalate to human

**Acceptance Criteria:**

- [ ] Detects when branch is behind base
- [ ] Attempts automatic rebase
- [ ] Resolves simple conflicts autonomously
- [ ] Runs tests before pushing resolution
- [ ] Escalates complex conflicts

**Files to create:**
- `src/conflicts.rs` - Conflict detection and resolution

**Files to modify:**
- `src/minion.rs` - Call conflict checker periodically
- `src/git.rs` - Add rebase operations

**Reference:** See DECISIONS.md section "Minion Behavior - Conflict Resolution"

---

### Issue #14: Add configuration validation on startup

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

Validate configuration file thoroughly on Lab startup with helpful error messages.

**Requirements:**

1. Check all required fields present
2. Validate repo format (owner/repo)
3. Validate numeric ranges (slots > 0, port 1-65535)
4. Validate paths exist and are writable
5. Check environment variables set
6. Validate GitHub token has required scopes
7. Pretty-print validation errors with suggestions

**Acceptance Criteria:**

- [ ] Invalid config produces clear error messages
- [ ] Errors include suggestions for fixing
- [ ] Validates GitHub token connectivity
- [ ] Checks file system permissions
- [ ] Lab refuses to start with invalid config

**Files to modify:**
- `src/config.rs` - Add validate() method
- `src/github.rs` - Add token scope checker

**Reference:** See DESIGN.md section "Configuration"

---

### Issue #15: Add `gru minions pause/resume` commands

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`

**Description:**

Allow manually pausing and resuming Minions without killing them.

**Requirements:**

1. `gru minions pause M001` - Sends pause signal to Minion
2. Minion saves state and stops working (but stays alive)
3. `gru minions resume M001` - Resumes from saved state
4. `gru minions list` shows paused state
5. Useful for debugging or blocking issues

**Acceptance Criteria:**

- [ ] Can pause active Minion
- [ ] Minion stops making changes but stays alive
- [ ] Can resume later and continue work
- [ ] State preserved across pause/resume
- [ ] List command shows paused status

**Files to modify:**
- `src/cli/minions.rs` - Add pause/resume commands
- `src/minion.rs` - Add pause/resume methods
- Possibly need signal handling to tmux session

**Reference:** See DESIGN.md section "Attach Commands"

---

## Phase 4: Advanced Features (V2+)

### Issue #16: Implement webhooks instead of polling

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`, `v2`

**Description:**

Replace polling with GitHub webhooks for instant notifications.

**Requirements:**

1. Start HTTP server to receive webhooks
2. Register webhooks with GitHub API
3. Handle events: issues.labeled, pull_request.review_requested, check_run.completed
4. Validate webhook signatures
5. Fall back to polling if webhooks fail
6. Make configurable (polling vs webhooks)

**Acceptance Criteria:**

- [ ] Receives GitHub webhook events
- [ ] Validates webhook signatures
- [ ] Reacts instantly to issue labels
- [ ] Falls back gracefully to polling
- [ ] Configurable via config.yaml

**Files to create:**
- `src/webhooks.rs` - Webhook server and handlers

**Files to modify:**
- `src/lab.rs` - Start webhook server alongside poller
- `src/config.rs` - Add webhook configuration

**Reference:** See DESIGN.md section "Future Roadmap - Event-Driven Architecture"

---

### Issue #17: Add codebase RAG with semantic search

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`, `v2`

**Description:**

Build local embedding index of codebase for semantic code search.

**Requirements:**

1. On repo clone, generate embeddings for all files
2. Store in vector database (could use simple SQLite with vector extension)
3. When Minion starts, search for relevant files semantically
4. Include top-K relevant files in initial context
5. Update embeddings on code changes

**Acceptance Criteria:**

- [ ] Embeddings generated for codebase
- [ ] Semantic search returns relevant files
- [ ] Minions get better initial context
- [ ] Embeddings update incrementally

**Files to create:**
- `src/embeddings.rs` - Embedding generation and search
- `src/vector_db.rs` - Simple vector storage

**Reference:** See DESIGN.md section "Future Roadmap - Caching & RAG"

---

### Issue #18: Implement cost tracking and budgets

**Labels:** `ready-for-minion`, `enhancement`, `priority:low`, `v2`

**Description:**

Track LLM token usage and costs, enforce per-issue budgets.

**Requirements:**

1. Track tokens used per Minion (from API responses)
2. Calculate cost based on model pricing
3. Add max_tokens_per_issue config option
4. Pause Minion if budget exceeded
5. Report costs in completion comments
6. Dashboard showing total spend

**Acceptance Criteria:**

- [ ] Tokens tracked accurately
- [ ] Cost calculated for each issue
- [ ] Budget enforcement works
- [ ] Completion comments include cost
- [ ] Can query total spend

**Files to create:**
- `src/cost.rs` - Cost tracking and budgets

**Files to modify:**
- `src/minion.rs` - Track token usage
- `src/config.rs` - Add budget options

**Reference:** See DESIGN.md section "Future Roadmap - Cost Optimization"

---

## Creating These Issues

To create these issues in your repository:

1. **Manual creation:** Copy-paste each issue above into GitHub's issue form

2. **Automated creation:** Use GitHub CLI:
   ```bash
   gh issue create \
     --title "Issue title" \
     --body "Issue description" \
     --label "ready-for-minion,enhancement"
   ```

3. **Batch script:** Create a script to parse this file and create all issues programmatically

## Prioritization Strategy

**Week 1:** Issues #1-4 (Multi-slot, PR creation, logging, events)
- Gets you to functional multi-Minion Lab with good observability

**Week 2:** Issues #5-9 (CI monitoring, retry, review, cleanup)
- Completes full autonomous lifecycle

**Week 3:** Issues #10-15 (Recovery, metrics, polish)
- Production-ready reliability and monitoring

**Week 4+:** Issues #16-18 (Advanced features)
- Nice-to-haves and optimizations

## Success Metrics

Track these to measure Gru's self-implementation progress:

- **Issues claimed by Minions:** Target 15+ out of 18
- **PRs merged autonomously:** Target 80%+ without human edits
- **Average time to completion:** Track and optimize
- **Token cost per issue:** Monitor and trend
- **Human interventions needed:** Should decrease over time

Once Gru has implemented these features on itself, it's ready for broader use!
