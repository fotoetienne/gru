# Gru Concepts

Gru runs AI agents that autonomously fix GitHub issues end-to-end. Here is the mental model.

---

## Minions

A **Minion** is an agent session with a unique ID (M000, M001, M002…). When you run `gru do 42`, Gru creates a Minion, assigns it issue #42, and lets it run autonomously. Each Minion handles the full lifecycle: claim the issue, implement a fix, open a PR, monitor CI, and respond to review comments — with no further input from you.

If a Minion fails and you retry, a fresh Minion with a new ID takes over.

## Worktrees

Each Minion works in an isolated **git worktree** under `~/.gru/work/`. Your main working directory is never touched. Multiple Minions can work in parallel on different issues without stepping on each other.

```
~/.gru/work/owner/repo/minion/issue-42-M001/
└── checkout/   ← the actual git worktree
```

The worktree's branch is named `minion/issue-42-M001`. Worktrees persist after the PR merges; run `gru clean` to remove them.

## Labels as State

GitHub labels are Gru's state machine. Gru looks for labels to decide what to do next.

**Issue labels:**

| Label | Meaning |
|---|---|
| `gru:todo` | Ready for a Minion to pick up |
| `gru:in-progress` | Claimed by a Minion, work underway |
| `gru:done` | PR opened, agent finished |
| `gru:failed` | Minion gave up, needs human review |
| `gru:blocked` | Minion hit a wall, needs human input |

**PR labels:**

| Label | Meaning |
|---|---|
| `gru:ready-to-merge` | All checks passed, awaiting merge |
| `gru:auto-merge` | Merge will happen automatically when checks pass |
| `gru:needs-human-review` | Minion escalated; human sign-off required |

Add `gru:todo` to an issue to queue it. Remove it to dequeue.

## GitHub as Database

Gru has no external database. Everything lives in GitHub:

- **Issues** are the task queue
- **Labels** are the state
- **PRs** are the output
- **Comments** are the logs

This means Gru's task state is visible in the GitHub UI, survives restarts, and requires no setup beyond `gh` auth. (Some local runtime metadata — Minion IDs, registry — lives under `~/.gru/state/`.)

## Lab Mode

`gru lab` runs Gru as a daemon. It polls your configured repositories every 30 seconds by default, picks up any issue labeled `gru:todo`, and spawns a Minion for it. Once running, it claims and works any `gru:todo` issue without further input.

```bash
gru lab   # watches all repos in ~/.gru/config.toml
```

You configure which repositories to watch in `~/.gru/config.toml`. Run `gru init` in a repo directory to register it.

## Agent Backends

Gru's orchestration is backend-agnostic. The default backend is Claude Code CLI, but you can configure others (e.g., OpenAI Codex) in `~/.gru/config.toml`. The same lifecycle — claim, implement, PR, monitor, review — runs regardless of which LLM is doing the work.

## The Lifecycle

```
Issue labeled gru:todo
        │
        ▼
Minion claims issue (gru:in-progress)
        │
        ▼
Worktree created, agent implements fix
        │
        ▼
PR opened (gru:done) ──► CI monitored ◄───────────────────┐
                                │                          │
                                ├─ CI fails ──► auto-fix   │
                                │              attempted   │
                                │              (2x max)    │
                                ▼                          │
                        Review comments handled            │
                                │                          │
                                ├─ Blocked ──► gru:blocked │
                                │              + escalation │
                                │                          │
                                └─ Changes requested ──────┘
                                   push fix, re-run CI

        (all checks pass, approved)
                │
                ▼
            PR merged
```

If anything goes unresolvably wrong, the Minion labels the issue `gru:blocked` or `gru:failed` and typically leaves a comment explaining what it needs.
