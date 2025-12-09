use crate::workspace;
use crate::worktree_scanner;
use anyhow::{Context, Result};
use std::io::Write;
use tokio::io::AsyncBufReadExt;

/// Handles the clean command to remove merged/closed worktrees
pub async fn handle_clean(dry_run: bool, force: bool, base_branch: &str) -> Result<i32> {
    let ws = workspace::Workspace::new().context("Failed to initialize workspace")?;

    println!("Scanning for worktrees in {}...", ws.repos().display());

    // Discover all worktrees
    let worktrees =
        worktree_scanner::discover_worktrees(ws.repos()).context("Failed to discover worktrees")?;

    if worktrees.is_empty() {
        println!("No worktrees found.");
        return Ok(0);
    }

    println!("Found {} worktrees. Checking status...\n", worktrees.len());

    // Check status of each worktree
    let mut cleanable = Vec::new();
    for wt in worktrees {
        let status = wt
            .status(base_branch)
            .with_context(|| format!("Failed to check status of {}", wt.path.display()))?;

        if status != worktree_scanner::WorktreeStatus::Active {
            cleanable.push((wt, status));
        }
    }

    if cleanable.is_empty() {
        println!("No worktrees to clean.");
        return Ok(0);
    }

    // Display cleanable worktrees
    println!("Cleanable worktrees:\n");
    for (wt, status) in &cleanable {
        let reason = match status {
            worktree_scanner::WorktreeStatus::Merged => "branch merged",
            worktree_scanner::WorktreeStatus::IssueClosed => "issue closed",
            worktree_scanner::WorktreeStatus::RemoteDeleted => "remote deleted",
            worktree_scanner::WorktreeStatus::Active => unreachable!(),
        };
        println!("  {} ({})", wt.path.display(), reason);
        println!("    Branch: {}", wt.branch);
        println!("    Repo: {}\n", wt.repo);
    }

    if dry_run {
        println!("Dry run mode - no worktrees were removed.");
        return Ok(0);
    }

    // Confirm removal unless force flag is set
    if !force {
        print!("Remove {} worktrees? [y/N]: ", cleanable.len());
        std::io::stdout().flush()?;

        let mut input = String::new();
        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);
        reader.read_line(&mut input).await?;
        let input = input.trim().to_lowercase();

        if input != "y" && input != "yes" {
            println!("Cancelled.");
            return Ok(0);
        }
    }

    // Remove worktrees
    println!("\nRemoving worktrees...");
    let mut removed = 0;
    let mut failed = 0;

    for (wt, _) in cleanable {
        print!("Removing {}... ", wt.path.display());
        std::io::stdout().flush()?;

        // Remove the worktree
        let status = std::process::Command::new("git")
            .args([
                "-C",
                &wt.bare_repo_path.to_string_lossy(),
                "worktree",
                "remove",
                &wt.path.to_string_lossy(),
            ])
            .output()?;

        if status.status.success() {
            println!("✓");
            removed += 1;

            // Also remove the branch from the bare repository
            let branch_result = std::process::Command::new("git")
                .args([
                    "-C",
                    &wt.bare_repo_path.to_string_lossy(),
                    "branch",
                    "-D",
                    &wt.branch,
                ])
                .output();

            if let Err(e) = branch_result {
                eprintln!("  Warning: Failed to delete branch: {}", e);
            }
        } else {
            println!("✗");
            eprintln!("  Error: {}", String::from_utf8_lossy(&status.stderr));
            failed += 1;
        }
    }

    println!("\nSummary: {} removed, {} failed", removed, failed);

    Ok(if failed > 0 { 1 } else { 0 })
}
