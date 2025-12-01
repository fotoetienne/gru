# Gru: Local-First LLM Agent Orchestrator

## 0) One-paragraph TL;DR

**Gru** is a single-binary, local-first orchestrator for running and supervising LLM-based agents that work on GitHub issues. Each Gru instance (a **Lab**) runs **Minions** locally—fetching issues, creating branches, generating code, testing, and opening PRs. After a PR is submitted, Minions remain active to monitor reviews, respond to comments, and react to failed checks. A **Tower** (optional) provides a web UI and proxy layer for remote access, handoffs, and live attach sessions. Labs don’t know about each other; GitHub acts as the shared source of truth. One binary, three modes: `gru lab`, `gru tower`, and `gru up` (both).

---

## 1) Core Concepts

### **Lab**

* A local worker that polls GitHub for issues labeled `ready-for-minion`.
* Claims issues optimistically, creates worktrees, launches Minions, and opens PRs.
* Keeps Minions alive post-PR to handle follow-up interactions such as code review feedback, test reruns, or CI failures.
* Exposes a **GraphQL API** and **WebSocket** for real-time event streaming.
* Runs fully offline except for GitHub and (optionally) Tower.

### **Tower** (optional)

* Hosts a web UI for viewing Minions, handoffs, and logs.
* Acts as a **relay** for Labs that dial out (no inbound networking required).
* Proxies GraphQL requests and PTY sessions to connected Labs.
* Stateless—if restarted, Labs reconnect and re-register.

### **GitHub as the database**

* **Issues** = task queue.
* **Labels** = state machine (`ready-for-minion`, `in-progress:<minion-id>`, `done/failed`).
* **PRs** = results and feedback loop.
* **Comments** = structured logs, handoffs, and review discussions.

---

## 2) System Overview

```
┌─────────────┐        WS + GraphQL proxy         ┌──────────────┐
│   Browser   │ <────────────────────────────────>│    Tower     │
│ (Web UI)    │                                   │ (optional)   │
└─────────────┘                                   └──────┬───────┘
                                                         │
                                                secure dial-out WS
                                                         │
                                                   ┌──────┴──────┐
                                                   │    Lab      │
                                                   │ (gru lab)   │
                                                   └──────┬──────┘
                                                          │
                                                GitHub API + Git ops
                                                          │
                                                   ┌──────┴──────┐
                                                   │   GitHub    │
                                                   │ (Issues/PRs)│
                                                   └─────────────┘
```

---

## 3) CLI Modes

### `gru lab`

Runs the local agent engine.

```bash
gru lab --port 7777 --slots 2 --tower https://tower.example.com
```

* Polls GitHub for `ready-for-minion` issues.
* Claims issues and launches Minions.
* Exposes GraphQL + WebSocket APIs.
* Optionally dials out to Tower.

### `gru tower`

Hosts the web UI and relays to connected Labs.

```bash
gru tower --port 8080 --ui ./ui-dist
```

* Proxies GraphQL and attach sessions.
* Performs GitHub OAuth for web users.
* No scheduling or storage.

### `gru up`

Convenience command to start both in one process.

```bash
gru up --slots 2 --port 7777 --ui ./ui-dist
```

---

## 4) Lab GraphQL API

### HTTP Endpoint

* `POST /graphql` for queries/mutations.

### WebSocket Endpoint

* `WS /graphql` (GraphQL over WebSocket, using `graphql-transport-ws` protocol).
* Used for **subscriptions** (live updates: Minion events, handoffs, attach output).

### Example Schema (simplified)

```graphql
schema { query: Query, mutation: Mutation, subscription: Subscription }

type Query {
  lab: LabInfo!              # metadata about this Lab instance
  issuesReady(repo: String!): [Issue!]!
  minions(state:[MinionState!]): [Minion!]!
}

type Mutation {
  startMinion(repo:String!, issueNumber:Int!): Minion!
  respondHandoff(minionId:ID!, data:JSON!): Boolean!
  openAttach(minionId:ID!): AttachSession!
}

type Subscription {
  minionEvents(minionId:ID!): MinionEvent!
  handoffs(repo:String): Handoff!
}
```

### Notes

* The `lab` field identifies the current Lab (hostname, slots, version, capabilities).

### Attach WebSocket

* `WS /attach/:sessionId` for real-time terminal (Minion REPL) streaming.

---

## 5) GitHub Integration

| Object      | Purpose      | Example                                                    |
| ----------- | ------------ | ---------------------------------------------------------- |
| **Label**   | State        | `ready-for-minion`, `in-progress:M42`, `minion:done`       |
| **Comment** | Minion trace | `[minions] claimed minion=M42 branch=minion/issue-123-M42` |
| **PR**      | Result       | `[minions][M42] Fixes #123`                                |

**First-PR-Wins Rule**: If multiple Labs claim an issue, the first PR merged/opened becomes canonical; others yield.

After submitting a PR, Minions monitor for:

* **Review comments**: automatically update code or seek human input.
* **Failed checks**: attempt to fix errors and push new commits.
* **Merge events**: mark `minion:done` and archive.

---

## 6) Tower Proxy Endpoints

```
GET  /ui/*                          # Static web bundle
POST /labs/:labId/graphql           # HTTP GraphQL proxy
WS   /labs/:labId/graphql           # GraphQL WS proxy
WS   /labs/:labId/attach/:sessionId # PTY proxy
WS   /labs/connect                  # Lab dial-out (control + events)
```

Tower relays requests to Labs that have dialed out; clients never connect directly to Labs.

---

## 7) File Layout (Lab)

```
~/.gru/
  repos/<owner>/<repo>.git          # bare mirrors
  work/<owner>/<repo>/<MINION_ID>/  # worktrees
  archive/<MINION_ID>/              # plan.md, events.jsonl, junit.xml
  config.yaml                       # local config
```

---

## 8) Example Lifecycle

1. Lab polls for `ready-for-minion` issues.
2. Claims one: adds `in-progress:<MINION_ID>` and posts claim comment.
3. Creates worktree and launches Minion.
4. Streams `minionEvents` (plan, patch, tests, PR).
5. Opens PR (`Fixes #123`) and posts summary.
6. Minion remains active for reviews, failed checks, and discussion updates.
7. Marks `minion:done`, archives logs, cleans up after merge.

If a second Lab picks the same issue, duplicate work is tolerated and visible in GitHub.

---

## 9) Security Model

* **Lab** holds your GitHub token; scoped to repos only.
* **Tower** never stores secrets; Labs authenticate via short-lived tokens.
* **Attach sessions** are temporary (5–15 min) and validated per-minion.
* **No inbound Lab traffic**: Labs dial out to Tower.

---

## 10) Why This Design

* **Local-first**: runs even if Tower is off.
* **One binary** keeps install & update simple.
* **GitHub as state** eliminates DB complexity.
* **Stateless Tower** enables easy restarts and remote UI.
* **Persistent Minions** ensure PRs receive intelligent follow-up without human babysitting.
* **No inter-lab coordination**: eventual consistency through GitHub.
* **Explicit Lab identity** avoids ambiguity in APIs.

---

## 11) Future Extensions

* Learned prioritizer for issues.
* Multi-repo orchestration.
* Local embedding index for code context.
* Cost and token accounting.
* Slack / mobile notifications via Tower.
* Continuous review support and reviewer feedback learning.
