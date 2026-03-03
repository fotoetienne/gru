# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Gru is a local-first LLM agent orchestrator that autonomously works on GitHub issues using Claude Code. It manages "Minions" (agent sessions) that claim issues, implement fixes, create PRs, and respond to reviews.

**Current Phase:** V1 - Basic delegation to Claude Code CLI
**Language:** Rust
**Architecture:** Single binary, GitHub as state store, git worktrees for isolation

## Build & Development Commands

```bash
# Build
just build

# Build with release optimizations
just build-release

# Run tests
just test

# Run tests with output
just test-verbose

# Run linter (with warnings as errors)
just lint

# Automatically fix clippy lints where possible
just fix-clippy

# Format code
just fmt

# Check formatting without modifying
just fmt-check

# Run all checks (format + lint + test + build)
just check

# Install locally
just install

# Clean build artifacts
just clean

# Show project information
just info
```

## Pre-commit Hooks

The project uses git hooks from `.githooks/` that automatically run:
- Code formatting check (`just fmt-check`)
- Linting (`just lint`)
- Tests (`just test`)
- Branch protection (blocks commits to main)
- TODO/FIXME warnings (non-blocking)

**To enable hooks:**
```bash
git config core.hooksPath .githooks
```

**When pre-commit hooks fail:**
1. Fix the issues using the suggested commands (e.g., `just fmt` or `just fix-clippy`)
2. **Review the diff** with `git diff` to see what changed
3. **Stage the fixed files selectively** with `git add <files>`
4. Never use `git add .` or `git add -A` - always stage files explicitly
5. Example workflow:
   ```bash
   git commit -m "Fix bug"
   # Pre-commit hook fails with formatting errors
   just fmt
   git diff              # Review what changed
   git add src/main.rs   # Stage only the files you want to commit
   git commit -m "Fix bug"
   ```

**To bypass in emergencies:**
```bash
git commit --no-verify
```

## Code Architecture

### Module Structure

- `src/main.rs` - CLI entry point using Clap, defines commands: init, do, review, rebase, path, attach, resume, clean, status, stop, prompt, prompts, lab
- `src/commands/` - Command handlers
  - `fix.rs` - Handles `gru do`: creates worktrees, spawns Claude CLI with stream-json output, monitors progress
  - `review.rs` - Delegates PR review to Claude CLI
  - `prompt.rs` - Ad-hoc prompt execution (literal or from prompt files)
  - `prompts.rs` - Lists available prompt files
  - `status.rs` - Lists active Minions from registry
  - `clean.rs` - Removes merged/closed worktrees
  - `path.rs` - Resolves Minion worktree paths
  - `attach.rs` - Attach terminal to a running Minion
  - `resume.rs` - Resume a stopped Minion
  - `stop.rs` - Stop a running Minion
  - `rebase.rs` - Rebase a Minion branch onto base
  - `lab.rs` - Daemon mode (polls for issues, spawns Minions)
  - `init.rs` - Initialize workspace for a repo
- `src/minion.rs` - Minion ID generation (monotonic counter, base36 format: M000, M001, etc.)
- `src/minion_registry.rs` - Persistent Minion tracking (`~/.gru/state/minions.json` with file locking)
- `src/minion_resolver.rs` - Resolve Minion by ID, issue number, or PR number
- `src/workspace.rs` - Manages `~/.gru/` directory structure (repos, work, archive, state)
- `src/git.rs` - Git operations (bare repos, worktrees, branch management)
- `src/claude_runner.rs` - Claude CLI subprocess spawning, stream monitoring, timeout/stuck detection
- `src/stream.rs` - Claude Code JSON stream parser (events: message_start, content_block_delta, etc.)
- `src/progress.rs` - Terminal progress display (spinner, tool status)
- `src/ci.rs` - CI monitoring, failure analysis, and auto-fix via Claude
- `src/pr_monitor.rs` - PR polling for reviews, CI status, and merge state
- `src/pr_state.rs` - PR state persistence (`.gru_pr_state.json`)
- `src/worktree_scanner.rs` - Discovers and checks status of worktrees
- `src/github.rs` - GitHub API client (octocrab + gh/ghe CLI wrappers)
- `src/url_utils.rs` - Issue/PR URL parsing
- `src/config.rs` - TOML configuration loader (`~/.gru/config.toml`)
- `src/prompt_loader.rs` - Built-in and custom prompt file loading
- `src/prompt_renderer.rs` - `{{ variable }}` template rendering for prompts
- `src/text_buffer.rs` - Streaming text buffer with flush intervals
- `src/reserved_commands.rs` - Reserved command name validation
- `src/progress_comments.rs` - GitHub PR progress comment formatting

