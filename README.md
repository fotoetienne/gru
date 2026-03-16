# Gru: Local-First LLM Agent Orchestrator

Gru is a local-first LLM agent orchestrator that autonomously works on GitHub issues. It manages "Minions" (agent sessions) that claim issues, implement fixes, create PRs, monitor CI, and respond to reviews.

Gru supports multiple agent backends — currently [Claude Code](https://github.com/anthropics/claude-code) and [OpenAI Codex](https://github.com/openai/codex) — with a pluggable architecture for adding more.

## Installation

### Prerequisites

- [Rust](https://rustup.rs/) (1.73 or later)
- At least one agent backend:
  - [Claude Code CLI](https://github.com/anthropics/claude-code) (default)
  - [OpenAI Codex CLI](https://github.com/openai/codex) (optional)
- Git and GitHub CLI (`gh`) recommended

### Install from Source

```bash
git clone https://github.com/fotoetienne/gru.git
cd gru
cargo install --path .
```

This will install the `gru` binary to `~/.cargo/bin/gru`.

## Usage

### Core Commands

**`gru do <issue>`** - Work on a GitHub issue autonomously

Creates a worktree, spawns Claude CLI, monitors progress via stream parsing, creates a PR, monitors CI and reviews, and iterates until done.

```bash
# Work on an issue by number (must be run from within the repo)
gru do 42

# Work on an issue by URL (works from anywhere)
gru do https://github.com/owner/repo/issues/42

# With timeout
gru do 42 --timeout 30m

# Use a specific agent backend
gru do 42 --agent codex
```

**`gru review <pr>`** - Review a GitHub pull request using Claude CLI

```bash
gru review 42
gru review https://github.com/owner/repo/pull/42
```

**`gru prompt <name>`** - Run a custom or built-in prompt

```bash
# Run a named prompt
gru prompt my-prompt --issue 42

# List available prompts
gru prompts
```

### Agent Backends

Gru supports multiple agent backends via the `--agent` flag. The default is `claude`.

```bash
# Use Claude Code (default)
gru do 42

# Use OpenAI Codex
gru do 42 --agent codex

# Agent flag works with review and prompt commands too
gru review 42 --agent codex
gru prompt my-prompt --agent codex
```

Set a default backend in `~/.gru/config.toml`:

```toml
[agent]
default = "codex"

[agent.claude]
binary = "/usr/local/bin/claude"   # Optional: override binary path
```

`gru status` shows which agent each Minion is using:

```
ID       AGENT    REPO                 ISSUE  TASK       PR       BRANCH                         MODE                   UPTIME   TOKENS
M001     claude   owner/repo           #42    do         #43      minion/issue-42-M001           monitoring (PR ready)  5m       1.2M
M002     codex    owner/repo           #44    do         -        minion/issue-44-M002           working                2m       -
```

See [docs/AGENTS.md](docs/AGENTS.md) for setup instructions for each backend.

### Minion Management

```bash
gru status              # List all active Minions
gru status M001         # Show details for a specific Minion
gru attach M001         # Attach terminal to a running Minion
gru stop M001           # Stop a running Minion
gru resume M001         # Resume a stopped Minion
gru rebase M001         # Rebase a Minion's branch
gru path M001           # Print worktree path for a Minion
gru clean               # Remove worktrees for merged/closed PRs
```

### Workspace Setup

```bash
gru init owner/repo                 # Initialize workspace for a repo
gru lab --repos owner/repo          # Start daemon mode with explicit repos
# or configure repos in ~/.gru/config.toml and run: gru lab
```

### Other Commands

```bash
gru --version           # Show version
gru --help              # Show help
gru do --help           # Show help for do command
```

## Error Handling

Gru provides helpful error messages:

- **Invalid issue format**: Clear examples of valid formats (number or GitHub URL)
- **Claude CLI not found**: Direct link to installation instructions
- **Other errors**: Contextual error messages with actionable information

## Development

### Building

```bash
cargo build
```

### Running Tests

```bash
cargo test
```

### Running Clippy

```bash
cargo clippy
```

### Using Just (optional)

This project includes a [Justfile](https://just.systems/) for common tasks:

```bash
just build   # Build the project
just test    # Run tests
just lint    # Run clippy
just check   # Run all checks
```

For a full list of commands with descriptions, run `just --list`.

### Pre-commit Hooks

This project includes pre-commit hooks to ensure code quality before commits are made. The hooks automatically run:

- **Code formatting check** (`just fmt-check`) - Ensures code follows Rust formatting standards
- **Linting** (`just lint`) - Catches common mistakes and enforces best practices across all code including tests
- **Tests** (`just test`) - Validates that all tests pass
- **Branch protection** - Prevents direct commits to the main branch
- **TODO/FIXME check** - Warns about TODO/FIXME comments (warning only, doesn't block commits)

#### Installing Hooks

To enable the pre-commit hooks, run:

```bash
git config core.hooksPath .githooks
```

This tells git to use hooks from the `.githooks/` directory. Simple and standard!

#### Bypassing Hooks

In emergencies, you can bypass the hooks using:

```bash
git commit --no-verify
```

**Note:** Use this sparingly, as it skips important code quality checks.

## Roadmap

Gru is being developed in phases. V1 is feature-complete with worktree management, lab mode, CI monitoring, and PR lifecycle management.

### Completed (V1)
- Autonomous issue fixing with full PR lifecycle
- Git worktree isolation (`~/.gru/work/`, `~/.gru/repos/`)
- Lab mode — continuous polling with configurable slots
- CI monitoring and auto-fix (up to 3 attempts)
- PR review monitoring and automated responses
- Minion management (attach, stop, resume, rebase)
- Custom prompt system with template variables
- Persistent Minion registry

### Future Phases
- **V2:** Multi-Lab coordination, distributed locking via GitHub Projects
- **V3:** Tower web UI, WebSocket live updates, OAuth auth
- **V4:** Issue dependency DAG, RAG embeddings, learned prioritization
- **V5:** Multi-repo orchestration, cost accounting, notifications

## Architecture

Gru's long-term vision includes three main components:

- **Lab**: Local worker that manages Minions and processes GitHub issues
- **Tower** (optional): Web UI and relay for remote access to Labs
- **GitHub**: Acts as the distributed database using issues, labels, and PRs

See [ARCHITECTURE.md](ARCHITECTURE.md) for the complete architecture documentation (coming soon).

## Contributing

Contributions are welcome! Please feel free to submit issues and pull requests.

## License

[License information to be added]

## Related Projects

- [Claude Code](https://github.com/anthropics/claude-code) - Official CLI for Claude
- [Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk) - Agent protocol implementation
- [Happy](https://happy.engineering/) - Mobile client for controlling Claude Code remotely
- [Emdash](https://github.com/generalaction/emdash) - Coding agent orchestration layer supporting 15+ CLIs with parallel execution
- [Code Conductor](https://github.com/ryanmac/code-conductor) - Run multiple Claude Code sub-agents in parallel with GitHub-native orchestration
- [Beads](https://github.com/steveyegge/beads) - Git-backed issue tracker giving coding agents persistent memory across sessions
- [Worktree CLI](https://github.com/agenttools/worktree) - Git worktree management for coding agents with isolated workspaces
- [Vibe Kanban](https://www.vibekanban.com/) - Local orchestration platform for running multiple AI coding agents in parallel with kanban-style task management
- [Cowork](https://support.claude.com/en/articles/13345190-getting-started-with-cowork) - Claude Desktop feature for autonomous multi-step task execution