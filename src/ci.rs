use crate::agent::AgentBackend;
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
pub(crate) const MAX_CI_FIX_ATTEMPTS: u32 = 2;

/// Polling interval for checking CI status (30 seconds)
const CI_POLL_INTERVAL_SECS: u64 = 30;

/// Maximum time to wait for CI checks to appear (5 minutes)
const CI_WAIT_TIMEOUT_SECS: u64 = 300;

/// Maximum time to wait for CI checks to complete (30 minutes)
const CI_COMPLETION_TIMEOUT_SECS: u64 = 1800;

/// Delay after pushing before polling CI, to allow GitHub to register checks (60s)
const POST_PUSH_DELAY_SECS: u64 = 60;

/// Maximum time for a single agent CI fix invocation (20 minutes)
const CI_FIX_TIMEOUT_SECS: u64 = 1200;

/// The status of a CI check run
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    #[default]
    Queued,
    InProgress,
    Completed,
    /// Catch-all for unknown statuses (e.g., GitHub's "waiting" for environment approvals)
    #[serde(other)]
    Unknown,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckStatus::Queued => write!(f, "queued"),
            CheckStatus::InProgress => write!(f, "in_progress"),
            CheckStatus::Completed => write!(f, "completed"),
            CheckStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// The conclusion of a completed check run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckConclusion {
    Success,
    Failure,
    Cancelled,
    TimedOut,
    ActionRequired,
    Neutral,
    Skipped,
    Stale,
    /// Catch-all for unknown or future conclusion values from the GitHub API.
    #[serde(other)]
    Unknown,
}

impl CheckConclusion {
    /// Returns true if this conclusion represents a CI failure.
    pub(crate) fn is_failed(&self) -> bool {
        matches!(
            self,
            CheckConclusion::Failure
                | CheckConclusion::Cancelled
                | CheckConclusion::TimedOut
                | CheckConclusion::ActionRequired
        )
    }
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
            CheckConclusion::Unknown => write!(f, "unknown"),
        }
    }
}

/// Represents a CI check run from GitHub.
///
/// This is the canonical CheckRun type used across the codebase.
/// Fields other than `conclusion` have defaults so the struct can be
/// deserialized from both the rich `--jq`-transformed output (ci.rs)
/// and the raw GitHub API response (pr_monitor.rs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckRun {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: CheckStatus,
    pub(crate) conclusion: Option<CheckConclusion>,
    /// Duration string from GitHub (e.g., "2m 34s")
    pub(crate) duration: Option<String>,
    /// Failure output/logs if available.
    ///
    /// In the raw GitHub API response `output` is an object with fields like
    /// `title`, `summary`, `text`, and `annotations_count`. When this struct is
    /// constructed from the `--jq`-transformed path in `fetch_check_runs`, the
    /// field is set to a pre-built `String`. The custom deserializer accepts
    /// both forms: strings pass through unchanged, while objects have their
    /// `title`/`summary`/`text` fields extracted and concatenated into a single
    /// string (if any are present). Only `null` or other unsupported types
    /// become `None`.
    #[serde(default, deserialize_with = "deserialize_output_field")]
    pub(crate) output: Option<String>,
}

/// Deserializes the `output` field accepting either a JSON string or an object
/// with `title`, `summary`, and `text` subfields (the raw GitHub API shape).
/// Strings pass through directly. Objects have their text fields extracted and
/// concatenated — matching what `fetch_check_runs` produces via its jq filter.
/// Null or other types become `None`.
fn deserialize_output_field<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let v = Option::<Value>::deserialize(deserializer)?;
    match v {
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Object(map)) => {
            let parts: Vec<String> = ["title", "summary", "text"]
                .iter()
                .filter_map(|k| map.get(*k)?.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            Ok(if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            })
        }
        _ => Ok(None),
    }
}

/// The type of CI failure for classification
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum FailureType {
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
pub(crate) enum CiResult {
    /// All checks passed
    AllPassed,
    /// Some checks failed
    Failed(Vec<CheckRun>),
    /// No checks found (might not have CI configured)
    NoChecks,
    /// Timed out waiting for checks
    Timeout,
}

/// Action to take after evaluating CI status in the fix loop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CiFixAction {
    /// CI passed or no checks configured — done successfully.
    Done,
    /// CI failed and we have remaining attempts — try to fix.
    AttemptFix,
    /// Skip to next attempt without fixing (e.g., timeout or no commits).
    RetryNextAttempt,
    /// Escalate to human — all attempts exhausted.
    Escalate(EscalationReason),
}

/// Reason for escalating a CI failure to a human.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EscalationReason {
    /// CI checks timed out on the final attempt.
    Timeout,
    /// CI checks failed on the final attempt.
    ChecksFailed,
    /// Fix attempt produced no new commits on the final attempt.
    NoCommits,
    /// A fix was pushed but no CI checks appeared — CI may not have been triggered.
    NoChecksAfterPush,
}

/// Decides what action to take after observing a CI result.
///
/// `post_push` indicates this is after a fix commit was pushed. In that case,
/// `NoChecks` is treated as an escalation rather than success, because CI was
/// previously running (it failed) and the absence of new checks means the fix
/// commit may not have triggered CI at all.
///
/// Pure function — no I/O, no side effects.
pub(crate) fn decide_ci_action(
    ci_result: &CiResult,
    attempt: u32,
    max_attempts: u32,
    post_push: bool,
) -> CiFixAction {
    match ci_result {
        CiResult::AllPassed => CiFixAction::Done,
        CiResult::NoChecks => {
            if post_push {
                CiFixAction::Escalate(EscalationReason::NoChecksAfterPush)
            } else {
                CiFixAction::Done
            }
        }
        CiResult::Timeout => {
            if attempt >= max_attempts {
                CiFixAction::Escalate(EscalationReason::Timeout)
            } else {
                CiFixAction::RetryNextAttempt
            }
        }
        CiResult::Failed(_) => {
            if attempt >= max_attempts {
                CiFixAction::Escalate(EscalationReason::ChecksFailed)
            } else {
                CiFixAction::AttemptFix
            }
        }
    }
}

