# Gru V1 Execution Plan

This ExecPlan is a living document maintained in accordance with [PLANS.md](PLANS.md). The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

## Purpose / Big Picture

After implementing this plan, you will have a working single-Lab Gru instance that autonomously claims GitHub issues labeled `ready-for-minion`, spawns Claude Code sessions (Minions) to implement solutions, opens pull requests, and monitors them through code review and CI checks. Users can attach to running Minions to observe their work in real-time.

**What you can do after this change:**
1. Start the Lab with `gru lab` and watch it automatically claim issues from configured repositories
2. See Minions working autonomously in isolated tmux sessions, creating branches, committing code, and opening PRs
3. Attach to any active Minion with `gru attach M001` to observe or interact with its Claude Code session
4. Watch Minions respond to CI failures, code review feedback, and merge conflicts without human intervention
5. View archived logs and metrics for completed issues in `~/.gru/archive/`

**Observable validation:**
- Run `gru init` to set up the environment successfully
- Run `gru lab --dry-run` to validate configuration
- Create a test issue with label `ready-for-minion` and watch the Lab claim it within 30 seconds
- Observe a Minion create a branch, commit code, open a draft PR, and convert it to ready-for-review
- Attach to a running Minion and see live output from its Claude Code session
- See completed work archived with structured logs, metrics, and event timelines

## Progress

- [ ] (2025-11-30) Project structure and Cargo setup
- [ ] (2025-11-30) Configuration loading with YAML and environment validation
- [ ] (2025-11-30) GitHub API integration - authentication and basic queries
- [ ] (2025-11-30) Repository management - bare clones and worktree creation
- [ ] (2025-11-30) Minion ID generation and state tracking
- [ ] (2025-11-30) Tmux session management for Claude Code
- [ ] (2025-11-30) Poller - GitHub issue and PR monitoring
- [ ] (2025-11-30) Scheduler - claim issues and manage slot allocation
- [ ] (2025-11-30) Minion lifecycle - claim, work, PR, monitor, complete
- [ ] (2025-11-30) Attach manager - read-only and interactive sessions
- [ ] (2025-11-30) Event logging - YAML comments and local JSONL files
- [ ] (2025-11-30) CLI commands - init, lab, attach, minions list
- [ ] (2025-11-30) Observability - structured logging and Prometheus metrics
- [ ] (2025-11-30) End-to-end integration test with live GitHub repository
- [ ] (2025-11-30) Documentation and user onboarding materials

## Surprises & Discoveries

(To be filled during implementation)

## Decision Log

- **Decision:** Use Rust for implementation
  **Rationale:** Primary developer familiarity, type safety benefits for state machine complexity, excellent CLI tooling with clap/tokio, future-proof for Zellij integration
  **Date:** 2025-11-30

- **Decision:** Use tmux for Minion session management (not custom multiplexing)
  **Rationale:** Battle-tested reliability, built-in multiplexing and scrollback, session persistence across Lab restarts, familiar to developers, can swap implementation later without changing API
  **Date:** 2025-11-30

- **Decision:** No SQLite database - in-memory state with file-based cursors
  **Rationale:** Simplifies V1, eliminates database complexity, recovery from GitHub is authoritative, easy to reason about
  **Date:** 2025-11-30

- **Decision:** Simplified 3-state label system
  **Rationale:** Minimize complexity for single-Lab scenario, detailed states tracked in comment events not labels, easier to visualize in GitHub UI
  **Date:** 2025-11-30

- **Decision:** Tokens only in environment variables (GRU_GITHUB_TOKEN, ANTHROPIC_API_KEY)
  **Rationale:** Never store secrets in config files, follows security best practices, prevents accidental commits
  **Date:** 2025-11-30

## Outcomes & Retrospective

(To be filled at major milestones and completion)

---

## Context and Orientation

You are implementing **Gru**, a local-first orchestrator that runs LLM-powered agents (Minions) to work on GitHub issues. This is the V1 single-Lab implementation - one binary, one process, no distributed coordination.

**Key architectural concepts:**

1. **Lab** - The main Rust process (`gru lab`) that orchestrates everything. Polls GitHub, manages Minions, handles API calls.

2. **Minion** - A Claude Code session running in a tmux session. Each Minion has:
   - Unique ID (base36: M001, M002, etc.)
   - Dedicated git worktree (`~/.gru/work/owner/repo/M042/`)
   - Tmux session name (`gru-minion-M042`)
   - One-to-one mapping: Minion = Claude Code session in tmux

3. **GitHub as database** - No SQLite. GitHub stores state via:
   - Labels (ready-for-minion → in-progress → minion:done/failed)
   - Comments (YAML frontmatter for structured events)
   - Timeline API (complete audit trail)

4. **File structure:**
   ```
   ~/.gru/
     repos/<owner>/<repo>.git     # Bare repository mirrors
     work/<owner>/<repo>/<ID>/    # Active worktrees (one per Minion)
     archive/<ID>/                # Completed Minion artifacts
     state/
       next_id.txt                # Monotonic counter for Minion IDs
       cursors.json               # GitHub timeline cursors
     logs/gru.log                 # Lab process logs
     config.yaml                  # Non-sensitive configuration
   ```

5. **Lifecycle flow:**
   - Poller finds issue with `ready-for-minion` label
   - Scheduler claims issue if slot available, adds `in-progress` label
   - Lab creates worktree and spawns Claude Code in tmux
   - Claude Code works autonomously: reads code, implements changes, commits, pushes
   - Lab monitors output, posts progress comments, creates draft PR
   - Minion converts PR to ready-for-review when done
   - Lab monitors PR for review comments and CI failures
   - On merge, Lab marks `minion:done`, archives logs, cleans up

**Key files you'll create:**
- `Cargo.toml` - Project dependencies
- `src/main.rs` - CLI entry point using clap
- `src/config.rs` - Configuration loading and validation
- `src/github.rs` - GitHub API client using octocrab
- `src/minion.rs` - Minion state machine and lifecycle
- `src/tmux.rs` - Tmux session management wrapper
- `src/lab.rs` - Lab orchestrator (poller, scheduler)
- `src/attach.rs` - Attach session manager
- `src/cli/*.rs` - Command implementations (init, lab, attach, etc.)

**External dependencies:**
- GitHub REST and GraphQL APIs
- tmux (must be installed on system)
- git (for worktree operations)
- Claude Code (assumed available via `claude` command or API)

