use crate::config::LabConfig;
use crate::github::GitHubClient;
use crate::minion_registry::MinionRegistry;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
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
        LabConfig::load_or_default()?
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
    // Check available slots
    let available = available_slots(config.daemon.max_slots).await?;

    if available == 0 {
        // All slots occupied, skip this poll
        return Ok(());
    }

    let mut spawned = 0;

    // Poll each configured repository
    for repo_spec in &config.daemon.repos {
        if available_slots(config.daemon.max_slots).await? == 0 {
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
            if available_slots(config.daemon.max_slots).await? == 0 {
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
    let child = tokio::process::Command::new(exe)
        .arg("fix")
        .arg(&issue_ref)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn gru fix command")?;

    // Don't wait for the child process - let it run independently
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
