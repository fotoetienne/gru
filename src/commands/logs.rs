use crate::log_viewer;
use crate::minion_resolver;
use anyhow::{Context, Result};

/// Handles the `gru logs` command: replay and follow minion event logs.
///
/// Resolves the minion by ID or issue number, then tails its events.jsonl
/// with the same progress formatting used by `gru do`.
pub async fn handle_logs(id: String, follow: bool, quiet: bool) -> Result<i32> {
    let minion = minion_resolver::resolve_minion(&id).await?;

    let events_path = minion.worktree_path.join("events.jsonl");

    if !events_path.exists() {
        eprintln!(
            "No events found for Minion {} ({})",
            minion.minion_id,
            events_path.display()
        );
        return Ok(1);
    }

    let issue_str = minion
        .issue_number
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());

    if follow {
        // Replay + follow live events (like docker logs -f)
        println!(
            "Streaming logs for Minion {} (issue #{})...",
            minion.minion_id, issue_str
        );
        println!("Press Ctrl+C to detach\n");

        log_viewer::tail_events(events_path, &minion.minion_id, &issue_str, quiet)
            .await
            .context("Failed to tail events")?;
    } else {
        // Replay only (reuse log_viewer for consistent formatting)
        let config = crate::progress::ProgressConfig {
            minion_id: minion.minion_id.clone(),
            issue: issue_str,
            quiet,
        };
        let progress = crate::progress::ProgressDisplay::new(config);

        log_viewer::replay_events(&events_path, &progress)?;
        progress.finish_with_message(&format!("End of logs for Minion {}", minion.minion_id));
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_logs_with_invalid_id() {
        let result = handle_logs("nonexistent-minion-xyz".to_string(), false, false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }
}
