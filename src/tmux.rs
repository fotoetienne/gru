//! Tmux window renaming support.
//!
//! When Gru runs inside tmux, long-lived commands automatically rename the
//! current tmux window so users can identify which Minion/issue is running.
//! On drop (or signal), `automatic-rename` is re-enabled so tmux reclaims
//! naming control — even if the process is killed without cleanup.

use std::ffi::OsStr;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::Mutex;

/// Global flag: when true, a signal handler should re-enable automatic-rename.
static TMUX_GUARD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Global window ID so the signal handler can target the correct window.
#[cfg(unix)]
static TMUX_WINDOW_ID: Mutex<Option<String>> = Mutex::new(None);

/// RAII guard that renames the current tmux window and re-enables
/// `automatic-rename` on drop.
///
/// If Gru is not running inside tmux (`$TMUX` unset), all operations are no-ops.
///
/// Instead of saving/restoring the original window name, we disable tmux's
/// `automatic-rename` on creation and re-enable it on drop. This way, even if
/// the guard never drops (e.g. `SIGKILL`), the next command the user runs will
/// trigger tmux to auto-rename the window based on the running process.
pub struct TmuxGuard {
    /// The `@id` of the window we renamed, used to target cleanup.
    /// `None` means we're not in tmux (no-op guard).
    window_id: Option<String>,
}

impl TmuxGuard {
    /// Create a new guard that renames the tmux window to `name`.
    ///
    /// Disables `automatic-rename` so the name sticks, and registers a SIGTERM
    /// handler for cleanup. If not inside tmux, returns a no-op guard.
    pub fn new(name: &str) -> Self {
        let window_id = match current_window_id() {
            Some(id) => {
                // Install signal hook first, before we modify tmux state,
                // so signals arriving mid-setup can still clean up.
                install_signal_hook();

                // Store window ID and mark guard active BEFORE modifying
                // tmux state, so a signal arriving mid-setup can clean up.
                #[cfg(unix)]
                if let Ok(mut global_id) = TMUX_WINDOW_ID.lock() {
                    *global_id = Some(id.clone());
                }
                TMUX_GUARD_ACTIVE.store(true, Ordering::SeqCst);

                set_automatic_rename(&id, false);
                rename_window(&id, name);
                Some(id)
            }
            None => None,
        };
        TmuxGuard { window_id }
    }

    /// Update the window name while keeping the same restore-on-drop behavior.
    ///
    /// No-op if not inside tmux (i.e., if the guard was created as a no-op).
    pub fn rename(&self, name: &str) {
        if let Some(ref id) = self.window_id {
            rename_window(id, name);
        }
    }
}

impl Drop for TmuxGuard {
    fn drop(&mut self) {
        if let Some(ref id) = self.window_id {
            set_automatic_rename(id, true);
            TMUX_GUARD_ACTIVE.store(false, Ordering::SeqCst);
            #[cfg(unix)]
            if let Ok(mut global_id) = TMUX_WINDOW_ID.lock() {
                *global_id = None;
            }
        }
    }
}

/// Install a SIGTERM handler (once) that re-enables `automatic-rename` before
/// the process exits.
///
/// SIGINT is intentionally NOT handled here — callers (lab.rs, fix/mod.rs, etc.)
/// already handle it via `tokio::signal::ctrl_c()`, which triggers graceful
/// shutdown and drops the `TmuxGuard`. Using `libc::signal` would replace
/// tokio's handlers and break that graceful shutdown path.
///
/// SIGTERM is not caught by callers, so we handle it here via tokio's signal
/// infrastructure (which cooperates with existing handlers).
fn install_signal_hook() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        #[cfg(unix)]
        {
            // Spawn a tokio task for SIGTERM listening. This cooperates with
            // tokio's signal infrastructure instead of replacing handlers.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async {
                    sigterm_listener().await;
                });
            }
        }
    });
}

#[cfg(unix)]
async fn sigterm_listener() {
    use tokio::signal::unix::{signal, SignalKind};

    let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
        return;
    };
    sigterm.recv().await;

    // Re-enable automatic-rename if a guard was active.
    if TMUX_GUARD_ACTIVE.swap(false, Ordering::SeqCst) {
        let window_id = TMUX_WINDOW_ID.lock().ok().and_then(|guard| guard.clone());
        if let Some(id) = window_id {
            set_automatic_rename(&id, true);
        } else {
            restore_automatic_rename_current_window();
        }
    }

    // Re-raise SIGTERM with default handler so the process exits
    // with the correct status for the parent.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::raise(libc::SIGTERM);
    }
}

/// Check if a `$TMUX` env var value indicates we are inside tmux.
fn is_tmux_env(val: Option<&OsStr>) -> bool {
    val.is_some_and(|v| !v.is_empty())
}

/// Get the current tmux window ID (e.g. `@0`). Returns `None` if not in tmux.
fn current_window_id() -> Option<String> {
    if !is_tmux_env(std::env::var_os("TMUX").as_deref()) {
        return None;
    }
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{window_id}"])
        .output()
        .ok()?;
    if output.status.success() {
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    } else {
        None
    }
}

/// Rename a specific tmux window. Silently ignores errors.
fn rename_window(window_id: &str, name: &str) {
    let _ = Command::new("tmux")
        .args(["rename-window", "-t", window_id, name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Set or unset `automatic-rename` on a specific window.
fn set_automatic_rename(window_id: &str, enable: bool) {
    let value = if enable { "on" } else { "off" };
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-w",
            "-t",
            window_id,
            "automatic-rename",
            value,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Re-enable automatic-rename on the current window (fallback when window ID
/// is unavailable, e.g. if the mutex is poisoned).
fn restore_automatic_rename_current_window() {
    let _ = Command::new("tmux")
        .args(["set-option", "-w", "automatic-rename", "on"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
        let guard = TmuxGuard { window_id: None };
        drop(guard);
    }

    #[test]
    fn test_drop_active_guard_does_not_panic() {
        // With a fake window_id, set_automatic_rename will silently fail
        // (no tmux server), which is fine — we just verify no panic.
        let guard = TmuxGuard {
            window_id: Some("@999".to_string()),
        };
        drop(guard);
    }

    #[test]
    fn test_guard_active_flag() {
        // Verify the global flag is not set for no-op guards
        TMUX_GUARD_ACTIVE.store(false, Ordering::SeqCst);
        let guard = TmuxGuard { window_id: None };
        assert!(!TMUX_GUARD_ACTIVE.load(Ordering::SeqCst));
        drop(guard);
        assert!(!TMUX_GUARD_ACTIVE.load(Ordering::SeqCst));
    }
}
