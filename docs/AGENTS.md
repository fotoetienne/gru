# Agent Backends

Gru uses a pluggable agent architecture. Each backend implements the `AgentBackend` trait, which normalizes different CLI tools into a common event stream that Gru can monitor, log, and act on.

## Available Backends

| Backend | CLI Tool | Flag Value | Status |
|---------|----------|------------|--------|
| Claude Code | `claude` | `--agent claude` | Default |
| OpenAI Codex | `codex` | `--agent codex` | Supported |

## Claude Code (default)

[Claude Code](https://github.com/anthropics/claude-code) is the default backend.

### Install

```bash
npm install -g @anthropic-ai/claude-code
```

### Verify

```bash
claude --version
claude --help
```

### Configure

No configuration is required — Gru uses Claude Code by default. Optionally override the binary path in `~/.gru/config.toml`:

```toml
[agent.claude]
binary = "/usr/local/bin/claude"
```

### How Gru Uses It

Gru spawns Claude Code in non-interactive mode with stream JSON output:

```bash
claude --print --verbose --session-id <uuid> --output-format stream-json --dangerously-skip-permissions --include-partial-messages "<prompt>"
```

Key flags:
- `--print` — non-interactive (prints to stdout and exits)
- `--verbose` — include tool calls in output
- `--output-format stream-json` — real-time event stream
- `--dangerously-skip-permissions` — autonomous operation
- `--session-id <uuid>` — maintain context across resumes

## OpenAI Codex

[Codex CLI](https://github.com/openai/codex) is an alternative backend using OpenAI models.

### Install

```bash
npm install -g @openai/codex
```

### Authenticate

Set your OpenAI API key:

```bash
export OPENAI_API_KEY="sk-..."
```

### Verify

```bash
codex --version
codex --help
```

### How Gru Uses It

Gru spawns Codex in full-auto mode with JSON output:

```bash
codex exec --json --full-auto "<prompt>"
```

Resume support uses:

```bash
codex exec resume --last --json --full-auto "<prompt>"
```

Note: Codex does not support interactive resume (`gru attach` will not work with Codex minions). Codex also ignores the `session_id` parameter — it relies on its own session persistence for both new and resumed sessions.

## Selecting a Backend

### Per-command

Use the `--agent` flag on any command that spawns an agent:

```bash
gru do 42 --agent codex
gru review 42 --agent codex
gru prompt my-prompt --agent codex
```

### As default

Set the default in `~/.gru/config.toml`:

```toml
[agent]
default = "codex"
```

The `--agent` flag always overrides the config default.

## Feature Comparison

| Feature | Claude Code | Codex |
|---------|-------------|-------|
| Autonomous work (`gru do`) | Yes | Yes |
| PR review (`gru review`) | Yes | Yes |
| Custom prompts (`gru prompt`) | Yes | Yes |
| Session resume (`gru resume`) | Yes | Yes (non-interactive) |
| Interactive attach (`gru attach`) | Yes | No |
| Token usage tracking | Yes | Yes |
| Stream monitoring | Yes | Yes |

## Adding a New Backend

To add a new agent backend:

1. Create `src/<name>_backend.rs` implementing the `AgentBackend` trait from `src/agent.rs`
2. Declare the module in `src/main.rs` with `mod <name>_backend;` (see the existing `mod claude_backend;` / `mod codex_backend;` lines) — without this, `crate::<name>_backend` doesn't exist and nothing else here will compile
3. Register it in `src/agent_registry.rs`:
   - Import the backend type
   - Add the name to `AVAILABLE_AGENTS`
   - Add a matching arm in `construct_backend()` — this is the single source of truth that both `resolve_backend()` and `all_process_names()` route through. **A name added to `AVAILABLE_AGENTS` without a `construct_backend` arm compiles fine but breaks at runtime in two different ways, neither of which is a clean error**: `resolve_backend()` passes the `AVAILABLE_AGENTS` check and then panics on the `.expect()` around `construct_backend`'s `None` result, while `all_process_names()` (used by `gru stop`'s process-scan fallback) silently drops that backend's process names via its `filter_map` with no error at all. No compile error either way.
4. Map the backend's output format to `AgentEvent` variants in `parse_events()`

The `AgentBackend` trait (`src/agent.rs`) currently has nine methods:
- `name()` — human-readable identifier
- `process_names()` — process name(s) to match for `gru stop`'s pgrep fallback
- `build_command()` — construct the CLI command for a new session
- `parse_events()` — convert stdout lines to normalized `AgentEvent`s
- `build_resume_command()` — required to implement (no default body); return `None` from it if the backend doesn't support resume, `Some(...)` otherwise
- `build_interactive_resume_command()` — required to implement (no default body); return `None` from it to disable attach support
- `build_oneshot_command()` — construct a single-turn, plain-text command (e.g. for the merge-readiness judge)
- `build_ci_fix_command()` — construct a backend-specific streaming-event command (matching whatever format `parse_events()` expects, e.g. Claude's `stream-json` or Codex's JSONL) for stateless CI-fix invocations
- `yolo_args()` — (optional) CLI args to bypass interactive permission prompts for `gru attach --yolo`; defaults to an empty `Vec`

**Convention:** new trait methods should carry a default body so existing backends keep compiling without changes, as `yolo_args()` did when added in #911. `process_names()` broke this convention when added in #912 (no default body), which meant every existing backend had to be updated in the same change — do this deliberately, not by accident.
