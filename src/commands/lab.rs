use crate::config::{parse_repo_entry, LabConfig};
use crate::github::{list_ready_issues_via_cli, GitHubClient};
use crate::labels;
use crate::minion_registry::{is_process_alive, with_registry};
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

        // Parse owner/repo or host/owner/repo
        let (host, owner, repo) = match parse_repo_entry(repo_spec) {
            Some(parsed) => parsed,
            None => {
                log::warn!("⚠️  Invalid repo format: '{}', skipping", repo_spec);
                continue;
            }
        };

        // Fetch ready issues, excluding blocked ones (both GitHub-blocked and minion:blocked).
        // Try CLI first (supports -is:blocked qualifier), fall back to octocrab with
        // client-side filtering if CLI is unavailable.
        let issue_numbers =
            match list_ready_issues_via_cli(&owner, &repo, &host, &config.daemon.label).await {
                Ok(numbers) => numbers,
                Err(cli_err) => {
                    log::warn!(
                        "⚠️  CLI issue fetch failed for {}: {}, trying API fallback",
                        repo_spec,
                        cli_err
                    );
                    match fallback_list_issues(&owner, &repo, &host, &config.daemon.label).await {
                        Ok(numbers) => numbers,
                        Err(e) => {
                            log::warn!("⚠️  API fallback also failed for {}: {}", repo_spec, e);
                            continue;
                        }
                    }
                }
            };

        // Create GitHub client for claiming issues
        let client = match GitHubClient::from_env_with_host(&owner, &repo, &host).await {
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

        // Try to spawn a Minion for each ready issue
        for issue_number in issue_numbers {
            if available == 0 {
                break;
            }

            // Check if issue is already being worked on (by a live process)
            if is_issue_claimed(repo_spec, issue_number).await? {
                continue;
            }

            // Try to claim the issue
            match client.claim_issue(&owner, &repo, issue_number).await {
                Ok(true) => {
                    // Successfully claimed, spawn Minion
                    match spawn_minion(repo_spec, &host, issue_number).await {
                        Ok(child) => {
                            children.push(child);
                            println!(
                                "✨ Spawned Minion for {}/issues/{}",
                                repo_spec, issue_number
                            );
                            spawned += 1;
                            available -= 1; // Decrement available slots after successful spawn
                        }
                        Err(e) => {
                            log::warn!(
                                "⚠️  Failed to spawn Minion for {}/issues/{}: {}",
                                repo_spec,
                                issue_number,
                                e
                            );
                            // Unclaim the issue since we failed to spawn
                            if let Err(e) = client
                                .remove_label(&owner, &repo, issue_number, labels::IN_PROGRESS)
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
                        issue_number,
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
    with_registry(|registry| {
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

        Ok(())
    })
    .await
}

/// Calculate available slots based on PID liveness of registered Minions
async fn available_slots(max_slots: usize) -> Result<usize> {
    let active_count = with_registry(move |registry| {
        let active = registry
            .list()
            .iter()
            .filter(|(_id, info)| info.pid.is_some_and(is_process_alive))
            .count();
        Ok(active)
    })
    .await?;

    Ok(max_slots.saturating_sub(active_count))
}

/// Check if an issue is already being worked on by a live Minion process
async fn is_issue_claimed(repo: &str, issue_number: u64) -> Result<bool> {
    let repo = repo.to_string();
    with_registry(move |registry| {
        let claimed = registry.list().iter().any(|(_id, info)| {
            info.repo == repo
                && info.issue == issue_number
                && info.pid.is_some_and(is_process_alive)
        });
        Ok(claimed)
    })
    .await
}

/// Spawn a Minion to work on an issue using the `gru do` command.
/// Returns the child process handle for lifecycle tracking.
async fn spawn_minion(repo: &str, host: &str, issue_number: u64) -> Result<Child> {
    let issue_ref = crate::github::build_issue_url_with_host(repo, host, issue_number)
        .with_context(|| format!("Invalid repo format: '{}'", repo))?;

    // Get the current executable path
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    // Create log directory and open log file for this minion's output
    let home = dirs::home_dir().context("Failed to determine home directory")?;
    let log_dir = home.join(".gru").join("state").join("logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    // Include host in log filename to avoid collisions when the same owner/repo
    // exists on different hosts (e.g., github.com/org/svc vs ghe.corp.com/org/svc).
    let safe_host = if host == "github.com" {
        String::new()
    } else {
        format!("{}-", host.replace('.', "-"))
    };
    let safe_repo = repo.replace('/', "-");
    let log_path = log_dir.join(format!(
        "{}{}-issue-{}.log",
        safe_host, safe_repo, issue_number
    ));
    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;
    let stderr_file = stdout_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    // Spawn `gru do <issue>` as a background process with output captured to log file
    let mut child = tokio::process::Command::new(exe)
        .arg("do")
        .arg(&issue_ref)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("Failed to spawn gru do command")?;

    // Give the process a moment to fail if there are startup issues
    // This prevents phantom slot occupancy from processes that immediately fail
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!(
            "Spawned process for {} exited immediately with status: {:?}",
            issue_ref,
            status
        );
    }

    println!("📝 Log: {}", log_path.display());

    Ok(child)
}

/// Fallback issue listing using octocrab API with client-side filtering.
/// Used when the gh CLI is unavailable. Cannot filter GitHub-native blocked state
/// (no API equivalent of `-is:blocked`), but does filter out `gru:blocked` and
/// `gru:in-progress` labels (accepting both old and new names).
///
/// If the configured label has a counterpart (old↔new), both are queried so that
/// repos in any migration state are covered.
async fn fallback_list_issues(
    owner: &str,
    repo: &str,
    host: &str,
    label: &str,
) -> Result<Vec<u64>> {
    let client = GitHubClient::from_env_with_host(owner, repo, host).await?;
    let mut issues = client.list_issues_with_label(owner, repo, label).await?;

    // Also fetch issues under the counterpart label name (old↔new) for backward compat
    if let Some(alt_label) = labels::counterpart_label(label) {
        if let Ok(alt_issues) = client.list_issues_with_label(owner, repo, alt_label).await {
            let existing: std::collections::HashSet<u64> =
                issues.iter().map(|i| i.number).collect();
            for issue in alt_issues {
                if !existing.contains(&issue.number) {
                    issues.push(issue);
                }
            }
        }
    }

    let filtered: Vec<u64> = issues
        .into_iter()
        .filter(|issue| {
            let label_names: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
            !labels::has_label(&label_names, labels::BLOCKED)
                && !labels::has_label(&label_names, labels::IN_PROGRESS)
        })
        .map(|issue| issue.number)
        .collect();

    Ok(filtered)
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
    fn test_log_path_github_com() {
        let safe_repo = "owner/repo".replace('/', "-");
        let log_name = format!("{}-issue-{}.log", safe_repo, 42);
        assert_eq!(log_name, "owner-repo-issue-42.log");
    }

    #[test]
    fn test_log_path_ghe_includes_host() {
        let host = "ghe.netflix.net";
        let safe_host = format!("{}-", host.replace('.', "-"));
        let safe_repo = "corp/service".replace('/', "-");
        let log_name = format!("{}{}-issue-{}.log", safe_host, safe_repo, 42);
        assert_eq!(log_name, "ghe-netflix-net-corp-service-issue-42.log");
    }

    // --- issue URL construction tests ---

    #[test]
    fn test_issue_ref_builds_full_github_url() {
        let url =
            crate::github::build_issue_url_with_host("fotoetienne/gru", "github.com", 42).unwrap();
        assert_eq!(url, "https://github.com/fotoetienne/gru/issues/42");
    }

    #[test]
    fn test_issue_ref_builds_ghe_url() {
        let url =
            crate::github::build_issue_url_with_host("corp/some-service", "ghe.netflix.net", 99)
                .unwrap();
        assert_eq!(url, "https://ghe.netflix.net/corp/some-service/issues/99");
    }

    #[test]
    fn test_build_issue_url_with_host_rejects_invalid() {
        assert!(crate::github::build_issue_url_with_host("", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("justrepo", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("/repo", "github.com", 1).is_none());
        assert!(crate::github::build_issue_url_with_host("owner/", "github.com", 1).is_none());
    }
}