/// Decides what action to take when a fix attempt produced no new commits.
///
/// Pure function — no I/O, no side effects.
pub(crate) fn decide_after_no_commits(attempt: u32, max_attempts: u32) -> CiFixAction {
    if attempt >= max_attempts {
        CiFixAction::Escalate(EscalationReason::NoCommits)
    } else {
        CiFixAction::RetryNextAttempt
    }
}

/// Returns the first 8 characters of an ASCII hex SHA for display purposes.
///
/// Uses `str::get` so it never panics on unexpected non-ASCII input.
fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
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

/// Returns the first `max_bytes` of a string, aligned to a UTF-8 char boundary.
fn safe_head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Retreat to the previous valid char boundary
    match (0..=max_bytes).rev().find(|&i| s.is_char_boundary(i)) {
        Some(boundary) => &s[..boundary],
        None => "",
    }
}

/// Truncates long log output by keeping the head and tail, separated by a marker.
///
/// CI logs for test/build failures typically have the diagnostic output near the
/// top (panic message, assertion failure) and summary noise at the end. Keeping
/// both ends gives the agent the best chance of seeing the actual error.
fn smart_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let half = max_bytes / 2;
    let head = safe_head(s, half);
    let tail = safe_tail(s, half);
    let omitted = s.len() - head.len() - tail.len();
    format!(
        "{}\n\n... (truncated {} {}) ...\n\n{}",
        head,
        omitted,
        if omitted == 1 { "byte" } else { "bytes" },
        tail
    )
}

/// Classifies a CI failure based on the check name and output
pub(crate) fn classify_failure(check: &CheckRun) -> FailureType {
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

/// Detects build/test/lint/format hints for the given worktree by inspecting
/// documentation files and package manifests present in the repository root.
///
/// Returns `None` when no recognized artifacts are found so callers can omit
/// the recipe block entirely rather than emitting gru-specific defaults.
fn detect_repo_build_hints(worktree_path: &Path) -> Option<String> {
    // CLAUDE.md / AGENTS.md take priority: the agent already has these in
    // context, so just tell it where to look rather than trying to parse them.
    if worktree_path.join("CLAUDE.md").exists() {
        return Some(
            "Check CLAUDE.md for the project's build, test, lint, and format commands.".to_string(),
        );
    }
    if worktree_path.join("AGENTS.md").exists() {
        return Some(
            "Check AGENTS.md for the project's build, test, lint, and format commands.".to_string(),
        );
    }

    // justfile is language-agnostic and takes precedence over per-ecosystem manifests.
    // Recipe names are not standardized, so tell the agent to list them rather than
    // guessing names like `just test` that may not exist in this repo.
    let has_justfile = worktree_path.join("justfile").exists()
        || worktree_path.join("Justfile").exists()
        || worktree_path.join("JUSTFILE").exists();
    if has_justfile {
        return Some(
            "Run `just --list` to see available recipes, then run the relevant ones (e.g. test, build, lint, fmt) before committing."
                .to_string(),
        );
    }

    // Rust
    if worktree_path.join("Cargo.toml").exists() {
        return Some(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `cargo test`\n\
             - For build errors: `cargo build`\n\
             - For lint errors: `cargo clippy`\n\
             - For format errors: `cargo fmt`"
                .to_string(),
        );
    }

    // Node.js / JavaScript / TypeScript — detect package manager from lockfile
    if worktree_path.join("package.json").exists() {
        let pm = if worktree_path.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if worktree_path.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        return Some(format!(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `{pm} test`\n\
             - For build errors: `{pm} run build`\n\
             - For lint errors: `{pm} run lint`\n\
             - For format errors: `{pm} run format`"
        ));
    }

    // Python — point the agent at whichever config files are actually present
    let has_pyproject = worktree_path.join("pyproject.toml").exists();
    let has_setup_cfg = worktree_path.join("setup.cfg").exists();
    let has_setup_py = worktree_path.join("setup.py").exists();
    if has_pyproject || has_setup_cfg || has_setup_py {
        let config_hint = match (has_pyproject, has_setup_cfg, has_setup_py) {
            (true, true, _) => {
                "check pyproject.toml or setup.cfg for the configured lint and format commands"
            }
            (true, false, true) => {
                "check pyproject.toml or setup.py for the configured lint and format commands"
            }
            (true, false, false) => {
                "check pyproject.toml for the configured lint and format commands"
            }
            (false, true, true) => {
                "check setup.cfg or setup.py for the configured lint and format commands"
            }
            (false, true, false) => "check setup.cfg for the configured lint and format commands",
            _ => "check setup.py for the configured lint and format commands",
        };
        return Some(format!(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `pytest`\n\
             - For lint/format errors: {config_hint}"
        ));
    }

    // Gradle (JVM)
    if worktree_path.join("build.gradle").exists()
        || worktree_path.join("build.gradle.kts").exists()
    {
        return Some(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `./gradlew test`\n\
             - For build errors: `./gradlew build`\n\
             - For lint errors: `./gradlew lint`"
                .to_string(),
        );
    }

    // Maven (JVM)
    if worktree_path.join("pom.xml").exists() {
        return Some(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `./mvnw test` (or `mvn test`)\n\
             - For build errors: `./mvnw compile` (or `mvn compile`)"
                .to_string(),
        );
    }

    // Go
    if worktree_path.join("go.mod").exists() {
        return Some(
            "Run the relevant checks locally before committing:\n\
             - For test failures: `go test ./...`\n\
             - For build errors: `go build ./...`\n\
             - For lint errors: `go vet ./...`\n\
             - For format errors: `gofmt -l .`"
                .to_string(),
        );
    }

    None
}

/// Builds the CI failure prompt for the agent to fix the issue
pub(crate) fn build_ci_fix_prompt(
    failed_checks: &[CheckRun],
    attempt: u32,
    worktree_path: &Path,
) -> String {
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

        match &check.output {
            Some(output) => {
                let truncated = smart_truncate(output, 10_000);
                prompt.push_str(&format!("## Failure Output\n```\n{}\n```\n\n", truncated));
            }
            None => {
                prompt.push_str(
                    "## Failure Output\n\
                     _(Workflow logs could not be retrieved automatically.)_\n\
                     Run the following to view the failure:\n\
                     ```\n\
                     BRANCH=$(git branch --show-current)\n\
                     gh run list --branch \"$BRANCH\" --limit 5 --json databaseId,name,conclusion | cat\n\
                     gh run view <run-id> --log-failed\n\
                     ```\n\n",
                );
            }
        }
    }

    prompt.push_str("Please fix the failing checks.");
    if let Some(hints) = detect_repo_build_hints(worktree_path) {
        prompt.push(' ');
        prompt.push_str(&hints);
    }
    prompt.push_str("\n\nAfter fixing, commit and push the changes.\n");

    prompt
}

