# Gru V0: Manual Kickoff Bootstrap

## Purpose

The absolute minimal implementation - manually kick off a Minion on a specific issue. No polling, no automation. Just prove the core mechanics work.

**Target: Working in 2-3 days**

## What Gru V0 Does

```bash
gru fix https://github.com/owner/repo/issues/123
```

This command:
1. Fetches issue details from GitHub
2. Creates worktree
3. Spawns Claude Code in tmux with issue context
4. Posts claim comment to GitHub
5. Done - Minion works autonomously from there

**What we defer to V0.1:**
- ❌ Polling and auto-claiming
- ❌ Multiple concurrent Minions
- ❌ Automatic cleanup
- ❌ Sophisticated prompts

**What we defer to V1:**
- Everything else (CI monitoring, retry, PR automation, etc.)

---

## Implementation Plan

### Milestone 1: Project Setup (2 hours)

**Goal:** Rust project with dependencies.

```bash
cargo new --name gru
cd gru
```

**Edit Cargo.toml:**
```toml
[package]
name = "gru"
version = "0.0.1"
edition = "2021"

[dependencies]
clap = { version = "4.5", features = ["derive"] }
tokio = { version = "1.40", features = ["full"] }
octocrab = "0.40"
anyhow = "1.0"
regex = "1.10"
```

**Create structure:**
```bash
mkdir -p src/cli
touch src/github.rs
touch src/git.rs
touch src/tmux.rs
touch src/minion.rs
touch src/cli/fix.rs
```

**Validation:**
```bash
cargo build  # Should compile
```

---

### Milestone 2: Parse Issue URL (1 hour)

**Goal:** Extract owner, repo, issue number from URL.

**src/main.rs:**
```rust
use clap::{Parser, Subcommand};
use anyhow::Result;

mod github;
mod git;
mod tmux;
mod minion;
mod cli;

#[derive(Parser)]
#[command(name = "gru", version, about = "Kick off AI Minions to fix issues")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Kick off a Minion to fix an issue
    Fix {
        /// GitHub issue URL
        issue_url: String,
    },
    /// Attach to a running Minion
    Attach {
        minion_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Fix { issue_url } => cli::fix::run(issue_url).await,
        Commands::Attach { minion_id } => cli::attach::run(minion_id).await,
    }
}
```

**src/cli/fix.rs:**
```rust
use anyhow::{bail, Context, Result};
use regex::Regex;

pub async fn run(issue_url: String) -> Result<()> {
    let (owner, repo, issue_number) = parse_issue_url(&issue_url)?;
    
    println!("🤖 Spawning Minion for {}/{}#{}", owner, repo, issue_number);
    
    // TODO: Rest of implementation
    
    Ok(())
}

fn parse_issue_url(url: &str) -> Result<(String, String, u64)> {
    let re = Regex::new(r"github\.com/([^/]+)/([^/]+)/issues/(\d+)").unwrap();
    
    let caps = re.captures(url)
        .context("Invalid GitHub issue URL. Expected: https://github.com/owner/repo/issues/123")?;
    
    let owner = caps.get(1).unwrap().as_str().to_string();
    let repo = caps.get(2).unwrap().as_str().to_string();
    let issue_number = caps.get(3).unwrap().as_str().parse()?;
    
    Ok((owner, repo, issue_number))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_issue_url() {
        let url = "https://github.com/owner/repo/issues/123";
        let (owner, repo, num) = parse_issue_url(url).unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
        assert_eq!(num, 123);
    }
}
```

**Validation:**
```bash
cargo test
cargo run -- fix https://github.com/test/test/issues/123
# Should print: Spawning Minion for test/test#123
```

---

### Milestone 3: Fetch Issue from GitHub (2 hours)

**Goal:** Get issue title and body using GitHub API.

**src/github.rs:**
```rust
use anyhow::{Context, Result};
use octocrab::Octocrab;

pub struct GitHubClient {
    client: Octocrab,
}

pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
}

impl GitHubClient {
    pub fn new() -> Result<Self> {
        let token = std::env::var("GRU_GITHUB_TOKEN")
            .context("GRU_GITHUB_TOKEN not set")?;
        
        let client = Octocrab::builder()
            .personal_token(token)
            .build()?;
        
        Ok(Self { client })
    }
    
    pub async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<Issue> {
        let issue = self.client
            .issues(owner, repo)
            .get(number)
            .await?;
        
        Ok(Issue {
            number: issue.number,
            title: issue.title,
            body: issue.body.unwrap_or_default(),
        })
    }
    
    pub async fn post_comment(&self, owner: &str, repo: &str, issue: u64, body: &str) -> Result<()> {
        self.client
            .issues(owner, repo)
            .create_comment(issue, body)
            .await?;
        Ok(())
    }
}
```