---

## Plan of Work

We'll implement Gru in these phases:

### Phase 1: Foundation (Project Setup & Configuration)
Set up the Rust project structure, define dependencies, and implement configuration loading with validation. Users will be able to run `gru init` to set up their environment and `gru lab --dry-run` to validate configuration.

### Phase 2: GitHub Integration
Implement the GitHub API client with authentication, issue queries, label operations, comment posting, and PR creation. This provides the foundation for all Lab-GitHub interactions.

### Phase 3: Repository & Worktree Management
Implement git operations for bare repository mirrors and worktree lifecycle management. This enables isolated workspaces for each Minion.

### Phase 4: Minion Core & Tmux Integration
Implement Minion ID generation, state tracking, and tmux session management for running Claude Code instances. This is the heart of the agent system.

### Phase 5: Poller & Scheduler
Implement the polling loop to find ready issues and the scheduler to claim them and allocate Minion slots. This makes the Lab autonomous.

### Phase 6: Minion Lifecycle & Event Logging
Implement the full Minion lifecycle from claim through PR submission, post-PR monitoring, and completion. Add structured event logging to GitHub comments and local JSONL files.

### Phase 7: Attach Sessions
Implement user attach functionality for observing and interacting with running Minions via tmux.

### Phase 8: CLI & Observability
Implement all CLI commands (init, lab, attach, minions list) and add structured logging with Prometheus metrics.

### Phase 9: Integration & Validation
End-to-end testing with a real GitHub repository, validation of all lifecycle stages, and documentation.

---

## Concrete Steps

### Phase 1: Foundation

**Step 1.1: Create Rust project**

```bash
cd ~/prj/gru
cargo init --name gru
```

**Step 1.2: Add dependencies to Cargo.toml**

Edit `Cargo.toml`:

```toml
[package]
name = "gru"
version = "0.1.0"
edition = "2021"

[dependencies]
# CLI framework
clap = { version = "4.5", features = ["derive", "cargo", "env"] }

# Async runtime
tokio = { version = "1.40", features = ["full"] }

# GitHub API
octocrab = "0.40"

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
serde_json = "1.0"

# Error handling
anyhow = "1.0"
thiserror = "1.0"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# Time
chrono = { version = "0.4", features = ["serde"] }

# Utilities
regex = "1.10"
lazy_static = "1.5"
base36 = "0.0.1"
rand = "0.8"

# HTTP client (for Claude API if needed)
reqwest = { version = "0.12", features = ["json"] }
```

**Step 1.3: Create module structure**

```bash
mkdir -p src/cli
touch src/config.rs
touch src/github.rs
touch src/minion.rs
touch src/tmux.rs
touch src/lab.rs
touch src/attach.rs
touch src/cli/mod.rs
touch src/cli/init.rs
touch src/cli/lab.rs
touch src/cli/attach.rs
touch src/cli/minions.rs
```

**Step 1.4: Implement configuration types**

Create `src/config.rs`:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub repos: Vec<String>,
    pub lab: LabConfig,
    pub git: GitConfig,
    pub observability: ObservabilityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabConfig {
    pub slots: usize,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    pub max_ci_retries: usize,
    pub archive_retention_days: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub user_name: String,
    pub user_email: String,
    pub branch_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_file: PathBuf,
    pub metrics_enabled: bool,
    pub metrics_port: u16,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;
        let content = std::fs::read_to_string(&config_path)
            .context(format!("Failed to read config file: {:?}", config_path))?;
        
        serde_yaml::from_str(&content)
            .context("Failed to parse config.yaml")
    }
    
    pub fn config_path() -> Result<PathBuf> {
        let home = std::env::var("HOME")
            .context("HOME environment variable not set")?;
        Ok(PathBuf::from(home).join(".gru/config.yaml"))
    }
    
    pub fn gru_root() -> Result<PathBuf> {
        let home = std::env::var("HOME")
            .context("HOME environment variable not set")?;
        Ok(PathBuf::from(home).join(".gru"))
    }
    
    pub fn validate(&self) -> Result<()> {
        // Check required environment variables
        std::env::var("GRU_GITHUB_TOKEN")
            .context("GRU_GITHUB_TOKEN environment variable not set")?;
        
        std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY environment variable not set")?;
        
        // Validate repos format
        for repo in &self.repos {
            if !repo.contains('/') {
                anyhow::bail!("Invalid repo format '{}', expected 'owner/repo'", repo);
            }
        }
        
        // Validate slots
        if self.lab.slots == 0 {
            anyhow::bail!("lab.slots must be at least 1");
        }
        
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: vec![],
            lab: LabConfig {
                slots: 2,
                poll_interval: Duration::from_secs(30),
                max_ci_retries: 10,
                archive_retention_days: 0,
            },
            git: GitConfig {
                user_name: "Gru Minion".to_string(),
                user_email: "minion@gru.local".to_string(),
                branch_prefix: "minion/issue-".to_string(),
            },
            observability: ObservabilityConfig {
                log_level: "info".to_string(),
                log_file: PathBuf::from("~/.gru/logs/gru.log"),
                metrics_enabled: true,
                metrics_port: 9090,
            },
        }
    }
}
```

**Step 1.5: Implement `gru init` command**

Create `src/cli/init.rs`:

```rust
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use crate::config::Config;

