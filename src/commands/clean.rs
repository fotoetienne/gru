use crate::workspace;
use crate::worktree_scanner;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use tokio::io::AsyncBufReadExt;

/// Check if a file path represents an ephemeral file that's safe to discard
/// Ephemeral files include logs, build artifacts, IDE configs, etc.
fn is_ephemeral_file(path: &Path) -> bool {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Check for specific ephemeral files
    // Note: Cargo.lock is ephemeral for binary projects (auto-regenerated)
    if matches!(
        file_name,
        "events.jsonl" | "Cargo.lock" | ".DS_Store" | "Thumbs.db"
    ) {
        return true;
    }

    // Check for log files
    if file_name.ends_with(".log") {
        return true;
    }

    // Check if any path component is an ephemeral directory
    // Use proper path component matching to avoid false positives
    // (e.g., "target_backup/" should not match "target/")
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if let Some(name_str) = name.to_str() {
                if matches!(
                    name_str,
                    "target"
                        | ".vscode"
                        | ".idea"
                        | ".vs"
                        | "node_modules"
                        | ".next"
                        | "dist"
                        | "build"
                        | ".cache"
                ) {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if worktree contains only ephemeral files
/// Returns (has_modified_files, only_ephemeral)
fn check_worktree_files(worktree_path: &Path) -> Result<(bool, bool)> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &worktree_path.to_string_lossy(),
            "status",
            "--porcelain",
        ])
        .output()
        .context("Failed to check git status")?;

    if !output.status.success() {
        anyhow::bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    if lines.is_empty() {
        // No modified or untracked files
        return Ok((false, true));
    }

    // Check if all modified/untracked files are ephemeral
    let mut has_important_files = false;

    for line in lines {
        // Parse git status --porcelain output
        // Format: XY filename
        // X = status in index, Y = status in working tree
        if line.len() < 3 {
            continue;
        }

        let mut file_path = &line[3..]; // Skip status characters and space

        // Handle quoted filenames (git porcelain format quotes special characters)
        // Example: ?? "file with spaces.txt"
        if file_path.starts_with('"') && file_path.ends_with('"') {
            file_path = &file_path[1..file_path.len() - 1];
        }

        // Handle renamed files (format: "old_name -> new_name")
        // For renames, we need to check both old and new names
        if let Some(arrow_pos) = file_path.find(" -> ") {
            let old_path = &file_path[..arrow_pos];
            let new_path = &file_path[arrow_pos + 4..];

            // Both old and new names must be ephemeral
            if !is_ephemeral_file(Path::new(old_path)) || !is_ephemeral_file(Path::new(new_path)) {
                has_important_files = true;
                break;
            }
            continue;
        }

        let path = Path::new(file_path);

        if !is_ephemeral_file(path) {
            has_important_files = true;
            break;
        }
    }

    Ok((true, !has_important_files))
}

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
        println!("    Repo: {}", wt.repo);

        // Check worktree dirty status to inform user what will happen
        match check_worktree_files(&wt.path) {
            Ok((has_modified, only_ephemeral)) => {
                if !has_modified {
                    println!("    Status: clean");
                } else if only_ephemeral {
                    println!("    Status: dirty (ephemeral files only - will auto-force)");
                } else {
                    println!("    Status: dirty (important files - requires --force)");
                }
            }
            Err(_) => {
                println!("    Status: unknown (unable to check)");
            }
        }
        println!();
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

        // Check if worktree has modified/untracked files
        let (has_modified, only_ephemeral) = match check_worktree_files(&wt.path) {
            Ok(result) => result,
            Err(e) => {
                println!("✗");
                eprintln!("  Error checking worktree status: {}", e);
                failed += 1;
                continue;
            }
        };

        // Decide whether to use --force flag
        // If user specified --force, use it; otherwise auto-force if only ephemeral files
        let force_needed = force || (has_modified && only_ephemeral);

        // Build command arguments - need to store string values to avoid lifetime issues
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(&wt.bare_repo_path)
            .arg("worktree")
            .arg("remove");

        if force_needed {
            cmd.arg("--force");
        }

        cmd.arg(&wt.path);

        // Remove the worktree
        let status = cmd.output()?;

        if status.status.success() {
            if force_needed && !force {
                println!("✓ (auto-forced for ephemeral files)");
            } else if force {
                println!("✓ (forced)");
            } else {
                println!("✓");
            }
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
            let stderr = String::from_utf8_lossy(&status.stderr);
            eprintln!("  Error: {}", stderr);

            // If removal failed due to important files, provide helpful message
            if has_modified
                && !only_ephemeral
                && stderr.contains("contains modified or untracked files")
            {
                eprintln!("  Worktree contains important uncommitted changes.");
                eprintln!("  Run 'gru clean --force' or manually clean the worktree first.");
            }

            failed += 1;
        }
    }

    println!("\nSummary: {} removed, {} failed", removed, failed);

    Ok(if failed > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_ephemeral_file_events_log() {
        assert!(is_ephemeral_file(Path::new("events.jsonl")));
    }

    #[test]
    fn test_is_ephemeral_file_cargo_lock() {
        assert!(is_ephemeral_file(Path::new("Cargo.lock")));
    }

    #[test]
    fn test_is_ephemeral_file_ds_store() {
        assert!(is_ephemeral_file(Path::new(".DS_Store")));
    }

    #[test]
    fn test_is_ephemeral_file_thumbs_db() {
        assert!(is_ephemeral_file(Path::new("Thumbs.db")));
    }

    #[test]
    fn test_is_ephemeral_file_target_dir() {
        assert!(is_ephemeral_file(Path::new("target/debug/gru")));
        assert!(is_ephemeral_file(Path::new("target/release/gru")));
    }

    #[test]
    fn test_is_ephemeral_file_ide_configs() {
        assert!(is_ephemeral_file(Path::new(".vscode/settings.json")));
        assert!(is_ephemeral_file(Path::new(".idea/workspace.xml")));
        assert!(is_ephemeral_file(Path::new(".vs/config.json")));
    }

    #[test]
    fn test_is_ephemeral_file_node_modules() {
        assert!(is_ephemeral_file(Path::new(
            "node_modules/package/index.js"
        )));
    }

    #[test]
    fn test_is_ephemeral_file_build_dirs() {
        assert!(is_ephemeral_file(Path::new("dist/bundle.js")));
        assert!(is_ephemeral_file(Path::new("build/output.js")));
        assert!(is_ephemeral_file(Path::new(".next/cache")));
        assert!(is_ephemeral_file(Path::new(".cache/data")));
    }

    #[test]
    fn test_is_ephemeral_file_log_files() {
        assert!(is_ephemeral_file(Path::new("debug.log")));
        assert!(is_ephemeral_file(Path::new("error.log")));
        assert!(is_ephemeral_file(Path::new("output.log")));
    }

    #[test]
    fn test_is_not_ephemeral_file_source_code() {
        assert!(!is_ephemeral_file(Path::new("src/main.rs")));
        assert!(!is_ephemeral_file(Path::new("src/commands/clean.rs")));
    }

    #[test]
    fn test_is_not_ephemeral_file_config_files() {
        assert!(!is_ephemeral_file(Path::new("Cargo.toml")));
        assert!(!is_ephemeral_file(Path::new("package.json")));
        assert!(!is_ephemeral_file(Path::new(".gitignore")));
    }

    #[test]
    fn test_is_not_ephemeral_file_documentation() {
        assert!(!is_ephemeral_file(Path::new("README.md")));
        assert!(!is_ephemeral_file(Path::new("CLAUDE.md")));
        assert!(!is_ephemeral_file(Path::new("docs/design.md")));
    }

    #[test]
    fn test_is_not_ephemeral_file_tests() {
        assert!(!is_ephemeral_file(Path::new("tests/integration_test.rs")));
    }

    // Edge case tests for path component matching
    #[test]
    fn test_path_component_matching_no_false_positives() {
        // These should NOT be considered ephemeral (false positive prevention)
        assert!(!is_ephemeral_file(Path::new("target_backup/file.txt")));
        assert!(!is_ephemeral_file(Path::new("building/config.yaml")));
        assert!(!is_ephemeral_file(Path::new("dist_old/bundle.js")));
        assert!(!is_ephemeral_file(Path::new(".vscode_settings/foo.json")));
    }

    #[test]
    fn test_path_component_matching_nested_ephemeral() {
        // Nested paths with ephemeral components should be ephemeral
        assert!(is_ephemeral_file(Path::new("src/target/debug/gru")));
        assert!(is_ephemeral_file(Path::new(
            "foo/bar/node_modules/pkg/index.js"
        )));
    }
}
