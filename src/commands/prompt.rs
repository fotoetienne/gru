use crate::agent::AgentEvent;
use crate::agent_registry;
use crate::agent_runner::{run_agent_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED};
use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::minion_registry::{
    with_registry, MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry, OrchestrationPhase,
};
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::prompt_loader;
use crate::prompt_renderer::{render_template, PromptContext};
use crate::url_utils::{parse_github_url, parse_issue_info, GitHubResourceType};
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
async fn fetch_issue_context(
    issue_str: &str,
) -> Result<(PromptContext, String, String, String, u64)> {
    let github_hosts = crate::config::load_github_hosts();
    let (owner, repo, issue_num_str, host) = parse_issue_info(issue_str, &github_hosts).await?;
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
                let info = crate::github::get_issue_via_cli(&owner, &repo, &host, issue_number)
                    .await
                    .context("Failed to fetch issue via gh CLI")?;
                context.issue_title = Some(info.title);
                context.issue_body = Some(info.body.unwrap_or_default());
            }
        }
    } else {
        let info = crate::github::get_issue_via_cli(&owner, &repo, &host, issue_number)
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

    Ok((context, owner, repo, host, issue_number))
}

/// Parses a PR argument (number or URL) into (owner, repo, pr_number)
///
/// For plain numbers, auto-detects the repository from the current directory.
/// For URLs, extracts components directly.
async fn parse_pr_arg(pr_str: &str) -> Result<(String, String, String, u64)> {
    let github_hosts = crate::config::load_github_hosts();

    if let Ok(num) = pr_str.parse::<u64>() {
        // Auto-detect repository from current directory
        git::detect_git_repo()
            .await
            .context("Failed to detect git repository")?;

        let remote_url = git::get_github_remote(&github_hosts)
            .await
            .context("Failed to get GitHub remote")?;

        let (host, owner, repo) = git::parse_github_remote(&remote_url, &github_hosts)
            .context("Failed to parse GitHub remote URL")?;

        return Ok((owner, repo, host, num));
    }

    if let Some(parsed) = parse_github_url(pr_str, &github_hosts) {
        if parsed.resource_type == GitHubResourceType::Pull {
            return Ok((parsed.owner, parsed.repo, parsed.host, parsed.number as u64));
        }
        anyhow::bail!(
            "Expected a GitHub pull request URL, but got an issue URL.\n\
             Did you mean to use --issue instead?"
        );
    }

    anyhow::bail!(
        "Invalid PR format. Expected: <number> or <github-url>\n\
         Examples:\n\
         - gru prompt \"...\" --pr 42\n\
         - gru prompt \"...\" --pr https://github.com/owner/repo/pull/42"
    );
}

/// Fetches PR data from GitHub and populates a PromptContext.
/// Returns (context, owner, repo, branch_name).
async fn fetch_pr_context(pr_str: &str) -> Result<(PromptContext, String, String, String, String)> {
    let (owner, repo, host, pr_number) = parse_pr_arg(pr_str).await?;

    println!("🔗 Fetching PR #{} from {}/{}...", pr_number, owner, repo);

    let mut context = PromptContext::new();
    context.pr_number = Some(pr_number);
    context.repo_owner = Some(owner.clone());
    context.repo_name = Some(repo.clone());

    let branch_name;

    // Try API first, fall back to CLI
    if let Some(github_client) = GitHubClient::try_from_env(&owner, &repo).await {
        match github_client.get_pr(&owner, &repo, pr_number).await {
            Ok(pr) => {
                context.pr_title = Some(pr.title.clone().unwrap_or_default());
                context.pr_body = Some(pr.body.unwrap_or_default());
                branch_name = pr.head.ref_field.clone();
            }
            Err(e) => {
                log::warn!(
                    "Failed to fetch PR via API: {}. Falling back to gh CLI...",
                    e
                );
                let info = crate::github::get_pr_via_cli(&owner, &repo, &host, pr_number)
                    .await
                    .context("Failed to fetch PR via gh CLI")?;
                context.pr_title = Some(info.title);
                context.pr_body = Some(info.body.unwrap_or_default());
                branch_name = info.head_ref_name;
            }
        }
    } else {
        let info = crate::github::get_pr_via_cli(&owner, &repo, &host, pr_number)
            .await
            .context(
                "Failed to fetch PR. Ensure gh is installed and authenticated, \
                 or set GRU_GITHUB_TOKEN.",
            )?;
        context.pr_title = Some(info.title);
        context.pr_body = Some(info.body.unwrap_or_default());
        branch_name = info.head_ref_name;
    }

    println!(
        "   PR #{}: {}",
        pr_number,
        context.pr_title.as_deref().unwrap_or("(no title)")
    );

    Ok((context, owner, repo, host, branch_name))
}

