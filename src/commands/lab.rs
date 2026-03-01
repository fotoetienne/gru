use crate::config::LabConfig;
use crate::github::GitHubClient;
use crate::minion_registry::{is_process_alive, MinionRegistry};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Child;
use tokio::time::sleep;

/// Handles the lab daemon command
pub async fn handle_lab(
    config_path: Option<PathBuf>,
    repos: Option<Vec<String>>,
    poll_interval: Option<u64>,
    max_slots: Option<usize>,
) -> Result<i32> {
    // Load configuration
    let config = if let Some(path) = config_path {
        LabConfig::load(&path)?
    } else {
        let default_path = LabConfig::default_path()?;
        if default_path.exists() {
            LabConfig::load(&default_path)?
        } else if repos.is_none() {
            log::warn!("⚠️  No config file found at {}", default_path.display());
            log::warn!("   Use --repos flag or create a config file");
            log::warn!("");
            log::warn!("Example:");
            log::warn!("  gru lab --repos owner/repo1,owner/repo2 --slots 2");
            log::warn!("");
            anyhow::bail!("No repositories configured");
        } else {
            LabConfig::default()
        }
    };

    // Apply CLI overrides
    let config = config.with_overrides(repos, poll_interval, max_slots);

    // Validate final configuration
    config.validate()?;

    println!("🚀 Starting Gru Lab daemon");
    println!(
        "📋 Monitoring {} repository(ies)",
        config.daemon.repos.len()
    );
    println!("🔄 Poll interval: {}s", config.daemon.poll_interval_secs);
    println!("🎰 Max concurrent slots: {}", config.daemon.max_slots);
    println!("🏷️  Watching for label: {}", config.daemon.label);
    println!();

    for repo in &config.daemon.repos {
        println!("  • {}", repo);
    }

    println!();
    println!("Press Ctrl-C to stop...");
    println!();

    // Track child processes for graceful shutdown
    let mut children: Vec<Child> = Vec::new();

    // Perform initial poll immediately for faster feedback
    if let Err(e) = poll_and_spawn(&config, &mut children).await {
        log::warn!("⚠️  Initial polling error: {}", e);
        log::warn!("   Continuing to poll...");
    }

    // Main polling loop
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n🛑 Received shutdown signal, stopping daemon...");
                shutdown_children(&mut children).await;
                break;
            }
            _ = sleep(config.poll_interval()) => {
                // Clean up finished child processes
                reap_children(&mut children);

                if let Err(e) = poll_and_spawn(&config, &mut children).await {
                    log::warn!("⚠️  Polling error: {}", e);
                    log::warn!("   Continuing to poll...");
                }
            }
        }
    }

    Ok(0)
}

/// Remove finished child processes from the tracking vec
fn reap_children(children: &mut Vec<Child>) {
    let mut i = 0;
    while i < children.len() {
        match children[i].try_wait() {
            Ok(Some(status)) => {
                log::info!("Minion process exited with status: {}", status);
                children.swap_remove(i);
            }
            Ok(None) => {
                i += 1; // Still running
            }
            Err(e) => {
                log::warn!("Failed to check child process status: {}", e);
                i += 1;
            }
        }
    }
}

/// Signal all child processes to shut down gracefully on Ctrl-C
async fn shutdown_children(children: &mut [Child]) {
    if children.is_empty() {
        return;
    }

    // Reap already-exited children first for an accurate running count
    let mut running_pids = Vec::new();
    for child in children.iter_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                log::info!("Minion process already exited with status: {}", status);
            }
            Ok(None) => {
                if let Some(pid) = child.id() {
                    running_pids.push(pid);
                }
            }
            Err(e) => {
                log::warn!("Failed to check child process status: {}", e);
            }
        }
    }

    if running_pids.is_empty() {
        println!("No running Minion processes to shut down.");
        return;
    }

    println!(
        "🔪 Signaling {} running Minion(s) to shut down...",
        running_pids.len()
    );

    // Send SIGTERM to all still-running children
    for pid in &running_pids {
        #[cfg(unix)]
        {
            // SAFETY: kill with SIGTERM is safe - it requests graceful termination.
            // The PID is valid because we just obtained it from the child handle.
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
    }

    // Wait briefly for graceful shutdown
    println!("⏳ Waiting for Minions to exit...");
    sleep(Duration::from_secs(5)).await;

    // Force-kill any remaining processes and reap to avoid zombies
    for child in children.iter_mut() {
        match child.try_wait() {
            Ok(Some(_)) => {} // Already exited
            _ => {
                log::warn!("Force-killing Minion process that didn't exit gracefully");
                let _ = child.kill().await;
                // Reap the child process to avoid leaving a zombie
                let _ = child.wait().await;
            }
        }
    }
}

