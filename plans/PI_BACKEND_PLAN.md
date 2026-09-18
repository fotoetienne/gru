# Pi Agent Backend Integration Plan

**Status:** In progress — #910/#911/#912 merged; #913 (the Pi backend) rebased and CI-green, awaiting merge
**Date:** 2026-09-16, revised 2026-09-18
**Relates to:** `plans/MULTI_AGENT_PRD.md` (Phase 3), `docs/DECISIONS.md`, epic #281, planning issue #352

## Summary

Add Netflix Pi (`pi`, npm `@netflix-internal/pi-agent`) as a third `AgentBackend`
alongside Claude Code and Codex, so Minions can run `gru do 42 --agent pi`.

Pi is a good fit for Netflix-internal work: it ships authenticated against internal
model providers (no separate API key to manage), reads `AGENTS.md` and `CLAUDE.md`
for project context, and — unlike Codex — supports interactive session resume, which
`gru attach` needs. Note that provider *authentication* being solved does not settle
provider *selection*: see work item 3.

## Decision: direct integration, not ACP

Pi does not speak the Agent Client Protocol. Verified against the installed bundle
(`@netflix-internal/pi-agent` 0.88.1): no `agent-client-protocol` dependency and no
ACP strings anywhere in the package. Its `--mode rpc` is Pi's own JSON-RPC surface,
unrelated to ACP.

