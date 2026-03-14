use crate::github;
use crate::labels;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

/// Maximum number of auto-fix attempts before escalating to human
pub const MAX_CI_FIX_ATTEMPTS: u32 = 2;

/// Polling interval for checking CI status (30 seconds)
const CI_POLL_INTERVAL_SECS: u64 = 30;

/// Maximum time to wait for CI checks to appear (5 minutes)
const CI_WAIT_TIMEOUT_SECS: u64 = 300;

/// Maximum time to wait for CI checks to complete (30 minutes)
const CI_COMPLETION_TIMEOUT_SECS: u64 = 1800;

/// Delay after pushing before polling CI, to allow GitHub to register checks (60s)
const POST_PUSH_DELAY_SECS: u64 = 60;

/// Maximum time for a single Claude CI fix invocation (20 minutes)
const CI_FIX_TIMEOUT_SECS: u64 = 1200;

/// The status of a CI check run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

/// The conclusion of a completed check run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Cancelled,
    TimedOut,
    ActionRequired,
    Neutral,
    Skipped,
    Stale,
}

impl fmt::Display for CheckConclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckConclusion::Success => write!(f, "success"),
            CheckConclusion::Failure => write!(f, "failure"),
            CheckConclusion::Cancelled => write!(f, "cancelled"),
            CheckConclusion::TimedOut => write!(f, "timed_out"),
            CheckConclusion::ActionRequired => write!(f, "action_required"),
            CheckConclusion::Neutral => write!(f, "neutral"),
            CheckConclusion::Skipped => write!(f, "skipped"),
            CheckConclusion::Stale => write!(f, "stale"),
        }
    }
}

/// Represents a CI check run from GitHub
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    /// Duration string from GitHub (e.g., "2m 34s")
    pub duration: Option<String>,
    /// Failure output/logs if available
    pub output: Option<String>,
}

/// The type of CI failure for classification
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FailureType {
    TestFailure,
    BuildError,
    LintError,
    FormatError,
    Timeout,
    Other(String),
}

impl fmt::Display for FailureType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureType::TestFailure => write!(f, "test failure"),
            FailureType::BuildError => write!(f, "build error"),
            FailureType::LintError => write!(f, "lint error"),
            FailureType::FormatError => write!(f, "format error"),
            FailureType::Timeout => write!(f, "timeout"),
            FailureType::Other(s) => write!(f, "{}", s),
        }
    }
}

/// Overall result of waiting for CI checks
#[derive(Debug)]
pub enum CiResult {
    /// All checks passed
    AllPassed,
    /// Some checks failed
    Failed(Vec<CheckRun>),
    /// No checks found (might not have CI configured)
    NoChecks,
    /// Timed out waiting for checks
    Timeout,
}

/// Returns the last `max_bytes` of a string, aligned to a UTF-8 char boundary.
/// Safe for all UTF-8 content (won't panic on multi-byte characters).
fn safe_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = s.len() - max_bytes;
    // Advance to the next valid char boundary
    match (start..s.len()).find(|&i| s.is_char_boundary(i)) {
        Some(boundary) => &s[boundary..],
        None => "",
    }
}

/// Classifies a CI failure based on the check name and output
pub fn classify_failure(check: &CheckRun) -> FailureType {
    let name_lower = check.name.to_lowercase();
    let output_lower = check.output.as_deref().unwrap_or("").to_lowercase();

    if check.conclusion == Some(CheckConclusion::TimedOut) {
        return FailureType::Timeout;
    }

    // Check output content first (more specific)
    if output_lower.contains("test")
        && (output_lower.contains("failed") || output_lower.contains("failure"))
    {
        return FailureType::TestFailure;
    }
    if output_lower.contains("error[e")
        || output_lower.contains("cannot find")
        || output_lower.contains("compilation")
    {
        return FailureType::BuildError;
    }
    if output_lower.contains("clippy")
        || (output_lower.contains("warning") && output_lower.contains("deny"))
    {
        return FailureType::LintError;
    }
    if output_lower.contains("rustfmt")
        || output_lower.contains("formatting")
        || output_lower.contains("fmt")
    {
        return FailureType::FormatError;
    }

    // Fall back to check name
    if name_lower.contains("test") {
        return FailureType::TestFailure;
    }
    if name_lower.contains("build") || name_lower.contains("compile") {
        return FailureType::BuildError;
    }
    if name_lower.contains("lint") || name_lower.contains("clippy") {
        return FailureType::LintError;
    }
    if name_lower.contains("fmt") || name_lower.contains("format") {
        return FailureType::FormatError;
    }

    FailureType::Other(name_lower)
}

