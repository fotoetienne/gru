use crate::git;
use crate::minion;
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::stream::EventStream;
use crate::url_utils::parse_issue_info;
use crate::workspace;
use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Timeout in seconds for each line read from Claude's output stream
/// Set to 5 minutes to accommodate long-running LLM operations
const STREAM_TIMEOUT_SECS: u64 = 300;

/// Handles the fix command by delegating to the Claude CLI
/// Returns the exit code from the claude process
pub async fn handle_fix(issue: &str, quiet: bool) -> Result<i32> {
    // Parse issue information (auto-detects repo from current directory if plain number)
    let (owner, repo, issue_num) = parse_issue_info(issue)?;

    // Always generate a unique minion ID
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("📋 Generated Minion ID: {}", minion_id);

    // Create workspace and launch Claude
    println!(
        "🚀 Setting up workspace for {}/{}#{}",
        owner, repo, issue_num
    );

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create bare repository path
    let bare_path = workspace.repos().join(&owner).join(format!("{}.git", repo));
    let git_repo = git::GitRepo::new(&owner, &repo, bare_path);

    // Ensure bare repository is cloned/updated
    println!("📦 Ensuring repository is cloned...");
    git_repo
        .ensure_bare_clone()
        .context("Failed to clone or update repository")?;

    // Create worktree path
    let repo_name = format!("{}/{}", owner, repo);
    let worktree_path = workspace
        .work_dir(&repo_name, &minion_id)
        .context("Failed to compute worktree path")?;

    // Create worktree with branch name: minion/issue-<num>-<id>
    let branch_name = format!("minion/issue-{}-{}", issue_num, minion_id);
    println!("🌿 Creating worktree with branch: {}", branch_name);

    git_repo
        .create_worktree(&branch_name, &worktree_path)
        .context("Failed to create worktree")?;

    println!("📂 Workspace created at: {}", worktree_path.display());
    println!("🤖 Launching Claude...\n");

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: issue.to_string(),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = Command::new("claude");
    cmd.arg("--print")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions")
        .arg(format!("/fix {}", issue_num))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .current_dir(&worktree_path)
        .env("GRU_WORKSPACE", &minion_id);

    // Spawn the command
    let mut child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    // Get the stdout handle
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture stdout from claude process")?;

    // Create event stream reader
    let mut stream = EventStream::from_stdout(stdout);

    // Process stream output asynchronously with timeout and error handling
    let stream_result = async {
        loop {
            // Handle timeout first, then flatten the stream result
            let line_result = timeout(Duration::from_secs(STREAM_TIMEOUT_SECS), stream.next_line())
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Timeout: Claude process hasn't produced output in {} seconds",
                        STREAM_TIMEOUT_SECS
                    )
                })?;

            // Now handle the stream result
            match line_result? {
                Some(output) => progress.handle_output(&output),
                None => break, // Stream ended normally
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Always wait for the process, regardless of stream errors
    let status = child.wait().await?;

    // Now check if there was a stream error
    stream_result?;

    // Finish the progress display
    if status.success() {
        progress.finish_with_message(&format!("✅ Completed issue {}", issue));
    } else {
        progress.finish_with_message(&format!("❌ Failed to fix issue {}", issue));
    }

    // Return the exit code from the claude process
    Ok(status.code().unwrap_or(128))
}