pub async fn run() -> Result<()> {
    println!("🤖 Gru Setup Wizard\n");
    
    // Check environment variables
    println!("Checking environment variables...");
    let github_token = std::env::var("GRU_GITHUB_TOKEN")
        .context("GRU_GITHUB_TOKEN not found. Set with: export GRU_GITHUB_TOKEN=\"ghp_...\"")?;
    println!("✓ GRU_GITHUB_TOKEN found");
    
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY not found. Set with: export ANTHROPIC_API_KEY=\"sk-ant-...\"")?;
    println!("✓ ANTHROPIC_API_KEY found\n");
    
    // Create directory structure
    println!("Creating directory structure...");
    let root = Config::gru_root()?;
    
    let dirs = [
        root.join("repos"),
        root.join("work"),
        root.join("archive"),
        root.join("state"),
        root.join("logs"),
    ];
    
    for dir in &dirs {
        fs::create_dir_all(dir)
            .context(format!("Failed to create directory: {:?}", dir))?;
        println!("✓ Created {}", dir.display());
    }
    
    // Generate config file if it doesn't exist
    let config_path = Config::config_path()?;
    if config_path.exists() {
        println!("\n⚠ Config file already exists: {}", config_path.display());
    } else {
        let default_config = Config::default();
        let yaml = serde_yaml::to_string(&default_config)?;
        
        let yaml_with_comments = format!(
r#"# Gru Configuration
# Add your repositories here (format: owner/repo)
{}
"#, yaml);
        
        fs::write(&config_path, yaml_with_comments)?;
        println!("\n✓ Generated config file: {}", config_path.display());
        println!("  Edit this file to configure repositories and settings.");
    }
    
    // Initialize next_id counter
    let next_id_path = root.join("state/next_id.txt");
    if !next_id_path.exists() {
        fs::write(&next_id_path, "1")?;
        println!("✓ Initialized Minion ID counter");
    }
    
    // Initialize cursors file
    let cursors_path = root.join("state/cursors.json");
    if !cursors_path.exists() {
        fs::write(&cursors_path, "{}")?;
        println!("✓ Initialized timeline cursors");
    }
    
    println!("\n✓ Setup complete! Edit ~/.gru/config.yaml and run 'gru lab'");
    
    Ok(())
}
```

**Step 1.6: Create CLI entry point**

Edit `src/main.rs`:

```rust
use clap::{Parser, Subcommand};
use anyhow::Result;

mod config;
mod cli;

#[derive(Parser)]
#[command(name = "gru")]
#[command(about = "Local-first LLM agent orchestrator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize Gru environment
    Init,
    /// Run the Lab (poll for issues and spawn Minions)
    Lab {
        /// Validate configuration without starting
        #[arg(long)]
        dry_run: bool,
    },
    /// Attach to a running Minion
    Attach {
        /// Minion ID (e.g., M042)
        minion_id: String,
        /// Interactive mode (can send input)
        #[arg(short, long)]
        interactive: bool,
    },
    /// List active Minions
    Minions {
        #[command(subcommand)]
        command: Option<MinionsCommands>,
    },
}

#[derive(Subcommand)]
enum MinionsCommands {
    /// List all active Minions
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Init => cli::init::run().await,
        Commands::Lab { dry_run } => {
            if dry_run {
                println!("Dry run mode - validating configuration...");
                let config = config::Config::load()?;
                config.validate()?;
                println!("✓ Configuration is valid");
                Ok(())
            } else {
                // TODO: Implement lab run
                println!("Lab not yet implemented");
                Ok(())
            }
        }
        Commands::Attach { minion_id, interactive } => {
            // TODO: Implement attach
            println!("Attach not yet implemented");
            Ok(())
        }
        Commands::Minions { command } => {
            match command {
                Some(MinionsCommands::List) | None => {
                    // TODO: Implement minions list
                    println!("Minions list not yet implemented");
                    Ok(())
                }
            }
        }
    }
}
```

Create `src/cli/mod.rs`:

```rust
pub mod init;
```

**Step 1.7: Test Phase 1**

```bash
# Build
cargo build

# Test init
./target/debug/gru init

# Should see:
# 🤖 Gru Setup Wizard
# (environment check and directory creation)

# Verify structure
ls -la ~/.gru/
# Should see: repos/ work/ archive/ state/ logs/ config.yaml

# Test dry-run (will fail until config edited)
export GRU_GITHUB_TOKEN="ghp_test123"
export ANTHROPIC_API_KEY="sk-ant-test123"

# Edit config to add a test repo
cat >> ~/.gru/config.yaml << 'EOF'
repos:
  - your-username/test-repo
EOF

./target/debug/gru lab --dry-run
# Should see: ✓ Configuration is valid
```

**Expected output from init:**
```
🤖 Gru Setup Wizard

Checking environment variables...
✓ GRU_GITHUB_TOKEN found
✓ ANTHROPIC_API_KEY found

Creating directory structure...
✓ Created /Users/you/.gru/repos
✓ Created /Users/you/.gru/work
✓ Created /Users/you/.gru/archive
✓ Created /Users/you/.gru/state
✓ Created /Users/you/.gru/logs

✓ Generated config file: /Users/you/.gru/config.yaml
  Edit this file to configure repositories and settings.
✓ Initialized Minion ID counter
✓ Initialized timeline cursors

✓ Setup complete! Edit ~/.gru/config.yaml and run 'gru lab'
```

---

### Phase 2: GitHub Integration

**Step 2.1: Implement GitHub client**

Create `src/github.rs`:

```rust
use anyhow::{Context, Result};
use octocrab::{Octocrab, models};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct GitHubClient {
    client: Octocrab,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
    
    pub async fn find_ready_issues(&self, owner: &str, repo: &str) -> Result<Vec<Issue>> {
        let issues = self.client
            .issues(owner, repo)
            .list()
            .state(octocrab::params::State::Open)
            .per_page(20)
            .send()
            .await?;
        
        let ready_issues = issues
            .items
            .into_iter()
            .filter(|issue| {
                issue.labels.iter().any(|label| label.name == "ready-for-minion")
            })
            .map(|issue| Issue {
                number: issue.number,
                title: issue.title,
                body: issue.body,
                labels: issue.labels.iter().map(|l| l.name.clone()).collect(),
                created_at: issue.created_at,
            })
            .collect();
        
        Ok(ready_issues)
    }
    
    pub async fn add_label(&self, owner: &str, repo: &str, issue_number: u64, label: &str) -> Result<()> {
        self.client
            .issues(owner, repo)
            .add_labels(issue_number, &[label.to_string()])
            .await?;
        Ok(())
    }
    
    pub async fn remove_label(&self, owner: &str, repo: &str, issue_number: u64, label: &str) -> Result<()> {
        self.client
            .issues(owner, repo)
            .remove_label(issue_number, label)
            .await?;
        Ok(())
    }
    
    pub async fn replace_labels(&self, owner: &str, repo: &str, issue_number: u64, labels: &[String]) -> Result<()> {
        // Octocrab doesn't have replace_labels, so we get current and update
        self.client
            .issues(owner, repo)
            .update(issue_number)
            .labels(labels)
            .send()
            .await?;
        Ok(())
    }
    
    pub async fn post_comment(&self, owner: &str, repo: &str, issue_number: u64, body: &str) -> Result<()> {
        self.client
            .issues(owner, repo)
            .create_comment(issue_number, body)
            .await?;
        Ok(())
    }
    
    pub async fn create_draft_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<u64> {
        let pr = self.client
            .pulls(owner, repo)
            .create(title, head, base)
            .body(body)
            .draft(true)
            .send()
            .await?;
        
        Ok(pr.number)
    }
    
    pub async fn mark_pr_ready(&self, owner: &str, repo: &str, pr_number: u64) -> Result<()> {
        // Update PR to mark as ready (convert from draft)
        // Note: This requires GraphQL API, octocrab might not support it directly
        // For now, implement with REST API workaround
        
        // TODO: Implement via GraphQL mutation or REST API
        // mutation {
        //   markPullRequestReadyForReview(input: {pullRequestId: "..."}) {
        //     pullRequest { number }
        //   }
        // }
        
        Ok(())
    }
    
    pub async fn get_default_branch(&self, owner: &str, repo: &str) -> Result<String> {
        let repo = self.client
            .repos(owner, repo)
            .get()
            .await?;
        
        Ok(repo.default_branch.unwrap_or_else(|| "main".to_string()))
    }
}
```

**Step 2.2: Test GitHub client**

Create `tests/test_github.rs`:

```rust
#[cfg(test)]
mod tests {
    use gru::github::GitHubClient;
    
