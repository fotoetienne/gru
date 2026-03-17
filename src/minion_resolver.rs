use crate::git;
use crate::github;
use crate::minion_registry::MinionRegistry;
use crate::workspace;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::PathBuf;

/// Regex for extracting issue links from PR bodies
static ISSUE_LINK_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:fixes|closes|resolves)\s+#(\d+)")
        .expect("Failed to compile issue link regex")
});

/// Information about a resolved Minion worktree
#[derive(Debug, Clone)]
pub struct MinionInfo {
    pub minion_id: String,
    pub issue_number: Option<u64>,
    #[allow(dead_code)]
    // Populated by resolver; callers currently only use minion_id/worktree_path
    pub repo_name: String,
    #[allow(dead_code)]
    // Populated by resolver; callers currently only use minion_id/worktree_path
    pub branch: String,
    /// The top-level minion directory (metadata lives here).
    /// Use `checkout_path()` to get the git worktree location.
    pub worktree_path: PathBuf,
    /// Used by find_by_issue_number_from_list to rank candidates.
    /// Values: "Active", "Stopped" (from registry), "Idle" (from filesystem scan).
    pub status: String,
    #[allow(dead_code)]
    // Populated by resolver; callers currently only use minion_id/worktree_path
    pub uptime: String,
}

impl MinionInfo {
    /// Returns the checkout path where the git worktree lives.
    ///
    /// New-style minions store the git worktree in `worktree_path/checkout/`.
    /// Legacy minions have the git worktree directly in `worktree_path/`.
    pub fn checkout_path(&self) -> PathBuf {
        crate::workspace::resolve_checkout_path(&self.worktree_path)
    }
}

/// Smart ID resolution that tries multiple strategies
///
/// Primary source: MinionRegistry (consistent with `gru status`)
/// Fallback source: Filesystem scan (for worktrees not in registry)
///
/// Resolution order:
/// 1. Try as exact minion ID (e.g., M0wy)
/// 2. Try with M prefix (e.g., 12 -> M12)
/// 3. Parse as number, search local minions by issue number
/// 4. Fallback to GitHub API for PRs (if online)
pub async fn resolve_minion(id: &str) -> Result<MinionInfo> {
    // Load minions from registry (primary source, consistent with gru status).
    // Returns empty vec on error — registry is best-effort, errors logged at debug level.
    let registry_minions = tokio::task::spawn_blocking(load_from_registry)
        .await
        .context("Failed to spawn blocking task for loading registry")?;

    // Try registry-based resolution first
    if let Some(info) = try_resolve_from_list(id, &registry_minions) {
        return Ok(info);
    }

    // Fallback: scan filesystem for worktrees not in registry
    let fs_minions = tokio::task::spawn_blocking(scan_all_minions)
        .await
        .context("Failed to spawn blocking task for scanning minions")??;

    if let Some(info) = try_resolve_from_list(id, &fs_minions) {
        return Ok(info);
    }

    // Last resort: try as PR number (single network call), search both sources
    if let Ok(num) = id.parse::<u64>() {
        if let Ok(issue_num) = resolve_issue_from_pr(num).await {
            if let Some(info) = find_by_issue_number_from_list(issue_num, &registry_minions) {
                return Ok(info);
            }
            if let Some(info) = find_by_issue_number_from_list(issue_num, &fs_minions) {
                return Ok(info);
            }
        }
    }

    anyhow::bail!(
        "Could not resolve ID '{}'. Tried:\n  \
         - Minion ID: {}\n  \
         - Minion ID: M{}\n  \
         - Issue/PR number: {}\n\n\
         Try 'gru status' to see active minions.",
        id,
        id,
        id,
        id
    )
}

/// Tries local resolution strategies against a list of minions (no network calls).
/// Returns Some(MinionInfo) if found, None otherwise.
fn try_resolve_from_list(id: &str, minions: &[MinionInfo]) -> Option<MinionInfo> {
    // Strategy 1: Try as exact minion ID
    if let Some(info) = find_by_minion_id_from_list(id, minions) {
        return Some(info);
    }

    // Strategy 2: Try with M prefix if not already present
    if !id.starts_with('M') {
        if let Some(info) = find_by_minion_id_from_list(&format!("M{}", id), minions) {
            return Some(info);
        }
    }

    // Strategy 3: Try as issue number
    if let Ok(num) = id.parse::<u64>() {
        if let Some(info) = find_by_issue_number_from_list(num, minions) {
            return Some(info);
        }
    }

    None
}