### Key Architectural Decisions

**Minion Execution:** Each Minion spawns a Claude Code CLI process with stream JSON output:
```bash
claude --print \
  --verbose \
  --session-id <UUID> \
  --output-format stream-json \
  --dangerously-skip-permissions \
  --include-partial-messages \
  "<task prompt>"
```

**Key Flags Explained:**
- `--print`: Non-interactive mode (no TTY needed)
- `--verbose`: Include tool calls in output
- `--session-id`: Maintains conversation context across restarts
- `--output-format stream-json`: Real-time event stream following Anthropic Messages API streaming format
- `--dangerously-skip-permissions`: Autonomous operation (no approval prompts)
- `--include-partial-messages`: Get streaming updates for better progress tracking

**Stream Parsing:**
- Parse JSON events from Claude's stdout following Anthropic Messages API streaming format
- Event types: `message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, `error`, `ping`
- Real-time monitoring enables stuck detection and progress tracking
- Events logged to `events.jsonl` for debugging

**State Management:**
- **GitHub as source of truth** - Labels, comments, PRs provide complete state
- **Local state:** Persistent Minion registry at `~/.gru/state/minions.json` (atomic writes with file locking)
- **No SQLite** - Rebuild state from GitHub on restart
- **Minion IDs:** Stored in `~/.gru/state/next_id.txt` with file locking for atomicity

**Git Worktrees:**
- Each Minion works in isolated worktree under `~/.gru/work/`
- Bare repo mirrors: `~/.gru/repos/owner/repo.git`
- Branch naming: `minion/issue-<number>-<minion-id>` (e.g., `minion/issue-42-M001`)
- Worktree paths use branch names: `~/.gru/work/owner/repo/minion/issue-42-M001/`
- The actual git checkout lives at `minion_dir/checkout/` (legacy worktrees without `checkout/` subdir are auto-detected)
- Multiple minions can work sequentially on same issue (git manages locking)
- Worktrees are created from bare repos to avoid conflicts
- Stale worktrees are automatically force-removed if they exist

**Timeout & Stuck Detection** (constants in `claude_runner.rs`):
- Stream timeout: 5 minutes per line (`STREAM_TIMEOUT_SECS = 300`)
- Inactivity warning: 5 minutes (`INACTIVITY_WARNING_SECS = 300`)
- Stuck threshold: 15 minutes (`INACTIVITY_STUCK_SECS = 900`)
- Optional timeout flag: `gru do 42 --timeout 10m` (exits if task exceeds duration)

### Design Philosophy

1. **Local-first** - Works offline except for GitHub API calls
2. **GitHub as database** - No separate state store
3. **Simple before clever** - Polling before webhooks, labels before state machines
4. **Autonomous agents** - Full lifecycle from claim to merge
5. **Single binary** - Easy deployment, no daemon dependencies

## Available Slash Commands

Located in `.claude/commands/`:

- `/do <issue# or URL>` - Work on an issue (run from worktree)
- `/setup-worktree <issue# or URL>` - Create git worktree for working on an issue
- `/decompose <issue# or URL>` - Break large issue into smaller sub-issues
- `/issue [description]` - Create a GitHub issue from description or context
- `/respond` - Respond to all comments/reviews on a PR
- `/pr_review <pr# or URL>` - Review a GitHub pull request
- `/rebase` - Rebase current branch onto default branch with intelligent conflict resolution
- `/next_issue` - Suggest next issue to work on based on priorities and dependencies

## Available Skills

Located in `.claude/skills/`:

- `agent-skills` - Helps create new Claude skills (templates, guidance, validation)
- `git-worktrees` - Git worktree management for Minion workspaces
- `git-rebase` - Intelligent git rebase with conflict resolution
- `project-manager` - Understands issue dependencies, critical path, helps prioritize work
- `product-manager` - Shapes features, writes PRDs/user stories, evaluates designs

