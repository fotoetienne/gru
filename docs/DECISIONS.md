# Gru Design Decisions

**Last Updated:** 2025-12-02
**Status:** Final decisions made via quantitative DMX analysis

---

## Critical Implementation Decisions (2025-12-02)

### Decision 1: Architecture Approach

**Question:** How to spawn and manage autonomous Claude Code agents?

**Answer:** **CLI + Stream Parsing** (scored 0.735/1.0)

After evaluating 6 approaches through spike testing and DMX analysis:

| Approach | Score | Verdict |
|----------|-------|---------|
| CLI + Stream Parsing | 0.735 | ✅ SELECTED |
| ACP Integration | 0.706 | Future (V2+) |
| Agent SDK (Python) | 0.688 | Too complex |
| Pure CLI | 0.559 | No monitoring |
| Rust + tmux | 0.466 | High fragility |
| Zellij | 0.210 | Failed tests |

**Implementation:**
```bash
claude --print \
  --session-id <UUID> \
  --output-format stream-json \
  --dangerously-skip-permissions
```

Parse JSON events from stdout for real-time monitoring.

**See:** `experiments/DMX_ANALYSIS.md` for full analysis.

### Decision 2: Implementation Language

**Question:** Python or Rust?

**Answer:** **Rust** (scored 0.890 vs Python 0.110 - 8x advantage)

Rust scored perfectly on all high-priority criteria:
- Single Binary Deployment: 10/10 (Python: 4/10)
- Daemon Reliability: 10/10 (Python: 6/10)
- Concurrency: 10/10 (Python: 6/10)
- Type Safety: 10/10 (Python: 3/10)

**Rationale:** The vision is "single-binary, local-first" (mentioned 3x in docs). Architecture requires 24/7 daemon with true concurrency for 10+ minions. Rust is the only logical choice for the production system.

**See:** `experiments/LANGUAGE_DECISION.md` for full analysis.

### Technology Stack

**Core:**
- Language: Rust
- Runtime: Tokio async
- CLI: Clap
- GraphQL: async-graphql
- Web: Axum
- GitHub: octocrab

**Key Dependencies:**
```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
async-graphql = "7"
axum = "0.7"
clap = { version = "4", features = ["derive"] }
octocrab = "0.38"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
```

**Timeline:** 6-8 weeks to production-ready V1

---

## V1: Simplified Single-Lab Design

### Core Principles

1. **Single Lab assumption** - No distributed coordination needed initially
2. **Simple labels for state** - `gru:todo`, `gru:in-progress`, `gru:done`, `gru:failed`
3. **Comments as event log** - GitHub timeline API provides complete audit trail
4. **Early draft PR** - Create as soon as branch exists, provides natural lock mechanism
5. **GitHub Actions for CI** - Delegate test execution to existing infrastructure
6. **Claude Code for agents (V1)** - Use Claude Code as the initial agent runtime, design for pluggable agents later

---

## Agent Runtime

### V1: Claude Code

**Decision:** Start with Claude Code as the agent runtime for Minions.

**Rationale:**
- ✅ **Built-in tool use** - Git, file operations, bash commands already available
- ✅ **Agentic by default** - Designed for autonomous multi-step tasks
- ✅ **MCP support** - Can integrate with external tools and services
- ✅ **Proven** - Battle-tested for coding tasks
- ✅ **Fast iteration** - Focus on orchestration, not building agent infrastructure

**Integration:**
```yaml
# config.yaml
agent:
  runtime: claude-code
  model: claude-sonnet-4-5
  tools:
    - git
    - bash
    - file_operations
  mcp_servers:
    - github  # For GitHub API operations
```

**Minion = Claude Code Session:**

Each Minion **is** a Claude Code session. One-to-one mapping.

```
Minion M42  =  Claude Code session working in worktree ~/.gru/work/owner/repo/M42
Minion M43  =  Claude Code session working in worktree ~/.gru/work/owner/repo/M43
```

**How it works:**
1. Lab spawns Claude Code process for each Minion (separate session per worktree)
2. Passes initial prompt with issue description and codebase context
3. Claude Code autonomously works: reads code, makes changes, commits, pushes
4. Lab monitors Claude Code output for events and state changes
5. Lab handles GitHub integration layer (labels, comments, PR creation)
6. When issue complete, Lab terminates Claude Code session and cleans up

**Example Minion initialization:**
```bash
# Lab spawns Claude Code for Minion M42
cd ~/.gru/work/owner/repo/M42
claude --session M42 --context issue-123.md --autonomous
```

**Prompt template:**
```markdown
You are Minion M42 working on issue #123 in owner/repo.

## Issue
[issue description]

## Your Task
1. Read the issue carefully
2. Explore the codebase to understand relevant code
3. Implement the requested changes
4. Commit when a logical unit of work is complete AND tests pass
5. Push commits to trigger CI
6. Monitor CI results and fix failures
7. When ready, notify the Lab that you're done

## Guidelines
- Commit frequently (after each successful CI run)
- Use commit messages: [minion:M42] <description>
- If stuck or tests fail repeatedly, escalate
- Working directory: ~/.gru/work/owner/repo/M42
- Branch: minion/issue-123-M42

## Tools Available
- Git operations (commit, push, diff)
- File read/write
- Bash commands
- GitHub API (via MCP)

Begin work now.
```

**Session Status Tracking:**

Claude Code sessions have internal states that Lab must track:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStatus {
    Thinking,          // Claude processing
    UsingTool,         // Executing tool (git, bash, etc)
    Responding,        // Generating response
    WaitingInput,      // Needs user input
    WaitingPermission, // Needs approval for action
    Idle,              // Waiting for next instruction
    Complete,          // Task finished
    Error,             // Encountered error
}

struct Minion {
    // ... existing fields ...

