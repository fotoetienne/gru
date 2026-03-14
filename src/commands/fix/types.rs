use crate::github::GitHubClient;
use std::path::PathBuf;
use std::process::ExitStatus;
use uuid::Uuid;

/// Options for the `gru do` (fix) command.
pub struct FixOptions {
    pub timeout: Option<String>,
    pub review_timeout: Option<String>,
    pub monitor_timeout: Option<String>,
    pub quiet: bool,
    pub force_new: bool,
    pub agent_name: String,
    pub no_watch: bool,
    pub auto_merge: bool,
    /// Detach immediately after spawning background worker (don't follow logs).
    pub detach: bool,
    /// Internal: run as background worker for a previously-registered minion.
    /// Value is the minion ID to look up in the registry.
    pub worker: Option<String>,
}

/// Maximum size of the output buffer for test detection (in bytes)
pub(crate) const MAX_OUTPUT_BUFFER_SIZE: usize = 10000;

/// Size to trim the output buffer to when it exceeds the maximum (in bytes)
pub(crate) const TRIM_OUTPUT_BUFFER_SIZE: usize = 5000;

/// Maximum number of review rounds to handle automatically
/// After this limit, the user must handle additional reviews manually
pub(crate) const MAX_REVIEW_ROUNDS: usize = 5;

/// Maximum number of auto-rebase attempts per monitoring session
/// After this limit, the minion escalates via PR comment
pub(crate) const MAX_REBASE_ATTEMPTS: usize = 2;

/// Result of resolving an issue argument into validated context.
/// Contains the parsed issue number as `u64`, eliminating repeated string parsing.
pub(crate) struct IssueContext {
    pub owner: String,
    pub repo: String,
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    pub host: String,
    pub issue_num: u64,
    /// Fetched issue details: (title, body, labels). None if fetch failed.
    pub details: Option<IssueDetails>,
    pub github_client: Option<GitHubClient>,
}

/// Fetched issue metadata from GitHub.
pub(crate) struct IssueDetails {
    pub title: String,
    pub body: String,
    pub labels: String,
}

/// Result of setting up a worktree for a minion.
pub(crate) struct WorktreeContext {
    pub minion_id: String,
    pub branch_name: String,
    /// Top-level minion directory where metadata lives (events.jsonl, PR_DESCRIPTION.md, etc.)
    pub minion_dir: PathBuf,
    /// Git worktree checkout path (minion_dir/checkout for new layout, minion_dir for legacy)
    pub checkout_path: PathBuf,
    pub session_id: Uuid,
}

/// Result of running an agent session.
pub(crate) struct AgentResult {
    pub status: ExitStatus,
}

/// Result of checking for existing minions on an issue.
pub(crate) enum ExistingMinionCheck {
    /// No existing minions found, proceed with new session.
    None,
    /// Found a stopped minion that can be resumed.
    Resumable(String, Box<crate::minion_registry::MinionInfo>),
    /// Found running minion(s), user shown options and should exit.
    AlreadyRunning,
}
