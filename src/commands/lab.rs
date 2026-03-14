use crate::config::{parse_repo_entry, LabConfig};
use crate::github::{list_ready_issues_via_cli, GitHubClient};
use crate::labels;
use crate::minion_registry::{
    is_process_alive, with_registry, MinionInfo, MinionMode, OrchestrationPhase,
};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Child;
use tokio::time::sleep;

/// Maximum number of attempts before a minion is marked as failed
const MAX_RESUME_ATTEMPTS: u32 = 3;

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

/// Information about a resumable minion found in the registry
struct ResumableMinion {
    minion_id: String,
    info: MinionInfo,
}

/// Build the set of `owner/repo` strings that this Lab instance monitors.
fn configured_repos(config: &LabConfig) -> HashSet<String> {
    config
        .daemon
        .repos
        .iter()
        .filter_map(|spec| {
            let (_host, owner, repo) = parse_repo_entry(spec)?;
            Some(format!("{}/{}", owner, repo))
        })
        .collect()
}

/// Resolve the host for a given `owner/repo` from the Lab config.
/// Returns `None` if the repo is not in the config.
fn host_for_repo(config: &LabConfig, owner_repo: &str) -> Option<String> {
    for spec in &config.daemon.repos {
        if let Some((host, owner, repo)) = parse_repo_entry(spec) {
            if format!("{}/{}", owner, repo) == owner_repo {
                return Some(host);
            }
        }
    }
    None
}

/// Scan the registry for minions that can be resumed.
///
/// A minion is resumable if:
/// - Its process is not running (mode is Stopped, or mode is Autonomous/Interactive
///   but the PID is dead — e.g. after SIGKILL before cleanup could run)
/// - Its orchestration phase is active (RunningAgent, CreatingPr, or MonitoringPr)
/// - Its worktree still exists on disk
/// - Its repo is in the Lab config
async fn find_resumable_minions(config: &LabConfig) -> Result<Vec<ResumableMinion>> {
    let repos = configured_repos(config);
    with_registry(move |registry| {
        let resumable = registry
            .list()
            .into_iter()
            .filter(|(_id, info)| {
                let process_dead =
                    info.mode == MinionMode::Stopped || !info.pid.is_some_and(is_process_alive);
                process_dead
                    && info.orchestration_phase.is_active()
                    && info.worktree.exists()
                    && repos.contains(&info.repo)
            })
            .map(|(minion_id, info)| ResumableMinion { minion_id, info })
            .collect();
        Ok(resumable)
    })
    .await
}

/// Mark a minion as failed in the registry and post an escalation comment on the issue.
async fn mark_exhausted_minion(minion_id: &str, info: &MinionInfo, host: &str) {
    let mid = minion_id.to_string();
    // Update registry to Failed
    if let Err(e) = with_registry(move |reg| {
        reg.update(&mid, |i| {
            i.orchestration_phase = OrchestrationPhase::Failed;
        })
    })
    .await
    {
        log::warn!(
            "Failed to mark minion {} as exhausted in registry: {}",
            minion_id,
            e
        );
    }

    // Post escalation comment and mark failed on GitHub
    let (owner, repo_name) = match info.repo.split_once('/') {
        Some(parts) => parts,
        None => return,
    };
    if let Ok(client) = GitHubClient::from_env_with_host(owner, repo_name, host).await {
        let comment = format!(
            "⚠️ **Minion {} has exceeded the maximum resume attempts ({}).**\n\n\
             Phase at failure: `{:?}`\n\n\
             This issue needs human attention.",
            minion_id, MAX_RESUME_ATTEMPTS, info.orchestration_phase,
        );
        let _ = client
            .post_comment(owner, repo_name, info.issue, &comment)
            .await;
        let _ = client.mark_issue_failed(owner, repo_name, info.issue).await;
    }
}

