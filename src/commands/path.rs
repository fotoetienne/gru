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
pub async fn handle_path(id: String) -> Result<i32> {
    let minion = minion_resolver::resolve_minion(&id).await?;

    // Output the checkout path (where users cd to for git work)
    println!("{}", minion.checkout_path().display());
    Ok(0)
}
