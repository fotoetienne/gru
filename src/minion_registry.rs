use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::agent::TokenUsage;
use crate::file_lock::lock_with_timeout;
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
            info.clear_pid();
            info.last_activity = Utc::now();
        })
    })
    .await;
}

/// Atomically mark a minion as failed: clear the process claim and set the
/// orchestration phase to [`OrchestrationPhase::Failed`] in a single registry write.
pub async fn mark_minion_failed(minion_id: &str) {
    let mid = minion_id.to_string();
    let _ = with_registry(move |reg| {
        reg.update(&mid, |info| {
            info.mode = MinionMode::Stopped;
            info.clear_pid();
            info.orchestration_phase = OrchestrationPhase::Failed;
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

impl OrchestrationPhase {
    /// Returns true if this phase represents an active (in-progress) state
    /// that can be resumed after interruption.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            OrchestrationPhase::RunningAgent
                | OrchestrationPhase::CreatingPr
                | OrchestrationPhase::MonitoringPr
        )
    }

    /// Returns true if this phase is a terminal state (Completed or Failed).
    /// Minions in terminal states do not block new attempts for the same issue.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrchestrationPhase::Completed | OrchestrationPhase::Failed
        )
    }
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
    /// Process start time (seconds since epoch) recorded at spawn.
    /// Used to detect PID reuse: if the OS-reported start time for the PID
    /// differs from this value, the PID was recycled to a different process.
    #[serde(default)]
    pub pid_start_time: Option<i64>,
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

    /// Clears the PID and its associated start time.
    pub fn clear_pid(&mut self) {
        self.pid = None;
        self.pid_start_time = None;
    }

    /// Checks whether this minion's process is still alive, accounting for PID reuse.
    ///
    /// Returns `true` only if:
    /// 1. A PID is recorded
    /// 2. The process exists (via `kill(pid, 0)`)
    /// 3. The process start time matches what was recorded at spawn (if available)
    ///
    /// If `pid_start_time` is `None` (legacy entries), falls back to the basic kill check.
    pub fn is_running(&self) -> bool {
        match self.pid {
            Some(pid) => is_process_alive_with_start_time(pid, self.pid_start_time),
            None => false,
        }
    }
}

/// Checks whether a process with the given PID is still alive.
///
/// Uses `kill(pid, 0)` on Unix (signal 0 checks process existence without delivering a signal).
/// Returns `true` if the process exists and is owned by the current user.
/// Returns `false` if the process does not exist, or exists but is owned by a different user
/// (EPERM). Since Gru spawns Claude processes as the same user, EPERM is not expected in practice.
///
/// **Warning:** This does NOT detect PID reuse. Prefer [`is_process_alive_with_start_time`]
/// or [`MinionInfo::is_running`] when a recorded start time is available.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Guard against PIDs that cannot be represented as a positive i32: kill() with a
        // negative pid sends to a process group instead of a single process.
        let Ok(pid_i32) = i32::try_from(pid) else {
            return false;
        };
        // kill(pid, 0) is always safe to call — it performs a permission check without
        // delivering any signal.
        unsafe { libc::kill(pid_i32, 0) == 0 }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Like [`is_process_alive`], but also verifies the process start time to detect PID reuse.
///
/// If `recorded_start_time` is `Some`, the OS-reported start time for the PID is compared
/// against it. If they differ, the PID was recycled to a different process and this returns
/// `false`. If `recorded_start_time` is `None` (legacy entries), falls back to the basic
/// kill-signal check.
pub fn is_process_alive_with_start_time(pid: u32, recorded_start_time: Option<i64>) -> bool {
    if !is_process_alive(pid) {
        return false;
    }

    // If we have a recorded start time, verify the PID still belongs to the same process.
    if let Some(recorded) = recorded_start_time {
        match get_process_start_time(pid) {
            Some(actual) => {
                if actual != recorded {
                    log::debug!(
                        "PID {} recycled: recorded start_time={}, actual={}",
                        pid,
                        recorded,
                        actual
                    );
                    return false;
                }
            }
            None => {
                // Couldn't query start time (process may have just exited between
                // the kill() check and this query). Fall through to return true —
                // callers like gru stop will simply get ESRCH when they try to
                // signal the PID, which is harmless.
            }
        }
    }

    true
}

/// Returns the start time (seconds since epoch) of a process, or `None` if unavailable.
///
/// On macOS, uses `sysctl` with `KERN_PROC/KERN_PROC_PID`.
/// On Linux, reads `/proc/<pid>/stat` and combines with boot time from `/proc/stat`.
#[cfg(unix)]
pub fn get_process_start_time(pid: u32) -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        get_process_start_time_macos(pid)
    }
    #[cfg(target_os = "linux")]
    {
        get_process_start_time_linux(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(not(unix))]
pub fn get_process_start_time(_pid: u32) -> Option<i64> {
    None
}

#[cfg(target_os = "macos")]
fn get_process_start_time_macos(pid: u32) -> Option<i64> {
    // Use `proc_pidinfo` with PROC_PIDTBSDINFO to get the process start time.
    // This avoids needing the kinfo_proc struct from libc (not exposed for macOS).
    use std::mem;

    // proc_bsdinfo struct layout (from <sys/proc_info.h>)
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: libc::uid_t,
        pbi_gid: libc::gid_t,
        pbi_ruid: libc::uid_t,
        pbi_rgid: libc::gid_t,
        pbi_svuid: libc::uid_t,
        pbi_svgid: libc::gid_t,
        _rfu_1: u32,
        pbi_comm: [libc::c_char; 16], // MAXCOMLEN
        pbi_name: [libc::c_char; 32], // 2 * MAXCOMLEN
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    const PROC_PIDTBSDINFO: libc::c_int = 3;

    extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    let mut info: ProcBsdInfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<ProcBsdInfo>() as libc::c_int;

    let ret = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };

    if ret <= 0 {
        return None;
    }

    Some(info.pbi_start_tvsec as i64)
}

