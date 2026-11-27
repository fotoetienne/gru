# Antigravity (`agy`) Agent Backend Implementation Plan

**Status:** Draft — awaiting PR 0 live CLI capture spike before coding
**Date:** 2026-11-26
**Relates to:** `plans/MULTI_AGENT_PRD.md`, `plans/PI_BACKEND_PLAN.md`, `docs/AGENTS.md`

## Summary

Add Google Antigravity CLI (`agy`, verified against v1.5.18) as a fourth `AgentBackend` alongside `claude`, `codex`, and `pi`, so Minions can run with `--agent antigravity` or `[agent] default = "antigravity"`.

---

## 1. Findings from the Codebase

### `AgentBackend` Trait (`src/agent.rs:255-456`)

`AgentBackend: Send + Sync` defines **14 methods** — **9 required** (no default body) and **5 optional** (defaulted, per the convention in `docs/AGENTS.md:258`):

| Method | Location | Required? | Purpose & Notes |
|---|---|---|---|
| `name` | `src/agent.rs:257` | Required | Display name (`"antigravity"`). |
| `process_names` | `src/agent.rs:264` | Required | Process names for `gru stop`'s `pgrep -f` fallback. **Subtle coupling:** `spawn_error_context` (`src/agent.rs:223-227`) uses `process_names()[0]` as the `[agent.<key>]` config section name. See Sections 5 & 9. |
| `build_command` | `src/agent.rs:270-276` | Required | New autonomous stream-json session. Must set `GH_HOST`, `current_dir(worktree_path)`, piped stdout, and `env_remove` `GRU_RETRY_PARENT` and `GRU_CONFIG_PATH` (matching `src/claude_runner.rs:44-45`, `src/codex_backend.rs:221-222`, `src/pi_backend.rs:331-332`). |
| `parse_events` | `src/agent.rs:285` | Required | Parse one stdout line into `Vec<AgentEvent>`. Silently return `Vec::new()` on blank/non-JSON/unrecognized lines. |
| `build_resume_command` | `src/agent.rs:291-297` | Required | Autonomous resume (`Option<TokioCommand>`). Used by `gru resume`, `gru do` auto-resume, and PR review responses (`src/commands/fix/agent.rs:156-163`, `src/commands/fix/agent.rs:484-486`). |
| `build_interactive_resume_command` | `src/agent.rs:306-311` | Required | Interactive TUI resume (`Option<TokioCommand>`) for `gru attach` (`src/commands/attach.rs:167-182`). Inherited stdio, no `-p` or stream-json. |
| `build_interactive_command` | `src/agent.rs:336-342` | Required | Fresh interactive TUI session with `system_prompt` and optional `initial_prompt` (after `--`) for `gru chat`/`pm`/`tpm` (`src/commands/chat.rs:58-60`, `src/commands/pm.rs:61-70`). Return `None` if unsupported. |
| `build_oneshot_command` | `src/agent.rs:361-366` | Required | Single-turn plain-text invocation (used by `src/merge_judge.rs:828` with `prompt_arg = "-"` streaming prompt over stdin). Must support the `"-"` stdin sentinel. |
| `build_ci_fix_command` | `src/agent.rs:378-383` | Required | Stateless stream-json invocation for CI auto-fix (`src/ci.rs:963`). Must not reuse a session ID. |
| `yolo_args` | `src/agent.rs:392-394` | Optional (default `vec![]`) | Extra flags appended by `gru attach --yolo` (`src/commands/attach.rs:206-208`). |
| `install_url` | `src/agent.rs:402-404` | Optional (default `None`) | URL appended to `spawn_error_context`. |
| `final_usage` | `src/agent.rs:423-425` | Optional (default `None`) | EOF fallback called by `run_agent_with_stream_monitoring` (`src/agent_runner.rs:367-374`) if no `Finished { usage: Some(_) }` was emitted. |
| `reset_usage` | `src/agent.rs:443` | Optional (no-op) | Called unconditionally before spawning each invocation (`src/agent_runner.rs:222`) because backend instances are reused across CI-fix retries. |
| `sanitize_oneshot_output` | `src/agent.rs:453-455` | Optional (identity) | Strip launcher stdout chatter if any (used by `PiBackend`, `src/merge_judge.rs:860`). |

### `AgentEvent` & `TokenUsage` Model (`src/agent.rs:20-93, 138-150`)

- Variants: `Started { usage }`, `Thinking { text }`, `ToolUse { tool_name, tool_use_id, input_summary }`, `ToolResult { tool_use_id, content, is_error }`, `TextDelta { text }`, `MessageComplete { stop_reason, usage }`, `Finished { usage }`, `Error { message }`, `ModelInfo { provider, model }`, `Ping`.
- `TokenUsage` fields: `input_tokens: u64`, `output_tokens: u64`, `cache_creation_input_tokens: Option<u64>`, `cache_read_input_tokens: Option<u64>`, `cost: Option<f64>`.
- **Runner accounting rules (`src/agent_runner.rs:401-439`):**
  - `Started { usage }` adds `input_tokens`, cache fields, `cost` (ignores `output_tokens`).
  - `MessageComplete { usage }` adds **only** `output_tokens` (ignores `input_tokens`, cache fields, `cost`).
  - `Finished { usage }` adds **all** fields (`input_tokens`, `output_tokens`, cache fields, `cost`).
- **Phase detection coupling (`src/commands/fix/agent.rs:394-433`):** `is_edit_tool()` matches `"edit" | "write" | "notebookedit" | "file_change"` (case-insensitive) and `is_command_tool()` matches `"bash" | "command"`. `is_test_command()` strips an optional `"Run: "` prefix from `input_summary`. Antigravity tool names (e.g., `run_command`, `write_to_file`, `replace_file_content`, `write_file`, `edit_file`) will not trigger `Implementing`/`Testing` progress-comment transitions unless either normalized in the backend or added to those matchers.

