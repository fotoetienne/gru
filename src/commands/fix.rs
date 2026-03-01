use crate::ci;
use crate::claude_runner::{
    build_claude_command, build_claude_resume_command, run_claude_with_stream_monitoring,
    EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::git;
use crate::github::GitHubClient;
use crate::minion;
use crate::minion_registry::{
    is_process_alive, MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry,
};
use crate::pr_monitor::{self, MonitorResult};
use crate::pr_state::PrState;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::progress_comments::{MinionPhase, ProgressCommentTracker};
use crate::stream;
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

/// Maximum size of the output buffer for test detection (in bytes)
const MAX_OUTPUT_BUFFER_SIZE: usize = 10000;

/// Size to trim the output buffer to when it exceeds the maximum (in bytes)
const TRIM_OUTPUT_BUFFER_SIZE: usize = 5000;

/// Default timeout for review process in seconds (30 minutes)
/// Reviews can take longer than fixes due to analysis depth
const DEFAULT_REVIEW_TIMEOUT_SECS: u64 = 1800;

/// Maximum number of review rounds to handle automatically
/// After this limit, the user must handle additional reviews manually
const MAX_REVIEW_ROUNDS: usize = 5;

// ---------------------------------------------------------------------------
// Phase context structs
// ---------------------------------------------------------------------------

/// Result of resolving an issue argument into validated context.
/// Contains the parsed issue number as `u64`, eliminating repeated string parsing.
pub struct IssueContext {
    pub owner: String,
    pub repo: String,
    pub issue_num: u64,
    /// Fetched issue details: (title, body, labels). None if fetch failed.
    pub details: Option<IssueDetails>,
    pub github_client: Option<GitHubClient>,
}

/// Fetched issue metadata from GitHub.
pub struct IssueDetails {
    pub title: String,
    pub body: String,
    pub labels: String,
}

/// Result of setting up a worktree for a minion.
pub struct WorktreeContext {
    pub minion_id: String,
    pub branch_name: String,
    pub worktree_path: PathBuf,
    pub session_id: Uuid,
}

/// Result of running a Claude session.
pub struct ClaudeResult {
    pub status: ExitStatus,
}

// ---------------------------------------------------------------------------
// Helper functions (unchanged from original)
// ---------------------------------------------------------------------------

/// Checks if a branch has been pushed to the remote
async fn is_branch_pushed(worktree_path: &Path, branch_name: &str) -> Result<bool> {
    let output = TokioCommand::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("rev-parse")
        .arg(format!("origin/{}", branch_name))
        .output()
        .await
        .context("Failed to check if branch is pushed")?;

    Ok(output.status.success())
}

/// Creates a WIP PR title and body template
fn create_wip_template(minion_id: &str, issue_num: u64, issue_title: &str) -> (String, String) {
    let title = format!("[{}] Fixes #{}: {}", minion_id, issue_num, issue_title);
    let body = format!(
        "## Summary\nAutomated fix for #{} by Minion {}\n\n## Status\n- [ ] Implementation\n- [ ] Tests\n- [ ] Review\n",
        issue_num, minion_id
    );
    (title, body)
}

/// Creates a PR for the given issue, returning the PR number
async fn create_pr_for_issue(
    owner: &str,
    repo: &str,
    branch_name: &str,
    issue_num: u64,
    minion_id: &str,
    worktree_path: &Path,
    issue_title_opt: Option<&str>,
) -> Result<String> {
    // Detect base branch
    let base_output = TokioCommand::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()
        .await
        .context("Failed to detect base branch")?;

    let base_branch = if base_output.status.success() {
        let raw = String::from_utf8_lossy(&base_output.stdout);
        raw.trim()
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("main")
            .to_string()
    } else {
        "main".to_string()
    };

    // Use cached title or generate a default
    let issue_title = issue_title_opt.unwrap_or("Fix issue");

    let (title, body) = create_wip_template(minion_id, issue_num, issue_title);

    // Check for PR_DESCRIPTION.md to determine if work is complete
    let desc_path = worktree_path.join("PR_DESCRIPTION.md");
    let (final_title, final_body, is_ready) = if desc_path.exists() {
        let desc_content = tokio::fs::read_to_string(&desc_path)
            .await
            .unwrap_or_default();
        let ready_title = format!("Fixes #{}: {}", issue_num, issue_title);
        (ready_title, desc_content, true)
    } else {
        (title, body, false)
    };

    // Create draft PR via gh CLI
    let mut pr_cmd = TokioCommand::new("gh");
    pr_cmd
        .arg("pr")
        .arg("create")
        .arg("--draft")
        .arg("--title")
        .arg(&final_title)
        .arg("--body")
        .arg(&final_body)
        .arg("--base")
        .arg(&base_branch)
        .arg("--head")
        .arg(branch_name)
        .arg("--repo")
        .arg(format!("{}/{}", owner, repo))
        .current_dir(worktree_path);

    let pr_output = pr_cmd.output().await.context("Failed to create draft PR")?;

    if !pr_output.status.success() {
        let stderr = String::from_utf8_lossy(&pr_output.stderr);
        anyhow::bail!("Failed to create PR: {}", stderr.trim());
    }

    let pr_url = String::from_utf8_lossy(&pr_output.stdout)
        .trim()
        .to_string();

    // Extract PR number from URL
    let pr_number = pr_url.rsplit('/').next().unwrap_or(&pr_url).to_string();

    // Mark ready if description exists
    if is_ready {
        // Mark PR as ready using gh CLI directly
        let _ = TokioCommand::new("gh")
            .args([
                "pr",
                "ready",
                &pr_number,
                "--repo",
                &format!("{}/{}", owner, repo),
            ])
            .output()
            .await;
        // Clean up description file
        let _ = tokio::fs::remove_file(&desc_path).await;
    }

    Ok(pr_number)
}

/// Handles GitHub CLI fetch result with fallback
async fn handle_cli_fetch_result(result: Result<crate::github::IssueInfo>) -> Option<IssueDetails> {
    match result {
        Ok(info) => Some(IssueDetails {
            title: info.title,
            body: info.body.unwrap_or_default(),
            labels: String::new(), // CLI doesn't return labels in IssueInfo
        }),
        Err(e) => {
            eprintln!("⚠️  Failed to fetch issue details via CLI: {}", e);
            None
        }
    }
}

/// Invokes Claude to address review comments using the same session
async fn invoke_claude_for_reviews(
    worktree_path: &Path,
    session_id: &Uuid,
    prompt: &str,
    timeout_opt: Option<&str>,
) -> Result<()> {
    let cmd = build_claude_resume_command(worktree_path, session_id, prompt);

    run_claude_with_stream_monitoring(
        cmd,
        worktree_path,
        timeout_opt,
        None::<fn(&stream::StreamOutput)>,
        None::<Box<dyn FnOnce(u32) + Send>>,
    )
    .await?;
    Ok(())
}

/// Trigger a PR review as a separate process, with a timeout
async fn trigger_pr_review(
    pr_number: &str,
    worktree_path: &Path,
    timeout_secs: Option<u64>,
) -> Result<i32> {
    // Validate PR number format
    if pr_number.is_empty() || !pr_number.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("Invalid PR number format: '{}'", pr_number);
    }

    let timeout_duration = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_REVIEW_TIMEOUT_SECS));

    let mut child = TokioCommand::new("gru")
        .arg("review")
        .arg(pr_number)
        .current_dir(worktree_path)
        .spawn()
        .context("Failed to spawn gru review process")?;

    match timeout(timeout_duration, child.wait()).await {
        Ok(Ok(status)) => Ok(status.code().unwrap_or(1)),
        Ok(Err(e)) => Err(e).context("Failed to wait for review process"),
        Err(_) => {
            // Timeout - kill the process
            let _ = child.kill().await;
            anyhow::bail!(
                "PR review timed out after {} seconds",
                timeout_duration.as_secs()
            );
        }
    }
}