#[cfg(target_os = "linux")]
fn get_process_start_time_linux(pid: u32) -> Option<i64> {
    // Read /proc/<pid>/stat to get start time in clock ticks since boot
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // Field 22 (1-indexed) is starttime. The comm field (2) may contain spaces/parens,
    // so find the last ')' first.
    let after_comm = stat.rfind(')')? + 2; // skip ") "
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    // starttime is field 20 after comm (0-indexed from after_comm)
    let start_ticks: u64 = fields.get(19)?.parse().ok()?;

    // Get system boot time from /proc/stat
    let proc_stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime_line = proc_stat.lines().find(|l| l.starts_with("btime "))?;
    let boot_time: i64 = btime_line.split_whitespace().nth(1)?.parse().ok()?;

    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }

    Some(boot_time + (start_ticks as i64 / ticks_per_sec))
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

        // Acquire exclusive lock with timeout to avoid deadlock from hung processes
        lock_with_timeout(&lock_file)
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

    /// Creates a boxed callback that registers a child PID in the minion
    /// registry when the agent process spawns.
    ///
    /// If `mode` is `Some`, the minion's mode is also updated (e.g. to
    /// `Autonomous`). All callbacks unconditionally set `pid` and
    /// `last_activity`.
    ///
    /// The callback uses synchronous `MinionRegistry::load` because the
    /// `on_spawn` contract is `FnOnce(u32) + Send` (not async). The registry
    /// update is dispatched to a background thread so that lock contention
    /// (with retry backoff) does not block the Tokio worker thread.
    pub fn pid_callback(
        minion_id: String,
        mode: Option<MinionMode>,
    ) -> Box<dyn FnOnce(u32) + Send> {
        Box::new(move |pid: u32| {
            // Capture the process start time immediately so we can detect PID reuse later.
            let start_time = get_process_start_time(pid);
            let _ = std::thread::Builder::new()
                .name("pid-callback".into())
                .spawn(move || match MinionRegistry::load(None) {
                    Ok(mut registry) => {
                        if let Err(e) = registry.update(&minion_id, |info| {
                            info.pid = Some(pid);
                            info.pid_start_time = start_time;
                            if let Some(m) = mode {
                                info.mode = m;
                            }
                            info.last_activity = Utc::now();
                        }) {
                            log::warn!(
                                "pid_callback: failed to update registry for {minion_id}: {e:#}"
                            );
                        }
                    }
                    Err(e) => {
                        log::warn!("pid_callback: failed to load registry for {minion_id}: {e:#}");
                    }
                });
        })
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
    /// [`MinionInfo::is_running`] to determine which Minions are actually running.
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
            pid_start_time: None,
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
                info.clear_pid();
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
        // PID 4194304 exceeds the typical Linux/macOS PID max
        assert!(!is_process_alive(4_194_304));
    }

    #[test]
    fn test_is_process_alive_rejects_pid_above_i32_max() {
        // PIDs above i32::MAX cannot be safely passed to kill(); verify they are rejected.
        assert!(!is_process_alive(i32::MAX as u32 + 1));
        assert!(!is_process_alive(u32::MAX));
    }

    #[test]
    fn test_is_process_alive_allows_i32_max_boundary() {
        // i32::MAX is the largest PID that can be safely cast; it should not be rejected
        // by the guard. It almost certainly doesn't exist, so the result is false.
        assert!(!is_process_alive(i32::MAX as u32));
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
            assert_eq!(retrieved.timeout_deadline.unwrap(), deadline);
            assert_eq!(retrieved.attempt_count, 3);
            assert!(retrieved.no_watch);
        }
    }

    #[test]
    fn test_orchestration_phase_is_active() {
        assert!(!OrchestrationPhase::Setup.is_active());
        assert!(OrchestrationPhase::RunningAgent.is_active());
        assert!(OrchestrationPhase::CreatingPr.is_active());
        assert!(OrchestrationPhase::MonitoringPr.is_active());
        assert!(!OrchestrationPhase::Completed.is_active());
        assert!(!OrchestrationPhase::Failed.is_active());
    }

    #[test]
    fn test_orchestration_phase_is_terminal() {
        assert!(!OrchestrationPhase::Setup.is_terminal());
        assert!(!OrchestrationPhase::RunningAgent.is_terminal());
        assert!(!OrchestrationPhase::CreatingPr.is_terminal());
        assert!(!OrchestrationPhase::MonitoringPr.is_terminal());
        assert!(OrchestrationPhase::Completed.is_terminal());
        assert!(OrchestrationPhase::Failed.is_terminal());
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

    #[test]
    fn test_get_process_start_time_current_process() {
        let pid = std::process::id();
        let start_time = get_process_start_time(pid);
        // On macOS/Linux, we should be able to get our own start time
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            start_time.is_some(),
            "Should get start time for own process"
        );
        // The start time should be a reasonable value (after 2020-01-01)
        if let Some(t) = start_time {
            assert!(t > 1_577_836_800, "Start time should be after 2020: {}", t);
        }
    }

    #[test]
    fn test_get_process_start_time_nonexistent() {
        // Very high PID that doesn't exist
        let start_time = get_process_start_time(4_194_304);
        assert!(start_time.is_none());
    }

    #[test]
    fn test_is_process_alive_with_start_time_matching() {
        let pid = std::process::id();
        let start_time = get_process_start_time(pid);
        // With matching start time, should return true
        assert!(is_process_alive_with_start_time(pid, start_time));
    }

    #[test]
    fn test_is_process_alive_with_start_time_mismatched() {
        let pid = std::process::id();
        // With a start time from the distant past, should detect as recycled
        let fake_start_time = Some(1_000_000_000_i64); // ~2001
        assert!(
            !is_process_alive_with_start_time(pid, fake_start_time),
            "Should detect PID reuse when start times differ"
        );
    }

    #[test]
    fn test_is_process_alive_with_start_time_none_fallback() {
        let pid = std::process::id();
        // With no recorded start time (legacy), falls back to basic kill check
        assert!(is_process_alive_with_start_time(pid, None));
    }

    #[test]
    fn test_is_process_alive_with_start_time_dead_pid() {
        // Non-existent PID should return false regardless of start time
        assert!(!is_process_alive_with_start_time(4_194_304, None));
        assert!(!is_process_alive_with_start_time(
            4_194_304,
            Some(1_000_000)
        ));
    }

    #[test]
    fn test_minion_info_is_running() {
        let mut info = test_minion_info();
        // No PID → not running
        assert!(!info.is_running());

        // Set PID to our own process with correct start time
        let pid = std::process::id();
        info.pid = Some(pid);
        info.pid_start_time = get_process_start_time(pid);
        assert!(info.is_running());

        // Set a fake start time → PID reuse detected → not running
        info.pid_start_time = Some(1_000_000_000);
        assert!(!info.is_running());
    }

    #[test]
    fn test_minion_info_clear_pid() {
        let mut info = test_minion_info();
        info.pid = Some(12345);
        info.pid_start_time = Some(1_700_000_000);

        info.clear_pid();
        assert_eq!(info.pid, None);
        assert_eq!(info.pid_start_time, None);
    }

    #[test]
    fn test_pid_start_time_backwards_compat() {
        let temp_dir = tempdir().unwrap();
        let registry_path = temp_dir.path().join("minions.json");

        // Write a registry JSON without pid_start_time (simulating old format)
        let old_json = r#"{
            "minions": {
                "M001": {
                    "repo": "owner/repo",
                    "issue": 42,
                    "command": "do",
                    "prompt": "Fix issue",
                    "started_at": "2024-01-01T00:00:00Z",
                    "branch": "minion/issue-42-M001",
                    "worktree": "/tmp/test",
                    "status": "active",
                    "session_id": "abc-123",
                    "pid": 12345,
                    "mode": "autonomous"
                }
            }
        }"#;
        fs::write(&registry_path, old_json).unwrap();

        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        let info = registry.get("M001").unwrap();
        assert_eq!(info.pid, Some(12345));
        // pid_start_time should default to None for old entries
        assert_eq!(info.pid_start_time, None);
    }
}
