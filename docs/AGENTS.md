# Agent Backends

Gru uses a pluggable agent architecture. Each backend implements the `AgentBackend` trait, which normalizes different CLI tools into a common event stream that Gru can monitor, log, and act on.

## Available Backends

| Backend | CLI Tool | Flag Value | Status |
|---------|----------|------------|--------|
| Claude Code | `claude` | `--agent claude` | Default |
| OpenAI Codex | `codex` | `--agent codex` | Supported |
| Netflix Pi | `pi` | `--agent pi` | Supported (internal) |

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

## Netflix Pi

Pi is Netflix's internal coding agent CLI (`pi`, npm `@netflix-internal/pi-agent`), installed via newt at `/opt/nflx/bin/pi`. It is already authenticated to internal model providers, so no separate API key is needed.

### Install

Install via newt (internal Netflix tooling). Not available outside Netflix.

### Verify

```bash
pi --version
pi --help
```

### How Gru Uses It

Gru spawns Pi in headless mode with JSON output:

```bash
pi -p --mode json --session-id <uuid> "<prompt>"
```

Resume uses the same `--session-id` with a new prompt.

Interactive resume (for `gru attach`) drops `-p` and `--mode json`, keeping `--session-id`, since Pi's TUI supports resuming a session with full history — unlike Codex.

There is no `--dangerously-skip-permissions` equivalent for Pi; `bash` and `edit` tool calls run without approval prompts by default under `-p`.

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

| Feature | Claude Code | Codex | Pi |
|---------|-------------|-------|----|
| Autonomous work (`gru do`) | Yes | Yes | Yes |
| PR review (`gru review`) | Yes | Yes | Yes |
| Custom prompts (`gru prompt`) | Yes | Yes | Yes |
| Session resume (`gru resume`) | Yes | Yes (non-interactive) | Yes |
| Interactive attach (`gru attach`) | Yes | No | Yes |
| Token usage tracking | Yes | Yes | Yes |
| Stream monitoring | Yes | Yes | Yes |

## Adding a New Backend

To add a new agent backend:

1. Create `src/<name>_backend.rs` implementing the `AgentBackend` trait from `src/agent.rs`
2. Register it in `src/agent_registry.rs` (add to `AVAILABLE_AGENTS` and the match in `resolve_backend`)
3. Map the backend's output format to `AgentEvent` variants in `parse_events()`

The `AgentBackend` trait requires:
- `name()` — human-readable identifier
- `build_command()` — construct the CLI command for a new session
- `parse_events()` — convert stdout lines to normalized `AgentEvent`s
- `build_resume_command()` — (optional) construct command to resume a session
- `build_interactive_resume_command()` — (optional) construct command for `gru attach`; return `None` to disable attach support