    #[tokio::test]
    #[ignore] // Only run with --ignored when testing against real GitHub
    async fn test_find_ready_issues() {
        let client = GitHubClient::new().unwrap();
        let issues = client.find_ready_issues("owner", "repo").await.unwrap();
        
        println!("Found {} ready issues", issues.len());
        for issue in issues {
            println!("  #{} - {}", issue.number, issue.title);
        }
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod config;
pub mod github;
```

Update `Cargo.toml`:

```toml
[lib]
name = "gru"
path = "src/lib.rs"

[[bin]]
name = "gru"
path = "src/main.rs"
```

```bash
# Test (with real credentials and repo)
cargo test --ignored test_find_ready_issues
```

---

### Phase 3: Repository & Worktree Management

**Step 3.1: Implement git operations**

Create `src/git.rs`:

```rust
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitRepo {
    pub bare_path: PathBuf,
    pub owner: String,
    pub repo: String,
}

impl GitRepo {
    pub fn new(owner: &str, repo: &str, gru_root: &Path) -> Self {
        let bare_path = gru_root.join(format!("repos/{}/{}.git", owner, repo));
        Self {
            bare_path,
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }
    
    pub async fn ensure_cloned(&self, github_token: &str) -> Result<()> {
        if self.bare_path.exists() {
            // Already cloned, fetch latest
            self.fetch().await?;
        } else {
            // Clone as bare repository
            std::fs::create_dir_all(self.bare_path.parent().unwrap())?;
            
            let url = format!(
                "https://{}@github.com/{}/{}.git",
                github_token, self.owner, self.repo
            );
            
            let output = Command::new("git")
                .args(["clone", "--bare", &url, self.bare_path.to_str().unwrap()])
                .output()
                .context("Failed to run git clone")?;
            
            if !output.status.success() {
                anyhow::bail!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        
        Ok(())
    }
    
    pub async fn fetch(&self) -> Result<()> {
        let output = Command::new("git")
            .args(["--git-dir", self.bare_path.to_str().unwrap(), "fetch", "--all"])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn create_worktree(
        &self,
        worktree_path: &Path,
        branch_name: &str,
        base_branch: &str,
    ) -> Result<()> {
        // Ensure parent directory exists
        std::fs::create_dir_all(worktree_path.parent().unwrap())?;
        
        // Create worktree and checkout new branch
        let output = Command::new("git")
            .args([
                "--git-dir",
                self.bare_path.to_str().unwrap(),
                "worktree",
                "add",
                "-b",
                branch_name,
                worktree_path.to_str().unwrap(),
                &format!("origin/{}", base_branch),
            ])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn remove_worktree(&self, worktree_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args([
                "--git-dir",
                self.bare_path.to_str().unwrap(),
                "worktree",
                "remove",
                worktree_path.to_str().unwrap(),
            ])
            .output()?;
        
        if !output.status.success() {
            tracing::warn!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        // Also remove directory if it still exists
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path)?;
        }
        
        Ok(())
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod git;
```

---

### Phase 4: Minion Core & Tmux Integration

**Step 4.1: Implement Minion ID generation**

Create `src/minion_id.rs`:

```rust
use anyhow::{Context, Result};
use std::path::Path;
use std::fs;

pub fn generate_next_id(gru_root: &Path) -> Result<String> {
    let counter_path = gru_root.join("state/next_id.txt");
    
    let current = fs::read_to_string(&counter_path)
        .context("Failed to read next_id.txt")?
        .trim()
        .parse::<u32>()
        .context("Invalid counter value")?;
    
    let next = current + 1;
    fs::write(&counter_path, next.to_string())?;
    
    // Convert to base36 with M prefix
    let id = format!("M{:03X}", current); // Using hex for now (easier), can switch to base36
    Ok(id)
}
```

**Step 4.2: Implement Minion state machine**

Create `src/minion.rs`:

```rust
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MinionState {
    InProgress,
    Failed,
    Done,
    Orphaned,
}

#[derive(Debug, Clone)]
pub struct Minion {
    pub id: String,
    pub lab_id: String,
    pub repo: String,
    pub issue_number: u64,
    pub branch: String,
    pub state: MinionState,
    
    pub worktree_path: PathBuf,
    pub pr_number: Option<u64>,
    
    pub tmux_session: String,
    
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    
    pub metrics: MinionMetrics,
}

#[derive(Debug, Clone, Default)]
pub struct MinionMetrics {
    pub tokens_used: u32,
    pub commits_created: u32,
    pub ci_runs: u32,
    pub retry_count: u32,
    pub duration_seconds: u32,
}

impl Minion {
    pub fn new(
        id: String,
        lab_id: String,
        repo: String,
        issue_number: u64,
        branch: String,
        worktree_path: PathBuf,
    ) -> Self {
        let tmux_session = format!("gru-minion-{}", id);
        
        Self {
            id,
            lab_id,
            repo,
            issue_number,
            branch,
            state: MinionState::InProgress,
            worktree_path,
            pr_number: None,
            tmux_session,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            metrics: MinionMetrics::default(),
        }
    }
    
    pub fn touch_activity(&mut self) {
        self.last_activity = Utc::now();
    }
}

pub fn generate_branch_name(issue_number: u64, issue_title: &str, labels: &[String], minion_id: &str) -> String {
    let type_prefix = if labels.contains(&"bug".to_string()) {
        "fix"
    } else if labels.contains(&"enhancement".to_string()) {
        "feat"
    } else if labels.contains(&"documentation".to_string()) {
        "docs"
    } else if labels.contains(&"refactor".to_string()) {
        "refactor"
    } else {
        "feat"
    };
    
    let slug = issue_title
        .to_lowercase()
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect::<String>();
    
    format!("{}/issue{}-{}-{}", type_prefix, issue_number, slug, minion_id)
}
```

**Step 4.3: Implement tmux wrapper**

Create `src/tmux.rs`:

```rust
use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct TmuxSession {
    pub name: String,
}

impl TmuxSession {
    pub fn new(name: String) -> Self {
        Self { name }
    }
    
    pub async fn create(&self, working_dir: &Path) -> Result<()> {
        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &self.name,
                "-c",
                working_dir.to_str().unwrap(),
            ])
            .output()
            .context("Failed to run tmux")?;
        
        if !output.status.success() {
            anyhow::bail!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn send_keys(&self, keys: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, keys, "Enter"])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!(
                "tmux send-keys failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn enable_logging(&self, log_file: &Path) -> Result<()> {
        let log_command = format!("cat >> {}", log_file.display());
        
        let output = Command::new("tmux")
            .args(["pipe-pane", "-t", &self.name, &log_command])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!(
                "tmux pipe-pane failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn attach(&self, read_only: bool) -> Result<()> {
        let mut args = vec!["attach-session", "-t", &self.name];
        if read_only {
            args.push("-r");
        }
        
        let status = Command::new("tmux")
            .args(&args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        
        if !status.success() {
            anyhow::bail!("tmux attach failed");
        }
        
        Ok(())
    }
    
    pub async fn kill(&self) -> Result<()> {
        let output = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output()?;
        
        if !output.status.success() {
            tracing::warn!(
                "tmux kill-session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        
        Ok(())
    }
    
    pub async fn exists(&self) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod minion;
pub mod minion_id;
pub mod tmux;
```

---

### Phase 5: Poller & Scheduler

**Step 5.1: Implement poller**

Create `src/poller.rs`:

```rust
use anyhow::Result;
use tokio::time::{interval, Duration};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::github::GitHubClient;
use crate::config::Config;

pub struct Poller {
    config: Config,
    github: GitHubClient,
}

impl Poller {
    pub fn new(config: Config, github: GitHubClient) -> Self {
        Self { config, github }
    }
    
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let mut ticker = interval(self.config.lab.poll_interval);
        
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.poll_once().await {
                        tracing::error!(error = %e, "Polling failed");
                    }
                }
                
                _ = shutdown.changed() => {
                    tracing::info!("Poller shutting down");
                    return Ok(());
                }
            }
        }
    }
    
    async fn poll_once(&self) -> Result<()> {
        for repo in &self.config.repos {
            let parts: Vec<&str> = repo.split('/').collect();
            if parts.len() != 2 {
                tracing::warn!(repo = %repo, "Invalid repo format");
                continue;
            }
            
            let (owner, repo_name) = (parts[0], parts[1]);
            
            match self.github.find_ready_issues(owner, repo_name).await {
                Ok(issues) => {
                    tracing::debug!(
                        repo = %repo,
                        count = issues.len(),
                        "Found ready issues"
                    );
                    
                    // TODO: Send issues to scheduler
                }
                Err(e) => {
                    tracing::error!(
                        repo = %repo,
                        error = %e,
                        "Failed to fetch issues"
                    );
                }
            }
        }
        
        Ok(())
    }
}
```

**Step 5.2: Implement scheduler**

Create `src/scheduler.rs`:

```rust
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::minion::Minion;
use crate::github::{GitHubClient, Issue};
use crate::config::Config;

pub struct Scheduler {
    config: Config,
    github: GitHubClient,
    active: Arc<RwLock<HashMap<String, Arc<Minion>>>>,
}

impl Scheduler {
    pub fn new(config: Config, github: GitHubClient) -> Self {
        Self {
            config,
            github,
            active: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    pub async fn try_claim_issue(&self, issue: Issue) -> Result<()> {
        let active = self.active.read().await;
        if active.len() >= self.config.lab.slots {
            tracing::debug!("No slots available");
            return Ok(());
        }
        drop(active);
        
        // TODO: Create Minion and spawn
        tracing::info!(
            issue_number = issue.number,
            title = %issue.title,
            "Would claim issue (not yet implemented)"
        );
        
        Ok(())
    }
    
    pub async fn get_active_minions(&self) -> Vec<Arc<Minion>> {
        self.active.read().await.values().cloned().collect()
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod poller;
pub mod scheduler;
```

---

### Phase 6: Minion Lifecycle & Event Logging

**Step 6.1: Implement event logging**

Create `src/events.rs`:

```rust
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum MinionEvent {
    #[serde(rename = "minion:claim")]
    Claim {
        minion_id: String,
        lab_id: String,
        branch: String,
        timestamp: DateTime<Utc>,
    },
    
    #[serde(rename = "minion:progress")]
    Progress {
        minion_id: String,
        phase: String,
        commits: u32,
        tests_passing: bool,
        duration_minutes: u32,
        timestamp: DateTime<Utc>,
    },
    
    #[serde(rename = "minion:done")]
    Done {
        minion_id: String,
        pr_number: u64,
        commits: u32,
        total_cost: String,
        timestamp: DateTime<Utc>,
    },
    
    #[serde(rename = "minion:failed")]
    Failed {
        minion_id: String,
        failure_reason: String,
        attempts: u32,
        last_error: String,
        timestamp: DateTime<Utc>,
    },
}

impl MinionEvent {
    pub fn format_as_comment(&self) -> String {
        let yaml = serde_yaml::to_string(self).unwrap();
        
        let (emoji, title) = match self {
            MinionEvent::Claim { .. } => ("🤖", "Minion claimed this issue"),
            MinionEvent::Progress { .. } => ("🔄", "Progress Update"),
            MinionEvent::Done { .. } => ("✅", "Work Complete"),
            MinionEvent::Failed { .. } => ("❌", "Minion needs help"),
        };
        
        format!("{} **{}**\n\n---\n{}---\n", emoji, title, yaml)
    }
    
    pub async fn append_to_jsonl(&self, archive_path: &Path) -> Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        
        let events_file = archive_path.join("events.jsonl");
        std::fs::create_dir_all(archive_path)?;
        
        let json = serde_json::to_string(self)?;
        
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_file)?;
        
        writeln!(file, "{}", json)?;
        
        Ok(())
    }
}
```

**Step 6.2: Implement Minion lifecycle methods**

Update `src/minion.rs` to add lifecycle methods:

```rust
use crate::github::GitHubClient;
use crate::git::GitRepo;
use crate::tmux::TmuxSession;
use crate::events::MinionEvent;

impl Minion {
    pub async fn claim(
        &mut self,
        github: &GitHubClient,
        git_repo: &GitRepo,
        base_branch: &str,
    ) -> Result<()> {
        let parts: Vec<&str> = self.repo.split('/').collect();
        let (owner, repo) = (parts[0], parts[1]);
        
        // Add in-progress label
        github.add_label(owner, repo, self.issue_number, "in-progress").await?;
        
        // Post claim comment
        let event = MinionEvent::Claim {
            minion_id: self.id.clone(),
            lab_id: self.lab_id.clone(),
            branch: self.branch.clone(),
            timestamp: Utc::now(),
        };
        
        let comment = event.format_as_comment();
        github.post_comment(owner, repo, self.issue_number, &comment).await?;
        
        // Create worktree
        git_repo.create_worktree(&self.worktree_path, &self.branch, base_branch).await?;
        
        // Create tmux session
        let tmux = TmuxSession::new(self.tmux_session.clone());
        tmux.create(&self.worktree_path).await?;
        
        // Enable logging
        let log_file = self.worktree_path.join(".gru-session.log");
        tmux.enable_logging(&log_file).await?;
        
        // Start Claude Code
        let prompt = self.generate_initial_prompt();
        let command = format!("claude --session {} --prompt '{}'", self.id, prompt.replace('\'', "'\\''"));
        tmux.send_keys(&command).await?;
        
        tracing::info!(
            minion_id = %self.id,
            issue = self.issue_number,
            "Minion claimed issue and started session"
        );
        
        Ok(())
    }
    
    fn generate_initial_prompt(&self) -> String {
        format!(
r#"You are Minion {} working on issue #{} in {}.

## Your Mission
1. Understand the issue requirements
2. Explore the codebase to identify relevant files
3. Implement the requested changes
4. Commit after each logical unit of work
5. Push commits to trigger GitHub Actions verification
6. Monitor CI, resolve merge conflicts proactively
7. When complete, notify me

## Working Environment
- Directory: {}
- Branch: {}
- Commit prefix: [minion:{}]

## Guidelines
- Commit frequently when tests pass
- Use descriptive commit messages
- You are autonomous - implement review suggestions or create follow-up issues
- If stuck after 10+ retries, pause and request help

Start working now.
"#,
            self.id,
            self.issue_number,
            self.repo,
            self.worktree_path.display(),
            self.branch,
            self.id
        )
    }
}
```

---

### Phase 7: Attach Sessions

**Step 7.1: Implement attach manager**

Create `src/attach.rs`:

```rust
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;
use crate::minion::Minion;
use crate::tmux::TmuxSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachMode {
    ReadOnly,
    Interactive,
}

pub struct AttachManager {
    active_minions: Arc<RwLock<HashMap<String, Arc<Minion>>>>,
}

impl AttachManager {
    pub fn new(active_minions: Arc<RwLock<HashMap<String, Arc<Minion>>>>) -> Self {
        Self { active_minions }
    }
    
    pub async fn attach(&self, minion_id: &str, mode: AttachMode) -> Result<()> {
        let minions = self.active_minions.read().await;
        let minion = minions
            .get(minion_id)
            .context(format!("Minion {} not found", minion_id))?;
        
        let tmux = TmuxSession::new(minion.tmux_session.clone());
        
        if !tmux.exists().await {
            anyhow::bail!("Minion {} tmux session not found", minion_id);
        }
        
        println!("╭────────────────────────────────────────────────────╮");
        println!("│ 🤖 Attached to Minion {}                         │", minion_id);
        println!("│ Issue: #{} - {}                │", minion.issue_number, &minion.repo);
        println!("│ Mode: {:?}                                       │", mode);
        println!("╰────────────────────────────────────────────────────╯");
        println!();
        
        tmux.attach(mode == AttachMode::ReadOnly).await?;
        
        println!("\nDetached from Minion {}", minion_id);
        
        Ok(())
    }
}
```

**Step 7.2: Implement attach CLI command**

Update `src/cli/attach.rs`:

```rust
use anyhow::Result;
use crate::attach::{AttachManager, AttachMode};

pub async fn run(minion_id: String, interactive: bool) -> Result<()> {
    // TODO: Get active minions from Lab
    // For now, just try to attach to tmux session directly
    
    let mode = if interactive {
        AttachMode::Interactive
    } else {
        AttachMode::ReadOnly
    };
    
    let session_name = format!("gru-minion-{}", minion_id);
    let tmux = crate::tmux::TmuxSession::new(session_name);
    
    if !tmux.exists().await {
        anyhow::bail!("Minion {} not found or session not running", minion_id);
    }
    
    println!("╭────────────────────────────────────────────────────╮");
    println!("│ 🤖 Attached to Minion {}                         │", minion_id);
    println!("│ Mode: {:?}                                       │", mode);
    println!("│ Press Ctrl+D to detach                            │");
    println!("╰────────────────────────────────────────────────────╯");
    println!();
    
    tmux.attach(mode == AttachMode::ReadOnly).await?;
    
    println!("\nDetached from Minion {}", minion_id);
    
    Ok(())
}
```

Update `src/main.rs` to wire up attach command:

```rust
Commands::Attach { minion_id, interactive } => {
    cli::attach::run(minion_id, interactive).await
}
```

Add to `src/cli/mod.rs`:

```rust
pub mod attach;
```

---

### Phase 8: CLI & Observability

**Step 8.1: Implement Lab runner**

Create `src/lab.rs`:

```rust
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{RwLock, watch};
use std::collections::HashMap;
use crate::config::Config;
use crate::github::GitHubClient;
use crate::poller::Poller;
use crate::scheduler::Scheduler;
use crate::minion::Minion;

pub struct Lab {
    config: Config,
    github: GitHubClient,
    active_minions: Arc<RwLock<HashMap<String, Arc<Minion>>>>,
}

impl Lab {
    pub fn new(config: Config) -> Result<Self> {
        let github = GitHubClient::new()?;
        
        Ok(Self {
            config,
            github,
            active_minions: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Lab starting...");
        
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        
        // Start poller
        let poller = Poller::new(self.config.clone(), self.github.clone());
        let poller_handle = {
            let shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                if let Err(e) = poller.run(shutdown).await {
                    tracing::error!(error = %e, "Poller failed");
                }
            })
        };
        
        // Wait for Ctrl+C
        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutdown signal received");
        
        shutdown_tx.send(true)?;
        poller_handle.await?;
        
        tracing::info!("Lab stopped");
        Ok(())
    }
}
```

**Step 8.2: Wire up lab command**

Update `src/cli/lab.rs`:

```rust
use anyhow::Result;
use crate::config::Config;
use crate::lab::Lab;

pub async fn run(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    config.validate()?;
    
    if dry_run {
        println!("✓ Configuration is valid");
        println!("\nConfigured repositories:");
        for repo in &config.repos {
            println!("  - {}", repo);
        }
        println!("\nLab settings:");
        println!("  Slots: {}", config.lab.slots);
        println!("  Poll interval: {:?}", config.lab.poll_interval);
        return Ok(());
    }
    
    let lab = Lab::new(config)?;
    lab.run().await
}
```

Update `src/main.rs`:

```rust
Commands::Lab { dry_run } => {
    cli::lab::run(dry_run).await
}
```

Add to `src/cli/mod.rs`:

```rust
pub mod lab;
```

**Step 8.3: Implement minions list command**

Create `src/cli/minions.rs`:

```rust
use anyhow::Result;

pub async fn list() -> Result<()> {
    // TODO: Get active minions from Lab state
    // For now, list tmux sessions
    
    use std::process::Command;
    
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;
    
    if !output.status.success() {
        println!("No active Minions");
        return Ok(());
    }
    
    let sessions = String::from_utf8_lossy(&output.stdout);
    let minion_sessions: Vec<&str> = sessions
        .lines()
        .filter(|line| line.starts_with("gru-minion-"))
        .collect();
    
    if minion_sessions.is_empty() {
        println!("No active Minions");
        return Ok(());
    }
    
    println!("Active Minions:");
    println!("───────────────");
    for session in minion_sessions {
        let minion_id = session.strip_prefix("gru-minion-").unwrap_or(session);
        println!("  {}", minion_id);
    }
    
    Ok(())
}
```

Update `src/main.rs`:

```rust
Commands::Minions { command } => {
    match command {
        Some(MinionsCommands::List) | None => {
            cli::minions::list().await
        }
    }
}
```

Add to `src/cli/mod.rs`:

```rust
pub mod minions;
```

**Step 8.4: Add structured logging**

Update `src/main.rs` to configure tracing:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let log_level = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "gru=info".to_string());
    
    tracing_subscriber::fmt()
        .with_env_filter(log_level)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .init();
    
    // ... rest of main
}
```

---

### Phase 9: Integration & Validation

**Step 9.1: Integration test plan**

Create test repository and issue:

```bash
# Create test repo on GitHub
# Add issue with label "ready-for-minion"

# Configure Gru
cat > ~/.gru/config.yaml << EOF
repos:
  - your-username/gru-test-repo

lab:
  slots: 1
  poll_interval: 10s
  max_ci_retries: 3
  archive_retention_days: 0

git:
  user_name: "Gru Test Minion"
  user_email: "test@gru.local"
  branch_prefix: "minion/issue-"

observability:
  log_level: "debug"
  log_file: "~/.gru/logs/gru.log"
  metrics_enabled: false
  metrics_port: 9090
EOF

# Set tokens
export GRU_GITHUB_TOKEN="your-token"
export ANTHROPIC_API_KEY="your-key"

# Run lab
cargo run -- lab
```

**Step 9.2: Validation checklist**

Test each component:

1. **Init command**
   ```bash
   rm -rf ~/.gru
   cargo run -- init
   # Verify all directories created
   ls -la ~/.gru/
   ```

2. **Config validation**
   ```bash
   cargo run -- lab --dry-run
   # Should show valid configuration
   ```

3. **Poller**
   ```bash
   # Create issue with ready-for-minion label
   cargo run -- lab
   # Watch logs for "Found ready issues"
   ```

4. **Claim and worktree creation**
   ```bash
   # Lab should claim issue within poll_interval
   # Check: ls ~/.gru/work/owner/repo/
   # Should see M001/ directory
   ```

5. **Tmux session**
   ```bash
   tmux list-sessions | grep gru-minion
   # Should see session
   ```

6. **Attach**
   ```bash
   # In another terminal
   cargo run -- attach M001
   # Should see Claude Code output
   ```

7. **GitHub integration**
   ```bash
   # Check issue on GitHub:
   # - Should have "in-progress" label
   # - Should have claim comment
   # - Should have draft PR created
   ```

8. **Cleanup**
   ```bash
   # Kill tmux session
   tmux kill-session -t gru-minion-M001
   
   # Verify Lab detects and archives
   ls ~/.gru/archive/M001/
   ```

---

## Validation and Acceptance

### End-to-End Validation

After implementing all phases, validate the complete system:

1. **Setup**
   ```bash
   gru init
   # Edit config to add test repository
   # Set environment variables
   ```

2. **Create test issue on GitHub**
   - Title: "Test issue for Gru"
   - Body: "Add a simple hello world function to README.md"
   - Label: "ready-for-minion"

3. **Start Lab**
   ```bash
   gru lab
   ```

4. **Observe autonomous behavior** (within 30 seconds):
   - Lab polls and finds issue
   - Minion claims issue (adds in-progress label, posts comment)
   - Branch created: `feat/issue1-test-issue-for-M001`
   - Draft PR opened
   - Tmux session started

5. **Attach to observe**
   ```bash
   gru attach M001
   # Watch Claude Code work
   # Detach with Ctrl+D
   ```

6. **Verify GitHub state**
   - Issue has `in-progress` label
   - Draft PR exists with Minion's work
   - Comment thread shows structured YAML events

7. **Monitor through completion**
   - Minion commits changes
   - PR converted to ready-for-review
   - Issue marked `minion:done` after merge

8. **Check archive**
   ```bash
   cat ~/.gru/archive/M001/events.jsonl
   # Should see complete event timeline
   ```

### Success Criteria

✅ `gru init` creates all necessary directories and files  
✅ `gru lab --dry-run` validates configuration  
✅ `gru lab` starts polling and claiming issues  
✅ Minions successfully create branches and worktrees  
✅ Tmux sessions spawn with Claude Code  
✅ GitHub labels update correctly (ready → in-progress → done)  
✅ Comments posted with structured YAML events  
✅ Draft PRs created successfully  
✅ `gru attach M001` connects to running session  
✅ `gru minions list` shows active Minions  
✅ Logs written to ~/.gru/logs/gru.log  
✅ Events archived to ~/.gru/archive/{ID}/events.jsonl  
✅ Cleanup on completion (worktree removed, session killed)  

---

## Idempotence and Recovery

### Safe Operations

All operations are designed to be idempotent:

- **Init**: Creates directories only if they don't exist, preserves existing config
- **Worktree creation**: Git worktree command is idempotent (fails safely if exists)
- **Tmux sessions**: Check existence before creating
- **Label operations**: GitHub API is idempotent for adding/removing labels
- **Comment posting**: New comments don't conflict with existing ones

### Recovery Scenarios

1. **Lab crashes mid-operation**
   - On restart, enumerate tmux sessions with prefix `gru-minion-`
   - Fetch issue state from GitHub (labels are source of truth)
   - Resume monitoring or mark as orphaned

2. **GitHub API failures**
   - Retry with exponential backoff
   - Log errors but continue operation
   - Don't lose local state

3. **Corrupted state files**
   - next_id.txt: If missing or invalid, start from 1
   - cursors.json: If missing, re-poll all issues
   - Config: Fail fast with clear error message

4. **Orphaned tmux sessions**
   ```bash
   # Manual cleanup if needed
   tmux list-sessions | grep gru-minion | cut -d: -f1 | xargs -I {} tmux kill-session -t {}
   ```

5. **Disk full**
   - Lab logs error and pauses polling
   - Existing Minions continue (tmux sessions unaffected)
   - Resume automatically when space available

---

## Artifacts and Notes

### Key Implementation Notes

1. **Octocrab limitations**: Some GitHub API features (like mark PR ready) require GraphQL. May need to use octocrab's GraphQL client or fall back to raw HTTP requests.

2. **Claude Code integration**: This plan assumes `claude` command is available. Actual integration may require:
   - Claude Code API endpoint if available
   - Alternative: Run via API with conversation continuity
   - Prompts need tuning based on Claude Code's actual behavior

3. **Tmux portability**: tmux is Unix-only. Windows users need WSL. Consider documenting this clearly in setup.

4. **Base36 vs Hex IDs**: Currently using hex (M001, M002), but could switch to true base36 if preferred (M00 through MZZ through M10 through MZZ through M20, etc).

5. **Rate limiting**: GitHub API rate limits (5000/hr) should be monitored. Add metrics for API calls per hour.

6. **Token security**: Never log tokens. Consider using keyring/keychain for token storage instead of environment variables in production.

### Testing Strategy

1. **Unit tests**: Test individual components (config parsing, branch name generation, event formatting)

2. **Integration tests**: Use GitHub's API test mode or create dedicated test repositories

3. **End-to-end tests**: Manual validation against real GitHub repository

4. **Load testing**: Multiple issues, multiple repos, test slot allocation

### Future Enhancements (Post-V1)

After V1 is stable, consider:

- [ ] SQLite persistence for faster startup recovery
- [ ] Webhooks instead of polling
- [ ] GraphQL API for Lab introspection
- [ ] Web UI (Tower component)
- [ ] Multi-Lab coordination
- [ ] Prometheus metrics endpoint
- [ ] Cost tracking and budgets
- [ ] Learning from past PRs

---

## Interfaces and Dependencies

### External Systems

**GitHub API (octocrab)**
- Issues endpoint: List, get, update labels, create comments
- Pulls endpoint: Create draft PR, mark ready, get status
- Repos endpoint: Get default branch
- Timeline endpoint: Fetch issue events (may need raw HTTP)

**Git CLI**
- Commands used: `clone --bare`, `fetch`, `worktree add`, `worktree remove`
- All git operations shell out to git binary

**Tmux CLI**
- Commands used: `new-session`, `send-keys`, `pipe-pane`, `attach-session`, `kill-session`, `has-session`
- Must be installed on system (brew install tmux)

**Claude Code**
- Assumed to be available as `claude` command or via API
- Receives initial prompt via stdin or command line
- Outputs to stdout/stderr (captured by tmux pipe-pane)

### Key Rust Dependencies

- `clap` ^4.5: CLI parsing with derive macros
- `tokio` ^1.40: Async runtime (full features)
- `octocrab` ^0.40: GitHub API client
- `serde` + `serde_yaml` + `serde_json`: Serialization
- `tracing` + `tracing-subscriber`: Structured logging
- `anyhow` + `thiserror`: Error handling
- `chrono`: DateTime handling

### File System Contract

```
~/.gru/
  config.yaml              # YAML, mode 0600 (contains no secrets in V1)
  repos/<owner>/<repo>.git # Bare git repositories
  work/<owner>/<repo>/<ID> # Git worktrees (ephemeral)
  archive/<ID>/            # Permanent logs (until manual cleanup)
    events.jsonl
    session.log
  state/
    next_id.txt            # Plain text integer
    cursors.json           # JSON map {repo/issue -> cursor}
  logs/
    gru.log               # Rotating log file (TODO: add rotation)
```

### GitHub Contract

**Labels used:**
- `ready-for-minion`: User adds to signal readiness
- `in-progress`: Lab adds when claimed
- `minion:done`: Lab adds on successful merge
- `minion:failed`: Lab adds on max retries exceeded

**Comment format:**
```markdown
🤖 **Title**

---
event: event_type
minion_id: M001
...
---

Human-readable message.
```

**Branch naming:**
- Format: `{type}/issue{number}-{slug}-{minion_id}`
- Example: `feat/issue123-test-issue-for-M001`
- Must be unique (enforced by PR creation check)

---

## Change Log

**2025-11-30 Initial Creation**
- Created V1 execution plan based on DESIGN.md and DECISIONS.md
- Defined 9 implementation phases
- Specified concrete steps with commands and expected outputs
- Added validation criteria and acceptance tests
