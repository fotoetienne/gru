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

- `src/main.rs` - CLI entry point using Clap, defines commands: fix, review, path, clean, status
- `src/commands/` - Command handlers
  - `fix.rs` - Creates worktrees, spawns Claude CLI with stream-json output, monitors progress
  - `review.rs` - Delegates PR review to Claude CLI
  - `status.rs` - Lists active Minions by scanning `~/.gru/work/`
  - `clean.rs` - Removes merged/closed worktrees
  - `path.rs` - Resolves Minion worktree paths
- `src/minion.rs` - Minion ID generation (monotonic counter, base36 format: M000, M001, etc.)
- `src/workspace.rs` - Manages `~/.gru/` directory structure (repos, work, archive)
- `src/git.rs` - Git operations (bare repos, worktrees, branch management)
- `src/stream.rs` - Claude Code JSON stream parser (events: message_start, content_block_delta, etc.)
- `src/progress.rs` - Progress display and stuck detection
- `src/worktree_scanner.rs` - Discovers and checks status of worktrees
- `src/github.rs` - GitHub API client using octocrab
- `src/logger.rs` - Logging utilities
- `src/url_utils.rs` - Issue/PR URL parsing

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
- **Local state:** In-memory only for V1, file-based cursors for timeline polling
- **No SQLite** - Rebuild state from GitHub on restart
- **Minion IDs:** Stored in `~/.gru/state/next_id.txt` with file locking for atomicity

**Git Worktrees:**
- Each Minion works in isolated worktree: `~/.gru/work/owner/repo/<minion-id>/`
- Bare repo mirrors: `~/.gru/repos/owner/repo.git`
- Branch naming: `minion/issue-<number>-<minion-id>` (e.g., `minion/issue-42-M001`)
- Example: Minion M001 working on issue 42 → Branch `minion/issue-42-M001` → Worktree `~/.gru/work/owner/repo/M001/`
- Multiple minions can work sequentially on same issue (git manages locking)
- Worktrees are created from bare repos to avoid conflicts
- Stale worktrees are automatically force-removed if they exist

**Timeout & Stuck Detection:**
- Stream timeout: 5 minutes per line (handles long LLM operations)
- Inactivity warning: 5 minutes (warns user of potential stuck state)
- Stuck threshold: 15 minutes (considers task stuck, exits with error)
- Optional timeout flag: `gru fix 42 --timeout 10m` (exits if task exceeds duration)

### Design Philosophy

1. **Local-first** - Works offline except for GitHub API calls
2. **GitHub as database** - No separate state store
3. **Simple before clever** - Polling before webhooks, labels before state machines
4. **Autonomous agents** - Full lifecycle from claim to merge
5. **Single binary** - Easy deployment, no daemon dependencies

## Available Slash Commands

Located in `.claude/commands/`:

- `/fix <issue# or URL>` - Implement a fix for an issue (run from worktree)
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
- Use `ghe` instead of `gh` for Netflix GitHub repos
- Never stage files with `git add -A` - always be explicit
- Authentication via `GRU_GITHUB_TOKEN` environment variable (not in config)
- Labels drive state machine: `ready-for-minion` → `in-progress` → `minion:done`/`minion:failed`
- Comments use YAML frontmatter for structured events
- Issue/PR parsing supports both numbers (when in repo) and full GitHub URLs

### Worktree Management
- Worktree paths use Minion IDs as directory names (deterministic)
- Create from bare repos to avoid conflicts
- Stale worktrees are automatically force-removed if they exist
- Multiple minions can work on same issue sequentially (git handles locking)
- Remove worktrees from outside (not from within themselves)
- Use `gh pr merge --auto` to avoid checkout conflicts on merge
- Post-merge cleanup: remove worktree + delete branch from bare repo

### Error Handling
- Retry with exponential backoff for transient failures
- Max 10-15 retry attempts before pausing for human review
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
├── work/               # Active Minion worktrees
│   └── owner/
│       └── repo/
│           └── M001/   # Minion ID as directory name
│               ├── .git
│               ├── events.jsonl  # Stream events log
│               └── <repo files>
├── state/              # Local state
│   └── next_id.txt     # Minion ID counter
└── archive/            # Completed work (future)
```

## Future Phases

- **V2:** Multi-Lab coordination, distributed locking via GitHub Projects, ACP integration
- **V3:** Tower web UI, WebSocket live updates, OAuth auth
- **V4:** Issue dependency DAG, RAG embeddings, learned prioritization
- **V5:** Multi-repo orchestration, cost accounting, notifications

See `docs/DESIGN.md` for full architecture and `docs/DECISIONS.md` for quantitative decision analysis.