/// Sets up a worktree for a PR by finding an existing one for the branch, or falling back to CWD
async fn setup_pr_worktree(
    owner: &str,
    repo: &str,
    host: &str,
    branch_name: &str,
) -> Result<Option<PathBuf>> {
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    let bare_path = workspace.repos().join(owner).join(format!("{}.git", repo));

    // Only look for existing worktrees if the bare repo exists
    if bare_path.exists() {
        let git_repo = git::GitRepo::new(owner, repo, host, bare_path);
        if let Some(existing_path) = git_repo
            .find_worktree_for_branch(branch_name)
            .await
            .context("Failed to check for existing worktree")?
        {
            println!(
                "♻️  Found existing worktree at: {}",
                existing_path.display()
            );
            return Ok(Some(existing_path));
        }
    }

    Ok(None)
}

/// Sets up a worktree for an issue, returning the worktree path and branch name
async fn setup_issue_worktree(
    owner: &str,
    repo: &str,
    host: &str,
    issue_number: u64,
    minion_id: &str,
) -> Result<(PathBuf, String)> {
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create bare repository path
    let bare_path = workspace.repos().join(owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(owner, repo, host, bare_path);

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

/// Formats prompt info into a displayable string.
/// Separated from handle_prompt_info for testability.
fn format_prompt_info(prompt: &prompt_loader::Prompt) -> String {
    use std::fmt::Write;
    let mut output = String::new();

    writeln!(output, "Prompt: {}", prompt.name).unwrap();

    // Description
    if let Some(ref desc) = prompt.metadata.description {
        writeln!(output, "{}", desc).unwrap();
    }

    // Required parameters (from requires field + required params)
    let requires = &prompt.metadata.requires;
    let required_params: Vec<&prompt_loader::PromptParam> = prompt
        .metadata
        .params
        .iter()
        .filter(|p| p.required)
        .collect();

    if !requires.is_empty() || !required_params.is_empty() {
        writeln!(output, "\nRequired parameters:").unwrap();
        for req in requires {
            match req.trim().to_lowercase().as_str() {
                "issue" => {
                    writeln!(output, "  --issue <number>    GitHub issue number or URL").unwrap()
                }
                "pr" => writeln!(output, "  --pr <number>       GitHub PR number or URL").unwrap(),
                _ => writeln!(output, "  {} (unknown requirement)", req).unwrap(),
            }
        }
        for param in &required_params {
            let desc = param.description.as_deref().unwrap_or("No description");
            writeln!(output, "  --param {}=<value>    {}", param.name, desc).unwrap();
        }
    }

    // Optional parameters
    let optional_params: Vec<&prompt_loader::PromptParam> = prompt
        .metadata
        .params
        .iter()
        .filter(|p| !p.required)
        .collect();

    if !optional_params.is_empty() {
        writeln!(output, "\nOptional parameters:").unwrap();
        for param in &optional_params {
            let desc = param.description.as_deref().unwrap_or("No description");
            writeln!(output, "  --param {}=<value>    {}", param.name, desc).unwrap();
        }
    }

    // Source location with override/shadowing indicator.
    // Only repo prompts can override built-ins (resolution: repo > built-in > global).
    // Global prompts are shadowed BY built-ins, not the other way around.
    let source_display = prompt.source.display();
    let matches_builtin = prompt_loader::BUILT_IN_PROMPTS
        .iter()
        .any(|b| b.name == prompt.name);
    if matches!(prompt.source, prompt_loader::PromptSource::Repo(_)) && matches_builtin {
        writeln!(
            output,
            "\nTemplate location: {} (overrides built-in)",
            source_display
        )
        .unwrap();
    } else if matches!(prompt.source, prompt_loader::PromptSource::Global(_)) && matches_builtin {
        // Note: currently unreachable through handle_prompt_info() because
        // load_prompts() resolves by priority and overwrites global entries with
        // built-ins of the same name. Kept for correctness if this formatter is
        // ever called from a different code path.
        writeln!(
            output,
            "\nTemplate location: {} (shadowed by built-in)",
            source_display
        )
        .unwrap();
    } else {
        writeln!(output, "\nTemplate location: {}", source_display).unwrap();
    }

    output
}

/// Formats built-in prompt info into a displayable string.
fn format_builtin_prompt_info(builtin: &prompt_loader::BuiltInPrompt) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    writeln!(output, "Prompt: {}", builtin.name).unwrap();
    writeln!(output, "{}", builtin.description).unwrap();
    if !builtin.requires.is_empty() {
        writeln!(output, "\nRequired parameters:").unwrap();
        for req in builtin.requires {
            match *req {
                "issue" => {
                    writeln!(output, "  --issue <number>    GitHub issue number or URL").unwrap()
                }
                "pr" => writeln!(output, "  --pr <number>       GitHub PR number or URL").unwrap(),
                _ => writeln!(output, "  {} (unknown requirement)", req).unwrap(),
            }
        }
    }
    writeln!(output, "\nTemplate location: built-in").unwrap();
    output
}

/// Handles the `gru prompt <name> --info` command by displaying prompt details
pub async fn handle_prompt_info(prompt_name: &str) -> Result<i32> {
    let trimmed = prompt_name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Prompt name cannot be empty");
    }

    // Check if it matches a built-in prompt (available even if file loading fails)
    let built_in = prompt_loader::BUILT_IN_PROMPTS
        .iter()
        .find(|b| b.name == trimmed);

    // Load file-based prompts; treat errors as non-fatal if we have a built-in fallback
    let repo_root = git::detect_git_repo().await.ok();
    let loaded_prompts = match prompt_loader::load_prompts(repo_root.as_deref()) {
        Ok(prompts) => prompts,
        Err(err) => {
            if let Some(builtin) = built_in {
                log::warn!("Failed to load prompts from files: {}", err);
                print!("{}", format_builtin_prompt_info(builtin));
                return Ok(0);
            }
            return Err(err);
        }
    };

    // File-based prompts take priority (they may override built-ins)
    if let Some(prompt) = loaded_prompts.get(trimmed) {
        print!("{}", format_prompt_info(prompt));
        return Ok(0);
    }

    // Fall back to built-in prompt
    if let Some(builtin) = built_in {
        print!("{}", format_builtin_prompt_info(builtin));
        return Ok(0);
    }

    // Prompt not found
    anyhow::bail!(
        "Unknown prompt '{}'. Run `gru prompts` to see available prompts.",
        trimmed
    );
}