### Comparison of Existing Backends

- **`ClaudeBackend` (`src/claude_backend.rs`):** Pre-assigns `--session-id <uuid>`, resumes via `--resume <uuid>`, buffers `ToolUse` JSON across `ContentBlockStart`/`InputJsonDelta`/`ContentBlockStop`, supports both `gru attach` and `gru chat`/`pm`/`tpm`, reports input usage on `Started` and output on `MessageComplete`.
- **`PiBackend` (`src/pi_backend.rs`):** Closest config/structure analogue (`binary`, `model`, `thinking` validated against `PI_THINKING_LEVELS`). Pre-assigns `--session-id <uuid>`, parses `usage` defensively via `serde_json::Value` (`src/pi_backend.rs:458-467`), accumulates per-turn input/cache/cost in `Mutex<TokenUsage>` and emits totals on `Finished` with `output_tokens: 0`, emits `ModelInfo`, supports `gru attach` and `gru chat`/`pm`/`tpm`.
- **`CodexBackend` (`src/codex_backend.rs`):** Closest behavioral analogue for session ID and interactive limits: ignores Gru's pre-generated `session_id`, returns `None` from both `build_interactive_resume_command` and `build_interactive_command`, uses `--dangerously-bypass-approvals-and-sandbox` because its sandbox blocks `~/.gru/repos/` gitdir writes and `gh` network calls (`src/codex_backend.rs:182-190`).

### Verified `agy` CLI Facts (local `agy` v1.5.18)

Read-only checks against `agy --help`, `agy --version`, and flag-parsing probes in `/tmp` established:

- **`-p` / `--print` takes a string value**, NOT a boolean flag (`agy -p --model ... "hi"` fails with exit code 2: `Error: -p took "--model" as its prompt... Attach the prompt to the flag (-p='your prompt') and move --model elsewhere on the command line.`). Passing `--print=<prompt>` (or `-p=<prompt>`) avoids ambiguity when prompts start with `-` (e.g., YAML frontmatter `---` or `-h`).
- **`--effort`** accepts `low|medium|high|xhigh|max`.
- **`--mode`** accepts `accept-edits|auto|plan`.
- **`--conversation`** (aliases `-r`, `--resume`) resumes by ID; `--continue` (`-c`) continues most recent.
- **`--print-timeout`** defaults to `0s` (waits until turn completes).
- **`--disable-slash-commands`** disables slash-command/skill expansion in print mode.
- **No `--system-prompt` flag exists** in `agy --help` (v1.5.18).
- **Stderr vs stdout:** Startup/error diagnostics go to `stderr` (`STDOUT: 0` on startup failure), keeping `stdout` clean for NDJSON/text.
- **Wrapper & subprocesses:** `/opt/homebrew/bin/agy` symlinks to a Homebrew wrapper script (`Usage of antigravity:`) and spawns an embedded language server (`Starting language server process with pid ...`).

---

## 2. Event Mapping (`agy` NDJSON -> `AgentEvent`)

| `agy` event | `step_type` / `state` / fields | Target `AgentEvent` | Status | Notes |
|---|---|---|---|---|
| `init` | `conversation_id`, `init.{cwd, tools, permission_mode}` | `AgentEvent::Started { usage: None }` | **Verified** (envelope) / **Assumption** (no usage/model on init) | Capture `conversation_id` internally (see Section 4). If `init` also exposes a `model` field, emit `AgentEvent::ModelInfo { provider: None, model }` too. |
| `step_update` | `step_type == "user_input"` (any `state`) | *(none — ignore)* | **Verified** (`step_type` name) | Echo of the user prompt; must not emit `TextDelta`. |
| `step_update` | `step_type == "agent_response"`, `state == "ACTIVE"`, non-empty `text_delta` | `AgentEvent::TextDelta { text }` | **Verified** | Stream incremental assistant text. Ignore empty `text_delta`. |
| `step_update` | `step_type == "agent_response"`, `state == "DONE"` | `AgentEvent::MessageComplete { stop_reason: Some("end_turn"), usage: None }` (plus `TextDelta` only if `text_delta` is a final unstreamed chunk) | **Verified** (`state`/`step_type`) / **Assumption** (`text_delta` on `DONE` & usage semantics) | **Important:** Must verify whether `DONE` repeats full text in `text_delta`. Keep `usage: None` here if `result.usage` provides aggregate session totals (avoids double-counting in `accumulate_token_usage`); still fold per-step usage into `accumulated_usage` for `final_usage()` EOF fallback. |
| `step_update` | Thinking / reasoning step (e.g., `step_type` in `"thinking" \| "thought" \| "planner_thought"`) | `AgentEvent::Thinking { text }` | **Assumption** | Exact `step_type` and delta field name unknown; update status spinner without spamming text output. |
| `step_update` | Tool-call step, `state == "ACTIVE"` (first seen for `step_index`) | `AgentEvent::ToolUse { tool_name, tool_use_id: step_index.to_string(), input_summary }` | **Assumption** | Emit once per `step_index` (dedup via `HashSet<u64>` if multiple `ACTIVE` updates arrive for a running command). Format summaries like `Run: <cmd>`, `Read: <path>`, `Edit: <path>`, `Write: <path>`. |
| `step_update` | Tool-call step, `state == "DONE"` (or error/failed state) | `AgentEvent::ToolResult { tool_use_id: step_index.to_string(), content, is_error }` | **Assumption** | Exact payload keys for tool output and error/exit-code flag must be verified from captured samples. |
| `result` | `result.status == "SUCCESS"`, `result.usage` | `AgentEvent::Finished { usage: Some(TokenUsage) }` | **Verified** (`status == "SUCCESS"`, `usage` present) / **Assumption** (exact field rollup & `thinking_tokens`) | Do **not** re-emit `result.response` as `TextDelta` if `agent_response` deltas already streamed it (fallback to emitting it only if zero `TextDelta`s were seen). |
| `result` | `result.status != "SUCCESS"` (e.g., `"ERROR"`, `"TIMEOUT"`, `"CANCELLED"`, `"RATE_LIMITED"`) | `[AgentEvent::Error { message }, AgentEvent::Finished { usage }]` | **Assumption** | Emit `Error` with best available error message from `result`, followed by `Finished` if `usage` is present so token accounting is preserved. |
| `error` (top-level, if it exists) | `{"event":"error", ...}` | `AgentEvent::Error { message }` | **Assumption** | Unknown whether `agy` emits a top-level `error` event or encodes failures in `result` / `stderr`. |
| Unrecognized event / non-JSON / blank line | — | `Vec::new()` | **Verified** (contract in `src/agent.rs:283-284`) | Silently skip; still counts as liveness in `src/agent_runner.rs:333-337`. |