That single fact is decisive: adopting ACP for Pi would mean writing and hosting an
adapter sidecar (the way Zed's `claude-code-acp` bridges Claude Code), which is
strictly more work than a native backend, not less.

The secondary argument is **client-assigned session IDs**. ACP session IDs are
assigned by the agent, whereas Gru generates a UUID up front and stores it in
`MinionInfo.session_id` to drive resume and attach. Pi's `--session-id` accepts Gru's
UUID directly, so the native path preserves Gru's model; ACP would require a mapping
layer. Stuck detection and token accounting are likewise unspecified by ACP.

Two arguments *not* worth making, for the record: ACP's `session/new` does take a
`cwd`, so worktree isolation is not an ACP gap, and `GH_HOST` injection is a property
of process spawning, orthogonal to the protocol. ACP's human-in-the-loop permission
model is also not a blocker — a headless client simply auto-approves.

`docs/DECISIONS.md:21` already scores ACP integration at 0.706 / "Future (V2+)", which
remains the right call and needs no change. `plans/MULTI_AGENT_PRD.md` does need
correcting (#919): it claims Claude Code, Codex, and Gemini "already support" ACP, but
Claude Code and Codex both rely on external adapters (`claude-code-acp`, `codex-acp`) —
Gemini CLI is the only one of the three with native ACP support.

A local `experiments/acp_integration_results.md` (2025-12-02) recommends the opposite
("proceed with ACP integration for V1"), but `experiments/` is gitignored by design
(`.gitignore:10`), so it is not repo state and is out of scope. An earlier claim in this
plan that the repo held two live opposite recommendations was wrong on that point.

ACP stays worth revisiting if Gru should be driven *by* editors, or should host
arbitrary third-party agents without new Rust per agent — as a single `AcpBackend`
transport under the existing trait, exactly as the PRD planned.

## Verified Pi capabilities

Probed against `/opt/nflx/bin/pi` (newt shim) on 2026-09-16:

| Gru requirement | Pi invocation | Notes |
|---|---|---|
| Headless streaming | `pi -p --mode json` | JSONL, one event per line on stdout |
| Session identity | `--session-id <uuid>` | Accepts Gru's UUID verbatim, creates if missing |
| Resume | same `--session-id` plus new prompt | Verified: codeword stored, recalled by a second process |
| Interactive resume | drop `-p`, keep `--session-id` | TUI with history — powers `gru attach` |
| Autonomous tool use | default under `-p` | `bash` and `edit` ran with no approval prompt, **in an already-trusted directory** — see work item 4 |
| Prompt on stdin | `pi -p` with piped stdin and no prompt arg | `pi -p -` emits nothing, so the `"-"` sentinel must mean "omit arg, pipe stdin" |
| Usage reporting | `input` / `output` / `cacheRead` / `cacheWrite` / `cost` | Richer than `TokenUsage`, but see work item 2 — most of it is dropped today |

Sessions are stored per working directory at
`~/.pi/agent/sessions/--<mangled-cwd>--/<timestamp>_<uuid>.jsonl`. Minion worktree
paths are stable, so resume works. A moved or recreated worktree loses history
*silently*: because `--session-id` creates the session when missing, `gru attach`
lands in a blank session rather than erroring the way Claude's `--resume <id>` does.

### Event mapping to `AgentEvent`

| Pi event | `AgentEvent` |
|---|---|
| `session`, `agent_start` | `Started` |
| `turn_start` | `Thinking` |
| `message_update` with `assistantMessageEvent.type == "text_delta"` | `TextDelta` |
| `tool_execution_start` (carries full `args`) | `ToolUse` with populated `input_summary` |
| `tool_execution_end` (`result.content[].text`, `isError`) | `ToolResult` |
| `turn_end` (carries `usage`) | `MessageComplete` |
| `agent_end` | `Finished` |
| `turn_failed`, any `error` | `Error` |

Ignored: `message_start`, `message_end`, `entry_appended`, `agent_settled`,
`tool_execution_update`, `toolcall_delta` (argument fragments — `tool_execution_start`
already carries the complete args).

The newt shim prints exactly two non-JSON lines to **stdout** ahead of the stream
(`Using existing agent-beach…`, `Using existing Netflix Pi distribution package…`);
the rest of its `INFO --- newt runtime context ---` block goes to stderr.
`parse_events` skips unrecognized lines by contract, so the streaming paths are fine.

## Work breakdown

Item 1 is implemented; the rest are open. Each maps to a GitHub issue.

1. **`PiBackend`** (#904, PR #913) — `src/pi_backend.rs` implementing all six
   `AgentBackend` methods, modelled on `src/codex_backend.rs`. Registration has *three*
   touch points in `src/agent_registry.rs`, not one: `AVAILABLE_AGENTS`, the
   `resolve_backend` match, and the exact-error-string assertion at line 56.

2. **Token usage is silently zeroed, and cost is discarded.** `accumulate_token_usage`
   (`src/agent_runner.rs:375`) takes only `output_tokens` from `MessageComplete`;
   input and cache fields are read from `Started` and `Finished` only. PR #913 maps all
   of Pi's usage onto `turn_end` → `MessageComplete` and emits
   `Finished { usage: None }`, so every Pi Minion will report `0 in / N out` in
   `gru status` with no error. Codex has the same defect. Fix on either side — emit
   session totals in `Finished`, or teach `accumulate_token_usage` to take input and
   cache from `MessageComplete`. Tracked in #914.

   Pi also reports dollar cost in the same `usage` object (per-category plus a total),
   which `TokenUsage` has no field for. Cost is **in scope** for this work, not deferred:
   since #915 pins a provider explicitly, cost is the signal that makes that choice's
   consequences visible. Tracked in #921, coordinated with #620 for display and with
   #914 because cost arrives through the same dropped path. The field must be optional —
   Claude and Codex report no cost, and `None` must mean "not reported" rather than zero.

3. **Model selection stays with Pi** (#915). `pi --help` documents `--provider` as
   defaulting to `google`, but that is not what a Netflix install does: the
   `netflix-provider` plugin auto-discovers Netflix models, and a probe run resolved to
   `nflx-openai/gpt-5.6-sol`. `pi --list-models` also exposes `nflx-anthropic/claude-opus-5`
   and the sonnet-5 line, so running Pi on Claude models is possible.

   Decision: do **not** pin a model. Pi users have already configured Pi, and overriding
   that would surprise them. Gru passes `--model`/`--thinking` only when `[agent.pi]` sets
   them. The cost of not pinning is that the model becomes invisible, so the backend must
   record the `provider` and `model` that Pi reports in `message_start`/`turn_end` — that
   buys reproducibility without taking the choice away.

4. **Trust posture** (#916). Pi discovers project-local extensions, skills, prompt
   templates, and themes, and has `-a/--approve` / `--no-approve` for trusting
   project-local files. Every Minion runs in a brand-new worktree — trust-on-first-use.

   A probe settled the operational half: in a fresh `git init` directory,
   `pi -p --mode json` ran to completion (exit 0, full stream through `agent_end`) with
   **no trust prompt**, so there is no hang-to-stuck-detector risk. It did read and obey
   a project-local `AGENTS.md`, but Claude Code does the same with `CLAUDE.md`, so that
   is existing Gru design rather than a Pi delta. Since `--approve` *grants* trust, the
   default already withholds it; pass `--no-approve` explicitly so a future change to
   Pi's default cannot silently start executing repo-supplied extensions.

5. **`GRU_RETRY_PARENT` env leak.** `claude_runner.rs:37,66` and
   `claude_backend.rs:224,256` call `.env_remove(GRU_RETRY_PARENT_ENV)` on every
   command; Codex and PR #913's Pi backend do not. Nested `gru` calls made from the
   agent's own `bash` tool will therefore defer `gru:failed` labeling.

6. **`config.agent.default` is dead config, so `gru lab` can only ever run Claude.**
   (#918. Note the scope: make a *user-configured* default effective. Claude remains
   `DEFAULT_AGENT` when nothing is configured — see Decisions below.)
   Two compounding facts: `grep -rn "agent\.default" src/` returns nothing outside
   `config.rs`, because `main.rs:480,514,555` fall back to `agent_registry::DEFAULT_AGENT`
   unconditionally; and `lab.rs` `spawn_minion` builds `gru do <issue_ref>` with no
   `--agent` at all. `[agent] default = "pi"` parses, validates (`validate_agent` does
   not even check the name resolves), and does nothing — while README.md and
   `docs/AGENTS.md` document it as the supported way to switch backends. Pi is reachable
   only via an explicit `gru do --agent pi` until this is wired up. `[agent.claude] binary`
   is dead in the same way.

7. **One-shot stdout filtering** (#908) — hardening, not a live bug. `build_oneshot_command`
   has exactly one consumer (`merge_judge.rs:825`), and `extract_json` already tolerates
   arbitrary leading prose via `find('{')`..`rfind('}')`; neither preamble line contains a
   brace. Worth doing because the shim wording is unstable, but it ranks below items 2–6.

8. **Config and docs** (#909) — `[agent.pi]` section, `docs/config.example.toml`, README,
   `CLAUDE.md` (both the module list at 111-114 and "Built-in backends" at 187), and
   `gru init --doctor`, which hardcodes "No agent backend found (claude or codex)"
   (`init.rs:133-148`) and so warns falsely on a Pi-only machine. `docs/AGENTS.md` needs
   more than a table row: its "Adding a New Backend" checklist (line 129) lists four trait
   methods where the trait has six, and #905/#907 each add another.

## Decisions

- **Model selection is left to Pi's own config** (#915). Gru overrides only when
  `[agent.pi]` sets a model, and records what Pi actually used.
- **#914 must be fixed in the backends**, by emitting session totals in `Finished`.
  Teaching `accumulate_token_usage` to read input from `MessageComplete` is explicitly
  rejected: Claude reports input in `Started` *and* emits `MessageComplete` with usage,
  so that path risks silently double-counting Claude.
- **`[agent.<name>] binary` will be honored**, not removed (#922). It matters most for Pi,
  whose newt shim path varies by host. Split out of #918 so PR #920 can land as-is.
- **`plans/MULTI_AGENT_PRD.md` is now tracked.** It was untracked while `CLAUDE.md`, #352,
  and this plan all cited it, so a minion working #919 correctly refused to edit a file it
  could not see.
- **Claude stays the default backend.** `agent_registry::DEFAULT_AGENT` remains `"claude"`.
  Pi is opt-in via `gru do --agent pi` or an explicit `[agent] default = "pi"` once #918
  makes that setting effective. Revisit only after Pi has run real Minions end to end.
- **Cost tracking is in scope** (#921), not deferred to #620 as originally planned.
- **#913 merges forward.** The Pi backend lands with the zero-token (#914) and unpinned-model
  (#915) defects known and tracked, rather than blocking the merge on them. Repo owner
  handles merge sequencing.

## Sequencing

`AgentBackend` is being mutated by concurrent PRs, and new trait methods without default
bodies break any backend that predates them:

- #911 (for #905) adds `yolo_args()` **with** a default body — safe in any order.
- #912 (for #907) adds `process_names()` **without** a default body, plus a second
  hardcoded backend list in `agent_registry::all_process_names()`. Omitting Pi there
  compiles clean and silently reproduces the very bug #907 exists to fix.

**Resolved as planned.** #910, #911, and #912 merged in that order, and #913 was rebased
onto the result: it implements `process_names() -> &["pi"]`, correctly leaves `yolo_args()`
to the default empty implementation (Pi has no bypass flag), and — critically — adds its
arm to `construct_backend`, not just to `AVAILABLE_AGENTS`. That last point was the real
hazard: `all_process_names()` uses `filter_map`, so a name registered in `AVAILABLE_AGENTS`
without a `construct_backend` arm is dropped silently, with no compile error, and
`gru stop` quietly stops finding that backend. #919 adds a warning about it to the
contributor checklist.

Convention going forward: new trait methods carry a default body, as #911 did and #912
did not.

## Known defects in the filed issues

- #905's acceptance criterion "`gru attach --yolo` against a Codex Minion launches
  successfully" is unsatisfiable. `CodexBackend::build_interactive_resume_command`
  returns `None` (`codex_backend.rs:63`), so `attach.rs:173` bails with "does not
  support interactive mode" long before the flag is appended at `attach.rs:203`. The
  unconditional `--dangerously-skip-permissions` is real but **unreachable** for every
  non-Claude backend today; Pi is the first backend that makes it reachable.
- The plan previously described phase detection as Claude-only. `"file_change"` and
  `"command"` (the Codex equivalents) are already handled at `fix/agent.rs:213,224`.
- #907 and this plan both omit that the same pgrep fallback is reached from `gru attach`'s
  auto-stop path (`attach.rs:311`), so the blind spot also breaks attach taking over a
  running Pi Minion.
- An undocumented contract links #904 and #906: `is_test_command`
  (`fix/agent.rs:415-447`) strips an optional `"Run: "` prefix and then matches
  `starts_with` against test-runner patterns. If Pi's bash summary is not
  `Run: <command>`, the Testing phase never fires and #906 looks fixed while being
  broken for Pi.

## Non-goals

- Converting `gru chat` and `gru pm` off their hardcoded `claude` invocation
  (`chat.rs:41`, `pm.rs:49`). Human-facing utilities, not Minion execution.
- `gru rebase` resolving the Minion's real backend. `rebase.rs:821` hardcodes
  `DEFAULT_AGENT`, so a Pi Minion's conflict resolution runs under Claude — as it
  already does for Codex. Acceptable for now, stated so it is not mistaken for an oversight.

## Tracking issues

| Issue | Scope | Blocked by |
|---|---|---|
| #904 | `PiBackend` core (PR #913) | — |
| #905 | `attach --yolo` sends a Claude-only flag to every backend (PR #911) | — |
| #906 | Phase detection tool names (PR #910) | — |
| #907 | `gru stop` pgrep fallback (PR #912) | — |
| #908 | One-shot stdout filtering (hardening) | #904 |
| #909 | `[agent.pi]` config, README, `docs/AGENTS.md`, `init --doctor` | #904 |
| #914 | Input and cache tokens dropped from `MessageComplete` | #904 |
| #915 | Pin Pi's provider and model | #904 |
| #916 | Trust posture in fresh worktrees, `GRU_RETRY_PARENT` leak | #904 |
| #917 | Reconcile Pi with trait methods from #905 and #907 | #904, #905, #907 |
| #918 | `config.agent.default` is dead config (PR #920) | — |
| #922 | Honor `[agent.<name>] binary` | #904 |
| #919 | Correct ACP docs, refresh backend checklist | #905, #907 |
| #921 | Per-session cost in `TokenUsage` | #904 |

## Risk

Pi is a fast-moving internal package, so the JSON event schema may churn. The mitigation
of fixture tests plus skip-unknown-events does **not** cover a renamed field: if
`message_update.assistantMessageEvent` changes shape, the backend goes silent rather than
erroring, and the Minion trips the 15-minute stuck detector instead of failing loudly.
Fixtures should be captured from a real `pi -p --mode json` run rather than hand-written
from this document, and it is worth asking the Pi team whether `--mode rpc` is a more
contractual surface than the JSON event stream.
