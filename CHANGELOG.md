# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-03-19

Initial release of Gru, a local-first LLM agent orchestrator that autonomously works on GitHub issues using Claude Code.

### Added

#### Core
- `gru do` command for autonomous issue resolution — claims an issue, implements a fix, creates a PR, and responds to reviews
- Git worktree isolation — each Minion works in its own worktree under `~/.gru/work/`, created from bare repo mirrors
- Real-time stream parsing of Claude Code's JSON event stream (message lifecycle, tool calls, errors)
- Stuck detection with configurable inactivity thresholds (5-minute warning, 15-minute escalation)
- Monotonic Minion IDs in base36 format (M000, M001, ..., M0Z4)
- Persistent Minion registry (`~/.gru/state/minions.json`) with atomic file locking
- Token usage tracking (input, output, cache creation, cache read)

#### Lab Mode
- `gru lab` daemon for continuous repository monitoring and autonomous Minion spawning
- Configurable polling interval, concurrency slots, and target label
- Multi-repo watching in a single daemon process
- Auto-resume of interrupted Minions with per-session deduplication
- Wake-up scan for completed Minions with unaddressed reviews
- Graceful shutdown on SIGINT/SIGTERM with optional child process cleanup

#### PR Lifecycle
- CI monitoring with automatic failure classification (test, build, lint, format, timeout)
- CI auto-fix — invokes the agent with failure context, up to 2 retry attempts before escalating
- PR monitoring loop — polls for reviews, CI status, and merge state every 30 seconds
- Deterministic merge-readiness checks: not draft, CI passing, review approved, no conflicts
- LLM merge judge — evaluates whether review feedback was genuinely addressed before auto-merging
- `gru:auto-merge` label support with configurable confidence threshold
- Review response — detects changes-requested reviews and creates response comments
- Progress comments on PRs with YAML frontmatter for structured event tracking

#### Multi-Agent Backends
- Pluggable `AgentBackend` trait for backend-agnostic agent execution
- Claude Code CLI backend with session persistence across restarts
- OpenAI Codex CLI backend with event mapping to the shared `AgentEvent` model
- `AgentRegistry` for backend resolution (`--agent` flag on `gru do`, `gru review`, `gru prompt`)

#### Configuration
- TOML configuration at `~/.gru/config.toml` with daemon, agent, and merge sections
- GitHub Enterprise Server (GHES) support via named host definitions
- Named repo references (`"name:owner/repo"`) for enterprise hosts
- `gru init` command to set up workspace and create GitHub labels

#### Issue Management
- Issue dependency checking via GitHub's native dependencies API with body-text fallback
- `**Blocked by:** #X, #Y` body-text convention for declaring dependencies
- Label-driven state machine: `gru:todo` → `gru:in-progress` → `gru:done` / `gru:failed`
- Minion resolution by ID, issue number, or PR number

#### Developer Tools
- `gru attach` — open an interactive session on an existing Minion
- `gru resume` — resume a stopped Minion with optional additional instructions
- `gru chat` — interactive project-aware chat session
- `gru logs` — view Minion event streams with follow mode and raw JSONL output
- `gru status` — list active Minions from the registry
- `gru clean` — remove worktrees for merged or closed PRs
- `gru stop` — terminate a running Minion (SIGTERM or SIGKILL)
- `gru path` — resolve a Minion's worktree filesystem path
- `gru rebase` — rebase a Minion branch onto the base branch
- `gru prompt` — ad-hoc prompt execution with template variable support
- `gru prompts` — list available built-in and custom prompt files
- `gru review` — delegate PR review to an agent backend

[Unreleased]: https://github.com/fotoetienne/gru/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/fotoetienne/gru/releases/tag/v0.1.0