---

## 3. Unknowns to Resolve Before Coding

Run these read-only / `/tmp`-scoped capture commands outside the sandbox (live `agy` calls require network + `~/.gemini/antigravity-cli/` access, which could not be exercised inside the read-only sandbox):

1. **Tool call & tool result shapes (read, write, edit, bash success, bash failure) + multi-turn usage rollup:**
   ```bash
   mkdir -p /tmp/agy-probe && cd /tmp/agy-probe && printf 'hello\n' > sample.txt
   agy --output-format stream-json --dangerously-skip-permissions --effort high \
     --print='1) Read sample.txt. 2) Edit sample.txt to replace hello with world. 3) Run `echo ok`. 4) Run `false` (expect exit 1). 5) Summarize in one sentence.' \
     > /tmp/agy-probe/tools.ndjson 2> /tmp/agy-probe/tools.stderr
   echo "EXIT=$?"
   ```
   *Check:* Every distinct `step_type` and `state`; whether `step_update` fires multiple `ACTIVE` events per `step_index`; tool name/args/output/error fields; whether thinking steps exist; whether `DONE` repeats `text_delta`; whether `step_update.usage` is per-step or cumulative; whether `result.usage` equals the sum of steps; whether `thinking_tokens` is included in `output_tokens` or separate; whether `init` or `result` exposes the resolved model name.

2. **Stdin handling for `build_oneshot_command` (`"-"` sentinel used by `src/merge_judge.rs:828`):**
   ```bash
   printf 'Reply with only: {"ok":true}\n' | agy --output-format text --dangerously-skip-permissions -p - > /tmp/agy-probe/stdin-dash.out 2> /tmp/agy-probe/stdin-dash.err; echo "DASH_EXIT=$?"
   printf 'Reply with only: {"ok":true}\n' | agy --output-format text --dangerously-skip-permissions -p "" > /tmp/agy-probe/stdin-empty.out 2> /tmp/agy-probe/stdin-empty.err; echo "EMPTY_EXIT=$?"
   printf '{"type":"user","content":"Reply with only: OK"}\n' | agy --input-format stream-json --output-format stream-json --dangerously-skip-permissions -p "" > /tmp/agy-probe/stdin-stream.out 2> /tmp/agy-probe/stdin-stream.err; echo "STREAM_EXIT=$?"
   ```
   *Check:* Which invocation reads a large prompt from stdin without hitting OS `ARG_MAX` and returns clean plain text on stdout. If `-p` cannot read stdin at all, document whether `build_oneshot_command` must use a different flag (or `--input-format`) or if `src/merge_judge.rs` needs a fallback.

3. **Error shapes (bad model, invalid effort, timeout, interrupted run):**
   ```bash
   agy --output-format stream-json --model nonexistent-model-xyz --print='hi' > /tmp/agy-probe/bad-model.out 2> /tmp/agy-probe/bad-model.err; echo "BAD_MODEL_EXIT=$?"
   agy --output-format stream-json --effort bogus --print='hi' > /tmp/agy-probe/bad-effort.out 2> /tmp/agy-probe/bad-effort.err; echo "BAD_EFFORT_EXIT=$?"
   agy --output-format stream-json --dangerously-skip-permissions --print-timeout 2s --print='Run `sleep 10` then say done' > /tmp/agy-probe/timeout.out 2> /tmp/agy-probe/timeout.err; echo "TIMEOUT_EXIT=$?"
   ```
   *Check:* Exact `result.status` values, error JSON fields vs stderr-only errors, and exit codes. (For an interrupted run, run a `sleep 10` prompt and send `SIGTERM`/`SIGINT` after 2s, inspecting stdout/stderr/exit code and whether any child language-server process survives.)

4. **Session resume (`--conversation`), unknown ID behavior, and interactive attach:**
   ```bash
   CID=$(jq -r 'select(.event=="init") | .conversation_id' /tmp/agy-probe/tools.ndjson)
   agy --conversation "$CID" --output-format stream-json --dangerously-skip-permissions --print='What word did we write into sample.txt?' > /tmp/agy-probe/resume.ndjson 2> /tmp/agy-probe/resume.err; echo "RESUME_EXIT=$?"
   agy --conversation 00000000-0000-0000-0000-000000000000 --output-format stream-json --dangerously-skip-permissions --print='hi' > /tmp/agy-probe/bad-cid.out 2> /tmp/agy-probe/bad-cid.err; echo "BAD_CID_EXIT=$?"
   ```
   *Check:* Confirm `CID` is always a UUID; confirm resumed `init` keeps the same `conversation_id` and remembers prior context; confirm whether an unknown UUID fails loudly (critical for pre-`init` crash recovery); test `agy --conversation "$CID"` interactively in a TTY to verify `gru attach`.

