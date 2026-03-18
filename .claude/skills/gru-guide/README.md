# gru-guide skill

An interactive guide that helps users install, configure, and use Gru.

## What it does

The skill activates when you ask about Gru setup, commands, or concepts, and walks you through whatever you need — interactively, running checks along the way.

**Capabilities:**
- **Onboarding** — prerequisites check, install, `gru init`, first `gru do`
- **Command reference** — explains any `gru` command with examples
- **Config help** — reads `~/.gru/config.toml` and explains settings
- **Concept explanations** — Minions, worktrees, labels-as-state, lifecycle
- **Troubleshooting** — diagnoses errors and suggests fixes

## How to activate

Just ask naturally:

- "How do I set up Gru?"
- "What does `gru lab` do?"
- "My Minion is stuck — what should I check?"
- "How do I configure a default agent backend?"
- "Explain what a worktree is"

## Knowledge base

The `docs/` directory contains symlinks to the live project documentation:

```
docs/
├── README.md        → ../../../../README.md
├── GETTING_STARTED.md → ../../../../docs/GETTING_STARTED.md
├── CONCEPTS.md      → ../../../../docs/CONCEPTS.md
├── AGENTS.md        → ../../../../docs/AGENTS.md
├── DESIGN.md        → ../../../../docs/DESIGN.md
├── DECISIONS.md     → ../../../../docs/DECISIONS.md
└── CLAUDE.md        → ../../../../CLAUDE.md
```

These symlinks mean the skill always has up-to-date information without duplicating content. When Claude reads `.claude/skills/gru-guide/docs/GETTING_STARTED.md`, it reads the actual project docs.
