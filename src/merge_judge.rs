//! LLM merge-readiness judge.
//!
//! After all deterministic merge checks pass (CI green, reviews approved, no
//! conflicts, not draft), this module invokes an LLM to evaluate whether review
//! feedback has been genuinely addressed before auto-merging.
//!
//! The judge returns one of three actions:
//! - **Merge** — all feedback genuinely addressed, proceed.
//! - **Wait** — not confident yet, re-evaluate after a duration.
//! - **Escalate** — needs human review, apply label + comment.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

use crate::agent::AgentBackend;
use crate::github;
use crate::labels;

/// Default confidence threshold (1-10). Only merge when confidence >= this.
pub(crate) const DEFAULT_CONFIDENCE_THRESHOLD: u8 = 8;

/// Maximum consecutive wait responses before the judge must decide merge or escalate.
const MAX_CONSECUTIVE_WAITS: u32 = 3;

/// Maximum consecutive parse/invocation failures before escalating.
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Maximum wait duration the LLM can request (in minutes).
const MAX_WAIT_MINUTES: u64 = 120;

/// Label applied when the judge escalates for human review.
const NEEDS_HUMAN_REVIEW_LABEL: &str = labels::NEEDS_HUMAN_REVIEW;

/// Action the judge can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JudgeAction {
    /// All feedback genuinely addressed — proceed with merge.
    Merge,
    /// Not confident yet — re-evaluate after the given duration.
    Wait(Duration),
    /// Needs human review — apply label and post comment.
    Escalate,
}

/// Structured response from the LLM judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JudgeResponseRaw {
    confidence: u8,
    action: String,
    #[serde(default)]
    wait_minutes: Option<u64>,
    reasoning: String,
}

/// Parsed judge response with typed action.
#[derive(Debug, Clone)]
pub(crate) struct JudgeResponse {
    pub(crate) confidence: u8,
    pub(crate) action: JudgeAction,
    pub(crate) reasoning: String,
}

/// Fingerprint of PR state used to avoid redundant judge invocations.
///
/// `ci_label_hash` covers CI conclusion + sorted label set so that a
/// red→green CI flip (or a label change) on the same head triggers
/// re-evaluation even when no new comments landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrStateFingerprint {
    pub(crate) head_sha: String,
    pub(crate) comment_count: usize,
    pub(crate) ci_label_hash: u64,
}

/// Authoritative live state of a PR. Injected into the judge prompt so the
/// LLM does not re-derive CI / label state from comment archaeology.
#[derive(Debug, Clone)]
pub(crate) struct CurrentFacts {
    pub(crate) head_sha: String,
    /// Rolled-up CI conclusion: "SUCCESS", "FAILURE", "PENDING", or "NONE".
    pub(crate) ci_conclusion: String,
    /// Labels currently on the PR.
    pub(crate) labels: Vec<String>,
    /// GitHub's `mergeable` flag: Some(true) clean, Some(false) conflicts,
    /// None when still computing.
    pub(crate) mergeable: Option<bool>,
}

