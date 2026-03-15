//! Tmux window renaming support.
//!
//! When Gru runs inside tmux, long-lived commands automatically rename the
//! current tmux window so users can identify which Minion/issue is running.
//! The original window name is restored when the guard is dropped.

use std::ffi::OsStr;
use std::process::Command;

/// RAII guard that renames the current tmux window and restores it on drop.
///
/// If Gru is not running inside tmux (`$TMUX` unset), all operations are no-ops.
pub struct TmuxGuard {
    original_name: Option<String>,
}

impl TmuxGuard {
    /// Create a new guard that renames the tmux window to `name`.
    ///
    /// Returns a guard that will restore the original name on drop.
    /// If not inside tmux, returns a no-op guard.
    pub fn new(name: &str) -> Self {
        let original = match save_current_name() {
            Some(orig) => {
                rename_window(name);
                Some(orig)
            }
            None => None,
        };
        TmuxGuard {
            original_name: original,
        }
    }
}

impl Drop for TmuxGuard {
    fn drop(&mut self) {
        if let Some(ref name) = self.original_name {
            rename_window(name);
        }
    }
}

/// Check if a `$TMUX` env var value indicates we are inside tmux.
fn is_tmux_env(val: Option<&OsStr>) -> bool {
    val.is_some_and(|v| !v.is_empty())
}

/// Save the current tmux window name. Returns `None` if not in tmux.
fn save_current_name() -> Option<String> {
    if !is_tmux_env(std::env::var_os("TMUX").as_deref()) {
        return None;
    }
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{window_name}"])
        .output()
        .ok()?;
    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    } else {
        None
    }
}

/// Rename the current tmux window. Silently ignores errors.
///
/// Uses `tmux rename-window` without a `-t` target, which renames the
/// window that contains the current pane. This relies on the `$TMUX`
/// socket path set by tmux on session creation.
fn rename_window(name: &str) {
    let _ = Command::new("tmux").args(["rename-window", name]).output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_tmux_env() {
        assert!(!is_tmux_env(None));
        assert!(!is_tmux_env(Some(OsStr::new(""))));
        assert!(is_tmux_env(Some(OsStr::new(
            "/tmp/tmux-1000/default,12345,0"
        ))));
    }

    #[test]
    fn test_drop_noop_guard_does_not_panic() {
        let guard = TmuxGuard {
            original_name: None,
        };
        drop(guard);
    }

    #[test]
    fn test_drop_active_guard_does_not_panic() {
        let guard = TmuxGuard {
            original_name: Some("original".to_string()),
        };
        drop(guard);
    }
}
