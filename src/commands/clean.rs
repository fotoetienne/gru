use crate::minion_registry::with_registry;
use crate::workspace;
use crate::worktree_scanner;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// Check if a file path represents an ephemeral file that's safe to discard
/// Ephemeral files include logs, build artifacts, IDE configs, etc.
fn is_ephemeral_file(path: &Path) -> bool {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Check for specific ephemeral files
    // Note: Cargo.lock is ephemeral for binary projects (auto-regenerated)
    // Note: events.jsonl and PR_DESCRIPTION.md live in the minion_dir in new
    // layouts (parent of checkout/), but we keep them here for legacy worktrees
    // where these files still live at the git worktree root.
    if matches!(
        file_name,
        "Cargo.lock" | ".DS_Store" | "Thumbs.db" | "events.jsonl" | "PR_DESCRIPTION.md"
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
async fn check_worktree_files(worktree_path: &Path) -> Result<(bool, bool)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await
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
    let worktrees = worktree_scanner::discover_worktrees(ws.repos())
        .await
        .context("Failed to discover worktrees")?;

    if worktrees.is_empty() {
        println!("No worktrees found.");
        return Ok(0);
    }

    println!("Found {} worktrees. Checking status...\n", worktrees.len());

    // Load registry and get active minion worktrees
    // Note: There's a narrow race condition where a minion could start between registry load
    // and worktree checks. This is acceptable given the trade-offs and typical usage patterns.
    let (active_minion_worktrees, stopped_minion_worktrees, stopped_minion_ids) =
        with_registry(|registry| {
            // Partition registry into active (live process) and stopped minion worktree paths.
            // Active minions are protected from cleanup; stopped minions are cleanable as a fallback
            // even when git status checks (merged/closed/remote-deleted) find nothing.
            let mut active = HashSet::new();
            let mut stopped = HashSet::new();
            // Track stopped minions by ID for orphan cleanup (registry entries with no git worktree)
            let mut stopped_ids: Vec<(String, std::path::PathBuf)> = Vec::new();

            for (minion_id, info) in registry.list() {
                let is_alive = match info.pid {
                    // On non-Unix, is_process_alive always returns false, so we conservatively
                    // assume a recorded PID is alive to avoid cleaning active worktrees.
                    Some(pid) => {
                        cfg!(not(unix)) || crate::minion_registry::is_process_alive(pid)
                    }
                    // No PID recorded: trust the mode field. Legacy entries (pre-PID) default
                    // to Stopped, so they won't block cleanup. But if mode says the minion is
                    // running, be conservative and protect the worktree.
                    None => matches!(
                        info.mode,
                        crate::minion_registry::MinionMode::Autonomous
                            | crate::minion_registry::MinionMode::Interactive
                    ),
                };
                // Use checkout_path() (not worktree) because the scanner discovers
                // checkout paths from `git worktree list`. The registry stores the
                // minion_dir (parent of checkout/), so comparing raw worktree paths
                // would never match and active minions would not be protected.
                let checkout = info.checkout_path();
                let canonical = match checkout.canonicalize() {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!(
                            "Warning: Failed to canonicalize checkout path for minion {}: {} (error: {})",
                            minion_id,
                            checkout.display(),
                            e
                        );
                        log::warn!(
                            "         Using original path, but this may cause comparison mismatches."
                        );
                        checkout
                    }
                };

                if is_alive {
                    active.insert(canonical);
                } else {
                    stopped.insert(canonical.clone());
                    stopped_ids.push((minion_id, canonical));
                }
            }

            Ok((active, stopped, stopped_ids))
        })
        .await
        .context(
            "Failed to load minion registry from the default location. This may be due to a missing \
             or corrupt registry file, or insufficient file permissions. The default registry is \
             typically located at ~/.gru/state/minions.json.",
        )?;

    // Prune stale worktree references (directory no longer exists on disk)
    let (stale, worktrees): (Vec<_>, Vec<_>) =
        worktrees.into_iter().partition(|wt| !wt.path.exists());

    if !stale.is_empty() {
        let mut bare_repos_to_prune = HashSet::new();
        for wt in &stale {
            println!(
                "Stale worktree reference: {} (directory missing)",
                wt.path.display()
            );
            bare_repos_to_prune.insert(wt.bare_repo_path.clone());
        }

        if !dry_run {
            for bare_repo in &bare_repos_to_prune {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(bare_repo.as_path())
                    .arg("worktree")
                    .arg("prune")
                    .output()
                    .await;

                match output {
                    Ok(result) if result.status.success() => {}
                    Ok(result) => {
                        log::warn!(
                            "Warning: git worktree prune failed for {}: {}",
                            bare_repo.display(),
                            String::from_utf8_lossy(&result.stderr)
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "Warning: Failed to run git worktree prune for {}: {}",
                            bare_repo.display(),
                            e
                        );
                    }
                }
            }
            println!("Pruned {} stale worktree reference(s).\n", stale.len());
        } else {
            println!(
                "Found {} stale worktree reference(s) to prune.\n",
                stale.len()
            );
        }
    }

    // Build set of discovered worktree paths for orphan detection later.
    // Use fallback-to-original on canonicalize failure for consistent path comparison.
    let discovered_paths: HashSet<_> = worktrees
        .iter()
        .map(|wt| wt.path.canonicalize().unwrap_or_else(|_| wt.path.clone()))
        .collect();

    // Check status of each worktree
    let mut cleanable = Vec::new();
    let mut skipped_active_minions = Vec::new();
    let mut skipped_open_prs = Vec::new();
    for wt in worktrees {
        // Skip if this worktree has an active minion
        // Canonicalize the worktree path for reliable comparison
        let canonical_wt_path = match wt.path.canonicalize() {
            Ok(canonical) => canonical,
            Err(e) => {
                log::warn!(
                    "Warning: Failed to canonicalize worktree path: {} (error: {})",
                    wt.path.display(),
                    e
                );
                log::warn!("         Using original path for comparison.");
                wt.path.clone()
            }
        };

        if active_minion_worktrees.contains(&canonical_wt_path) {
            skipped_active_minions.push(wt);
            continue;
        }

        let status = wt
            .status(base_branch)
            .await
            .with_context(|| format!("Failed to check status of {}", wt.path.display()))?;

        if status != worktree_scanner::WorktreeStatus::Active {
            cleanable.push((wt, status));
        } else if stopped_minion_worktrees.contains(&canonical_wt_path) {
            // Git status says "active" but the minion process is stopped.
            // Before marking as cleanable, check if there's an open PR under review.
            let has_open_pr = wt.check_has_open_pr().await.unwrap_or_else(|e| {
                log::warn!("Failed to check for open PRs: {}", e);
                // Conservative default: assume an open PR exists so we don't
                // accidentally clean a worktree under review.
                true
            });

            if has_open_pr {
                skipped_open_prs.push(wt);
            } else {
                cleanable.push((wt, worktree_scanner::WorktreeStatus::MinionStopped));
            }
        }
    }

    // Find orphaned registry entries (stopped minions not discovered as git worktrees)
    // These are entries like ad-hoc prompts that have a work directory but no bare repo.
    let orphaned_minions: Vec<_> = stopped_minion_ids
        .into_iter()
        .filter(|(_id, path)| !discovered_paths.contains(path))
        .collect();

    // Display skipped worktrees with active minions
    if !skipped_active_minions.is_empty() {
        println!(
            "Skipped {} worktree(s) with active minions:\n",
            skipped_active_minions.len()
        );
        for wt in &skipped_active_minions {
            println!("  {} (active minion)", wt.path.display());
            println!("    Branch: {}", wt.branch);
            println!("    Repo: {}", wt.repo);
            println!();
        }
    }

    // Display skipped worktrees with open PRs
    if !skipped_open_prs.is_empty() {
        println!(
            "Skipped {} worktree(s) with open PRs:\n",
            skipped_open_prs.len()
        );
        for wt in &skipped_open_prs {
            println!("  {} (open PR)", wt.path.display());
            println!("    Branch: {}", wt.branch);
            println!("    Repo: {}", wt.repo);
            println!();
        }
    }

    if cleanable.is_empty() && orphaned_minions.is_empty() {
        let has_skips = !skipped_active_minions.is_empty() || !skipped_open_prs.is_empty();
        if !has_skips {
            println!("No worktrees to clean.");
        } else {
            println!(
                "No cleanable worktrees found (skipped worktrees have active minions or open PRs)."
            );
        }
        return Ok(0);
    }

    // Display cleanable worktrees
    if !cleanable.is_empty() {
        println!("Cleanable worktrees:\n");
        for (wt, status) in &cleanable {
            let reason = match status {
                worktree_scanner::WorktreeStatus::Merged => "branch merged",
                worktree_scanner::WorktreeStatus::PrMerged => "PR merged",
                worktree_scanner::WorktreeStatus::IssueClosed => "issue closed",
                worktree_scanner::WorktreeStatus::RemoteDeleted => "remote deleted",
                worktree_scanner::WorktreeStatus::MinionStopped => "minion stopped",
                worktree_scanner::WorktreeStatus::Active => {
                    unreachable!("Active worktree should not be in cleanable list")
                }
            };
            println!("  {} ({})", wt.path.display(), reason);
            println!("    Branch: {}", wt.branch);
            println!("    Repo: {}", wt.repo);

            // Check worktree dirty status to inform user what will happen
            match check_worktree_files(&wt.path).await {
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
    }

    // Display orphaned registry entries
    if !orphaned_minions.is_empty() {
        println!("Orphaned registry entries:\n");
        for (minion_id, path) in &orphaned_minions {
            println!("  {} (minion stopped, no git worktree)", minion_id);
            println!("    Path: {}", path.display());
            println!();
        }
    }

    let total_cleanable = cleanable.len() + orphaned_minions.len();

    if dry_run {
        println!("Dry run mode - nothing was removed.");
        return Ok(0);
    }

    // Confirm removal unless force flag is set
    if !force {
        print!("Remove {} item(s)? [y/N]: ", total_cleanable);
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
    // Collect minion IDs to remove from registry in a single batch
    let mut registry_ids_to_remove: Vec<String> = Vec::new();

    for (wt, _) in cleanable {
        print!("Removing {}... ", wt.path.display());
        std::io::stdout().flush()?;

        // Check if worktree has modified/untracked files
        let (has_modified, only_ephemeral) = match check_worktree_files(&wt.path).await {
            Ok(result) => result,
            Err(e) => {
                println!("✗");
                log::error!("  Error checking worktree status: {}", e);
                failed += 1;
                continue;
            }
        };

        // Decide whether to use --force flag
        // If user specified --force, use it; otherwise auto-force if only ephemeral files
        let force_needed = force || (has_modified && only_ephemeral);

        // Build command arguments - need to store string values to avoid lifetime issues
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&wt.bare_repo_path)
            .arg("worktree")
            .arg("remove");

        if force_needed {
            cmd.arg("--force");
        }

        cmd.arg(&wt.path);

        // Remove the worktree
        let status = cmd.output().await.with_context(|| {
            format!(
                "failed to run `git worktree remove{}` for path {} (bare repo: {})",
                if force_needed { " --force" } else { "" },
                wt.path.display(),
                wt.bare_repo_path.display(),
            )
        })?;

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
            let branch_result = Command::new("git")
                .arg("-C")
                .arg(&wt.bare_repo_path)
                .arg("branch")
                .arg("-D")
                .arg(&wt.branch)
                .output()
                .await;

            if let Err(e) = branch_result {
                log::warn!("  Warning: Failed to delete branch: {}", e);
            }

            // For new-style layouts (checkout/ subdir), clean up the parent minion_dir
            // which contains metadata files (events.jsonl, .gru_pr_state.json, etc.)
            if wt
                .path
                .file_name()
                .map(|n| n == "checkout")
                .unwrap_or(false)
            {
                if let Some(parent_dir) = wt.path.parent() {
                    // Safety: only remove directories inside the Gru workspace work dir
                    // to avoid accidentally deleting user-created worktrees outside ~/.gru/
                    let work_dir = ws
                        .work()
                        .canonicalize()
                        .unwrap_or_else(|_| ws.work().to_path_buf());
                    let safe_to_remove = parent_dir
                        .canonicalize()
                        .map(|p| p.starts_with(&work_dir))
                        .unwrap_or(false);

                    if safe_to_remove && parent_dir.exists() {
                        if let Err(e) = tokio::fs::remove_dir_all(parent_dir).await {
                            log::warn!(
                                "  Warning: Failed to remove minion directory {}: {}",
                                parent_dir.display(),
                                e
                            );
                        }
                    } else if parent_dir.exists() && !safe_to_remove {
                        log::warn!(
                            "  Skipping minion dir cleanup: {} is outside workspace",
                            parent_dir.display()
                        );
                    }
                }
            }

            // Queue minion ID for batch registry removal
            // For new-style layouts, the minion ID is the parent dir name
            // For legacy layouts, the minion ID is the worktree dir name
            let dir_for_id = if wt
                .path
                .file_name()
                .map(|n| n == "checkout")
                .unwrap_or(false)
            {
                wt.path.parent().and_then(|p| p.file_name())
            } else {
                wt.path.file_name()
            };
            if let Some(dir_name) = dir_for_id {
                if let Some(dir_str) = dir_name.to_str() {
                    registry_ids_to_remove.push(dir_str.to_string());
                }
            }
        } else {
            println!("✗");
            let stderr = String::from_utf8_lossy(&status.stderr);
            log::error!("  Error: {}", stderr);

            // If removal failed due to important files, provide helpful message
            if has_modified
                && !only_ephemeral
                && stderr.contains("contains modified or untracked files")
            {
                log::warn!("  Worktree contains important uncommitted changes.");
                log::warn!("  Run 'gru clean --force' or manually clean the worktree first.");
            }

            failed += 1;
        }
    }

    // Clean up orphaned registry entries
    if !orphaned_minions.is_empty() {
        println!("\nCleaning orphaned registry entries...");
        for (minion_id, path) in &orphaned_minions {
            print!("  Removing {} ({})... ", minion_id, path.display());
            std::io::stdout().flush()?;

            // Safety: only remove directories inside the workspace work directory
            // to guard against corrupt or hand-edited registry entries.
            // Canonicalize both paths to prevent traversal attacks (e.g., "work/../../etc").
            if path.exists() {
                let canonical_path = match path.canonicalize() {
                    Ok(p) => p,
                    Err(e) => {
                        println!("✗");
                        log::warn!(
                            "  Skipping removal: failed to canonicalize {} ({})",
                            path.display(),
                            e
                        );
                        failed += 1;
                        continue;
                    }
                };
                let work_dir = ws
                    .work()
                    .canonicalize()
                    .unwrap_or_else(|_| ws.work().to_path_buf());
                if !canonical_path.starts_with(&work_dir) {
                    println!("✗");
                    log::warn!(
                        "  Skipping removal: path {} is outside workspace ({})",
                        canonical_path.display(),
                        work_dir.display()
                    );
                    failed += 1;
                    continue;
                }
                if let Err(e) = tokio::fs::remove_dir_all(&canonical_path).await {
                    println!("✗");
                    log::error!("  Error removing directory: {}", e);
                    failed += 1;
                    continue;
                }
            }

            registry_ids_to_remove.push(minion_id.clone());
            println!("✓");
            removed += 1;
        }
    }

    // Batch-remove all cleaned minions from the registry in a single load/save cycle
    if !registry_ids_to_remove.is_empty() {
        if let Err(e) = with_registry(move |registry| {
            registry.remove_batch(&registry_ids_to_remove)?;
            Ok(())
        })
        .await
        {
            log::warn!("Warning: Failed to update registry after cleanup: {}", e);
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
        // Kept as ephemeral for legacy worktrees where it lives at git root
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
    fn test_is_ephemeral_file_pr_description() {
        // Kept as ephemeral for legacy worktrees where it lives at git root
        assert!(is_ephemeral_file(Path::new("PR_DESCRIPTION.md")));
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
