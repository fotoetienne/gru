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
        log::warn!("Warning: --issue flag is deprecated and will be ignored.");
        log::warn!("Using positional argument '{}' instead.", id);
        log::warn!("In the future, use: gru path {}\n", id);
    }
    if pr.is_some() {
        log::warn!("Warning: --pr flag is deprecated and will be ignored.");
        log::warn!("Using positional argument '{}' instead.", id);
        log::warn!("In the future, use: gru path {}\n", id);
    }

    // Always use the positional id argument (non-deprecated)
    // Deprecated flags are ignored to ensure the non-deprecated argument wins
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Output the checkout path (where users cd to for git work)
    println!("{}", minion.checkout_path().display());
    Ok(0)
}