## Testing Strategy

- **Unit tests:** Test individual functions and modules (e.g., minion ID generation, branch parsing)
- **Integration tests:** Test GitHub API interactions (may use mocks)
- **Pre-commit tests:** All tests must pass before commits are allowed
- Use `just test-verbose` for verbose test output with `--nocapture`

## Important Implementation Notes

### Claude Code Integration
- Stream JSON output is the **primary** interface for monitoring Minions
- Session IDs maintain conversation context across restarts
- `--dangerously-skip-permissions` enables autonomous operation
- See `experiments/DMX_ANALYSIS.md` for approach comparison (CLI + Stream Parsing scored 0.735)

### GitHub Integration
- Never stage files with `git add -A` - always be explicit
- Authentication priority: `gh` CLI token (`gh auth token`), then `GRU_GITHUB_TOKEN` env var as fallback
- Labels drive state machine: `ready-for-minion` → `in-progress` → `minion:done`/`minion:failed`
- Comments use YAML frontmatter for structured events
- Issue/PR parsing supports both numbers (when in repo) and full GitHub URLs

### Worktree Management
- Worktree paths use branch names as directory paths (e.g., `minion/issue-42-M001/`)
- Create from bare repos to avoid conflicts
- Stale worktrees are automatically force-removed if they exist
- Multiple minions can work on same issue sequentially (git handles locking)
- Remove worktrees from outside (not from within themselves)
- Use `gh pr merge --auto` to avoid checkout conflicts on merge
- Post-merge cleanup: remove worktree + delete branch from bare repo

### Error Handling
- Retry with exponential backoff for transient failures
- CI auto-fix: max 3 attempts (`MAX_FIX_ATTEMPTS` in `ci.rs`), then escalate
- PR monitor API retries: max 5 attempts (`DEFAULT_MAX_RETRIES` in `pr_monitor.rs`)
- Escalate via comments with `minion:blocked` label
- CI failures analyzed and auto-fixed when possible
- Proper context in error messages using `anyhow::Context`

### Async/Await Patterns
- Use Tokio for async runtime (`#[tokio::main]`)
- Async file operations with `tokio::fs`
- Async process spawning with `tokio::process::Command`
- Async stream parsing with `AsyncBufReadExt`
- Timeout support with `tokio::time::timeout`

### Code Quality
- All code must pass `cargo clippy --all-targets -- -D warnings`
- Format with `cargo fmt --all`
- Avoid `#[allow(dead_code)]` for public APIs - only use during development
- Use `anyhow::Result` for error handling
- Use `Context` trait to add context to errors

## Filesystem Layout

```
~/.gru/
├── repos/              # Bare git repositories
│   └── owner/
│       └── repo.git/
├── work/               # Active Minion workspaces
│   └── owner/
│       └── repo/
│           └── minion/
│               └── issue-42-M001/      # Minion directory (metadata)
│                   ├── events.jsonl     # Stream events log
│                   ├── .gru_pr_state.json  # PR state
│                   ├── PR_DESCRIPTION.md   # PR description (when ready)
│                   └── checkout/        # Git worktree (repo files)
│                       ├── .git
│                       ├── src/
│                       └── Cargo.toml
├── state/              # Local state
│   ├── next_id.txt     # Minion ID counter
│   ├── minions.json    # Persistent Minion registry
│   └── minions.json.lock  # Registry file lock
└── archive/            # Completed work (future)
```

**Path concepts:**
- **`minion_dir`** — top-level minion directory for metadata. What `workspace.work_dir()` returns.
- **`checkout_path`** — `minion_dir/checkout/`. The actual git worktree where Claude runs.
- Legacy worktrees (no `checkout/` subdir) are auto-detected at runtime.

## Future Phases

- **V2:** Multi-Lab coordination, distributed locking via GitHub Projects, ACP integration
- **V3:** Tower web UI, WebSocket live updates, OAuth auth
- **V4:** Issue dependency DAG, RAG embeddings, learned prioritization
- **V5:** Multi-repo orchestration, cost accounting, notifications

See `docs/DESIGN.md` for full architecture and `docs/DECISIONS.md` for quantitative decision analysis.