impl CurrentFacts {
    fn ci_label_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.ci_conclusion.hash(&mut hasher);
        let mut sorted = self.labels.clone();
        sorted.sort();
        sorted.dedup();
        for l in &sorted {
            l.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Tracks the judge's state across poll iterations.
#[derive(Debug)]
pub(crate) struct JudgeState {
    /// Last PR state we evaluated.
    last_fingerprint: Option<PrStateFingerprint>,
    /// Number of consecutive "wait" responses with no state change.
    consecutive_waits: u32,
    /// Number of consecutive failures (parse errors, CLI errors) on the same fingerprint.
    consecutive_failures: u32,
    /// When the current wait expires (if any).
    wait_until: Option<DateTime<Utc>>,
    /// When the next failure retry is allowed (exponential backoff).
    retry_after: Option<DateTime<Utc>>,
    /// Whether the judge has escalated and the label was confirmed applied.
    label_applied: bool,
    /// Whether a failure-triggered escalation has already been performed
    /// for the current fingerprint (prevents repeated label/comment spam).
    failure_escalated: bool,
}

impl JudgeState {
    pub(crate) fn new() -> Self {
        Self {
            last_fingerprint: None,
            consecutive_waits: 0,
            consecutive_failures: 0,
            wait_until: None,
            retry_after: None,
            label_applied: false,
            failure_escalated: false,
        }
    }

    /// Returns true if the judge should be invoked for the given PR state.
    pub(crate) fn should_invoke(&self, fingerprint: &PrStateFingerprint) -> bool {
        // If PR state changed, always re-invoke.
        if self.last_fingerprint.as_ref() != Some(fingerprint) {
            return true;
        }

        // If we've hit the failure cap, stop retrying until state changes.
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return false;
        }

        // If we have failures below the cap, retry after backoff expires.
        if self.consecutive_failures > 0 {
            return match self.retry_after {
                Some(until) => Utc::now() >= until,
                None => true,
            };
        }

        // Same state — only invoke if a wait timer expired.
        if let Some(until) = self.wait_until {
            return Utc::now() >= until;
        }

        // Same state, no wait timer — don't re-invoke.
        false
    }

    /// Record a failure (parse error, CLI error) for the given fingerprint.
    /// Updates the fingerprint and sets a backoff timer so `should_invoke`
    /// will allow retries up to `MAX_CONSECUTIVE_FAILURES`.
    pub(crate) fn record_failure(&mut self, fingerprint: PrStateFingerprint) {
        let state_changed = self.last_fingerprint.as_ref() != Some(&fingerprint);
        if state_changed {
            self.consecutive_failures = 0;
            // Reset stale wait state from previous fingerprint to avoid
            // leaking wait_until/consecutive_waits into the new state.
            self.consecutive_waits = 0;
            self.wait_until = None;
            self.failure_escalated = false;
        }
        self.consecutive_failures += 1;
        // Exponential backoff: 2min, 4min, 8min (capped at 10min).
        let backoff_mins = (2u64.saturating_pow(self.consecutive_failures)).min(10);
        self.retry_after = Some(Utc::now() + chrono::Duration::minutes(backoff_mins as i64));
        self.last_fingerprint = Some(fingerprint);
    }

    /// Returns the number of consecutive failures on the current fingerprint.
    pub(crate) fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Returns the retry backoff remaining in minutes (rounded up), or 0
    /// if no backoff is active.
    pub(crate) fn retry_backoff_minutes(&self) -> i64 {
        match self.retry_after {
            Some(until) => {
                let remaining = until - Utc::now();
                (remaining.num_seconds() + 59).max(0) / 60 // round up
            }
            None => 0,
        }
    }

    /// Returns true if the failure cap has been reached and escalation
    /// has not yet been performed for this fingerprint.
    pub(crate) fn should_escalate_on_failure(&self) -> bool {
        self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES && !self.failure_escalated
    }

    /// Mark that a failure-triggered escalation has been performed,
    /// preventing repeated label/comment spam on subsequent poll cycles.
    pub(crate) fn mark_failure_escalated(&mut self) {
        self.failure_escalated = true;
    }

    /// Record the judge's response and update internal state.
    pub(crate) fn record_response(
        &mut self,
        fingerprint: PrStateFingerprint,
        response: &JudgeResponse,
    ) {
        let state_changed = self.last_fingerprint.as_ref() != Some(&fingerprint);

        if state_changed {
            self.consecutive_waits = 0;
        }

        self.consecutive_failures = 0;
        self.retry_after = None;
        self.failure_escalated = false;
        self.last_fingerprint = Some(fingerprint);

        match &response.action {
            JudgeAction::Wait(duration) => {
                self.consecutive_waits += 1;
                let chrono_duration = chrono::Duration::from_std(*duration).unwrap_or_else(|e| {
                    log::warn!("Wait duration out of range: {e}, using 15m default");
                    chrono::Duration::minutes(15)
                });
                self.wait_until = Some(Utc::now() + chrono_duration);
            }
            JudgeAction::Escalate => {
                self.consecutive_waits = 0;
                self.wait_until = None;
                // Note: label_applied is set by the caller after the label is
                // actually applied, not here.
            }
            JudgeAction::Merge => {
                self.consecutive_waits = 0;
                self.wait_until = None;
            }
        }
    }

    /// Returns the number of consecutive waits with no state change.
    pub(crate) fn consecutive_waits(&self) -> u32 {
        self.consecutive_waits
    }

    /// Mark that the escalation label was successfully applied.
    pub(crate) fn mark_label_applied(&mut self) {
        self.label_applied = true;
    }

    /// Returns true if the label was previously applied by the judge.
    pub(crate) fn label_was_applied(&self) -> bool {
        self.label_applied
    }

    /// Note that `gru:needs-human-review` was cleared by a human.
    /// Only has effect if the label was previously applied.
    pub(crate) fn mark_escalation_cleared(&mut self) {
        if self.label_applied {
            self.label_applied = false;
            // Reset fingerprint and failure counter so the judge re-evaluates
            // on next check with a fresh retry budget.
            self.last_fingerprint = None;
            self.consecutive_failures = 0;
            self.retry_after = None;
            self.failure_escalated = false;
        }
    }
}

/// Run multiple `gh` commands in parallel, returning results in input order.
///
/// Each entry in `arg_sets` is a list of arguments passed to `github::run_gh`.
/// All commands are spawned concurrently and the function returns once all
/// complete, or propagates the first error encountered.
async fn run_gh_parallel(host: &str, arg_sets: Vec<Vec<String>>) -> Result<Vec<String>> {
    let count = arg_sets.len();
    let mut set = JoinSet::new();

    for (idx, args) in arg_sets.into_iter().enumerate() {
        let host = host.to_string();
        set.spawn(async move {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let result = github::run_gh(&host, &refs).await?;
            Ok::<_, anyhow::Error>((idx, result))
        });
    }

    let mut results: Vec<Option<String>> = (0..count).map(|_| None).collect();
    while let Some(join_result) = set.join_next().await {
        match join_result.context("gh task failed (panic or cancellation)")? {
            Ok((idx, output)) => {
                results[idx] = Some(output);
            }
            Err(e) => {
                // Cancel remaining tasks to match try_join! short-circuit semantics.
                set.abort_all();
                return Err(e);
            }
        }
    }

    // Every spawned task populates its index, so None is unreachable here.
    results
        .into_iter()
        .enumerate()
        .map(|(i, opt)| opt.ok_or_else(|| anyhow::anyhow!("missing result at index {i}")))
        .collect()
}

/// Lightweight snapshot fetch: fingerprint + authoritative current facts.
///
/// Fetches PR metadata (head SHA, labels, mergeable), comment counts, and
/// check-runs for the current head so the fingerprint reacts to CI / label
/// transitions even without a new comment, and so the judge prompt can
/// carry authoritative live state.
pub(crate) async fn get_pr_snapshot(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<(PrStateFingerprint, CurrentFacts)> {
    let repo_full = github::repo_slug(owner, repo);

    // Phase 1: need head SHA before we can query check-runs. Do PR +
    // comment/review counts in parallel (head SHA is not needed for counts).
    let phase1 = run_gh_parallel(
        host,
        vec![
            vec![
                "api".into(),
                format!("repos/{repo_full}/pulls/{pr_number}"),
                "--cache".into(),
                "20s".into(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/issues/{pr_number}/comments"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                "length".into(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/pulls/{pr_number}/reviews"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                "length".into(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/pulls/{pr_number}/comments"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                "length".into(),
            ],
        ],
    )
    .await?;

    #[derive(Deserialize)]
    struct PrJson {
        head: Head,
        #[serde(default)]
        mergeable: Option<bool>,
        #[serde(default)]
        labels: Vec<LabelJson>,
    }
    #[derive(Deserialize)]
    struct Head {
        sha: String,
    }
    #[derive(Deserialize)]
    struct LabelJson {
        name: String,
    }

    let pr: PrJson = serde_json::from_str(&phase1[0]).context("Failed to parse PR JSON")?;
    let head_sha = pr.head.sha;
    let labels: Vec<String> = pr.labels.into_iter().map(|l| l.name).collect();
    let mergeable = pr.mergeable;

    let ic = parse_paginated_lengths(phase1[1].as_bytes());
    let rv = parse_paginated_lengths(phase1[2].as_bytes());
    let rc = parse_paginated_lengths(phase1[3].as_bytes());

    // Phase 2: check-runs for the head SHA.
    let ci_conclusion = fetch_ci_conclusion(host, &repo_full, &head_sha).await;

    let facts = CurrentFacts {
        head_sha: head_sha.clone(),
        ci_conclusion,
        labels,
        mergeable,
    };

    let fingerprint = PrStateFingerprint {
        head_sha,
        comment_count: ic + rv + rc,
        ci_label_hash: facts.ci_label_hash(),
    };

    Ok((fingerprint, facts))
}

/// Fetch check-runs for `head_sha` and roll them up into a single conclusion.
///
/// Returns:
/// - "FAILURE" if any check failed / was cancelled / timed out / action_required
/// - "PENDING" if any check is still running / queued
/// - "SUCCESS" if all checks are success / neutral / skipped
/// - "NONE" if there are no checks or the API call fails (logged as warning)
///
/// Errors are swallowed intentionally: the merge judge is a best-effort
/// advisor and shouldn't fail when CI metadata is unavailable.
async fn fetch_ci_conclusion(host: &str, repo_full: &str, head_sha: &str) -> String {
    let endpoint = format!("repos/{repo_full}/commits/{head_sha}/check-runs");
    let output = match github::run_gh(
        host,
        &[
            "api",
            &endpoint,
            "--cache",
            "20s",
            "--paginate",
            "--jq",
            ".check_runs[] | {status, conclusion}",
        ],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Failed to fetch check-runs for {head_sha}: {e}");
            return "NONE".to_string();
        }
    };

    roll_up_check_runs(&output)
}

#[derive(Deserialize)]
struct CheckRunStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}

/// Roll up newline-delimited check-run JSON (one object per line) into a
/// single conclusion string.
fn roll_up_check_runs(ndjson: &str) -> String {
    let mut any = false;
    let mut pending = false;
    let mut failed = false;

    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(cr) = serde_json::from_str::<CheckRunStatus>(line) else {
            continue;
        };
        any = true;
        if cr.status != "completed" {
            pending = true;
            continue;
        }
        match cr.conclusion.as_deref() {
            Some("success") | Some("neutral") | Some("skipped") => {}
            Some(_) => failed = true,
            None => pending = true,
        }
    }

    if !any {
        "NONE".to_string()
    } else if failed {
        "FAILURE".to_string()
    } else if pending {
        "PENDING".to_string()
    } else {
        "SUCCESS".to_string()
    }
}

/// Parse the output of `gh api --paginate --jq "length"`.
/// Each page outputs a number on its own line; sum them.
fn parse_paginated_lengths(stdout: &[u8]) -> usize {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<usize>().ok())
        .sum()
}