/// Builds the CI failure prompt for Claude to fix the issue
pub fn build_ci_fix_prompt(failed_checks: &[CheckRun], attempt: u32) -> String {
    let mut prompt = format!(
        "Your PR's CI checks failed (attempt {}/{}). Please analyze the failure and fix it.\n\n",
        attempt, MAX_CI_FIX_ATTEMPTS
    );

    for check in failed_checks {
        let failure_type = classify_failure(check);
        let conclusion = check
            .conclusion
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let duration = check.duration.as_deref().unwrap_or("unknown");

        prompt.push_str(&format!(
            "## Failed Check\n\
             **Name:** {}\n\
             **Type:** {}\n\
             **Conclusion:** {}\n\
             **Duration:** {}\n\n",
            check.name, failure_type, conclusion, duration
        ));

        if let Some(output) = &check.output {
            // Truncate very long output to avoid overwhelming the prompt
            let truncated = if output.len() > 10_000 {
                format!(
                    "... (truncated, showing last ~10000 bytes) ...\n{}",
                    safe_tail(output, 10_000)
                )
            } else {
                output.clone()
            };

            prompt.push_str(&format!("## Failure Output\n```\n{}\n```\n\n", truncated));
        }
    }

    prompt.push_str(
        "Please fix the failing checks. Run the relevant checks locally before committing:\n",
    );
    prompt.push_str("- For test failures: `just test`\n");
    prompt.push_str("- For build errors: `just build`\n");
    prompt.push_str("- For lint errors: `just lint` or `just fix-clippy`\n");
    prompt.push_str("- For format errors: `just fmt`\n");
    prompt.push_str("\nAfter fixing, commit and push the changes.\n");

    prompt
}

/// Fetches check runs for a given PR ref using the gh CLI.
/// Returns a list of CheckRun structs parsed from the gh CLI output.
pub async fn fetch_check_runs(owner: &str, repo: &str, git_ref: &str) -> Result<Vec<CheckRun>> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = github::gh_command_for_repo(&repo_full);

    let output = Command::new(gh_cmd)
        .args([
            "api",
            &format!("repos/{}/commits/{}/check-runs", repo_full, git_ref),
            "--jq",
            ".check_runs[] | {name: .name, status: .status, conclusion: .conclusion, started_at: .started_at, completed_at: .completed_at, output_title: .output.title, output_summary: .output.summary, output_text: .output.text}",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to execute gh CLI for check runs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch check runs: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut checks = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let json: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("Failed to parse check run JSON: {}", line))?;

        let status = match json["status"].as_str().unwrap_or("") {
            "queued" => CheckStatus::Queued,
            "in_progress" => CheckStatus::InProgress,
            "completed" => CheckStatus::Completed,
            _ => continue,
        };

        let conclusion = json["conclusion"].as_str().and_then(|c| match c {
            "success" => Some(CheckConclusion::Success),
            "failure" => Some(CheckConclusion::Failure),
            "cancelled" => Some(CheckConclusion::Cancelled),
            "timed_out" => Some(CheckConclusion::TimedOut),
            "action_required" => Some(CheckConclusion::ActionRequired),
            "neutral" => Some(CheckConclusion::Neutral),
            "skipped" => Some(CheckConclusion::Skipped),
            "stale" => Some(CheckConclusion::Stale),
            _ => None,
        });

        // Build output from available fields
        let mut output_parts = Vec::new();
        if let Some(title) = json["output_title"].as_str() {
            if !title.is_empty() {
                output_parts.push(title.to_string());
            }
        }
        if let Some(summary) = json["output_summary"].as_str() {
            if !summary.is_empty() {
                output_parts.push(summary.to_string());
            }
        }
        if let Some(text) = json["output_text"].as_str() {
            if !text.is_empty() {
                output_parts.push(text.to_string());
            }
        }

        let output_str = if output_parts.is_empty() {
            None
        } else {
            Some(output_parts.join("\n\n"))
        };

        // Calculate duration from timestamps
        let duration = compute_duration(json["started_at"].as_str(), json["completed_at"].as_str());

        checks.push(CheckRun {
            name: json["name"].as_str().unwrap_or("unknown").to_string(),
            status,
            conclusion,
            duration,
            output: output_str,
        });
    }

    Ok(checks)
}

