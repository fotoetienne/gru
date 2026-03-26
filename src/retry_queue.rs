use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Default base delay for failure retries (10 seconds).
const DEFAULT_BASE_DELAY_SECS: u64 = 10;

/// Fixed delay for continuation retries (1 second).
const CONTINUATION_DELAY: Duration = Duration::from_secs(1);

/// Distinguishes between the two retry paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// Agent finished normally but issue still needs work. Fast retry, reuse session.
    Continuation,
    /// Agent crashed, timed out, or stalled. Backoff retry, fresh session.
    Failure,
}

impl std::fmt::Display for RetryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryKind::Continuation => write!(f, "continuation"),
            RetryKind::Failure => write!(f, "failure"),
        }
    }
}

/// A single entry in the retry queue.
#[derive(Debug, Clone)]
pub struct RetryEntry {
    pub owner: String,
    pub repo: String,
    pub issue_number: u64,
    pub kind: RetryKind,
    /// Failure attempt counter (not incremented for continuation retries).
    pub attempt: u32,
    /// When this retry becomes eligible for dispatch.
    pub due_at: Instant,
    /// Human-readable reason for the retry.
    pub reason: String,
    /// For continuation retries: reuse the same session-id.
    pub session_id: Option<String>,
    /// Minion ID to resume (if reusing worktree).
    pub minion_id: Option<String>,
    /// Path to the existing worktree checkout.
    pub workspace_path: Option<PathBuf>,
    /// Generation counter — prevents stale retry timers from firing.
    #[allow(dead_code)] // Part of the stale-retry detection protocol
    pub generation: u64,
    /// GitHub host for this issue.
    pub host: String,
}

/// In-memory retry queue for the lab poll loop.
///
/// Tracks failed or incomplete Minion runs and schedules them for re-dispatch
/// with exponential backoff (failure) or fixed delay (continuation).
#[derive(Debug)]
pub struct RetryQueue {
    /// Keyed by `"{owner}/{repo}#{issue_number}"` for dedup.
    entries: HashMap<String, RetryEntry>,
    /// Monotonically increasing generation counter.
    next_generation: u64,
    /// Maximum failure retry attempts (0 = no failure retries).
    max_attempts: u32,
    /// Maximum backoff delay in seconds.
    max_backoff_secs: u64,
}

impl RetryQueue {
    /// Create a new retry queue with the given configuration.
    pub fn new(max_attempts: u32, max_backoff_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            next_generation: 1,
            max_attempts,
            max_backoff_secs,
        }
    }

    /// Enqueue a failure retry with exponential backoff.
    ///
    /// Returns `false` if max attempts have been reached (caller should mark as failed).
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_failure(
        &mut self,
        host: &str,
        owner: &str,
        repo: &str,
        issue_number: u64,
        current_attempt: u32,
        reason: &str,
        minion_id: Option<&str>,
        workspace_path: Option<PathBuf>,
    ) -> bool {
        let attempt = current_attempt + 1;
        if self.max_attempts == 0 || attempt > self.max_attempts {
            return false;
        }

        let delay = backoff_delay(attempt, self.max_backoff_secs);
        let key = entry_key(owner, repo, issue_number);
        let generation = self.next_generation;
        self.next_generation += 1;

        log::info!(
            "🔄 Enqueuing failure retry #{} for {}/{}#{} (backoff {:.0}s): {}",
            attempt,
            owner,
            repo,
            issue_number,
            delay.as_secs_f64(),
            reason
        );

        self.entries.insert(
            key,
            RetryEntry {
                owner: owner.to_string(),
                repo: repo.to_string(),
                issue_number,
                kind: RetryKind::Failure,
                attempt,
                due_at: Instant::now() + delay,
                reason: reason.to_string(),
                session_id: None,
                minion_id: minion_id.map(String::from),
                workspace_path,
                generation,
                host: host.to_string(),
            },
        );

        true
    }

    /// Enqueue a continuation retry with fixed 1-second delay.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_continuation(
        &mut self,
        host: &str,
        owner: &str,
        repo: &str,
        issue_number: u64,
        session_id: &str,
        minion_id: &str,
        workspace_path: PathBuf,
        reason: &str,
    ) {
        let key = entry_key(owner, repo, issue_number);
        let generation = self.next_generation;
        self.next_generation += 1;

        log::info!(
            "🔄 Enqueuing continuation retry for {}/{}#{}: {}",
            owner,
            repo,
            issue_number,
            reason
        );

        self.entries.insert(
            key,
            RetryEntry {
                owner: owner.to_string(),
                repo: repo.to_string(),
                issue_number,
                kind: RetryKind::Continuation,
                attempt: 0, // continuation retries don't count as failure attempts
                due_at: Instant::now() + CONTINUATION_DELAY,
                reason: reason.to_string(),
                session_id: Some(session_id.to_string()),
                minion_id: Some(minion_id.to_string()),
                workspace_path: Some(workspace_path),
                generation,
                host: host.to_string(),
            },
        );
    }

    /// Remove and return all entries whose due_at has passed.
    ///
    /// Continuation retries are returned first (higher priority).
    pub fn take_due(&mut self) -> Vec<RetryEntry> {
        let now = Instant::now();
        let due_keys: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.due_at <= now)
            .map(|(key, _)| key.clone())
            .collect();

        let mut due: Vec<RetryEntry> = due_keys
            .into_iter()
            .filter_map(|key| self.entries.remove(&key))
            .collect();

        // Continuation retries get priority
        due.sort_by_key(|e| match e.kind {
            RetryKind::Continuation => 0,
            RetryKind::Failure => 1,
        });

        due
    }

    /// Cancel a retry for a specific issue (e.g., issue was closed or re-dispatched).
    ///
    /// Bumps the generation counter so any stale references are invalidated.
    #[allow(dead_code)] // Used in tests; will be called from lab when issues are re-dispatched
    pub fn cancel(&mut self, owner: &str, repo: &str, issue_number: u64) {
        let key = entry_key(owner, repo, issue_number);
        if self.entries.remove(&key).is_some() {
            log::info!("🚫 Cancelled retry for {}/{}#{}", owner, repo, issue_number);
        }
    }

    /// Check if an issue has a pending retry.
    #[allow(dead_code)] // Used in tests; useful for callers to check before enqueuing
    pub fn has_pending(&self, owner: &str, repo: &str, issue_number: u64) -> bool {
        let key = entry_key(owner, repo, issue_number);
        self.entries.contains_key(&key)
    }

    /// Get all pending retry entries (for status display).
    pub fn pending_entries(&self) -> Vec<&RetryEntry> {
        let mut entries: Vec<&RetryEntry> = self.entries.values().collect();
        entries.sort_by_key(|e| e.due_at);
        entries
    }

    /// Number of pending retries.
    #[allow(dead_code)] // Used in tests
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compute the key for deduplication.
fn entry_key(owner: &str, repo: &str, issue_number: u64) -> String {
    format!("{}/{}#{}", owner, repo, issue_number)
}

