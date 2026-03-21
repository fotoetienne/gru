use crate::agent::AgentEvent;
use crate::agent_registry;
use crate::agent_runner::{run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED};
use crate::git;
use crate::github;
use crate::minion;
use crate::minion_registry::{
    with_registry, MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry, OrchestrationPhase,
};
use crate::minion_resolver;
use crate::pr_state::PrState;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::prompt_loader;
use crate::prompt_renderer::{render_template, PromptContext};
use crate::tmux::TmuxGuard;
use crate::url_utils::parse_pr_info;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use std::env;
use std::path::Path;
use uuid::Uuid;

/// Handles the review command by setting up workspace and spawning autonomous Claude agent with stream parsing
/// Returns the exit code from the claude process
pub(crate) async fn handle_review(pr_arg: Option<String>, agent_name: &str) -> Result<i32> {
    // Validate agent name early, before any side effects (registry, worktree)
    let backend = agent_registry::resolve_backend(agent_name)?;

    // Resolve PR information from various input formats
    let (owner, repo, pr_num, branch, host) = match pr_arg {
        None => resolve_pr_from_current_worktree().await?,
        Some(arg) => resolve_pr_from_arg(&arg).await?,
    };

    // Rename tmux window for the review
    let _tmux_guard = TmuxGuard::new(&format!("gru:review:#{}", pr_num));

    println!(
        "🔍 Setting up workspace for {}/{}#{} (branch: {})",
        owner, repo, pr_num, branch
    );

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Generate minion ID for registry tracking
    let minion_id =
        minion::generate_minion_id().context("Failed to generate Minion ID for review")?;

    // Create bare repository path
    let bare_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(&owner, &repo, &host, bare_path);

    // Ensure bare repository is cloned/updated
    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .await
        .with_context(|| format!("Failed to clone or update repository for PR {}", pr_num))?;

    // Check if a worktree already exists for this branch
    let (minion_dir, checkout_path) = if let Some(existing_path) = git_repo
        .find_worktree_for_branch(&branch)
        .await
        .context("Failed to check for existing worktree")?
    {
        println!(
            "♻️  Reusing existing worktree at: {}",
            existing_path.display()
        );
        // For existing worktrees, detect the layout (new vs legacy)
        // If checkout_path == existing_path, it's legacy (minion_dir is the same)
        // If existing_path ends in "checkout", the parent is minion_dir
        let minion_dir = if existing_path
            .file_name()
            .map(|n| n == "checkout")
            .unwrap_or(false)
        {
            existing_path
                .parent()
                .unwrap_or(&existing_path)
                .to_path_buf()
        } else {
            existing_path.clone()
        };
        (minion_dir, existing_path)
    } else {
        // No existing worktree, fetch the branch and create one
        println!("🔄 Fetching PR branch: {}", branch);
        git_repo
            .fetch_branch(&branch)
            .await
            .with_context(|| format!("Failed to fetch PR branch '{}'", branch))?;

        let repo_name = github::repo_slug(&owner, &repo);
        let minion_dir = workspace
            .work_dir(&repo_name, &minion_id)
            .context("Failed to compute minion directory path")?;

        let checkout_path = minion_dir.join("checkout");

        // Ensure the minion directory exists
        tokio::fs::create_dir_all(&minion_dir)
            .await
            .context("Failed to create minion directory")?;

        println!("🌿 Creating worktree for branch: {}", branch);
        git_repo
            .checkout_worktree(&branch, &checkout_path)
            .await
            .with_context(|| format!("Failed to checkout worktree for PR {}", pr_num))?;

        (minion_dir, checkout_path)
    };

    // Fetch PR details for prompt rendering
    let pr_num_u64: u64 = pr_num
        .parse()
        .with_context(|| format!("Invalid PR number: '{}'", pr_num))?;
    let pr_details = match fetch_pr_details(&owner, &repo, &host, pr_num_u64).await {
        Ok(details) => Some(details),
        Err(e) => {
            log::warn!("Failed to fetch PR details: {e}. Using fallback prompt.");
            None
        }
    };

    // Fetch the issue number linked to this PR (if any)
    let linked_issue = find_issue_for_pr(&owner, &repo, &host, &pr_num)
        .await
        .unwrap_or_else(|e| {
            log::warn!(
                "Warning: Failed to fetch linked issue for PR #{}: {}",
                pr_num,
                e
            );
            0
        });

    // Generate a unique session ID for conversation continuity
    let session_id = Uuid::new_v4();

    // Build the review prompt using the template system
    let review_prompt = build_review_prompt(
        &owner,
        &repo,
        &pr_num,
        pr_details.as_ref(),
        &checkout_path,
        &minion_id,
    );

    // Register minion in registry (worktree field stores the minion_dir)
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: github::repo_slug(&owner, &repo),
        issue: linked_issue,
        command: "review".to_string(),
        prompt: review_prompt.chars().take(200).collect(),
        started_at: now,
        branch: branch.clone(),
        worktree: minion_dir.clone(),
        status: "active".to_string(),
        pr: Some(pr_num.clone()),
        session_id: session_id.to_string(),
        pid: None,
        pid_start_time: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
        orchestration_phase: OrchestrationPhase::RunningAgent,
        token_usage: None,
        agent_name: agent_name.to_string(),
        timeout_deadline: None,
        attempt_count: 0,
        no_watch: false,
        last_review_check_time: None,
        wake_reason: None,
        archived_at: None,
    };

    // Register the Minion (spawn_blocking to avoid holding lock during review)
    let minion_id_clone = minion_id.clone();
    with_registry(move |registry| registry.register(minion_id_clone, registry_info)).await?;

    println!("🤖 Launching autonomous review agent...\n");

    // Create progress display for review
    // If there's no linked issue (linked_issue == 0), display the PR number instead
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: if linked_issue == 0 {
            format!("PR {}", pr_num)
        } else {
            linked_issue.to_string()
        },
        quiet: false,
    };
    let progress = std::sync::Arc::new(ProgressDisplay::new(config));

    // Build the command with flags for autonomous stream-json output
    let cmd = backend.build_command(&checkout_path, &session_id, &review_prompt, &host);

    // Record child PID on spawn; mode is already set to Autonomous at registration.
    let on_spawn = MinionRegistry::pid_callback(minion_id.clone(), None);

    // Run agent with stream monitoring (no timeout for reviews)
    let progress_cb = std::sync::Arc::clone(&progress);
    let output_callback = move |event: &AgentEvent| {
        progress_cb.handle_event(event);
    };

    let run_result = run_agent_with_stream_monitoring(
        cmd,
        &*backend,
        &minion_dir,
        None,
        Some(output_callback),
        Some(on_spawn),
    )
    .await;

    // Best-effort cleanup: clear PID, set mode to Stopped, save token usage, and
    // mark orchestration phase as terminal. Review is ephemeral (does not support
    // resume), so we always move to Completed/Failed to prevent the lab daemon from
    // attempting to resume a finished review.
    let exit_ok = run_result.as_ref().is_ok_and(|r| r.status.success());
    let token_usage = run_result.as_ref().ok().map(|r| r.token_usage.clone());
    let cleanup_id = minion_id.clone();
    let _ = with_registry(move |registry| {
        registry.update(&cleanup_id, |info| {
            info.clear_pid();
            info.mode = MinionMode::Stopped;
            info.orchestration_phase = if exit_ok {
                OrchestrationPhase::Completed
            } else {
                OrchestrationPhase::Failed
            };
            if let Some(usage) = token_usage {
                info.token_usage = Some(usage);
            }
        })
    })
    .await;

    // Now check if there was a stream error (after cleanup)
    let agent_run = run_result?;
    let status = agent_run.status;

    // Log token usage
    if agent_run.token_usage.total_tokens() > 0 {
        log::info!(
            "📊 Token usage: {}",
            agent_run.token_usage.display_compact()
        );
    }

    // Finish the progress display
    if status.success() {
        progress.finish_with_message(&format!("✅ Review complete for PR #{}", pr_num));
    } else {
        progress.finish_with_message(&format!("❌ Review failed for PR #{}", pr_num));
    }

    // Return the exit code from the agent process
    // Use 128 for signal terminations to follow shell conventions
    Ok(status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
}