/// Posts a progress comment to the issue (fire-and-forget).
async fn try_post_progress_comment(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    issue_num: u64,
    body: &str,
) -> bool {
    match client.post_comment(owner, repo, issue_num, body).await {
        Ok(_) => true,
        Err(e) => {
            log::warn!("⚠️  Failed to post progress comment: {}", e);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Resolve Issue
// ---------------------------------------------------------------------------

/// Resolves an issue argument into validated context.
///
/// Parses the issue string, checks for existing minions (unless `force_new`),
/// initializes the GitHub client, and fetches issue details.
async fn resolve_issue(issue: &str, force_new: bool) -> Result<IssueContext> {
    let (owner, repo, issue_num_str) = parse_issue_info(issue)?;
    let issue_num: u64 = issue_num_str
        .parse()
        .context("Failed to parse issue number")?;

    // Check for existing Minions on this issue
    if !force_new {
        check_existing_minions(&owner, &repo, issue_num).await?;
    }

    // Initialize GitHub client (optional - only if token is available)
    let github_client = GitHubClient::from_env(&owner, &repo).await.ok();
    if github_client.is_none() {
        println!("⚠️  No GitHub authentication found - progress comments will not be posted");
    }

    // Claim the issue by adding in-progress label (fire-and-forget)
    if let Some(ref client) = github_client {
        claim_issue(client, &owner, &repo, issue_num).await;
    }

    // Fetch issue details
    let details = fetch_issue_details(&owner, &repo, issue_num, &github_client).await;

    Ok(IssueContext {
        owner,
        repo,
        issue_num,
        details,
        github_client,
    })
}

/// Checks if there are existing minions working on this issue and returns an
/// error with suggestions if so.
async fn check_existing_minions(owner: &str, repo: &str, issue_num: u64) -> Result<()> {
    let repo_for_check = format!("{}/{}", owner, repo);
    let mut existing = tokio::task::spawn_blocking(move || {
        let registry = MinionRegistry::load(None)?;
        Ok::<_, anyhow::Error>(registry.find_by_issue(&repo_for_check, issue_num))
    })
    .await
    .context("Failed to spawn blocking task for duplicate check")??;

    if existing.is_empty() {
        return Ok(());
    }

    // Sort deterministically: running Minions first, then by most recent start time.
    existing.sort_by(|(_, a), (_, b)| {
        let a_running = a.pid.map(is_process_alive).unwrap_or(false);
        let b_running = b.pid.map(is_process_alive).unwrap_or(false);
        b_running
            .cmp(&a_running)
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });

    eprintln!(
        "Error: {} existing Minion(s) found for issue {}:\n",
        existing.len(),
        issue_num
    );

    for (minion_id, info) in &existing {
        let actually_running = info.pid.map(is_process_alive).unwrap_or(false);
        let status_msg = if actually_running {
            match info.mode {
                MinionMode::Autonomous => "running (autonomous)",
                MinionMode::Interactive => "running (interactive)",
                MinionMode::Stopped => "running",
            }
        } else {
            "stopped"
        };
        eprintln!("  {} - status: {}", minion_id, status_msg);
    }

    let (best_id, best_info) = existing.first().unwrap();
    let best_running = best_info.pid.map(is_process_alive).unwrap_or(false);

    eprintln!("\nOptions:");
    if best_running {
        eprintln!("  - Attach interactively: gru attach {}", best_id);
    } else {
        eprintln!("  - Resume work:          gru resume {}", best_id);
        eprintln!("  - Attach interactively: gru attach {}", best_id);
    }
    eprintln!(
        "  - Create new session:   gru fix {} --force-new",
        issue_num
    );

    // Return a special exit-code error that handle_fix can map to Ok(1)
    anyhow::bail!("existing_minion_found");
}

/// Claims an issue by adding the in-progress label.
async fn claim_issue(client: &GitHubClient, owner: &str, repo: &str, issue_num: u64) {
    match client.claim_issue(owner, repo, issue_num).await {
        Ok(true) => {
            println!("🏷️  Added 'in-progress' label to issue #{}", issue_num);
        }
        Ok(false) => {
            log::warn!(
                "⚠️  Issue #{} is already claimed by another Minion",
                issue_num
            );
            log::warn!("   This may indicate a race condition or multiple gru instances.");
            log::warn!("   Continuing anyway (worktree already created)...");
        }
        Err(e) => {
            log::warn!("⚠️  Failed to add label to issue: {}", e);
            log::warn!("   Continuing anyway...");
        }
    }
}

/// Fetches issue details from GitHub API with CLI fallback.
async fn fetch_issue_details(
    owner: &str,
    repo: &str,
    issue_num: u64,
    github_client: &Option<GitHubClient>,
) -> Option<IssueDetails> {
    if let Some(ref client) = github_client {
        match client.get_issue(owner, repo, issue_num).await {
            Ok(issue) => {
                let labels = issue
                    .labels
                    .iter()
                    .map(|l| l.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let body = issue.body.unwrap_or_default();
                Some(IssueDetails {
                    title: issue.title,
                    body,
                    labels,
                })
            }
            Err(e) => {
                eprintln!("⚠️  Failed to fetch issue details via API: {}", e);
                eprintln!("   Falling back to gh CLI...");
                let cli_result = crate::github::get_issue_via_cli(owner, repo, issue_num).await;
                let result = handle_cli_fetch_result(cli_result).await;
                if result.is_none() {
                    eprintln!("   Fix authentication with: gh auth login");
                }
                result
            }
        }
    } else {
        let cli_result = crate::github::get_issue_via_cli(owner, repo, issue_num).await;
        let result = handle_cli_fetch_result(cli_result).await;
        if result.is_none() {
            eprintln!("   Fix authentication with: gh auth login");
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Setup Worktree
// ---------------------------------------------------------------------------

/// Sets up the workspace: generates minion ID, clones repo, creates worktree,
/// registers the minion in the registry.
async fn setup_worktree(ctx: &IssueContext) -> Result<WorktreeContext> {
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
    let git_repo = git::GitRepo::new(&ctx.owner, &ctx.repo, bare_path);

    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .context("Failed to clone or update repository")?;

    let branch_name = format!("minion/issue-{}-{}", ctx.issue_num, minion_id);
    println!("🌿 Creating worktree with branch: {}", branch_name);

    let repo_name = format!("{}/{}", ctx.owner, ctx.repo);
    let worktree_path = workspace
        .work_dir(&repo_name, &branch_name)
        .context("Failed to compute worktree path")?;

    git_repo
        .create_worktree(&branch_name, &worktree_path)
        .context("Failed to create worktree")?;

    println!("📂 Workspace created at: {}", worktree_path.display());

    let session_id = Uuid::new_v4();

    // Register the Minion in the registry
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: repo_name.clone(),
        issue: ctx.issue_num,
        command: "fix".to_string(),
        prompt: format!("/fix {}", ctx.issue_num),
        started_at: now,
        branch: branch_name.clone(),
        worktree: worktree_path.clone(),
        status: "active".to_string(),
        pr: None,
        session_id: session_id.to_string(),
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
    };

    let minion_id_clone = minion_id.clone();
    tokio::task::spawn_blocking(move || {
        let mut registry = MinionRegistry::load(None)?;
        registry.register(minion_id_clone, registry_info)
    })
    .await
    .context("Failed to spawn blocking task for registry registration")??;

    println!("📝 Registered Minion {} in registry", minion_id);

    Ok(WorktreeContext {
        minion_id,
        branch_name,
        worktree_path,
        session_id,
    })
}

// ---------------------------------------------------------------------------
// Phase 3: Run Claude
// ---------------------------------------------------------------------------

/// Builds the prompt string from issue context.
fn build_fix_prompt(ctx: &IssueContext) -> String {
    if let Some(ref details) = ctx.details {
        let labels_section = if details.labels.is_empty() {
            String::new()
        } else {
            format!("\nLabels: {}", details.labels)
        };

        format!(
            r#"
# Issue #{}: {}

URL: https://github.com/{}/{}/issues/{}{}

## Description:
{}

# Instructions

## 1. Check if Decomposition is Needed
- Assess the issue's complexity:
  - Does it involve multiple distinct components or systems?
  - Does it have multiple acceptance criteria?
  - Would it take more than a few hours to complete?
  - Does it mix different types of work (backend + frontend + docs)?

- **If the issue is complex and should be broken down:**
  - Recommend to the user: "This issue seems complex. Run `/decompose $ARGUMENTS` to break it into smaller sub-issues first."
  - Stop the fix workflow here - wait for user to decompose

- **If the issue is focused and ready to fix:**
  - Proceed to the next step

## 2. Plan the Fix
- Explore the codebase to understand the relevant code
- Create a detailed plan using TodoWrite with specific steps to fix the issue
- Consider tests that need to be added or updated

## 3. Implement the Fix
- Work through each todo item
- Write clean, minimal code changes
- Add or update tests as needed
- Check CLAUDE.md for project-specific build/test commands
- Run tests to verify the fix

## 4. Code Review
- Make a commit with the changes
- Use the Task tool with `subagent_type='code-reviewer'` to perform an autonomous code review
- The code-reviewer agent will analyze the changes for:
  - Code correctness and logic errors
  - Security vulnerabilities
  - Error handling gaps
  - Edge cases
  - Adherence to project conventions (check CLAUDE.md)
  - Test coverage
- Address any issues raised by the code-reviewer before proceeding
- If the review identifies significant problems, iterate on the implementation

## 5. Finish Your Work

When your implementation is complete and ready for human review:

1. **Commit your implementation changes** with a descriptive commit message
2. **Push the branch** to the remote repository
3. Write `PR_DESCRIPTION.md` in the root of the repository with this format:
   ```markdown
   ## Summary
   - Key change 1
   - Key change 2

   ## Test plan
   - How you tested this
   - Commands run: cargo test, just check, etc.

   ## Notes
   - Context reviewers should know
   - Follow-up work if any
   ```

**DO NOT commit PR_DESCRIPTION.md** - Gru will read this file locally from your worktree, use it to create the PR description, mark the PR ready, and then delete it automatically.

**IMPORTANT:** Only write `PR_DESCRIPTION.md` when work is truly complete and ready for human review. If work is still in progress, don't create this file - Gru will create a draft PR instead.

## 6. Iterate on Feedback
- Look at CI check results
- Address any issues raised by the CI checks
- Read review comments
- Determine which comments require changes. Sometimes reviewers are wrong!
- Make the necessary changes
- For any comments that you've determined don't require changes, acknowledge them
- Make a reply that addresses each comment and includes a summary of the changes made
- Repeat until the PR is ready to merge
"#,
            ctx.issue_num,
            details.title,
            ctx.owner,
            ctx.repo,
            ctx.issue_num,
            labels_section,
            details.body
        )
    } else {
        format!("/fix {}", ctx.issue_num)
    }
}

/// Runs a Claude session with stream monitoring and progress tracking.
///
/// Spawns the Claude CLI, tracks progress, records PID in registry,
/// and cleans up on exit (success or failure).
async fn run_claude_session(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    quiet: bool,
    timeout_opt: Option<&str>,
) -> Result<ClaudeResult> {
    println!("🤖 Launching Claude...\n");

    let prompt = build_fix_prompt(issue_ctx);

    let mut cmd = build_claude_command(&wt_ctx.worktree_path, &wt_ctx.session_id, &prompt);
    cmd.env("GRU_WORKSPACE", &wt_ctx.minion_id);

    let config = ProgressConfig {
        minion_id: wt_ctx.minion_id.clone(),
        issue: issue_ctx.issue_num.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    let mut progress_tracker = ProgressCommentTracker::new(wt_ctx.minion_id.clone());

    struct CallbackState<'a> {
        raw_output_buffer: String,
        progress: &'a ProgressDisplay,
        progress_tracker: &'a mut ProgressCommentTracker,
    }

    let mut callback_state = CallbackState {
        raw_output_buffer: String::new(),
        progress: &progress,
        progress_tracker: &mut progress_tracker,
    };

    let callback = |output: &stream::StreamOutput| {
        if let stream::StreamOutput::RawLine(ref line) = output {
            callback_state.raw_output_buffer.push_str(line);
            if callback_state.raw_output_buffer.len() > MAX_OUTPUT_BUFFER_SIZE {
                let mut trim_pos = TRIM_OUTPUT_BUFFER_SIZE;
                while trim_pos > 0 && !callback_state.raw_output_buffer.is_char_boundary(trim_pos) {
                    trim_pos -= 1;
                }
                callback_state.raw_output_buffer =
                    callback_state.raw_output_buffer.split_off(trim_pos);
            }
        }

        callback_state.progress.handle_output(output);

        if let stream::StreamOutput::RawLine(ref line) = output {
            let previous_phase = callback_state.progress_tracker.current_phase();

            if line.contains("Plan") || line.contains("plan") {
                if previous_phase != MinionPhase::Planning {
                    callback_state
                        .progress_tracker
                        .set_phase(MinionPhase::Planning);
                }
            } else if (line.contains("Implement") || line.contains("Writing"))
                && previous_phase != MinionPhase::Implementing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Implementing);
            } else if (line.contains("test") || line.contains("Test"))
                && previous_phase != MinionPhase::Testing
            {
                callback_state
                    .progress_tracker
                    .set_phase(MinionPhase::Testing);
            }
        }
    };

    let pid_minion_id = wt_ctx.minion_id.clone();
    let on_spawn: Box<dyn FnOnce(u32) + Send> = Box::new(move |pid: u32| {
        if let Ok(mut registry) = MinionRegistry::load(None) {
            let _ = registry.update(&pid_minion_id, |info| {
                info.pid = Some(pid);
                info.last_activity = Utc::now();
            });
        }
    });

    let run_result = run_claude_with_stream_monitoring(
        cmd,
        &wt_ctx.worktree_path,
        timeout_opt,
        Some(callback),
        Some(on_spawn),
    )
    .await;

    // Always clear PID and set mode to Stopped, regardless of success or error
    let exit_minion_id = wt_ctx.minion_id.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(mut registry) = MinionRegistry::load(None) {
            let _ = registry.update(&exit_minion_id, |info| {
                info.pid = None;
                info.mode = MinionMode::Stopped;
            });
        }
    })
    .await
    .context("Failed to update registry after process exit")?;

    let status = run_result?;

    // Post final completion comment
    if let Some(ref client) = issue_ctx.github_client {
        progress_tracker.set_phase(MinionPhase::Completed);

        let final_message = if status.success() {
            "✅ Task completed successfully!".to_string()
        } else {
            "❌ Task failed.".to_string()
        };

        let update = progress_tracker.create_update(final_message);
        let comment_body = update.format_comment();

        try_post_progress_comment(
            client,
            &issue_ctx.owner,
            &issue_ctx.repo,
            issue_ctx.issue_num,
            &comment_body,
        )
        .await;
    }

    // Finish the progress display
    if status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue_ctx.issue_num));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue_ctx.issue_num));
    }

    Ok(ClaudeResult { status })
}

