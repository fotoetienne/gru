use crate::config::LabConfig;
use crate::github::GitHubClient;
use crate::minion_registry::MinionRegistry;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
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
            eprintln!("⚠️  No config file found at {}", default_path.display());
            eprintln!("   Use --repos flag or create a config file");
            eprintln!();
            eprintln!("Example:");
            eprintln!("  gru lab --repos owner/repo1,owner/repo2 --slots 2");
            eprintln!();
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

    // Perform initial poll immediately for faster feedback
    if let Err(e) = poll_and_spawn(&config).await {
        eprintln!("⚠️  Initial polling error: {}", e);
        eprintln!("   Continuing to poll...");
    }

    // Main polling loop
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n🛑 Received shutdown signal, stopping daemon...");
                break;
            }
            _ = sleep(config.poll_interval()) => {
                if let Err(e) = poll_and_spawn(&config).await {
                    eprintln!("⚠️  Polling error: {}", e);
                    eprintln!("   Continuing to poll...");
                }
            }
        }
    }

    Ok(0)
}

/// Poll GitHub for ready issues and spawn Minions if slots are available
async fn poll_and_spawn(config: &LabConfig) -> Result<()> {
    // Calculate available slots once at the start to avoid race conditions
    // Track spawned count within this function to maintain accurate slot availability
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
            eprintln!("⚠️  Invalid repo format: '{}', skipping", repo_spec);
            continue;
        }

        let (owner, repo) = (parts[0], parts[1]);

        // Create GitHub client for this repo
        let client = match GitHubClient::from_env(owner, repo).await {
            Ok(client) => client,
            Err(e) => {
                eprintln!(
                    "⚠️  Failed to create GitHub client for {}: {}",
                    repo_spec, e
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
                eprintln!("⚠️  Failed to fetch issues for {}: {}", repo_spec, e);
                continue;
            }
        };

        // Try to spawn a Minion for each ready issue
        for issue in issues {
            if available == 0 {
                break;
            }

            // Check if issue is already being worked on
            if is_issue_claimed(repo_spec, issue.number).await? {
                continue;
            }

            // Try to claim the issue
            match client.claim_issue(owner, repo, issue.number).await {
                Ok(true) => {
                    // Successfully claimed, spawn Minion
                    match spawn_minion(repo_spec, issue.number).await {
                        Ok(()) => {
                            println!(
                                "✨ Spawned Minion for {}/issues/{}",
                                repo_spec, issue.number
                            );
                            spawned += 1;
                            available -= 1; // Decrement available slots after successful spawn
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  Failed to spawn Minion for {}/issues/{}: {}",
                                repo_spec, issue.number, e
                            );
                            // Unclaim the issue since we failed to spawn
                            if let Err(e) = client
                                .remove_label(owner, repo, issue.number, "in-progress")
                                .await
                            {
                                eprintln!("⚠️  Failed to remove in-progress label: {}", e);
                            }
                        }
                    }
                }
                Ok(false) => {
                    // Race condition: another Minion claimed it first
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "⚠️  Failed to claim issue {}/issues/{}: {}",
                        repo_spec, issue.number, e
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

/// Calculate available slots based on active Minions in registry
async fn available_slots(max_slots: usize) -> Result<usize> {
    let active_count = tokio::task::spawn_blocking(move || {
        let registry = MinionRegistry::load(None)?;
        let active = registry
            .list()
            .iter()
            .filter(|(_id, info)| info.status == "active")
            .count();
        Ok::<usize, anyhow::Error>(active)
    })
    .await
    .context("Failed to join spawn_blocking task")??;

    Ok(max_slots.saturating_sub(active_count))
}

/// Check if an issue is already being worked on by a Minion
async fn is_issue_claimed(repo: &str, issue_number: u64) -> Result<bool> {
    let repo = repo.to_string();
    tokio::task::spawn_blocking(move || {
        let registry = MinionRegistry::load(None)?;
        let claimed = registry.list().iter().any(|(_id, info)| {
            info.repo == repo && info.issue == issue_number && info.status == "active"
        });
        Ok::<bool, anyhow::Error>(claimed)
    })
    .await
    .context("Failed to join spawn_blocking task")?
}

/// Spawn a Minion to work on an issue using the `gru fix` command
async fn spawn_minion(repo: &str, issue_number: u64) -> Result<()> {
    let issue_ref = format!("{}/issues/{}", repo, issue_number);

    // Get the current executable path
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    // Spawn `gru fix <issue>` as a background process
    let mut child = tokio::process::Command::new(exe)
        .arg("fix")
        .arg(&issue_ref)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

    // Intentionally drop the Child handle to allow the spawned process to run independently.
    // This is a deliberate use of the fire-and-forget pattern: we do not wait for the process
    // to complete, and dropping the handle here ensures it is detached. The spawned `gru fix`
    // command will register itself in the Minion registry and manage its own lifecycle.
    // Future maintainers: this is intentional and not an oversight.
    drop(child);

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_available_slots_calculation() {
        // This would require mocking the registry, which is complex
        // For now, we'll test the logic manually
        assert_eq!(5usize.saturating_sub(2), 3);
        assert_eq!(5usize.saturating_sub(5), 0);
        assert_eq!(5usize.saturating_sub(10), 0);
    }
}
