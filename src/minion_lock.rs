//! Per-minion advisory lockfile for defence-in-depth against duplicate agent
//! subprocesses.
//!
//! The registry-level check in `session_claim::check_and_claim_session` is the
//! primary mechanism that prevents two processes from running an agent against
//! the same minion session. This module adds a kernel-enforced advisory lock
//! that holds for the lifetime of the owning process, so any future registry
//! bug (bad mode transition, stale PID not detected, concurrent write race in
//! the JSON file) still cannot produce two live agent subprocesses for the
//! same minion.
//!
//! Acquire the lock in the three process entry points that spawn an agent:
//!   - `fix::run_worker` (the background worker for `gru do`).
//!   - `resume::run_resume_pipeline` (for `gru resume`).
//!   - `attach::handle_attach`, just before spawning the interactive agent.
//!
//! The lock is released automatically on normal exit, panic, or SIGKILL — the
//! kernel drops the file lock when the fd is closed, so no shutdown handler is
//! required.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::session_claim::SessionClaimError;
use crate::workspace::Workspace;

/// Subdirectory under `~/.gru/state/` holding per-minion lockfiles.
const MINION_LOCKS_SUBDIR: &str = "minions";

/// Computes the lockfile path `<state>/minions/<minion_id>.lock`.
fn lock_path(state_dir: &Path, minion_id: &str) -> PathBuf {
    state_dir
        .join(MINION_LOCKS_SUBDIR)
        .join(format!("{minion_id}.lock"))
}

/// RAII guard holding an exclusive advisory lock on a per-minion lockfile.
///
/// When this guard is dropped the kernel releases the lock (via fd close).
/// Holding the fd is what enforces the lock — callers should keep this guard
/// alive for as long as they own the minion's agent process.
pub(crate) struct MinionLock {
    // Held only for its Drop impl / underlying fd; not otherwise accessed.
    #[allow(dead_code)]
    file: File,
    minion_id: String,
    path: PathBuf,
}

impl MinionLock {
    /// Attempts to acquire an exclusive non-blocking lock on
    /// `~/.gru/state/minions/<minion_id>.lock`.
    ///
    /// Returns [`SessionClaimError::AlreadyRunning`] if another process
    /// already holds the lock. Other IO failures propagate as
    /// [`anyhow::Error`].
    ///
    /// **Important:** the `mode` field of the returned error is set to
    /// [`MinionMode::Autonomous`] as a placeholder — we cannot read the
    /// registry for the actual mode without re-entering registry code. The
    /// value is only meaningful for the error's `Display` message. Callers
    /// (specifically `handle_attach`) that branch on `mode == Autonomous`
    /// to decide whether to auto-stop a running minion MUST acquire the
    /// advisory lock **after** the registry-level
    /// `check_and_claim_session` — never before — or the hardcoded mode
    /// will route a kernel-lock contention into the auto-stop path.
    pub(crate) fn try_acquire(minion_id: &str) -> Result<Self> {
        let ws = Workspace::global().context("Failed to initialize workspace for minion lock")?;
        Self::try_acquire_in_dir(ws.state(), minion_id)
    }

    /// Test-friendly variant that acquires the lock under an explicit state
    /// directory rather than the global workspace. Used by unit tests to
    /// avoid clashing with the real `~/.gru/state/minions/`.
    pub(crate) fn try_acquire_in_dir(state_dir: &Path, minion_id: &str) -> Result<Self> {
        let path = lock_path(state_dir, minion_id);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create lock directory {}", parent.display()))?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open minion lock file {}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(MinionLock {
                file,
                minion_id: minion_id.to_string(),
                path,
            }),
            Err(e) if is_lock_contention(&e) => Err(SessionClaimError::AlreadyRunning {
                minion_id: minion_id.to_string(),
                mode: crate::minion_registry::MinionMode::Autonomous,
            }
            .into()),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("Failed to acquire minion lock {}", path.display()))),
        }
    }

    /// The minion ID associated with this lock (for diagnostics / tests).
    #[cfg(test)]
    pub(crate) fn minion_id(&self) -> &str {
        &self.minion_id
    }

    /// The path of the lockfile (for diagnostics / tests).
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for MinionLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinionLock")
            .field("minion_id", &self.minion_id)
            .field("path", &self.path)
            .finish()
    }
}

