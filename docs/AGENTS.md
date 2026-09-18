# Agent Backends

Gru uses a pluggable agent architecture. Each backend implements the `AgentBackend` trait, which normalizes different CLI tools into a common event stream that Gru can monitor, log, and act on.

## Available Backends

| Backend | CLI Tool | Flag Value | Status |
|---------|----------|------------|--------|
| Claude Code | `claude` | `--agent claude` | Default |
| OpenAI Codex | `codex` | `--agent codex` | Supported |
| Pi | `pi` | `--agent pi` | Supported |

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

## Pi

[Pi](https://github.com/earendil-works/pi-mono) is a coding agent CLI (`pi`) with a pluggable
provider model — it supports many model providers, configured on the Pi side rather than by Gru.

This backend's event parsing (`src/pi_backend.rs`) was implemented and verified against a
particular Pi distribution's JSON event schema (`session`/`agent_start`, `turn_start`,
`tool_execution_start`/`_end` with `toolCallId`/`toolName`, `turn_end` usage, etc.). If your
`pi` resolves to a build with a different event shape, unrecognized lines are silently
skipped rather than erroring — so a mismatch shows up as missing tool/progress tracking, not
a crash. If progress output is empty or tool calls never appear, compare your `pi`'s
`--mode json` output against the event names above.

### Install

See [pi-mono](https://github.com/earendil-works/pi-mono) for install options (npm
`@earendil-works/pi-coding-agent`). Some environments distribute Pi through a wrapper or
internal package rather than a plain `pi` on `$PATH`; use `[agent.pi].binary` in
`~/.gru/config.toml` to point Gru at it. Model selection can also optionally be driven
from Gru's config — all of these keys live in the same `[agent.pi]` table:

```toml
[agent.pi]
binary = "/usr/local/bin/pi"
model = "anthropic/claude-sonnet-5"   # "provider/id", optionally with a ":<thinking>" suffix
thinking = "high"                     # off | minimal | low | medium | high | xhigh | max
```

Every key is optional and independent — set only the ones you need. Authentication
belongs to Pi's own configuration, not Gru's — Gru does not manage provider credentials.
When `model`/`thinking` are unset, Pi uses whatever it is already configured for.

### Verify

```bash
pi --version
pi --help
```

### How Gru Uses It

Gru spawns Pi in headless mode with JSON output:

```bash
pi -p --mode json --session-id <uuid> [--model <model>] [--thinking <level>] "<prompt>"
```

`--model`/`--thinking` are only added when `[agent.pi]` sets them in config, and are
always placed before the prompt argument in case Pi's `-p` positional is greedy.

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
2. Declare the module in `src/main.rs` with `mod <name>_backend;` (see the existing `mod claude_backend;` / `mod codex_backend;` lines) — without this, `crate::<name>_backend` doesn't exist and nothing else here will compile
3. Register it in `src/agent_registry.rs`:
   - Import the backend type
   - Add the name to `AVAILABLE_AGENTS`
   - Add a matching arm in `construct_backend()` — this is the single source of truth that both `resolve_backend()` and `all_process_names()` route through. **A name added to `AVAILABLE_AGENTS` without a `construct_backend` arm compiles fine but breaks at runtime in two different ways, neither of which is a clean error**: `resolve_backend()` passes the `AVAILABLE_AGENTS` check and then panics on the `.expect()` around `construct_backend`'s `None` result, while `all_process_names()` (used by `gru stop`'s process-scan fallback) silently drops that backend's process names via its `filter_map` with no error at all. No compile error either way.
4. Map the backend's output format to `AgentEvent` variants in `parse_events()`
5. Update the `do` command's `--agent` help string in `src/main.rs` (`"Agent backend to use (claude, codex). Defaults to claude."`) — it enumerates backends by name but isn't derived from `AVAILABLE_AGENTS`, so a new backend added without touching it leaves `--help` output stale. The `review`/`prompt` commands' help (`"Agent backend to use (e.g., 'claude')."`) is a non-exhaustive example, not an enumeration, so it doesn't need updating for each new backend

The `AgentBackend` trait (`src/agent.rs`) currently has eleven methods:
- `name()` — human-readable identifier
- `process_names()` — process name(s) to match for `gru stop`'s pgrep fallback
- `build_command()` — construct the CLI command for a new session
- `parse_events()` — convert stdout lines to normalized `AgentEvent`s
- `build_resume_command()` — required to implement (no default body); return `None` from it if the backend doesn't support resume, `Some(...)` otherwise
- `build_interactive_resume_command()` — required to implement (no default body); return `None` from it to disable attach support
- `build_oneshot_command()` — construct a single-turn, plain-text command (e.g. for the merge-readiness judge)
- `build_ci_fix_command()` — construct a backend-specific streaming-event command (matching whatever format `parse_events()` expects, e.g. Claude's `stream-json` or Codex's JSONL) for stateless CI-fix invocations
- `yolo_args()` — (optional) CLI args to bypass interactive permission prompts for `gru attach --yolo`; defaults to an empty `Vec`
- `final_usage()` — (optional) recover backend-internal accumulated usage on stream EOF when no `Finished { usage: Some(_) }` was seen; defaults to `None`. See the token usage convention below (#914)
- `reset_usage()` — (optional) clear backend-internal accumulated usage; called unconditionally by the runner before spawning each new invocation's process. No-op by default. See the token usage convention below (#914)

**Convention:** new trait methods should carry a default body so existing backends keep compiling without changes, as `yolo_args()` did when added in #911. `process_names()` broke this convention when added in #912 (no default body), which meant every existing backend had to be updated in the same change — do this deliberately, not by accident.

**Token usage convention (#914):** `accumulate_token_usage()` in `src/agent_runner.rs` only reads `output_tokens` from `MessageComplete`, and reads all four fields (input, output, both cache) unconditionally from `Started` and `Finished`. If your backend reports input/cache usage per-turn rather than once at session start (as Pi and Codex do), do **not** add it to `MessageComplete`'s usage — that path is shared by every backend and would double-count or misattribute input tokens for backends that report them elsewhere (e.g. Claude's `Started` event). Instead, accumulate per-turn input/cache totals in backend-internal state (see `PiBackend`/`CodexBackend`'s `Mutex<TokenUsage>` field — safe without real concurrency since one backend instance drives one session's `parse_events()` calls sequentially) and emit the running totals once in a session-end `Finished` event, with `output_tokens: 0` in that event since output is already accumulated per-turn via `MessageComplete`. **A backend instance is reused across independent invocations** (e.g. the CI-fix retry loop in `src/ci.rs` drives multiple attempts through the same `&dyn AgentBackend`), so implement `AgentBackend::reset_usage()` to clear this state — `run_agent_with_stream_monitoring` calls it unconditionally before spawning each invocation's process. **Don't reset on a stream-start event instead** (Pi's `session`/`agent_start`, Codex's `thread.started`): a process that crashes or fails auth before ever emitting that event would leave a prior invocation's totals in place. **If your session-end event is inferred rather than confirmed against real CLI output** (as Codex's `thread.completed` currently is), also implement `AgentBackend::final_usage()` to return the same accumulated totals — `run_agent_with_stream_monitoring` calls it once the stream hits EOF if no `Finished { usage: Some(_) }` was observed, so a wrong event name degrades to "recovered a different way" instead of "silently zero".
