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
    /// Pause to review the assembled prompt before launching the agent.
    pub discuss: bool,
    /// Skip dependency checking entirely.
    pub ignore_deps: bool,
    /// Internal: run as background worker for a previously-registered minion.
    /// Value is the minion ID to look up in the registry.
    pub worker: Option<String>,
}

/// Maximum number of agent re-invocation rounds per feedback type (reviews and issue comments).
/// Each feedback type tracks its own counter independently, so the effective cap per session
/// is `MAX_REVIEW_ROUNDS` rounds of formal reviews PLUS `MAX_REVIEW_ROUNDS` rounds of issue
/// comments. After the limit is reached for a type, additional feedback of that type requires
/// manual handling.
pub(crate) const MAX_REVIEW_ROUNDS: usize = 5;

/// Maximum number of auto-rebase attempts per monitoring session
/// After this limit, the minion escalates via PR comment
pub(crate) const MAX_REBASE_ATTEMPTS: usize = 2;

/// Result of resolving an issue argument into validated context.
///
/// `issue_num` is `Some(n)` when the minion is working on a GitHub issue (e.g., `gru do`),
/// and `None` for ad-hoc operations without a linked issue (e.g., `gru prompt --pr`, `gru review`).
/// GitHub API calls that require an issue number (label updates, comments) are skipped when `None`.
pub(crate) struct IssueContext {
    pub(crate) owner: String,
    pub(crate) repo: String,
    /// GitHub hostname (e.g., "github.com" or "ghe.example.com")
    pub(crate) host: String,
    /// GitHub issue number, or `None` for operations without a linked issue.
    pub(crate) issue_num: Option<u64>,
    /// Fetched issue details: (title, body, labels). None if fetch failed.
    pub(crate) details: Option<IssueDetails>,
}

/// Fetched issue metadata from GitHub.
pub(crate) struct IssueDetails {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
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