/// Returns true if the error represents lock contention (file already locked).
///
/// Mirrors the detection logic in [`crate::file_lock`] — kept local so this
/// module can be used without pulling in the registry-level locking helpers.
fn is_lock_contention(e: &io::Error) -> bool {
    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
        return true;
    }
    #[cfg(unix)]
    {
        if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return true;
        }
    }
    #[cfg(windows)]
    if e.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_succeeds_on_first_call() {
        let tmp = tempdir().unwrap();
        let lock = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap();
        assert_eq!(lock.minion_id(), "M001");
        assert!(lock.path().exists(), "lockfile should be created on disk");
    }

    #[test]
    fn second_acquire_fails_with_already_running() {
        let tmp = tempdir().unwrap();
        let _first = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap();

        let err = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap_err();
        let claim = err
            .downcast_ref::<SessionClaimError>()
            .expect("second acquire must surface SessionClaimError::AlreadyRunning");
        let SessionClaimError::AlreadyRunning { minion_id, .. } = claim;
        assert_eq!(minion_id, "M001");
    }

    #[test]
    fn drop_releases_lock() {
        let tmp = tempdir().unwrap();
        {
            let _lock = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap();
            // Lock held inside this scope.
        }
        // After drop, a fresh acquire should succeed.
        let _lock2 = MinionLock::try_acquire_in_dir(tmp.path(), "M001")
            .expect("lock should be reacquirable after drop");
    }

    #[test]
    fn different_minions_do_not_conflict() {
        let tmp = tempdir().unwrap();
        let _a = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap();
        // A different minion ID must not be blocked by M001's lock.
        let _b = MinionLock::try_acquire_in_dir(tmp.path(), "M002").unwrap();
    }

    #[test]
    fn lock_directory_is_created_lazily() {
        let tmp = tempdir().unwrap();
        let minions_dir = tmp.path().join(MINION_LOCKS_SUBDIR);
        assert!(
            !minions_dir.exists(),
            "precondition: minions/ does not yet exist"
        );

        let _lock = MinionLock::try_acquire_in_dir(tmp.path(), "M042").unwrap();
        assert!(
            minions_dir.is_dir(),
            "minions/ must be created on first acquire"
        );
    }

    #[test]
    fn lock_across_processes_is_simulated_by_separate_fds() {
        // fs2 file locks are per-fd on the OS, so reopening the same path and
        // calling try_lock_exclusive is equivalent to another process trying
        // to take the lock. This exercises the same code path as the
        // "concurrent worker" integration test at a unit-test level.
        let tmp = tempdir().unwrap();
        let _holder = MinionLock::try_acquire_in_dir(tmp.path(), "M077").unwrap();

        let path = lock_path(tmp.path(), "M077");
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        let err = contender
            .try_lock_exclusive()
            .expect_err("contender must not be able to lock held file");
        assert!(is_lock_contention(&err), "unexpected error kind: {err}");
    }

    /// Integration-style test (acceptance criterion for issue #865): spawn
    /// multiple concurrent workers against the same minion and confirm
    /// exactly one acquires the lock while the others fail with
    /// `SessionClaimError::AlreadyRunning`.
    ///
    /// Uses real OS threads rather than tokio tasks so each acquisition
    /// runs in parallel on distinct file descriptors (fs2 uses flock(2) on
    /// Unix, which is per-OFD — two opens from the same process still
    /// contend, so this models real cross-process behaviour).
    #[test]
    fn concurrent_workers_only_one_proceeds() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        const WORKERS: usize = 8;
        let barrier = Arc::new(Barrier::new(WORKERS));

        let handles: Vec<_> = (0..WORKERS)
            .map(|_| {
                let state_dir = state_dir.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || -> Result<MinionLock> {
                    barrier.wait();
                    MinionLock::try_acquire_in_dir(&state_dir, "M100")
                })
            })
            .collect();

        let mut acquired = 0;
        let mut already_running = 0;
        let mut other_err = 0;

        // Keep the winning lock alive until all contenders have been inspected
        // so that a slow thread doesn't racily succeed after the winner drops.
        let mut winners: Vec<MinionLock> = Vec::new();

        for h in handles {
            match h.join().expect("worker thread panicked") {
                Ok(lock) => {
                    winners.push(lock);
                    acquired += 1;
                }
                Err(e) if e.downcast_ref::<SessionClaimError>().is_some() => {
                    already_running += 1;
                }
                Err(_) => other_err += 1,
            }
        }

        assert_eq!(
            acquired, 1,
            "exactly one worker must acquire the lock (got {acquired})"
        );
        assert_eq!(
            already_running,
            WORKERS - 1,
            "the remaining workers must surface AlreadyRunning (got {already_running})"
        );
        assert_eq!(
            other_err, 0,
            "no worker should fail with an unexpected error"
        );
    }
}
