# Gru: Local-First LLM Agent Orchestrator

Gru is a command-line tool that helps you work with GitHub issues using LLM-based agents. Currently in Phase 1, it provides a convenient way to delegate issue fixing to the Claude CLI.

## Installation

### Prerequisites

- [Rust](https://rustup.rs/) (1.70 or later)
- [Claude CLI](https://github.com/anthropics/claude-code) installed and configured
- Git and GitHub CLI (`gh`) recommended

### Install from Source

```bash
git clone https://github.com/fotoetienne/gru.git
cd gru
cargo install --path .
```

This will install the `gru` binary to `~/.cargo/bin/gru`.

## Usage

### Current Features (Phase 1)

**`gru fix <issue>`** - Fix a GitHub issue using Claude CLI

Delegates to Claude CLI's `/fix` command with improved error handling and validation.

```bash
# Fix an issue by number (must be run from within the repo)
gru fix 42

# Fix an issue by URL (works from anywhere)
gru fix https://github.com/owner/repo/issues/42
```

### Other Commands

```bash
# Show version
gru --version

# Show help
gru --help

# Show help for fix command
gru fix --help
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

## Roadmap

Gru is being developed in phases, with Phase 1 now complete. Future phases will add:

### Phase 2: Local Minion Management
- Direct integration with Claude Agent Protocol
- Local worktree management (`~/.gru/work/`)
- Bare repository mirroring (`~/.gru/repos/`)
- Issue claiming and branch creation

### Phase 3: Lab Mode
- Continuous polling for `ready-for-minion` labeled issues
- Parallel Minion execution with configurable slots
- GraphQL API for querying Minion status
- WebSocket support for real-time updates
- Post-PR monitoring (reviews, CI failures, comments)

### Phase 4: Tower Mode
- Web UI for monitoring Labs and Minions
- Multi-Lab coordination
- Proxy layer for remote access
- Handoff and live attach sessions

### Phase 5: Advanced Features
- Learned prioritization
- Multi-repo orchestration
- Local embedding index
- Cost and token accounting
- Slack/mobile notifications

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
