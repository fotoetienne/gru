use crate::url_utils::normalize_minion_id;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use std::path::PathBuf;
use tokio::process::Command;

/// Regex for extracting issue links from PR bodies
static ISSUE_LINK_REGEX: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?i)(?:fixes|closes|resolves)\s+#(\d+)")
        .expect("Failed to compile issue link regex")
});

/// Handles the path command to resolve a Minion's worktree path
/// Returns 0 on success, 1 on error
pub async fn handle_path(
    minion_id: Option<String>,
    issue: Option<u64>,
    pr: Option<u64>,
) -> Result<i32> {
    // Validate that at least one option is provided
    // Note: clap's conflicts_with_all ensures mutual exclusion (at most one)
    if minion_id.is_none() && issue.is_none() && pr.is_none() {
        anyhow::bail!("Must provide either a minion ID, --issue, or --pr");
    }

    // Resolve to a Minion ID
    let resolved_minion_id = if let Some(id) = minion_id {
        // Direct Minion ID resolution
        normalize_minion_id(&id)?
    } else if let Some(issue_num) = issue {
        // Resolve from issue number via GitHub API
        resolve_minion_from_issue(issue_num).await?
    } else {
        // Must be PR number (validated above that at least one is present)
        resolve_minion_from_pr(pr.unwrap()).await?
    };

    // Construct the worktree path
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let worktree_base = home.join(".gru").join("work");

    // Find the Minion worktree (this validates existence)
    let worktree_path = find_minion_worktree(&worktree_base, &resolved_minion_id)?;

    // Output just the path to stdout
    println!("{}", worktree_path.display());
    Ok(0)
}

/// Finds a Minion's worktree by searching the work directory
pub fn find_minion_worktree(work_dir: &std::path::Path, minion_id: &str) -> Result<PathBuf> {
    use std::fs;

    // The structure is ~/.gru/work/owner/repo/M<id>
    // We need to search for the Minion ID across all owner/repo subdirectories

    if !work_dir.exists() {
        anyhow::bail!("Work directory does not exist. No Minions have been created yet.");
    }

    // Walk through owner directories
    for owner_entry in fs::read_dir(work_dir)? {
        let owner_entry = owner_entry?;
        let owner_path = owner_entry.path();

        if !owner_path.is_dir() {
            continue;
        }

        // Walk through repo directories
        for repo_entry in fs::read_dir(&owner_path)? {
            let repo_entry = repo_entry?;
            let repo_path = repo_entry.path();

            if !repo_path.is_dir() {
                continue;
            }

            // Check if this repo has the Minion worktree
            let minion_path = repo_path.join(minion_id);

            // Defensive check: verify the path stays within the work directory
            if !minion_path.starts_with(work_dir) {
                anyhow::bail!(
                    "Security error: Minion path escapes work directory. This should never happen."
                );
            }

            if minion_path.exists() && minion_path.is_dir() {
                return Ok(minion_path);
            }
        }
    }

    anyhow::bail!(
        "No worktree found for Minion {}. It may not have been created yet.",
        minion_id
    );
}

/// Resolves a Minion ID from a GitHub issue number
async fn resolve_minion_from_issue(issue_num: u64) -> Result<String> {
    // Use gh CLI to get issue labels
    let output = Command::new("gh")
        .args(["issue", "view", &issue_num.to_string(), "--json", "labels"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch issue #{}: {}", issue_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    // Look for in-progress:M<id> label
    let labels = json["labels"]
        .as_array()
        .context("Issue labels field is not an array")?;

    for label in labels {
        let label_name = label["name"]
            .as_str()
            .context("Label name is not a string")?;

        if let Some(minion_id) = label_name.strip_prefix("in-progress:") {
            if minion_id.starts_with('M') {
                return Ok(minion_id.to_string());
            }
        }
    }

    anyhow::bail!(
        "No active Minion found for issue #{}. Issue may not be in progress.",
        issue_num
    );
}

/// Resolves a Minion ID from a GitHub PR number
async fn resolve_minion_from_pr(pr_num: u64) -> Result<String> {
    // Use gh CLI to get linked issue from PR body
    let output = Command::new("gh")
        .args(["pr", "view", &pr_num.to_string(), "--json", "body"])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR #{}: {}", pr_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    let body = json["body"]
        .as_str()
        .context("PR body field is not a string")?;

    // Look for "Fixes #<issue>" or "Closes #<issue>" in the PR body
    if let Some(captures) = ISSUE_LINK_REGEX.captures(body) {
        let issue_num = captures[1]
            .parse::<u64>()
            .context("Failed to parse issue number from PR body")?;

        // Now resolve the Minion from that issue
        return resolve_minion_from_issue(issue_num).await;
    }

    anyhow::bail!(
        "No linked issue found for PR #{}. PR must contain 'Fixes #<issue>' in its description.",
        pr_num
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_minion_worktree_nonexistent_work_dir() {
        use std::path::PathBuf;
        let nonexistent_dir = PathBuf::from("/tmp/gru_test_nonexistent");
        let result = find_minion_worktree(&nonexistent_dir, "M42");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Work directory does not exist"));
    }
}
