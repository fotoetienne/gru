use crate::log_viewer;
use crate::minion_registry::with_registry;
use crate::minion_resolver;
use anyhow::{Context, Result};

/// Default number of events to show before following (in auto-follow mode).
const DEFAULT_LAST_N: usize = 20;

/// Handles the `gru logs` command: replay and/or follow minion event logs.
///
/// Auto-detects follow mode based on minion state (running → follow, stopped → replay only).
/// Use `-f` to force follow, `--no-follow` to force replay-only.
/// Supports `--raw` for piping raw JSONL and `-n` for showing last N events.
pub async fn handle_logs(
    id: String,
    force_follow: bool,
    no_follow: bool,
    raw: bool,
    last_n: Option<usize>,
    quiet: bool,
) -> Result<i32> {
    let minion = minion_resolver::resolve_minion(&id).await?;

    let events_path = minion.worktree_path.join("events.jsonl");

    let issue_str = minion
        .issue_number
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());

    // Determine follow mode:
    // - --no-follow: always replay only
    // - -f: always follow
    // - otherwise: auto-detect based on minion state
    let follow = if no_follow {
        false
    } else if force_follow {
        true
    } else {
        is_minion_running(&minion.minion_id).await
    };

    // Only check for missing events file when not following.
    // In follow mode, the log_viewer's wait-for-file logic handles files
    // that haven't been created yet (e.g., minion just started).
    if !follow && !events_path.exists() {
        eprintln!(
            "No events found for Minion {} ({})",
            minion.minion_id,
            events_path.display()
        );
        return Ok(1);
    }

    // In follow mode, default to showing last 20 events before tailing.
    // In no-follow mode, default to showing all events.
    let effective_n = if follow {
        Some(last_n.unwrap_or(DEFAULT_LAST_N))
    } else {
        last_n
    };

    if raw {
        if follow {
            log_viewer::tail_events_raw(events_path, &minion.minion_id, effective_n)
                .await
                .context("Failed to tail events")?;
        } else {
            match effective_n {
                Some(n) => {
                    log_viewer::replay_last_n_events_raw(&events_path, n)?;
                }
                None => {
                    log_viewer::replay_events_raw(&events_path)?;
                }
            }
        }
    } else if follow {
        if !quiet {
            eprintln!(
                "Streaming logs for Minion {} (issue #{})...",
                minion.minion_id, issue_str
            );
            eprintln!("Press Ctrl+C to detach\n");
        }

        log_viewer::tail_events_last_n(
            events_path,
            &minion.minion_id,
            &issue_str,
            quiet,
            effective_n,
        )
        .await
        .context("Failed to tail events")?;
    } else {
        let config = crate::progress::ProgressConfig {
            minion_id: minion.minion_id.clone(),
            issue: issue_str.clone(),
            quiet,
        };
        let progress = crate::progress::ProgressDisplay::new(config);

        match effective_n {
            Some(n) => {
                if !quiet {
                    eprintln!(
                        "Replaying last {} events for Minion {} (issue #{})...\n",
                        n, minion.minion_id, issue_str
                    );
                }
                log_viewer::replay_last_n_events(&events_path, n, &progress)?;
            }
            None => {
                log_viewer::replay_events(&events_path, &progress)?;
            }
        }
        progress.finish_with_message(&format!("End of logs for Minion {}", minion.minion_id));
    }

    Ok(0)
}

/// Checks if a minion's worker process is currently running.
async fn is_minion_running(minion_id: &str) -> bool {
    let mid = minion_id.to_string();
    with_registry(move |reg| Ok(reg.get(&mid).map(|info| info.is_running()).unwrap_or(false)))
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_logs_with_invalid_id() {
        let result = handle_logs(
            "nonexistent-minion-xyz".to_string(),
            false,
            false,
            false,
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[tokio::test]
    async fn test_handle_logs_no_follow_with_invalid_id() {
        let result = handle_logs(
            "nonexistent-minion-xyz".to_string(),
            false,
            true,
            false,
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }

    #[tokio::test]
    async fn test_handle_logs_raw_with_invalid_id() {
        let result = handle_logs(
            "nonexistent-minion-xyz".to_string(),
            false,
            false,
            true,
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }
}