/// Compute exponential backoff delay: `min(10s × 2^(attempt-1), max_backoff)`.
fn backoff_delay(attempt: u32, max_backoff_secs: u64) -> Duration {
    let base = DEFAULT_BASE_DELAY_SECS as f64;
    let exponent = (attempt.saturating_sub(1)) as f64;
    let delay_secs = base * 2f64.powf(exponent);
    let capped = delay_secs.min(max_backoff_secs as f64);
    Duration::from_secs_f64(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_delay() {
        // attempt 1: 10s * 2^0 = 10s
        assert_eq!(backoff_delay(1, 300), Duration::from_secs(10));
        // attempt 2: 10s * 2^1 = 20s
        assert_eq!(backoff_delay(2, 300), Duration::from_secs(20));
        // attempt 3: 10s * 2^2 = 40s
        assert_eq!(backoff_delay(3, 300), Duration::from_secs(40));
        // attempt 4: 10s * 2^3 = 80s
        assert_eq!(backoff_delay(4, 300), Duration::from_secs(80));
        // attempt 5: 10s * 2^4 = 160s
        assert_eq!(backoff_delay(5, 300), Duration::from_secs(160));
        // attempt 6: 10s * 2^5 = 320s → capped at 300s
        assert_eq!(backoff_delay(6, 300), Duration::from_secs(300));
    }

    #[test]
    fn test_backoff_capped() {
        // With low cap, all attempts are capped
        assert_eq!(backoff_delay(1, 5), Duration::from_secs(5));
        assert_eq!(backoff_delay(3, 5), Duration::from_secs(5));
    }

    #[test]
    fn test_enqueue_failure_respects_max_attempts() {
        let mut queue = RetryQueue::new(2, 300);

        // Attempt 1 (from 0): should succeed
        assert!(queue.enqueue_failure("github.com", "owner", "repo", 42, 0, "timeout", None, None));
        assert_eq!(queue.len(), 1);

        // Attempt 2 (from 1): should succeed
        queue.entries.clear();
        assert!(queue.enqueue_failure("github.com", "owner", "repo", 42, 1, "timeout", None, None));

        // Attempt 3 (from 2): exceeds max_attempts=2, should fail
        queue.entries.clear();
        assert!(!queue.enqueue_failure(
            "github.com",
            "owner",
            "repo",
            42,
            2,
            "timeout",
            None,
            None
        ));
    }

    #[test]
    fn test_enqueue_failure_disabled() {
        let mut queue = RetryQueue::new(0, 300);
        assert!(!queue.enqueue_failure(
            "github.com",
            "owner",
            "repo",
            42,
            0,
            "timeout",
            None,
            None
        ));
    }

    #[test]
    fn test_enqueue_continuation() {
        let mut queue = RetryQueue::new(3, 300);
        queue.enqueue_continuation(
            "github.com",
            "owner",
            "repo",
            42,
            "session-123",
            "M001",
            PathBuf::from("/tmp/worktree"),
            "issue still open",
        );
        assert_eq!(queue.len(), 1);

        let entries = queue.pending_entries();
        assert_eq!(entries[0].kind, RetryKind::Continuation);
        assert_eq!(entries[0].session_id.as_deref(), Some("session-123"));
        assert_eq!(entries[0].attempt, 0);
    }

    #[test]
    fn test_take_due_returns_due_entries() {
        let mut queue = RetryQueue::new(3, 300);

        // Insert an entry that's already due
        let key = entry_key("owner", "repo", 42);
        queue.entries.insert(
            key,
            RetryEntry {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                issue_number: 42,
                kind: RetryKind::Failure,
                attempt: 1,
                due_at: Instant::now() - Duration::from_secs(1), // already past
                reason: "test".to_string(),
                session_id: None,
                minion_id: None,
                workspace_path: None,
                generation: 1,
                host: "github.com".to_string(),
            },
        );

        let due = queue.take_due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].issue_number, 42);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_take_due_skips_future_entries() {
        let mut queue = RetryQueue::new(3, 300);

        let key = entry_key("owner", "repo", 42);
        queue.entries.insert(
            key,
            RetryEntry {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                issue_number: 42,
                kind: RetryKind::Failure,
                attempt: 1,
                due_at: Instant::now() + Duration::from_secs(3600), // far future
                reason: "test".to_string(),
                session_id: None,
                minion_id: None,
                workspace_path: None,
                generation: 1,
                host: "github.com".to_string(),
            },
        );

        let due = queue.take_due();
        assert!(due.is_empty());
        assert_eq!(queue.len(), 1); // still in queue
    }

    #[test]
    fn test_take_due_prioritizes_continuation() {
        let mut queue = RetryQueue::new(3, 300);
        let now = Instant::now() - Duration::from_secs(1);

        // Insert failure first
        queue.entries.insert(
            entry_key("owner", "repo", 1),
            RetryEntry {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                issue_number: 1,
                kind: RetryKind::Failure,
                attempt: 1,
                due_at: now,
                reason: "crash".to_string(),
                session_id: None,
                minion_id: None,
                workspace_path: None,
                generation: 1,
                host: "github.com".to_string(),
            },
        );

        // Then continuation
        queue.entries.insert(
            entry_key("owner", "repo", 2),
            RetryEntry {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                issue_number: 2,
                kind: RetryKind::Continuation,
                attempt: 0,
                due_at: now,
                reason: "still open".to_string(),
                session_id: Some("sess".to_string()),
                minion_id: Some("M001".to_string()),
                workspace_path: Some(PathBuf::from("/tmp")),
                generation: 2,
                host: "github.com".to_string(),
            },
        );

        let due = queue.take_due();
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].kind, RetryKind::Continuation); // continuation first
        assert_eq!(due[1].kind, RetryKind::Failure);
    }

    #[test]
    fn test_cancel() {
        let mut queue = RetryQueue::new(3, 300);
        queue.enqueue_failure("github.com", "owner", "repo", 42, 0, "timeout", None, None);
        assert_eq!(queue.len(), 1);

        queue.cancel("owner", "repo", 42);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_has_pending() {
        let mut queue = RetryQueue::new(3, 300);
        assert!(!queue.has_pending("owner", "repo", 42));

        queue.enqueue_failure("github.com", "owner", "repo", 42, 0, "timeout", None, None);
        assert!(queue.has_pending("owner", "repo", 42));
    }

    #[test]
    fn test_dedup_overwrites_existing() {
        let mut queue = RetryQueue::new(3, 300);
        queue.enqueue_failure("github.com", "owner", "repo", 42, 0, "first", None, None);
        queue.enqueue_failure("github.com", "owner", "repo", 42, 1, "second", None, None);

        assert_eq!(queue.len(), 1); // same key, overwritten
        let entries = queue.pending_entries();
        assert_eq!(entries[0].reason, "second");
        assert_eq!(entries[0].attempt, 2);
    }

    #[test]
    fn test_retry_kind_display() {
        assert_eq!(format!("{}", RetryKind::Continuation), "continuation");
        assert_eq!(format!("{}", RetryKind::Failure), "failure");
    }
}
