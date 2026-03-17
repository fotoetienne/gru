# Gru Concepts

A concise mental model for understanding how Gru works.

---

## Minions

A **Minion** is an agent session with a unique ID (M000, M001, M002…). When you run `gru do 42`, Gru creates a Minion, assigns it issue #42, and lets it run autonomously. Each Minion handles the full lifecycle: claim the issue, implement a fix, open a PR, monitor CI, and respond to review comments — with no further input from you.

Minions are numbered monotonically and never reused. If a Minion fails and you retry, you get a new Minion with a new ID.

## Worktrees

Each Minion works in an isolated **git worktree** under `~/.gru/work/`. Your main working directory is never touched. Multiple Minions can work in parallel on different issues without stepping on each other.

```
~/.gru/work/owner/repo/minion/issue-42-M001/
└── checkout/   ← the actual git worktree
```

The worktree's branch is named `minion/issue-42-M001`. When the PR merges, Gru cleans up the worktree automatically.

## Labels as State

GitHub labels are Gru's state machine. Gru looks for labels to decide what to do next:

| Label | Meaning |
|---|---|
| `gru:todo` | Ready for a Minion to pick up |
| `gru:in-progress` | Claimed by a Minion, work underway |
| `gru:done` | PR merged, issue resolved |
| `gru:failed` | Minion gave up, needs human review |
| `gru:blocked` | Minion hit a wall, needs human input |

Add `gru:todo` to an issue to queue it. Remove it to dequeue.

## GitHub as Database

Gru has no external database. Everything lives in GitHub:

- **Issues** are the task queue
- **Labels** are the state
- **PRs** are the output
- **Comments** are the logs

This means Gru's full state is visible in the GitHub UI, survives restarts, and requires no setup beyond `gh` auth.

## Lab Mode

`gru lab` runs Gru as a daemon. It polls your configured repositories every 30 seconds, picks up any issue labeled `gru:todo`, and spawns a Minion for it. Set it and forget it.

```bash
gru lab   # watches all repos in ~/.gru/config.toml
```

## Agent Backends

Gru's orchestration is backend-agnostic. The default backend is Claude Code CLI, but you can configure others (e.g., OpenAI Codex). The same lifecycle — claim, implement, PR, monitor, review — runs regardless of which LLM is doing the work.

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
PR opened, CI monitored
        │
        ├─ CI fails ──► auto-fix attempted (up to 2x)
        │
        ▼
Review comments handled
        │
        ├─ Blocked ───► gru:blocked label + human escalation
        │
        ▼
PR merged, worktree cleaned up (gru:done)
```

If anything goes unresolvably wrong, the Minion labels the issue `gru:blocked` or `gru:failed` and leaves a comment explaining what it needs.
