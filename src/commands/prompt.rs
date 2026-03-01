use crate::claude_runner::{
    build_claude_command, run_claude_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::minion_registry::{
    MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry, OrchestrationPhase,
};
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::prompt_renderer::{render_template, PromptContext};
use crate::stream;
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Parses --param KEY=VALUE arguments into a HashMap
fn parse_params(params: &[String]) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for param in params {
        let (key, value) = param
            .split_once('=')
            .with_context(|| format!("Invalid --param format: '{}'. Expected KEY=VALUE", param))?;
        if key.is_empty() {
            anyhow::bail!("Invalid --param: key cannot be empty in '{}'", param);
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Fetches issue data from GitHub and populates a PromptContext
async fn fetch_issue_context(issue_str: &str) -> Result<(PromptContext, String, String, u64)> {
    let (owner, repo, issue_num_str) = parse_issue_info(issue_str).await?;
    let issue_number: u64 = issue_num_str
        .parse()
        .context("Failed to parse issue number")?;

    println!(
        "📋 Fetching issue #{} from {}/{}...",
        issue_number, owner, repo
    );

    let mut context = PromptContext::new();
    context.issue_number = Some(issue_number);
    context.repo_owner = Some(owner.clone());
    context.repo_name = Some(repo.clone());

    // Try API first, fall back to CLI
    if let Some(github_client) = GitHubClient::try_from_env(&owner, &repo).await {
        match github_client.get_issue(&owner, &repo, issue_number).await {
            Ok(issue) => {
                context.issue_title = Some(issue.title.clone());
                context.issue_body = Some(issue.body.unwrap_or_default());
            }
            Err(e) => {
                log::warn!(
                    "Failed to fetch issue via API: {}. Falling back to gh CLI...",
                    e
                );
                let info = crate::github::get_issue_via_cli(&owner, &repo, issue_number)
                    .await
                    .context("Failed to fetch issue via gh CLI")?;
                context.issue_title = Some(info.title);
                context.issue_body = Some(info.body.unwrap_or_default());
            }
        }
    } else {
        let info = crate::github::get_issue_via_cli(&owner, &repo, issue_number)
            .await
            .context(
                "Failed to fetch issue. Ensure gh is installed and authenticated, \
                 or set GRU_GITHUB_TOKEN.",
            )?;
        context.issue_title = Some(info.title);
        context.issue_body = Some(info.body.unwrap_or_default());
    }

    println!(
        "   Issue #{}: {}",
        issue_number,
        context.issue_title.as_deref().unwrap_or("(no title)")
    );

    Ok((context, owner, repo, issue_number))
}

/// Sets up a worktree for an issue, returning the worktree path and branch name
async fn setup_issue_worktree(
    owner: &str,
    repo: &str,
    issue_number: u64,
    minion_id: &str,
) -> Result<(PathBuf, String)> {
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create bare repository path
    let bare_path = workspace.repos().join(owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(owner, repo, bare_path);

    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .await
        .context("Failed to clone or update repository")?;

    let branch_name = format!("minion/issue-{}-{}", issue_number, minion_id);
    println!("🌿 Creating worktree with branch: {}", branch_name);

    let repo_name = format!("{}/{}", owner, repo);
    let worktree_path = workspace
        .work_dir(&repo_name, &branch_name)
        .context("Failed to compute worktree path")?;

    git_repo
        .create_worktree(&branch_name, &worktree_path)
        .await
        .context("Failed to create worktree")?;

    println!("📂 Worktree created at: {}", worktree_path.display());

    Ok((worktree_path, branch_name))
}

/// Handles the prompt command by launching Claude with an ad-hoc prompt
/// Returns the exit code from the claude process
pub async fn handle_prompt(
    prompt: &str,
    issue_opt: Option<String>,
    no_worktree: bool,
    params: Vec<String>,
    timeout_opt: Option<String>,
    quiet: bool,
) -> Result<i32> {
    // Validate prompt doesn't start with flags (security check)
    let trimmed_prompt = prompt.trim();
    if trimmed_prompt.starts_with('-') {
        anyhow::bail!(
            "Prompt cannot start with '-' (looks like a command flag). \
             Use quotes around your prompt: gru prompt \"your prompt here\""
        );
    }

    if trimmed_prompt.is_empty() {
        anyhow::bail!("Prompt cannot be empty");
    }

    // Parse custom parameters
    let custom_params = parse_params(&params)?;

    // Build prompt context from --issue flag and custom params
    let mut context = PromptContext::new();
    let mut issue_owner: Option<String> = None;
    let mut issue_repo: Option<String> = None;
    let mut issue_number_val: Option<u64> = None;

    if let Some(ref issue_str) = issue_opt {
        let (issue_ctx, owner, repo, issue_num) = fetch_issue_context(issue_str).await?;
        context = issue_ctx;
        issue_owner = Some(owner);
        issue_repo = Some(repo);
        issue_number_val = Some(issue_num);
    }

    // Apply custom params (these override standard variables)
    context.params = custom_params;

    // Generate a unique minion ID for session tracking
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("🆔 Session: {}", minion_id);

    // Generate a unique session ID (UUID) for Claude's --session-id flag.
    // Created early so the registry can record the actual session ID.
    let session_id = Uuid::new_v4();

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Set up worktree or ad-hoc workspace
    let (workspace_path, branch_name, run_dir) = if issue_opt.is_some() && !no_worktree {
        let owner = issue_owner.as_deref().unwrap();
        let repo = issue_repo.as_deref().unwrap();
        let issue_num = issue_number_val.unwrap();

        let (wt_path, branch) = setup_issue_worktree(owner, repo, issue_num, &minion_id).await?;
        context.worktree_path = Some(wt_path.clone());
        context.branch_name = Some(branch.clone());
        let run_dir = wt_path.clone();
        (wt_path, branch, run_dir)
    } else if no_worktree && issue_opt.is_some() {
        // --issue with --no-worktree: use the current directory as both the
        // workspace and the run directory so the registry matches reality.
        println!("ℹ️  Running without worktree - Claude will work in the current directory");
        let run_dir = std::env::current_dir().context("Failed to get current working directory")?;
        let wt_path = run_dir.clone();
        (wt_path, String::new(), run_dir)
    } else {
        // Ad-hoc workspace: ~/.gru/work/ad-hoc/<minion-id>/
        let wt_path = workspace
            .work_dir("ad-hoc", &minion_id)
            .context("Failed to compute workspace path")?;
        tokio::fs::create_dir_all(&wt_path)
            .await
            .context("Failed to create workspace directory")?;
        let run_dir = std::env::current_dir().context("Failed to get current working directory")?;
        (wt_path, String::new(), run_dir)
    };

    // Set cwd to the actual execution directory (after worktree decision)
    context.cwd = Some(run_dir.clone());

    // Render the prompt with variable substitution
    let variables = context.to_variables();
    let rendered_prompt = render_template(prompt, &variables);

    // Save rendered prompt to file for debugging and audit trail
    let prompt_file = workspace_path.join("prompt.txt");
    tokio::fs::write(&prompt_file, &rendered_prompt)
        .await
        .context("Failed to save prompt to workspace")?;

    println!("📂 Workspace: {}", workspace_path.display());

    // Register minion in registry
    let repo_display = if let (Some(ref owner), Some(ref repo)) = (&issue_owner, &issue_repo) {
        format!("{}/{}", owner, repo)
    } else {
        "ad-hoc".to_string()
    };
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: repo_display,
        issue: issue_number_val.unwrap_or(0),
        command: "prompt".to_string(),
        prompt: rendered_prompt.clone(),
        started_at: now,
        branch: branch_name,
        worktree: workspace_path.clone(),
        status: "active".to_string(),
        pr: None,
        session_id: session_id.to_string(),
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
        orchestration_phase: OrchestrationPhase::RunningClaude,
    };

    let mut registry = MinionRegistry::load(None).context("Failed to load Minion registry")?;
    registry
        .register(minion_id.clone(), registry_info)
        .context("Failed to register prompt Minion in registry")?;

    println!("🤖 Launching Claude...\n");

    // Create progress display
    let issue_display = if let Some(issue_num) = issue_number_val {
        format!(
            "#{}: {}",
            issue_num,
            context.issue_title.as_deref().unwrap_or("(no title)")
        )
    } else {
        format!("ad-hoc: {}", rendered_prompt)
    };
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue_display,
        quiet,
    };
    let progress = std::sync::Arc::new(ProgressDisplay::new(config));

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = build_claude_command(&run_dir, &session_id, &rendered_prompt);
    cmd.env("GRU_WORKSPACE", &minion_id);

    // Build on_spawn callback to record the child PID in the registry
    let pid_minion_id = minion_id.clone();
    let on_spawn: Box<dyn FnOnce(u32) + Send> = Box::new(move |pid: u32| {
        if let Ok(mut registry) = MinionRegistry::load(None) {
            let _ = registry.update(&pid_minion_id, |info| {
                info.pid = Some(pid);
                info.last_activity = Utc::now();
            });
        }
    });

    // Run Claude with stream monitoring
    let progress_cb = std::sync::Arc::clone(&progress);
    let output_callback = move |output: &stream::StreamOutput| {
        progress_cb.handle_output(output);
    };

    let run_result = run_claude_with_stream_monitoring(
        cmd,
        &workspace_path,
        timeout_opt.as_deref(),
        Some(output_callback),
        Some(on_spawn),
    )
    .await;

    // Remove minion from registry (best effort - don't fail if this errors).
    // No need to update PID/mode first since the entry is being deleted.
    if let Ok(mut registry) = MinionRegistry::load(None) {
        if let Err(e) = registry.remove(&minion_id) {
            log::info!(
                "Warning: Failed to remove minion {} from registry: {}",
                minion_id,
                e
            );
        }
    }

    // Now check if there was a stream error (after cleanup)
    let status = run_result?;

    // Finish the progress display and return appropriate exit code
    if status.success() {
        progress.finish_with_message("✅ Task completed");
        println!("\n📁 Session workspace: {}", workspace_path.display());
        println!("💡 To resume this session, use: gru resume {}", minion_id);
        Ok(0)
    } else {
        let exit_code = status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        progress.finish_with_message(&format!("❌ Task failed (exit code: {})", exit_code));
        println!(
            "\n📝 Events saved to: {}",
            workspace_path.join("events.jsonl").display()
        );
        println!(
            "📄 Prompt saved to: {}",
            workspace_path.join("prompt.txt").display()
        );
        Ok(exit_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_prompt_rejects_flag_like_input() {
        let result = handle_prompt("--help", None, false, vec![], None, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot start with '-'"));
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_empty_input() {
        let result = handle_prompt("", None, false, vec![], None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));

        let result = handle_prompt("   ", None, false, vec![], None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_parse_params_valid() {
        let params = vec!["key1=value1".to_string(), "key2=value2".to_string()];
        let result = parse_params(&params).unwrap();
        assert_eq!(result.get("key1"), Some(&"value1".to_string()));
        assert_eq!(result.get("key2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_parse_params_value_with_equals() {
        // Values can contain = signs
        let params = vec!["key=a=b=c".to_string()];
        let result = parse_params(&params).unwrap();
        assert_eq!(result.get("key"), Some(&"a=b=c".to_string()));
    }

    #[test]
    fn test_parse_params_empty_value() {
        let params = vec!["key=".to_string()];
        let result = parse_params(&params).unwrap();
        assert_eq!(result.get("key"), Some(&String::new()));
    }

    #[test]
    fn test_parse_params_no_equals() {
        let params = vec!["invalid".to_string()];
        let result = parse_params(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn test_parse_params_empty_key() {
        let params = vec!["=value".to_string()];
        let result = parse_params(&params);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("key cannot be empty"));
    }

    #[test]
    fn test_parse_params_empty_list() {
        let params: Vec<String> = vec![];
        let result = parse_params(&params).unwrap();
        assert!(result.is_empty());
    }
}