    session_status: SessionStatus,
    current_tool: Option<String>, // Which tool is being used (if UsingTool)
    last_output: DateTime<Utc>,   // Last time session produced output
}
```

**Lab monitors session status by:**
1. Parsing Claude Code's JSON output stream
2. Watching for status indicators in stdout/stderr
3. Detecting tool use events
4. Tracking idle time (no output = potentially stuck)

**Example status transitions:**
```
idle → thinking → using_tool(git) → thinking → responding → idle
idle → thinking → using_tool(bash) → waiting_permission → idle
idle → thinking → responding → complete
```

**User Attach Sessions:**

Users can attach to any active Minion to observe or intervene:

```bash
# Attach to Minion M42 (read-only by default)
gru attach M42

# Attach with interactive mode (can send input)
gru attach M42 --interactive

# Attach to see live output
gru attach M42 --follow
```

**Architecture:**
```
┌──────────────┐
│   User TTY   │
└──────┬───────┘
       │
       │ gru attach M42
       ▼
┌──────────────┐
│     Lab      │
│              │
│  ┌────────┐  │
│  │ Attach │  │
│  │Manager │  │
│  └───┬────┘  │
└──────┼───────┘
       │
       │ Multiplex session I/O
       ▼
┌──────────────────────┐
│ Claude Code Session  │
│ (Minion M42)        │
│                      │
│ stdin  ←─────────────┼─── Lab + Attached users
│ stdout ─────────────→│──→ Lab + Attached users
│ stderr ─────────────→│──→ Lab + Attached users
└──────────────────────┘
```

**Attach Session Management:**

```rust
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use chrono::{DateTime, Utc, Duration};

struct AttachSession {
    id: String,
    minion_id: String,
    user_id: String,
    mode: AttachMode,
    started_at: DateTime<Utc>,
    expires_at: DateTime<Utc>, // Max 30 minutes

    // I/O streams
    input: mpsc::Sender<Vec<u8>>,    // User → Minion
    output: mpsc::Receiver<Vec<u8>>, // Minion → User
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachMode {
    ReadOnly,    // Just observe
    Interactive, // Can send input
}

struct AttachManager {
    sessions: Arc<RwLock<HashMap<String, AttachSession>>>,
}

impl AttachManager {
    async fn attach(&self, minion_id: &str, mode: AttachMode) -> Result<AttachSession, Error> {
        let minion = self.lab.get_minion(minion_id)
            .ok_or(Error::MinionNotFound)?;

        let (input_tx, input_rx) = mpsc::channel(100);
        let (output_tx, output_rx) = mpsc::channel(100);

        let session = AttachSession {
            id: generate_id(),
            minion_id: minion_id.to_string(),
            mode: mode.clone(),
            started_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(30),
            input: input_tx,
            output: output_rx,
        };

        // Start multiplexing minion output to this session
        let minion_clone = minion.clone();
        let output_tx_clone = output_tx.clone();
        tokio::spawn(async move {
            Self::stream_output(minion_clone, output_tx_clone).await;
        });

        // If interactive, also forward input
        if mode == AttachMode::Interactive {
            let minion_clone = minion.clone();
            tokio::spawn(async move {
                Self::stream_input(input_rx, minion_clone).await;
            });
        }

        self.sessions.write().await.insert(session.id.clone(), session.clone());
        Ok(session)
    }
}
```

**Security considerations:**
- Attach sessions timeout after 30 minutes
- Only one interactive attach per Minion at a time
- Multiple read-only attaches allowed
- Lab retains full control; user input is mediated
- Attach sessions preserved in audit log

**Use cases:**
```bash
# Watch Minion work in real-time
gru attach M42 --follow

# Debug stuck Minion
gru attach M42 --interactive
> # Can send commands to help unstick

# Review what Minion is doing
gru attach M42
[Shows current status: "using_tool(git)", last 100 lines of output]

# Detach but leave Minion running
Ctrl+D or type 'detach'
```

**UI Display:**
```
╭─────────────────────────────────────────────╮
│ Attached to Minion M42                      │
│ Issue: #123 - Add user authentication       │
│ Status: using_tool(bash)                    │
│ Uptime: 15m 32s                            │
│ Commits: 2                                  │
╰─────────────────────────────────────────────╯

[M42] Running: npm test
[M42] 
[M42] > test
[M42] > jest
[M42] 
[M42]  PASS  src/auth.test.js
[M42]    ✓ should generate JWT token (45ms)
[M42]    ✓ should validate token (12ms)
[M42] 
[M42] Tests: 2 passed, 2 total
[M42] Status: thinking
[M42] All tests passed! Committing changes...

Press Ctrl+D to detach (Minion continues running)
```

**Implementation: tmux vs Custom (HISTORICAL — superseded by CLI + stream-json)**

> **Note:** This entire section is historical. The tmux approach was evaluated but never shipped.
> V1 uses CLI + stream-json parsing instead. Kept for decision context only.

**Option A: Use tmux**

Each Minion runs in a dedicated tmux session:

```bash
# Lab spawns Minion M42
tmux new-session -d -s "minion-M42" -c ~/.gru/work/owner/repo/M42
tmux send-keys -t "minion-M42" "claude --session M42 --context issue-123.md" Enter

# User attaches
gru attach M42
# → Lab runs: tmux attach-session -t "minion-M42" -r  (read-only)

# Interactive attach
gru attach M42 --interactive
# → Lab runs: tmux attach-session -t "minion-M42"
```

**Pros:**
- ✅ **Battle-tested** - tmux handles all session management, multiplexing
- ✅ **Free scrollback** - Built-in history buffer
- ✅ **Multiple attaches** - tmux natively supports many viewers
- ✅ **Survives Lab restart** - Sessions persist if Lab crashes
- ✅ **Standard tooling** - Users already know tmux commands
- ✅ **Copy/paste** - tmux copy mode works out of the box
- ✅ **Session recording** - `tmux pipe-pane` for logging

**Cons:**
- ⚠️ **External dependency** - Requires tmux installed
- ⚠️ **Less control** - Harder to intercept/mediate I/O
- ⚠️ **Platform dependency** - tmux not available on Windows (WSL only)
- ⚠️ **Session pollution** - Orphaned tmux sessions if cleanup fails
- ⚠️ **Abstraction leakage** - Users see raw tmux, not Gru's semantics

**Option B: Custom I/O multiplexing**

Lab manages stdin/stdout/stderr directly:

```rust
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use std::process::Stdio;

struct Minion {
    // ... existing fields ...

