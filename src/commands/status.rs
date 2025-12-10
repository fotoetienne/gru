use crate::minion_resolver;
use anyhow::Result;

/// Handles the status command by displaying active Minions
/// Optionally filters by a specific ID (minion ID, issue number, or PR number)
pub async fn handle_status(id: Option<String>) -> Result<i32> {
    // Use spawn_blocking to avoid blocking the async runtime
    let minions = tokio::task::spawn_blocking(minion_resolver::scan_all_minions).await??;

    // Filter by ID if provided (stays purely local for performance)
    let filtered_minions = if let Some(filter_id) = id {
        // Try as issue/PR number first (most common case)
        let filtered: Vec<_> = if let Ok(num) = filter_id.parse::<u64>() {
            minions
                .iter()
                .filter(|m| m.issue_number == Some(num))
                .cloned()
                .collect()
        } else {
            // Try as minion ID (exact or with M prefix)
            minions
                .into_iter()
                .filter(|m| m.minion_id == filter_id || m.minion_id == format!("M{}", filter_id))
                .collect()
        };

        if filtered.is_empty() {
            eprintln!("No Minions found matching '{}'", filter_id);
            return Ok(1);
        }
        filtered
    } else {
        minions
    };

    if filtered_minions.is_empty() {
        println!("No active Minions");
        return Ok(0);
    }

    // Print table header
    println!(
        "{:<8} {:<8} {:<20} {:<30} {:<10} {:<8}",
        "MINION", "ISSUE", "REPO", "BRANCH", "STATUS", "UPTIME"
    );

    // Print each minion
    for minion in &filtered_minions {
        let issue_display = minion
            .issue_number
            .map(|n| format!("#{}", n))
            .unwrap_or_else(|| "?".to_string());

        println!(
            "{:<8} {:<8} {:<20} {:<30} {:<10} {:<8}",
            minion.minion_id,
            issue_display,
            minion.repo_name,
            minion.branch,
            minion.status,
            minion.uptime
        );
    }

    println!();
    println!("{} Minion(s) found", filtered_minions.len());

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_status_no_filter() {
        // This test verifies that handle_status succeeds without filtering
        let result = handle_status(None).await;
        assert!(result.is_ok());
    }
}
