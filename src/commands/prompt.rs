use crate::claude_runner::{
    build_claude_command, run_claude_with_stream_monitoring, EXIT_CODE_SIGNAL_TERMINATED,
};
use crate::minion;
use crate::minion_registry::{MinionInfo as RegistryMinionInfo, MinionMode, MinionRegistry};
use crate::progress::{ProgressConfig, ProgressDisplay};
use crate::stream;
use crate::workspace;
use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

/// Handles the prompt command by launching Claude with an ad-hoc prompt
/// Returns the exit code from the claude process
pub async fn handle_prompt(prompt: &str, timeout_opt: Option<String>, quiet: bool) -> Result<i32> {
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

    // Generate a unique minion ID for session tracking
    let minion_id = minion::generate_minion_id().context("Failed to generate Minion ID")?;
    println!("🆔 Session: {}", minion_id);

    // Initialize workspace
    let workspace = workspace::Workspace::new().context("Failed to initialize Gru workspace")?;

    // Create workspace path for ad-hoc prompts: ~/.gru/work/ad-hoc/<minion-id>/
    // Note: We use "ad-hoc" as a pseudo-repo name and minion_id as the branch name
    // to leverage the existing work_dir validation (path traversal protection, etc.)
    let workspace_path = workspace
        .work_dir("ad-hoc", &minion_id)
        .context("Failed to compute workspace path")?;

    // Create the workspace directory
    tokio::fs::create_dir_all(&workspace_path)
        .await
        .context("Failed to create workspace directory")?;

    // Save prompt to file for debugging and audit trail
    let prompt_file = workspace_path.join("prompt.txt");
    tokio::fs::write(&prompt_file, prompt)
        .await
        .context("Failed to save prompt to workspace")?;

    println!("📂 Workspace: {}", workspace_path.display());

    // Register minion in registry
    // The "ad-hoc" repo name is a special reserved value used in the MinionRegistry
    // to represent prompt minions that are not associated with any real repository.
    // This allows us to leverage the existing registry and workspace mechanisms for
    // tracking, displaying, and managing prompt-based minions, while clearly
    // distinguishing them from repo-based minions. Any code that filters, displays,
    // or processes minions by repo should be aware that "ad-hoc" is a special case
    // and may require different handling (e.g., prompts have no issues, branches, or PRs).
    let now = Utc::now();
    let registry_info = RegistryMinionInfo {
        repo: "ad-hoc".to_string(), // Special reserved value for prompt minions
        issue: 0,                   // Prompts don't have issues
        command: "prompt".to_string(),
        prompt: prompt.to_string(),
        started_at: now,
        branch: String::new(), // Prompts don't have branches
        worktree: workspace_path.clone(),
        status: "active".to_string(),
        pr: None,                      // Prompts don't have PRs
        session_id: minion_id.clone(), // Prompts use minion_id as session_id
        pid: None,
        mode: MinionMode::Autonomous,
        last_activity: now,
    };

    let mut registry = MinionRegistry::load(None).context("Failed to load Minion registry")?;
    registry
        .register(minion_id.clone(), registry_info)
        .context("Failed to register prompt Minion in registry")?;

    // Generate a unique session ID (UUID) for Claude's --session-id flag
    let session_id = Uuid::new_v4();

    println!("🤖 Launching Claude...\n");

    // Create progress display
    let config = ProgressConfig {
        minion_id: minion_id.clone(),
        issue: format!("ad-hoc: {}", prompt),
        quiet,
    };
    let progress = ProgressDisplay::new(config);

    // Get current working directory to pass to Claude
    let cwd = std::env::current_dir().context("Failed to get current working directory")?;

    // Build the command with flags for non-interactive stream-json output
    let mut cmd = build_claude_command(&cwd, &session_id, prompt);
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
    let output_callback = move |output: &stream::StreamOutput| {
        progress.handle_output(output);
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
        println!("\n✅ Task completed");
        println!("\n📁 Session workspace: {}", workspace_path.display());
        println!("💡 To resume this session, use: gru resume {}", minion_id);
        Ok(0)
    } else {
        let exit_code = status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);
        println!("\n❌ Task failed (exit code: {})", exit_code);
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
        let result = handle_prompt("--help", None, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot start with '-'"));
    }

    #[tokio::test]
    async fn test_handle_prompt_rejects_empty_input() {
        let result = handle_prompt("", None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));

        let result = handle_prompt("   ", None, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }
}
