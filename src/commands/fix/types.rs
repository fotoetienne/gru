use std::path::PathBuf;
use std::process::ExitStatus;
use uuid::Uuid;

/// Options for the `gru do` (fix) command.
pub(crate) struct FixOptions {
    pub(crate) timeout: Option<String>,
    pub(crate) review_timeout: Option<String>,
    pub(crate) monitor_timeout: Option<String>,
    pub(crate) quiet: bool,
    pub(crate) force_new: bool,
    pub(crate) agent_name: String,
    pub(crate) no_watch: bool,
    pub(crate) auto_merge: bool,
    /// Detach immediately after spawning background worker (don't follow logs).
    pub(crate) detach: bool,
    /// Skip dependency checking entirely.
    pub(crate) ignore_deps: bool,
    /// Internal: run as background worker for a previously-registered minion.
    /// Value is the minion ID to look up in the registry.
    pub(crate) worker: Option<String>,
}

/// Maximum number of review rounds to handle automatically
/// After this limit, the user must handle additional reviews manually
pub(crate) const MAX_REVIEW_ROUNDS: usize = 5;

/// Maximum number of auto-rebase attempts per monitoring session
/// After this limit, the minion escalates via PR comment
pub(crate) const MAX_REBASE_ATTEMPTS: usize = 2;

/// Result of resolving an issue argument into validated context.
/// Contains the parsed issue number as `u64`, eliminating repeated string parsing.
pub(crate) struct IssueContext {
    pub(crate) owner: String,
    pub(crate) repo: String,
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    pub(crate) host: String,
    pub(crate) issue_num: u64,
    /// Fetched issue details: (title, body, labels). None if fetch failed.
    pub(crate) details: Option<IssueDetails>,
}

/// Fetched issue metadata from GitHub.
pub(crate) struct IssueDetails {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) labels: String,
}

/// Result of setting up a worktree for a minion.
pub(crate) struct WorktreeContext {
    pub(crate) minion_id: String,
    pub(crate) branch_name: String,
    /// Top-level minion directory where metadata lives (events.jsonl, PR_DESCRIPTION.md, etc.)
    pub(crate) minion_dir: PathBuf,
    /// Git worktree checkout path (minion_dir/checkout for new layout, minion_dir for legacy)
    pub(crate) checkout_path: PathBuf,
    pub(crate) session_id: Uuid,
}

/// Result of running an agent session.
pub(crate) struct AgentResult {
    pub(crate) status: ExitStatus,
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
