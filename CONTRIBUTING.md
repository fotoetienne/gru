# Contributing to Gru

Thanks for your interest in contributing to Gru! This guide covers everything you need to get started.

## Prerequisites

- [Rust](https://rustup.rs/) (1.73 or later)
- [just](https://github.com/casey/just) command runner
- [cargo-nextest](https://nexte.st/) (`cargo install cargo-nextest`)
- Git and [GitHub CLI](https://cli.github.com/) (`gh`)
- At least one agent backend:
  - [Claude Code CLI](https://github.com/anthropics/claude-code) (default)
  - [OpenAI Codex CLI](https://github.com/openai/codex) (alternative)

## Dev Setup

```bash
git clone https://github.com/fotoetienne/gru.git
cd gru
```

Enable pre-commit hooks (recommended):

```bash
git config core.hooksPath .githooks
```

Build and run tests to verify your setup:

```bash
just check
```

## Building

```bash
just build            # Debug build
just build-release    # Release build
just install          # Release build and install to ~/.cargo/bin/
```

## Testing

```bash
just test             # Run all tests
just test-verbose     # Run tests with output
```

## Linting & Formatting

```bash
just fmt              # Format code
just fmt-check        # Check formatting without modifying
just lint             # Run clippy with warnings as errors
just fix-clippy       # Auto-fix clippy lints where possible
```

## Pre-Submit Checklist

Before submitting a PR, run:

```bash
just check
```

This runs a formatting check, linting, tests, and a build in one command. The pre-commit hooks run these same checks automatically if you've enabled them.

## Pre-Commit Hooks

The `.githooks/pre-commit` hook runs on every commit:

1. **Branch protection** (blocks direct commits to `main`)
2. **Formatting check** (`just fmt-check`)
3. **Clippy linter** (`just lint`)
4. **Tests** (`just test`)
5. **TODO/FIXME warnings** (non-blocking)

If a hook fails, fix the issue and re-commit. Do not use `git add -A` or `git add .` — always stage files explicitly.

## PR Workflow

1. Create a feature branch from `main`
2. Make your changes in small, focused commits
3. Run `just check` to verify everything passes
4. Push your branch and open a PR against `main`
5. Ensure CI checks pass
6. Address review feedback

Keep PRs focused on a single change. If your work touches multiple areas, consider splitting it into separate PRs.

## Issue Labels

Gru uses labels to track issue and PR state:

| Label | Meaning |
|-------|---------|
| `gru:todo` | Issue is ready to be worked on |
| `gru:in-progress` | Work is actively underway |
| `gru:done` | Work is complete |
| `gru:failed` | Minion was unable to complete the work |
| `gru:blocked` | Issue is blocked and needs attention |
| `gru:ready-to-merge` | PR is approved and ready to merge |
| `gru:auto-merge` | PR should be auto-merged when checks pass |
| `gru:needs-human-review` | PR requires human review before merging |
| `priority:high` | High-priority issue |
| `priority:medium` | Medium-priority issue |
| `priority:low` | Low-priority issue |

## Architecture

For architectural context, module structure, and design decisions, see [CLAUDE.md](CLAUDE.md). That file is the authoritative reference for how the codebase is organized.

## Code Style

- Use `anyhow::Result` for error handling with descriptive context via the `Context` trait
- All code must pass `cargo clippy --all-targets -- -D warnings`
- Format with `cargo fmt --all`
- Write tests for new functionality