/// Fetch the full PR context for the judge prompt via `gh`.
///
/// Fetches diff, comments, reviews, and review comments in parallel.
/// Uses `--paginate --jq '.[]'` to flatten multi-page JSON arrays into
/// a newline-delimited JSON stream, then wraps in `[...]` for valid JSON.
async fn fetch_pr_context(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<PrContext> {
    let repo_full = github::repo_slug(owner, repo);

    let results = run_gh_parallel(
        host,
        vec![
            vec![
                "pr".into(),
                "diff".into(),
                pr_number.into(),
                "-R".into(),
                repo_full.clone(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/issues/{pr_number}/comments"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                ".[]".into(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/pulls/{pr_number}/reviews"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                ".[]".into(),
            ],
            vec![
                "api".into(),
                format!("repos/{repo_full}/pulls/{pr_number}/comments"),
                "--cache".into(),
                "20s".into(),
                "--paginate".into(),
                "--jq".into(),
                ".[]".into(),
            ],
        ],
    )
    .await?;

    Ok(PrContext {
        diff: results[0].clone(),
        comments: filter_bookkeeping_ndjson(results[1].as_bytes()),
        reviews: filter_bookkeeping_ndjson(results[2].as_bytes()),
        review_comments: filter_bookkeeping_ndjson(results[3].as_bytes()),
    })
}

/// Return true if `body` is Gru's own bookkeeping — not reviewer-feedback signal.
///
/// Matches on body patterns, not on author identity, because only the agent
/// posts these exact shapes. Importantly this does NOT drop substantive
/// Minion replies to reviewers (e.g. "fixed in c9cf557"): those lack these
/// markers and remain in the context as evidence the feedback was addressed.
pub(crate) fn is_bookkeeping_body(body: &str) -> bool {
    // YAML frontmatter notifications: "---\ntype: monitoring-paused\n---",
    // "---\ntype: *-cleared\n---", etc.
    if body.starts_with("---\n") {
        if let Some(after) = body.strip_prefix("---\n") {
            if let Some(end) = after.find("\n---") {
                let fm = &after[..end];
                if fm.lines().any(|l| l.trim_start().starts_with("type:")) {
                    return true;
                }
            }
        }
    }
    // Escalation comments posted by ci.rs.
    if body.contains("## 🚨 CI Fix Escalation") {
        return true;
    }
    // Prior merge-judge verdicts.
    if body.contains("🧑\u{200d}⚖️ **Merge readiness") || body.contains("🧑‍⚖️ **Merge readiness")
    {
        return true;
    }
    // Minion progress updates from progress_comments.rs (contain "progress update").
    if body.contains("🤖 **Minion ") && body.contains("progress update**") {
        return true;
    }
    false
}

/// Filter out bookkeeping entries from newline-delimited JSON objects and
/// re-wrap as a JSON array.
fn filter_bookkeeping_ndjson(stdout: &[u8]) -> String {
    let raw = String::from_utf8_lossy(stdout);
    let mut items: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            // Keep malformed lines rather than silently dropping evidence.
            items.push(line.to_string());
            continue;
        };
        let body = val.get("body").and_then(|b| b.as_str()).unwrap_or("");
        if is_bookkeeping_body(body) {
            continue;
        }
        items.push(line.to_string());
    }
    if items.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", items.join(","))
    }
}

struct PrContext {
    diff: String,
    comments: String,
    reviews: String,
    review_comments: String,
}

/// Render the `## Current facts` block injected before the diff.
fn format_current_facts(facts: &CurrentFacts) -> String {
    let labels = if facts.labels.is_empty() {
        "(none)".to_string()
    } else {
        facts.labels.join(", ")
    };
    let mergeable = match facts.mergeable {
        Some(true) => "true",
        Some(false) => "false (conflicts)",
        None => "unknown (GitHub still computing)",
    };
    format!(
        "## Current facts (authoritative — supersedes any claims in comments below)\n\
         - head SHA: {}\n\
         - CI status: {}\n\
         - Labels: {}\n\
         - Mergeable: {}\n\n\
         These are the live facts as observed right now. Do NOT re-derive CI or \
         label state from comment text. Any comment that claims CI is failing, \
         that `gru:blocked` is present, or that merge is blocked for a reason \
         contradicting the facts above is **stale and superseded** — it describes \
         a previous head or a transient state that has since cleared.\n",
        facts.head_sha, facts.ci_conclusion, labels, mergeable,
    )
}

