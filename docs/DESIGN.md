# Gru: Design Document

> **Version:** 1.0 (Single-Lab MVP)  
> **Last Updated:** 2025-11-30

## Table of Contents

1. [Introduction](#introduction)
2. [Architecture Overview](#architecture-overview)
3. [Core Components](#core-components)
4. [Session Management & Attach](#session-management--attach)
5. [Data Model](#data-model)
6. [GitHub Integration](#github-integration)
7. [Minion Lifecycle](#minion-lifecycle)
8. [State Management](#state-management)
9. [CI/CD Integration](#cicd-integration)
10. [Error Handling & Recovery](#error-handling--recovery)
11. [Security Model](#security-model)
12. [API Specification](#api-specification)
13. [File System Layout](#file-system-layout)
14. [Configuration](#configuration)
15. [Observability](#observability)
16. [Future Roadmap](#future-roadmap)

---

## Introduction

### What is Gru?

**Gru** is a local-first orchestrator that runs LLM-powered agents (called **Minions**) to autonomously work on GitHub issues. Each Gru instance (a **Lab**) continuously monitors GitHub repositories for issues labeled `ready-for-minion`, claims them, implements solutions, runs tests via GitHub Actions, opens pull requests, and responds to code review feedback.

### Design Philosophy

1. **Local-first**: Works offline except for GitHub API calls
2. **GitHub as database**: No separate state store; GitHub is source of truth
3. **Simple before clever**: Start with polling + labels, optimize later
4. **Autonomous agents**: Minions handle the full lifecycle from claim to merge
5. **Human-in-the-loop**: Clear escalation paths when Minions need help

### V1 Scope

This design describes the **single-Lab MVP**:
- ✅ One Lab instance (no multi-Lab coordination)
- ✅ Multi-repo support (Lab watches multiple repos)
- ✅ Simple 3-state label machine (`ready-for-minion` → `in-progress` → `done`/`failed`)
- ✅ Local testing via pre-commit hooks + GitHub Actions for verification
- ✅ In-memory state (no SQLite), file-based cursors
- ✅ CLI-only (no web UI/Tower)
- ✅ Polling (30s interval, no webhooks yet)
- ✅ Tokens via environment variables only

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                           Lab Process                         │
│                                                               │
│  ┌─────────────┐      ┌──────────────┐    ┌──────────────┐  │
│  │   Poller    │─────>│ Scheduler    │───>│ Minion Pool  │  │
│  │             │      │              │    │              │  │
│  │ - Fetch     │      │ - Claim      │    │ - M1 (run)   │  │
│  │   issues    │      │   issues     │    │ - M2 (run)   │  │
│  │ - Check     │      │ - Assign     │    │ - M3 (pause) │  │
│  │   PRs       │      │   slots      │    │              │  │
│  └─────────────┘      └──────────────┘    └──────────────┘  │
│         │                     │                    │         │
└─────────┼─────────────────────┼────────────────────┼─────────┘
          │                     │                    │
          └─────────────────────┴────────────────────┘
                                │
                         GitHub REST/GraphQL API
                                │
          ┌─────────────────────┴────────────────────┐
          │                                          │
    ┌─────▼──────┐                          ┌───────▼──────┐
    │   Issues   │                          │  Workflows   │
    │            │                          │              │
    │ - Labels   │◄────────────────────────►│ - Checks API │
    │ - Comments │                          │ - Status     │
    │ - Timeline │                          │              │
    └────────────┘                          └──────────────┘
          │
          │
    ┌─────▼──────┐
    │  Pull      │
    │  Requests  │
    │            │
    │ - Reviews  │
    │ - Comments │
    └────────────┘
```

### Key Architectural Decisions

| Decision | Rationale |
|----------|-----------|
| **Single binary** | Easy deployment, no dependencies |
| **GitHub as state store** | Eliminates DB complexity, provides audit trail |
| **Git worktrees** | Isolated workspaces per Minion |
| **Draft PR early** | Natural lock mechanism, visible progress |
| **GitHub Actions for CI** | Reuses existing infra, proper isolation |
| **YAML comments** | Human-readable structured data |
| **Polling (V1)** | Simple, reliable; webhooks deferred to V2 |

---

## Core Components

### Lab

The **Lab** is the main process that orchestrates Minions.

**Responsibilities:**
- Poll GitHub for `ready-for-minion` issues
- Manage Minion slots (max concurrent Minions)
- Monitor PRs for review feedback and CI failures
- Persist Minion state to disk
- Expose GraphQL API for introspection (future)

**Configuration:**
```yaml
# ~/.gru/config.yaml
github:
  token: ghp_xxxxxxxxxxxx
  repos:
    - owner/repo1
    - owner/repo2
    
lab:
  slots: 2                    # Max concurrent Minions
  poll_interval: 30s          # How often to check for new issues
  minion_timeout: 2h          # Max time before declaring stuck
  
llm:
  provider: anthropic
  model: claude-sonnet-4-5
  max_tokens_per_issue: 100000
```

### Minion

A **Minion** is a Claude Code session working on a single issue.

**Key Insight:** Each Minion **IS** a Claude Code session. One-to-one mapping.

```
Minion M42  =  Claude Code session in worktree ~/.gru/work/owner/repo/M42
Minion M43  =  Claude Code session in worktree ~/.gru/work/owner/repo/M43
```

**How Lab manages Minions:**
1. Lab spawns Claude Code process with initial prompt
2. Claude Code session runs autonomously in dedicated worktree
3. Lab monitors Claude Code stdout/stderr for events
4. Lab handles GitHub integration (labels, comments, PR operations)
5. When complete, Lab terminates session and archives logs

**Lifecycle States:**
- `InProgress` - Actively working (planning, implementing, testing, reviewing)
- `Failed` - Exceeded retry limit (10-15 attempts), paused for human help
- `Done` - PR merged or issue closed, session terminated and archived
- `Orphaned` - Issue closed while Minion was running, kept alive for inspection

**Note:** Detailed sub-states (planning, testing, review) tracked internally and in comment events, not as explicit enum values.

**Minion Structure:**
```rust
use chrono::{DateTime, Utc};

struct Minion {
    id: String,              // Base36: "M001", "M002", ..., "M0ZZ"
    lab_id: String,          // hostname of Lab
    repo: String,            // "owner/repo"
    issue_number: i32,       // 123
    branch: String,          // e.g., "feat/issue123-add-user-auth-M007"
    state: MinionState,      // InProgress, Failed, Done, Orphaned

    worktree_path: String,   // ~/.gru/work/owner/repo/M042
    pr_number: Option<i32>,  // None until PR created

    // tmux session (V1 uses tmux, not direct process management)
    tmux_session: String,    // "gru-minion-M042"

    started_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,

    metrics: MinionMetrics,
}

struct MinionMetrics {
    tokens_used: i32,
    commits_created: i32,
    ci_runs: i32,
    retry_count: i32,
    duration_seconds: i32,
}
```

**Initial Prompt Template:**
```markdown
You are Minion M042 working on issue #123 in owner/repo.

## Issue
{issue_title}

{issue_body}

## Your Mission
1. Understand the issue requirements
2. Explore the codebase to identify relevant files
3. Implement the requested changes
4. Commit after each logical unit of work (tests run automatically via pre-commit hook)
5. Push commits to trigger GitHub Actions verification
6. Monitor CI, stale branches, merge conflicts - keep PR up to date
7. Respond to review feedback autonomously
8. Mark ready for review when complete

## Working Environment
- Directory: {worktree_path}
- Branch: {branch_name} (branched from {default_branch})
- Commit prefix: [minion:{minion_id}]

## Guidelines
- Commit after each logical unit of work (tests run automatically via pre-commit hook)
- Use descriptive commit messages
- You are autonomous - implement review suggestions, decline with reasoning, or create follow-up issues
- Resolve merge conflicts yourself (run tests to verify, only push if tests pass)
- If blocked after 10+ retry attempts, pause and request human help
- Keep the PR updated - rebase when stale, resolve conflicts proactively

## Context
Everything else (README, CONTRIBUTING.md, git history) is in the repository. 
Explore as needed. Use subagents for test execution to avoid context bloat.

Start working now.
```

### Poller

The **Poller** monitors GitHub for work.

**Issue Polling:**
```graphql
query FindReadyIssues($repo: String!) {
  repository(owner: $owner, name: $name) {
    issues(
      first: 20
      states: OPEN
      labels: ["ready-for-minion"]
      orderBy: {field: CREATED_AT, direction: ASC}
    ) {
      nodes {
        number
        title
        body
        labels(first: 10) { nodes { name } }
        createdAt
      }
    }
  }
}
```

**PR Polling (for active Minions):**
- Check for new review comments
- Check for failed check runs
- Check for merge events

**Polling Strategy:**
```rust
use tokio::time::{interval, Duration};
use tokio::select;

async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticker = interval(self.poll_interval);

    loop {
        select! {
            _ = ticker.tick() => {
                // Poll for new issues
                if self.lab.has_available_slots() {
                    let issues = self.fetch_ready_issues().await;
                    for issue in issues {
                        self.scheduler.enqueue(issue).await;
                    }
                }

                // Poll active PRs for updates
                for minion in self.lab.active_minions() {
                    let updates = self.fetch_pr_updates(minion.pr_number).await;
                    minion.handle_updates(updates).await;
                }
            }

            _ = shutdown.changed() => {
                return;
            }
        }
    }
}
```

### Scheduler

The **Scheduler** assigns issues to available Minion slots.

**Prioritization (V1 - simple):**
1. Issues with `priority:high` label
2. Oldest issues first (FIFO)

**Slot Management:**
```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

struct Scheduler {
    max_slots: usize,
    active: Arc<RwLock<HashMap<String, Arc<Minion>>>>, // minion_id -> Minion
}

async fn try_claim_issue(&self, issue: Issue) -> Result<Arc<Minion>, SchedulerError> {
    let active = self.active.read().await;
    if active.len() >= self.max_slots {
        return Err(SchedulerError::NoSlotsAvailable);
    }
    drop(active);

    let minion = self.create_minion(issue);

    // Attempt to claim on GitHub
    self.claim_issue(&minion).await?;

    let minion_arc = Arc::new(minion);
    self.active.write().await.insert(minion_arc.id.clone(), minion_arc.clone());

    let minion_clone = minion_arc.clone();
    tokio::spawn(async move {
        minion_clone.run().await;
    });

    Ok(minion_arc)
}
```

---

## Session Management & Attach

### Claude Code Session Status

Each Minion (Claude Code session) has an internal status that the Lab tracks:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStatus {
    Thinking,          // Processing/reasoning
    UsingTool,         // Executing tool
    Responding,        // Generating response
    WaitingInput,      // Needs user input
    WaitingPermission, // Needs approval
    Idle,              // Ready for instruction
    Complete,          // Task finished
    Error,             // Encountered error
}
```

**Lab monitors status by:**
- Parsing Claude Code's JSON output stream
- Detecting tool invocation events
- Tracking output timestamps (idle detection)
- Watching for completion/error signals

**Status transition examples:**
```
idle → thinking → using_tool(git_commit) → thinking → responding → idle
idle → thinking → using_tool(bash) → waiting_permission → idle
idle → thinking → responding → complete
```

### Attach Architecture

Users can attach to running Minions to observe or interact:

```
┌─────────────┐
│  User TTY   │  gru attach M42
└──────┬──────┘
       │
       ▼
┌─────────────────────────────┐
│          Lab                │
│                             │
│  ┌───────────────────┐      │
│  │  AttachManager    │      │
│  │  - sessions       │      │
│  │  - multiplex I/O  │      │
│  └────────┬──────────┘      │
└───────────┼─────────────────┘
            │
            │ Bidirectional stream
            ▼
┌─────────────────────────────┐
│   Claude Code Session       │
│   (Minion M42)              │
│                             │
│   stdin  ← Lab + Attachers  │
│   stdout → Lab + Attachers  │
│   stderr → Lab + Attachers  │
└─────────────────────────────┘
```

### Attach Modes

```rust
use tokio::sync::mpsc;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachMode {
    ReadOnly,     // Observe only
    Interactive,  // Can send input
}

struct AttachSession {
    id: String,
    minion_id: String,
    user_id: String,
    mode: AttachMode,
    started_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,  // Max 30 minutes

    input: mpsc::Sender<Vec<u8>>,   // User → Minion
    output: mpsc::Receiver<Vec<u8>>, // Minion → User
}
```

### Attach Commands

```bash
# List active Minions
gru minions list

# Attach read-only (watch mode)
gru attach M42

# Attach read-only with live streaming
gru attach M42 --follow

# Attach interactive (can send input)
gru attach M42 --interactive

# Detach (Minion continues)
<Ctrl+D> or type 'detach'
```

### Attach Session Management

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::{timeout, Duration};
use tracing::{info, error};

struct AttachManager {
    sessions: Arc<RwLock<HashMap<String, AttachSession>>>,
}

async fn attach(&self, minion_id: String, mode: AttachMode) -> Result<AttachSession, AttachError> {
    let minion = self.lab.get_minion(&minion_id)
        .ok_or(AttachError::MinionNotFound)?;

    // Check interactive attach limit
    if mode == AttachMode::Interactive {
        if self.has_interactive_session(&minion_id).await {
            return Err(AttachError::InteractiveSessionExists);
        }
    }

    let (input_tx, input_rx) = mpsc::channel(100);
    let (output_tx, output_rx) = mpsc::channel(1000);

    let session = AttachSession {
        id: generate_session_id(),
        minion_id: minion_id.clone(),
        user_id: String::new(),
        mode: mode.clone(),
        started_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::minutes(30),
        input: input_tx,
        output: output_rx,
    };

    // Start multiplexing Minion output to this session
    let minion_clone = minion.clone();
    let output_tx_clone = output_tx.clone();
    tokio::spawn(async move {
        self.stream_output(minion_clone, output_tx_clone).await;
    });

    // If interactive, forward user input to Minion
    if mode == AttachMode::Interactive {
        let minion_clone = minion.clone();
        tokio::spawn(async move {
            self.stream_input(input_rx, minion_clone).await;
        });
    }

    self.sessions.write().await.insert(session.id.clone(), session.clone());
    info!(
        session_id = %session.id,
        minion_id = %minion_id,
        mode = ?mode,
        "user attached to minion"
    );

    Ok(session)
}

async fn stream_output(&self, minion: Arc<Minion>, output: mpsc::Sender<Vec<u8>>) {
    let mut reader = BufReader::new(minion.claude_session.stdout);

    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return, // Session ended
            Ok(_) => {}
        }

        // Send to attached session
        if timeout(Duration::from_secs(1), output.send(line)).await.is_err() {
            // Session not consuming, drop line
        }
    }
}

async fn stream_input(&self, mut input: mpsc::Receiver<Vec<u8>>, minion: Arc<Minion>) {
    while let Some(data) = input.recv().await {
        // Forward to Minion's stdin
        if let Err(e) = minion.claude_session.stdin.write_all(&data).await {
            error!(error = %e, "failed to send input to minion");
            return;
        }
    }
}
```

### Attach UI

```
╭──────────────────────────────────────────────────╮
│ 🤖 Attached to Minion M42                        │
│ Issue: #123 - Add user authentication            │
│ Status: using_tool(bash)                         │
│ Uptime: 15m 32s | Commits: 2 | Tokens: 12.5k    │
╰──────────────────────────────────────────────────╯

[M42 15:23:45] Running: npm test
[M42 15:23:45] 
[M42 15:23:47] > test
[M42 15:23:47] > jest
[M42 15:23:48] 
[M42 15:23:50]  PASS  src/auth.test.js
[M42 15:23:50]    ✓ should generate JWT token (45ms)
[M42 15:23:50]    ✓ should validate token (12ms)
[M42 15:23:50] 
[M42 15:23:50] Tests: 2 passed, 2 total
[M42 15:23:50] Status: thinking
[M42 15:23:52] All tests passed! Committing changes...
[M42 15:23:52] Status: using_tool(git_commit)

╭──────────────────────────────────────────────────╮
│ Mode: readonly | Press Ctrl+D to detach          │
╰──────────────────────────────────────────────────╯
```

### Security & Constraints

**Session limits:**
- Maximum 30-minute duration (auto-expire)
- Only 1 interactive session per Minion
- Unlimited read-only sessions per Minion
- Session activity logged for audit

**Permissions:**
- Lab always retains control
- User input mediated through Lab
- Can't force-terminate Minion from attach
- Can suggest actions but Lab decides

**Use cases:**
1. **Debugging** - Attach interactive to unstick blocked Minion
2. **Monitoring** - Watch Minion work in real-time
3. **Learning** - Observe agent reasoning and tool use
4. **Intervention** - Provide clarification when Minion asks questions

---

## Data Model

### Issue States (Labels)

**Simplified 3-state machine:**

```
┌─────────────────┐
│ ready-for-minion│  (user adds this when issue is ready)
└────────┬────────┘
         │
         │ Lab claims issue
         ▼
  ┌──────────────┐
  │ in-progress  │  (Minion actively working)
  └──────┬───────┘
         │
         ├───────────┐
         │           │
         │           │ Max retries exceeded (10-15 attempts)
         │           ▼
         │     ┌──────────────┐
         │     │ minion:failed│  (paused, needs human help)
         │     └──────────────┘
         │
         │ PR merged or issue closed
         ▼
  ┌──────────────┐
  │ minion:done  │  (archived, cleaned up)
  └──────────────┘
```

**Note:** Detailed states (planning, implementing, testing, review, blocked) are tracked in YAML comment events, not labels. Labels only reflect high-level lifecycle.

### Event Types (YAML Comments)

All structured Minion events are posted as GitHub comments with YAML frontmatter:

```yaml
---
event: <event_type>
minion_id: <string>
timestamp: <ISO8601>
# ... event-specific fields
---
```

**Event Types:**

| Event | Fields | Posted When |
|-------|--------|-------------|
| `minion:claim` | `lab_id`, `branch` | Issue claimed |
| `minion:plan` | `plan_summary`, `estimated_tokens` | Execution plan generated |
| `minion:progress` | `phase`, `commits`, `tests_passing` | Periodic updates |
| `minion:commit` | `sha`, `message`, `ci_run_id` | Code committed |
| `minion:blocked` | `reason`, `question` | Needs human input |
| `minion:failed` | `failure_reason`, `attempts`, `last_error` | Escalation |
| `minion:done` | `pr_number`, `commits`, `total_cost` | Completion |

**Example:**
```markdown
🤖 **Minion M42 claimed this issue**

---
event: minion:claim
minion_id: M42
lab_id: macbook-pro.local
branch: minion/issue-123-M42
timestamp: 2025-01-30T14:23:45Z
---

Starting work on this issue. I'll create a draft PR shortly.

📋 **Execution Plan**
1. Implement user authentication endpoints
2. Add JWT token generation
3. Write unit tests
4. Update API documentation
```

---

## GitHub Integration

### Authentication

**GitHub App (preferred for production):**
- Scoped permissions: `contents:write`, `issues:write`, `pull_requests:write`, `checks:read`
- Per-repository installation
- Audit trail via GitHub Apps

**Personal Access Token (V1 approach):**
- Classic token with `repo` and `workflow` scopes
- Stored in environment variable `GRU_GITHUB_TOKEN` (not in config file)

### API Usage Patterns

**Label Operations:**
```rust
// Add label
// PUT /repos/{owner}/{repo}/issues/{issue_number}/labels
// Body: ["claimed"]

// Replace all labels
// PUT /repos/{owner}/{repo}/issues/{issue_number}/labels
// Body: ["in-progress", "minion:M42"]

// Remove label
// DELETE /repos/{owner}/{repo}/issues/{issue_number}/labels/{label}
```

**Comment Posting:**
```rust
// POST /repos/{owner}/{repo}/issues/{issue_number}/comments
// Body: {
//     "body": "🤖 **Minion M42**\n\n---\nevent: minion:progress\n..."
// }
```

**Timeline Retrieval:**
```rust
// GET /repos/{owner}/{repo}/issues/{issue_number}/timeline
// Accept: application/vnd.github.mockingbird-preview+json

// Parse YAML frontmatter from comments
for event in timeline {
    if event.event_type == "commented" {
        let yaml_data = extract_frontmatter(&event.body);
        let minion_event = parse_yaml(&yaml_data)?;
    }
}
```

**Draft PR Creation:**
```rust
// POST /repos/{owner}/{repo}/pulls
// Body: {
//     "title": "[DRAFT] Fixes #123: Add user authentication",
//     "head": "minion/issue-123-M42",
//     "base": "main",
//     "body": "🤖 This PR is being worked on by Minion M42...",
//     "draft": true
// }
```

**Check Runs Monitoring:**
```rust
use tokio::time::{sleep, Duration};

// GET /repos/{owner}/{repo}/commits/{sha}/check-runs

// Subscribe to check run completion
loop {
    let check_runs = fetch_check_runs(&commit_sha).await?;
    if all_complete(&check_runs) {
        if all_passed(&check_runs) {
            minion.on_ci_pass().await;
        } else {
            minion.on_ci_fail(&check_runs).await;
        }
        break;
    }
    sleep(Duration::from_secs(10)).await;
}
```

### Rate Limiting

GitHub REST API rate limits:
- **Authenticated**: 5,000 requests/hour
- **GraphQL**: 5,000 points/hour (cost varies by query)

**Mitigation strategies:**
```rust
use chrono::{DateTime, Utc};
use tokio::time::sleep_until;
use std::collections::HashMap;

struct RateLimiter {
    remaining: i32,
    reset_at: DateTime<Utc>,
}

impl RateLimiter {
    async fn wait(&self) {
        if self.remaining < 100 {
            sleep_until(self.reset_at.into()).await;
        }
    }
}

// Use conditional requests where possible
let mut headers = HashMap::new();
headers.insert("If-None-Match", etag);
headers.insert("If-Modified-Since", last_modified);
// Returns 304 Not Modified (doesn't count against quota)
```

---

## Minion Lifecycle

### 1. Claim Phase

```rust
async fn claim(&mut self, issue: Issue) -> Result<(), MinionError> {
    // 1. Add 'claimed' label
    self.github.add_label(&issue, "claimed").await?;

    // 2. Post claim comment
    let comment = format_claim_comment(&self.id, &self.lab_id);
    self.github.post_comment(&issue, &comment).await?;

    // 3. Create branch
    self.branch = format!("minion/issue-{}-{}", issue.number, self.id);
    self.git.create_branch(&self.branch, "main").await?;

    // 4. Create draft PR (acts as lock)
    match self.github.create_draft_pr(&issue, &self.branch).await {
        Ok(pr) => {
            self.pr_number = Some(pr.number);
        }
        Err(e) if e.is_pr_exists() => {
            // Another Lab may have claimed this issue
            self.cleanup().await;
            return Err(MinionError::LostRace);
        }
        Err(e) => return Err(e.into()),
    }

    // 5. Update label: claimed -> in-progress
    self.github.replace_labels(&issue, &["in-progress"]).await?;

    Ok(())
}
```

### 2. Planning Phase

```rust
async fn generate_plan(&self) -> Result<Plan, MinionError> {
    // Read issue context
    let issue = self.github.get_issue(self.issue_number).await?;

    // Fetch relevant codebase context
    let files = self.identify_relevant_files(&issue).await;
    let code_context = self.read_files(&files).await;

    // Generate execution plan via LLM
    let prompt = format_planning_prompt(&issue, &code_context);
    let plan = self.llm.generate(&prompt).await?;

    // Post plan to issue
    let plan_comment = format_plan_comment(&self.id, &plan);
    self.github.post_comment(&issue, &plan_comment).await?;

    // Save plan locally
    self.save_plan(&plan).await?;

    Ok(plan)
}
```

### 3. Implementation Phase

```rust
async fn implement(&mut self, plan: Plan) -> Result<(), MinionError> {
    for step in &plan.steps {
        // Generate code changes
        let changes = self.generate_changes(step).await;

        // Apply changes to worktree
        for change in changes {
            self.apply_change(&change).await;
        }

        // Run local validation (optional)
        if let Err(_) = self.run_local_checks().await {
            // Fix and retry
            continue;
        }

        // Commit changes
        let commit_msg = format_commit_message(&self.id, step);
        let sha = self.git.commit(&commit_msg).await?;

        // Push to trigger CI
        self.git.push(&self.branch).await?;

        // Wait for CI
        if let Err(e) = self.wait_for_ci(&sha).await {
            // CI failed, attempt to fix
            if let Err(_) = self.fix_ci_failure(&sha).await {
                self.metrics.retry_count += 1;
                if self.metrics.retry_count > MAX_RETRIES {
                    return self.escalate("CI failures exceeded max retries").await;
                }
            }
        }

        // Post progress update
        self.post_progress_update(step).await;
    }

    Ok(())
}
```

### 4. Review Phase

```rust
use tokio::time::{sleep, Duration};

async fn handle_review(&mut self) -> Result<(), MinionError> {
    // Convert draft to ready for review
    self.github.mark_pr_ready(self.pr_number.unwrap()).await?;

    // Update labels
    self.github.replace_labels(&self.issue, &["review"]).await?;

    // Post summary comment
    let summary = self.generate_summary();
    self.github.post_comment(&self.issue, &summary).await?;

    // Monitor for review feedback
    loop {
        // Check for new review comments
        let comments = self.github.get_review_comments(
            self.pr_number.unwrap(),
            self.last_seen_comment_id
        ).await?;

        for comment in comments {
            if self.can_handle_autonomously(&comment) {
                // Implement requested changes
                self.implement_review_feedback(&comment).await?;
            } else {
                // Ask for clarification or escalate
                self.respond_to_reviewer(&comment).await?;
            }
        }

        // Check if merged
        let pr = self.github.get_pr(self.pr_number.unwrap()).await?;
        if pr.merged {
            return self.complete().await;
        }

        // Check if closed without merge
        if pr.state == "closed" && !pr.merged {
            return self.abandon().await;
        }

        sleep(Duration::from_secs(30)).await;
    }
}
```

### 5. Completion Phase

```rust
async fn complete(&mut self) -> Result<(), MinionError> {
    // Add done label
    self.github.add_label(&self.issue, "minion:done").await?;

    // Post completion comment with metrics
    self.post_completion_comment().await?;

    // Archive logs and events
    self.archive_logs().await?;

    // Cleanup worktree
    self.git.remove_worktree(&self.worktree_path).await?;

    // Remove from active Minions
    self.lab.remove_minion(&self.id).await?;

    Ok(())
}
```

---

## State Management

### Local State (SQLite)

```sql
-- ~/.gru/state/minions.db

CREATE TABLE minions (
    id TEXT PRIMARY KEY,
    lab_id TEXT NOT NULL,
    repo TEXT NOT NULL,
    issue_number INTEGER NOT NULL,
    branch TEXT NOT NULL,
    state TEXT NOT NULL,
    pr_number INTEGER,
    
    started_at TIMESTAMP NOT NULL,
    last_activity TIMESTAMP NOT NULL,
    
    tokens_used INTEGER DEFAULT 0,
    commits_created INTEGER DEFAULT 0,
    ci_runs INTEGER DEFAULT 0,
    retry_count INTEGER DEFAULT 0,
    
    UNIQUE(repo, issue_number)
);

CREATE TABLE timeline_cursors (
    repo TEXT NOT NULL,
    issue_number INTEGER NOT NULL,
    cursor TEXT NOT NULL,
    last_checked TIMESTAMP NOT NULL,
    
    PRIMARY KEY (repo, issue_number)
);
```

**Why SQLite?**
- Single file, no daemon process
- ACID transactions
- Efficient queries for active Minions
- Simple backup (copy file)

### GitHub State (Source of Truth)

GitHub stores the canonical state via:
- **Labels** - Current state visible in UI
- **Timeline** - Complete event history
- **PR** - Work artifacts and review discussions

**State Reconciliation:**
```rust
async fn reconcile_state(&self) -> Result<(), LabError> {
    // On startup, rebuild state from GitHub
    let local_minions = self.db.get_active_minions().await?;

    for mut minion in local_minions {
        // Fetch issue timeline
        let timeline = self.github.get_timeline(&minion.issue).await?;

        // Reconstruct state from events
        let actual_state = derive_state_from_timeline(&timeline);

        if actual_state != minion.state {
            // GitHub is source of truth
            minion.state = actual_state.clone();
            self.db.update_minion(&minion).await?;
        }

        // Resume or abandon based on state
        match actual_state.as_str() {
            "in-progress" => {
                let minion_clone = minion.clone();
                tokio::spawn(async move {
                    minion_clone.resume().await;
                });
            }
            "review" => {
                let minion_clone = minion.clone();
                tokio::spawn(async move {
                    minion_clone.monitor_review().await;
                });
            }
            "minion:done" | "minion:failed" => {
                minion.cleanup().await?;
            }
            _ => {}
        }
    }

    Ok(())
}
```

---

## CI/CD Integration

### GitHub Actions Workflow

Minions rely on repository's existing workflows:

```yaml
# .github/workflows/ci.yml
name: CI

on:
  push:
    branches: ['**']  # Run on all branches including minion branches
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Setup
        run: make setup
      - name: Lint
        run: make lint
      - name: Test
        run: make test
      - name: Build
        run: make build
```

### Monitoring Check Runs

```rust
use tokio::time::{timeout, sleep, Duration};

async fn wait_for_ci(&mut self, commit_sha: &str) -> Result<(), MinionError> {
    let timeout_duration = Duration::from_secs(30 * 60);
    let mut interval = tokio::time::interval(Duration::from_secs(10));

    timeout(timeout_duration, async {
        loop {
            interval.tick().await;

            let check_runs = self.github.get_check_runs(commit_sha).await?;

            if !all_complete(&check_runs) {
                continue;
            }

            if all_passed(&check_runs) {
                self.metrics.ci_runs += 1;
                return Ok(());
            }

            // CI failed - fetch logs
            let logs = self.github.get_check_run_logs(&check_runs).await?;
            return Err(MinionError::CIFailure {
                check_runs,
                logs,
            });
        }
    })
    .await
    .map_err(|_| MinionError::CITimeout)?
}
```

### Handling CI Failures

```rust
async fn fix_ci_failure(&mut self, err: &CIFailureError) -> Result<(), MinionError> {
    // Analyze failure logs
    let analysis = self.analyze_failure(&err.logs).await;

    // Classify failure type
    match analysis.failure_type {
        FailureType::FlakyTest => {
            // Retry without changes
            self.git.commit("Retry flaky tests", &["--allow-empty"]).await?;
            self.git.push(&self.branch).await?;
            Ok(())
        }

        FailureType::TestFailure => {
            // Generate fix
            let fix = self.llm.generate_fix(&analysis).await?;
            self.apply_fix(&fix).await;
            self.git.commit(&format!("[minion:{}] Fix test failures", self.id), &[]).await?;
            self.git.push(&self.branch).await?;
            Ok(())
        }

        FailureType::BuildError => {
            // Attempt to fix build
            let fix = self.llm.generate_build_fix(&analysis).await?;
            self.apply_fix(&fix).await;
            self.git.commit(&format!("[minion:{}] Fix build errors", self.id), &[]).await?;
            self.git.push(&self.branch).await?;
            Ok(())
        }

        FailureType::Timeout => {
            // Escalate - likely infrastructure issue
            self.escalate("CI timeout - may need human intervention").await
        }

        _ => {
            self.escalate(&format!("Unknown CI failure: {}", analysis.summary)).await
        }
    }
}
```

---

## Error Handling & Recovery

### Retry Strategy

```rust
use tokio::time::{sleep, Duration};
use rand::Rng;

struct RetryConfig {
    max_attempts: usize,        // Default: 5
    initial_backoff: Duration,  // Default: 5s
    max_backoff: Duration,      // Default: 5m
    backoff_factor: f64,        // Default: 2.0
}

async fn retry_with_backoff<F, Fut, T, E>(
    &self,
    operation: F,
    config: RetryConfig,
) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::error::Error,
{
    let mut backoff = config.initial_backoff;
    let mut rng = rand::thread_rng();

    for attempt in 0..config.max_attempts {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                // Check if error is retryable
                if !is_retryable(&err) {
                    return Err(err);
                }

                if attempt == config.max_attempts - 1 {
                    return Err(err);
                }

                // Exponential backoff with jitter
                let jitter_ms = rng.gen_range(0..=(backoff.as_millis() / 10)) as u64;
                let jitter = Duration::from_millis(jitter_ms);
                sleep(backoff + jitter).await;

                backoff = Duration::from_secs_f64(backoff.as_secs_f64() * config.backoff_factor);
                if backoff > config.max_backoff {
                    backoff = config.max_backoff;
                }
            }
        }
    }

    Err(MinionError::MaxRetriesExceeded)
}
```

### Escalation to Humans

```rust
use chrono::Utc;

async fn escalate(&mut self, reason: &str) -> Result<(), MinionError> {
    // Update state
    self.state = MinionState::Blocked;
    self.db.update_minion(self).await?;

    // Add label
    self.github.add_label(&self.issue, "minion:blocked").await?;

    // Post escalation comment
    let comment = format!(
        r#"❌ **Minion {} needs help**

---
event: minion:blocked
minion_id: {}
reason: {}
attempts: {}
timestamp: {}
---

I've encountered an issue I can't resolve on my own. Could a human take a look?

**What I tried:**
{}

**Logs:** See [workflow run]({})

cc @repo-maintainer
"#,
        self.id,
        self.id,
        reason,
        self.metrics.retry_count,
        Utc::now().to_rfc3339(),
        self.format_attempt_history(),
        self.get_workflow_run_url()
    );

    self.github.post_comment(&self.issue, &comment).await?;

    // Pause Minion (don't cleanup - human may resume)
    self.pause().await?;

    Ok(())
}
```

### Crash Recovery

```rust
use chrono::Utc;

async fn recover(&self) -> Result<(), LabError> {
    // On startup, check for orphaned Minions
    let active_minions = self.db.get_active_minions().await?;

    for mut minion in active_minions {
        let last_activity = minion.last_activity;

        // If no activity for > 1 hour, likely crashed
        if Utc::now().signed_duration_since(last_activity).num_hours() > 1 {
            // Check GitHub state
            let issue = self.github.get_issue(minion.issue_number).await?;
            let labels = &issue.labels;

            if has_label(labels, "in-progress") {
                // Still marked as in-progress on GitHub
                // Try to resume or fail gracefully
                if minion.can_resume() {
                    let minion_clone = minion.clone();
                    tokio::spawn(async move {
                        minion_clone.resume().await;
                    });
                } else {
                    minion.escalate("Lab crashed, unable to resume").await?;
                }
            }
        }
    }

    Ok(())
}
```

---

## Security Model

### Threat Model

**Trusted:**
- Lab operator (has filesystem access)
- GitHub (source of truth)
- LLM provider (Anthropic, OpenAI)

**Untrusted:**
- Issue authors (may request malicious code)
- PR reviewers (compromise unlikely but possible)
- Dependencies installed during CI (supply chain risk)

### Mitigations

**1. GitHub Token Security**
```rust
// ~/.gru/config.yaml (mode 0600)
// github:
//   token: ghp_xxxxxxxxxxxx

// Never log tokens
impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token=***REDACTED***")
    }
}
```

**2. Secret Detection**
```rust
use regex::Regex;

// Pre-commit hook
async fn pre_commit_check(&self, files: &[String]) -> Result<(), MinionError> {
    for file in files {
        if contains_secrets(file)? {
            return Err(MinionError::SecretsDetected(file.clone()));
        }
    }
    Ok(())
}

lazy_static::lazy_static! {
    static ref SECRET_PATTERNS: Vec<Regex> = vec![
        Regex::new(r"(?i)api[_-]?key\s*=\s*['"][a-zA-Z0-9]{20,}['"]").unwrap(),
        Regex::new(r"(?i)password\s*=\s*['"][^'"]+['"]").unwrap(),
        Regex::new(r"ghp_[a-zA-Z0-9]{36}").unwrap(),
        Regex::new(r"-----BEGIN PRIVATE KEY-----").unwrap(),
    ];
}
```

**3. Worktree Isolation**
```bash
# Each Minion gets isolated worktree
~/.gru/work/owner/repo/M42/  # No access to other Minions
```

**4. CI Sandboxing**
- Tests run in GitHub Actions (containerized)
- No local code execution beyond git operations
- Minion has no access to repo secrets (only CI does)

**5. Rate Limiting**
```rust
// Prevent resource exhaustion
struct ResourceLimits {
    max_tokens_per_issue: i32,   // 100k tokens
    max_commits_per_issue: i32,  // 50 commits
    max_ci_runs_per_issue: i32,  // 20 runs
    max_retries_per_issue: i32,  // 5 attempts
}
```

---

## API Specification

### GraphQL Schema (Future - V2)

```graphql
schema {
  query: Query
  mutation: Mutation
  subscription: Subscription
}

type Query {
  lab: Lab!
  minion(id: ID!): Minion
  minions(state: [MinionState!], repo: String): [Minion!]!
  issue(repo: String!, number: Int!): Issue
}

type Mutation {
  claimIssue(repo: String!, number: Int!): Minion
  pauseMinion(id: ID!): Boolean!
  resumeMinion(id: ID!): Boolean!
  abandonMinion(id: ID!): Boolean!
}

type Subscription {
  minionEvents(id: ID!): MinionEvent!
  labEvents: LabEvent!
}

type Lab {
  id: ID!
  hostname: String!
  version: String!
  slots: Int!
  activeMinions: Int!
  startedAt: DateTime!
}

type Minion {
  id: ID!
  labId: ID!
  repo: String!
  issueNumber: Int!
  branch: String!
  state: MinionState!
  prNumber: Int
  
  startedAt: DateTime!
  lastActivity: DateTime!
  
  metrics: MinionMetrics!
  events: [MinionEvent!]!
}

type MinionMetrics {
  tokensUsed: Int!
  commitsCreated: Int!
  ciRuns: Int!
  retryCount: Int!
  durationSeconds: Int!
}

enum MinionState {
  CLAIMED
  PLANNING
  IMPLEMENTING
  TESTING
  BLOCKED
  REVIEW
  DONE
  FAILED
}

type MinionEvent {
  type: String!
  timestamp: DateTime!
  data: JSON!
}
```

### REST Endpoints (V1)

```
GET  /health                  # Health check
GET  /metrics                 # Prometheus metrics
GET  /api/lab                 # Lab info
GET  /api/minions             # List active Minions
GET  /api/minions/:id         # Minion details
POST /api/minions/:id/pause   # Pause Minion
POST /api/minions/:id/resume  # Resume Minion
POST /api/minions/:id/abandon # Abandon Minion
```

---

## File System Layout

```
~/.gru/
├── config.yaml                      # Lab configuration
├── repos/                           # Bare repository mirrors
│   └── owner/
│       └── repo.git/                # Bare clone
├── work/                            # Active worktrees
│   └── owner/
│       └── repo/
│           ├── M42/                 # Minion M42's worktree
│           │   ├── .git             # Worktree metadata
│           │   └── <repo files>
│           └── M43/                 # Minion M43's worktree
├── archive/                         # Completed Minion artifacts
│   ├── M42/
│   │   ├── events.jsonl             # Structured event log
│   │   ├── plan.md                  # Execution plan
│   │   ├── commits.log              # Git commit history
│   │   ├── ci-results.json          # CI check run results
│   │   └── metrics.json             # Cost and performance metrics
│   └── M43/
├── state/
│   ├── next_id.txt                  # Monotonic counter for Minion IDs (base36)
│   └── cursors.json                 # GitHub timeline cursors per issue
└── logs/
    └── gru.log                      # Lab process logs

# Note: No SQLite database - active Minions tracked in-memory only
# Recovery on restart: enumerate tmux sessions, fetch state from GitHub
```

---

## Configuration

### Environment Variables (Required)

**Tokens via environment variables only (no plaintext in config):**

```bash
# Required
export GRU_GITHUB_TOKEN="ghp_xxxxxxxxxxxx"
export ANTHROPIC_API_KEY="sk-ant-xxxxxxxxxxxx"

# Lab will fail on startup if these are missing
```

### config.yaml

**Non-sensitive settings only:**

```yaml
# ~/.gru/config.yaml

# Repositories to monitor (multi-repo support)
repos:
  - owner/repo1
  - owner/repo2
  - anotherowner/repo3

lab:
  # Concurrency - slots shared across all repos
  slots: 2
  
  # Polling interval (seconds)
  poll_interval: 30
  
  # Retry limits
  max_ci_retries: 10  # High limit before pausing for human review
  
  # Archive retention (0 = keep forever)
  archive_retention_days: 0

git:
  # User identity for commits
  user_name: "Gru Minion"
  user_email: "minion@gru.local"
  
  # Branch naming
  branch_prefix: "minion/issue-"

observability:
  # Logging
  log_level: info
  log_file: ~/.gru/logs/gru.log
  
  # Metrics (Prometheus format)
  metrics_enabled: true
  metrics_port: 9090
```

---

## Observability

### Structured Logging

```rust
use tracing::{info, error};

info!(
    minion_id = %self.id,
    issue = self.issue_number,
    repo = %self.repo,
    "minion claimed issue"
);

error!(
    minion_id = %self.id,
    commit_sha = %sha,
    attempts = self.metrics.retry_count,
    error = %err,
    "CI failed"
);
```

### Metrics (Prometheus)

```
# HELP gru_minions_active Number of active Minions
# TYPE gru_minions_active gauge
gru_minions_active{state="implementing"} 2
gru_minions_active{state="review"} 1

# HELP gru_issues_claimed_total Total issues claimed
# TYPE gru_issues_claimed_total counter
gru_issues_claimed_total 42

# HELP gru_prs_merged_total Total PRs merged
# TYPE gru_prs_merged_total counter
gru_prs_merged_total 35

# HELP gru_ci_runs_total Total CI runs triggered
# TYPE gru_ci_runs_total counter
gru_ci_runs_total{result="pass"} 120
gru_ci_runs_total{result="fail"} 15

# HELP gru_tokens_used_total Total LLM tokens consumed
# TYPE gru_tokens_used_total counter
gru_tokens_used_total 1523421

# HELP gru_issue_duration_seconds Time from claim to completion
# TYPE gru_issue_duration_seconds histogram
gru_issue_duration_seconds_bucket{le="300"} 5
gru_issue_duration_seconds_bucket{le="1800"} 15
gru_issue_duration_seconds_bucket{le="3600"} 30
```

### Event Log (events.jsonl)

```jsonl
{"event":"claimed","minion_id":"M42","issue":123,"timestamp":"2025-01-30T12:34:56Z"}
{"event":"plan_generated","minion_id":"M42","tokens":450,"steps":4}
{"event":"commit","minion_id":"M42","sha":"abc123","message":"Add auth"}
{"event":"ci_triggered","minion_id":"M42","workflow":"test","run_id":123456}
{"event":"ci_passed","minion_id":"M42","duration_ms":45000}
{"event":"pr_ready","minion_id":"M42","pr_number":789}
{"event":"review_comment","minion_id":"M42","comment_id":999}
{"event":"merged","minion_id":"M42","pr_number":789}
{"event":"completed","minion_id":"M42","total_tokens":15234,"commits":5,"duration_s":1800}
```

---

## Future Roadmap

### V2: Multi-Lab Coordination

- **Distributed locking** via GitHub Projects v2
- **Heartbeat protocol** for liveness detection
- **Stale issue reclamation** (Labs can pick up abandoned work)

### V3: Tower & Web UI

- **Central dashboard** for all Labs
- **Real-time attach sessions** (PTY streaming)
- **Live event subscriptions** via WebSocket
- **OAuth authentication** for users

### V4: Advanced Features

- **Issue dependency DAG** (wait for blockers)
- **Codebase RAG** (semantic search via embeddings)
- **Learned prioritization** (predict issue complexity)
- **Cost optimization** (model selection, prompt caching)
- **Webhook support** (replace polling)
- **Slack/email notifications**
- **Multi-repo orchestration**
- **Review feedback learning** (improve from past PRs)

---

## References

### External Documentation

- [GitHub REST API](https://docs.github.com/en/rest)
- [GitHub GraphQL API](https://docs.github.com/en/graphql)
- [GitHub Timeline Events](https://docs.github.com/en/rest/issues/timeline)
- [GitHub Checks API](https://docs.github.com/en/rest/checks)
- [GitHub Actions](https://docs.github.com/en/actions)
- [Git Worktrees](https://git-scm.com/docs/git-worktree)

### Related Projects

- [Sweep](https://github.com/sweepai/sweep) - AI junior developer
- [Devin](https://www.cognition-labs.com/devin) - AI software engineer
- [AutoGPT](https://github.com/Significant-Gravitas/AutoGPT) - Autonomous agents

---

**Last Updated:** 2025-01-30  
**Version:** 1.0 (Single-Lab MVP)
