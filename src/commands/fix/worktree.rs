use super::types::{FixOptions, IssueContext, WorktreeContext};
use crate::agent_runner::parse_timeout;
use crate::git;
use crate::minion;
use crate::minion_registry::{with_registry, MinionInfo, MinionMode, OrchestrationPhase};
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

/// Sets up the workspace: generates minion ID, clones repo, creates worktree,
/// registers the minion in the registry.
pub(super) async fn setup_worktree(
    ctx: &IssueContext,
    agent_name: &str,
    opts: &FixOptions,
) -> Result<WorktreeContext> {
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    println!(
        "🚀 Setting up workspace for {}/{}#{}",
        ctx.owner, ctx.repo, ctx.issue_num
    );

    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    let bare_path = workspace
        .repos()
        .join(&ctx.owner)
        .join(format!("{}.git", ctx.repo));
    let git_repo = git::GitRepo::new(&ctx.owner, &ctx.repo, &ctx.host, bare_path);

    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .await
        .context("Failed to clone or update repository")?;

    let branch_name = format!("minion/issue-{}-{}", ctx.issue_num, minion_id);
    println!("🌿 Creating worktree with branch: {}", branch_name);

    let repo_name = crate::github::repo_slug(&ctx.owner, &ctx.repo);
    let minion_dir = workspace
        .work_dir(&repo_name, &branch_name)
        .context("Failed to compute minion directory path")?;

    // Create checkout subdirectory for the git worktree
    let checkout_path = minion_dir.join("checkout");

    // Ensure the minion directory exists
    tokio::fs::create_dir_all(&minion_dir)
        .await
        .context("Failed to create minion directory")?;

    git_repo
        .create_worktree(&branch_name, &checkout_path)
        .await
        .context("Failed to create worktree")?;

    println!("📂 Workspace created at: {}", checkout_path.display());

    let session_id = Uuid::new_v4();

    // Register the Minion in the registry (worktree field stores the minion_dir)
    let now = Utc::now();
    let timeout_deadline = opts
        .timeout
        .as_deref()
        .map(parse_timeout)
        .transpose()
        .context("Invalid --timeout value")?
        .map(|dur| {
            chrono::TimeDelta::from_std(dur)
                .map_err(|_| anyhow::anyhow!("--timeout value is too large to represent"))
        })
        .transpose()?
        .map(|delta| now + delta);
    let registry_info = MinionInfo {
        repo: repo_name.clone(),
        issue: ctx.issue_num,
        command: "do".to_string(),
        prompt: format!("/do {}", ctx.issue_num),
        started_at: now,
        branch: branch_name.clone(),
        worktree: minion_dir.clone(),
        status: "active".to_string(),
        pr: None,
        session_id: session_id.to_string(),
        pid: None,
        pid_start_time: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
        orchestration_phase: OrchestrationPhase::Setup,
        token_usage: None,
        agent_name: agent_name.to_string(),
        timeout_deadline,
        attempt_count: 1,
        no_watch: opts.no_watch,
        last_review_check_time: None,
        wake_reason: None,
    };

    let minion_id_clone = minion_id.clone();
    with_registry(move |registry| registry.register(minion_id_clone, registry_info)).await?;

    println!("📝 Registered Minion {} in registry", minion_id);

    Ok(WorktreeContext {
        minion_id,
        branch_name,
        minion_dir,
        checkout_path,
        session_id,
    })
}
