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
    // Clone bare_path here since GitRepo::new takes ownership, but we need it later for git fetch
    let git_repo = git::GitRepo::new(&owner, &repo, bare_path.clone());

    // Ensure bare repository is cloned/updated
    println!("📦 Ensuring repository is cloned...");
    git_repo.ensure_bare_clone().context(format!(
        "Failed to clone or update repository for PR {}",
        pr_num
    ))?;

    // Fetch the specific PR branch to ensure it's available locally
    println!("🔄 Fetching PR branch: {}", branch);
    let fetch_output = Command::new("git")
        .arg("-C")
        .arg(&bare_path)
        .arg("fetch")
        .arg("origin")
        .arg(format!("{}:{}", branch, branch))
        .output()
        .await
        .context("Failed to execute git fetch for PR branch")?;

    if !fetch_output.status.success() {
        let stderr = String::from_utf8_lossy(&fetch_output.stderr);
        anyhow::bail!("Failed to fetch PR branch '{}': {}", branch, stderr);
    }

    // Create worktree path based on branch name
    let repo_name = format!("{}/{}", owner, repo);
    let worktree_path = workspace
        .work_dir(&repo_name, &branch)
        .context("Failed to compute worktree path")?;

    // Create worktree if it doesn't exist, or reuse existing one
    if !worktree_path.exists() {
        println!("🌿 Creating worktree for branch: {}", branch);
        git_repo
            .checkout_worktree(&branch, &worktree_path)
            .context(format!("Failed to checkout worktree for PR {}", pr_num))?;
    } else {
        println!("📂 Using existing worktree: {}", worktree_path.display());
    }

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
