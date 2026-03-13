use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::agent::TokenUsage;
use crate::workspace::Workspace;

/// Async helper that loads the registry inside `spawn_blocking`, runs the
/// provided closure, and flattens the `JoinError` / inner `Result`.
///
/// This encapsulates the common pattern of:
/// ```ignore
/// tokio::task::spawn_blocking(move || {
///     let mut registry = MinionRegistry::load(None)?;
///     registry.register/update/remove(...)
/// })
/// .await
/// .context("...")??;
/// ```
///
/// The double `??` (JoinError then inner Result) is a subtle footgun that
/// this helper eliminates.
pub async fn with_registry<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&mut MinionRegistry) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut registry = MinionRegistry::load(None)?;
        f(&mut registry)
    })
    .await
    .context("Registry task panicked")?
}

/// Reverts a minion's registry entry to Stopped mode (best-effort).
///
/// Used when a command has claimed a session (e.g. as Interactive or Autonomous)
/// but cannot proceed (spawn failure, bad session ID, unsupported backend, etc.).
/// Errors are silently ignored since this is a cleanup path.
pub async fn revert_to_stopped(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.pid = None;
            info.last_activity = Utc::now();
        })
    })
    .await;
}

/// The execution mode of a Minion session
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MinionMode {
    /// Autonomous operation via `gru do` or `gru resume` - stream monitoring
    Autonomous,
    /// Interactive operation via `gru attach` - user in terminal
    Interactive,
    /// Process not running (pid is None)
    #[default]
    Stopped,
}

impl std::fmt::Display for MinionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MinionMode::Autonomous => write!(f, "autonomous"),
            MinionMode::Interactive => write!(f, "interactive"),
            MinionMode::Stopped => write!(f, "stopped"),
        }
    }
}

/// Tracks which phase of the fix orchestration a Minion has reached.
/// Used to resume interrupted sessions from the correct phase.
///
/// Variant order matters: derives `PartialOrd`/`Ord` so earlier phases compare
/// less than later ones (e.g., `Setup < RunningAgent < CreatingPr`).
/// Note: `running_claude` is accepted as a legacy alias for `running_agent`.
/// `Failed` sorts last since it is a terminal state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationPhase {
    /// Initial state - worktree setup in progress or not started
    #[default]
    Setup,
    /// Worktree created, agent session running
    #[serde(alias = "running_claude")]
    RunningAgent,
    /// Agent completed, PR creation in progress
    CreatingPr,
    /// PR created, monitoring lifecycle (reviews, CI)
    MonitoringPr,
    /// All phases completed successfully
    Completed,
    /// Task failed
    Failed,
}

/// Generates a default session ID for backwards compatibility
fn default_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generates a default last_activity timestamp for backwards compatibility
fn default_last_activity() -> DateTime<Utc> {
    Utc::now()
}

/// Metadata about a Minion tracked by the Lab
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinionInfo {
    /// Repository the Minion is working on (e.g., "fotoetienne/gru")
    pub repo: String,
    /// Issue number the Minion is addressing
    pub issue: u64,
    /// Command that started the Minion (e.g., "do", "review", "respond", "rebase")
    pub command: String,
    /// The prompt that was given to the Minion
    pub prompt: String,
    /// When the Minion was started (ISO 8601 timestamp)
    pub started_at: DateTime<Utc>,
    /// Git branch the Minion is working on
    pub branch: String,
    /// Worktree path where the Minion is working
    pub worktree: PathBuf,
    /// Current status (e.g., "active", "idle")
    pub status: String,
    /// PR number associated with this Minion (if any)
    pub pr: Option<String>,

    // Session lifecycle fields
    /// Claude Code session UUID for resume/attach operations
    #[serde(default = "default_session_id")]
    pub session_id: String,
    /// Process ID of the Claude Code process (None when not running)
    #[serde(default)]
    pub pid: Option<u32>,
    /// Current execution mode of the Minion
    #[serde(default)]
    pub mode: MinionMode,
    /// Timestamp of the last observed activity (for stuck detection)
    #[serde(default = "default_last_activity")]
    pub last_activity: DateTime<Utc>,
    /// Which orchestration phase this minion has reached (for resume after interruption)
    #[serde(default)]
    pub orchestration_phase: OrchestrationPhase,
    /// Accumulated token usage for this minion's session
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
    /// Name of the agent backend used by this minion (e.g., "claude", "codex")
    #[serde(default = "default_agent_name")]
    pub agent_name: String,
    /// Absolute deadline after which the minion should be timed out
    #[serde(default)]
    pub timeout_deadline: Option<DateTime<Utc>>,
    /// Number of attempts made for this minion (for retry tracking)
    #[serde(default)]
    pub attempt_count: u32,
    /// Whether to skip watching (PR monitoring) after agent completes
    #[serde(default)]
    pub no_watch: bool,
}