/// Build the judge prompt with full PR context.
fn build_judge_prompt(
    pr_number: &str,
    context: &PrContext,
    facts: &CurrentFacts,
    consecutive_waits: u32,
    confidence_threshold: u8,
) -> String {
    let force_decide = if consecutive_waits >= MAX_CONSECUTIVE_WAITS - 1 {
        format!(
            "\n\nIMPORTANT: This is your final evaluation opportunity (attempt {} of {}). \
             You have already returned \"wait\" multiple times with no PR state changes. \
             You MUST choose either \"merge\" or \"escalate\" — do NOT return \"wait\" again.\n",
            consecutive_waits + 1,
            MAX_CONSECUTIVE_WAITS
        )
    } else {
        String::new()
    };

    let current_facts = format_current_facts(facts);

    format!(
        r#"You are a merge-readiness judge for PR #{pr_number}. All deterministic checks have already passed (CI green, reviews approved, no conflicts, not draft).

## Scope

Your job is **reviewer-feedback satisfaction only**. You do NOT re-adjudicate deterministic gates (CI, mergeability, labels) — those are captured in the "Current facts" block below and are authoritative. Focus exclusively on whether human reviewer comments have been genuinely addressed.

{current_facts}
Evaluate whether review feedback has been **genuinely addressed** — not just mechanically replied to.

## Evaluation criteria

1. Has every piece of reviewer feedback been addressed? (replied to meaningfully, not just acknowledged)
2. Are there any open questions from reviewers that haven't been answered?
3. Did the reviewer indicate satisfaction (even implicitly, e.g., "looks good", emoji reaction, no further comments after a reasonable time)?
4. Are there "nit" or "optional" comments that don't need to block?
5. How recently did the last interaction happen? Is the reviewer likely still reviewing?

## Confidence threshold

The merge confidence threshold is {confidence_threshold}/10. Only recommend "merge" if your confidence is >= {confidence_threshold}.

## Available actions

- **merge** — All feedback genuinely addressed. Conversations wrapped up. Proceed with merge.
- **wait** — Not confident yet, but the situation may resolve on its own. Specify wait_minutes (max {max_wait}).
  Examples: reviewer posted recently and agent just responded; reviewer said "I'll look again".
- **escalate** — Not confident this will resolve without human input. Explain what's unresolved.
{force_decide}
## PR diff

```
{diff}
```

## PR comments (issue comments)

```json
{comments}
```

## Reviews

```json
{reviews}
```

## Review comments (inline)

```json
{review_comments}
```

## Required output format

Respond with ONLY a JSON object, no other text:

```json
{{
  "confidence": <1-10>,
  "action": "merge" | "wait" | "escalate",
  "wait_minutes": <number, only if action is "wait">,
  "reasoning": "<brief explanation>"
}}
```"#,
        pr_number = pr_number,
        current_facts = current_facts,
        confidence_threshold = confidence_threshold,
        max_wait = MAX_WAIT_MINUTES,
        force_decide = force_decide,
        diff = truncate_if_needed(&context.diff, 50_000),
        comments = truncate_if_needed(&context.comments, 20_000),
        reviews = truncate_if_needed(&context.reviews, 20_000),
        review_comments = truncate_if_needed(&context.review_comments, 20_000),
    )
}

/// Truncate a string if it exceeds the given byte limit, appending a notice.
fn truncate_if_needed(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        // Find a valid UTF-8 boundary.
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut result = s[..end].to_string();
        result.push_str("\n\n[Content truncated due to size limits]");
        result
    }
}

/// Invoke the LLM judge via the agent backend, passing the prompt via stdin.
/// Returns the raw response text on success (even if not parseable as JSON).
async fn invoke_judge_cli_raw(
    backend: &dyn AgentBackend,
    worktree_path: &std::path::Path,
    prompt: &str,
) -> Result<String> {
    let mut cmd = backend.build_oneshot_command(worktree_path, "-");
    cmd.stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn agent backend '{}' for merge judge",
            backend.name()
        )
    })?;

    // Write prompt to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("Failed to write prompt to agent stdin")?;
        // Drop closes stdin, signaling EOF.
    }

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("Failed to wait for agent backend '{}'", backend.name()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Agent backend '{}' exited with error: {}",
            backend.name(),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse the judge's JSON response from potentially noisy LLM output.
fn parse_judge_response(text: &str) -> Result<JudgeResponse> {
    // Try to find JSON in the response (LLM might wrap it in markdown fences).
    let json_str = extract_json(text).context("No JSON object found in judge response")?;

    let raw: JudgeResponseRaw =
        serde_json::from_str(json_str).context("Failed to parse judge JSON")?;

    let action = match raw.action.as_str() {
        "merge" => JudgeAction::Merge,
        "wait" => {
            let minutes = raw.wait_minutes.unwrap_or(15).min(MAX_WAIT_MINUTES);
            JudgeAction::Wait(Duration::from_secs(minutes.saturating_mul(60)))
        }
        "escalate" => JudgeAction::Escalate,
        other => anyhow::bail!("Unknown judge action: {}", other),
    };

    Ok(JudgeResponse {
        confidence: raw.confidence.min(10),
        action,
        reasoning: raw.reasoning,
    })
}

/// Extract the first JSON object from text that may contain markdown fences.
fn extract_json(text: &str) -> Option<&str> {
    // Try finding JSON between code fences first.
    if let Some(start) = text.find("```json") {
        let after_fence = &text[start + 7..];
        if let Some(end) = after_fence.find("```") {
            return Some(after_fence[..end].trim());
        }
    }
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        if let Some(end) = after_fence.find("```") {
            let inner = after_fence[..end].trim();
            if inner.starts_with('{') {
                return Some(inner);
            }
        }
    }

    // Try finding a bare JSON object.
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }

    // Find first '{' and last '}' as fallback.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if start < end {
        Some(&text[start..=end])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the merge-readiness judge and return its decision, or `None` if
