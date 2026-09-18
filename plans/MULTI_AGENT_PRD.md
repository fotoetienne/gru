# Feature: Multi-Agent Backend Support

## Problem Statement

**Who**: Developers using Gru to automate GitHub issue resolution
**Pain**: Gru is hardcoded to Claude Code CLI as its only agent backend. Users who prefer other coding agents (OpenAI Codex, Google Gemini CLI, goose), want cost optimization across providers, or need resilience against provider outages are locked in.
**Impact**: Limits Gru's addressable market, creates single-provider risk, and prevents users from choosing the best agent for their workload.

## Current State

Gru is **deeply coupled** to Claude Code CLI across every layer:

| Layer | Claude-Specific Coupling |
|-------|--------------------------|
| Process spawning | Hardcoded `claude` binary + flags (`--session-id`, `--dangerously-skip-permissions`, `--output-format stream-json`) |
| Stream parsing | Anthropic Messages API streaming format (`message_start`, `content_block_delta`, `tool_use`) |
| Progress display | Parses Claude-specific tool names (`Bash`, `Read`, `Write`, `Edit`, etc.) |
| Prompt construction | Uses Claude Code slash commands (`/do`, `/review`) |
| Session management | Claude's `--session-id` / `--resume` flags |
| Stuck detection | Assumes line-by-line JSON streaming events |

There are **zero abstraction boundaries** — no traits, no interfaces, no pluggable backends.

## Proposed Solution

Introduce an `AgentBackend` trait that abstracts how Gru spawns, monitors, and interacts with coding agents. Ship Claude Code as the default backend, then add Codex CLI as the second backend to validate the abstraction.

The abstraction sits at the **process boundary** — Gru doesn't care how the agent works internally, only that it can:
1. Accept a task (prompt + worktree)
2. Stream progress events
3. Be monitored for stuck/timeout
4. Maintain session context for resume

## User Stories

- As a developer, I want to choose which coding agent Gru uses so that I can use my preferred provider
- As a developer, I want to set a default agent per-repo so that different projects use different agents
- As a developer, I want Gru's progress display to work regardless of which agent backend is running
- As a developer, I want to switch agents without losing worktree/issue state so that I can retry with a different agent if one fails

## Core Principles Check

- **Local-first**: All supported agents (Claude Code, Codex CLI, Gemini CLI) run locally. No cloud dependency added.
- **One binary**: Gru stays one binary. Agents are external CLIs the user installs separately.
- **GitHub as state**: No change. Agent choice is local config, not GitHub state.
- **Stateless Tower**: No change. Tower doesn't need to know which agent a Lab uses.
- **No inter-lab coordination**: No change. Each Minion picks its agent independently.

## Design

### Agent Backend Trait

```rust
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Human-readable name (e.g., "claude", "codex", "gemini")
    fn name(&self) -> &str;

    /// Build the command to spawn this agent
    fn build_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
        prompt: &str,
    ) -> TokioCommand;

    /// Parse a line of stdout into a normalized agent event
    fn parse_event(&self, line: &str) -> Option<AgentEvent>;

    /// Whether this agent supports session resume
    fn supports_resume(&self) -> bool;

    /// Build a resume command (if supported)
    fn build_resume_command(
        &self,
        worktree_path: &Path,
        session_id: &Uuid,
    ) -> Option<TokioCommand>;
}
```

### Normalized Event Model

```rust
pub enum AgentEvent {
    /// Agent started processing
    Started { session_id: String },
    /// Agent is thinking/working (for spinner)
    Thinking,
    /// Agent is using a tool
    ToolUse { tool_name: String, input_preview: Option<String> },
    /// Agent produced text output
    TextDelta { text: String },
    /// Agent completed a message
    MessageComplete { token_usage: Option<TokenUsage> },
    /// Agent finished the task
    Finished { token_usage: Option<TokenUsage> },
    /// Agent encountered an error
    Error { message: String },
    /// Keepalive / heartbeat
    Ping,
}
```

This is the **minimum viable abstraction** — just enough to support progress display, stuck detection, and result capture without leaking agent-specific details.

### Configuration

```toml
# ~/.gru/config.toml

[agent]
default = "claude"  # or "codex", "gemini"

# Per-agent settings
[agent.claude]
binary = "claude"  # default, can override path

[agent.codex]
binary = "codex"
```

CLI override: `gru do 42 --agent codex`

### Agent Client Protocol (ACP)

