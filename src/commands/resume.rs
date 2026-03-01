use crate::commands::attach;
use anyhow::Result;

/// Handles the resume command by delegating to attach.
///
/// `gru resume` and `gru attach` are identical: both resolve a Minion and
/// exec `claude -r` in its worktree. This thin wrapper preserves the
/// `resume` subcommand for backwards compatibility while sharing all logic
/// with `attach`.
pub async fn handle_resume(id: String, yolo: bool) -> Result<i32> {
    attach::handle_attach(id, yolo).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_resume_with_invalid_id() {
        let result = handle_resume("nonexistent-minion-xyz".to_string(), false).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
        assert!(err_msg.contains("gru status"));
    }

    #[tokio::test]
    async fn test_handle_resume_yolo_with_invalid_id() {
        let result = handle_resume("nonexistent-minion-xyz".to_string(), true).await;
        assert!(result.is_err());

        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(err_msg.contains("Could not resolve ID"));
    }
}