/// Options for the prompt command, grouped to avoid too many function arguments
#[derive(Debug, Default)]
pub struct PromptOptions {
    pub issue: Option<String>,
    pub pr: Option<String>,
    pub no_worktree: bool,
    pub worktree: Option<String>,
    pub params: Vec<String>,
    pub timeout: Option<String>,
    pub quiet: bool,
    pub agent_name: String,
}

/// Handles the prompt command by launching Claude with an ad-hoc prompt
/// Returns the exit code from the claude process
pub async fn handle_prompt(prompt: &str, opts: PromptOptions) -> Result<i32> {
    let issue_opt = opts.issue;
    let pr_opt = opts.pr;
    let no_worktree = opts.no_worktree;
    let worktree_opt = opts.worktree;
    let params = opts.params;
    let timeout_opt = opts.timeout;
    let quiet = opts.quiet;
    let agent_name = opts.agent_name;
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

    // Validate --worktree path early, before any network calls or state changes
    if let Some(ref wt) = worktree_opt {
        let p = PathBuf::from(wt);
        if !p.exists() {
            anyhow::bail!(
                "Worktree path does not exist: {}\n\
                 Ensure the path exists before using --worktree.",
                p.display()
            );
        }
        if !p.is_dir() {
            anyhow::bail!("Worktree path is not a directory: {}", p.display());
        }
    }

    // Parse custom parameters
    let custom_params = parse_params(&params)?;

    // Try to resolve the prompt as a file-based prompt name.
    // If it matches a loaded prompt file, use its content and validate requirements.
    // Otherwise, treat it as ad-hoc prompt text.
    // Use git toplevel (not cwd) so prompts are found from subdirectories.
    let repo_root = git::detect_git_repo().await.ok();
    let loaded_prompts = prompt_loader::load_prompts(repo_root.as_deref()).unwrap_or_else(|e| {
        log::warn!("Failed to load prompt files: {}", e);
        HashMap::new()
    });
    let resolved_prompt = if let Some(file_prompt) = loaded_prompts.get(trimmed_prompt) {
        // Validate prompt syntax (non-empty content, valid param names)
        prompt_loader::validate_prompt(file_prompt)?;

        // Validate requirements before proceeding
        prompt_loader::validate_prompt_requirements(
            &file_prompt.name,
            &file_prompt.metadata,
            issue_opt.is_some(),
            pr_opt.is_some(),
            &custom_params,
        )?;

        file_prompt.content.clone()
    } else {
        trimmed_prompt.to_string()
    };

    // Build prompt context from --issue and --pr flags and custom params
    let mut context = PromptContext::new();
    let mut context_owner: Option<String> = None;
    let mut context_repo: Option<String> = None;
    let mut context_host: Option<String> = None;
    let mut issue_number_val: Option<u64> = None;
    let mut pr_owner: Option<String> = None;
    let mut pr_repo: Option<String> = None;
    let mut pr_host: Option<String> = None;
    let mut pr_branch: Option<String> = None;

    if let Some(ref issue_str) = issue_opt {
        let (issue_ctx, owner, repo, host, issue_num) = fetch_issue_context(issue_str).await?;
        context = issue_ctx;
        context_owner = Some(owner);
        context_repo = Some(repo);
        context_host = Some(host);
        issue_number_val = Some(issue_num);
    }

    if let Some(ref pr_str) = pr_opt {
        let (pr_ctx, owner, repo, host, branch) = fetch_pr_context(pr_str).await?;
        // Merge PR context into existing context (issue fields are preserved)
        context.pr_number = pr_ctx.pr_number;
        context.pr_title = pr_ctx.pr_title;
        context.pr_body = pr_ctx.pr_body;
        // Track PR's repo separately for worktree lookup
        pr_owner = Some(owner.clone());
        pr_repo = Some(repo.clone());
        pr_host = Some(host.clone());
        // Set repo info from PR if not already set by --issue
        if context_owner.is_none() {
            context_owner = Some(owner);
            context.repo_owner = pr_ctx.repo_owner;
        }
        if context_repo.is_none() {
            context_repo = Some(repo);
            context.repo_name = pr_ctx.repo_name;
        }
        pr_branch = Some(branch);
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

    // Set up worktree or ad-hoc workspace.
    // Priority order:
    //   1. --worktree <path>: use explicit path (validated to exist)
    //   2. --no-worktree: force CWD even when --issue/--pr provided
    //   3. --issue: auto-create worktree for issue
    //   4. --pr: reuse existing worktree or fall back to CWD
    //   5. Default: ad-hoc workspace with CWD as run directory
    //
    // When both --issue and --pr are provided, --issue wins for worktree creation
    // since Claude runs in the issue worktree. PR template variables are still
    // populated regardless of which worktree is used.
    let has_context = issue_opt.is_some() || pr_opt.is_some();
    let use_auto_worktree = !no_worktree && worktree_opt.is_none();
    let (workspace_path, branch_name, run_dir) = if let Some(ref explicit_path) = worktree_opt {
        // --worktree <path>: use the explicit path as both workspace and run directory.
        // Path existence and is_dir checks were already done at the top of the function.
        let wt_path = PathBuf::from(explicit_path)
            .canonicalize()
            .with_context(|| format!("Failed to resolve worktree path: {}", explicit_path))?;
        if !quiet {
            println!("📂 Using explicit worktree: {}", wt_path.display());
        }
        context.worktree_path = Some(wt_path.clone());
        // Preserve PR branch name for registry and template context
        let branch = pr_branch.clone().unwrap_or_default();
        if !branch.is_empty() {
            context.branch_name = Some(branch.clone());
        }
        let run_dir = wt_path.clone();
        (wt_path, branch, run_dir)
    } else if issue_opt.is_some() && use_auto_worktree {
        let owner = context_owner.as_deref().unwrap();
        let repo = context_repo.as_deref().unwrap();
        let issue_num = issue_number_val.unwrap();

        let host = context_host.as_deref().unwrap_or("github.com");
        let (wt_path, branch) =
            setup_issue_worktree(owner, repo, host, issue_num, &minion_id).await?;
        context.worktree_path = Some(wt_path.clone());
        context.branch_name = Some(branch.clone());
        let run_dir = wt_path.clone();
        (wt_path, branch, run_dir)
    } else if pr_opt.is_some() && use_auto_worktree {
        // --pr: try to find an existing worktree for the PR branch, fall back to CWD
        // Use the PR's own owner/repo for worktree lookup (may differ from --issue repo)
        let owner = pr_owner.as_deref().unwrap();
        let repo = pr_repo.as_deref().unwrap();
        let branch = pr_branch.as_deref().unwrap();

        let host = pr_host.as_deref().unwrap_or("github.com");
        if let Some(wt_path) = setup_pr_worktree(owner, repo, host, branch).await? {
            context.worktree_path = Some(wt_path.clone());
            context.branch_name = Some(branch.to_string());
            let run_dir = wt_path.clone();
            (wt_path, branch.to_string(), run_dir)
        } else {
            println!("ℹ️  No existing worktree found for PR branch - using current directory");
            let run_dir =
                std::env::current_dir().context("Failed to get current working directory")?;
            let wt_path = run_dir.clone();
            context.worktree_path = Some(wt_path.clone());
            context.branch_name = Some(branch.to_string());
            (wt_path, branch.to_string(), run_dir)
        }
    } else if no_worktree && has_context {
        // --no-worktree: use the current directory as both the
        // workspace and the run directory so the registry matches reality.
        println!("ℹ️  Running without worktree - Claude will work in the current directory");
        let run_dir = std::env::current_dir().context("Failed to get current working directory")?;
        let wt_path = run_dir.clone();
        context.worktree_path = Some(wt_path.clone());
        // Preserve PR branch name for registry and template context
        let branch = pr_branch.clone().unwrap_or_default();
        if !branch.is_empty() {
            context.branch_name = Some(branch.clone());
        }
        (wt_path, branch, run_dir)
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
    let rendered_prompt = render_template(&resolved_prompt, &variables);

    // Save rendered prompt to file for debugging and audit trail
    let prompt_file = workspace_path.join("prompt.txt");
    tokio::fs::write(&prompt_file, &rendered_prompt)
        .await
        .context("Failed to save prompt to workspace")?;

    println!("📂 Workspace: {}", workspace_path.display());

    // Resolve the agent backend early, before any registry/state side effects
    let backend = agent_registry::resolve_backend(&agent_name)?;

    // Register minion in registry
    let repo_display = if let (Some(ref owner), Some(ref repo)) = (&context_owner, &context_repo) {
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
        orchestration_phase: OrchestrationPhase::RunningAgent,
        token_usage: None,
        agent_name: agent_name.clone(),
    };

    let minion_id_clone = minion_id.clone();
    with_registry(move |registry| registry.register(minion_id_clone, registry_info)).await?;

    println!("🤖 Launching {}...\n", backend.name());

    // Create progress display
    let issue_display = if let Some(issue_num) = issue_number_val {
        format!(
            "#{}: {}",
            issue_num,
            context.issue_title.as_deref().unwrap_or("(no title)")
        )
    } else if let Some(pr_num) = context.pr_number {
        format!(
            "PR #{}: {}",
            pr_num,
            context.pr_title.as_deref().unwrap_or("(no title)")
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
    let mut cmd = backend.build_command(&run_dir, &session_id, &rendered_prompt);
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

    // Run agent with stream monitoring
    let progress_cb = std::sync::Arc::clone(&progress);
    let output_callback = move |event: &AgentEvent| {
        progress_cb.handle_event(event);
    };

    let run_result = run_agent_with_stream_monitoring(
        cmd,
        &*backend,
        &workspace_path,
        timeout_opt.as_deref(),
        Some(output_callback),
        Some(on_spawn),
    )
    .await;

    // Remove minion from registry (best effort - don't fail if this errors).
    // No need to update PID/mode first since the entry is being deleted.
    let remove_id = minion_id.clone();
    if let Err(e) = with_registry(move |registry| {
        registry.remove(&remove_id)?;
        Ok(())
    })
    .await
    {
        log::info!(
            "Warning: Failed to remove minion {} from registry: {}",
            minion_id,
            e
        );
    }

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
        let result = handle_prompt("--help", PromptOptions::default()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot start with '-'"));
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_empty_input() {
        let result = handle_prompt("", PromptOptions::default()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));

        let result = handle_prompt("   ", PromptOptions::default()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_nonexistent_worktree_path() {
        let opts = PromptOptions {
            worktree: Some("/nonexistent/path/that/does/not/exist".to_string()),
            ..PromptOptions::default()
        };
        let result = handle_prompt("test prompt", opts).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("does not exist"),
            "Expected 'does not exist' error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_file_as_worktree_path() {
        // Use tempfile::NamedTempFile for unique paths and automatic cleanup
        let temp_file =
            tempfile::NamedTempFile::new().expect("Failed to create temp file for test");
        let opts = PromptOptions {
            worktree: Some(temp_file.path().to_string_lossy().to_string()),
            ..PromptOptions::default()
        };
        let result = handle_prompt("test prompt", opts).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not a directory"),
            "Expected 'not a directory' error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_handle_prompt_worktree_flag_accepts_existing_path() {
        // Use tempfile::TempDir for unique paths and automatic cleanup.
        // The prompt will proceed past validation but fail later (no Claude binary),
        // which proves the path validation itself passed.
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir for test");
        let opts = PromptOptions {
            worktree: Some(temp_dir.path().to_string_lossy().to_string()),
            ..PromptOptions::default()
        };
        let result = handle_prompt("test prompt", opts).await;
        // We expect an error further down (workspace init, claude binary, etc.)
        // but NOT the "does not exist" or "not a directory" error
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("does not exist") && !msg.contains("not a directory"),
                "Path validation should pass for existing directory, got: {}",
                msg
            );
        }
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

    // --- parse_pr_arg tests (URL paths only; plain numbers need git context) ---

    #[tokio::test]
    async fn test_parse_pr_arg_with_url() {
        let result = parse_pr_arg("https://github.com/fotoetienne/gru/pull/42")
            .await
            .unwrap();
        assert_eq!(result.0, "fotoetienne");
        assert_eq!(result.1, "gru");
        assert_eq!(result.2, "github.com");
        assert_eq!(result.3, 42);
    }

    #[tokio::test]
    async fn test_parse_pr_arg_with_url_and_query_params() {
        let result = parse_pr_arg("https://github.com/owner/repo/pull/123?foo=bar")
            .await
            .unwrap();
        assert_eq!(result.0, "owner");
        assert_eq!(result.1, "repo");
        assert_eq!(result.2, "github.com");
        assert_eq!(result.3, 123);
    }

    #[tokio::test]
    async fn test_parse_pr_arg_rejects_issue_url() {
        let err = parse_pr_arg("https://github.com/owner/repo/issues/42")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("issue URL"),
            "Expected specific error for issue URL given to PR parser, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_parse_pr_arg_rejects_invalid() {
        assert!(parse_pr_arg("not-a-number").await.is_err());
        assert!(parse_pr_arg("").await.is_err());
        assert!(parse_pr_arg("-42").await.is_err());
    }

    // --- format_prompt_info tests ---

    #[test]
    fn test_format_prompt_info_full() {
        let prompt = prompt_loader::Prompt {
            name: "do".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Work on a GitHub issue with tests and PR".to_string()),
                requires: vec!["issue".to_string()],
                params: vec![prompt_loader::PromptParam {
                    name: "target".to_string(),
                    description: Some("Specific file/module to focus on".to_string()),
                    required: false,
                }],
            },
            content: "template content".to_string(),
            source: prompt_loader::PromptSource::Repo(PathBuf::from(".gru/prompts/do.md")),
        };

        let output = format_prompt_info(&prompt);
        assert!(output.contains("Prompt: do"));
        assert!(output.contains("Work on a GitHub issue with tests and PR"));
        assert!(output.contains("Required parameters:"));
        assert!(output.contains("--issue <number>"));
        assert!(output.contains("Optional parameters:"));
        assert!(output.contains("--param target=<value>"));
        assert!(output.contains("Specific file/module to focus on"));
        assert!(output.contains("(overrides built-in)"));
    }

    #[test]
    fn test_format_prompt_info_no_description() {
        let prompt = prompt_loader::Prompt {
            name: "simple".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: None,
                requires: vec![],
                params: vec![],
            },
            content: "template content".to_string(),
            source: prompt_loader::PromptSource::Global(PathBuf::from(
                "/home/user/.gru/prompts/simple.md",
            )),
        };

        let output = format_prompt_info(&prompt);
        assert!(output.contains("Prompt: simple"));
        assert!(!output.contains("Required parameters:"));
        assert!(!output.contains("Optional parameters:"));
        assert!(output.contains("~/.gru/prompts/simple.md"));
        assert!(!output.contains("overrides built-in"));
    }

    #[test]
    fn test_format_prompt_info_required_params_only() {
        let prompt = prompt_loader::Prompt {
            name: "analyze".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Analyze a component".to_string()),
                requires: vec![],
                params: vec![prompt_loader::PromptParam {
                    name: "component".to_string(),
                    description: Some("Component to analyze".to_string()),
                    required: true,
                }],
            },
            content: "template content".to_string(),
            source: prompt_loader::PromptSource::Repo(PathBuf::from(".gru/prompts/analyze.md")),
        };

        let output = format_prompt_info(&prompt);
        assert!(output.contains("Required parameters:"));
        assert!(output.contains("--param component=<value>    Component to analyze"));
        assert!(!output.contains("Optional parameters:"));
    }

    #[test]
    fn test_format_prompt_info_pr_requirement() {
        let prompt = prompt_loader::Prompt {
            name: "review-custom".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Custom review prompt".to_string()),
                requires: vec!["pr".to_string()],
                params: vec![],
            },
            content: "template content".to_string(),
            source: prompt_loader::PromptSource::Repo(PathBuf::from(
                ".gru/prompts/review-custom.md",
            )),
        };

        let output = format_prompt_info(&prompt);
        assert!(output.contains("--pr <number>"));
    }

    #[test]
    fn test_format_builtin_prompt_info() {
        let builtin = prompt_loader::BuiltInPrompt {
            name: "do",
            description: "Work on a GitHub issue with tests and PR",
            requires: &["issue"],
            content: "",
        };
        let output = format_builtin_prompt_info(&builtin);
        assert!(output.contains("Prompt: do"));
        assert!(output.contains("Work on a GitHub issue with tests and PR"));
        assert!(output.contains("--issue <number>"));
        assert!(output.contains("Template location: built-in"));
    }

    #[test]
    fn test_format_prompt_info_builtin_source_not_marked_as_override() {
        // Even if the name matches a built-in, a BuiltIn-sourced prompt
        // should not be marked as "overrides built-in"
        let prompt = prompt_loader::Prompt {
            name: "do".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Built-in do".to_string()),
                requires: vec![],
                params: vec![],
            },
            content: "content".to_string(),
            source: prompt_loader::PromptSource::BuiltIn,
        };

        let output = format_prompt_info(&prompt);
        assert!(!output.contains("overrides built-in"));
        assert!(output.contains("Template location: built-in"));
    }

    #[test]
    fn test_format_prompt_info_global_source_shadowed_by_builtin() {
        // A global prompt named "do" should show "shadowed by built-in"
        // because built-ins have higher priority than global prompts
        let prompt = prompt_loader::Prompt {
            name: "do".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Global do attempt".to_string()),
                requires: vec![],
                params: vec![],
            },
            content: "content".to_string(),
            source: prompt_loader::PromptSource::Global(PathBuf::from(
                "/home/user/.gru/prompts/do.md",
            )),
        };

        let output = format_prompt_info(&prompt);
        assert!(
            output.contains("shadowed by built-in"),
            "Global prompt matching built-in name should show shadowed indicator, got: {}",
            output
        );
        assert!(!output.contains("overrides built-in"));
        assert!(output.contains("~/.gru/prompts/do.md"));
    }

    #[test]
    fn test_format_prompt_info_repo_source_overrides_builtin() {
        // A repo prompt named "do" should show "overrides built-in"
        let prompt = prompt_loader::Prompt {
            name: "do".to_string(),
            metadata: prompt_loader::PromptMetadata {
                description: Some("Team do workflow".to_string()),
                requires: vec!["issue".to_string()],
                params: vec![],
            },
            content: "content".to_string(),
            source: prompt_loader::PromptSource::Repo(PathBuf::from("/repo/.gru/prompts/do.md")),
        };

        let output = format_prompt_info(&prompt);
        assert!(
            output.contains("overrides built-in"),
            "Repo prompt matching built-in name should show overrides indicator, got: {}",
            output
        );
        assert!(output.contains(".gru/prompts/do.md"));
    }

    #[tokio::test]
    async fn test_handle_prompt_info_builtin_success() {
        // "do" is always in BUILT_IN_PROMPTS
        let result = handle_prompt_info("do").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_handle_prompt_info_rejects_empty_name() {
        let result = handle_prompt_info("").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));

        let result = handle_prompt_info("  ").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn test_handle_prompt_info_unknown_prompt() {
        let result = handle_prompt_info("nonexistent-prompt-xyz").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown prompt"));
        assert!(err.contains("nonexistent-prompt-xyz"));
        assert!(err.contains("gru prompts"));
    }
}