    claude_session: Option<ClaudeCodeSession>,
    output_buffer: Arc<RwLock<RingBuffer>>, // Last N lines for late attachers
    attachments: Arc<RwLock<Vec<AttachSession>>>, // Currently attached users
}

impl Minion {
    async fn start(&mut self) -> Result<(), Error> {
        let mut cmd = Command::new("claude")
            .arg("--session")
            .arg(&self.id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Capture I/O
        let stdin = cmd.stdin.take().ok_or(Error::StdinMissing)?;
        let stdout = cmd.stdout.take().ok_or(Error::StdoutMissing)?;
        let stderr = cmd.stderr.take().ok_or(Error::StderrMissing)?;

        self.claude_session = Some(ClaudeCodeSession {
            process: cmd,
            stdin,
            stdout,
            stderr,
        });

        // Start output multiplexing
        let minion_clone = self.clone();
        tokio::spawn(async move {
            minion_clone.multiplex_output(stdout).await;
        });

        let minion_clone = self.clone();
        tokio::spawn(async move {
            minion_clone.multiplex_output(stderr).await;
        });

        Ok(())
    }

    async fn multiplex_output<R: AsyncRead + Unpin>(&self, reader: R) {
        let mut lines = BufReader::new(reader).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let line_bytes = line.as_bytes().to_vec();

            // Save to buffer (for late attachers)
            self.output_buffer.write().await.write(&line_bytes);

            // Send to all attached sessions
            let attachments = self.attachments.read().await;
            for attach in attachments.iter() {
                // Non-blocking send
                let _ = attach.output.try_send(line_bytes.clone());
            }

            // Save to archive
            self.archive_output(&line_bytes).await;
        }
    }
}
```

**Pros:**
- ✅ **Full control** - Lab can intercept/filter/log everything
- ✅ **No dependencies** - Pure Go, works everywhere
- ✅ **Tight integration** - Easy to parse status, trigger events
- ✅ **Clean semantics** - Abstract away raw terminal details
- ✅ **Cross-platform** - Works on Windows, Linux, macOS

**Cons:**
- ⚠️ **More code** - Need to implement multiplexing, buffering
- ⚠️ **Terminal quirks** - Have to handle PTY, control sequences
- ⚠️ **Less resilient** - Sessions die with Lab (unless we persist)
- ⚠️ **Reinventing wheel** - Solving problems tmux already solved

**Decision: V1 uses CLI + Stream Parsing (tmux superseded)**

**IMPORTANT UPDATE (2025-12-02):** After DMX analysis and spike testing, tmux approach has been **superseded** by CLI + stream-json parsing. This provides better monitoring with less complexity.

**Original V1 tmux rationale (now superseded):**
1. **Speed to MVP** - tmux gives us attach functionality for free
2. **Reliability** - tmux is rock-solid, handles edge cases we'd miss
3. **User familiarity** - Developers already know tmux
4. **Easy migration** - Can swap implementation later without changing API

**Why CLI + stream-json is better:**
- No subprocess complexity (tmux sessions)
- No fragile regex parsing
- JSON events provide structured monitoring
- Claude CLI has `--output-format stream-json` natively
- Simpler deployment (no tmux dependency)
- See `experiments/DMX_ANALYSIS.md` for quantitative comparison

**V1 Implementation:**
```rust
use tokio::process::Command;
use std::path::Path;

impl Minion {
    async fn start(&mut self) -> Result<(), Error> {
        let session_name = format!("gru-minion-{}", self.id);

        // Create tmux session
        Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name, "-c", &self.worktree_path])
            .output()
            .await?;

        // Start Claude Code in tmux
        let command = format!("claude --session {} --context {}", self.id, self.context_file);
        Command::new("tmux")
            .args(["send-keys", "-t", &session_name, &command, "Enter"])
            .output()
            .await?;

        // Enable logging
        let log_path = Path::new(&self.archive_path).join("session.log");
        let log_command = format!("cat >> {}", log_path.display());
        Command::new("tmux")
            .args(["pipe-pane", "-t", &session_name, &log_command])
            .output()
            .await?;

        self.tmux_session = Some(session_name);
        Ok(())
    }
}

