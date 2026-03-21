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
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;

use crate::github;
use crate::labels;

/// Default confidence threshold (1-10). Only merge when confidence >= this.
pub(crate) const DEFAULT_CONFIDENCE_THRESHOLD: u8 = 8;

/// Maximum consecutive wait responses before the judge must decide merge or escalate.
const MAX_CONSECUTIVE_WAITS: u32 = 3;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrStateFingerprint {
    pub(crate) head_sha: String,
    pub(crate) comment_count: usize,
}

/// Tracks the judge's state across poll iterations.
#[derive(Debug)]
pub(crate) struct JudgeState {
    /// Last PR state we evaluated.
    last_fingerprint: Option<PrStateFingerprint>,
    /// Number of consecutive "wait" responses with no state change.
    consecutive_waits: u32,
    /// When the current wait expires (if any).
    wait_until: Option<DateTime<Utc>>,
    /// Whether the judge has escalated and the label was confirmed applied.
    label_applied: bool,
}

impl JudgeState {
    pub(crate) fn new() -> Self {
        Self {
            last_fingerprint: None,
            consecutive_waits: 0,
            wait_until: None,
            label_applied: false,
        }
    }

    /// Returns true if the judge should be invoked for the given PR state.
    pub(crate) fn should_invoke(&self, fingerprint: &PrStateFingerprint) -> bool {
        // If PR state changed, always re-invoke.
        if self.last_fingerprint.as_ref() != Some(fingerprint) {
            return true;
        }

        // Same state — only invoke if a wait timer expired.
        if let Some(until) = self.wait_until {
            return Utc::now() >= until;
        }

        // Same state, no wait timer — don't re-invoke.
        false
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
            // Reset fingerprint so the judge re-evaluates on next check.
            self.last_fingerprint = None;
        }
    }
}

/// Lightweight fingerprint fetch — only head SHA + comment counts.
/// Used to check `should_invoke` before fetching full context.
pub(crate) async fn get_pr_fingerprint(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<PrStateFingerprint> {
    let repo_full = github::repo_slug(owner, repo);

    let pr_fut = {
        let host = host.to_string();
        let ep = format!("repos/{repo_full}/pulls/{pr_number}");
        async move {
            let stdout = github::run_gh(&host, &["api", &ep]).await?;
            #[derive(Deserialize)]
            struct PrHead {
                head: Head,
            }
            #[derive(Deserialize)]
            struct Head {
                sha: String,
            }
            let pr: PrHead = serde_json::from_str(&stdout).context("Failed to parse PR JSON")?;
            Ok::<String, anyhow::Error>(pr.head.sha)
        }
    };

    let ic_fut = {
        let host = host.to_string();
        let ep = format!("repos/{repo_full}/issues/{pr_number}/comments");
        async move {
            let stdout =
                github::run_gh(&host, &["api", &ep, "--paginate", "--jq", "length"]).await?;
            Ok::<usize, anyhow::Error>(parse_paginated_lengths(stdout.as_bytes()))
        }
    };

    let rv_fut = {
        let host = host.to_string();
        let ep = format!("repos/{repo_full}/pulls/{pr_number}/reviews");
        async move {
            let stdout =
                github::run_gh(&host, &["api", &ep, "--paginate", "--jq", "length"]).await?;
            Ok::<usize, anyhow::Error>(parse_paginated_lengths(stdout.as_bytes()))
        }
    };

    let rc_fut = {
        let host = host.to_string();
        let ep = format!("repos/{repo_full}/pulls/{pr_number}/comments");
        async move {
            let stdout =
                github::run_gh(&host, &["api", &ep, "--paginate", "--jq", "length"]).await?;
            Ok::<usize, anyhow::Error>(parse_paginated_lengths(stdout.as_bytes()))
        }
    };

    let (head_sha, ic, rv, rc) = tokio::try_join!(pr_fut, ic_fut, rv_fut, rc_fut)?;

    Ok(PrStateFingerprint {
        head_sha,
        comment_count: ic + rv + rc,
    })
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
async fn fetch_pr_context(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<PrContext> {
    let repo_full = github::repo_slug(owner, repo);

    // Fetch diff, comments, reviews, and review comments in parallel.
    // Use `--paginate --jq '.[]'` to flatten multi-page JSON arrays into
    // a newline-delimited JSON stream, then wrap in `[...]` for valid JSON.
    let diff_fut = {
        let host = host.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let stdout = github::run_gh(&host, &["pr", "diff", &pr, "-R", &rf]).await?;
            Ok::<String, anyhow::Error>(stdout)
        }
    };

    let comments_fut = {
        let host = host.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/issues/{pr}/comments");
            let stdout =
                github::run_gh(&host, &["api", &endpoint, "--paginate", "--jq", ".[]"]).await?;
            Ok::<String, anyhow::Error>(wrap_ndjson(stdout.as_bytes()))
        }
    };

    let reviews_fut = {
        let host = host.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/pulls/{pr}/reviews");
            let stdout =
                github::run_gh(&host, &["api", &endpoint, "--paginate", "--jq", ".[]"]).await?;
            Ok::<String, anyhow::Error>(wrap_ndjson(stdout.as_bytes()))
        }
    };

    let review_comments_fut = {
        let host = host.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/pulls/{pr}/comments");
            let stdout =
                github::run_gh(&host, &["api", &endpoint, "--paginate", "--jq", ".[]"]).await?;
            Ok::<String, anyhow::Error>(wrap_ndjson(stdout.as_bytes()))
        }
    };

    let (diff, comments, reviews, review_comments) =
        tokio::try_join!(diff_fut, comments_fut, reviews_fut, review_comments_fut)?;

    Ok(PrContext {
        diff,
        comments,
        reviews,
        review_comments,
    })
}

