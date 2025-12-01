# Gru Bootstrap Plan: Minimal Viable Gru 0.1

## Purpose

This plan defines the absolute minimum implementation needed to get Gru working well enough to build the rest of itself. Once Gru 0.1 is functional, we can create GitHub issues for the remaining features and let Minions implement them.

**Target: Working system in 3-5 days of focused development**

## What Gru 0.1 Must Do

1. Poll GitHub for issues labeled `ready-for-minion`
2. Claim one issue (single slot only for 0.1)
3. Create worktree and spawn Claude Code in tmux
4. Post basic GitHub updates (labels, comments)
5. Let humans attach to observe progress
6. Clean up when told to

**What we can defer:**
- Multiple concurrent Minions (slots > 1)
- Sophisticated error handling and retries
- CI monitoring and auto-fixes
- Post-PR review monitoring
- Metrics and observability
- Archive system
- Full lifecycle automation

## Bootstrap Implementation Plan

### Milestone 1: Hello World Gru (Day 1)

**Goal:** `cargo run -- init` works and creates directory structure.

**Implementation:**
- [ ] Create Cargo.toml with minimal dependencies (clap, tokio, serde, serde_yaml)
- [ ] Implement Config struct with basic fields (repos, github token from env)
- [ ] Implement `gru init` command that:
  - Creates ~/.gru directories
  - Generates template config.yaml
  - Validates GRU_GITHUB_TOKEN env var

**Validation:**
```bash
cargo run -- init
ls ~/.gru/  # Should show dirs
cat ~/.gru/config.yaml  # Should show template
```

### Milestone 2: GitHub Integration (Day 1-2)

**Goal:** Can query GitHub and post comments.

**Implementation:**
- [ ] Implement GitHubClient with octocrab
- [ ] Implement find_ready_issues() 
- [ ] Implement add_label() and remove_label()
- [ ] Implement post_comment()
- [ ] Simple test: fetch issues from real repo

**Validation:**
```bash
# Create test issue with ready-for-minion label
# Run test that fetches it
cargo test test_github_integration -- --ignored
```

### Milestone 3: Git Operations (Day 2)

**Goal:** Can create worktrees.

**Implementation:**
- [ ] Implement GitRepo with bare clone path
- [ ] Implement ensure_cloned() using git CLI
- [ ] Implement create_worktree() using git CLI
- [ ] Test: clone repo and create worktree

**Validation:**
```bash
# Manual test
cargo run -- test-worktree owner/repo
ls ~/.gru/work/owner/repo/test/  # Should see worktree
```

### Milestone 4: Tmux Session Management (Day 2)

**Goal:** Can spawn and attach to tmux sessions.

**Implementation:**
- [ ] Implement TmuxSession wrapper
- [ ] Implement create(), send_keys(), attach()
- [ ] Test: create session, send echo command, verify output

**Validation:**
```bash
# Manual test
cargo run -- test-tmux
tmux attach -t gru-test  # Should see session
```

### Milestone 5: Basic Minion (Day 3)

**Goal:** Can spawn a Minion that runs Claude Code.

**Implementation:**
- [ ] Implement Minion struct (minimal fields)
- [ ] Implement claim() method that:
  - Adds in-progress label
  - Posts claim comment
  - Creates worktree
  - Spawns tmux with Claude Code
- [ ] Hardcode simple initial prompt
- [ ] No automatic cleanup yet (manual)

**Validation:**
```bash
# Create test issue
# Run: cargo run -- test-claim owner/repo 123
# Should see:
# - Label added
# - Comment posted
# - Tmux session created
# - Can attach with: gru attach M001
```

### Milestone 6: Simple Poller (Day 3-4)

**Goal:** Automatically claims issues.

**Implementation:**
- [ ] Implement Poller that runs every 30s
- [ ] Find ready issues
- [ ] If no active Minion, claim first issue
- [ ] Spawn Minion
- [ ] Keep running until Ctrl+C

**Validation:**
```bash
# Create test issue with ready-for-minion
cargo run -- lab
# Within 30s, should claim issue
# Verify with: tmux ls
```

### Milestone 7: Attach Command (Day 4)

**Goal:** Users can watch Minions work.

**Implementation:**
- [ ] Implement `gru attach <minion-id>`
- [ ] List tmux sessions with gru-minion- prefix
- [ ] Attach to specified session (read-only by default)
- [ ] Add --interactive flag

**Validation:**
```bash
# With running Minion
gru attach M001
# Should see Claude Code output
# Ctrl+D to detach
```

