use crate::minion_registry::{is_process_alive, with_registry, MinionInfo, MinionMode};
use anyhow::Result;
use chrono::Utc;

/// Typed errors from session-claim operations (shared by attach and resume).
#[derive(Debug)]
pub enum SessionClaimError {
    /// The minion has a live process — user must stop it first.
    AlreadyRunning { minion_id: String, mode: MinionMode },
    /// Registry shows a non-Stopped mode but no PID is recorded.
    InconsistentState { minion_id: String, mode: MinionMode },
}

impl std::fmt::Display for SessionClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionClaimError::AlreadyRunning { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is already running (mode: {}). Stop it first with: gru stop {}",
                    minion_id, mode, minion_id
                )
            }
            SessionClaimError::InconsistentState { minion_id, mode } => {
                write!(
                    f,
                    "Minion {} is currently in {} mode without an associated process. \
                     Please wait or run 'gru status' / 'gru stop {}' to recover.",
                    minion_id, mode, minion_id
                )
            }
        }
    }
}

impl std::error::Error for SessionClaimError {}

/// Atomically checks if a minion is available and claims it with the given mode.
///
/// This combines the mode check and the mode update in a single `with_registry`
/// call, which holds an exclusive file lock for the duration. This prevents
/// TOCTOU races between concurrent attach/resume calls.
///
/// Returns a clone of the [`MinionInfo`] (as it was before claiming) if the
/// minion is found in the registry. Callers can extract whatever fields they
/// need from this snapshot.
///
/// # Errors
///
/// - [`SessionClaimError::AlreadyRunning`] if the minion has a live process.
/// - [`SessionClaimError::InconsistentState`] if mode is non-Stopped but no PID
///   is recorded.
///
/// # Graceful degradation
///
/// If `graceful` is true, transient registry failures (lock contention, IO
/// errors) are swallowed and the function returns `Ok(None)`. When false, all
/// registry errors propagate.
pub async fn check_and_claim_session(
    minion_id: &str,
    target_mode: MinionMode,
    graceful: bool,
) -> Result<Option<MinionInfo>> {
    let id = minion_id.to_string();
    let result = with_registry(move |reg| {
        let info = match reg.get(&id) {
            Some(info) => info.clone(),
            None => return Ok(None),
        };

        if info.mode != MinionMode::Stopped {
            match info.pid {
                Some(pid_val) => {
                    // On Unix, verify the process is actually alive.
                    // On non-Unix, is_process_alive always returns false, so be
                    // conservative and treat any recorded PID as alive.
                    let is_running = cfg!(not(unix)) || is_process_alive(pid_val);
                    if is_running {
                        return Err(SessionClaimError::AlreadyRunning {
                            minion_id: id,
                            mode: info.mode.clone(),
                        }
                        .into());
                    }
                    // Stale entry: process is dead but registry still thinks
                    // it's running. Reset to Stopped before claiming.
                    reg.update(&id, |i| {
                        i.mode = MinionMode::Stopped;
                        i.pid = None;
                        i.last_activity = Utc::now();
                    })?;
                }
                None => {
                    return Err(SessionClaimError::InconsistentState {
                        minion_id: id,
                        mode: info.mode.clone(),
                    }
                    .into());
                }
            }
        }

        // Claim the session with the requested mode
        reg.update(&id, |i| {
            i.mode = target_mode.clone();
            i.last_activity = Utc::now();
        })?;

        Ok(Some(info))
    })
    .await;

    match result {
        Ok(info) => Ok(info),
        Err(e) => {
            if e.downcast_ref::<SessionClaimError>().is_some() {
                Err(e)
            } else if graceful {
                // Registry unavailable — proceed without it
                log::debug!("Could not check registry: {}", e);
                Ok(None)
            } else {
                Err(e.context("Failed to access minion registry"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_claim_error_display_already_running() {
        let err = SessionClaimError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("already running"));
        assert!(msg.contains("autonomous"));
        assert!(msg.contains("gru stop M001"));
    }

    #[test]
    fn test_session_claim_error_display_inconsistent_state() {
        let err = SessionClaimError::InconsistentState {
            minion_id: "M002".to_string(),
            mode: MinionMode::Interactive,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("interactive mode"));
        assert!(msg.contains("without an associated process"));
        assert!(msg.contains("gru stop M002"));
    }

    #[test]
    fn test_session_claim_error_is_downcastable() {
        let err: anyhow::Error = SessionClaimError::AlreadyRunning {
            minion_id: "M001".to_string(),
            mode: MinionMode::Autonomous,
        }
        .into();
        assert!(err.downcast_ref::<SessionClaimError>().is_some());
    }
}
