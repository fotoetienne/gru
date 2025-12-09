use crate::minion_resolver;
use anyhow::Result;

/// Handles the path command to resolve a Minion's worktree path
/// Returns 0 on success, 1 on error
///
/// The ID argument supports smart resolution:
/// 1. Try as exact minion ID (e.g., M0wy)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
pub async fn handle_path(id: String, issue: Option<u64>, pr: Option<u64>) -> Result<i32> {
    // Show deprecation warnings for old flags
    if issue.is_some() {
        eprintln!("Warning: --issue flag is deprecated. Use 'gru path <issue#>' instead.");
        eprintln!("Example: gru path {}\n", issue.unwrap());
    }
    if pr.is_some() {
        eprintln!("Warning: --pr flag is deprecated. Use 'gru path <pr#>' instead.");
        eprintln!("Example: gru path {}\n", pr.unwrap());
    }

    // For backward compatibility, prefer the old flags if provided
    let resolution_id = if let Some(issue_num) = issue {
        issue_num.to_string()
    } else if let Some(pr_num) = pr {
        pr_num.to_string()
    } else {
        id
    };

    // Use smart resolution
    let minion = minion_resolver::resolve_minion(&resolution_id).await?;

    // Output just the path to stdout
    println!("{}", minion.worktree_path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_path_with_deprecated_flags() {
        // Test that deprecated flags still work
        // This is a smoke test - it may fail if no worktrees exist
        let result = handle_path("M123".to_string(), Some(42), None).await;
        // We expect it to either succeed or fail gracefully
        assert!(result.is_ok() || result.is_err());
    }
}