5. **Process names & signal cleanup during a live run:**
   ```bash
   agy --output-format stream-json --dangerously-skip-permissions --print='Run `sleep 5`' >/dev/null 2>&1 &
   PID=$!; sleep 2; ps -eo pid,ppid,comm,args | grep -E 'agy|antigravity'; kill -TERM $PID; wait $PID; sleep 1
   ps -eo pid,ppid,comm,args | grep -E 'agy|antigravity'
   ```
   *Check:* Exact `comm` and `args` strings (for `process_names()` and `src/commands/stop.rs`'s `pgrep -f`), and whether `SIGTERM` on the parent cleanly tears down the spawned language server.

6. **Sandbox & git worktree / `gh` compatibility:**
   ```bash
   # From a git worktree whose gitdir lives outside the checkout (like ~/.gru/work/.../checkout):
   agy --output-format stream-json --dangerously-skip-permissions --print='Run `git status` and `gh auth status`'
   ```
   *Check:* Verify `--dangerously-skip-permissions` (without `--sandbox`) permits writes to an external bare-repo gitdir and outbound `gh` network calls, even if `~/.gemini/antigravity-cli/settings.json` has sandbox enabled.

7. **Slash command expansion & `--settings` system prompt check:**
   - Run `agy --output-format stream-json --disable-slash-commands --print='/do 42'` vs without `--disable-slash-commands` to verify whether `/do` / `/pr_review` fallback strings or issue bodies with `/foo` trigger unintended slash-command expansion.
   - Check `https://antigravity.google/docs/cli/reference.md` for whether `--settings` supports any system-prompt key (could not fetch inside sandbox).

8. **Model list format:**
   ```bash
   agy models
   ```
   *Check:* Exact model ID format to document in `docs/config.example.toml` and `docs/AGENTS.md`.

---

## 4. Session Handling (`conversation_id` Capture, Persistence, and Resume)

### Why Antigravity Differs from Claude and Pi

- Gru currently generates `let session_id = Uuid::new_v4()` **before** spawning the agent in `src/commands/fix/worktree.rs:74,103`, `src/commands/prompt.rs:902,685`, `src/commands/review.rs:143,167`, and `src/commands/rebase.rs:822`.
- `agy` ignores pre-assigned IDs on fresh sessions and assigns its own `conversation_id` in the `init` NDJSON event, then accepts `--conversation <id>` on resume.

### Recommended Architecture

1. **Capture in `AntigravityBackend`:**
   - Store `captured_session_id: Mutex<Option<String>>` on `AntigravityBackend`.
   - When `parse_events()` parses `{"event":"init","conversation_id":"<uuid>",...}`, store the UUID in `captured_session_id`.
   - Add **defaulted** methods to `AgentBackend` (`src/agent.rs`) so existing backends need no edits:
     - `fn captured_session_id(&self) -> Option<String> { None }`
     - `fn reset_session(&self) {}` (called alongside `backend.reset_usage()` in `src/agent_runner.rs:222`)
     - `fn assigns_session_id(&self) -> bool { false }`

2. **Propagate immediately during streaming (not only on exit):**
   - Extend `run_agent_with_stream_monitoring` (`src/agent_runner.rs:198-205`) with an optional `on_session_id: Option<Box<dyn FnOnce(String) + Send>>` callback (or group callbacks into a small `StreamCallbacks` struct to keep the signature clean) — mirroring `on_spawn: Option<Box<dyn FnOnce(u32) + Send>>`.
   - After `backend.parse_events(trimmed)` in the stream loop (`src/agent_runner.rs:340-355`), if `on_session_id.is_some()` and `let Some(sid) = backend.captured_session_id()`, take and invoke the callback immediately so it fires once.
   - **Why immediate persistence matters:** If the worker is killed (`SIGKILL`), times out, or hits stuck-detection mid-run, the real `conversation_id` is already saved in `~/.gru/state/minions.json`, so `gru resume` / `gru attach` will still find it.

3. **Update `MinionRegistry` (`src/minion_registry.rs`):**
   - Add a helper `MinionRegistry::session_id_callback(minion_id: String)` mirroring `MinionRegistry::pid_callback`, updating `info.session_id = captured_id` and setting `info.session_established = Some(true)` in `~/.gru/state/minions.json`.
   - **Pre-`init` crash guard:** In `src/commands/fix/worker.rs:39-43`, `OrchestrationPhase::RunningAgent` is written *before* the agent starts, and on resume `use_resume = *start_phase > OrchestrationPhase::Setup`. If `agy` crashes before emitting `init` (auth failure, bad model), `info.session_id` still holds Gru's synthetic UUID, which `agy --conversation <uuid>` will reject.
     - Add `#[serde(default)] pub(crate) session_established: Option<bool>` to `MinionInfo` (`src/minion_registry.rs:591-679`): initialize to `None` for legacy/pre-assigned backends (treated as established for backwards compatibility) and `Some(false)` when `backend.assigns_session_id()` is true, flipping to `Some(true)` inside `session_id_callback`.
     - In `src/commands/fix/worker.rs:43`, set `let use_resume = *start_phase > OrchestrationPhase::Setup && wt_ctx.session_established != Some(false);` so a pre-`init` crash cleanly retries with a fresh `build_command` instead of passing a bogus UUID to `--conversation`.

4. **Update in-memory `WorktreeContext.session_id` for in-process review responses:**
   - **Easy-to-miss trap:** In `gru do`, `src/commands/fix/mod.rs:202-212` constructs `wt_ctx` once before `run_agent_phase`, and later passes the same `&wt_ctx.session_id` into `monitor_pr_phase` -> `invoke_agent_for_reviews` (`src/commands/fix/monitor.rs:1146,1243`).
   - If `wt_ctx.session_id` is not updated in memory when `init` arrives, in-process PR review responses during the same `gru do` run will pass the stale pre-generated UUID to `agy --conversation`.
   - **Fix:** Either return `captured_session_id: Option<Uuid>` on `AgentResult` (`src/commands/fix/types.rs:72-74`) and mutate `wt_ctx.session_id` in `src/commands/fix/mod.rs:225` and `src/commands/resume.rs:329`, or pass `&mut WorktreeContext` into `run_agent_phase`.

5. **Impact on `src/commands/resume.rs`, `src/commands/attach.rs`, and `src/commands/prompt.rs`:**
   - Provided `agy`'s `conversation_id` is always a valid UUID (verify in Section 3 #4), `Uuid::parse_str(&info.session_id)` in `src/commands/resume.rs:184`, `src/commands/attach.rs:158`, and `src/commands/fix/mod.rs:202,485` works unchanged.
   - `src/commands/resume.rs` only needs to propagate `info.session_established` into `WorktreeContext` (for the pre-`init` crash guard) and accept updated `session_id` from `run_agent_phase` if a pre-`init` retry establishes one.
   - `src/commands/prompt.rs:807` should also wire `on_session_id` because `src/commands/prompt.rs:746` advertises `gru resume <minion_id>`.
   - *(Contingency: if `agy` ever emits non-UUID conversation IDs, `session_id` in `WorktreeContext` and `AgentBackend` would need widening from `&Uuid` to `&str`.)*

---

## 5. Configuration (`[agent.antigravity]`)

### Struct & Validation (`src/config.rs`)

Follow the `[agent.pi]` pattern (`src/config.rs:110-133`):

- Add constant:
  ```rust
  pub(crate) const ANTIGRAVITY_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
  ```
- Add struct `AntigravityAgentConfig` with `#[derive(Debug, Clone, Serialize, Deserialize, Default)]`:
  - `binary: Option<String>` — CLI binary override (defaults to `"agy"` on `$PATH`).
  - `model: Option<String>` — passed via `--model <model>` (`None` lets `agy` use its own default from `~/.gemini/antigravity-cli/settings.json`).
  - `effort: Option<String>` — passed via `--effort <level>` (`None` lets `agy` use its own default).
  - *Defer for V1:* Do not add `mode` (headless autonomous runs use `--dangerously-skip-permissions`) or `print_timeout` (Gru's `--timeout` + `AgentRunner` already manage wall-clock limits; keep `agy`'s default `0s`) unless probing reveals a concrete need.
- Add `#[serde(default)] pub(crate) antigravity: AntigravityAgentConfig` to `AgentConfig` (`src/config.rs:57-84`).
- Add validation in `LabConfig::validate_agent()` (`src/config.rs:1008-1109`):
  - Reject empty/whitespace `agent.antigravity.binary` (error message names `"agy"` as default).
  - Reject empty/whitespace `agent.antigravity.model`.
  - Reject `agent.antigravity.effort` not in `ANTIGRAVITY_EFFORT_LEVELS`.
- Update `LabConfig::default_config_toml()` (`src/config.rs:752-780`) with a commented `# [agent.antigravity]` block.

### Binary-Resolution Helper & `spawn_error_context` Fix

- In `src/agent_registry.rs` (after line 144), add:
  ```rust
  pub(crate) fn configured_antigravity_binary() -> String {
      crate::config::try_load_config()
          .and_then(|c| c.agent.antigravity.binary)
          .unwrap_or_else(|| "agy".to_string())
  }
  ```
- **Fix `spawn_error_context` (`src/agent.rs:210-227`):** Currently derives the `[agent.<key>]` config section from `backend.process_names()[0]`. Because Antigravity's binary is `agy` while its config section is `[agent.antigravity]`, add a defaulted trait method:
  ```rust
  fn config_key(&self) -> &str {
      self.process_names().first().copied().unwrap_or(self.name())
  }
  ```
  Override `config_key(&self) -> &str { "antigravity" }` on `AntigravityBackend` and have `spawn_error_context` call `backend.config_key()`. That decouples process matching (`&["agy", "antigravity"]`) from TOML section naming.

### `docs/config.example.toml` Update

- Update line 103 comment to list `"antigravity"` among options (`"claude", "codex", "pi", "antigravity"`).
- Add a commented `# [agent.antigravity]` section after `[agent.codex]` (`docs/config.example.toml:140-148`) documenting `binary`, `model` (noting `agy models`), and `effort` (`low|medium|high|xhigh|max`).
- Note: `config_example_parses()` (`src/config.rs:2692-2759`) runs `serde_ignored` over the uncommented example, so every key in `docs/config.example.toml` must match `AntigravityAgentConfig` exactly.

---

## 6. Interactive Mode (`gru chat`/`pm`/`tpm` vs `gru attach`)

Gru separates interactive mode into **two distinct trait methods**, and Antigravity's capabilities differ across them:

1. **`gru chat` / `gru pm` / `gru tpm` (`build_interactive_command`, `src/agent.rs:336-342`):**
   - **Recommendation: Return `None`** (same as Codex), causing `src/commands/chat.rs:60` and `src/commands/pm.rs:69` to return `interactive_unsupported_error()`.
   - **Rationale:** Verified from `agy --help` (v1.5.18) that `agy` has **no `--system-prompt` flag**. Passing the role/project prompt via `-i`/`--prompt-interactive` is not equivalent — it submits a user turn immediately on startup (breaking `gru chat` when `initial_prompt` is `None`) and does not establish a system prompt. Revisit only if Unknown #7 finds a `--settings` system-prompt key or `agy` adds `--system-prompt`.

2. **`gru attach` (`build_interactive_resume_command`, `src/agent.rs:306-311`):**
   - **Recommendation: Return `None` in the initial MVP PR; implement `Some(...)` in the session-resume PR** once `conversation_id` capture is verified.
   - **Command shape when enabled:**
     ```text
     agy --conversation <conversation_id> [--model <model>] [--effort <effort>]
     ```
     with inherited stdio, `current_dir(worktree_path)`, `GH_HOST`, and `env_remove` of `GRU_RETRY_PARENT` / `GRU_CONFIG_PATH`.
   - **`yolo_args()`:** Return `vec!["--dangerously-skip-permissions"]` (verified in `agy --help`) so `gru attach --yolo` works seamlessly.
   - Note: In `src/commands/attach.rs:156-183`, if `session_established == Some(false)` (pre-`init` crash), `attach` should fail with a clear message rather than launching `agy --conversation <synthetic-uuid>`.

---

## 7. Registry, CLI, and Documentation Edits Needed

| File | Required Changes |
|---|---|
| `src/antigravity_backend.rs` *(new)* | Implement `AntigravityBackend` (`AgentBackend`), command builders (using `--print=<prompt>` for safe flag parsing), NDJSON event parser, tool summary formatter, usage accumulation + `reset_usage`/`final_usage`, session-ID capture. |
| `src/main.rs` | Add `mod antigravity_backend;` (near line 3). Update `--agent` help strings on `Do` (L158), `Review` (L260), and `Prompt` (L417) to include `antigravity`. Update `Chat` (L91), `Pm` (L105), `Tpm` (L119) help text from `"Codex has no interactive mode."` to e.g. `"Only claude and pi support interactive sessions."` |
| `src/agent.rs` | Add defaulted `config_key()` (used by `spawn_error_context` at L223). In session-capture PR, add defaulted `captured_session_id()`, `reset_session()`, and `assigns_session_id()`. |
| `src/agent_registry.rs` | Import `AntigravityBackend`. Add `"antigravity"` to `AVAILABLE_AGENTS` (L12). Extend `AgentOverrides` (L22-31), `construct_backend()` (L39-57), and `resolve_backend()` (L172-188 — load config for all backends). Add `configured_antigravity_binary()`. |
| `src/config.rs` | Add `ANTIGRAVITY_EFFORT_LEVELS`, `AntigravityAgentConfig`, `AgentConfig.antigravity`, `validate_agent()` checks, and `default_config_toml()` section. |
| `src/commands/stop.rs` | Add `&crate::agent_registry::configured_antigravity_binary()` to `agent_process_match_names()` (L177-183). Update hardcoded `vec!["claude", "codex", "pi", "gru"]` assertions in tests (L329-398). |
| `src/commands/init.rs` | Check `configured_antigravity_binary()` in `check_prerequisites()` (L143-166), print `✓ <bin> (Antigravity CLI)`, and include it in the "No agent backend found" warning. |
| `src/commands/fix/agent.rs` | Add verified `agy` edit/command tool names to `is_edit_tool()` (L394-399) and `is_command_tool()` (L403-405) (unless normalized in `antigravity_backend.rs`). |
| `docs/config.example.toml` | Add `"antigravity"` to `[agent] default` comment and add `[agent.antigravity]` section (required by `CLAUDE.md:239` & CI test). |
| `docs/AGENTS.md` | Add Antigravity to Available Backends table, full Antigravity setup/usage/flags section, and Feature Comparison table. |
| `README.md` | Add Antigravity to intro (L13), Prerequisites (L137-140), Multi-Agent Support examples (L222-246), and Related Projects. |
| `CLAUDE.md` | Add `src/antigravity_backend.rs` to Module Structure (L110-116) and Multi-Agent Architecture (L188). |
| `docs/introduction.md`, `docs/GETTING_STARTED.md` | Update backend list at `docs/introduction.md:12` (and optionally `docs/GETTING_STARTED.md:132`). |

---

## 8. Test Plan

### Stream Parser & Command Builder Tests (`src/antigravity_backend.rs`)

Mirror `src/pi_backend.rs:652-1631` and `src/codex_backend.rs:504-1237`:

- **Metadata:** `name() == "antigravity"`, `config_key() == "antigravity"`, `process_names()` contains `"agy"` (and `"antigravity"` if confirmed), `install_url()`, `yolo_args()`.
- **Command builders:**
  - `build_command`: verifies binary, `--output-format stream-json`, `--dangerously-skip-permissions`, `--print=<prompt>` (including a prompt starting with `-` / `---`), `--model` and `--effort` present when configured and omitted when `None`, `GH_HOST` (both `github.com` and GHE), and `env_remove` for `GRU_RETRY_PARENT` and `GRU_CONFIG_PATH`.
  - `build_ci_fix_command`: verifies statelessness (no `--conversation` flag) and stream-json flags.
  - `build_oneshot_command`: verifies plain-text output format and the `"-"` stdin sentinel behavior (verified against Unknown #2).
  - `build_resume_command`: verifies `--conversation <uuid>` + `--output-format stream-json` + `--print=<prompt>`.
  - `build_interactive_resume_command`: verifies `--conversation <uuid>` with no `-p` / `--output-format`.
  - `build_interactive_command`: verifies `None`.
- **Recorded NDJSON fixture tests** (using real captured lines from Section 3):
  - `init` -> `Started { usage: None }` and `captured_session_id() == Some(<uuid>)`.
  - `step_update` (`user_input`) -> empty vec.
  - `step_update` (`agent_response` `ACTIVE` deltas + `DONE`) -> `TextDelta` per chunk, no duplicated text on `DONE`, `MessageComplete` on `DONE`.
  - `step_update` (tool calls: read/edit/write/command success & failure) -> single `ToolUse` with formatted `input_summary` (no duplicate on repeated `ACTIVE` updates for same `step_index`) and `ToolResult { is_error }`.
  - `step_update` (thinking/reasoning, if present) -> `Thinking`.
  - `result` (`SUCCESS`) -> `Finished { usage: Some(...) }`; non-success -> `Error` (+ `Finished` if usage present).
  - Defensive parsing: malformed `usage` object does not drop `MessageComplete` / `Finished`; blank lines, non-JSON lines, and unknown `event`/`step_type` values return empty vec.
- **Usage accounting & lifecycle:**
  - Full pipeline test feeding recorded multi-step NDJSON through `parse_events()` -> `agent_runner::accumulate_token_usage()`, asserting exact totals with **zero double-counting**.
  - `reset_usage()` / `reset_session()` clears prior totals and captured `conversation_id` across reused backend instances (including when a second invocation crashes before emitting `init`).
  - `final_usage()` recovers accumulated step usage when the stream hits EOF without a `result` event.

### Registry, Config, Runner & Integration Unit Tests

- **`src/agent_registry.rs`:** `test_resolve_antigravity`, update `test_resolve_unknown_fails` (L228), `test_resolve_backend_forwards_configured_antigravity_settings` (binary + model + effort), `configured_antigravity_binary()` fallback & override.
- **`src/config.rs`:** `[agent.antigravity]` absent defaults to `None`, valid section parses, empty `binary`/`model` rejected, invalid `effort` rejected, `[agent] default = "antigravity"` accepted, and `config_example_parses()` passes.
- **`src/agent.rs`:** `spawn_error_context` on `AntigravityBackend` cites `[agent.antigravity] binary` (not `[agent.agy]`) and includes `install_url`.
- **`src/commands/stop.rs`:** update default process-name tests to include `agy` (+ custom `[agent.antigravity] binary` basename test).
- **`src/commands/fix/agent.rs`:** `is_edit_tool` / `is_command_tool` / `detect_phase_transition` tests for `agy`'s tool names.
- **`src/agent_runner.rs`:** async test with a stub backend emitting a captured session ID on `init`, verifying `on_session_id` fires once during streaming and persists even if the process exits non-zero.

---

## 9. Risks & Mitigations

| Risk | Details | Mitigation |
|---|---|---|
| **Stuck detection on silent runs** | `src/agent_runner.rs` enforces `STREAM_TIMEOUT_SECS = 300` (5m per stdout line) (L19, L310-320). If `agy` executes a long shell command (e.g., `cargo test`) or high-effort reasoning (`--effort xhigh\|max`) without emitting periodic `step_update` lines for >5m, Gru kills the process with `StreamTimeout` and marks the issue `gru:blocked`. | Verify `agy`'s heartbeat / `step_update` cadence during a long `sleep` / compile command in Section 3. If `agy` is silent during long tool calls, document the caveat in `docs/AGENTS.md` (and consider a backend-configurable or raised stream timeout in a follow-up if needed). Keep `--print-timeout` at `0s` so `agy` doesn't race Gru's `--timeout`. |
| **Token usage double-counting & `thinking_tokens`** | `accumulate_token_usage` (`src/agent_runner.rs:401-439`) adds `MessageComplete.usage.output_tokens` AND all fields of `Finished.usage`. Also `agy` reports `thinking_tokens` and `total_tokens`, while `TokenUsage` only has `input_tokens`, `output_tokens`, `cache_*`, `cost`. | Verify from captured multi-step samples whether `result.usage` is cumulative and whether `thinking_tokens` is a subset of `output_tokens`. Emit `MessageComplete { usage: None }` and surface totals once in `Finished { usage: Some(...) }`, reserving per-step accumulation solely for `final_usage()` EOF fallback. |
| **Sandbox & git worktree writes** | Like Codex (`src/codex_backend.rs:182-190`), a Gru worktree's `.git` gitdir lives in `~/.gru/repos/owner/repo.git` (outside `checkout/`), and Minions must call `gh` over the network. Passing `--sandbox` (or inheriting sandbox enabled in `~/.gemini/antigravity-cli/settings.json`) will break commits and `gh`. | Never pass `--sandbox`; always pass `--dangerously-skip-permissions`. Verify in Section 3 #6 whether `--settings` is needed to explicitly disable sandbox if a user enabled it globally. |
| **Shared `~/.gemini/antigravity-cli/` state & language server concurrency** | Observed during probing: `agy` reads/writes `~/.gemini/antigravity-cli/` (`log/`, `installation_id`, `crashes/`) and spawns a language server process per CLI invocation. Under `gru lab` with `max_slots > 1`, concurrent `agy` processes may contend on state files or leak language-server processes on `SIGKILL`. | Test signal cleanup in Section 3 #5; note concurrency behavior in `docs/AGENTS.md` if any contention or orphan processes appear. |
| **`-p` flag parsing footgun** | Verified that `-p` consumes the next argv token even if it looks like a flag (`-p --model ...` fails with exit 2). Prompts starting with `-` (e.g., `---`) can trigger flag-parsing heuristics. | Always pass `--print=<prompt>` (attached with `=`) and place all other flags before it. |
| **Schema drift in `agy` NDJSON** | `agy` is actively evolving (v1.5.18). Strict deserialization of nested objects could drop terminal events (`DONE`, `result`). | Mirror `PiBackend`'s defensive pattern (`src/pi_backend.rs:363-368, 458-467`): deserialize optional nested `usage` / tool args via `serde_json::Value` and ignore unknown fields. |
| **`spawn_error_context` config-key mismatch** | `src/agent.rs:223-227` assumes `process_names()[0]` equals the TOML section name; `agy` != `antigravity`. | Add defaulted `AgentBackend::config_key()` before registering `AntigravityBackend`. |

---

## 10. Step-by-Step PR Plan

1. **PR 0 — Protocol & CLI Capture Spike *(no repo code; prerequisite for PR 2+)*:**
   - Run the commands in Section 3 against live `agy`, record sanitized NDJSON fixtures, and settle the 8 unknown groups (especially tool-call schemas, usage semantics, stdin oneshot behavior, and `--conversation` behavior).
2. **PR 1 — Config Schema & Helpers *(can ship independently)*:**
   - Add `ANTIGRAVITY_EFFORT_LEVELS`, `AntigravityAgentConfig`, `AgentConfig.antigravity`, validation, `default_config_toml()`, `docs/config.example.toml` update, `configured_antigravity_binary()` in `src/agent_registry.rs`, and unit tests (`config.rs`, `config_example_parses`).
   - *Independent because:* adding an unused optional TOML table + helper does not register `"antigravity"` in `AVAILABLE_AGENTS` yet and breaks nothing.
3. **PR 2 — `AgentBackend::config_key()` & `src/antigravity_backend.rs` MVP *(depends on PR 0)*:**
   - Add defaulted `config_key()` to `AgentBackend` and use it in `spawn_error_context` (`src/agent.rs:223`).
   - Implement `src/antigravity_backend.rs` with command builders (`build_command`, `build_oneshot_command`, `build_ci_fix_command`), verified NDJSON parser, tool summaries, `TokenUsage` accumulation, `reset_usage`/`final_usage`, and fixture-backed unit tests. Return `None` from `build_resume_command`, `build_interactive_resume_command`, and `build_interactive_command` in this PR.
4. **PR 3 — Registration, CLI, `stop`/`init`, Phase Detection & Docs *(depends on PR 1 + PR 2; first end-to-end shippable milestone)*:**
   - Register `"antigravity"` in `src/agent_registry.rs` (`AVAILABLE_AGENTS`, `AgentOverrides`, `construct_backend`, `resolve_backend`) + registry tests.
   - Add `mod antigravity_backend;` and update `--agent` help strings in `src/main.rs`.
   - Wire `configured_antigravity_binary()` into `src/commands/stop.rs` and `src/commands/init.rs` (+ tests).
   - Add `agy` edit/command tool names to `is_edit_tool`/`is_command_tool` in `src/commands/fix/agent.rs` (+ tests).
   - Update `docs/AGENTS.md`, `README.md`, `CLAUDE.md`, `docs/introduction.md`, and `docs/GETTING_STARTED.md`.
   - *Milestone:* `gru do`, `gru review`, `gru prompt`, `gru lab`, CI auto-fix, and merge judge all work with `--agent antigravity`.
5. **PR 4 — Backend-Assigned Session ID Capture Infrastructure *(can ship independently of PR 1-3)*:**
   - Add defaulted `captured_session_id()`, `reset_session()`, and `assigns_session_id()` to `AgentBackend` (`src/agent.rs`).
   - Wire `on_session_id` callback into `run_agent_with_stream_monitoring` (`src/agent_runner.rs`).
   - Add `#[serde(default)] session_established: Option<bool>` and `MinionRegistry::session_id_callback` in `src/minion_registry.rs`.
   - Persist captured session ID on `init` in `src/commands/fix/agent.rs` and `src/commands/prompt.rs`, refresh in-memory `WorktreeContext.session_id` (so `monitor_pr_phase` review responses use the real ID), and guard pre-`init` resume in `src/commands/fix/worker.rs` + `src/commands/resume.rs`.
   - Add runner/registry unit tests.
6. **PR 5 — Antigravity Resume & Interactive Attach *(depends on PR 3 + PR 4)*:**
   - Implement `captured_session_id()` / `reset_session()` / `assigns_session_id() -> true`, `build_resume_command` (`--conversation <id>` + `--print=<prompt>`), `build_interactive_resume_command` (`--conversation <id>`), and `yolo_args()` (`["--dangerously-skip-permissions"]`) in `src/antigravity_backend.rs`.
   - Guard `src/commands/attach.rs` against `session_established == Some(false)`.
   - Add unit tests and update `docs/AGENTS.md` Feature Comparison table.
7. **PR 6 *(Optional / Deferred)* — Interactive `gru chat`/`pm`/`tpm`:**
   - Only if `agy` exposes a system-prompt mechanism (`--system-prompt` or `--settings` key): implement `build_interactive_command`, update `interactive_unsupported_error()` (`src/agent_registry.rs:152-159`), and update `src/main.rs` help text for `chat`/`pm`/`tpm`.

---

## Unverified Items

- Live `agy` NDJSON schemas for tool calls/results, thinking steps, errors, timeouts, rate limits, `result.usage` vs `step_update.usage`, and `thinking_tokens` (could not run live LLM calls inside the sandbox due to restricted `~/.gemini/antigravity-cli/` and network access).
- Exact stdin mechanism for `agy` print mode (`-p -` vs `-p ""` vs `--input-format stream-json`).
- Whether `agy` `--settings` supports a system-prompt key (`https://antigravity.google/docs/cli/reference.md` could not be fetched).
- Runtime process `comm`/`args` and signal cleanup of `agy`'s spawned language server (`ps` was blocked by sandbox permissions).