### Milestone 8: Manual Completion (Day 4-5)

**Goal:** Can manually tell Minion it's done.

**Implementation:**
- [ ] Implement `gru minions list`
- [ ] Implement `gru minions complete <id>`
  - Kills tmux session
  - Removes worktree
  - Posts completion comment
  - Adds minion:done label

**Validation:**
```bash
gru minions list  # Show active
gru minions complete M001
# Session killed, worktree removed
```

### Milestone 9: Integration Test (Day 5)

**Goal:** End-to-end workflow works.

**Test scenario:**
1. Create test issue: "Add hello() function to README.md"
2. Label: ready-for-minion
3. Start Lab: `gru lab`
4. Verify within 30s:
   - Issue claimed (label changed)
   - Comment posted
   - Tmux session running
5. Attach: `gru attach M001`
6. Watch Minion work (or let it run)
7. When done, complete: `gru minions complete M001`
8. Verify cleanup

**Success criteria:**
✅ Lab polls and claims automatically  
✅ Worktree created with correct branch  
✅ Claude Code running in tmux  
✅ Can attach and observe  
✅ GitHub updated correctly  
✅ Manual completion works  

## What's Missing (Intentionally)

These will become issues for Gru to implement on itself:

- ❌ Multiple concurrent Minions (slots > 1)
- ❌ Automatic PR creation
- ❌ CI monitoring
- ❌ Post-PR review handling
- ❌ Retry logic and error recovery
- ❌ Sophisticated event logging
- ❌ Archive system
- ❌ Metrics/observability
- ❌ Configuration validation
- ❌ Proper logging setup

## Simplified Code Structure

```
gru/
├── Cargo.toml
└── src/
    ├── main.rs           # CLI entry point
    ├── config.rs         # Config loading (simplified)
    ├── github.rs         # GitHub client (4-5 methods)
    ├── git.rs            # Git operations (2-3 methods)
    ├── tmux.rs           # Tmux wrapper (3-4 methods)
    ├── minion.rs         # Minion struct + claim()
    ├── poller.rs         # Simple poll loop
    └── cli/
        ├── init.rs       # gru init
        ├── lab.rs        # gru lab
        ├── attach.rs     # gru attach
        └── minions.rs    # gru minions (list, complete)
```

**Estimated LOC: ~800-1000 lines total**

## Minimal Dependencies

```toml
[dependencies]
clap = { version = "4.5", features = ["derive"] }
tokio = { version = "1.40", features = ["full"] }
octocrab = "0.40"
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
anyhow = "1.0"
chrono = { version = "0.4", features = ["serde"] }
```

No tracing, no metrics, no thiserror, no regex - keep it minimal.

## Simplified Configuration

```yaml
# ~/.gru/config.yaml
repos:
  - sspalding/gru

# Everything else hardcoded in 0.1:
# - Single slot
# - 30s poll interval
# - No retries
```

## Simplified Minion Prompt

```rust
fn generate_initial_prompt(&self, issue_title: &str, issue_body: &str) -> String {
    format!(
r#"You are working on issue #{} in {}.

Issue: {}

{}

Instructions:
1. Explore the codebase to understand the context
2. Implement the requested changes
3. Commit your work with message: [minion:{}] <description>
4. When done, say "MINION_COMPLETE" and stop

Start now.
"#,
        self.issue_number,
        self.repo,
        issue_title,
        issue_body,
        self.id
    )
}
```

## Next Steps After Bootstrap

Once Gru 0.1 works, create issues for Gru to implement:

1. Issue #1: Add multiple slot support
2. Issue #2: Implement automatic PR creation
3. Issue #3: Add CI monitoring
4. Issue #4: Implement retry logic
5. Issue #5: Add event logging system
6. Issue #6: Implement archive system
7. ...and so on

Label them all `ready-for-minion` and let Gru build itself!

## Time Estimate

**Optimistic:** 3 days (if everything goes smoothly)  
**Realistic:** 5 days (accounting for debugging, tweaks)  
**Pessimistic:** 7 days (if significant API issues or rework needed)

**Critical path:** GitHub integration → Git/tmux → Basic Minion → Poller

## Success Definition

Gru 0.1 is successful when:

1. You can start `gru lab` and walk away
2. It claims an issue from GitHub within 30 seconds
3. You can `gru attach` and see Claude Code working
4. The Minion makes commits to its branch
5. You can manually complete when done
6. It's stable enough to run for hours without crashing

At that point, **Gru can start implementing its own features!**