/// invocation was skipped (PR state unchanged, no timer expired).
///
/// This is the main entry point. It:
/// 1. Fetches a lightweight fingerprint to check if invocation is needed
/// 2. If needed, fetches full PR context
/// 3. Builds the judge prompt and invokes the LLM
/// 4. Enforces max consecutive waits (coerces to escalate)
/// 5. Returns the response
#[allow(clippy::too_many_arguments)]
pub(crate) async fn evaluate(
    backend: &dyn AgentBackend,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    worktree_path: &std::path::Path,
    state: &mut JudgeState,
    confidence_threshold: u8,
) -> Result<Option<JudgeResponse>> {
    // Lightweight snapshot check first to avoid fetching full context.
    let (fingerprint, facts) = get_pr_snapshot(host, owner, repo, pr_number).await?;

    if !state.should_invoke(&fingerprint) {
        return Ok(None);
    }

    println!("🧑‍⚖️ Invoking merge-readiness judge for PR #{}...", pr_number);

    let context = fetch_pr_context(host, owner, repo, pr_number).await?;

    let prompt = build_judge_prompt(
        pr_number,
        &context,
        &facts,
        state.consecutive_waits(),
        confidence_threshold,
    );

    let raw_response = match invoke_judge_cli_raw(backend, worktree_path, &prompt).await {
        Ok(text) => text,
        Err(e) => {
            log::warn!("Judge CLI invocation failed: {}", e);
            state.record_failure(fingerprint);
            return Err(e);
        }
    };

    let mut response = match parse_judge_response(&raw_response) {
        Ok(r) => r,
        Err(e) => {
            // Log the raw response for diagnostics — truncate to avoid log spam.
            let preview = truncate_if_needed(&raw_response, 500);
            log::warn!(
                "Judge response parse failed: {}. Raw response: {}",
                e,
                preview
            );
            state.record_failure(fingerprint);
            return Err(e);
        }
    };

    // Hard guard: if we've hit max consecutive waits and the LLM still says
    // "wait", coerce to "escalate" to prevent infinite wait loops.
    if state.consecutive_waits() >= MAX_CONSECUTIVE_WAITS - 1 {
        if let JudgeAction::Wait(_) = &response.action {
            log::warn!(
                "Judge returned 'wait' after {} consecutive waits — coercing to 'escalate'",
                state.consecutive_waits() + 1
            );
            response.action = JudgeAction::Escalate;
            response.reasoning = format!(
                "{} [Coerced to escalate after {} consecutive wait responses]",
                response.reasoning,
                state.consecutive_waits() + 1
            );
        }
    }

    // Update state with the same fingerprint used for gating.
    state.record_response(fingerprint, &response);

    println!(
        "🧑‍⚖️ Judge verdict: {} (confidence: {}/10)",
        match &response.action {
            JudgeAction::Merge => "MERGE".to_string(),
            JudgeAction::Wait(d) => format!("WAIT ({}m)", d.as_secs() / 60),
            JudgeAction::Escalate => "ESCALATE".to_string(),
        },
        response.confidence
    );
    println!("   Reasoning: {}", response.reasoning);

    Ok(Some(response))
}

/// Apply the `gru:needs-human-review` label to a PR.
pub(crate) async fn add_needs_human_review_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<()> {
    let repo_full = github::repo_slug(owner, repo);
    github::run_gh(
        host,
        &[
            "pr",
            "edit",
            pr_number,
            "--add-label",
            NEEDS_HUMAN_REVIEW_LABEL,
            "-R",
            &repo_full,
        ],
    )
    .await?;

    Ok(())
}

/// Ensure the `gru:needs-human-review` label exists in the repository.
pub(crate) async fn ensure_needs_human_review_label(
    host: &str,
    owner: &str,
    repo: &str,
) -> Result<()> {
    let (color, description) = labels::get_label_info(NEEDS_HUMAN_REVIEW_LABEL)
        .expect("NEEDS_HUMAN_REVIEW must be in ALL_LABELS");
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/labels");
    let name_field = format!("name={NEEDS_HUMAN_REVIEW_LABEL}");
    let color_field = format!("color={color}");
    let desc_field = format!("description={description}");

    if let Err(e) = github::run_gh(
        host,
        &[
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &name_field,
            "-f",
            &color_field,
            "-f",
            &desc_field,
        ],
    )
    .await
    {
        let msg = e.to_string();
        if !msg.contains("already_exists") {
            log::warn!(
                "Failed to create {} label: {}",
                NEEDS_HUMAN_REVIEW_LABEL,
                msg
            );
        }
    }

    Ok(())
}