/// Loads minions from the MinionRegistry and converts them to MinionInfo.
/// Returns an empty vec on any error (registry is best-effort).
fn load_from_registry() -> Vec<MinionInfo> {
    let registry = match MinionRegistry::load(None) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("Failed to load MinionRegistry: {}", e);
            return Vec::new();
        }
    };

    // Don't filter by worktree.exists() here — callers (attach, stop, etc.) already
    // handle missing worktrees gracefully, and filtering would silently hide entries
    // that gru status shows.
    registry
        .list()
        .into_iter()
        .map(|(minion_id, info)| {
            let issue_number = if info.issue > 0 {
                Some(info.issue)
            } else {
                None
            };

            let uptime = {
                let now = chrono::Utc::now();
                let duration = now.signed_duration_since(info.started_at);
                // Guard against clock skew (started_at in the future)
                let minutes = duration.num_minutes().max(0);
                let hours = duration.num_hours().max(0);
                let days = duration.num_days().max(0);
                if days > 0 {
                    format!("{}d", days)
                } else if hours > 0 {
                    format!("{}h", hours)
                } else if minutes > 0 {
                    format!("{}m", minutes)
                } else {
                    "< 1m".to_string()
                }
            };

            let status = if info.is_running() {
                "Active".to_string()
            } else {
                "Stopped".to_string()
            };

            MinionInfo {
                minion_id,
                issue_number,
                repo_name: info.repo,
                branch: info.branch,
                worktree_path: info.worktree,
                status,
                uptime,
            }
        })
        .collect()
}

/// Find a minion by exact minion ID from a pre-scanned list
fn find_by_minion_id_from_list(minion_id: &str, minions: &[MinionInfo]) -> Option<MinionInfo> {
    minions.iter().find(|m| m.minion_id == minion_id).cloned()
}

/// Parse the numeric counter from a minion ID (e.g., "M0j8" -> Some(656)).
/// Returns None if the ID doesn't start with 'M' or contains invalid base36.
fn parse_minion_counter(id: &str) -> Option<u64> {
    let digits = id.strip_prefix('M').or_else(|| id.strip_prefix('m'))?;
    if digits.is_empty() {
        return None;
    }
    let mut result: u64 = 0;
    for c in digits.chars() {
        let digit = match c {
            '0'..='9' => (c as u64) - ('0' as u64),
            'a'..='z' => (c as u64) - ('a' as u64) + 10,
            'A'..='Z' => (c as u64) - ('A' as u64) + 10, // legacy uppercase
            _ => return None,
        };
        result = result.checked_mul(36)?.checked_add(digit)?;
    }
    Some(result)
}

/// Find a minion by issue number from a pre-scanned list.
/// When multiple minions match the same issue, prefers:
/// 1. Active/running minions over stopped ones
/// 2. Higher (more recent) minion ID as tiebreaker (compared numerically)
fn find_by_issue_number_from_list(issue_num: u64, minions: &[MinionInfo]) -> Option<MinionInfo> {
    minions
        .iter()
        .filter(|m| m.issue_number == Some(issue_num))
        .max_by(|a, b| {
            let a_active = a.status == "Active";
            let b_active = b.status == "Active";
            a_active.cmp(&b_active).then_with(|| {
                let a_counter = parse_minion_counter(&a.minion_id);
                let b_counter = parse_minion_counter(&b.minion_id);
                a_counter.cmp(&b_counter)
            })
        })
        .cloned()
}