/// Fetches the workflow run logs for a failed check to get more detailed output.
/// Falls back gracefully if logs aren't available.
pub async fn fetch_check_logs(
    owner: &str,
    repo: &str,
    check_name: &str,
    pr_number: u64,
    branch: &str,
) -> Result<Option<String>> {
    // Use gh to get the workflow run associated with this PR
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = github::gh_command_for_repo(&repo_full);

    let output = Command::new(gh_cmd)
        .args([
            "run",
            "list",
            "--repo",
            &repo_full,
            "--branch",
            branch,
            "--json",
            "databaseId,name,conclusion,headBranch",
            "--limit",
            "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to list workflow runs")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let runs: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();

    // Find the failed run matching our check
    for run in &runs {
        let name = run["name"].as_str().unwrap_or("");
        let conclusion = run["conclusion"].as_str().unwrap_or("");

        if name.to_lowercase().contains(&check_name.to_lowercase()) && conclusion == "failure" {
            if let Some(run_id) = run["databaseId"].as_u64() {
                // Try to get the logs for this run
                let log_output = Command::new(gh_cmd)
                    .args([
                        "run",
                        "view",
                        &run_id.to_string(),
                        "--repo",
                        &repo_full,
                        "--log-failed",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await;

                if let Ok(log_result) = log_output {
                    if log_result.status.success() {
                        let logs = String::from_utf8_lossy(&log_result.stdout).to_string();
                        if !logs.is_empty() {
                            return Ok(Some(logs));
                        }
                    }
                }
            }
        }
    }

    // Also try using gh pr checks to get status info
    let pr_output = Command::new(gh_cmd)
        .args(["pr", "checks", &pr_number.to_string(), "--repo", &repo_full])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    if let Ok(pr_result) = pr_output {
        if pr_result.status.success() || !pr_result.stdout.is_empty() {
            let checks_output = String::from_utf8_lossy(&pr_result.stdout).to_string();
            if !checks_output.is_empty() {
                return Ok(Some(checks_output));
            }
        }
    }

    Ok(None)
}

/// Waits for CI checks to complete on a PR, polling at regular intervals.
/// Returns the overall CI result.
pub async fn wait_for_ci(owner: &str, repo: &str, head_sha: &str) -> Result<CiResult> {
    let start = std::time::Instant::now();
    let wait_timeout = Duration::from_secs(CI_WAIT_TIMEOUT_SECS);
    let completion_timeout = Duration::from_secs(CI_COMPLETION_TIMEOUT_SECS);
    let poll_interval = Duration::from_secs(CI_POLL_INTERVAL_SECS);

    // Phase 1: Wait for checks to appear
    eprintln!("⏳ Waiting for CI checks to start...");
    let mut checks_found = false;

    loop {
        if start.elapsed() > wait_timeout && !checks_found {
            eprintln!(
                "⚠️  No CI checks found after {} seconds",
                CI_WAIT_TIMEOUT_SECS
            );
            return Ok(CiResult::NoChecks);
        }

        if start.elapsed() > completion_timeout {
            eprintln!(
                "⏱️  CI timeout after {} seconds",
                CI_COMPLETION_TIMEOUT_SECS
            );
            return Ok(CiResult::Timeout);
        }

        let checks = fetch_check_runs(owner, repo, head_sha).await?;

        if checks.is_empty() {
            sleep(poll_interval).await;
            continue;
        }

        if !checks_found {
            checks_found = true;
            eprintln!("🔄 CI checks detected ({}), monitoring...", checks.len());
        }

        // Check if all runs are completed
        let all_completed = checks.iter().all(|c| c.status == CheckStatus::Completed);

        if all_completed {
            let failed: Vec<CheckRun> = checks
                .into_iter()
                .filter(|c| {
                    c.conclusion != Some(CheckConclusion::Success)
                        && c.conclusion != Some(CheckConclusion::Skipped)
                        && c.conclusion != Some(CheckConclusion::Neutral)
                })
                .collect();

            if failed.is_empty() {
                return Ok(CiResult::AllPassed);
            } else {
                return Ok(CiResult::Failed(failed));
            }
        }

        // Show progress
        let completed = checks
            .iter()
            .filter(|c| c.status == CheckStatus::Completed)
            .count();
        let total = checks.len();
        eprintln!("  ⏳ {}/{} checks completed...", completed, total);

        sleep(poll_interval).await;
    }
}

/// Gets the HEAD SHA for a PR branch in a worktree
pub async fn get_head_sha(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to get HEAD SHA")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get HEAD SHA: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Gets the PR number associated with the current branch
pub async fn get_pr_number(owner: &str, repo: &str, branch: &str) -> Result<Option<u64>> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = github::gh_command_for_repo(&repo_full);

    let output = Command::new(gh_cmd)
        .args([
            "pr", "list", "--repo", &repo_full, "--head", branch, "--json", "number", "--limit",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to list PRs")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let prs: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();

    Ok(prs.first().and_then(|pr| pr["number"].as_u64()))
}

/// Invokes Claude Code to fix CI failures in the given worktree.
/// Returns the exit code from the Claude process.
pub async fn invoke_ci_fix(
    worktree_path: &Path,
    failed_checks: &[CheckRun],
    attempt: u32,
) -> Result<i32> {
    let prompt = build_ci_fix_prompt(failed_checks, attempt);

    let mut cmd = Command::new("claude");
    cmd.arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--dangerously-skip-permissions")
        .arg(&prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .current_dir(worktree_path);

    let child = cmd.spawn().context(
        "claude command not found. Install from: https://github.com/anthropics/claude-code",
    )?;

    let fix_timeout = Duration::from_secs(CI_FIX_TIMEOUT_SECS);
    let output = tokio::time::timeout(fix_timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Claude CI fix timed out after {} seconds",
                CI_FIX_TIMEOUT_SECS
            )
        })?
        .context("Failed to wait for Claude process")?;

    Ok(output.status.code().unwrap_or(128))
}

/// Posts an escalation comment on the PR when CI fixes are exhausted
pub async fn post_escalation_comment(
    owner: &str,
    repo: &str,
    pr_number: u64,
    failed_checks: &[CheckRun],
    attempts: u32,
) -> Result<()> {
    let mut body = format!(
        "## 🚨 CI Fix Escalation\n\n\
         Automated CI fix failed after **{}/{}** attempts. Human intervention required.\n\n\
         ### Failed Checks\n\n",
        attempts, MAX_CI_FIX_ATTEMPTS
    );

    for check in failed_checks {
        let failure_type = classify_failure(check);
        let conclusion = check
            .conclusion
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        body.push_str(&format!(
            "- **{}** — {} ({})\n",
            check.name, failure_type, conclusion
        ));

        if let Some(output) = &check.output {
            let truncated = safe_tail(output, 2000);
            body.push_str(&format!("  ```\n  {}\n  ```\n", truncated));
        }
    }

    body.push_str(&format!("\n**Labels:** `{}`\n", labels::BLOCKED));

    post_escalation_comment_body(owner, repo, pr_number, &body).await
}

/// Post an escalation comment when CI exhaustion occurs due to timeout or no-commits.
pub async fn post_exhaustion_escalation_comment(
    owner: &str,
    repo: &str,
    pr_number: u64,
    reason: &str,
    attempts: u32,
) -> Result<()> {
    let body = format!(
        "## 🚨 CI Fix Escalation\n\n\
         Automated CI fix failed after **{}/{}** attempts. Human intervention required.\n\n\
         ### Reason\n\n\
         {}\n\n\
         **Labels:** `{}`\n",
        attempts,
        MAX_CI_FIX_ATTEMPTS,
        reason,
        labels::BLOCKED
    );

    post_escalation_comment_body(owner, repo, pr_number, &body).await
}

/// Posts an escalation comment body to a PR and adds the blocked label.
async fn post_escalation_comment_body(
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> Result<()> {
    let repo_full = format!("{}/{}", owner, repo);
    let gh_cmd = github::gh_command_for_repo(&repo_full);

    let output = Command::new(gh_cmd)
        .args([
            "pr",
            "comment",
            &pr_number.to_string(),
            "--repo",
            &repo_full,
            "--body",
            body,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to post escalation comment")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to post escalation comment: {}", stderr);
    }

    // Add the blocked label
    let _ = Command::new(gh_cmd)
        .args([
            "pr",
            "edit",
            &pr_number.to_string(),
            "--repo",
            &repo_full,
            "--add-label",
            labels::BLOCKED,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    Ok(())
}

/// Main CI monitoring and fix loop.
///
/// After a PR is created/updated:
/// 1. Wait for CI checks to complete
/// 2. If checks fail, invoke Claude to fix
/// 3. Retry up to MAX_CI_FIX_ATTEMPTS times
/// 4. Escalate if all attempts fail
///
/// Returns Ok(true) if CI passed (possibly after fixes), Ok(false) if escalated.
pub async fn monitor_and_fix_ci(
    owner: &str,
    repo: &str,
    pr_number: u64,
    branch: &str,
    worktree_path: &Path,
) -> Result<bool> {
    for attempt in 1..=MAX_CI_FIX_ATTEMPTS {
        // Get the current HEAD SHA
        let head_sha = get_head_sha(worktree_path).await?;

        eprintln!(
            "\n🔍 CI monitoring (attempt {}/{}) for commit {}...",
            attempt,
            MAX_CI_FIX_ATTEMPTS,
            &head_sha[..8.min(head_sha.len())]
        );

        // Wait for CI to complete
        let ci_result = wait_for_ci(owner, repo, &head_sha).await?;

        match ci_result {
            CiResult::AllPassed => {
                eprintln!("✅ All CI checks passed!");
                return Ok(true);
            }
            CiResult::NoChecks => {
                eprintln!("ℹ️  No CI checks configured, skipping CI monitoring");
                return Ok(true);
            }
            CiResult::Timeout => {
                eprintln!("⏱️  CI checks timed out");
                if attempt == MAX_CI_FIX_ATTEMPTS {
                    eprintln!(
                        "🚨 Max fix attempts ({}) reached with CI timeout, escalating to human",
                        MAX_CI_FIX_ATTEMPTS
                    );
                    post_exhaustion_escalation_comment(
                        owner,
                        repo,
                        pr_number,
                        "CI checks timed out on all attempts.",
                        attempt,
                    )
                    .await
                    .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
                    return Ok(false);
                }
                continue;
            }
            CiResult::Failed(mut failed_checks) => {
                let check_names: Vec<&str> =
                    failed_checks.iter().map(|c| c.name.as_str()).collect();
                eprintln!("❌ CI checks failed: {}", check_names.join(", "));

                // Try to fetch more detailed logs for each failed check
                for check in &mut failed_checks {
                    if check.output.is_none() || check.output.as_deref() == Some("") {
                        if let Ok(Some(logs)) =
                            fetch_check_logs(owner, repo, &check.name, pr_number, branch).await
                        {
                            check.output = Some(logs);
                        }
                    }
                }

                if attempt == MAX_CI_FIX_ATTEMPTS {
                    // Last attempt failed, escalate
                    eprintln!(
                        "🚨 Max fix attempts ({}) reached, escalating to human",
                        MAX_CI_FIX_ATTEMPTS
                    );
                    post_escalation_comment(owner, repo, pr_number, &failed_checks, attempt)
                        .await
                        .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
                    return Ok(false);
                }

                // Invoke Claude to fix
                eprintln!(
                    "🔧 Invoking Claude to fix CI failures (attempt {}/{})...",
                    attempt, MAX_CI_FIX_ATTEMPTS
                );
                let exit_code = invoke_ci_fix(worktree_path, &failed_checks, attempt).await?;

                if exit_code != 0 {
                    eprintln!(
                        "⚠️  Claude fix attempt returned non-zero exit code: {}",
                        exit_code
                    );
                }

                // Check if Claude actually made new commits
                let new_sha = get_head_sha(worktree_path).await?;
                if new_sha == head_sha {
                    eprintln!("⚠️  Claude made no new commits, cannot retry");
                    if attempt < MAX_CI_FIX_ATTEMPTS {
                        continue;
                    }
                    eprintln!(
                        "🚨 Max fix attempts ({}) reached with no commits, escalating to human",
                        MAX_CI_FIX_ATTEMPTS
                    );
                    post_escalation_comment(owner, repo, pr_number, &failed_checks, attempt)
                        .await
                        .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
                    return Ok(false);
                }

                // Push the fix
                eprintln!("📤 Pushing CI fix...");
                let push_output = Command::new("git")
                    .args(["push", "origin", branch])
                    .current_dir(worktree_path)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await
                    .context("Failed to push CI fix")?;

                if !push_output.status.success() {
                    let stderr = String::from_utf8_lossy(&push_output.stderr);
                    eprintln!("⚠️  Push failed: {}", stderr);
                    break;
                }

                // Wait for GitHub to register checks for the new commit
                eprintln!(
                    "⏳ Waiting {}s for CI to register new checks...",
                    POST_PUSH_DELAY_SECS
                );
                sleep(Duration::from_secs(POST_PUSH_DELAY_SECS)).await;

                // Loop continues to check CI again
            }
        }
    }

    // Should not reach here, but handle gracefully
    Ok(false)
}

/// Computes a human-readable duration string from start/end timestamps
fn compute_duration(started_at: Option<&str>, completed_at: Option<&str>) -> Option<String> {
    let start = started_at?;
    let end = completed_at?;

    let start_dt = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let end_dt = chrono::DateTime::parse_from_rfc3339(end).ok()?;

    let duration = end_dt.signed_duration_since(start_dt);
    let total_secs = duration.num_seconds();

    if total_secs < 0 {
        return None;
    }

    let minutes = total_secs / 60;
    let seconds = total_secs % 60;

    if minutes > 0 {
        Some(format!("{}m {}s", minutes, seconds))
    } else {
        Some(format!("{}s", seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_test_failure_by_output() {
        let check = CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: Some("2m 34s".to_string()),
            output: Some("FAILED tests/test_auth.rs - test failed".to_string()),
        };
        assert_eq!(classify_failure(&check), FailureType::TestFailure);
    }

    #[test]
    fn test_classify_test_failure_by_name() {
        let check = CheckRun {
            name: "Test Suite".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        };
        assert_eq!(classify_failure(&check), FailureType::TestFailure);
    }

    #[test]
    fn test_classify_build_error_by_output() {
        let check = CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: Some("error[E0433]: failed to resolve: could not find `foo`".to_string()),
        };
        assert_eq!(classify_failure(&check), FailureType::BuildError);
    }

    #[test]
    fn test_classify_build_error_by_name() {
        let check = CheckRun {
            name: "Build".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        };
        assert_eq!(classify_failure(&check), FailureType::BuildError);
    }

    #[test]
    fn test_classify_lint_error_by_output() {
        let check = CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: Some("clippy::needless_return".to_string()),
        };
        assert_eq!(classify_failure(&check), FailureType::LintError);
    }

    #[test]
    fn test_classify_lint_error_by_name() {
        let check = CheckRun {
            name: "Clippy Lint".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        };
        assert_eq!(classify_failure(&check), FailureType::LintError);
    }

    #[test]
    fn test_classify_format_error_by_output() {
        let check = CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: Some("rustfmt check failed".to_string()),
        };
        assert_eq!(classify_failure(&check), FailureType::FormatError);
    }

    #[test]
    fn test_classify_timeout() {
        let check = CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::TimedOut),
            duration: None,
            output: None,
        };
        assert_eq!(classify_failure(&check), FailureType::Timeout);
    }

    #[test]
    fn test_classify_unknown() {
        let check = CheckRun {
            name: "Custom Check".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        };
        assert_eq!(
            classify_failure(&check),
            FailureType::Other("custom check".to_string())
        );
    }

    #[test]
    fn test_build_ci_fix_prompt_single_failure() {
        let checks = vec![CheckRun {
            name: "Test Suite".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: Some("2m 34s".to_string()),
            output: Some("FAILED tests/test_auth.rs - test_invalid_token\n\nAssertionError: Expected None, got Some(Token { ... })".to_string()),
        }];

        let prompt = build_ci_fix_prompt(&checks, 1);
        assert!(prompt.contains("attempt 1/2"));
        assert!(prompt.contains("Test Suite"));
        assert!(prompt.contains("test failure"));
        assert!(prompt.contains("FAILED tests/test_auth.rs"));
        assert!(prompt.contains("`just test`"));
    }

    #[test]
    fn test_build_ci_fix_prompt_multiple_failures() {
        let checks = vec![
            CheckRun {
                name: "Build".to_string(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                duration: Some("1m 10s".to_string()),
                output: Some("error[E0433]: failed to resolve".to_string()),
            },
            CheckRun {
                name: "Lint".to_string(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                duration: Some("30s".to_string()),
                output: None,
            },
        ];

        let prompt = build_ci_fix_prompt(&checks, 2);
        assert!(prompt.contains("attempt 2/2"));
        assert!(prompt.contains("Build"));
        assert!(prompt.contains("Lint"));
    }

    #[test]
    fn test_build_ci_fix_prompt_truncates_long_output() {
        let long_output = "x".repeat(20_000);
        let checks = vec![CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: Some(long_output),
        }];

        let prompt = build_ci_fix_prompt(&checks, 1);
        assert!(prompt.contains("truncated"));
        // The prompt should be significantly shorter than 20000 chars of output
        assert!(prompt.len() < 15_000);
    }

    #[test]
    fn test_compute_duration() {
        assert_eq!(
            compute_duration(Some("2024-01-01T00:00:00Z"), Some("2024-01-01T00:02:34Z")),
            Some("2m 34s".to_string())
        );

        assert_eq!(
            compute_duration(Some("2024-01-01T00:00:00Z"), Some("2024-01-01T00:00:45Z")),
            Some("45s".to_string())
        );

        assert_eq!(compute_duration(None, Some("2024-01-01T00:00:00Z")), None);
        assert_eq!(compute_duration(Some("2024-01-01T00:00:00Z"), None), None);
        assert_eq!(compute_duration(None, None), None);
    }

    #[test]
    fn test_compute_duration_invalid_timestamps() {
        assert_eq!(compute_duration(Some("not-a-date"), Some("also-not")), None);
    }

    #[test]
    fn test_check_conclusion_display() {
        assert_eq!(CheckConclusion::Success.to_string(), "success");
        assert_eq!(CheckConclusion::Failure.to_string(), "failure");
        assert_eq!(CheckConclusion::TimedOut.to_string(), "timed_out");
    }

    #[test]
    fn test_failure_type_display() {
        assert_eq!(FailureType::TestFailure.to_string(), "test failure");
        assert_eq!(FailureType::BuildError.to_string(), "build error");
        assert_eq!(
            FailureType::Other("custom".to_string()).to_string(),
            "custom"
        );
    }

    #[test]
    fn test_safe_tail_short_string() {
        assert_eq!(safe_tail("hello", 10), "hello");
    }

    #[test]
    fn test_safe_tail_exact_length() {
        assert_eq!(safe_tail("hello", 5), "hello");
    }

    #[test]
    fn test_safe_tail_truncates() {
        assert_eq!(safe_tail("hello world", 5), "world");
    }

    #[test]
    fn test_safe_tail_multibyte_utf8() {
        // "héllo" where 'é' is 2 bytes (0xC3 0xA9)
        let s = "héllo";
        // Should not panic even if max_bytes lands in the middle of 'é'
        let result = safe_tail(s, 4);
        assert!(result.len() <= 4 || result.starts_with('é') || result.starts_with('l'));
        // The important thing is it doesn't panic
    }

    #[test]
    fn test_safe_tail_emoji() {
        // Each emoji is 4 bytes
        let s = "🔥🔧🚀";
        let result = safe_tail(s, 5);
        // Should give us at least one complete emoji without panicking
        assert!(!result.is_empty());
    }

    #[test]
    fn test_safe_tail_empty_string() {
        assert_eq!(safe_tail("", 10), "");
    }
}