/// Check if the `gru:needs-human-review` label is present on a PR.
///
/// Returns `Err` on API failures — the caller should treat errors
/// conservatively (do not proceed with merge if the check fails).
pub(crate) async fn has_needs_human_review_label(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<bool> {
    let repo_full = github::repo_slug(owner, repo);
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");

    let stdout = github::run_gh(host, &["api", &endpoint, "--cache", "20s"]).await?;

    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let labels: Vec<Label> =
        serde_json::from_str(&stdout).context("Failed to parse labels JSON")?;
    Ok(labels.iter().any(|l| l.name == NEEDS_HUMAN_REVIEW_LABEL))
}

/// Post an escalation comment explaining why the judge escalated.
pub(crate) async fn post_judge_escalation_comment(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    response: &JudgeResponse,
) {
    let repo_full = github::repo_slug(owner, repo);
    let body = format!(
        "🧑‍⚖️ **Merge readiness: {}/10 — needs human review**\n\n{}\n\n\
         _To proceed, remove the `gru:needs-human-review` label. \
         The judge will re-evaluate on the next PR state change._",
        response.confidence, response.reasoning
    );

    match github::run_gh(
        host,
        &[
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ],
    )
    .await
    {
        Ok(_) => {
            log::info!("Posted merge judge escalation comment on PR #{}", pr_number);
        }
        Err(e) => {
            log::warn!(
                "Failed to post judge escalation comment on PR #{}: {}",
                pr_number,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_judge_response_merge() {
        let text =
            r#"{"confidence": 9, "action": "merge", "reasoning": "All feedback addressed."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.confidence, 9);
        assert_eq!(resp.action, JudgeAction::Merge);
        assert_eq!(resp.reasoning, "All feedback addressed.");
    }

    #[test]
    fn test_parse_judge_response_wait() {
        let text = r#"{"confidence": 5, "action": "wait", "wait_minutes": 30, "reasoning": "Reviewer just posted."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.confidence, 5);
        assert_eq!(resp.action, JudgeAction::Wait(Duration::from_secs(1800)));
        assert_eq!(resp.reasoning, "Reviewer just posted.");
    }

    #[test]
    fn test_parse_judge_response_wait_default_minutes() {
        let text = r#"{"confidence": 5, "action": "wait", "reasoning": "Need more time."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.action, JudgeAction::Wait(Duration::from_secs(900)));
    }

    #[test]
    fn test_parse_judge_response_wait_clamped() {
        let text = r#"{"confidence": 5, "action": "wait", "wait_minutes": 9999, "reasoning": "Long wait."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(
            resp.action,
            JudgeAction::Wait(Duration::from_secs(MAX_WAIT_MINUTES * 60))
        );
    }

    #[test]
    fn test_parse_judge_response_escalate() {
        let text = r#"{"confidence": 3, "action": "escalate", "reasoning": "Unresolved architectural question."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.confidence, 3);
        assert_eq!(resp.action, JudgeAction::Escalate);
    }

    #[test]
    fn test_parse_judge_response_in_markdown_fence() {
        let text = r#"Here is my assessment:

```json
{"confidence": 8, "action": "merge", "reasoning": "Looks good."}
```

That's my verdict."#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.confidence, 8);
        assert_eq!(resp.action, JudgeAction::Merge);
    }

    #[test]
    fn test_parse_judge_response_confidence_capped() {
        let text = r#"{"confidence": 15, "action": "merge", "reasoning": "Very confident."}"#;
        let resp = parse_judge_response(text).unwrap();
        assert_eq!(resp.confidence, 10);
    }

    #[test]
    fn test_parse_judge_response_invalid_action() {
        let text = r#"{"confidence": 5, "action": "approve", "reasoning": "Something."}"#;
        assert!(parse_judge_response(text).is_err());
    }

    #[test]
    fn test_parse_judge_response_no_json() {
        let text = "I think we should merge this PR.";
        assert!(parse_judge_response(text).is_err());
    }

    #[test]
    fn test_extract_json_bare_object() {
        let text = r#"{"confidence": 8, "action": "merge", "reasoning": "ok"}"#;
        assert_eq!(extract_json(text), Some(text));
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let text =
            r#"Here is my answer: {"confidence": 8, "action": "merge", "reasoning": "ok"} done"#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
    }

    #[test]
    fn test_judge_state_should_invoke_first_time() {
        let state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        assert!(state.should_invoke(&fp));
    }

    #[test]
    fn test_judge_state_no_reinvoke_same_state() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        let response = JudgeResponse {
            confidence: 9,
            action: JudgeAction::Merge,
            reasoning: "ok".to_string(),
        };
        state.record_response(fp.clone(), &response);
        assert!(!state.should_invoke(&fp));
    }

    #[test]
    fn test_judge_state_reinvoke_on_state_change() {
        let mut state = JudgeState::new();
        let fp1 = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        let response = JudgeResponse {
            confidence: 9,
            action: JudgeAction::Merge,
            reasoning: "ok".to_string(),
        };
        state.record_response(fp1, &response);

        let fp2 = PrStateFingerprint {
            head_sha: "def456".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        assert!(state.should_invoke(&fp2));
    }

    #[test]
    fn test_judge_state_consecutive_waits() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        let wait_resp = JudgeResponse {
            confidence: 5,
            action: JudgeAction::Wait(Duration::from_secs(60)),
            reasoning: "waiting".to_string(),
        };

        state.record_response(fp.clone(), &wait_resp);
        assert_eq!(state.consecutive_waits(), 1);

        // Same fingerprint, another wait.
        state.record_response(fp.clone(), &wait_resp);
        assert_eq!(state.consecutive_waits(), 2);

        // State change resets.
        let fp2 = PrStateFingerprint {
            head_sha: "def".to_string(),
            comment_count: 2,
            ci_label_hash: 0,
        };
        state.record_response(fp2, &wait_resp);
        assert_eq!(state.consecutive_waits(), 1);
    }

    #[test]
    fn test_judge_state_escalation_label_tracking() {
        let mut state = JudgeState::new();
        assert!(!state.label_was_applied());

        // Marking label applied works.
        state.mark_label_applied();
        assert!(state.label_was_applied());

        // Clearing escalation resets fingerprint.
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        let resp = JudgeResponse {
            confidence: 3,
            action: JudgeAction::Escalate,
            reasoning: "needs review".to_string(),
        };
        state.record_response(fp.clone(), &resp);
        assert!(!state.should_invoke(&fp)); // Same state, shouldn't invoke.

        state.mark_escalation_cleared();
        assert!(!state.label_was_applied());
        assert!(state.should_invoke(&fp)); // Fingerprint reset, should invoke.
    }

    #[test]
    fn test_judge_state_mark_escalation_cleared_noop_without_label() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        let resp = JudgeResponse {
            confidence: 9,
            action: JudgeAction::Merge,
            reasoning: "ok".to_string(),
        };
        state.record_response(fp.clone(), &resp);

        // Clearing without label_applied should be a no-op.
        state.mark_escalation_cleared();
        assert!(!state.should_invoke(&fp)); // Fingerprint NOT reset.
    }

    #[test]
    fn test_judge_state_record_failure_sets_backoff() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 1);
        // Backoff timer is set, so should_invoke returns false immediately.
        assert!(!state.should_invoke(&fp));
        // But retry_after is set (not None), so it will eventually retry.
        assert!(state.retry_after.is_some());
    }

    #[test]
    fn test_judge_state_failure_counter_increments() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 1);
        assert!(!state.should_escalate_on_failure());

        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 2);
        assert!(!state.should_escalate_on_failure());

        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 3);
        assert!(state.should_escalate_on_failure());
    }

    #[test]
    fn test_judge_state_failure_counter_resets_on_state_change() {
        let mut state = JudgeState::new();
        let fp1 = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        state.record_failure(fp1);
        state.record_failure(PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        });
        assert_eq!(state.consecutive_failures(), 2);

        // Different fingerprint resets the counter.
        let fp2 = PrStateFingerprint {
            head_sha: "def".to_string(),
            comment_count: 2,
            ci_label_hash: 0,
        };
        state.record_failure(fp2);
        assert_eq!(state.consecutive_failures(), 1);
    }

    #[test]
    fn test_judge_state_success_resets_failure_counter() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        state.record_failure(fp.clone());
        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 2);

        // A successful response resets failures.
        let resp = JudgeResponse {
            confidence: 9,
            action: JudgeAction::Merge,
            reasoning: "ok".to_string(),
        };
        state.record_response(fp, &resp);
        assert_eq!(state.consecutive_failures(), 0);
    }

    #[test]
    fn test_judge_state_failure_at_cap_blocks_reinvocation() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };

        // Hit the failure cap.
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            state.record_failure(fp.clone());
        }
        assert_eq!(state.consecutive_failures(), MAX_CONSECUTIVE_FAILURES);
        // At cap, should_invoke returns false even if backoff expired.
        state.retry_after = None;
        assert!(!state.should_invoke(&fp));

        // But a new fingerprint should trigger invocation.
        let fp2 = PrStateFingerprint {
            head_sha: "def456".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        assert!(state.should_invoke(&fp2));
    }

    #[test]
    fn test_judge_state_failure_retry_allowed_after_backoff() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        state.record_failure(fp.clone());
        assert_eq!(state.consecutive_failures(), 1);

        // Backoff hasn't expired — should not invoke.
        assert!(!state.should_invoke(&fp));

        // Simulate backoff expiring.
        state.retry_after = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(state.should_invoke(&fp));
    }

    #[test]
    fn test_judge_state_failure_escalated_guard() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            state.record_failure(fp.clone());
        }
        assert!(state.should_escalate_on_failure());

        // After marking escalated, should_escalate_on_failure returns false.
        state.mark_failure_escalated();
        assert!(!state.should_escalate_on_failure());
    }

    #[test]
    fn test_judge_state_failure_resets_stale_wait_on_state_change() {
        let mut state = JudgeState::new();
        let fp1 = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };
        let wait_resp = JudgeResponse {
            confidence: 5,
            action: JudgeAction::Wait(Duration::from_secs(60)),
            reasoning: "waiting".to_string(),
        };
        state.record_response(fp1, &wait_resp);
        assert_eq!(state.consecutive_waits(), 1);
        assert!(state.wait_until.is_some());

        // Failure on a new fingerprint should reset wait state.
        let fp2 = PrStateFingerprint {
            head_sha: "def".to_string(),
            comment_count: 2,
            ci_label_hash: 0,
        };
        state.record_failure(fp2);
        assert_eq!(state.consecutive_waits(), 0);
        assert!(state.wait_until.is_none());
    }

    #[test]
    fn test_judge_state_escalation_cleared_resets_failure_counter() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
            ci_label_hash: 0,
        };

        // Accumulate failures up to the escalation threshold.
        state.record_failure(fp.clone());
        state.record_failure(fp.clone());
        state.record_failure(fp.clone());
        assert!(state.should_escalate_on_failure());

        // Simulate label being applied after failure escalation.
        state.mark_label_applied();

        // Human clears the label — should reset failure counter and fingerprint.
        state.mark_escalation_cleared();
        assert_eq!(state.consecutive_failures(), 0);
        assert!(!state.should_escalate_on_failure());
        // Fingerprint is cleared, so should_invoke returns true.
        assert!(state.should_invoke(&fp));
    }

    #[test]
    fn test_truncate_if_needed() {
        let s = "hello world";
        assert_eq!(truncate_if_needed(s, 100), s);
        assert!(truncate_if_needed(s, 5).starts_with("hello"));
        assert!(truncate_if_needed(s, 5).contains("[Content truncated"));
    }

    #[test]
    fn test_truncate_if_needed_utf8_boundary() {
        let s = "héllo";
        // 'é' is 2 bytes, so "hé" = 3 bytes. Truncating at 2 should give "h" + notice.
        let result = truncate_if_needed(s, 2);
        assert!(result.starts_with('h'));
        assert!(result.contains("[Content truncated"));
    }

    #[test]
    fn test_filter_bookkeeping_ndjson_empty() {
        assert_eq!(filter_bookkeeping_ndjson(b""), "[]");
        assert_eq!(filter_bookkeeping_ndjson(b"  \n  "), "[]");
    }

    #[test]
    fn test_filter_bookkeeping_ndjson_single_object() {
        let input = br#"{"id":1,"body":"hello"}"#;
        let result = filter_bookkeeping_ndjson(input);
        assert_eq!(result, r#"[{"id":1,"body":"hello"}]"#);
    }

    #[test]
    fn test_filter_bookkeeping_ndjson_multiple_objects() {
        let input =
            b"{\"id\":1,\"body\":\"a\"}\n{\"id\":2,\"body\":\"b\"}\n{\"id\":3,\"body\":\"c\"}";
        let result = filter_bookkeeping_ndjson(input);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn test_parse_paginated_lengths() {
        assert_eq!(parse_paginated_lengths(b"5\n3\n2\n"), 10);
        assert_eq!(parse_paginated_lengths(b"0\n"), 0);
        assert_eq!(parse_paginated_lengths(b""), 0);
        assert_eq!(parse_paginated_lengths(b"10\n"), 10);
    }

    fn test_facts() -> CurrentFacts {
        CurrentFacts {
            head_sha: "deadbeef".to_string(),
            ci_conclusion: "SUCCESS".to_string(),
            labels: vec!["gru:auto-merge".to_string()],
            mergeable: Some(true),
        }
    }

    #[test]
    fn test_build_judge_prompt_includes_threshold() {
        let ctx = PrContext {
            diff: "some diff".to_string(),
            comments: "[]".to_string(),
            reviews: "[]".to_string(),
            review_comments: "[]".to_string(),
        };
        let prompt = build_judge_prompt("42", &ctx, &test_facts(), 0, 8);
        assert!(prompt.contains("confidence threshold is 8/10"));
        assert!(!prompt.contains("IMPORTANT: This is your final evaluation"));
        assert!(prompt.contains("## Current facts"));
        assert!(prompt.contains("head SHA: deadbeef"));
        assert!(prompt.contains("CI status: SUCCESS"));
    }

    #[test]
    fn test_build_judge_prompt_force_decide_on_max_waits() {
        let ctx = PrContext {
            diff: "diff".to_string(),
            comments: "[]".to_string(),
            reviews: "[]".to_string(),
            review_comments: "[]".to_string(),
        };
        // MAX_CONSECUTIVE_WAITS - 1 = 2, so at 2 consecutive waits, force decide.
        let prompt = build_judge_prompt("42", &ctx, &test_facts(), 2, 8);
        assert!(prompt.contains("IMPORTANT: This is your final evaluation"));
        assert!(prompt.contains("attempt 3 of 3"));
    }

    #[test]
    fn test_is_bookkeeping_body_detects_monitoring_paused_frontmatter() {
        let body = "---\ntype: monitoring-paused\n---\n\n⏸️ This PR's automated agent has paused.";
        assert!(is_bookkeeping_body(body));
    }

    #[test]
    fn test_is_bookkeeping_body_detects_ci_fix_escalation() {
        let body = "## 🚨 CI Fix Escalation\n\nBuild failed: foo\n\n<sub>🤖 M1cu</sub>";
        assert!(is_bookkeeping_body(body));
    }

    #[test]
    fn test_is_bookkeeping_body_detects_prior_judge_verdict() {
        let body = "🧑‍⚖️ **Merge readiness: 3/10 — needs human review**\n\nStale.";
        assert!(is_bookkeeping_body(body));
    }

    #[test]
    fn test_is_bookkeeping_body_preserves_minion_reply_to_reviewer() {
        // Substantive Minion reply with a signature — must NOT be filtered.
        let body = "Fixed in c9cf557 — switched from `interval_secs` to \
                    `interval_millis` as requested.\n\n<sub>🤖 M1cu</sub>";
        assert!(!is_bookkeeping_body(body));
    }

    #[test]
    fn test_is_bookkeeping_body_preserves_reviewer_comment() {
        let body = "Please rename `interval_secs` to `interval_millis`.";
        assert!(!is_bookkeeping_body(body));
    }

    #[test]
    fn test_filter_bookkeeping_ndjson_drops_bookkeeping_preserves_signal() {
        // Reproduces the verso#275 stream: monitoring-paused + stale CI
        // escalation + prior 3/10 verdict all stripped; reviewer request
        // and Minion substantive reply preserved.
        // Each line is a JSON object; body values embed escaped \n pairs.
        let lines = [
            "{\"id\":1,\"body\":\"---\\ntype: monitoring-paused\\n---\\n\\nPaused\"}",
            "{\"id\":2,\"body\":\"## \u{1f6a8} CI Fix Escalation\\n\\nfailing on interval_secs\"}",
            "{\"id\":3,\"body\":\"\u{1f9d1}\u{200d}\u{2696}\u{fe0f} **Merge readiness: 3/10 \u{2014} needs human review**\\n\\nCI failing\"}",
            "{\"id\":4,\"body\":\"Please rename interval_secs to interval_millis.\"}",
            "{\"id\":5,\"body\":\"Fixed in c9cf557.\\n\\n<sub>\u{1f916} M1cu</sub>\"}",
        ];
        let ndjson = lines.join("\n");
        let out = filter_bookkeeping_ndjson(ndjson.as_bytes());
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        let ids: Vec<u64> = parsed
            .iter()
            .filter_map(|v| v.get("id").and_then(|i| i.as_u64()))
            .collect();
        assert_eq!(ids, vec![4, 5]);
    }

    #[test]
    fn test_roll_up_check_runs_all_success() {
        let nd = r#"{"status":"completed","conclusion":"success"}
{"status":"completed","conclusion":"success"}"#;
        assert_eq!(roll_up_check_runs(nd), "SUCCESS");
    }

    #[test]
    fn test_roll_up_check_runs_any_failure() {
        let nd = r#"{"status":"completed","conclusion":"success"}
{"status":"completed","conclusion":"failure"}"#;
        assert_eq!(roll_up_check_runs(nd), "FAILURE");
    }

    #[test]
    fn test_roll_up_check_runs_pending() {
        let nd = r#"{"status":"completed","conclusion":"success"}
{"status":"in_progress","conclusion":null}"#;
        assert_eq!(roll_up_check_runs(nd), "PENDING");
    }

    #[test]
    fn test_roll_up_check_runs_empty_is_none() {
        assert_eq!(roll_up_check_runs(""), "NONE");
    }

    #[test]
    fn test_roll_up_check_runs_failure_beats_pending() {
        // Priority: FAILURE > PENDING > SUCCESS. A failed check dominates
        // even when another check is still in progress.
        let nd = r#"{"status":"completed","conclusion":"failure"}
{"status":"in_progress","conclusion":null}"#;
        assert_eq!(roll_up_check_runs(nd), "FAILURE");
    }

    #[test]
    fn test_ci_label_hash_differs_for_different_ci() {
        let green = CurrentFacts {
            head_sha: "abc".to_string(),
            ci_conclusion: "SUCCESS".to_string(),
            labels: vec!["gru:auto-merge".to_string()],
            mergeable: Some(true),
        };
        let red = CurrentFacts {
            ci_conclusion: "FAILURE".to_string(),
            ..green.clone()
        };
        assert_ne!(green.ci_label_hash(), red.ci_label_hash());
    }

    #[test]
    fn test_ci_label_hash_label_order_independent() {
        let a = CurrentFacts {
            head_sha: "abc".to_string(),
            ci_conclusion: "SUCCESS".to_string(),
            labels: vec!["a".to_string(), "b".to_string()],
            mergeable: Some(true),
        };
        let b = CurrentFacts {
            labels: vec!["b".to_string(), "a".to_string()],
            ..a.clone()
        };
        assert_eq!(a.ci_label_hash(), b.ci_label_hash());
    }

    #[test]
    fn test_fingerprint_ci_change_triggers_reinvoke_on_same_head() {
        // Regression for the "CI flips red→green on same head with no new
        // comments" gap. Pre-fix the judge would skip re-evaluation.
        let mut state = JudgeState::new();
        let red_fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 5,
            ci_label_hash: 1111, // hash of (FAILURE, labels)
        };
        let response = JudgeResponse {
            confidence: 3,
            action: JudgeAction::Escalate,
            reasoning: "CI red".to_string(),
        };
        state.record_response(red_fp, &response);

        let green_fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 5,
            ci_label_hash: 2222, // hash of (SUCCESS, labels)
        };
        assert!(
            state.should_invoke(&green_fp),
            "judge must re-invoke when CI flips on same head"
        );
    }

    // --- T7: Wait timer expiry test ---

    #[test]
    fn test_judge_state_wait_timer_expiry() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        // Record a Wait response with Duration::ZERO so wait_until = now + 0,
        // meaning the timer is already expired by the time we check.
        let response = JudgeResponse {
            confidence: 5,
            action: JudgeAction::Wait(Duration::ZERO),
            reasoning: "need more time".to_string(),
        };
        state.record_response(fp.clone(), &response);

        // should_invoke should return true because the timer is already expired.
        assert!(
            state.should_invoke(&fp),
            "should_invoke must return true after wait timer expires"
        );
    }

    #[test]
    fn test_judge_state_wait_timer_not_expired() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
            ci_label_hash: 0,
        };
        // Record a Wait response with a very long duration.
        let response = JudgeResponse {
            confidence: 5,
            action: JudgeAction::Wait(Duration::from_secs(3600)),
            reasoning: "need more time".to_string(),
        };
        state.record_response(fp.clone(), &response);

        // Same fingerprint, timer should NOT have expired.
        assert!(
            !state.should_invoke(&fp),
            "should_invoke must return false while wait timer is active"
        );
    }
}