/// Scans all minion worktrees and returns MinionInfo structs
pub fn scan_all_minions() -> Result<Vec<MinionInfo>> {
    let workspace = workspace::Workspace::new().context("Failed to initialize workspace")?;
    let work_path = workspace.work();

    if !work_path.exists() {
        return Ok(Vec::new());
    }

    let mut minions = Vec::new();

    // Iterate over owner directories
    for owner_entry in std::fs::read_dir(work_path)? {
        let owner_entry = owner_entry?;
        if !owner_entry.path().is_dir() {
            continue;
        }

        // Iterate over repo directories
        for repo_entry in std::fs::read_dir(owner_entry.path())? {
            let repo_entry = repo_entry?;
            if !repo_entry.path().is_dir() {
                continue;
            }

            // Iterate over minion directories (should start with 'M')
            for minion_entry in std::fs::read_dir(repo_entry.path())? {
                let minion_entry = minion_entry?;
                let minion_path = minion_entry.path();

                if !minion_path.is_dir() {
                    continue;
                }

                let minion_id = minion_entry.file_name().to_string_lossy().to_string();

                // Validate minion ID against path traversal and invalid characters
                if minion_id.contains('/') || minion_id.contains('\\') || minion_id.contains("..") {
                    continue; // Skip invalid directory names
                }

                if minion_id.len() < 2
                    || !minion_id.starts_with('M')
                    || !minion_id.chars().all(|c| c.is_alphanumeric())
                {
                    continue; // Skip non-minion directories
                }

                // Security check: verify the path stays within the work directory
                if !minion_path.starts_with(work_path) {
                    continue; // Skip paths that escape the work directory
                }

                // Check if this is a valid git worktree (new or legacy layout)
                let checkout_path = {
                    let checkout_subdir = minion_path.join("checkout");
                    if checkout_subdir.join(".git").exists() {
                        checkout_subdir
                    } else if minion_path.join(".git").exists() {
                        minion_path.clone()
                    } else {
                        continue; // Neither layout found
                    }
                };

                // Get the branch name from git
                let branch_output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout_path)
                    .arg("branch")
                    .arg("--show-current")
                    .output()?;

                let branch = String::from_utf8_lossy(&branch_output.stdout)
                    .trim()
                    .to_string();

                // Parse issue number from branch name (format: minion/issue-<num>-<id>)
                let issue_number = parse_issue_from_branch(&branch);

                // Determine status (Active or Idle) based on git index modification time
                let status = determine_status(&checkout_path)?;

                // Calculate uptime from worktree creation time
                let uptime = calculate_uptime(&minion_path)?;

                // Build repo name from path components
                let owner = owner_entry.file_name().to_string_lossy().to_string();
                let repo = repo_entry.file_name().to_string_lossy().to_string();
                let repo_name = format!("{}/{}", owner, repo);

                minions.push(MinionInfo {
                    minion_id,
                    issue_number,
                    repo_name,
                    branch,
                    worktree_path: minion_path,
                    status,
                    uptime,
                });
            }
        }
    }

    // Sort by minion ID
    minions.sort_by(|a, b| a.minion_id.cmp(&b.minion_id));

    Ok(minions)
}

/// Parses the issue number from a branch name
/// Expected format: minion/issue-<num>-<id>
fn parse_issue_from_branch(branch: &str) -> Option<u64> {
    if let Some(issue_part) = branch.strip_prefix("minion/issue-") {
        // Extract the number before the next hyphen
        if let Some(pos) = issue_part.find('-') {
            if let Ok(num) = issue_part[..pos].parse::<u64>() {
                return Some(num);
            }
        }
    }
    None
}

/// Determines if a Minion is Active or Idle based on git index modification time
/// A Minion is considered Active if the git index was modified in the last 5 minutes
fn determine_status(worktree_path: &std::path::Path) -> Result<String> {
    // Use git rev-parse to get the actual git directory path
    // In worktrees, .git is a file, not a directory
    let git_dir_output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg("--git-dir")
        .output()?;

    if !git_dir_output.status.success() {
        return Ok("Idle".to_string());
    }

    let git_dir = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();
    let git_index = std::path::PathBuf::from(git_dir).join("index");

    if !git_index.exists() {
        return Ok("Idle".to_string());
    }

    let metadata = std::fs::metadata(&git_index)?;
    let modified = metadata.modified()?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(modified).unwrap_or_default();

    // Consider active if modified within the last 5 minutes
    if elapsed.as_secs() < 300 {
        Ok("Active".to_string())
    } else {
        Ok("Idle".to_string())
    }
}