**Update src/cli/fix.rs:**
```rust
use crate::github::GitHubClient;

pub async fn run(issue_url: String) -> Result<()> {
    let (owner, repo, issue_number) = parse_issue_url(&issue_url)?;
    
    println!("🤖 Spawning Minion for {}/{}#{}", owner, repo, issue_number);
    
    let github = GitHubClient::new()?;
    let issue = github.get_issue(&owner, &repo, issue_number).await?;
    
    println!("📋 Issue: {}", issue.title);
    println!("{}", issue.body);
    
    Ok(())
}
```

**Validation:**
```bash
export GRU_GITHUB_TOKEN="ghp_your_token"
cargo run -- fix https://github.com/owner/repo/issues/123
# Should print issue title and body
```

---

### Milestone 4: Git Worktree Setup (3 hours)

**Goal:** Clone repo and create worktree.

**src/git.rs:**
```rust
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitRepo {
    pub owner: String,
    pub repo: String,
    pub bare_path: PathBuf,
}

impl GitRepo {
    pub fn new(owner: &str, repo: &str) -> Self {
        let home = std::env::var("HOME").unwrap();
        let bare_path = PathBuf::from(home)
            .join(".gru/repos")
            .join(owner)
            .join(format!("{}.git", repo));
        
        Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            bare_path,
        }
    }
    
    pub async fn ensure_cloned(&self) -> Result<()> {
        if self.bare_path.exists() {
            self.fetch()?;
        } else {
            self.clone()?;
        }
        Ok(())
    }
    
    fn clone(&self) -> Result<()> {
        std::fs::create_dir_all(self.bare_path.parent().unwrap())?;
        
        let token = std::env::var("GRU_GITHUB_TOKEN")?;
        let url = format!(
            "https://{}@github.com/{}/{}.git",
            token, self.owner, self.repo
        );
        
        let output = Command::new("git")
            .args(["clone", "--bare", &url, self.bare_path.to_str().unwrap()])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        Ok(())
    }
    
    fn fetch(&self) -> Result<()> {
        let output = Command::new("git")
            .args(["--git-dir", self.bare_path.to_str().unwrap(), "fetch", "--all"])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("git fetch failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        Ok(())
    }
    
    pub fn create_worktree(&self, branch_name: &str, minion_id: &str) -> Result<PathBuf> {
        let home = std::env::var("HOME")?;
        let worktree_path = PathBuf::from(home)
            .join(".gru/work")
            .join(&self.owner)
            .join(&self.repo)
            .join(minion_id);
        
        std::fs::create_dir_all(worktree_path.parent().unwrap())?;
        
        let output = Command::new("git")
            .args([
                "--git-dir", self.bare_path.to_str().unwrap(),
                "worktree", "add",
                "-b", branch_name,
                worktree_path.to_str().unwrap(),
                "origin/main",  // TODO: Get default branch
            ])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("git worktree add failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        Ok(worktree_path)
    }
}
```

**Validation:**
```bash
cargo run -- fix https://github.com/owner/repo/issues/123
# Should clone repo to ~/.gru/repos/owner/repo.git
# Should create worktree at ~/.gru/work/owner/repo/M001
```

---

### Milestone 5: Tmux + Claude Code (3 hours)

**Goal:** Spawn Claude Code session in tmux.

**src/tmux.rs:**
```rust
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct TmuxSession {
    pub name: String,
}

impl TmuxSession {
    pub fn new(name: String) -> Self {
        Self { name }
    }
    
    pub fn create(&self, working_dir: &Path) -> Result<()> {
        let output = Command::new("tmux")
            .args([
                "new-session", "-d",
                "-s", &self.name,
                "-c", working_dir.to_str().unwrap(),
            ])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("tmux new-session failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        Ok(())
    }
    
    pub fn send_keys(&self, keys: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, keys, "Enter"])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("tmux send-keys failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        Ok(())
    }
    
    pub fn attach(&self) -> Result<()> {
        let status = Command::new("tmux")
            .args(["attach-session", "-t", &self.name])
            .status()?;
        
        if !status.success() {
            anyhow::bail!("tmux attach failed");
        }
        
        Ok(())
    }
}
```