/// Wrap newline-delimited JSON objects into a JSON array string.
fn wrap_ndjson(stdout: &[u8]) -> String {
    let raw = String::from_utf8_lossy(stdout);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "[]".to_string();
    }
    // Each line from `--jq '.[]'` is a complete JSON object.
    // Join them with commas and wrap in brackets.
    let items: Vec<&str> = trimmed.lines().collect();
    format!("[{}]", items.join(","))
}

struct PrContext {
    diff: String,
    comments: String,
    reviews: String,
    review_comments: String,
}

/// Build the judge prompt with full PR context.
fn build_judge_prompt(
    pr_number: &str,
    context: &PrContext,
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

    format!(
        r#"You are a merge-readiness judge for PR #{pr_number}. All deterministic checks have already passed (CI green, reviews approved, no conflicts, not draft).

Your job is to evaluate whether review feedback has been **genuinely addressed** — not just mechanically replied to.

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

/// Invoke the LLM judge via Claude CLI, passing the prompt via stdin.
async fn invoke_judge_cli(worktree_path: &std::path::Path, prompt: &str) -> Result<JudgeResponse> {
    let mut child = TokioCommand::new("claude")
        .arg("--print")
        .arg("--output-format")
        .arg("text")
        .arg("--dangerously-skip-permissions")
        .arg("--max-turns")
        .arg("1")
        .arg("-") // read prompt from stdin
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn Claude CLI for merge judge")?;

    // Write prompt to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("Failed to write prompt to Claude stdin")?;
        // Drop closes stdin, signaling EOF.
    }

    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for Claude CLI")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Claude CLI exited with error: {}", stderr.trim());
    }

    let response_text = String::from_utf8_lossy(&output.stdout);
    parse_judge_response(&response_text)
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
pub(crate) async fn evaluate(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: &str,
    worktree_path: &std::path::Path,
    state: &mut JudgeState,
    confidence_threshold: u8,
) -> Result<Option<JudgeResponse>> {
    // Lightweight fingerprint check first to avoid fetching full context.
    let fingerprint = get_pr_fingerprint(host, owner, repo, pr_number).await?;

    if !state.should_invoke(&fingerprint) {
        return Ok(None);
    }

    println!("🧑‍⚖️ Invoking merge-readiness judge for PR #{}...", pr_number);

    let context = fetch_pr_context(host, owner, repo, pr_number).await?;

    let prompt = build_judge_prompt(
        pr_number,
        &context,
        state.consecutive_waits(),
        confidence_threshold,
    );

    let mut response = invoke_judge_cli(worktree_path, &prompt).await?;

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

    let stdout = github::run_gh(host, &["api", &endpoint]).await?;

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
        };
        assert!(state.should_invoke(&fp));
    }

    #[test]
    fn test_judge_state_no_reinvoke_same_state() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
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
        };
        assert!(state.should_invoke(&fp2));
    }

    #[test]
    fn test_judge_state_consecutive_waits() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc".to_string(),
            comment_count: 1,
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
    fn test_wrap_ndjson_empty() {
        assert_eq!(wrap_ndjson(b""), "[]");
        assert_eq!(wrap_ndjson(b"  \n  "), "[]");
    }

    #[test]
    fn test_wrap_ndjson_single_object() {
        let input = br#"{"id":1,"body":"hello"}"#;
        let result = wrap_ndjson(input);
        assert_eq!(result, r#"[{"id":1,"body":"hello"}]"#);
    }

    #[test]
    fn test_wrap_ndjson_multiple_objects() {
        let input = b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}";
        let result = wrap_ndjson(input);
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

    #[test]
    fn test_build_judge_prompt_includes_threshold() {
        let ctx = PrContext {
            diff: "some diff".to_string(),
            comments: "[]".to_string(),
            reviews: "[]".to_string(),
            review_comments: "[]".to_string(),
        };
        let prompt = build_judge_prompt("42", &ctx, 0, 8);
        assert!(prompt.contains("confidence threshold is 8/10"));
        assert!(!prompt.contains("IMPORTANT: This is your final evaluation"));
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
        let prompt = build_judge_prompt("42", &ctx, 2, 8);
        assert!(prompt.contains("IMPORTANT: This is your final evaluation"));
        assert!(prompt.contains("attempt 3 of 3"));
    }

    // --- T7: Wait timer expiry test ---

    #[test]
    fn test_judge_state_wait_timer_expiry() {
        let mut state = JudgeState::new();
        let fp = PrStateFingerprint {
            head_sha: "abc123".to_string(),
            comment_count: 5,
        };
        // Record a Wait response with a 1ms duration so it expires immediately.
        let response = JudgeResponse {
            confidence: 5,
            action: JudgeAction::Wait(Duration::from_millis(1)),
            reasoning: "need more time".to_string(),
        };
        state.record_response(fp.clone(), &response);

        // Same fingerprint, timer not expired yet (in practice it may already be).
        // Sleep to guarantee expiry.
        std::thread::sleep(Duration::from_millis(5));

        // Now should_invoke should return true because the timer expired.
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
