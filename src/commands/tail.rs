use crate::commands::logs;
use anyhow::Result;

/// Handles the `gru tail` command (hidden alias for `gru logs`).
///
/// Delegates to `handle_logs` with `--no-follow` mapped from the tail flag.
pub async fn handle_tail(
    id: String,
    no_follow: bool,
    raw: bool,
    last_n: Option<usize>,
    quiet: bool,
) -> Result<i32> {
    logs::handle_logs(id, false, no_follow, raw, last_n, quiet).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_tail_with_invalid_id() {
        let result = handle_tail(
            "nonexistent-minion-xyz".to_string(),
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
}
