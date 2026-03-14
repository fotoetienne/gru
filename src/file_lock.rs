use std::fs::File;
use std::io;
use std::thread;
use std::time::Duration;

use fs2::FileExt;

/// Maximum number of attempts to acquire an exclusive file lock before giving up.
const MAX_LOCK_ATTEMPTS: u32 = 20;

/// Initial backoff duration between lock attempts.
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);

/// Maximum backoff duration between lock attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(3);

/// Attempts to acquire an exclusive lock on the given file with retry and backoff.
///
/// Uses `try_lock_exclusive` in a loop instead of the blocking `lock_exclusive`,
/// so a hung process holding the lock cannot deadlock all concurrent callers.
///
/// Retries up to [`MAX_LOCK_ATTEMPTS`] times with exponential backoff starting
/// at [`INITIAL_BACKOFF`] and capped at [`MAX_BACKOFF`] (~42s total).
pub fn lock_with_timeout(file: &File) -> io::Result<()> {
    lock_with_attempts(file, MAX_LOCK_ATTEMPTS)
}

fn lock_with_attempts(file: &File, max_attempts: u32) -> io::Result<()> {
    if max_attempts == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_attempts must be at least 1",
        ));
    }

    let mut backoff = INITIAL_BACKOFF;

    for attempt in 0..max_attempts {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(e) if is_lock_contention(&e) => {
                if attempt + 1 == max_attempts {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "Failed to acquire file lock after {} attempts (~{:.1}s): {}",
                            max_attempts,
                            total_backoff_secs(max_attempts),
                            e,
                        ),
                    ));
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("lock loop exhausted without returning")
}

/// Returns true if the error represents lock contention (file already locked).
fn is_lock_contention(e: &io::Error) -> bool {
    // On Unix, try_lock returns EWOULDBLOCK (same as EAGAIN on most platforms).
    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
        return true;
    }
    #[cfg(unix)]
    {
        if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return true;
        }
    }
    // On Windows, fs2 maps ERROR_LOCK_VIOLATION to PermissionDenied.
    #[cfg(windows)]
    if e.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    false
}

/// Computes the approximate total backoff duration for a given number of attempts.
fn total_backoff_secs(attempts: u32) -> f64 {
    let mut total = Duration::ZERO;
    let mut backoff = INITIAL_BACKOFF;
    for _ in 0..attempts.saturating_sub(1) {
        total += backoff;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    total.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn test_lock_with_timeout_succeeds() {
        let temp_dir = tempfile::tempdir().unwrap();
        let lock_path = temp_dir.path().join("test.lock");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();

        // Should succeed immediately when no contention
        lock_with_timeout(&file).unwrap();

        // Unlock so tempdir cleanup works
        file.unlock().unwrap();
    }

    #[test]
    fn test_lock_with_timeout_fails_when_held() {
        let temp_dir = tempfile::tempdir().unwrap();
        let lock_path = temp_dir.path().join("test.lock");

        // Holder takes the lock
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        holder.lock_exclusive().unwrap();

        // Second file handle tries to acquire with only 2 attempts for speed
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();

        let result = lock_with_attempts(&contender, 2);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        holder.unlock().unwrap();
    }

    #[test]
    fn test_is_lock_contention() {
        #[cfg(unix)]
        {
            let err = io::Error::from_raw_os_error(libc::EWOULDBLOCK);
            assert!(is_lock_contention(&err));
        }

        let err = io::Error::new(io::ErrorKind::WouldBlock, "would block");
        assert!(is_lock_contention(&err));

        let err = io::Error::new(io::ErrorKind::NotFound, "not found");
        assert!(!is_lock_contention(&err));

        // PermissionDenied should NOT be treated as contention on Unix
        #[cfg(unix)]
        {
            let err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
            assert!(!is_lock_contention(&err));
        }
    }

    #[test]
    fn test_lock_with_zero_attempts_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let lock_path = temp_dir.path().join("test.lock");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();

        let result = lock_with_attempts(&file, 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_total_backoff_secs() {
        // With 1 attempt there are 0 waits
        assert_eq!(total_backoff_secs(1), 0.0);
        // With 2 attempts there is 1 wait of 50ms
        assert!((total_backoff_secs(2) - 0.05).abs() < 0.001);
    }
}