// ---------------------------------------------------------------------------
// Phase 4: Create PR
// ---------------------------------------------------------------------------

/// Creates a PR for the branch and updates labels/registry.
/// Returns the PR number if successful.
async fn handle_pr_creation(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
) -> Result<Option<String>> {
    println!("\n🔍 Checking if branch was pushed...");
    let branch_pushed = is_branch_pushed(&wt_ctx.worktree_path, &wt_ctx.branch_name).await?;

    if !branch_pushed {
        println!("ℹ️  Branch was not pushed. No PR will be created.");
        println!(
            "   Push your changes with: git push origin {}",
            wt_ctx.branch_name
        );
        return Ok(None);
    }

    println!("📋 Branch was pushed, creating pull request...");

    let issue_title_cached = issue_ctx.details.as_ref().map(|d| d.title.as_str());

    match create_pr_for_issue(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        issue_ctx.issue_num,
        &wt_ctx.minion_id,
        &wt_ctx.worktree_path,
        issue_title_cached,
    )
    .await
    {
        Ok(pr_number) => {
            // Save PR state
            let pr_state = PrState::new(pr_number.clone(), issue_ctx.issue_num.to_string());
            pr_state
                .save(&wt_ctx.worktree_path)
                .context("Failed to save PR state")?;

            // Update registry with PR number
            let minion_id_clone = wt_ctx.minion_id.clone();
            let pr_number_clone = pr_number.clone();
            tokio::task::spawn_blocking(move || {
                let mut registry = MinionRegistry::load(None)?;
                registry.update(&minion_id_clone, |info| {
                    info.pr = Some(pr_number_clone);
                    info.status = "idle".to_string();
                })
            })
            .await
            .context("Failed to spawn blocking task for registry update")??;

            println!("✅ Draft PR created: #{}", pr_number);
            println!(
                "🔗 View PR at: https://github.com/{}/{}/pull/{}",
                issue_ctx.owner, issue_ctx.repo, pr_number
            );

            // Mark issue as done (fire-and-forget)
            if let Some(ref client) = issue_ctx.github_client {
                match client
                    .mark_issue_done(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num)
                    .await
                {
                    Ok(()) => {
                        println!("🏷️  Updated issue label to 'minion:done'");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to update issue label: {}", e);
                    }
                }
            }

            Ok(Some(pr_number))
        }
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("already exists") || err_msg.contains("A pull request for branch") {
                log::info!(
                    "ℹ️  A PR already exists for branch '{}'",
                    wt_ctx.branch_name
                );
                log::info!(
                    "   Check: https://github.com/{}/{}/pulls",
                    issue_ctx.owner,
                    issue_ctx.repo
                );
            } else if err_msg.contains("branch not found") || err_msg.contains("does not exist") {
                log::warn!("⚠️  Branch was pushed but is no longer available.");
                log::warn!("   It may have been deleted or force-pushed.");
            } else {
                log::warn!("⚠️  Failed to create PR: {}", e);
            }
            log::warn!("   You can create the PR manually if needed.");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 5: Monitor PR lifecycle
// ---------------------------------------------------------------------------

/// Monitors a PR for reviews, CI failures, and merge/close events.
/// Handles automatic review rounds up to MAX_REVIEW_ROUNDS.
async fn monitor_pr_lifecycle(
    issue_ctx: &IssueContext,
    wt_ctx: &WorktreeContext,
    pr_number: &str,
    timeout_opt: Option<&str>,
) {
    // Auto-trigger review for Minion-created PRs
    println!("\n🔍 Starting automated PR review...");
    match trigger_pr_review(pr_number, &wt_ctx.worktree_path, None).await {
        Ok(review_exit_code) => {
            if review_exit_code == 0 {
                println!("✅ PR review completed successfully");
            } else {
                log::warn!(
                    "⚠️  PR review completed with exit code: {}",
                    review_exit_code
                );
            }
        }
        Err(e) => {
            log::warn!("⚠️  Failed to run PR review: {}", e);
            log::warn!("   You can review manually with: gru review {}", pr_number);
        }
    }

    // Start monitoring the PR for review comments, CI failures, and merge/close events
    println!("\n👀 Monitoring PR for updates (polling every 30s)...");
    println!("   Press Ctrl+C to stop monitoring\n");

    let mut review_round = 0;
    loop {
        match pr_monitor::monitor_pr(
            &issue_ctx.owner,
            &issue_ctx.repo,
            pr_number,
            &wt_ctx.worktree_path,
        )
        .await
        {
            Ok(MonitorResult::Merged) => {
                println!("✅ PR #{} was merged successfully!", pr_number);
                println!("🎉 Issue {} is complete!", issue_ctx.issue_num);
                break;
            }
            Ok(MonitorResult::Closed) => {
                println!("⚠️  PR #{} was closed without merging", pr_number);
                println!("   The issue may need to be reopened or addressed differently");
                break;
            }
            Ok(MonitorResult::NewReviews(comments)) => {
                review_round += 1;
                let count = comments.len();
                println!(
                    "💬 Detected {} new review comment(s) on PR #{} (review round {}/{})",
                    count, pr_number, review_round, MAX_REVIEW_ROUNDS
                );

                if review_round > MAX_REVIEW_ROUNDS {
                    println!(
                        "⚠️  Reached maximum review rounds limit ({})",
                        MAX_REVIEW_ROUNDS
                    );
                    println!("   Additional reviews will need manual handling");
                    println!(
                        "   View PR: https://github.com/{}/{}/pull/{}",
                        issue_ctx.owner, issue_ctx.repo, pr_number
                    );
                    break;
                }

                let review_prompt =
                    pr_monitor::format_review_prompt(issue_ctx.issue_num, pr_number, &comments);

                println!("🔄 Re-invoking to address review feedback...\n");

                match invoke_claude_for_reviews(
                    &wt_ctx.worktree_path,
                    &wt_ctx.session_id,
                    &review_prompt,
                    timeout_opt,
                )
                .await
                {
                    Ok(()) => {
                        println!("\n✅ Finished addressing review comments");
                        println!("🔄 Continuing to monitor PR...\n");
                    }
                    Err(e) => {
                        log::warn!("⚠️  Failed to address review comments: {}", e);
                        log::warn!("   You can address them manually");
                        break;
                    }
                }
            }
            Ok(MonitorResult::FailedChecks(count)) => {
                println!(
                    "❌ Detected {} failed CI check(s) on PR #{}",
                    count, pr_number
                );
                println!(
                    "   Review the checks at: https://github.com/{}/{}/pull/{}/checks",
                    issue_ctx.owner, issue_ctx.repo, pr_number
                );
                println!("   Fix issues and push updates to the branch");
                break;
            }
            Err(e) => {
                log::warn!("⚠️  PR monitoring failed: {}", e);
                log::warn!(
                    "   You can monitor manually at: https://github.com/{}/{}/pull/{}",
                    issue_ctx.owner,
                    issue_ctx.repo,
                    pr_number
                );
                break;
            }
        }
    }
}

/// Monitors CI after the initial fix and attempts auto-fixes if checks fail.
/// Returns Ok(true) if CI passed, Ok(false) if escalated/failed.
async fn monitor_ci_after_fix(
    owner: &str,
    repo: &str,
    branch: &str,
    worktree_path: &Path,
) -> Result<bool> {
    let pr_number = match ci::get_pr_number(owner, repo, branch).await? {
        Some(num) => num,
        None => {
            eprintln!(
                "ℹ️  No PR found for branch {}, skipping CI monitoring",
                branch
            );
            return Ok(true);
        }
    };

    eprintln!("🔍 Monitoring CI for PR #{}", pr_number);
    ci::monitor_and_fix_ci(owner, repo, pr_number, branch, worktree_path).await
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Handles the fix command by delegating to the Claude CLI.
/// Returns the exit code from the claude process.
///
/// Orchestrates 5 phases:
/// 1. `resolve_issue` - Parse issue, check duplicates, fetch details
/// 2. `setup_worktree` - Clone repo, create worktree, register minion
/// 3. `run_claude_session` - Build prompt, run Claude, track progress
/// 4. `handle_pr_creation` - Push check, create PR, update labels
/// 5. `monitor_pr_lifecycle` - Review, poll for updates, handle feedback
pub async fn handle_fix(
    issue: &str,
    timeout_opt: Option<String>,
    quiet: bool,
    force_new: bool,
) -> Result<i32> {
    // Phase 1: Resolve issue
    let issue_ctx = match resolve_issue(issue, force_new).await {
        Ok(ctx) => ctx,
        Err(e) if e.to_string() == "existing_minion_found" => return Ok(1),
        Err(e) => return Err(e),
    };

    // Phase 2: Setup worktree
    let wt_ctx = setup_worktree(&issue_ctx).await?;

    // Phase 3: Run Claude
    let claude_result =
        run_claude_session(&issue_ctx, &wt_ctx, quiet, timeout_opt.as_deref()).await?;

    if !claude_result.status.success() {
        // Mark issue as failed (fire-and-forget)
        if let Some(ref client) = issue_ctx.github_client {
            match client
                .mark_issue_failed(&issue_ctx.owner, &issue_ctx.repo, issue_ctx.issue_num)
                .await
            {
                Ok(()) => {
                    println!("🏷️  Updated issue label to 'minion:failed'");
                }
                Err(e) => {
                    log::warn!("⚠️  Failed to update issue label: {}", e);
                }
            }
        }

        return Ok(claude_result
            .status
            .code()
            .unwrap_or(EXIT_CODE_SIGNAL_TERMINATED));
    }

    // Phase 4: Create PR
    let pr_number = handle_pr_creation(&issue_ctx, &wt_ctx).await?;

    // Phase 5: Monitor PR lifecycle (review + polling)
    if let Some(ref pr_num) = pr_number {
        monitor_pr_lifecycle(&issue_ctx, &wt_ctx, pr_num, timeout_opt.as_deref()).await;
    }

    // CI monitoring
    let ci_passed = monitor_ci_after_fix(
        &issue_ctx.owner,
        &issue_ctx.repo,
        &wt_ctx.branch_name,
        &wt_ctx.worktree_path,
    )
    .await;
    match ci_passed {
        Ok(true) => log::info!("✅ CI checks passed"),
        Ok(false) => log::warn!("⚠️  CI checks failed or were escalated"),
        Err(e) => log::warn!("⚠️  CI monitoring error (non-fatal): {}", e),
    }

    Ok(claude_result
        .status
        .code()
        .unwrap_or(EXIT_CODE_SIGNAL_TERMINATED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_is_branch_pushed_nonexistent() {
        use std::env;

        // Test with a non-existent directory
        let temp_dir = env::temp_dir().join("gru-test-nonexistent");
        let result = is_branch_pushed(&temp_dir, "test-branch").await;

        // Git command will fail, but we return Ok(false) to indicate branch is not pushed
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_build_fix_prompt_with_details() {
        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 42,
            details: Some(IssueDetails {
                title: "Fix the widget".to_string(),
                body: "The widget is broken".to_string(),
                labels: "bug, priority:high".to_string(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx);
        assert!(prompt.contains("# Issue #42: Fix the widget"));
        assert!(prompt.contains("octocat/hello-world/issues/42"));
        assert!(prompt.contains("The widget is broken"));
        assert!(prompt.contains("Labels: bug, priority:high"));
        assert!(prompt.contains("## 1. Check if Decomposition is Needed"));
    }

    #[test]
    fn test_build_fix_prompt_without_details() {
        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 42,
            details: None,
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx);
        assert_eq!(prompt, "/fix 42");
    }

    #[test]
    fn test_build_fix_prompt_empty_labels() {
        let ctx = IssueContext {
            owner: "octocat".to_string(),
            repo: "hello-world".to_string(),
            issue_num: 7,
            details: Some(IssueDetails {
                title: "Add feature".to_string(),
                body: "Please add this feature".to_string(),
                labels: String::new(),
            }),
            github_client: None,
        };

        let prompt = build_fix_prompt(&ctx);
        assert!(prompt.contains("# Issue #7: Add feature"));
        assert!(!prompt.contains("Labels:"));
    }

    #[test]
    fn test_create_wip_template() {
        let (title, body) = create_wip_template("M042", 123, "Fix login bug");
        assert_eq!(title, "[M042] Fixes #123: Fix login bug");
        assert!(body.contains("Automated fix for #123 by Minion M042"));
        assert!(body.contains("- [ ] Implementation"));
    }

    #[test]
    fn test_issue_context_typed_issue_num() {
        // Verify that IssueContext uses u64, not String
        let ctx = IssueContext {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            issue_num: 42,
            details: None,
            github_client: None,
        };
        // The issue_num is already a u64 - no parsing needed
        assert_eq!(ctx.issue_num, 42u64);
    }
}