/// Default agent name for backwards compatibility with existing registry entries
fn default_agent_name() -> String {
    "claude".to_string()
}

impl MinionInfo {
    /// Returns the checkout path where the git worktree lives.
    ///
    /// New-style minions store the git worktree in `worktree/checkout/`.
    /// Legacy minions have the git worktree directly in `worktree/`.
    /// This method detects the layout at runtime and returns the correct path.
    pub fn checkout_path(&self) -> PathBuf {
        crate::workspace::resolve_checkout_path(&self.worktree)
    }
}

/// Checks whether a process with the given PID is still alive.
///
/// Uses `kill(pid, 0)` on Unix (signal 0 checks process existence without delivering a signal).
/// Returns `true` if the process exists and is owned by the current user.
/// Returns `false` if the process does not exist, or exists but is owned by a different user
/// (EPERM). Since Gru spawns Claude processes as the same user, EPERM is not expected in practice.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: On Linux and macOS, valid PIDs are positive integers well below i32::MAX
        // (typically max ~4 million). The cast from u32 is safe for all realistic PID values.
        // kill(pid, 0) is always safe to call — it performs a permission check without
        // delivering any signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Root structure for the minions registry file
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryData {
    minions: HashMap<String, MinionInfo>,
}

/// Registry for tracking all Minions managed by this Lab
///
/// The registry stores persistent metadata about Minions including:
/// - What command spawned them
/// - What repo and issue they're working on
/// - Associated PR numbers
/// - Start time and status
///
/// The registry is stored at `~/.gru/state/minions.json` and uses atomic
/// writes (temp file + rename) to prevent corruption.
///
/// File locking ensures that concurrent access to the registry is properly serialized.
pub struct MinionRegistry {
    /// Path to the registry file
    registry_path: PathBuf,
    /// In-memory registry data
    data: RegistryData,
    /// Lock file handle - holding this keeps the exclusive lock
    _lock_file: File,
}

impl MinionRegistry {
    /// Loads the registry from disk, or creates a new empty registry if the file doesn't exist
    ///
    /// # Arguments
    ///
    /// * `state_dir` - Optional custom state directory path. If None, uses `~/.gru/state/`.
    ///   This parameter is primarily for testing with isolated temp directories.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The state directory cannot be accessed
    /// - The registry file exists but cannot be read
    /// - The registry file contains invalid JSON
    pub fn load(state_dir: Option<&Path>) -> Result<Self> {
        let state_path = if let Some(custom_dir) = state_dir {
            // Test path: use provided directory and ensure it exists
            fs::create_dir_all(custom_dir)
                .with_context(|| format!("Failed to create state directory: {:?}", custom_dir))?;
            custom_dir.to_path_buf()
        } else {
            // Production path: use cached workspace
            Workspace::global()?.state().to_path_buf()
        };

        let registry_path = state_path.join("minions.json");
        let lock_path = state_path.join("minions.json.lock");

        // Open or create lock file
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("Failed to open lock file: {:?}", lock_path))?;

        // Acquire exclusive lock - this will block if another process holds the lock
        lock_file
            .lock_exclusive()
            .with_context(|| format!("Failed to acquire exclusive lock on {:?}", lock_path))?;

        // Load existing registry or create new one
        let data = if registry_path.exists() {
            let contents = fs::read_to_string(&registry_path)
                .with_context(|| format!("Failed to read registry file: {:?}", registry_path))?;

            serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse registry JSON: {:?}", registry_path))?
        } else {
            RegistryData {
                minions: HashMap::new(),
            }
        };