/// Poll GitHub for ready issues and spawn Minions if slots are available
async fn poll_and_spawn(config: &LabConfig, children: &mut Vec<Child>) -> Result<()> {
    // Prune stale registry entries (worktrees that no longer exist)
    prune_stale_entries().await?;

    // Calculate available slots using PID liveness (not registry status string)
    let mut available = available_slots(config.daemon.max_slots).await?;

    if available == 0 {
        // All slots occupied, skip this poll
        return Ok(());
    }

    let mut spawned = 0;

    // Poll each configured repository
    for repo_spec in &config.daemon.repos {
        if available == 0 {
            break;
        }

        // Parse owner/repo
        let parts: Vec<&str> = repo_spec.split('/').collect();
        if parts.len() != 2 {
            log::warn!("⚠️  Invalid repo format: '{}', skipping", repo_spec);
            continue;
        }

        let (owner, repo) = (parts[0], parts[1]);

        // Create GitHub client for this repo
        let client = match GitHubClient::from_env(owner, repo).await {
            Ok(client) => client,
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to create GitHub client for {}: {}",
                    repo_spec,
                    e
                );
                continue;
            }
        };

        // Fetch issues with the configured label
        let issues = match client
            .list_issues_with_label(owner, repo, &config.daemon.label)
            .await
        {
            Ok(issues) => issues,
            Err(e) => {
                log::warn!("⚠️  Failed to fetch issues for {}: {}", repo_spec, e);
                continue;
            }
        };

        // Try to spawn a Minion for each ready issue
        for issue in issues {
            if available == 0 {
                break;
            }

            // Check if issue is already being worked on (by a live process)
            if is_issue_claimed(repo_spec, issue.number).await? {
                continue;
            }

            // Try to claim the issue
            match client.claim_issue(owner, repo, issue.number).await {
                Ok(true) => {
                    // Successfully claimed, spawn Minion
                    match spawn_minion(repo_spec, issue.number).await {
                        Ok(child) => {
                            children.push(child);
                            println!(
                                "✨ Spawned Minion for {}/issues/{}",
                                repo_spec, issue.number
                            );
                            spawned += 1;
                            available -= 1; // Decrement available slots after successful spawn
                        }
                        Err(e) => {
                            log::warn!(
                                "⚠️  Failed to spawn Minion for {}/issues/{}: {}",
                                repo_spec,
                                issue.number,
                                e
                            );
                            // Unclaim the issue since we failed to spawn
                            if let Err(e) = client
                                .remove_label(owner, repo, issue.number, "in-progress")
                                .await
                            {
                                log::warn!("⚠️  Failed to remove in-progress label: {}", e);
                            }
                        }
                    }
                }
                Ok(false) => {
                    // Race condition: another Minion claimed it first
                    continue;
                }
                Err(e) => {
                    log::warn!(
                        "⚠️  Failed to claim issue {}/issues/{}: {}",
                        repo_spec,
                        issue.number,
                        e
                    );
                    continue;
                }
            }
        }
    }

    if spawned > 0 {
        println!();
    }

    Ok(())
}

/// Prune stale registry entries where worktrees no longer exist
async fn prune_stale_entries() -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut registry = MinionRegistry::load(None)?;
        let minions = registry.list();

        let stale_ids: Vec<String> = minions
            .iter()
            .filter(|(_id, info)| !info.worktree.exists())
            .map(|(id, _)| id.clone())
            .collect();

        if !stale_ids.is_empty() {
            let count = registry.remove_batch(&stale_ids)?;
            log::info!("🗑️  Pruned {} stale Minion(s) from registry", count);
        }

        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Failed to join spawn_blocking task")?
}

/// Calculate available slots based on PID liveness of registered Minions
async fn available_slots(max_slots: usize) -> Result<usize> {
    let active_count = tokio::task::spawn_blocking(move || {
        let registry = MinionRegistry::load(None)?;
        let active = registry
            .list()
            .iter()
            .filter(|(_id, info)| info.pid.is_some_and(is_process_alive))
            .count();
        Ok::<usize, anyhow::Error>(active)
    })
    .await
    .context("Failed to join spawn_blocking task")??;

    Ok(max_slots.saturating_sub(active_count))
}

/// Check if an issue is already being worked on by a live Minion process
async fn is_issue_claimed(repo: &str, issue_number: u64) -> Result<bool> {
    let repo = repo.to_string();
    tokio::task::spawn_blocking(move || {
        let registry = MinionRegistry::load(None)?;
        let claimed = registry.list().iter().any(|(_id, info)| {
            info.repo == repo
                && info.issue == issue_number
                && info.pid.is_some_and(is_process_alive)
        });
        Ok::<bool, anyhow::Error>(claimed)
    })
    .await
    .context("Failed to join spawn_blocking task")?
}

/// Spawn a Minion to work on an issue using the `gru fix` command.
/// Returns the child process handle for lifecycle tracking.
async fn spawn_minion(repo: &str, issue_number: u64) -> Result<Child> {
    let issue_ref = format!("{}/issues/{}", repo, issue_number);

    // Get the current executable path
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    // Create log directory and open log file for this minion's output
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    let log_dir = home.join(".gru").join("state").join("logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let safe_repo = repo.replace('/', "-");
    let log_path = log_dir.join(format!("{}-issue-{}.log", safe_repo, issue_number));
    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;
    let stderr_file = stdout_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    // Spawn `gru fix <issue>` as a background process with output captured to log file
    let mut child = tokio::process::Command::new(exe)
        .arg("fix")
        .arg(&issue_ref)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("Failed to spawn gru fix command")?;

    // Give the process a moment to fail if there are startup issues
    // This prevents phantom slot occupancy from processes that immediately fail
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!(
            "Spawned process for {}/issues/{} exited immediately with status: {:?}",
            repo,
            issue_number,
            status
        );
    }

    println!("📝 Log: {}", log_path.display());

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reap_children_empty() {
        let mut children: Vec<Child> = Vec::new();
        reap_children(&mut children);
        assert!(children.is_empty());
    }

    #[test]
    fn test_log_path_construction() {
        let safe_repo = "owner/repo".replace('/', "-");
        let log_name = format!("{}-issue-{}.log", safe_repo, 42);
        assert_eq!(log_name, "owner-repo-issue-42.log");
    }
}