/// Resolves PR information from the current worktree directory
/// Reads the .gru_pr_state.json file to get the PR number
async fn resolve_pr_from_current_worktree() -> Result<(String, String, String, String, String)> {
    // Detect current directory as git repository
    let current_dir = env::current_dir().context("Failed to get current directory")?;

    // Check if we're in a git repository
    git::detect_git_repo().await.context(
        "Not in a git repository. Run from a Minion worktree or provide a PR number/URL/Minion ID.",
    )?;

    // Try to load PR state from current directory
    let pr_state = PrState::load(&current_dir)
        .context("Failed to check for PR state file")?
        .context(
            "No PR state found in current directory. This doesn't appear to be a Minion worktree.\n\
             Try: gru review <pr-number> or gru review <minion-id>",
        )?;

    // Get PR info from the PR number
    get_pr_info_from_number(&pr_state.pr_number).await
}

/// Resolves PR information from a user-provided argument
/// Handles Minion IDs, issue numbers, PR numbers, and URLs
async fn resolve_pr_from_arg(arg: &str) -> Result<(String, String, String, String, String)> {
    let mut errors = Vec::new();

    // Strategy 1: Try as Minion ID (only if it looks like one — avoids
    // unnecessary registry/filesystem I/O for URLs and PR numbers)
    let looks_like_minion =
        arg.len() >= 2 && arg.starts_with('M') && arg[1..].chars().all(|c| c.is_alphanumeric());
    if looks_like_minion {
        match resolve_pr_from_minion_id(arg).await {
            Ok(pr_info) => return Ok(pr_info),
            Err(e) => errors.push(format!("Minion ID '{}': {:#}", arg, e)),
        }
    }

    // Strategy 2: Try as PR number or URL (existing behavior)
    let github_hosts = crate::config::load_host_registry().all_hosts();
    match parse_pr_info(arg, &github_hosts).await {
        Ok(pr_info) => return Ok(pr_info),
        Err(e) => errors.push(format!("PR number/URL '{}': {:#}", arg, e)),
    }

    // Strategy 3: Fallback - try as issue number
    if let Ok(issue_num) = arg.parse::<u64>() {
        match find_pr_for_issue(issue_num).await {
            Ok(pr_num) => match get_pr_info_from_number(&pr_num).await {
                Ok(pr_info) => return Ok(pr_info),
                Err(e) => errors.push(format!(
                    "Issue #{}: Found PR but failed to get info: {:#}",
                    issue_num, e
                )),
            },
            Err(e) => errors.push(format!("Issue #{}: {:#}", issue_num, e)),
        }
    }

    anyhow::bail!(
        "Could not resolve '{}' to a PR.\n\nAttempted strategies:\n{}",
        arg,
        errors
            .iter()
            .map(|e| format!("  • {}", e))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Resolves PR information from a Minion ID
async fn resolve_pr_from_minion_id(
    minion_id: &str,
) -> Result<(String, String, String, String, String)> {
    let minion = minion_resolver::resolve_minion(minion_id).await?;

    // Load PR state from the minion's worktree
    let pr_state = PrState::load(&minion.worktree_path)
        .context("Failed to check for PR state file in Minion worktree")?
        .context(format!(
            "Minion {} doesn't have a PR yet. The Minion may still be working on the issue.",
            minion_id
        ))?;

    // Get PR info from the PR number
    get_pr_info_from_number(&pr_state.pr_number).await
}

/// Fetches PR information (owner, repo, pr_num, branch) given a PR number
async fn get_pr_info_from_number(pr_num: &str) -> Result<(String, String, String, String, String)> {
    // Validate that pr_num is actually a number to provide better error messages
    pr_num
        .parse::<u64>()
        .with_context(|| format!("Invalid PR number format: '{}'", pr_num))?;

    // Use parse_pr_info which fetches metadata from GitHub
    let github_hosts = crate::config::load_host_registry().all_hosts();
    parse_pr_info(pr_num, &github_hosts).await
}

/// Finds a PR number associated with an issue number
/// Uses gh CLI to search for PRs that link to the issue
async fn find_pr_for_issue(issue_num: u64) -> Result<String> {
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
    let repo_full = github::repo_slug(&det_owner, &det_repo);
    // Safe: issue_num is validated as u64 by the type system, which can only contain digits.
    // This prevents command injection as the format string will never contain shell metacharacters.
    let output = github::gh_cli_command(&host)
        .args([
            "pr",
            "list",
            "--repo",
            &repo_full,
            "--search",
            &format!("linked:issue#{}", issue_num),
            "--json",
            "number",
            "--limit",
            "1",
        ])
        .output()
        .await
        .context("Failed to execute gh pr list. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to search for PRs linked to issue #{}: {}",
            issue_num,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse gh pr list output")?;

    // Check if we got any results
    let prs = json.as_array().context("Expected array from gh pr list")?;

    if prs.is_empty() {
        anyhow::bail!("No PR found linked to issue #{}", issue_num);
    }

    let pr_num = prs[0]["number"]
        .as_u64()
        .context("PR number is not a valid integer")?;

    Ok(pr_num.to_string())
}

/// Finds issue numbers linked to a PR
/// Uses gh CLI to fetch issues that this PR closes/fixes
/// Returns the first linked issue number, or 0 if no issues are linked
async fn find_issue_for_pr(owner: &str, repo: &str, host: &str, pr_num: &str) -> Result<u64> {
    let repo_full = github::repo_slug(owner, repo);
    // Safe: pr_num is already validated as a number earlier in the call chain
    let output = github::gh_cli_command(host)
        .args([
            "pr",
            "view",
            pr_num,
            "--repo",
            &repo_full,
            "--json",
            "closingIssuesReferences",
            "--jq",
            ".closingIssuesReferences[0].number",
        ])
        .output()
        .await
        .context("Failed to execute gh pr view. Is GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!(
            "Warning: Failed to fetch linked issues for PR #{}: {}",
            pr_num,
            stderr
        );
        return Ok(0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // If the output is "null" or empty, no issue is linked
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(0);
    }

    // Parse the issue number
    trimmed
        .parse::<u64>()
        .context("Failed to parse issue number from PR")
        .or_else(|e| {
            log::warn!("Warning: {}", e);
            Ok(0)
        })
}

/// Details about a PR used for prompt rendering
struct PrDetails {
    title: String,
    body: String,
}

/// Fetches PR title and body from GitHub for prompt rendering via gh CLI.
async fn fetch_pr_details(owner: &str, repo: &str, host: &str, pr_num: u64) -> Result<PrDetails> {
    let info = github::get_pr_via_cli(owner, repo, host, pr_num)
        .await
        .context("Failed to fetch PR details via gh CLI")?;
    Ok(PrDetails {
        title: info.title,
        body: info.body.unwrap_or_default(),
    })
}

/// Builds the review prompt using the prompt template system.
///
/// Loads the "review" prompt template (built-in or overridden via `.gru/prompts/review.md`),
/// builds a `PromptContext` from the PR details, and renders the template.
/// Falls back to `/pr_review <pr_num>` when PR details are unavailable or no prompt is found.
fn build_review_prompt(
    owner: &str,
    repo: &str,
    pr_num: &str,
    details: Option<&PrDetails>,
    worktree_path: &Path,
    minion_id: &str,
) -> String {
    let Some(details) = details else {
        return format!("/pr_review {}", pr_num);
    };

    let prompt_template = match prompt_loader::resolve_prompt("review", Some(worktree_path)) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Failed to load review prompt: {e}, using /pr_review fallback");
            None
        }
    };

    let template_content = match prompt_template {
        Some(ref p) => &p.content,
        None => {
            log::warn!(
                "No 'review' prompt found (built-in or override), using /pr_review fallback"
            );
            return format!("/pr_review {}", pr_num);
        }
    };

    let mut prompt_ctx = PromptContext::new();
    prompt_ctx.pr_number = pr_num.parse().ok();
    prompt_ctx.pr_title = Some(details.title.clone());
    prompt_ctx.pr_body = Some(details.body.clone());
    prompt_ctx.repo_owner = Some(owner.to_string());
    prompt_ctx.repo_name = Some(repo.to_string());
    prompt_ctx.worktree_path = Some(worktree_path.to_path_buf());
    prompt_ctx.minion_id = Some(minion_id.to_string());

    let variables = prompt_ctx.to_variables();
    render_template(template_content, &variables)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_review_prompt_with_details() {
        let tmp = tempfile::tempdir().unwrap();
        let details = PrDetails {
            title: "Fix the widget".to_string(),
            body: "This PR fixes the broken widget".to_string(),
        };

        let prompt = build_review_prompt(
            "octocat",
            "hello-world",
            "456",
            Some(&details),
            tmp.path(),
            "M042",
        );
        assert!(prompt.starts_with("# PR #456: Fix the widget"));
        assert!(prompt.contains("octocat/hello-world/pull/456"));
        assert!(prompt.contains("This PR fixes the broken widget"));
        assert!(prompt.contains("## 1. Fetch PR Details"));
    }

    #[test]
    fn test_build_review_prompt_without_details() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt = build_review_prompt("octocat", "hello-world", "456", None, tmp.path(), "M042");
        assert_eq!(prompt, "/pr_review 456");
    }

    #[test]
    fn test_build_review_prompt_uses_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let details = PrDetails {
            title: "Template test".to_string(),
            body: "Body content here".to_string(),
        };

        let prompt = build_review_prompt(
            "myorg",
            "myproject",
            "99",
            Some(&details),
            tmp.path(),
            "M042",
        );

        // Verify template variables were substituted
        assert!(!prompt.contains("{{ pr_number }}"));
        assert!(!prompt.contains("{{ pr_title }}"));
        assert!(!prompt.contains("{{ pr_body }}"));
        assert!(!prompt.contains("{{ repo_owner }}"));
        assert!(!prompt.contains("{{ repo_name }}"));

        // Verify the substituted values are present
        assert!(prompt.contains("99"));
        assert!(prompt.contains("Template test"));
        assert!(prompt.contains("Body content here"));
        assert!(prompt.contains("myorg"));
        assert!(prompt.contains("myproject"));
    }

    #[test]
    fn test_build_review_prompt_repo_override() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts_dir = tmp.path().join(".gru").join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        // Create a custom review prompt that overrides the built-in
        std::fs::write(
            prompts_dir.join("review.md"),
            r#"---
description: Custom review
requires: [pr]
---
CUSTOM: Review PR #{{ pr_number }} - {{ pr_title }}"#,
        )
        .unwrap();

        let details = PrDetails {
            title: "Custom test".to_string(),
            body: "Custom body".to_string(),
        };

        let prompt = build_review_prompt("owner", "repo", "55", Some(&details), tmp.path(), "M042");
        assert_eq!(prompt, "CUSTOM: Review PR #55 - Custom test");
    }
}