        Ok(MinionRegistry {
            registry_path,
            data,
            _lock_file: lock_file,
        })
    }

    /// Saves the registry to disk using atomic writes (temp file + rename)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The temp file cannot be created
    /// - The JSON cannot be serialized
    /// - The file cannot be written
    /// - The rename operation fails
    pub fn save(&self) -> Result<()> {
        // Serialize to pretty JSON
        let json = serde_json::to_string_pretty(&self.data)
            .context("Failed to serialize registry to JSON")?;

        // Write to temporary file in the same directory
        let temp_path = self.registry_path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .with_context(|| format!("Failed to create temp file: {:?}", temp_path))?;

        file.write_all(json.as_bytes())
            .with_context(|| format!("Failed to write to temp file: {:?}", temp_path))?;

        file.sync_all()
            .with_context(|| format!("Failed to sync temp file: {:?}", temp_path))?;

        drop(file); // Close the file before renaming

        // Atomically rename temp file to registry file
        fs::rename(&temp_path, &self.registry_path).with_context(|| {
            format!(
                "Failed to rename temp file {:?} to {:?}",
                temp_path, self.registry_path
            )
        })?;

        Ok(())
    }

    /// Registers a new Minion in the registry
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier (e.g., "M001")
    /// * `info` - Metadata about the Minion
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A Minion with the same ID already exists
    /// - The registry cannot be saved to disk
    pub fn register(&mut self, minion_id: String, info: MinionInfo) -> Result<()> {
        if self.data.minions.contains_key(&minion_id) {
            anyhow::bail!("Minion {} is already registered", minion_id);
        }

        self.data.minions.insert(minion_id, info);
        self.save()
            .context("Failed to save registry after registering Minion")?;
        Ok(())
    }

    /// Updates an existing Minion's metadata
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier
    /// * `update_fn` - Function that takes a mutable reference to the MinionInfo and updates it
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The Minion ID does not exist in the registry
    /// - The registry cannot be saved to disk
    pub fn update<F>(&mut self, minion_id: &str, update_fn: F) -> Result<()>
    where
        F: FnOnce(&mut MinionInfo),
    {
        let info = self
            .data
            .minions
            .get_mut(minion_id)
            .with_context(|| format!("Minion {} not found in registry", minion_id))?;

        update_fn(info);

        self.save()
            .context("Failed to save registry after updating Minion")?;
        Ok(())
    }

    /// Returns all Minions in the registry
    pub fn list(&self) -> Vec<(String, MinionInfo)> {
        self.data
            .minions
            .iter()
            .map(|(id, info)| (id.clone(), info.clone()))
            .collect()
    }

    /// Checks if a Minion exists in the registry
    #[cfg(test)]
    pub fn exists(&self, minion_id: &str) -> bool {
        self.data.minions.contains_key(minion_id)
    }

    /// Gets a Minion's metadata by ID
    pub fn get(&self, minion_id: &str) -> Option<&MinionInfo> {
        self.data.minions.get(minion_id)
    }

    /// Finds all Minions associated with a specific issue in a specific repo
    ///
    /// Returns all matching Minions as (minion_id, MinionInfo) pairs regardless
    /// of mode or PID status (including stopped entries). Callers should check
    /// `is_process_alive` to determine which Minions are actually running.
    pub fn find_by_issue(&self, repo: &str, issue: u64) -> Vec<(String, MinionInfo)> {
        self.data
            .minions
            .iter()
            .filter(|(_, info)| info.repo == repo && info.issue == issue)
            .map(|(id, info)| (id.clone(), info.clone()))
            .collect()
    }

    /// Removes a Minion from the registry
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be saved to disk
    pub fn remove(&mut self, minion_id: &str) -> Result<Option<MinionInfo>> {
        let removed = self.data.minions.remove(minion_id);
        if removed.is_some() {
            self.save()
                .context("Failed to save registry after removing Minion")?;
        }
        Ok(removed)
    }

    /// Removes multiple Minions from the registry in a single save operation.
    ///
    /// Returns the number of minions actually removed (i.e., that existed in the registry).
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be saved to disk
    pub fn remove_batch(&mut self, minion_ids: &[String]) -> Result<usize> {
        let mut count = 0;
        for id in minion_ids {
            if self.data.minions.remove(id).is_some() {
                count += 1;
            }
        }
        if count > 0 {
            self.save()
                .context("Failed to save registry after batch removal")?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Creates a test MinionInfo with sensible defaults
    fn test_minion_info() -> MinionInfo {
        let now = Utc::now();
        MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "do".to_string(),
            prompt: "Do issue #42".to_string(),
            started_at: now,
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            pid: None,
            mode: MinionMode::Autonomous,
            last_activity: now,
            orchestration_phase: OrchestrationPhase::Setup,
            token_usage: None,
            agent_name: "claude".to_string(),
            timeout_deadline: None,
            attempt_count: 0,
            no_watch: false,
        }
    }

    #[test]
    fn test_load_creates_empty_registry() {
        let temp_dir = tempdir().unwrap();
        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        assert_eq!(registry.list().len(), 0);
    }

    #[test]
    fn test_register_and_list() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();
        registry.register("M001".to_string(), info.clone()).unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "M001");
        assert_eq!(list[0].1.repo, "fotoetienne/gru");
        assert_eq!(list[0].1.issue, 42);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();
        registry.register("M001".to_string(), info.clone()).unwrap();
        let result = registry.register("M001".to_string(), info);
        assert!(result.is_err());
    }

    #[test]
    fn test_update() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();
        registry.register("M001".to_string(), info).unwrap();

        registry
            .update("M001", |info| {
                info.pr = Some("71".to_string());
                info.status = "idle".to_string();
            })
            .unwrap();

        let updated = registry.get("M001").unwrap();
        assert_eq!(updated.pr, Some("71".to_string()));
        assert_eq!(updated.status, "idle");
    }

    #[test]
    fn test_update_nonexistent_fails() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let result = registry.update("M999", |info| {
            info.status = "idle".to_string();
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_persistence() {
        let temp_dir = tempdir().unwrap();

        let info = test_minion_info();

        // Create registry and register a minion
        {
            let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            registry.register("M001".to_string(), info).unwrap();
        }

        // Load registry again and verify data persisted
        {
            let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            let list = registry.list();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].0, "M001");
            assert_eq!(list[0].1.repo, "fotoetienne/gru");
        }
    }

    #[test]
    fn test_exists() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        assert!(!registry.exists("M001"));

        let info = test_minion_info();
        registry.register("M001".to_string(), info).unwrap();
        assert!(registry.exists("M001"));
    }

    #[test]
    fn test_get() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        assert!(registry.get("M001").is_none());

        let info = test_minion_info();
        registry.register("M001".to_string(), info).unwrap();
        let retrieved = registry.get("M001").unwrap();
        assert_eq!(retrieved.repo, "fotoetienne/gru");
        assert_eq!(retrieved.issue, 42);
    }

    #[test]
    fn test_remove() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();

        registry.register("M001".to_string(), info).unwrap();
        assert!(registry.exists("M001"));

        let removed = registry.remove("M001").unwrap();
        assert!(removed.is_some());
        assert!(!registry.exists("M001"));

        // Removing again should return None
        let removed2 = registry.remove("M001").unwrap();
        assert!(removed2.is_none());
    }

    #[test]
    fn test_remove_batch() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();
        registry.register("M001".to_string(), info.clone()).unwrap();
        registry.register("M002".to_string(), info.clone()).unwrap();
        registry.register("M003".to_string(), info).unwrap();
        assert_eq!(registry.list().len(), 3);

        // Batch remove two existing and one non-existent
        let count = registry
            .remove_batch(&["M001".to_string(), "M003".to_string(), "M999".to_string()])
            .unwrap();
        assert_eq!(count, 2);
        assert!(!registry.exists("M001"));
        assert!(registry.exists("M002"));
        assert!(!registry.exists("M003"));

        // Verify persistence
        drop(registry);
        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        assert_eq!(registry.list().len(), 1);
        assert!(registry.exists("M002"));
    }

    #[test]
    fn test_remove_batch_empty() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = test_minion_info();
        registry.register("M001".to_string(), info).unwrap();

        // Batch remove with empty list should be a no-op
        let count = registry.remove_batch(&[]).unwrap();
        assert_eq!(count, 0);
        assert!(registry.exists("M001"));
    }

    #[test]
    fn test_session_lifecycle_fields_persisted() {
        let temp_dir = tempdir().unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();

        let info = MinionInfo {
            session_id: session_id.clone(),
            pid: Some(12345),
            mode: MinionMode::Autonomous,
            ..test_minion_info()
        };

        {
            let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            registry.register("M001".to_string(), info).unwrap();
        }

        // Reload and verify lifecycle fields persisted
        {
            let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            let retrieved = registry.get("M001").unwrap();
            assert_eq!(retrieved.session_id, session_id);
            assert_eq!(retrieved.pid, Some(12345));
            assert_eq!(retrieved.mode, MinionMode::Autonomous);
        }
    }

    #[test]
    fn test_update_lifecycle_fields() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            pid: Some(9999),
            mode: MinionMode::Autonomous,
            ..test_minion_info()
        };

        registry.register("M001".to_string(), info).unwrap();

        // Simulate process exit: clear PID and set mode to Stopped
        registry
            .update("M001", |info| {
                info.pid = None;
                info.mode = MinionMode::Stopped;
            })
            .unwrap();

        let updated = registry.get("M001").unwrap();
        assert_eq!(updated.pid, None);
        assert_eq!(updated.mode, MinionMode::Stopped);
    }

    #[test]
    fn test_backwards_compatibility_missing_fields() {
        let temp_dir = tempdir().unwrap();
        let registry_path = temp_dir.path().join("minions.json");

        // Write a registry JSON without the new fields (simulating old format)
        let old_json = r#"{
            "minions": {
                "M001": {
                    "repo": "fotoetienne/gru",
                    "issue": 42,
                    "command": "fix",
                    "prompt": "Fix issue #42",
                    "started_at": "2024-01-01T00:00:00Z",
                    "branch": "minion/issue-42-M001",
                    "worktree": "/tmp/test",
                    "status": "active",
                    "pr": null
                }
            }
        }"#;
        fs::write(&registry_path, old_json).unwrap();

        // Loading should succeed with defaults applied
        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        let info = registry.get("M001").unwrap();

        assert_eq!(info.repo, "fotoetienne/gru");
        assert_eq!(info.pid, None);
        assert_eq!(info.mode, MinionMode::Stopped);
        // session_id should get a default UUID
        assert!(!info.session_id.is_empty());
        // token_usage should default to None for old registry entries
        assert!(info.token_usage.is_none());
        // New fields should default correctly
        assert!(info.timeout_deadline.is_none());
        assert_eq!(info.attempt_count, 0);
        assert!(!info.no_watch);
    }

    #[test]
    fn test_minion_mode_serialization() {
        // Verify MinionMode serializes to lowercase strings
        let autonomous = serde_json::to_string(&MinionMode::Autonomous).unwrap();
        assert_eq!(autonomous, r#""autonomous""#);

        let interactive = serde_json::to_string(&MinionMode::Interactive).unwrap();
        assert_eq!(interactive, r#""interactive""#);

        let stopped = serde_json::to_string(&MinionMode::Stopped).unwrap();
        assert_eq!(stopped, r#""stopped""#);

        // Verify deserialization
        let mode: MinionMode = serde_json::from_str(r#""autonomous""#).unwrap();
        assert_eq!(mode, MinionMode::Autonomous);
    }

    #[test]
    fn test_is_process_alive_with_current_process() {
        // Our own process should be alive
        let our_pid = std::process::id();
        assert!(is_process_alive(our_pid));
    }

    #[test]
    fn test_is_process_alive_with_nonexistent_pid() {
        // Use a high but valid PID (won't overflow i32 to -1 or 0)
        // PID 4194304 exceeds the typical Linux/macOS PID max
        assert!(!is_process_alive(4_194_304));
    }

    #[test]
    fn test_minion_mode_default() {
        assert_eq!(MinionMode::default(), MinionMode::Stopped);
    }

    #[test]
    fn test_find_by_issue_returns_matching_minions() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info1 = MinionInfo {
            issue: 42,
            repo: "owner/repo".to_string(),
            ..test_minion_info()
        };
        let info2 = MinionInfo {
            issue: 42,
            repo: "owner/repo".to_string(),
            ..test_minion_info()
        };
        let info3 = MinionInfo {
            issue: 99,
            repo: "owner/repo".to_string(),
            ..test_minion_info()
        };

        registry.register("M001".to_string(), info1).unwrap();
        registry.register("M002".to_string(), info2).unwrap();
        registry.register("M003".to_string(), info3).unwrap();

        let results = registry.find_by_issue("owner/repo", 42);
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|(id, _)| id == "M001"));
        assert!(results.iter().any(|(id, _)| id == "M002"));
    }

    #[test]
    fn test_find_by_issue_no_match() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            issue: 42,
            repo: "owner/repo".to_string(),
            ..test_minion_info()
        };
        registry.register("M001".to_string(), info).unwrap();

        // Different issue number
        assert!(registry.find_by_issue("owner/repo", 99).is_empty());

        // Different repo
        assert!(registry.find_by_issue("other/repo", 42).is_empty());
    }

    #[test]
    fn test_find_by_issue_empty_registry() {
        let temp_dir = tempdir().unwrap();
        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        assert!(registry.find_by_issue("owner/repo", 42).is_empty());
    }

    #[test]
    fn test_orchestration_phase_serialization() {
        let setup = serde_json::to_string(&OrchestrationPhase::Setup).unwrap();
        assert_eq!(setup, r#""setup""#);

        let running = serde_json::to_string(&OrchestrationPhase::RunningAgent).unwrap();
        assert_eq!(running, r#""running_agent""#);

        let creating_pr = serde_json::to_string(&OrchestrationPhase::CreatingPr).unwrap();
        assert_eq!(creating_pr, r#""creating_pr""#);

        let monitoring = serde_json::to_string(&OrchestrationPhase::MonitoringPr).unwrap();
        assert_eq!(monitoring, r#""monitoring_pr""#);

        let completed = serde_json::to_string(&OrchestrationPhase::Completed).unwrap();
        assert_eq!(completed, r#""completed""#);

        let failed = serde_json::to_string(&OrchestrationPhase::Failed).unwrap();
        assert_eq!(failed, r#""failed""#);

        // Verify deserialization of both old and new names
        let phase: OrchestrationPhase = serde_json::from_str(r#""running_agent""#).unwrap();
        assert_eq!(phase, OrchestrationPhase::RunningAgent);
        let phase: OrchestrationPhase = serde_json::from_str(r#""running_claude""#).unwrap();
        assert_eq!(phase, OrchestrationPhase::RunningAgent);
    }

    #[test]
    fn test_orchestration_phase_default() {
        assert_eq!(OrchestrationPhase::default(), OrchestrationPhase::Setup);
    }

    #[test]
    fn test_orchestration_phase_persisted() {
        let temp_dir = tempdir().unwrap();

        let info = MinionInfo {
            orchestration_phase: OrchestrationPhase::RunningAgent,
            ..test_minion_info()
        };

        {
            let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            registry.register("M001".to_string(), info).unwrap();
        }

        // Reload and verify
        {
            let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            let retrieved = registry.get("M001").unwrap();
            assert_eq!(
                retrieved.orchestration_phase,
                OrchestrationPhase::RunningAgent
            );
        }
    }

    #[test]
    fn test_new_fields_persisted() {
        let temp_dir = tempdir().unwrap();
        let deadline = Utc::now() + chrono::Duration::hours(1);

        let info = MinionInfo {
            timeout_deadline: Some(deadline),
            attempt_count: 3,
            no_watch: true,
            ..test_minion_info()
        };

        {
            let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            registry.register("M001".to_string(), info).unwrap();
        }

        // Reload and verify
        {
            let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            let retrieved = registry.get("M001").unwrap();
            assert_eq!(
                retrieved.timeout_deadline.unwrap().timestamp(),
                deadline.timestamp()
            );
            assert_eq!(retrieved.attempt_count, 3);
            assert!(retrieved.no_watch);
        }
    }

    #[test]
    fn test_orchestration_phase_backwards_compat() {
        let temp_dir = tempdir().unwrap();
        let registry_path = temp_dir.path().join("minions.json");

        // Write a registry JSON without orchestration_phase (simulating old format)
        let old_json = r#"{
            "minions": {
                "M001": {
                    "repo": "fotoetienne/gru",
                    "issue": 42,
                    "command": "fix",
                    "prompt": "Fix issue #42",
                    "started_at": "2024-01-01T00:00:00Z",
                    "branch": "minion/issue-42-M001",
                    "worktree": "/tmp/test",
                    "status": "active",
                    "pr": null,
                    "session_id": "550e8400-e29b-41d4-a716-446655440000",
                    "pid": null,
                    "mode": "stopped",
                    "last_activity": "2024-01-01T00:00:00Z"
                }
            }
        }"#;
        fs::write(&registry_path, old_json).unwrap();

        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        let info = registry.get("M001").unwrap();
        assert_eq!(info.orchestration_phase, OrchestrationPhase::Setup);
    }
}
