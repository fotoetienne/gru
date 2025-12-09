use crate::git;
use crate::url_utils::parse_pr_info;
use crate::workspace;
use anyhow::{Context, Result};
use tokio::process::Command;

/// Handles the review command by setting up workspace and delegating to the Claude CLI
/// Returns the exit code from the claude process
pub async fn handle_review(pr: &str) -> Result<i32> {
    // Parse PR information and fetch metadata from GitHub
    let (owner, repo, pr_num, branch) = parse_pr_info(pr).await?;

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
        .arg(format!("/pr_review {}", pr))
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