[ACP](https://github.com/agentclientprotocol/agent-client-protocol) is an emerging standard for editor-to-agent communication. Claude Code, Codex, and Gemini already support it.

**Decision: Don't adopt ACP yet.** Reasons:
1. ACP is designed for interactive editor sessions, not headless orchestration
2. Gru needs monitoring primitives (stuck detection, timeout) that ACP doesn't specify
3. The native trait gives us exactly the abstraction we need
4. We can adopt ACP later as a transport *under* the trait if it matures for our use case

**Revisit ACP in Phase 3** once we have 2+ working backends and understand our actual abstraction requirements.

## MVP Scope (Phase 1-2)

### Phase 1: Extract Abstraction (refactor only)

**In scope:**
- [ ] Define `AgentBackend` trait and `AgentEvent` enum
- [ ] Implement `ClaudeBackend` that wraps existing `claude_runner.rs` logic
- [ ] Refactor `fix.rs`, `review.rs`, `prompt.rs` to use trait instead of direct Claude calls
- [ ] Refactor `progress.rs` to consume `AgentEvent` instead of `ClaudeEvent`
- [ ] Refactor `stream.rs` — Claude-specific parsing moves into `ClaudeBackend`
- [ ] Add `[agent]` config section with `default = "claude"`
- [ ] All existing tests pass, behavior unchanged

**Out of scope:**
- No new agent backends
- No CLI `--agent` flag yet
- No ACP integration

### Phase 2: Codex CLI Backend

**In scope:**
- [ ] Implement `CodexBackend` for OpenAI Codex CLI
- [ ] Map Codex output format to `AgentEvent`
- [ ] Add `--agent` flag to `gru do` and `gru review`
- [ ] Config support for `[agent.codex]`
- [ ] Documentation for setting up Codex with Gru
- [ ] Integration tests with mock Codex output

**Out of scope:**
- Gemini backend (Phase 3)
- Agent-specific prompt optimization (Phase 3)
- Auto-selection / routing (future)

## Future Scope (Phase 3+)

- [ ] Gemini CLI backend
- [ ] ACP evaluation and potential adoption
- [ ] Agent-specific prompt templates (optimize prompts per agent)
- [ ] Agent capability detection (does it support resume? tool calling? streaming?)
- [ ] Fallback routing (if primary agent fails, try secondary)
- [ ] Cost tracking per agent provider
- [ ] Per-issue agent override via GitHub label (e.g., `agent:codex`)

## Success Metrics

- **Phase 1**: All existing tests pass. Zero behavior change. ClaudeBackend is the only implementation.
- **Phase 2**: Codex can complete a simple issue end-to-end via `gru do --agent codex <issue>`. Progress display works for both agents.
- **Adoption signal**: >10% of Gru users configure a non-Claude agent within 30 days of Phase 2 shipping.

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Lowest common denominator — abstraction loses Claude-specific capabilities | Degraded experience for Claude users | Allow backend-specific features via optional trait methods; Claude stays fully featured |
| Agent output formats diverge significantly | Hard to normalize events | Start with Codex (closest to Claude in design); keep AgentEvent minimal |
| Codex/Gemini don't support headless autonomous mode well | Backend is crippled | Validate agent capabilities before committing to full backend; can ship as "experimental" |
| Maintenance burden of multiple backends | Slower feature development | Each backend is isolated behind trait; community can contribute backends |
| Prompt portability — agents behave differently with same prompt | Inconsistent quality | Allow per-agent prompt templates in Phase 3; keep core task description agent-agnostic |

## Open Questions

1. **Session portability**: Can a task started with Claude be resumed with Codex? Probably not — sessions are agent-specific. Is that okay?
2. **Feature parity reporting**: Should `gru status` show which agent each Minion is using? (Probably yes.)
3. **Prompt adaptation**: How much do we need to customize prompts per agent? Is the `/do` prompt Claude-specific or mostly agent-agnostic?
4. **Progress fidelity**: If an agent doesn't stream tool calls, progress display is degraded. Do we show a simpler spinner, or is that a dealbreaker?
5. **Agent availability check**: Should Gru verify the agent binary exists at startup, or fail at spawn time?

## Acceptance Criteria

### Phase 1

**Given** the codebase is refactored to use `AgentBackend` trait,
**When** running `gru do <issue>` with default config,
**Then** behavior is identical to current implementation (Claude Code).

**Given** the `AgentBackend` trait exists,
**When** a developer wants to add a new agent,
**Then** they implement the trait and register it — no changes to `fix.rs` or `progress.rs` needed.

### Phase 2

**Given** Codex CLI is installed and configured in `~/.gru/config.toml`,
**When** running `gru do --agent codex <issue>`,
**Then** Gru spawns Codex, displays progress, and produces a PR.

**Given** a Minion is running with Codex,
**When** viewing `gru status`,
**Then** the agent backend is displayed (e.g., `M042 | codex | issue #123 | running`).