/// Resume interrupted minions, filling available slots before new issue claims.
///
/// Returns the number of minions successfully resumed.
async fn resume_interrupted_minions(
    config: &LabConfig,
    children: &mut Vec<Child>,
    available: &mut usize,
) -> Result<usize> {
    if *available == 0 {
        return Ok(0);
    }

    let resumable = find_resumable_minions(config).await?;
    if resumable.is_empty() {
        return Ok(0);
    }

    println!(
        "🔄 Found {} resumable Minion(s) from previous session",
        resumable.len()
    );

    let mut resumed = 0;

    for candidate in resumable {
        if *available == 0 {
            break;
        }

        let host = match host_for_repo(config, &candidate.info.repo) {
            Some(h) => h,
            None => continue, // repo no longer in config
        };

        // Skip minions that have exceeded max attempts
        if candidate.info.attempt_count >= MAX_RESUME_ATTEMPTS {
            println!(
                "⏭️  Skipping {} (issue #{}, {}): attempt_count {} >= max {}",
                candidate.minion_id,
                candidate.info.issue,
                candidate.info.repo,
                candidate.info.attempt_count,
                MAX_RESUME_ATTEMPTS,
            );
            mark_exhausted_minion(&candidate.minion_id, &candidate.info, &host).await;
            continue;
        }

        println!(
            "♻️  Resuming {} (issue #{}, {}, phase: {:?})",
            candidate.minion_id,
            candidate.info.issue,
            candidate.info.repo,
            candidate.info.orchestration_phase,
        );

        match spawn_minion(&candidate.info.repo, &host, candidate.info.issue).await {
            Ok(child) => {
                children.push(child);
                resumed += 1;
                *available -= 1;
            }
            Err(e) => {
                log::warn!(
                    "⚠️  Failed to resume {} for issue #{}: {}",
                    candidate.minion_id,
                    candidate.info.issue,
                    e
                );
            }
        }
    }

    if resumed > 0 {
        println!("✅ Resumed {} Minion(s)", resumed);
    }

    Ok(resumed)
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

    // Resume interrupted minions first, before claiming new issues
    let resumed = resume_interrupted_minions(config, children, &mut available).await?;
    let mut spawned = resumed;

    if available == 0 {
        if spawned > 0 {
            println!();
        }
        return Ok(());
    }

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
        // Canonical owner/repo form for registry lookups and issue URL building
        let repo_full = format!("{}/{}", owner, repo);

        // Fetch ready issues, excluding blocked ones (both GitHub-blocked and gru:blocked).
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
            if is_issue_claimed(&repo_full, issue_number).await? {
                continue;
            }

            // Try to claim the issue
            match client.claim_issue(&owner, &repo, issue_number).await {
                Ok(true) => {
                    // Successfully claimed, spawn Minion
                    match spawn_minion(&repo_full, &host, issue_number).await {
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
                            // Restore the ready label so the issue remains visible for retry
                            if let Err(e) = client
                                .add_label(&owner, &repo, issue_number, &config.daemon.label)
                                .await
                            {
                                log::warn!(
                                    "⚠️  Failed to restore ready label '{}': {}",
                                    config.daemon.label,
                                    e
                                );
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
    // Sanitize by replacing any non-alphanumeric characters with hyphens.
    let safe_host = if host == "github.com" {
        String::new()
    } else {
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{}-", sanitized)
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
/// `gru:in-progress` labels.
async fn fallback_list_issues(
    owner: &str,
    repo: &str,
    host: &str,
    label: &str,
) -> Result<Vec<u64>> {
    let client = GitHubClient::from_env_with_host(owner, repo, host).await?;
    let issues = client.list_issues_with_label(owner, repo, label).await?;

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
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let safe_host = format!("{}-", sanitized);
        let safe_repo = "corp/service".replace('/', "-");
        let log_name = format!("{}{}-issue-{}.log", safe_host, safe_repo, 42);
        assert_eq!(log_name, "ghe-netflix-net-corp-service-issue-42.log");
    }

    #[test]
    fn test_log_path_host_with_port_is_sanitized() {
        let host = "ghe.example.com:8443";
        let sanitized: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        assert_eq!(sanitized, "ghe-example-com-8443");
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

    // --- configured_repos / host_for_repo tests ---

    fn test_lab_config(repos: Vec<&str>) -> LabConfig {
        let mut config = LabConfig::default();
        config.daemon.repos = repos.into_iter().map(String::from).collect();
        config.daemon.max_slots = 2;
        config
    }

    #[test]
    fn test_configured_repos_basic() {
        let config = test_lab_config(vec!["owner/repo1", "owner/repo2"]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains("owner/repo1"));
        assert!(repos.contains("owner/repo2"));
    }

    #[test]
    fn test_configured_repos_with_host() {
        let config = test_lab_config(vec!["ghe.corp.com/owner/repo1", "owner/repo2"]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains("owner/repo1"));
        assert!(repos.contains("owner/repo2"));
    }

    #[test]
    fn test_configured_repos_skips_invalid() {
        let config = test_lab_config(vec!["owner/repo1", "invalid", ""]);
        let repos = configured_repos(&config);
        assert_eq!(repos.len(), 1);
        assert!(repos.contains("owner/repo1"));
    }

    #[test]
    fn test_host_for_repo_github_com() {
        let config = test_lab_config(vec!["owner/repo1"]);
        assert_eq!(
            host_for_repo(&config, "owner/repo1"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_host_for_repo_ghe() {
        let config = test_lab_config(vec!["ghe.corp.com/owner/repo1"]);
        assert_eq!(
            host_for_repo(&config, "owner/repo1"),
            Some("ghe.corp.com".to_string())
        );
    }

    #[test]
    fn test_host_for_repo_not_found() {
        let config = test_lab_config(vec!["owner/repo1"]);
        assert_eq!(host_for_repo(&config, "other/repo"), None);
    }

    #[test]
    fn test_max_resume_attempts_constant() {
        assert_eq!(MAX_RESUME_ATTEMPTS, 3);
    }
}
