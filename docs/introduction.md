# Gru

[![CI](https://github.com/fotoetienne/gru/actions/workflows/ci.yml/badge.svg)](https://github.com/fotoetienne/gru/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/fotoetienne/gru/blob/main/LICENSE)

**Gru turns GitHub issues into merged PRs — autonomously, locally, with the AI coding agent of your choice.**

Point it at an issue and it handles the rest: implementation, PR, code review, CI fixes, rebases — all in an isolated worktree that never touches your working directory.

![Animated terminal demo: running "gru do <issue>" — Gru fetches the issue, spawns an agent, and opens a PR autonomously](./demo.gif)

Gru is **agent-agnostic**. It ships with backends for [Claude Code](https://github.com/anthropics/claude-code) and [OpenAI Codex](https://github.com/openai/codex), and its pluggable architecture makes it straightforward to add more.

## Quick Start

```bash
# Install (macOS Apple Silicon — see Getting Started for other platforms)
curl -fL https://github.com/fotoetienne/gru/releases/latest/download/gru-aarch64-apple-darwin.tar.gz | tar xz
sudo mv gru /usr/local/bin/

# Initialize a repo
gru init owner/repo

# Fix an issue — Gru handles the rest
gru do 42
```

For a full walkthrough, see [Getting Started](GETTING_STARTED.md).

## How It Works

1. `gru init owner/repo` creates a bare git mirror at `~/.gru/repos/`
2. `gru do 42` creates an isolated worktree, spawns the agent, and monitors progress via streaming JSON
3. The agent reads the issue, explores the code, makes changes, and runs tests
4. After committing, a code-reviewer subagent checks for correctness, security, and convention issues before the PR is created
5. Gru opens a PR, watches CI, and feeds failures back to the agent for auto-fix (up to 2 attempts before escalating)
6. Review comments are forwarded to the agent for responses
7. Labels (`gru:todo` → `gru:in-progress` → `gru:done` / `gru:failed`) track state on GitHub

## Links

- [GitHub Repository](https://github.com/fotoetienne/gru)
- [Releases](https://github.com/fotoetienne/gru/releases/latest)
- [Contributing](https://github.com/fotoetienne/gru/blob/main/CONTRIBUTING.md)
- [License: Apache 2.0](https://github.com/fotoetienne/gru/blob/main/LICENSE)