**src/minion.rs:**
```rust
use anyhow::Result;
use std::path::PathBuf;

pub struct Minion {
    pub id: String,
    pub owner: String,
    pub repo: String,
    pub issue_number: u64,
    pub worktree_path: PathBuf,
    pub tmux_session: String,
}

impl Minion {
    pub fn new(owner: String, repo: String, issue_number: u64) -> Self {
        let id = generate_id();
        let tmux_session = format!("gru-{}", id);
        
        Self {
            id,
            owner,
            repo,
            issue_number,
            worktree_path: PathBuf::new(),  // Set later
            tmux_session,
        }
    }
    
    pub fn generate_prompt(&self, issue_title: &str, issue_body: &str) -> String {
        format!(
r#"You are a Minion fixing issue #{} in {}/{}.

## Issue
{}

## Description
{}

## Your Mission
1. Explore the codebase to understand context
2. Implement the fix or feature
3. Write or update tests as needed
4. Commit with message: [minion:{}] <description>
5. When done, say "DONE" and stop

## Guidelines
- Make focused, incremental commits
- Test your changes
- Be thorough but concise

Start now.
"#,
            self.issue_number,
            self.owner,
            self.repo,
            issue_title,
            issue_body,
            self.id
        )
    }
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("M{:03X}", (timestamp % 4096))  // M000 to MFFF
}
```

---

### Milestone 6: Wire It All Together (2 hours)

**Goal:** Complete `gru fix` command.

**Update src/cli/fix.rs:**
```rust
use anyhow::Result;
use crate::{github::GitHubClient, git::GitRepo, tmux::TmuxSession, minion::Minion};

pub async fn run(issue_url: String) -> Result<()> {
    let (owner, repo, issue_number) = parse_issue_url(&issue_url)?;
    
    println!("🤖 Spawning Minion for {}/{}#{}", owner, repo, issue_number);
    
    // Fetch issue
    let github = GitHubClient::new()?;
    let issue = github.get_issue(&owner, &repo, issue_number).await?;
    println!("📋 {}", issue.title);
    
    // Setup git
    let git_repo = GitRepo::new(&owner, &repo);
    println!("📦 Cloning repository...");
    git_repo.ensure_cloned().await?;
    
    // Create Minion
    let mut minion = Minion::new(owner.clone(), repo.clone(), issue_number);
    let branch_name = format!("fix/issue{}-{}", issue_number, minion.id);
    
    println!("🌿 Creating worktree: {}", branch_name);
    minion.worktree_path = git_repo.create_worktree(&branch_name, &minion.id)?;
    
    // Post claim comment
    let claim_msg = format!("🤖 Minion {} is working on this issue.", minion.id);
    github.post_comment(&owner, &repo, issue_number, &claim_msg).await?;
    println!("💬 Posted claim comment");
    
    // Create tmux session
    let tmux = TmuxSession::new(minion.tmux_session.clone());
    tmux.create(&minion.worktree_path)?;
    println!("📺 Created tmux session: {}", minion.tmux_session);
    
    // Start Claude Code
    let prompt = minion.generate_prompt(&issue.title, &issue.body);
    let prompt_file = minion.worktree_path.join(".gru-prompt.txt");
    std::fs::write(&prompt_file, &prompt)?;
    
    // TODO: Adjust this command based on how you invoke Claude Code
    let claude_cmd = format!("cat .gru-prompt.txt && claude");
    tmux.send_keys(&claude_cmd)?;
    
    println!("✅ Minion {} spawned!", minion.id);
    println!("\nAttach with: gru attach {}", minion.id);
    println!("Or: tmux attach -t {}", minion.tmux_session);
    
    Ok(())
}

// ... parse_issue_url stays same
```

**Create src/cli/attach.rs:**
```rust
use anyhow::Result;
use crate::tmux::TmuxSession;

pub async fn run(minion_id: String) -> Result<()> {
    let session_name = format!("gru-{}", minion_id);
    let tmux = TmuxSession::new(session_name);
    
    println!("🔌 Attaching to Minion {}...", minion_id);
    tmux.attach()?;
    
    Ok(())
}
```

