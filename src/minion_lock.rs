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

/// Rejects minion IDs containing characters that could escape the lock directory.
///
/// Any `/`, `\`, or `..` in an ID would let a caller (corrupt registry entry,
/// hand-passed `--worker` flag) write a lockfile outside `<state>/minions/`.
/// Mirrors the same defence applied to minion IDs in `workspace::archive_dir`.
fn validate_minion_id(minion_id: &str) -> io::Result<()> {
    if minion_id.is_empty()
        || minion_id.contains('/')
        || minion_id.contains('\\')
        || minion_id.contains("..")
        || minion_id.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Minion ID '{minion_id}' contains invalid characters"),
        ));
    }
    Ok(())
}

/// Computes the lockfile path `<state>/minions/<minion_id>.lock`.
///
/// Callers must have validated `minion_id` via [`validate_minion_id`] first —
/// this function performs a final containment check but does not re-validate
/// characters.
fn lock_path(state_dir: &Path, minion_id: &str) -> io::Result<PathBuf> {
    let minions_dir = state_dir.join(MINION_LOCKS_SUBDIR);
    let path = minions_dir.join(format!("{minion_id}.lock"));

    // Belt-and-braces containment check: refuse any joined path whose components
    // include `..`, since a lexical `starts_with` alone would accept
    // `minions/../escape.lock`. Done on the constructed path (no filesystem
    // touch) so traversal is caught before we open anything.
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Minion lock path would escape {}", minions_dir.display()),
        ));
    }
    if !path.starts_with(&minions_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Minion lock path would escape {}", minions_dir.display()),
        ));
    }
    Ok(path)
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
    /// Returns [`SessionClaimError::LockContention`] if another process
    /// already holds the lock. The separate variant (distinct from
    /// [`SessionClaimError::AlreadyRunning`]) avoids fabricating a
    /// registry mode we don't know, and keeps the `mode == Autonomous`
    /// branching in `handle_attach` bound strictly to the registry-level
    /// claim. Other IO failures propagate as [`anyhow::Error`].
    pub(crate) fn try_acquire(minion_id: &str) -> Result<Self> {
        let ws = Workspace::global().context("Failed to initialize workspace for minion lock")?;
        Self::try_acquire_in_dir(ws.state(), minion_id)
    }

    /// Test-friendly variant that acquires the lock under an explicit state
    /// directory rather than the global workspace. Used by unit tests to
    /// avoid clashing with the real `~/.gru/state/minions/`.
    pub(crate) fn try_acquire_in_dir(state_dir: &Path, minion_id: &str) -> Result<Self> {
        validate_minion_id(minion_id)
            .with_context(|| format!("Invalid minion ID for lock: {minion_id:?}"))?;
        let path = lock_path(state_dir, minion_id)
            .with_context(|| format!("Failed to resolve lock path for {minion_id:?}"))?;

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
            Err(e) if is_lock_contention(&e) => Err(SessionClaimError::LockContention {
                minion_id: minion_id.to_string(),
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
    fn rejects_minion_ids_that_could_escape_state_dir() {
        let tmp = tempdir().unwrap();
        let minions_dir = tmp.path().join(MINION_LOCKS_SUBDIR);

        // Each of these would either traverse out of minions/ or drop a lockfile
        // at an attacker-chosen path. All must be rejected before open(2).
        let bad_ids = [
            "",                     // empty
            "..",                   // parent dir
            "../escape",            // traversal
            "foo/../../etc/passwd", // embedded traversal
            "a/b",                  // forward slash
            r"a\b",                 // backslash (Windows separator)
            "null\0byte",           // NUL byte
        ];

        for id in bad_ids {
            let err = MinionLock::try_acquire_in_dir(tmp.path(), id).unwrap_err_or_else(id);
            let msg = format!("{:#}", err);
            assert!(
                msg.contains("invalid characters") || msg.contains("escape"),
                "expected rejection for {id:?}, got: {msg}"
            );
        }

        // The minions/ directory should never have been written to since every
        // input was rejected before create_dir_all ran.
        assert!(
            !minions_dir.exists(),
            "no lockfiles should have been created for invalid IDs"
        );
    }

    /// Test helper: like `Result::unwrap_err`, but annotates the panic with the
    /// input that was expected to fail.
    trait UnwrapErrOrElse {
        type Err;
        fn unwrap_err_or_else(self, label: &str) -> Self::Err;
    }
    impl<T: std::fmt::Debug, E> UnwrapErrOrElse for std::result::Result<T, E> {
        type Err = E;
        fn unwrap_err_or_else(self, label: &str) -> E {
            match self {
                Ok(v) => panic!("expected error for input {label:?}, got Ok({v:?})"),
                Err(e) => e,
            }
        }
    }

    #[test]
    fn lock_path_rejects_traversal_even_if_validation_skipped() {
        let tmp = tempdir().unwrap();
        // Direct call into lock_path with a pathological ID — simulates a future
        // refactor that accidentally skips `validate_minion_id`. The containment
        // check must still catch the traversal.
        let err = lock_path(tmp.path(), "../escape").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn second_acquire_fails_with_lock_contention() {
        let tmp = tempdir().unwrap();
        let _first = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap();

        let err = MinionLock::try_acquire_in_dir(tmp.path(), "M001").unwrap_err();
        let claim = err
            .downcast_ref::<SessionClaimError>()
            .expect("second acquire must surface SessionClaimError::LockContention");
        match claim {
            SessionClaimError::LockContention { minion_id } => assert_eq!(minion_id, "M001"),
            other => panic!("expected LockContention, got {other:?}"),
        }
        // The Display message must not fabricate a registry mode (review feedback on #872).
        let msg = format!("{:#}", err);
        assert!(!msg.contains("mode:"), "unexpected mode in message: {msg}");
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

        let path = lock_path(tmp.path(), "M077").unwrap();
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
    /// `SessionClaimError::LockContention`.
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
        let mut contended = 0;
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
                Err(e)
                    if matches!(
                        e.downcast_ref::<SessionClaimError>(),
                        Some(SessionClaimError::LockContention { .. })
                    ) =>
                {
                    contended += 1;
                }
                Err(_) => other_err += 1,
            }
        }

        assert_eq!(
            acquired, 1,
            "exactly one worker must acquire the lock (got {acquired})"
        );
        assert_eq!(
            contended,
            WORKERS - 1,
            "the remaining workers must surface LockContention (got {contended})"
        );
        assert_eq!(
            other_err, 0,
            "no worker should fail with an unexpected error"
        );
    }
}
