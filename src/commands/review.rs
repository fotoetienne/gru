use crate::git;
use crate::minion_resolver;
use crate::pr_state::PrState;
use crate::url_utils::parse_pr_info;
use crate::workspace;
use anyhow::{Context, Result};
use std::env;
use tokio::process::Command;

/// Handles the review command by setting up workspace and delegating to the Claude CLI
/// Returns the exit code from the claude process
pub async fn handle_review(pr_arg: Option<String>) -> Result<i32> {
    // Resolve PR information from various input formats
    let (owner, repo, pr_num, branch) = match pr_arg {
        None => resolve_pr_from_current_worktree().await?,
        Some(arg) => resolve_pr_from_arg(&arg).await?,
    };

    println!(
        "🔍 Setting up workspace for {}/{}#{} (branch: {})",
        owner, repo, pr_num, branch
    );

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create bare repository path
    let bare_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(&owner, &repo, bare_path);

    // Ensure bare repository is cloned/updated
    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .with_context(|| format!("Failed to clone or update repository for PR {}", pr_num))?;

    // Check if a worktree already exists for this branch
    let worktree_path = if let Some(existing_path) = git_repo
        .find_worktree_for_branch(&branch)
        .context("Failed to check for existing worktree")?
    {
        println!(
            "♻️  Reusing existing worktree at: {}",
            existing_path.display()
        );
        existing_path
    } else {
        // No existing worktree, fetch the branch and create one
        println!("🔄 Fetching PR branch: {}", branch);
        git_repo
            .fetch_branch(&branch)
            .with_context(|| format!("Failed to fetch PR branch '{}'", branch))?;

        let repo_name = format!("{}/{}", owner, repo);
        let new_worktree_path = workspace
            .work_dir(&repo_name, &branch)
            .context("Failed to compute worktree path")?;

        println!("🌿 Creating worktree for branch: {}", branch);
        git_repo
            .checkout_worktree(&branch, &new_worktree_path)
            .with_context(|| format!("Failed to checkout worktree for PR {}", pr_num))?;

        new_worktree_path
    };

    println!("🤖 Launching agent for PR review...\n");

    // Execute the claude CLI with the /pr_review command in the worktree
    let status = Command::new("claude")
        .arg(format!("/pr_review {}", pr_num))
        .current_dir(&worktree_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context(
            "claude command not found. Install from: https://github.com/anthropics/claude-code",
        )?;

    // Return the exit code from the claude process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(128))
}

/// Resolves PR information from the current worktree directory
/// Reads the .gru_pr_state.json file to get the PR number
async fn resolve_pr_from_current_worktree() -> Result<(String, String, String, String)> {
    // Detect current directory as git repository
    let current_dir = env::current_dir().context("Failed to get current directory")?;

    // Check if we're in a git repository
    git::detect_git_repo().context(
        "Not in a git repository. Run from a Minion worktree or provide a PR number/URL/Minion ID.",
    )?;

    // Try to load PR state from current directory
    let pr_state = PrState::load(&current_dir)
        .context("Failed to check for PR state file")?
        .context(
            "No PR state found in current directory. This doesn't appear to be a Minion worktree.\n\
             Try: gru review <pr-number> or gru review <minion-id>",
        )?;

    // Get PR info from the PR number
    get_pr_info_from_number(&pr_state.pr_number).await
}

/// Resolves PR information from a user-provided argument
/// Handles Minion IDs, issue numbers, PR numbers, and URLs
async fn resolve_pr_from_arg(arg: &str) -> Result<(String, String, String, String)> {
    let mut errors = Vec::new();

    // Strategy 1: Try as Minion ID (if it looks like one)
    if looks_like_minion_id(arg) {
        match resolve_pr_from_minion_id(arg).await {
            Ok(pr_info) => return Ok(pr_info),
            Err(e) => errors.push(format!("Minion ID '{}': {:#}", arg, e)),
        }
    }

    // Strategy 2: Try as PR number or URL (existing behavior)
    match parse_pr_info(arg).await {
        Ok(pr_info) => return Ok(pr_info),
        Err(e) => errors.push(format!("PR number/URL '{}': {:#}", arg, e)),
    }

    // Strategy 3: Fallback - try as issue number
    if let Ok(issue_num) = arg.parse::<u64>() {
        match find_pr_for_issue(issue_num).await {
            Ok(pr_num) => match get_pr_info_from_number(&pr_num).await {
                Ok(pr_info) => return Ok(pr_info),
                Err(e) => errors.push(format!(
                    "Issue #{}: Found PR but failed to get info: {:#}",
                    issue_num, e
                )),
            },
            Err(e) => errors.push(format!("Issue #{}: {:#}", issue_num, e)),
        }
    }

    anyhow::bail!(
        "Could not resolve '{}' to a PR.\n\nAttempted strategies:\n{}",
        arg,
        errors
            .iter()
            .map(|e| format!("  • {}", e))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Checks if a string looks like a Minion ID
/// Minion IDs start with 'M' followed by alphanumeric characters
///
/// # Examples
/// Valid: "M001", "M42", "M0tk", "MABC123"
/// Invalid: "42", "M", "m001", "M-42"
fn looks_like_minion_id(s: &str) -> bool {
    s.starts_with('M') && s.len() > 1 && s.chars().all(|c| c.is_alphanumeric())
}

/// Resolves PR information from a Minion ID
async fn resolve_pr_from_minion_id(minion_id: &str) -> Result<(String, String, String, String)> {
    let minion = minion_resolver::resolve_minion(minion_id).await?;

    // Load PR state from the minion's worktree
    let pr_state = PrState::load(&minion.worktree_path)
        .context("Failed to check for PR state file in Minion worktree")?
        .context(format!(
            "Minion {} doesn't have a PR yet. The Minion may still be working on the issue.",
            minion_id
        ))?;

    // Get PR info from the PR number
    get_pr_info_from_number(&pr_state.pr_number).await
}

/// Fetches PR information (owner, repo, pr_num, branch) given a PR number
async fn get_pr_info_from_number(pr_num: &str) -> Result<(String, String, String, String)> {
    // Validate that pr_num is actually a number to provide better error messages
    pr_num
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_num))?;

    // Use parse_pr_info which fetches metadata from GitHub
    parse_pr_info(pr_num).await
}

/// Finds a PR number associated with an issue number
/// Uses gh CLI to search for PRs that link to the issue
async fn find_pr_for_issue(issue_num: u64) -> Result<String> {
    // Safe: issue_num is validated as u64 by the type system, which can only contain digits.
    // This prevents command injection as the format string will never contain shell metacharacters.
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--search",
            &format!("linked:issue#{}", issue_num),
            "--json",
            "number",
            "--limit",
            "1",
        ])
        .output()
        .await
        .context("Failed to execute gh pr list. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to search for PRs linked to issue #{}: {}",
            issue_num,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh pr list output")?;

    // Check if we got any results
    let prs = json.as_array().context("Expected array from gh pr list")?;

    if prs.is_empty() {
        anyhow::bail!("No PR found linked to issue #{}", issue_num);
    }

    let pr_num = prs[0]["number"]
        .as_u64()
        .context("PR number is not a valid integer")?;

    Ok(pr_num.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_minion_id_valid() {
        assert!(looks_like_minion_id("M001"));
        assert!(looks_like_minion_id("M42"));
        assert!(looks_like_minion_id("M0tk"));
        assert!(looks_like_minion_id("MABC123"));
    }

    #[test]
    fn test_looks_like_minion_id_invalid() {
        assert!(!looks_like_minion_id("42")); // No M prefix
        assert!(!looks_like_minion_id("M")); // Too short
        assert!(!looks_like_minion_id("m001")); // Lowercase m
        assert!(!looks_like_minion_id("M-42")); // Contains non-alphanumeric
        assert!(!looks_like_minion_id("M 42")); // Contains space
        assert!(!looks_like_minion_id("")); // Empty string
    }
}