**Create src/cli/mod.rs:**
```rust
pub mod fix;
pub mod attach;
```

---

### Milestone 7: Test End-to-End (1 hour)

**Complete workflow:**

1. **Setup:**
   ```bash
   export GRU_GITHUB_TOKEN="ghp_..."
   export ANTHROPIC_API_KEY="sk-ant-..."
   cargo build --release
   ```

2. **Create test issue:**
   - Go to GitHub
   - Create issue: "Add hello world function"
   - Get URL

3. **Kick off Minion:**
   ```bash
   ./target/release/gru fix https://github.com/owner/repo/issues/123
   ```
   
   Expected output:
   ```
   🤖 Spawning Minion for owner/repo#123
   📋 Add hello world function
   📦 Cloning repository...
   🌿 Creating worktree: fix/issue123-M042
   💬 Posted claim comment
   📺 Created tmux session: gru-M042
   ✅ Minion M042 spawned!
   
   Attach with: gru attach M042
   Or: tmux attach -t gru-M042
   ```

4. **Attach and observe:**
   ```bash
   ./target/release/gru attach M042
   # Should see Claude Code working
   # Ctrl+B then D to detach
   ```

5. **Verify on GitHub:**
   - Issue has comment: "Minion M042 is working on this"
   - Branch exists (if Minion pushed)

**Success criteria:**
✅ `gru fix URL` spawns Minion  
✅ GitHub comment posted  
✅ Worktree created  
✅ Tmux session running  
✅ `gru attach` works  
✅ Claude Code makes commits  

---

## What V0 Proves

After completing V0, you've validated:

1. ✅ GitHub API integration works
2. ✅ Git worktree mechanics work  
3. ✅ Tmux session management works
4. ✅ Claude Code can be spawned and controlled
5. ✅ The core "Minion" concept is viable

## Next: V0.1 Adds Polling

Once V0 works, V0.1 adds:

1. **Config file** - `~/.gru/config.yaml` with repos list
2. **Poller** - Check repos every 30s for `ready-for-minion` label
3. **Auto-claim** - Spawn Minion automatically when issue found
4. **Cleanup command** - `gru minions complete M042`

This makes V0.1 the "Gru 0.1" described in the original bootstrap plan.

## Simplified Architecture

```
┌─────────────────────────────────────────┐
│  $ gru fix github.com/.../issues/123    │
└─────────────────┬───────────────────────┘
                  │
                  ▼
         ┌────────────────┐
         │   Parse URL    │
         └────────┬───────┘
                  │
                  ▼
         ┌────────────────┐
         │  Fetch Issue   │ ◄──── GitHub API
         └────────┬───────┘
                  │
                  ▼
         ┌────────────────┐
         │  Clone Repo    │ ◄──── git clone --bare
         └────────┬───────┘
                  │
                  ▼
         ┌────────────────┐
         │ Create Worktree│ ◄──── git worktree add
         └────────┬───────┘
                  │
                  ▼
         ┌────────────────┐
         │ Spawn Tmux     │ ◄──── tmux new-session
         └────────┬───────┘
                  │
                  ▼
         ┌────────────────┐
         │ Start Claude   │ ◄──── claude (in tmux)
         └────────┬───────┘
                  │
                  ▼
              ┌───────┐
              │ Done! │
              └───────┘
```

**Total code: ~500-600 lines**

## Time Breakdown

- Milestone 1 (Setup): 2 hours
- Milestone 2 (Parse URL): 1 hour  
- Milestone 3 (GitHub): 2 hours
- Milestone 4 (Git): 3 hours
- Milestone 5 (Tmux): 3 hours
- Milestone 6 (Integration): 2 hours
- Milestone 7 (Testing): 1 hour

**Total: ~14 hours = 2 solid days**

## Success Definition

V0 succeeds when you can:

```bash
gru fix https://github.com/sspalding/gru/issues/1
# Wait 10 seconds
gru attach M042
# See Claude Code implementing the fix
# Detach and let it run
# Check GitHub - see commits pushed
```

That's it! No fancy features, just the core loop working.

---

**Ready to build?** Start with Milestone 1!