/// Calculates the uptime of a worktree based on its creation time
fn calculate_uptime(worktree_path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(worktree_path)?;
    let created = metadata.created().or_else(|_| metadata.modified())?;
    let now = std::time::SystemTime::now();
    let elapsed = now.duration_since(created).unwrap_or_default();

    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        Ok(format!("{}d", days))
    } else if hours > 0 {
        Ok(format!("{}h", hours))
    } else if minutes > 0 {
        Ok(format!("{}m", minutes))
    } else {
        Ok("< 1m".to_string())
    }
}

/// Extracts the linked issue number from a GitHub PR
/// Returns the issue number if the PR contains "Fixes #<num>", "Closes #<num>", or "Resolves #<num>"
async fn resolve_issue_from_pr(pr_num: u64) -> Result<u64> {
    // Detect repo from current directory to pick gh vs ghe
    git::detect_git_repo()
        .await
        .context("Failed to detect git repository")?;
    let github_hosts = crate::config::load_host_registry().all_hosts();
    let remote_url = git::get_github_remote(&github_hosts)
        .await
        .context("Failed to get GitHub remote")?;
    let (host, det_owner, det_repo) = git::parse_github_remote(&remote_url, &github_hosts)
        .context("Failed to parse GitHub remote URL")?;
    let repo_full = format!("{}/{}", det_owner, det_repo);
    // Use gh CLI to get linked issue from PR body
    let output = github::gh_cli_command(&host)
        .args([
            "pr",
            "view",
            &pr_num.to_string(),
            "--repo",
            &repo_full,
            "--json",
            "body",
        ])
        .output()
        .await
        .context("Failed to execute gh command. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR #{}: {}", pr_num, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh output as JSON")?;

    let body = json["body"]
        .as_str()
        .context("PR body field is not a string")?;

    // Look for "Fixes #<issue>" or "Closes #<issue>" in the PR body
    if let Some(captures) = ISSUE_LINK_REGEX.captures(body) {
        let issue_num = captures[1]
            .parse::<u64>()
            .context("Failed to parse issue number from PR body")?;

        return Ok(issue_num);
    }

    anyhow::bail!(
        "No linked issue found for PR #{}. PR must contain 'Fixes #<issue>' in its description.",
        pr_num
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a MinionInfo for testing
    fn test_minion(id: &str, issue: Option<u64>, repo: &str) -> MinionInfo {
        test_minion_with_status(id, issue, repo, "Stopped")
    }

    /// Helper to create a MinionInfo with a specific status
    fn test_minion_with_status(
        id: &str,
        issue: Option<u64>,
        repo: &str,
        status: &str,
    ) -> MinionInfo {
        MinionInfo {
            minion_id: id.to_string(),
            issue_number: issue,
            repo_name: repo.to_string(),
            branch: format!("minion/issue-{}-{}", issue.unwrap_or(0), id),
            worktree_path: PathBuf::from(format!("/tmp/test/{}", id)),
            status: status.to_string(),
            uptime: "1m".to_string(),
        }
    }

    #[test]
    fn test_parse_issue_from_branch_valid() {
        assert_eq!(parse_issue_from_branch("minion/issue-42-M001"), Some(42));
        assert_eq!(parse_issue_from_branch("minion/issue-123-M999"), Some(123));
        assert_eq!(parse_issue_from_branch("minion/issue-1-M0tk"), Some(1));
    }

    #[test]
    fn test_parse_issue_from_branch_invalid() {
        assert_eq!(parse_issue_from_branch("main"), None);
        assert_eq!(parse_issue_from_branch("feature/branch"), None);
        assert_eq!(parse_issue_from_branch(""), None);
        assert_eq!(parse_issue_from_branch("minion/issue-"), None);
    }

    #[test]
    fn test_try_resolve_by_exact_minion_id() {
        let minions = vec![
            test_minion("M001", Some(42), "owner/repo"),
            test_minion("M002", Some(99), "owner/repo"),
        ];
        let result = try_resolve_from_list("M001", &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M001");
    }

    #[test]
    fn test_try_resolve_with_m_prefix() {
        let minions = vec![test_minion("M42", Some(10), "owner/repo")];
        let result = try_resolve_from_list("42", &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M42");
    }

    #[test]
    fn test_try_resolve_by_issue_number() {
        let minions = vec![
            test_minion("M001", Some(42), "owner/repo"),
            test_minion("M002", Some(99), "owner/repo"),
        ];
        let result = try_resolve_from_list("99", &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M002");
    }

    #[test]
    fn test_try_resolve_not_found() {
        let minions = vec![test_minion("M001", Some(42), "owner/repo")];
        let result = try_resolve_from_list("M999", &minions);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_resolve_empty_list() {
        let result = try_resolve_from_list("M001", &[]);
        assert!(result.is_none());
    }

    // --- ISSUE_LINK_REGEX tests ---

    #[test]
    fn test_issue_link_regex_fixes() {
        let caps = ISSUE_LINK_REGEX.captures("Fixes #42");
        assert!(caps.is_some());
        assert_eq!(&caps.unwrap()[1], "42");
    }

    #[test]
    fn test_issue_link_regex_closes() {
        let caps = ISSUE_LINK_REGEX.captures("Closes #123");
        assert!(caps.is_some());
        assert_eq!(&caps.unwrap()[1], "123");
    }

    #[test]
    fn test_issue_link_regex_resolves() {
        let caps = ISSUE_LINK_REGEX.captures("Resolves #7");
        assert!(caps.is_some());
        assert_eq!(&caps.unwrap()[1], "7");
    }

    #[test]
    fn test_issue_link_regex_case_insensitive() {
        let caps = ISSUE_LINK_REGEX.captures("fixes #99");
        assert!(caps.is_some());
        assert_eq!(&caps.unwrap()[1], "99");

        let caps = ISSUE_LINK_REGEX.captures("FIXES #99");
        assert!(caps.is_some());
    }

    #[test]
    fn test_issue_link_regex_in_body() {
        let body = "This PR implements the feature.\n\nFixes #42\n\nMore details here.";
        let caps = ISSUE_LINK_REGEX.captures(body);
        assert!(caps.is_some());
        assert_eq!(&caps.unwrap()[1], "42");
    }

    #[test]
    fn test_issue_link_regex_no_match() {
        assert!(ISSUE_LINK_REGEX
            .captures("No issue reference here")
            .is_none());
        assert!(ISSUE_LINK_REGEX.captures("See #42").is_none()); // "See" is not a closing keyword
        assert!(ISSUE_LINK_REGEX.captures("Fixes without number").is_none());
    }

    #[test]
    fn test_issue_link_regex_no_space_does_not_match() {
        // \s+ requires at least one space between keyword and #
        assert!(ISSUE_LINK_REGEX.captures("Fixes#42").is_none());
        assert!(ISSUE_LINK_REGEX.captures("Closes#123").is_none());
    }

    // --- find_by_minion_id_from_list tests ---

    #[test]
    fn test_find_by_minion_id_found() {
        let minions = vec![
            test_minion("M001", Some(42), "owner/repo"),
            test_minion("M002", Some(99), "owner/repo"),
        ];
        let result = find_by_minion_id_from_list("M002", &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M002");
    }

    #[test]
    fn test_find_by_minion_id_not_found() {
        let minions = vec![test_minion("M001", Some(42), "owner/repo")];
        assert!(find_by_minion_id_from_list("M999", &minions).is_none());
    }

    #[test]
    fn test_find_by_minion_id_empty_list() {
        assert!(find_by_minion_id_from_list("M001", &[]).is_none());
    }

    // --- find_by_issue_number_from_list tests ---

    #[test]
    fn test_find_by_issue_number_found() {
        let minions = vec![
            test_minion("M001", Some(42), "owner/repo"),
            test_minion("M002", Some(99), "owner/repo"),
        ];
        let result = find_by_issue_number_from_list(99, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M002");
    }

    #[test]
    fn test_find_by_issue_number_not_found() {
        let minions = vec![test_minion("M001", Some(42), "owner/repo")];
        assert!(find_by_issue_number_from_list(999, &minions).is_none());
    }

    #[test]
    fn test_find_by_issue_number_none_issue() {
        let minions = vec![test_minion("M001", None, "owner/repo")];
        assert!(find_by_issue_number_from_list(42, &minions).is_none());
    }

    // --- parse_issue_from_branch edge cases ---

    #[test]
    fn test_parse_issue_from_branch_large_number() {
        assert_eq!(
            parse_issue_from_branch("minion/issue-999999-M0ab"),
            Some(999999)
        );
    }

    #[test]
    fn test_parse_issue_from_branch_no_minion_id() {
        // Missing the trailing ID part after number-hyphen
        assert_eq!(parse_issue_from_branch("minion/issue-42"), None);
    }

    // --- Multi-minion issue resolution tests ---

    #[test]
    fn test_find_by_issue_prefers_active_over_stopped() {
        let minions = vec![
            test_minion_with_status("M0j7", Some(353), "owner/repo", "Stopped"),
            test_minion_with_status("M0j8", Some(353), "owner/repo", "Active"),
        ];
        let result = find_by_issue_number_from_list(353, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M0j8");
    }

    #[test]
    fn test_find_by_issue_prefers_active_regardless_of_order() {
        // Active minion listed first, stopped second
        let minions = vec![
            test_minion_with_status("M0j8", Some(353), "owner/repo", "Active"),
            test_minion_with_status("M0j7", Some(353), "owner/repo", "Stopped"),
        ];
        let result = find_by_issue_number_from_list(353, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M0j8");
    }

    #[test]
    fn test_find_by_issue_tiebreaks_by_most_recent_id() {
        // Both stopped — should pick M0j8 (higher/more recent ID)
        let minions = vec![
            test_minion_with_status("M0j7", Some(353), "owner/repo", "Stopped"),
            test_minion_with_status("M0j8", Some(353), "owner/repo", "Stopped"),
        ];
        let result = find_by_issue_number_from_list(353, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M0j8");
    }

    #[test]
    fn test_find_by_issue_tiebreaks_active_by_most_recent_id() {
        // Both active — should pick higher ID
        let minions = vec![
            test_minion_with_status("M0j7", Some(353), "owner/repo", "Active"),
            test_minion_with_status("M0j8", Some(353), "owner/repo", "Active"),
        ];
        let result = find_by_issue_number_from_list(353, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M0j8");
    }

    #[test]
    fn test_find_by_issue_prefers_active_over_idle() {
        // Filesystem scan produces "Idle" instead of "Stopped"
        let minions = vec![
            test_minion_with_status("M0j7", Some(353), "owner/repo", "Idle"),
            test_minion_with_status("M0j8", Some(353), "owner/repo", "Active"),
        ];
        let result = find_by_issue_number_from_list(353, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M0j8");
    }

    #[test]
    fn test_find_by_issue_tiebreaks_across_id_widths() {
        // M0zz (counter 1295) vs M100 (counter 1296) — different base36 widths
        // Lexicographic would incorrectly pick M0zz; numeric comparison picks M100
        let minions = vec![
            test_minion_with_status("M0zz", Some(42), "owner/repo", "Stopped"),
            test_minion_with_status("M100", Some(42), "owner/repo", "Stopped"),
        ];
        let result = find_by_issue_number_from_list(42, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M100");
    }

    #[test]
    fn test_parse_minion_counter() {
        assert_eq!(parse_minion_counter("M000"), Some(0));
        assert_eq!(parse_minion_counter("M001"), Some(1));
        assert_eq!(parse_minion_counter("M00a"), Some(10));
        assert_eq!(parse_minion_counter("M0zz"), Some(1295));
        assert_eq!(parse_minion_counter("M100"), Some(1296));
        // Legacy uppercase
        assert_eq!(parse_minion_counter("M00A"), Some(10));
        // Invalid
        assert_eq!(parse_minion_counter(""), None);
        assert_eq!(parse_minion_counter("X001"), None);
    }

    #[test]
    fn test_find_by_issue_single_match_still_works() {
        let minions = vec![
            test_minion("M001", Some(42), "owner/repo"),
            test_minion("M002", Some(99), "owner/repo"),
        ];
        let result = find_by_issue_number_from_list(42, &minions);
        assert!(result.is_some());
        assert_eq!(result.unwrap().minion_id, "M001");
    }
}
