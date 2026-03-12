use anyhow::{Context, Result};
use std::process::ExitStatus;
use tokio::time::Duration;

/// Grace period for the child process to exit after receiving SIGTERM.
const CTRL_C_GRACE_SECS: u64 = 5;

/// Send a termination signal to the child process.
/// On Unix, sends SIGTERM for graceful shutdown.
/// On other platforms, attempts kill (there is no graceful signal equivalent).
pub fn signal_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill with SIGTERM is safe — it requests graceful termination.
            // The PID was just obtained from the child handle.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        // No graceful signal on non-Unix; start_kill is best-effort
        let _ = child.start_kill();
    }
}

/// Wait for a child process, handling Ctrl-C gracefully.
///
/// On Ctrl-C: sends SIGTERM, waits up to `CTRL_C_GRACE_SECS`, then force-kills.
/// Used by both `gru chat` and `gru attach` to ensure clean shutdown.
pub async fn wait_with_ctrlc_handling(child: &mut tokio::process::Child) -> Result<ExitStatus> {
    tokio::select! {
        result = child.wait() => result.context("Failed to wait for child process"),
        _ = tokio::signal::ctrl_c() => {
            signal_child(child);
            match tokio::time::timeout(
                Duration::from_secs(CTRL_C_GRACE_SECS),
                child.wait(),
            ).await {
                Ok(result) => result.context("Failed to wait for child after interrupt"),
                Err(_) => {
                    log::warn!("Child process did not exit within {}s, force-killing", CTRL_C_GRACE_SECS);
                    let _ = child.kill().await;
                    child.wait().await.context("Failed to reap child after force-kill")
                }
            }
        }
    }
}