impl AttachManager {
    async fn attach(&self, minion_id: &str, mode: AttachMode) -> Result<(), Error> {
        let minion = self.lab.get_minion(minion_id)
            .ok_or(Error::MinionNotFound)?;

        let mut args = vec!["attach-session", "-t", minion.tmux_session.as_ref().unwrap()];

        if mode == AttachMode::ReadOnly {
            args.push("-r"); // read-only flag
        }

        Command::new("tmux")
            .args(&args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()?
            .wait()
            .await?;

        Ok(())
    }
}
```

**When to build custom (historical — no longer applicable):**
- Multiple users request Windows support (tmux unavailable)
- Need fine-grained I/O interception for features
- Want to eliminate external dependencies
- Performance issues with tmux overhead

> **Note:** The CLI + stream-json approach eliminated the need for both tmux and custom I/O multiplexing.

**Alternative: Zellij (historical)**

[Zellij](https://zellij.dev/) is a modern Rust-based terminal multiplexer gaining popularity:

**Pros:**
- ✅ **Better defaults** - Sessions auto-managed, better UX out of the box
- ✅ **Plugin system** - Native WASM plugins for extensibility
- ✅ **Modern codebase** - Written in Rust, active development
- ✅ **Better UI** - Context-aware bottom bar, easier discoverability
- ✅ **Simpler API** - Cleaner command structure

**Cons:**
- ⚠️ **Less mature** - Newer project, smaller ecosystem
- ⚠️ **Lower adoption** - Not universally installed like tmux/screen
- ⚠️ **API stability** - May change more rapidly than tmux

**Zellij's Rust API:**

If Gru is written in Rust, Zellij offers interesting integration possibilities:

**Available Rust crates:**
- `zellij-tile` - Plugin API for extending Zellij
- `zellij-utils` - CLI enums including `Sessions` and `SessionCommand`
- `zellij-client` - Client library for programmatic control

**Potential advantages:**
```rust
// Hypothetical: Direct Rust API instead of shelling out
use zellij_client::Session;

let session = Session::new("gru-minion-M42")
    .working_dir(&minion.worktree_path)
    .create()?;

session.send_keys("claude --session M42...")?;
session.attach(AttachMode::ReadOnly)?;
```

**However, research shows:**
- ⚠️ **Plugin-focused** - Zellij's Rust API primarily for WASM plugins, not embedding
- ⚠️ **Still CLI-based** - Session control via `zellij attach`, `zellij list-sessions` commands
- ⚠️ **Not a library** - No documented API for embedding Zellij as a library
- ⚠️ **Similar to tmux** - Would still shell out to `zellij` binary

**Reality check:**
Both tmux and Zellij are external binaries you invoke via CLI. Neither offers a true embeddable library API. The integration code looks nearly identical:

```rust
// tmux
Command::new("tmux")
    .args(["new-session", "-d", "-s", session_name])
    .spawn()?;

// zellij  
Command::new("zellij")
    .args(["--session", session_name])
    .spawn()?;
```

**Historical verdict:** tmux was initially favored for V1, but CLI + stream-json parsing proved superior and was selected instead. Neither tmux, Zellij, nor GNU Screen are used in the shipped implementation.

---

## Implementation Language: Go vs Rust

### Context

Gru is a CLI tool that:
- Manages processes (Claude Code CLI sessions)
- Makes HTTP calls (GitHub API)
- Does file I/O (git worktrees, logs)
- Provides CLI interface
- Potentially exposes GraphQL API (future)

### Option A: Go

**Pros:**
- ✅ **Fast to ship** - Simpler syntax, faster compile times
- ✅ **Better for services** - Excellent HTTP/gRPC libraries, proven for APIs
- ✅ **Easy concurrency** - Goroutines + channels are simple and powerful
- ✅ **Great CLI libraries** - cobra, viper mature and widely used
- ✅ **Deployment** - Single static binary, cross-compile trivial
- ✅ **GitHub integrations** - go-github library is comprehensive
- ✅ **Process management** - os/exec is straightforward
- ✅ **Familiar** - More developers know Go than Rust

**Cons:**
- ⚠️ **Error handling** - Verbose `if err != nil` everywhere
- ⚠️ **Type safety** - Weaker than Rust (no sum types, nil pointers)
- ⚠️ **Memory usage** - Larger binaries, GC overhead
- ⚠️ **Less trendy** - Rust has more mindshare in 2024/2025

**Good fit for Gru because:**
- Orchestration layer (not performance-critical compute)
- Heavy I/O and API calls (Go's sweet spot)
- Need to ship quickly
- Service patterns well-established

### Option B: Rust

**Pros:**
- ✅ **Type safety** - Sum types, no null, exhaustive matching
- ✅ **Performance** - Zero-cost abstractions, no GC
- ✅ **Modern tooling** - cargo, clippy, rustfmt excellent
- ✅ **Small binaries** - More compact than Go
- ✅ **Growing ecosystem** - tokio, serde, clap mature
- ✅ **Memory safety** - Prevents entire classes of bugs

**Cons:**
- ⚠️ **Slower to ship** - Longer compile times, steeper learning curve
- ⚠️ **Complexity** - Lifetimes, ownership, async can be hard
- ⚠️ **Smaller ecosystem** - Fewer libraries than Go for some domains
- ⚠️ **Harder onboarding** - Contributors need Rust knowledge
- ⚠️ **Async ecosystem** - Still evolving, some rough edges

**Good fit for Gru because:**
- CLI tools are Rust's sweet spot
- Type safety helps with state machine complexity
- Modern developers prefer Rust
- Can use Zellij plugin system (future)

### Comparison for Gru's Specific Needs

| Aspect | Go | Rust |
|--------|----|----- |
| HTTP client | `net/http` ★★★★★ | `reqwest` ★★★★☆ |
| GitHub API | `go-github` ★★★★★ | `octocrab` ★★★☆☆ |
| CLI framework | `cobra` ★★★★★ | `clap` ★★★★★ |
| Process mgmt | `os/exec` ★★★★☆ | `std::process` ★★★★☆ |
| GraphQL | `gqlgen` ★★★★★ | `async-graphql` ★★★★☆ |
| SQLite | `go-sqlite3` ★★★★★ | `rusqlite` ★★★★★ |
| YAML parsing | `gopkg.in/yaml.v3` ★★★★★ | `serde_yaml` ★★★★★ |
| Time to MVP | ★★★★★ | ★★★☆☆ |
| Type safety | ★★★☆☆ | ★★★★★ |
| Contributor pool | ★★★★★ | ★★★☆☆ |

### Decision: Rust for V1 ✅

**CONFIRMED (2025-12-02):** DMX analysis strongly validates Rust choice with 0.890 score vs Python 0.110.

**Rationale:**
1. **Single Binary** - Emphasized 3x in product docs; Rust delivers perfectly (10/10)
2. **Daemon Reliability** - 24/7 operation requires Rust's stability (10/10)
3. **True Concurrency** - Managing 10+ minions needs no-GIL parallelism (10/10)
4. **Type Safety** - State machine + lifecycle management benefit from compile-time guarantees (10/10)
5. **Production Polish** - "It just works" deployment experience (10/10)
6. **Modern tooling** - cargo, clippy, rustfmt are excellent
7. **CLI sweet spot** - Rust excels at CLI tools (ripgrep, fd, bat, etc.)

**Key Insight:** The architecture was designed for Rust's strengths. Not a preference, but an alignment with requirements.

**Code structure:**
```
gru/
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs      # CLI setup (clap)
│   │   ├── lab.rs      # gru lab command
│   │   └── attach.rs   # gru attach command
│   ├── lab/
│   │   ├── mod.rs      # Lab orchestrator
│   │   ├── scheduler.rs
│   │   └── poller.rs
│   ├── minion/
│   │   ├── mod.rs      # Minion state machine
│   │   └── session.rs  # tmux session wrapper
│   ├── github/
│   │   ├── mod.rs      # GitHub API client (octocrab)
│   │   └── events.rs   # Timeline, labels, PRs
│   └── attach/
│       └── manager.rs  # Attach session management
├── Cargo.toml
└── Cargo.lock
```

**Key dependencies:**
```toml
[dependencies]
clap = { version = "4.5", features = ["derive"] }
tokio = { version = "1.40", features = ["full"] }
octocrab = "0.40"  # GitHub API
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
serde_json = "1.0"
sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio"] }
anyhow = "1.0"
thiserror = "1.0"
tracing = "0.1"
tracing-subscriber = "0.3"
```

**When to reconsider Rust:**
- Performance becomes measurable bottleneck
- Want to leverage Zellij plugin system
- Type safety bugs become significant
- Rust ecosystem catches up for GitHub/API work
- Team composition shifts to Rust-heavy

**Hybrid approach (unlikely):**
- Core in Go for speed
- Performance-critical pieces in Rust (via FFI)
- Probably overkill for Gru's use case

### Alternative: Why Not TypeScript/Python?

**TypeScript:**
- ❌ Runtime overhead (Node.js)
- ❌ Deployment complexity (node_modules)
- ✅ Good for Tower web UI (future)

**Python:**
- ❌ Deployment (virtualenv, dependencies)
- ❌ Slower performance
- ❌ No static typing (even with mypy)
- ✅ Quick prototyping

**Implementation Notes:**

- Rust + Tokio provides excellent async foundation
- CLI + stream-json parsing eliminates need for complex session management
- No tmux/zellij dependency reduces complexity
- Single binary deployment aligns with product vision
- See `experiments/` for working prototypes validating approach

---

### Future: Pluggable Agent Architecture

**Goal:** Support multiple agent runtimes as ecosystem evolves.

**Design for extensibility:**

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

// Agent interface
#[async_trait]
trait Agent {
    // Initialize agent with context
    async fn initialize(&mut self, ctx: AgentContext) -> Result<(), Error>;

    // Execute task
    async fn execute(&mut self, task: Task) -> Result<Result, Error>;

    // Stream events during execution
    fn events(&self) -> mpsc::Receiver<AgentEvent>;

    // Pause/Resume/Stop
    async fn pause(&mut self) -> Result<(), Error>;
    async fn resume(&mut self) -> Result<(), Error>;
    async fn stop(&mut self) -> Result<(), Error>;
}

// Agent runtime types
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentRuntime {
    ClaudeCode,   // claude-code
    OpenAIAgents, // openai-agents (Future)
    Custom,       // custom (Future)
    LocalLLM,     // local-llm (Future)
}
```

**Potential future runtimes:**
- **OpenAI Agents** - When OpenAI ships their agent framework
- **Devin API** - If Cognition Labs opens API access
- **Local LLMs** - For privacy-sensitive codebases (Llama, Mistral)
- **Custom agents** - User-defined agent scripts/binaries
- **Hybrid** - Combine multiple agents (Claude for planning, specialist for security, etc.)

**Adapter pattern:**
```rust
struct ClaudeCodeAdapter {
    session: Option<ClaudeCodeSession>,
}

#[async_trait]
impl Agent for ClaudeCodeAdapter {
    async fn execute(&mut self, task: Task) -> Result<Result, Error> {
        // Translate task to Claude Code prompt
        let prompt = format_prompt(&task);

        // Execute via Claude Code API/CLI
        let session = self.session.as_mut().ok_or(Error::SessionNotInitialized)?;
        let result = session.run(&prompt).await?;

        // Parse result and events
        parse_result(&result)
    }

    // ... other trait methods
}
```

**Why defer this:**
- ❌ **Premature abstraction** - Only one runtime for V1, wait for real use cases
- ❌ **Ecosystem immature** - Agent frameworks still evolving rapidly
- ❌ **YAGNI** - May never need multiple runtimes if Claude Code sufficient

**When to build pluggable system:**
- Multiple users requesting alternative runtimes
- Cost optimization needs (cheaper local models for simple tasks)
- Privacy requirements (can't use cloud LLMs)
- Performance needs (local agents faster for certain tasks)

---

## State Management

### Labels

**Issue States:**
- `gru:todo` - Issue is ready to be claimed
- `gru:in-progress` - Minion actively working
- `gru:done` - Completed successfully
- `gru:failed` - Failed after retries
- `gru:blocked` - Minion needs human help
- `gru:ready-to-merge` - PR passes checks and is ready
- `gru:auto-merge` - Auto-merge enabled on PR
- `gru:needs-human-review` - PR requires human review before merge

**Rationale:** Simple, visible in UI, easy to filter. The `gru:` prefix namespaces labels to avoid collisions with user labels.

### Event Log via Comments

Use GitHub Timeline API (`GET /repos/:owner/:repo/issues/:number/timeline`) to:
- Reconstruct full Minion lifecycle
- Detect state transitions
- Build audit trail
- Track all labeled/commented/cross-referenced events

**Comment Format (YAML):**
```yaml
---
event: minion:claim
minion_id: M42
lab_id: lab-hostname
branch: minion/issue-123-M42
timestamp: 2025-01-30T12:34:56Z
---
```

**Example claim comment:**
```markdown
🤖 **Minion M42 claimed this issue**

---
event: minion:claim
minion_id: M42
lab_id: lab-macbook-pro.local
branch: minion/issue-123-M42
timestamp: 2025-01-30T12:34:56Z
---

I'll start working on this now. You can track progress in the draft PR.
```

**Example progress update:**
```markdown
🔄 **Progress Update**

---
event: minion:progress
minion_id: M42
phase: implementation
commits: 2
tests_passing: true
duration_minutes: 15
---

Completed authentication endpoints. Running CI checks now.
```

**Example failure report:**
```markdown
❌ **Minion M42 needs help**

---
event: minion:failed
minion_id: M42
failure_reason: ci_failed_max_retries
attempts: 5
last_error: "Test suite timeout after 10 minutes"
---

I've tried fixing the test failures 5 times but keep hitting timeouts. 
Could you take a look at the CI logs? @human-reviewer
```

**Advantages:**
- ✅ Immutable, ordered history
- ✅ Accessible via REST and GraphQL
- ✅ No external database needed
- ✅ Human-readable in UI

---

## Draft PR as Lock Mechanism

### Workflow

1. **Lab claims issue** → add `claimed` label, post claim comment
2. **Lab creates branch** → `minion/issue-123-M42`
3. **Lab immediately creates draft PR** → Title: `[DRAFT] Fixes #123`
4. **If PR creation succeeds** → Lab owns the issue, proceed with work
5. **If PR creation fails** (duplicate branch) → Another Lab won, abort gracefully

### Commit Strategy

**Rule of thumb:** Minion commits when:
- Set of related changes complete AND
- CI passes (tests green)

**Benefits:**
- ✅ Checkpoints progress (can resume on crash)
- ✅ Incremental review possible
- ✅ CI runs on each checkpoint
- ✅ Clear history of Minion's work

**Commit Message Format:**
```
[minion:M42] Add user authentication

- Implement login endpoint
- Add JWT token generation
- Add password hashing

Tests: ✓ (12 passed)
```

---

## GitHub Actions Integration

### Lab Responsibilities

- **Trigger workflows** via repository dispatch or workflow dispatch
- **Monitor check runs** via GitHub Checks API
- **React to failures** by fetching logs and attempting fixes

### Minion Workflow

1. Push commit to branch
2. GitHub Actions workflow triggers automatically
3. Minion polls check runs status
4. **On success** → proceed to next task or mark ready for review
5. **On failure** → fetch failure logs, analyze, attempt fix, commit retry

### Advantages over Local Testing

- ✅ No local test dependencies/setup
- ✅ Proper isolation (containers/VMs)
- ✅ Reuses existing CI configuration
- ✅ Matches human developer workflow
- ✅ No resource limits on Lab host

---

## Issue Dependency DAG (Future)

Support dependencies via issue body metadata:

```markdown
## Dependencies
- depends-on: #123
- blocks: #456
```

**Implementation:**
- Parse issue body on claim
- Check dependency status before starting
- Wait if dependencies not resolved
- Add `blocked` label with reason

---

## Projects v2 Integration (Future)

When multi-Lab or advanced tracking needed:

### Custom Fields
- **Status** (single-select): Ready | Claimed | In Progress | Review | Done | Failed
- **Minion ID** (text)
- **Lab** (text)
- **Cost** (number) - LLM tokens/dollars
- **Started** (date)
- **Retries** (number)

### Advantages
- Visual Kanban board
- Rich querying capabilities
- Better UX for filtering/sorting
- Built-in cost tracking

**Note:** Defer until single-Lab proven and multi-Lab coordination needed.

---

## File Layout

```
~/.gru/
  repos/
    <owner>/
      <repo>.git                    # Bare repository mirror
  work/
    <owner>/
      <repo>/
        <MINION_ID>/                # Git worktree for active Minion
          .git                      # Worktree metadata
          <repo files>              # Working copy
  archive/
    <MINION_ID>/
      events.jsonl                  # Structured event log
      plan.md                       # Minion's execution plan
      commits.log                   # Git commit history
      ci-results.json               # CI check run results
  state/
    minions.db                      # SQLite: active Minions state
    cursors.json                    # GitHub timeline cursors per issue
  config.yaml                       # Lab configuration
```

---

## Simplified Lifecycle (V1)

### Issue Claim
1. Poll GitHub for issues with `gru:todo` label
2. Select highest priority (manual priority label or oldest)
3. Add `gru:in-progress` label, remove `gru:todo`
4. Post structured claim comment with Minion ID, timestamp
5. Create branch `minion/issue-123-M42`
6. Create draft PR immediately

### Minion Work Loop
1. Read issue description and comments
2. Generate execution plan
3. Implement changes in worktree
4. Run local validation (lint, type check)
5. Commit changes
6. Push to branch → triggers CI
7. Wait for CI results
8. **If CI passes** → continue or mark ready for review
9. **If CI fails** → analyze logs, attempt fix, goto step 4
10. **Max retries exceeded** → add `gru:failed` label, request human help

### PR Submission
1. Convert draft PR to ready for review
2. Post summary comment with:
   - Changes made
   - Test results
   - Cost estimate (tokens used)
   - Confidence score
3. Subscribe to PR review events
4. Monitor for review comments and check failures

### Post-PR Monitoring
1. Poll PR for new review comments
2. Respond to review feedback:
   - **Simple changes** → implement and push
   - **Unclear requests** → ask clarifying questions
   - **Complex refactors** → create handoff for human
3. Monitor check runs for failures
4. On merge → add `gru:done`, archive logs, cleanup worktree

---

## Error Handling

### Retry Strategy
- **Flaky tests** → retry up to 3 times with exponential backoff
- **CI failures** → analyze logs, attempt fix, max 5 iterations
- **Rate limits** → exponential backoff, switch to lower priority work
- **Network errors** → retry with jitter

### Escalation
After exhausting retries:
1. Add `gru:failed` label
2. Post detailed failure report comment
3. Tag human for assistance
4. Park Minion in paused state (don't cleanup)
5. Human can resume via attach session or abandon

---

## Observability

### Structured Events (events.jsonl)
```jsonl
{"event":"claimed","minion_id":"M42","issue":123,"timestamp":"2025-01-30T12:34:56Z"}
{"event":"plan_generated","tokens":450,"plan":"..."}
{"event":"commit","sha":"abc123","message":"Add auth","tests_passed":true}
{"event":"ci_triggered","workflow":"test","run_id":123456}
{"event":"ci_passed","duration_ms":45000}
{"event":"pr_created","pr_number":789,"draft":false}
```

### Metrics to Track
- Issues claimed per hour
- Time to first commit
- Time to PR submission
- CI pass rate
- Review response time
- Cost per issue (LLM tokens)
- Success rate (merged vs abandoned)

---

## Security

### Token Scoping
- **Lab GitHub token** requires: `repo`, `workflow`, `read:org`
- Store in `~/.gru/config.yaml` with restricted permissions (0600)

### Sandbox Considerations
- CI runs in GitHub Actions (already isolated)
- Local worktrees isolated per Minion
- No network access during local validation (future: use network namespace)

### Secrets Handling
- Never commit secrets (pre-commit hook checks)
- Minion has no access to repo secrets (only CI does)
- Redact sensitive data from logs and comments

---

## Future Optimizations

### Event-Driven Architecture
- Replace polling with GitHub webhooks
- Labs listen for `issues.labeled`, `pull_request.review_requested`, `check_run.completed`
- Reduce API calls and latency

### Caching & RAG
- Local embedding index of codebase for semantic search
- Cache GitHub API responses with ETags
- Reuse test results across similar changes

### Multi-Lab Coordination
- First-PR-wins for conflict resolution
- Heartbeat comments for liveness detection
- Stale issue reclamation (after 1 hour no activity)

### Cost Optimization
- Model selection based on task complexity (Haiku for simple, Sonnet for complex)
- Prompt caching for repeated codebase context
- Incremental context updates (don't resend entire codebase)

---

## Design Constraints

### What We're NOT Building (Yet)

- ❌ Multi-Lab distributed locking
- ❌ Real-time collaboration between Minions
- ❌ Custom test execution environments
- ❌ Local LLM support (cloud-only for V1)
- ❌ Web UI (Tower deferred to V2)
- ❌ Slack/notifications
- ❌ Learning from past PRs
- ❌ Code review quality scoring

### V1 Scope

- ✅ Single Lab, local execution
- ✅ Multi-repo support (one Lab watches multiple repos)
- ✅ Simple label-based state machine (3 states + done/failed)
- ✅ GitHub Actions for CI
- ✅ Local testing via pre-commit hooks
- ✅ Draft PR workflow
- ✅ Basic error handling and retries
- ✅ Event log in comments + local files
- ✅ CLI-only interface (`gru lab`)
- ✅ Manual issue prioritization
- ✅ No SQLite (in-memory state, file-based cursors)

---

## Additional V1 Design Decisions

### State Management

**No SQLite database:**
- In-memory state for active Minions
- Simple JSON file for timeline cursors (`~/.gru/state/cursors.json`)
- Recovery on restart: check Minion registry, fetch issue state from GitHub
- Archive logs to disk for completed/failed Minions

**Labels (simplified to 3 states):**
- `gru:todo` → `gru:in-progress` → `gru:done` / `gru:failed`
- No `claimed` intermediate state (goes directly to `in-progress`)
- Detailed state (review, blocked, testing) in YAML comment events

**Minion lifecycle:**
- Stay alive indefinitely until PR merged/closed
- No timeout for inactive PRs (occupies slot but ensures responsiveness)
- Failed Minions stay alive for debugging (`gru attach` to inspect)
- Orphaned Minions (issue closed while running) marked as `Orphaned` state, kept alive

### Testing & CI

**Local testing via pre-commit hooks:**
- Lab runs repo-init to install git hooks
- Pre-commit hook runs tests automatically before allowing commit
- Minion commits frequently (each logical unit of work)
- Tests run automatically, blocking bad commits
- GitHub Actions runs as secondary verification

**CI monitoring (30s poll interval):**
- Poll check runs every 30 seconds
- High retry limit (10-15 attempts) before escalating
- On max retries: pause (not fail), request human review
- Minion monitors: failed checks, pending checks, stale branch, merge conflicts

**Conflict resolution:**
- Minion attempts to resolve merge conflicts
- Runs tests locally to verify resolution
- Only pushes if tests pass
- If tests fail after resolution: pause and request human help

### Minion Behavior

**Review autonomy:**
- Maximally autonomous - implements changes, answers questions, refactors
- Can decline suggestions with reasoning
- Can create follow-up issues for out-of-scope work (creates immediately, links in comment)
- Only escalates when truly stuck

**Minion ID format:**
- Sequential base36 with padding: `M001`, `M002`, ..., `M00z`, `M010`, ..., `Mzzz`
- Compact, human-readable, sortable
- Monotonic counter stored in `~/.gru/state/next_id.txt`

**Branch management:**
- Format: `minion/issue-<number>-<minion-id>`
- Examples: `minion/issue-123-M007`, `minion/issue-456-M00a`
- Branches from repository default branch (main/master/develop, detected via API)
- On PR merge: delete both local and remote branch

**Branch naming logic:**
```rust
fn generate_branch_name(issue_number: i32, minion_id: &str) -> String {
    format!("minion/issue-{}-{}", issue_number, minion_id)
}
```

### Configuration

**Tokens in environment variables only:**
```bash
export GRU_GITHUB_TOKEN="ghp_..."
export ANTHROPIC_API_KEY="sk-ant-..."
```

**Config file for non-sensitive settings:**
```yaml
# ~/.gru/config.yaml
repos:
  - owner/repo1
  - owner/repo2

lab:
  slots: 2
  poll_interval: 30s
```

**Multi-repo support:**
- One Lab instance watches all configured repos
- Slots shared across all repos
- Scheduler prioritizes across repos

**Global config only (V1):**
- Per-repo overrides deferred to V2
- Single `~/.gru/config.yaml`

### Draft PR Workflow

**Initial creation:**
- Title: `[DRAFT] Fixes #123: <issue title>`
- Body: Template with "🤖 Minion M042 is working on this..."
- Created immediately after branch creation

**Ready for review:**
- Title: `Fixes #123: <descriptive title>` (remove DRAFT prefix)
- Body: Updated with proper description, changes, approach
- Convert from draft to ready

### Prompt Configuration

**Minimal initial context:**
- Issue description only
- Working directory and branch name
- Guidelines about committing and testing
- Everything else (README, CONTRIBUTING, git history) available in worktree

**Commit guideline:**
- "Commit after each logical unit of work (tests run automatically via pre-commit hook)"

### Archives

**Retention policy:**
- Keep forever by default (V1)
- Just text files, minimal space
- User can manually clean `~/.gru/archive/`
- Add retention config later if needed

### CLI Output

**Human-readable by default:**
```
$ gru minions list

ID    Issue  Repo          State        Uptime    Commits
───────────────────────────────────────────────────────────
M001  #123   owner/repo    in-progress  15m 32s   2
M002  #456   owner/other   review       2h 14m    5
```

Add `--json` flag in future if needed for scripting.

### Onboarding

**`gru init` command:**

First-time setup wizard that:
1. Creates `~/.gru/` directory structure
2. Generates template `config.yaml` with comments
3. Checks for required environment variables (`GRU_GITHUB_TOKEN`, `ANTHROPIC_API_KEY`)
4. Validates GitHub token scopes (requires `repo`, `workflow`)
5. Tests GitHub API connectivity
6. Optionally clones/mirrors configured repos
7. Sets up git config (user.name, user.email for commits)

**Example flow:**
```bash
$ gru init

🤖 Gru Setup Wizard

Checking environment variables...
✓ GRU_GITHUB_TOKEN found
✓ ANTHROPIC_API_KEY found

Validating GitHub token...
✓ Token has required scopes: repo, workflow
✓ Connected to GitHub as: username

Creating directory structure...
✓ Created ~/.gru/repos
✓ Created ~/.gru/work
✓ Created ~/.gru/archive
✓ Created ~/.gru/state
✓ Created ~/.gru/logs

Generated config file: ~/.gru/config.yaml
Edit this file to configure repositories and settings.

Repository setup:
? Clone repositories now? (y/n): y
✓ Cloned owner/repo1
✓ Cloned owner/repo2

✓ Setup complete! Run 'gru lab' to start.
```

**Subsequent runs:**
- `gru lab` checks if `~/.gru/` exists
- If missing, suggests running `gru init` first
- Auto-creates missing subdirectories if root exists

## Self-Review Strategy: Prompt-Based over Structured Loop

**Question:** How should Minions self-review their work before opening a PR?

**Answer:** **Prompt-based self-review (Option A)** — defer structured enforcement loop (Option B)

**Context:** Issue #515 proposed three options for self-review:
- **Option A (Prompt-based):** Add review instructions to the task prompt, relying on the code-reviewer agent
- **Option B (Structured loop):** Add a dedicated review phase in the orchestration layer with DONE/ITERATE gating
- **Option C (Model mixing):** Use different models for work vs review (future enhancement)

**Data (from #655 investigation):**
- 92% of Minion runs invoke the code-reviewer agent via prompt instructions
- When consumed, 100% of reviews find actionable issues (63% high-priority)
- Minions consistently address review findings when they read them
- The 8% that skip review have legitimate reasons (duplicate issues, mechanical changes, post-review fixups)

**Decision:** Option A is sufficient. The prompt-based approach in `src/prompt_loader.rs` (Section 4: Code Review) already achieves high review rates without orchestration complexity. The main gap was an async fire-and-forget problem where reviews were triggered but not consumed, which is a prompt fix (#648, #649), not an orchestration change.

**Why not Option B:**
- Adds orchestration complexity (DONE/ITERATE parsing, iteration caps, extra agent calls)
- 2-3x token cost increase per Minion
- Solves a problem that prompt engineering handles at 92%+ rate
- The remaining gap is addressable by improving prompt reliability, not adding a structured loop

**Revisit when:**
- Prompt-based review rate drops below 80%
- Review quality degrades (reviews stop finding actionable issues)
- Multi-agent workflows require explicit review gating

**See:** #515 (original proposal), #655 (data-driven update), #648/#649 (prompt fixes)

---

## Open Questions (Deferred)

1. **Comment rate limiting**: How often should Minions post progress updates? (Freely vs batched vs significant events only)
2. **Cost limits**: Max tokens per issue before pausing? (Default: unlimited for V1?)
3. **Per-repo config overrides**: When to add support?
4. **Archive retention**: When to add configurable cleanup?

---

## References

- [GitHub Timeline Events API](https://docs.github.com/en/rest/issues/timeline)
- [GitHub Checks API](https://docs.github.com/en/rest/checks)
- [GitHub Projects v2 API](https://docs.github.com/en/issues/planning-and-tracking-with-projects/automating-your-project/using-the-api-to-manage-projects)