/// Fetches check runs for a given PR ref using the gh CLI.
/// Returns a list of CheckRun structs parsed from the gh CLI output.
pub(crate) async fn fetch_check_runs(
    host: &str,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<Vec<CheckRun>> {
    let repo_full = github::repo_slug(owner, repo);

    let endpoint = format!("repos/{}/commits/{}/check-runs", repo_full, git_ref);
    let jq_filter = ".check_runs[] | {name: .name, status: .status, conclusion: .conclusion, started_at: .started_at, completed_at: .completed_at, output_title: .output.title, output_summary: .output.summary, output_text: .output.text}";
    let stdout = github::run_gh(
        host,
        &["api", &endpoint, "--cache", "20s", "--jq", jq_filter],
    )
    .await?;
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

/// Fetches failed workflow run logs for a branch.
///
/// Tries all recently-failed workflow runs on the branch and returns the first
/// non-empty `--log-failed` output. Name-based matching is intentionally omitted:
/// multi-job workflows produce check-run names like "CI / test" while the
/// workflow run itself is named "CI", so substring matching fails reliably.
async fn fetch_check_logs(
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<Option<String>> {
    let repo_full = github::repo_slug(owner, repo);

    let output = github::gh_cli_command(host)
        .args([
            "run",
            "list",
            "--repo",
            &repo_full,
            "--branch",
            branch,
            "--json",
            "databaseId,conclusion",
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

    // gh run list returns runs in most-recent-first order; we return the first
    // non-empty log found, so the agent sees the most recent failure.
    for run in &runs {
        let conclusion = run["conclusion"].as_str().unwrap_or("");
        // Mirror what filter_failed_checks considers a failure.
        let is_failed = matches!(
            conclusion,
            "failure" | "cancelled" | "timed_out" | "action_required"
        );
        if !is_failed {
            continue;
        }
        if let Some(run_id) = run["databaseId"].as_u64() {
            let log_output = github::gh_cli_command(host)
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

    Ok(None)
}

/// Returns true if every check run has reached the `Completed` status.
fn all_checks_completed(checks: &[CheckRun]) -> bool {
    checks.iter().all(|c| c.status == CheckStatus::Completed)
}

/// Filters a list of completed check runs, returning only those that failed.
///
/// Treats `Success`, `Skipped`, and `Neutral` conclusions as passing.
/// All other conclusions (including `None`) are considered failures.
fn filter_failed_checks(checks: Vec<CheckRun>) -> Vec<CheckRun> {
    checks
        .into_iter()
        .filter(|c| {
            c.conclusion != Some(CheckConclusion::Success)
                && c.conclusion != Some(CheckConclusion::Skipped)
                && c.conclusion != Some(CheckConclusion::Neutral)
        })
        .collect()
}

/// Waits for CI checks to complete on a PR, polling at regular intervals.
/// Returns the overall CI result.
pub(crate) async fn wait_for_ci(
    host: &str,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Result<CiResult> {
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

        let checks = fetch_check_runs(host, owner, repo, head_sha).await?;

        if checks.is_empty() {
            sleep(poll_interval).await;
            continue;
        }

        if !checks_found {
            checks_found = true;
            eprintln!("🔄 CI checks detected ({}), monitoring...", checks.len());
        }

        // Check if all runs are completed
        let all_completed = all_checks_completed(&checks);

        if all_completed {
            let failed = filter_failed_checks(checks);

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
pub(crate) async fn get_head_sha(worktree_path: &Path) -> Result<String> {
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

/// Retrieves the current head SHA of a PR from the GitHub API.
///
/// Returns `None` if the PR cannot be found, the `gh` CLI call fails, or the
/// output is empty. Errors are logged as warnings before returning `None`.
/// Uses the same `GH_TIMEOUT_SECS` timeout / `kill_on_drop` behavior as the
/// rest of the codebase via `github::run_gh`.
pub(crate) async fn get_pr_head_sha_from_github(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Option<String> {
    let repo_full = github::repo_slug(owner, repo);
    let pr_num_str = pr_number.to_string();
    match github::run_gh(
        host,
        &[
            "pr",
            "view",
            &pr_num_str,
            "--repo",
            &repo_full,
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ],
    )
    .await
    {
        Ok(stdout) => {
            let sha = stdout.trim().to_string();
            if sha.is_empty() {
                None
            } else {
                Some(sha)
            }
        }
        Err(e) => {
            eprintln!(
                "⚠️  Could not retrieve PR #{} head SHA from GitHub ({}/{}): {}",
                pr_number, owner, repo, e
            );
            None
        }
    }
}

/// Gets the PR number associated with the current branch
/// Looks up a PR number for the given branch.
///
/// When `state` is `None`, only open PRs are returned (gh default).
/// Pass `Some("all")` to include open, closed, and merged PRs.
pub(crate) async fn get_pr_number(
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    state: Option<&str>,
) -> Result<Option<u64>> {
    let repo_full = github::repo_slug(owner, repo);

    let mut args = vec![
        "pr", "list", "--repo", &repo_full, "--head", branch, "--json", "number", "--limit", "1",
    ];

    if let Some(s) = state {
        args.extend(["--state", s]);
    }

    let output = github::gh_cli_command(host)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to list PRs")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let prs: Vec<serde_json::Value> = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("Failed to parse gh pr list output: {}", e);
            return Ok(None);
        }
    };

    Ok(prs.first().and_then(|pr| pr["number"].as_u64()))
}

/// Invokes the agent backend to fix CI failures in the given worktree.
/// Returns the exit code from the agent process.
pub(crate) async fn invoke_ci_fix(
    backend: &dyn AgentBackend,
    worktree_path: &Path,
    failed_checks: &[CheckRun],
    attempt: u32,
) -> Result<i32> {
    let prompt = build_ci_fix_prompt(failed_checks, attempt, worktree_path);

    let mut cmd = backend.build_ci_fix_command(worktree_path, &prompt);
    // Ensure the agent process is killed if the wall-clock timeout fires;
    // without this the child keeps running after wait_with_output is dropped.
    cmd.kill_on_drop(true);
    let child = cmd
        .spawn()
        .with_context(|| format!("Agent backend '{}' failed to start", backend.name()))?;

    let fix_timeout = Duration::from_secs(CI_FIX_TIMEOUT_SECS);
    let output = tokio::time::timeout(fix_timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{} CI fix timed out after {} seconds",
                backend.name(),
                CI_FIX_TIMEOUT_SECS
            )
        })?
        .with_context(|| format!("Failed to wait for {} process", backend.name()))?;

    Ok(output.status.code().unwrap_or(128))
}

/// Posts an escalation comment on the PR when CI fixes are exhausted
pub(crate) async fn post_escalation_comment(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    failed_checks: &[CheckRun],
    attempts: u32,
    minion_id: &str,
) -> Result<()> {
    let mut detail = format!(
        "Automated CI fix failed after **{}/{}** attempts. Human intervention required.\n\n\
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

        detail.push_str(&format!(
            "- **{}** — {} ({})\n",
            check.name, failure_type, conclusion
        ));

        if let Some(output) = &check.output {
            let truncated = smart_truncate(output, 2000);
            detail.push_str(&format!("```\n{}\n```\n", truncated));
        }
    }

    detail.push_str(&format!("\n**Labels:** `{}`\n", labels::BLOCKED));

    let body = crate::progress_comments::format_escalation_comment(
        "CI Fix Escalation",
        &detail,
        minion_id,
    );
    post_escalation_comment_body(host, owner, repo, pr_number, &body).await
}

/// Post an escalation comment when CI exhaustion occurs due to timeout or no-commits.
pub(crate) async fn post_exhaustion_escalation_comment(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    reason: &str,
    attempts: u32,
    minion_id: &str,
) -> Result<()> {
    let detail = format!(
        "Automated CI fix failed after **{}/{}** attempts. Human intervention required.\n\n\
         ### Reason\n\n\
         {}\n\n\
         **Labels:** `{}`\n",
        attempts,
        MAX_CI_FIX_ATTEMPTS,
        reason,
        labels::BLOCKED
    );

    let body = crate::progress_comments::format_escalation_comment(
        "CI Fix Escalation",
        &detail,
        minion_id,
    );
    post_escalation_comment_body(host, owner, repo, pr_number, &body).await
}

/// Posts an escalation comment body to a PR and adds the blocked label.
async fn post_escalation_comment_body(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> Result<()> {
    let repo_full = github::repo_slug(owner, repo);

    let pr_str = pr_number.to_string();
    github::run_gh(
        host,
        &[
            "pr", "comment", &pr_str, "--repo", &repo_full, "--body", body,
        ],
    )
    .await?;

    // Add the blocked label (best-effort)
    let _ = github::run_gh(
        host,
        &[
            "pr",
            "edit",
            &pr_str,
            "--repo",
            &repo_full,
            "--add-label",
            labels::BLOCKED,
        ],
    )
    .await;

    Ok(())
}

/// Main CI monitoring and fix loop.
///
/// After a PR is created/updated:
/// 1. Wait for CI checks to complete
/// 2. If checks fail, invoke the agent backend to fix
/// 3. Retry up to MAX_CI_FIX_ATTEMPTS times
/// 4. Escalate if all attempts fail
///
/// Returns Ok(true) if CI passed (possibly after fixes), Ok(false) if escalated.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn monitor_and_fix_ci(
    backend: &dyn AgentBackend,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    branch: &str,
    worktree_path: &Path,
    minion_id: &str,
) -> Result<bool> {
    // Tracks whether a fix commit has already been pushed. When true, a
    // subsequent NoChecks result means CI did not trigger after the push
    // (path filters, draft PR, skip-ci, etc.) rather than "no CI configured".
    let mut post_push = false;

    for attempt in 1..=MAX_CI_FIX_ATTEMPTS {
        // Get the current HEAD SHA
        let head_sha = get_head_sha(worktree_path).await?;

        eprintln!(
            "\n🔍 CI monitoring (attempt {}/{}) for commit {}...",
            attempt,
            MAX_CI_FIX_ATTEMPTS,
            short_sha(&head_sha)
        );

        // Wait for CI to complete
        let ci_result = wait_for_ci(host, owner, repo, &head_sha).await?;

        match decide_ci_action(&ci_result, attempt, MAX_CI_FIX_ATTEMPTS, post_push) {
            CiFixAction::Done => {
                match ci_result {
                    CiResult::AllPassed => eprintln!("✅ All CI checks passed!"),
                    CiResult::NoChecks => {
                        eprintln!("ℹ️  No CI checks configured, skipping CI monitoring")
                    }
                    _ => {}
                }
                return Ok(true);
            }
            CiFixAction::RetryNextAttempt => {
                // Only reachable via CiResult::Timeout (non-final attempt)
                eprintln!("⏱️  CI checks timed out");
                continue;
            }
            CiFixAction::Escalate(EscalationReason::Timeout) => {
                return escalate_timeout(host, owner, repo, pr_number, attempt, minion_id).await;
            }
            CiFixAction::Escalate(EscalationReason::ChecksFailed) => {
                let mut failed_checks = match ci_result {
                    CiResult::Failed(checks) => checks,
                    _ => unreachable!(),
                };
                enrich_check_logs(host, owner, repo, branch, &mut failed_checks).await;
                return escalate_checks_failed(
                    host,
                    owner,
                    repo,
                    pr_number,
                    &failed_checks,
                    attempt,
                    minion_id,
                )
                .await;
            }
            CiFixAction::Escalate(EscalationReason::NoChecksAfterPush) => {
                return escalate_no_checks_after_push(
                    host, owner, repo, pr_number, &head_sha, attempt, minion_id,
                )
                .await;
            }
            CiFixAction::AttemptFix => {
                let failed_checks = match ci_result {
                    CiResult::Failed(checks) => checks,
                    _ => unreachable!(),
                };
                let fix_result = run_ci_fix_attempt(
                    backend,
                    host,
                    owner,
                    repo,
                    pr_number,
                    branch,
                    worktree_path,
                    minion_id,
                    &head_sha,
                    failed_checks,
                    attempt,
                )
                .await?;
                match fix_result {
                    // Push succeeded — next CI wait is post-push.
                    CiFixLoopAction::Continue => {
                        post_push = true;
                        continue;
                    }
                    // Retrying after an agent error or no-commits: nothing was
                    // pushed, so post_push stays unchanged.
                    CiFixLoopAction::ContinueNoPush => continue,
                    CiFixLoopAction::Break => break,
                    CiFixLoopAction::Return(val) => return Ok(val),
                }
            }
            // NoCommits escalation is only returned by decide_after_no_commits
            CiFixAction::Escalate(EscalationReason::NoCommits) => unreachable!(),
        }
    }

    // Reached when the fix attempt breaks the loop (e.g., git push failure).
    Ok(false)
}

/// Internal signal from `run_ci_fix_attempt` back to the main loop.
enum CiFixLoopAction {
    /// Push succeeded: `continue` and set `post_push = true`.
    Continue,
    /// Retrying but nothing was pushed (agent error or no new commits).
    /// `continue` without setting `post_push`.
    ContinueNoPush,
    /// Push failed: `break` the outer loop.
    Break,
    /// Escalated to human: return this value immediately.
    Return(bool),
}

/// Enrich failed checks with detailed logs when output is missing.
///
/// Fetches `--log-failed` output once and applies it to all checks that have
/// no output. A single fetch is sufficient because all failing jobs in a given
/// workflow run appear in the same `gh run view --log-failed` output.
async fn enrich_check_logs(
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    failed_checks: &mut [CheckRun],
) {
    let needs_enrichment = failed_checks
        .iter()
        .any(|c| c.output.is_none() || c.output.as_deref() == Some(""));
    if !needs_enrichment {
        return;
    }

    if let Ok(Some(logs)) = fetch_check_logs(host, owner, repo, branch).await {
        for check in failed_checks.iter_mut() {
            if check.output.is_none() || check.output.as_deref() == Some("") {
                check.output = Some(logs.clone());
            }
        }
    }
}

/// Escalate when CI checks timed out on final attempt.
async fn escalate_timeout(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    attempt: u32,
    minion_id: &str,
) -> Result<bool> {
    eprintln!(
        "🚨 Max fix attempts ({}) reached with CI timeout, escalating to human",
        MAX_CI_FIX_ATTEMPTS
    );
    post_exhaustion_escalation_comment(
        host,
        owner,
        repo,
        pr_number,
        "CI checks timed out on all attempts.",
        attempt,
        minion_id,
    )
    .await
    .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
    Ok(false)
}

/// Escalate when CI checks failed on final attempt.
async fn escalate_checks_failed(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    failed_checks: &[CheckRun],
    attempt: u32,
    minion_id: &str,
) -> Result<bool> {
    eprintln!(
        "🚨 Max fix attempts ({}) reached, escalating to human",
        MAX_CI_FIX_ATTEMPTS
    );
    post_escalation_comment(
        host,
        owner,
        repo,
        pr_number,
        failed_checks,
        attempt,
        minion_id,
    )
    .await
    .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
    Ok(false)
}

/// Escalate when a fix was pushed but no CI checks appeared afterward.
///
/// Queries the PR's current head SHA from GitHub to provide diagnostic
/// information: if it differs from the local SHA the push may not have
/// registered, which is a separate bug to investigate.
async fn escalate_no_checks_after_push(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    local_sha: &str,
    attempt: u32,
    minion_id: &str,
) -> Result<bool> {
    eprintln!(
        "🚨 CI did not trigger after fix push (attempt {}/{}), escalating to human",
        attempt, MAX_CI_FIX_ATTEMPTS
    );

    let pr_sha = get_pr_head_sha_from_github(host, owner, repo, pr_number).await;

    let sha_note = match pr_sha.as_deref() {
        Some(s) if s == local_sha => format!(
            "GitHub PR head SHA matches the local commit (`{}`), so the push registered — \
             but no CI checks appeared. Possible causes: workflow path filters excluded the \
             touched files, the PR is in draft state, the commit message contains `[skip ci]`, \
             or the required workflow was disabled.",
            short_sha(local_sha)
        ),
        Some(s) => format!(
            "GitHub PR head SHA (`{}`) does not match the local commit (`{}`). \
             The push may not have registered on GitHub.",
            short_sha(s),
            short_sha(local_sha)
        ),
        None => format!(
            "Could not retrieve the PR head SHA from GitHub to compare with the \
             local commit (`{}`).",
            short_sha(local_sha)
        ),
    };

    let reason = format!(
        "A fix commit was pushed but no CI checks appeared within the \
         {CI_WAIT_TIMEOUT_SECS}-second window.\n\n{sha_note}"
    );

    post_exhaustion_escalation_comment(host, owner, repo, pr_number, &reason, attempt, minion_id)
        .await
        .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
    Ok(false)
}

/// Escalate with a reason string (timeout, fix error, or no commits).
async fn escalate_exhaustion(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    reason: &str,
    attempt: u32,
    minion_id: &str,
) -> CiFixLoopAction {
    post_exhaustion_escalation_comment(host, owner, repo, pr_number, reason, attempt, minion_id)
        .await
        .unwrap_or_else(|e| eprintln!("⚠️  Failed to post escalation: {}", e));
    CiFixLoopAction::Return(false)
}

/// Run a single CI fix attempt: enrich logs, invoke Claude, check for new
/// commits, and push. Returns a loop-control signal for the caller.
#[allow(clippy::too_many_arguments)]
async fn run_ci_fix_attempt(
    backend: &dyn AgentBackend,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    branch: &str,
    worktree_path: &Path,
    minion_id: &str,
    head_sha: &str,
    mut failed_checks: Vec<CheckRun>,
    attempt: u32,
) -> Result<CiFixLoopAction> {
    let check_names: Vec<&str> = failed_checks.iter().map(|c| c.name.as_str()).collect();
    eprintln!("❌ CI checks failed: {}", check_names.join(", "));

    enrich_check_logs(host, owner, repo, branch, &mut failed_checks).await;

    // Invoke agent to fix
    eprintln!(
        "🔧 Invoking {} to fix CI failures (attempt {}/{})...",
        backend.name(),
        attempt,
        MAX_CI_FIX_ATTEMPTS
    );
    let fix_result = invoke_ci_fix(backend, worktree_path, &failed_checks, attempt).await;

    match fix_result {
        Ok(exit_code) => {
            if exit_code != 0 {
                eprintln!(
                    "⚠️  Agent fix attempt returned non-zero exit code: {}",
                    exit_code
                );
            }
        }
        Err(e) => {
            eprintln!("⚠️  Agent CI fix failed: {}", e);
            match decide_after_no_commits(attempt, MAX_CI_FIX_ATTEMPTS) {
                CiFixAction::RetryNextAttempt => return Ok(CiFixLoopAction::ContinueNoPush),
                CiFixAction::Escalate(_) => {
                    eprintln!(
                        "🚨 Max fix attempts ({}) reached with CI fix error, escalating to human",
                        MAX_CI_FIX_ATTEMPTS
                    );
                    return Ok(escalate_exhaustion(
                        host,
                        owner,
                        repo,
                        pr_number,
                        &format!("CI fix process failed: {}", e),
                        attempt,
                        minion_id,
                    )
                    .await);
                }
                _ => unreachable!(),
            }
        }
    }

    // Check if the agent actually made new commits
    let new_sha = get_head_sha(worktree_path).await?;
    if new_sha == head_sha {
        eprintln!("⚠️  Agent made no new commits, cannot retry");
        match decide_after_no_commits(attempt, MAX_CI_FIX_ATTEMPTS) {
            CiFixAction::RetryNextAttempt => return Ok(CiFixLoopAction::ContinueNoPush),
            CiFixAction::Escalate(_) => {
                eprintln!(
                    "🚨 Max fix attempts ({}) reached with no commits, escalating to human",
                    MAX_CI_FIX_ATTEMPTS
                );
                return Ok(escalate_exhaustion(
                    host,
                    owner,
                    repo,
                    pr_number,
                    "CI fix attempt produced no new commits on all attempts.",
                    attempt,
                    minion_id,
                )
                .await);
            }
            _ => unreachable!(),
        }
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
        return Ok(CiFixLoopAction::Break);
    }

    // Wait for GitHub to register checks for the new commit
    eprintln!(
        "⏳ Waiting {}s for CI to register new checks...",
        POST_PUSH_DELAY_SECS
    );
    sleep(Duration::from_secs(POST_PUSH_DELAY_SECS)).await;

    // Loop continues to check CI again
    Ok(CiFixLoopAction::Continue)
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
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Justfile"), "").unwrap();
        let checks = vec![CheckRun {
            name: "Test Suite".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: Some("2m 34s".to_string()),
            output: Some("FAILED tests/test_auth.rs - test_invalid_token\n\nAssertionError: Expected None, got Some(Token { ... })".to_string()),
        }];

        let prompt = build_ci_fix_prompt(&checks, 1, dir.path());
        assert!(prompt.contains("attempt 1/2"));
        assert!(prompt.contains("Test Suite"));
        assert!(prompt.contains("test failure"));
        assert!(prompt.contains("FAILED tests/test_auth.rs"));
        assert!(prompt.contains("just --list"));
    }

    #[test]
    fn test_build_ci_fix_prompt_multiple_failures() {
        let dir = tempfile::tempdir().unwrap();
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

        let prompt = build_ci_fix_prompt(&checks, 2, dir.path());
        assert!(prompt.contains("attempt 2/2"));
        assert!(prompt.contains("Build"));
        assert!(prompt.contains("Lint"));
    }

    #[test]
    fn test_build_ci_fix_prompt_truncates_long_output() {
        let dir = tempfile::tempdir().unwrap();
        let long_output = "x".repeat(20_000);
        let checks = vec![CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: Some(long_output),
        }];

        let prompt = build_ci_fix_prompt(&checks, 1, dir.path());
        assert!(prompt.contains("truncated"));
        // The prompt should be significantly shorter than 20000 chars of output
        assert!(prompt.len() < 15_000);
    }

    #[test]
    fn test_build_ci_fix_prompt_no_just_for_non_rust_repo() {
        // A repo with only package.json should not get `just` commands
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let checks = vec![CheckRun {
            name: "Test".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        }];

        let prompt = build_ci_fix_prompt(&checks, 1, dir.path());
        assert!(
            !prompt.contains("just"),
            "prompt should not mention `just` for a Node.js repo"
        );
        assert!(prompt.contains("npm test"));
    }

    #[test]
    fn test_build_ci_fix_prompt_omits_recipes_when_no_manifest() {
        // A repo with no recognized artifacts should omit the recipe block entirely
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![CheckRun {
            name: "CI".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        }];

        let prompt = build_ci_fix_prompt(&checks, 1, dir.path());
        assert!(!prompt.contains("just"));
        assert!(!prompt.contains("cargo"));
        assert!(!prompt.contains("npm"));
        assert!(prompt.contains("Please fix the failing checks."));
        assert!(prompt.contains("After fixing, commit and push"));
    }

    #[test]
    fn test_detect_repo_build_hints_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Build\n`cargo test`").unwrap();
        // Also put a Cargo.toml to ensure CLAUDE.md wins
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("CLAUDE.md"));
        assert!(!hints.contains("cargo test"));
    }

    #[test]
    fn test_detect_repo_build_hints_rust_no_justfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("cargo test"));
        assert!(!hints.contains("just"));
    }

    #[test]
    fn test_detect_repo_build_hints_justfile_beats_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Justfile"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("just --list"));
        assert!(!hints.contains("cargo test"));
    }

    #[test]
    fn test_detect_repo_build_hints_none_for_unknown_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_repo_build_hints(dir.path()).is_none());
    }

    #[test]
    fn test_detect_repo_build_hints_claude_md_beats_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("CLAUDE.md"));
        assert!(!hints.contains("AGENTS.md"));
    }

    #[test]
    fn test_detect_repo_build_hints_node_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("`pnpm test`"));
        assert!(!hints.contains("`npm test`"));
    }

    #[test]
    fn test_detect_repo_build_hints_node_yarn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("`yarn test`"));
        assert!(!hints.contains("`npm test`"));
    }

    #[test]
    fn test_detect_repo_build_hints_go() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/foo\n\ngo 1.22",
        )
        .unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("go test ./..."));
        assert!(!hints.contains("just"));
    }

    #[test]
    fn test_detect_repo_build_hints_maven() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        let hints = detect_repo_build_hints(dir.path()).unwrap();
        assert!(hints.contains("mvnw") || hints.contains("mvn"));
        assert!(!hints.contains("just"));
    }

    #[test]
    fn test_build_ci_fix_prompt_no_output_includes_fallback_instructions() {
        // Regression: checks with empty output.* must still give the agent
        // something actionable rather than a silent, empty failure block.
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![CheckRun {
            name: "CI / test".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: Some("1m 30s".to_string()),
            output: None,
        }];

        let prompt = build_ci_fix_prompt(&checks, 1, dir.path());
        assert!(
            prompt.contains("gh run view"),
            "prompt must mention gh run view"
        );
        assert!(
            prompt.contains("log-failed"),
            "prompt must mention --log-failed"
        );
    }

    #[test]
    fn test_smart_truncate_short_string() {
        let s = "short";
        assert_eq!(smart_truncate(s, 100), "short");
    }

    #[test]
    fn test_smart_truncate_exact_length() {
        let s = "hello";
        assert_eq!(smart_truncate(s, 5), "hello");
    }

    #[test]
    fn test_smart_truncate_keeps_head_and_tail() {
        // 20 'a's + 20 'b's = 40 chars; truncate to 20 keeps head(10) + tail(10)
        let s = format!("{}{}", "a".repeat(20), "b".repeat(20));
        let result = smart_truncate(&s, 20);
        assert!(result.contains("aaa"), "should preserve head");
        assert!(result.contains("bbb"), "should preserve tail");
        assert!(
            result.contains("truncated"),
            "should include truncation marker"
        );
        // The full 40-char input must not appear verbatim in the result
        assert!(!result.contains(&s), "full input must not appear verbatim");
    }

    #[test]
    fn test_smart_truncate_diagnostic_at_top_preserved() {
        // Simulates a panic message at the top followed by verbose noise below.
        let head = "thread 'main' panicked at 'assertion failed', src/lib.rs:42\n";
        let noise = "very noisy post-failure output\n".repeat(500);
        let s = format!("{}{}", head, noise);
        let result = smart_truncate(&s, 2_000);
        assert!(
            result.contains("panicked"),
            "panic message from head must be preserved"
        );
    }

    #[test]
    fn test_safe_head_short_string() {
        assert_eq!(safe_head("hello", 10), "hello");
    }

    #[test]
    fn test_safe_head_truncates() {
        assert_eq!(safe_head("hello world", 5), "hello");
    }

    #[test]
    fn test_safe_head_multibyte_utf8() {
        let s = "héllo";
        // Should not panic even if max_bytes lands mid-char
        let result = safe_head(s, 2);
        assert!(s.starts_with(result));
        assert!(result.len() <= 2);
    }

    #[test]
    fn test_safe_head_zero_budget() {
        assert_eq!(safe_head("hello", 0), "");
    }

    #[test]
    fn test_smart_truncate_zero_budget() {
        // Exercises the half=0 path; must not panic.
        let result = smart_truncate("hello world", 0);
        assert!(result.contains("truncated"));
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

    // --- decide_ci_action tests ---

    #[test]
    fn test_decide_ci_action_all_passed() {
        let result = CiResult::AllPassed;
        assert_eq!(decide_ci_action(&result, 1, 2, false), CiFixAction::Done);
    }

    #[test]
    fn test_decide_ci_action_no_checks_not_post_push() {
        // NoChecks before any fix push → treat as "no CI configured", done.
        let result = CiResult::NoChecks;
        assert_eq!(decide_ci_action(&result, 1, 2, false), CiFixAction::Done);
    }

    #[test]
    fn test_decide_ci_action_no_checks_post_push() {
        // NoChecks after a fix was pushed → CI did not trigger, escalate.
        let result = CiResult::NoChecks;
        assert_eq!(
            decide_ci_action(&result, 2, 2, true),
            CiFixAction::Escalate(EscalationReason::NoChecksAfterPush)
        );
    }

    #[test]
    fn test_decide_ci_action_no_checks_post_push_non_final_attempt() {
        // Even on a non-final attempt, NoChecks-after-push should escalate
        // (retrying won't help if CI is not configured to trigger).
        let result = CiResult::NoChecks;
        assert_eq!(
            decide_ci_action(&result, 1, 2, true),
            CiFixAction::Escalate(EscalationReason::NoChecksAfterPush)
        );
    }

    #[test]
    fn test_decide_ci_action_timeout_not_last_attempt() {
        let result = CiResult::Timeout;
        assert_eq!(
            decide_ci_action(&result, 1, 2, false),
            CiFixAction::RetryNextAttempt
        );
    }

    #[test]
    fn test_decide_ci_action_timeout_last_attempt() {
        let result = CiResult::Timeout;
        assert_eq!(
            decide_ci_action(&result, 2, 2, false),
            CiFixAction::Escalate(EscalationReason::Timeout)
        );
    }

    #[test]
    fn test_decide_ci_action_failed_not_last_attempt() {
        let result = CiResult::Failed(vec![CheckRun {
            name: "test".to_string(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            duration: None,
            output: None,
        }]);
        assert_eq!(
            decide_ci_action(&result, 1, 2, false),
            CiFixAction::AttemptFix
        );
    }

    #[test]
    fn test_decide_ci_action_failed_last_attempt() {
        let result = CiResult::Failed(vec![]);
        assert_eq!(
            decide_ci_action(&result, 2, 2, false),
            CiFixAction::Escalate(EscalationReason::ChecksFailed)
        );
    }

    #[test]
    fn test_decide_ci_action_single_attempt_max() {
        // With max_attempts=1, first attempt is already the last
        let result = CiResult::Failed(vec![]);
        assert_eq!(
            decide_ci_action(&result, 1, 1, false),
            CiFixAction::Escalate(EscalationReason::ChecksFailed)
        );

        let timeout = CiResult::Timeout;
        assert_eq!(
            decide_ci_action(&timeout, 1, 1, false),
            CiFixAction::Escalate(EscalationReason::Timeout)
        );
    }

    #[test]
    fn test_decide_ci_action_passed_on_last_attempt() {
        // CI can pass on any attempt
        let result = CiResult::AllPassed;
        assert_eq!(decide_ci_action(&result, 2, 2, false), CiFixAction::Done);
    }

    // --- decide_after_no_commits tests ---

    #[test]
    fn test_decide_after_no_commits_not_last_attempt() {
        assert_eq!(decide_after_no_commits(1, 2), CiFixAction::RetryNextAttempt);
    }

    #[test]
    fn test_decide_after_no_commits_last_attempt() {
        assert_eq!(
            decide_after_no_commits(2, 2),
            CiFixAction::Escalate(EscalationReason::NoCommits)
        );
    }

    #[test]
    fn test_decide_after_no_commits_beyond_max() {
        // attempt > max should still escalate
        assert_eq!(
            decide_after_no_commits(3, 2),
            CiFixAction::Escalate(EscalationReason::NoCommits)
        );
    }

    #[test]
    fn test_decide_after_no_commits_single_attempt() {
        assert_eq!(
            decide_after_no_commits(1, 1),
            CiFixAction::Escalate(EscalationReason::NoCommits)
        );
    }

    // --- T5: CI completion/failure filtering tests ---

    fn make_check(
        name: &str,
        status: CheckStatus,
        conclusion: Option<CheckConclusion>,
    ) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            status,
            conclusion,
            duration: None,
            output: None,
        }
    }

    #[test]
    fn test_all_checks_completed_when_all_completed() {
        let checks = vec![
            make_check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            make_check(
                "lint",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
        ];
        assert!(all_checks_completed(&checks));
    }

    #[test]
    fn test_all_checks_completed_with_in_progress() {
        let checks = vec![
            make_check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            make_check("lint", CheckStatus::InProgress, None),
        ];
        assert!(!all_checks_completed(&checks));
    }

    #[test]
    fn test_all_checks_completed_with_queued() {
        let checks = vec![make_check("build", CheckStatus::Queued, None)];
        assert!(!all_checks_completed(&checks));
    }

    #[test]
    fn test_all_checks_completed_empty() {
        // Documents vacuous-truth behavior of Iterator::all on empty input.
        // In practice, wait_for_ci guards against empty checks before calling
        // all_checks_completed, so this path is not reachable in production.
        let checks: Vec<CheckRun> = vec![];
        assert!(all_checks_completed(&checks));
    }

    #[test]
    fn test_filter_failed_checks_all_passing() {
        let checks = vec![
            make_check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            make_check(
                "lint",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped),
            ),
            make_check(
                "optional",
                CheckStatus::Completed,
                Some(CheckConclusion::Neutral),
            ),
        ];
        let failed = filter_failed_checks(checks);
        assert!(failed.is_empty());
    }

    #[test]
    fn test_filter_failed_checks_with_failure() {
        let checks = vec![
            make_check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            make_check(
                "test",
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            ),
        ];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "test");
    }

    #[test]
    fn test_filter_failed_checks_cancelled_is_failure() {
        let checks = vec![make_check(
            "deploy",
            CheckStatus::Completed,
            Some(CheckConclusion::Cancelled),
        )];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_filter_failed_checks_timed_out_is_failure() {
        let checks = vec![make_check(
            "slow-test",
            CheckStatus::Completed,
            Some(CheckConclusion::TimedOut),
        )];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_filter_failed_checks_action_required_is_failure() {
        let checks = vec![make_check(
            "review",
            CheckStatus::Completed,
            Some(CheckConclusion::ActionRequired),
        )];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_filter_failed_checks_stale_is_failure() {
        let checks = vec![make_check(
            "old",
            CheckStatus::Completed,
            Some(CheckConclusion::Stale),
        )];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_filter_failed_checks_none_conclusion_is_failure() {
        let checks = vec![make_check("mystery", CheckStatus::Completed, None)];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn test_filter_failed_checks_mixed() {
        let checks = vec![
            make_check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            make_check(
                "test",
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            ),
            make_check(
                "lint",
                CheckStatus::Completed,
                Some(CheckConclusion::Neutral),
            ),
            make_check(
                "deploy",
                CheckStatus::Completed,
                Some(CheckConclusion::Cancelled),
            ),
            make_check(
                "optional",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped),
            ),
        ];
        let failed = filter_failed_checks(checks);
        assert_eq!(failed.len(), 2);
        let names: Vec<&str> = failed.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"deploy"));
    }
}
