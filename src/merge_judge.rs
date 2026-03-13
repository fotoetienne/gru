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
use tokio::process::Command as TokioCommand;

use crate::github::gh_command_for_repo;

/// Default confidence threshold (1-10). Only merge when confidence >= this.
pub const DEFAULT_CONFIDENCE_THRESHOLD: u8 = 8;

/// Maximum consecutive wait responses before the judge must decide merge or escalate.
const MAX_CONSECUTIVE_WAITS: u32 = 3;

/// Label applied when the judge escalates for human review.
const NEEDS_HUMAN_REVIEW_LABEL: &str = "gru:needs-human-review";

/// Action the judge can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeAction {
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
pub struct JudgeResponse {
    pub confidence: u8,
    pub action: JudgeAction,
    pub reasoning: String,
}

/// Fingerprint of PR state used to avoid redundant judge invocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStateFingerprint {
    pub head_sha: String,
    pub comment_count: usize,
}

/// Tracks the judge's state across poll iterations.
#[derive(Debug)]
pub struct JudgeState {
    /// Last PR state we evaluated.
    last_fingerprint: Option<PrStateFingerprint>,
    /// Number of consecutive "wait" responses with no state change.
    consecutive_waits: u32,
    /// When the current wait expires (if any).
    wait_until: Option<DateTime<Utc>>,
    /// Whether the judge has escalated (applied `gru:needs-human-review`).
    has_escalated: bool,
}

impl JudgeState {
    pub fn new() -> Self {
        Self {
            last_fingerprint: None,
            consecutive_waits: 0,
            wait_until: None,
            has_escalated: false,
        }
    }

    /// Returns true if the judge should be invoked for the given PR state.
    pub fn should_invoke(&self, fingerprint: &PrStateFingerprint) -> bool {
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
    pub fn record_response(&mut self, fingerprint: PrStateFingerprint, response: &JudgeResponse) {
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
                self.has_escalated = true;
            }
            JudgeAction::Merge => {
                self.consecutive_waits = 0;
                self.wait_until = None;
            }
        }
    }

    /// Returns the number of consecutive waits with no state change.
    pub fn consecutive_waits(&self) -> u32 {
        self.consecutive_waits
    }

    /// Returns true if the judge has previously escalated.
    pub fn has_escalated(&self) -> bool {
        self.has_escalated
    }

    /// Note that `gru:needs-human-review` was cleared by a human.
    /// Only has effect if the judge previously escalated.
    pub fn mark_escalation_cleared(&mut self) {
        if self.has_escalated {
            self.has_escalated = false;
            // Reset fingerprint so the judge re-evaluates on next check.
            self.last_fingerprint = None;
        }
    }
}

/// Fetch the full PR context for the judge prompt via `gh`.
///
/// Also returns the head SHA and total comment count for fingerprinting.
async fn fetch_pr_context(owner: &str, repo: &str, pr_number: &str) -> Result<PrContext> {
    let repo_full = format!("{owner}/{repo}");
    let gh_cmd = gh_command_for_repo(&repo_full);

    // Fetch PR details (for head SHA), diff, comments, and reviews in parallel.
    let pr_details_fut = {
        let gh = gh_cmd.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/pulls/{pr}");
            let output = TokioCommand::new(&gh)
                .args(["api", &endpoint])
                .output()
                .await
                .context("Failed to fetch PR details")?;
            #[derive(Deserialize)]
            struct PrHead {
                head: Head,
            }
            #[derive(Deserialize)]
            struct Head {
                sha: String,
            }
            let pr: PrHead = serde_json::from_slice(&output.stdout)
                .context("Failed to parse PR details JSON")?;
            Ok::<String, anyhow::Error>(pr.head.sha)
        }
    };

    let diff_fut = {
        let gh = gh_cmd.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let output = TokioCommand::new(&gh)
                .args(["pr", "diff", &pr, "-R", &rf])
                .output()
                .await
                .context("Failed to fetch PR diff")?;
            Ok::<String, anyhow::Error>(String::from_utf8_lossy(&output.stdout).to_string())
        }
    };

    let comments_fut = {
        let gh = gh_cmd.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/issues/{pr}/comments");
            let output = TokioCommand::new(&gh)
                .args(["api", &endpoint, "--paginate"])
                .output()
                .await
                .context("Failed to fetch PR comments")?;
            Ok::<String, anyhow::Error>(String::from_utf8_lossy(&output.stdout).to_string())
        }
    };

    let reviews_fut = {
        let gh = gh_cmd.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/pulls/{pr}/reviews");
            let output = TokioCommand::new(&gh)
                .args(["api", &endpoint, "--paginate"])
                .output()
                .await
                .context("Failed to fetch PR reviews")?;
            Ok::<String, anyhow::Error>(String::from_utf8_lossy(&output.stdout).to_string())
        }
    };

    let review_comments_fut = {
        let gh = gh_cmd.to_string();
        let rf = repo_full.clone();
        let pr = pr_number.to_string();
        async move {
            let endpoint = format!("repos/{rf}/pulls/{pr}/comments");
            let output = TokioCommand::new(&gh)
                .args(["api", &endpoint, "--paginate"])
                .output()
                .await
                .context("Failed to fetch review comments")?;
            Ok::<String, anyhow::Error>(String::from_utf8_lossy(&output.stdout).to_string())
        }
    };

    let (head_sha, diff, comments, reviews, review_comments) = tokio::try_join!(
        pr_details_fut,
        diff_fut,
        comments_fut,
        reviews_fut,
        review_comments_fut
    )?;

    // Count total comments for fingerprinting (all 3 sources for consistency).
    let comment_count = count_json_array_items(&comments)
        + count_json_array_items(&reviews)
        + count_json_array_items(&review_comments);

    Ok(PrContext {
        head_sha,
        diff,
        comments,
        reviews,
        review_comments,
        comment_count,
    })
}

/// Count items in a JSON array string. Returns 0 on parse failure.
fn count_json_array_items(json: &str) -> usize {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .map(|v| v.len())
        .unwrap_or(0)
}

struct PrContext {
    head_sha: String,
    diff: String,
    comments: String,
    reviews: String,
    review_comments: String,
    comment_count: usize,
}

/// Build the judge prompt with full PR context.
fn build_judge_prompt(
    pr_number: &str,
    context: &PrContext,
    consecutive_waits: u32,
    confidence_threshold: u8,
) -> String {
    let force_decide = if consecutive_waits >= MAX_CONSECUTIVE_WAITS - 1 {
        "\n\nIMPORTANT: This is your final evaluation opportunity. You have already returned \
         \"wait\" multiple times with no PR state changes. You MUST choose either \"merge\" \
         or \"escalate\" — do NOT return \"wait\" again.\n"
    } else {
        ""
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
- **wait** — Not confident yet, but the situation may resolve on its own. Specify wait_minutes.
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

/// Invoke the LLM judge via Claude CLI and parse the response.
async fn invoke_judge_cli(worktree_path: &std::path::Path, prompt: &str) -> Result<JudgeResponse> {
    let output = TokioCommand::new("claude")
        .arg("--print")
        .arg("--output-format")
        .arg("text")
        .arg("--dangerously-skip-permissions")
        .arg("--max-turns")
        .arg("1")
        .arg(prompt)
        .current_dir(worktree_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("Failed to invoke Claude CLI for merge judge")?;

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
            let minutes = raw.wait_minutes.unwrap_or(15);
            JudgeAction::Wait(Duration::from_secs(minutes * 60))
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

/// Run the merge-readiness judge and return its decision.
///
/// This is the main entry point. It:
/// 1. Fetches full PR context (including head SHA and comment counts)
/// 2. Computes a fingerprint to decide whether invocation is needed
/// 3. Builds the judge prompt
/// 4. Invokes the LLM
/// 5. Parses and returns the response
pub async fn evaluate(
    owner: &str,
    repo: &str,
    pr_number: &str,
    worktree_path: &std::path::Path,
    state: &mut JudgeState,
    confidence_threshold: u8,
) -> Result<JudgeResponse> {
    // Fetch context first — the fingerprint is derived from the same data.
    let context = fetch_pr_context(owner, repo, pr_number).await?;

    let fingerprint = PrStateFingerprint {
        head_sha: context.head_sha.clone(),
        comment_count: context.comment_count,
    };

    if !state.should_invoke(&fingerprint) {
        anyhow::bail!("Judge invocation skipped — PR state unchanged and no wait timer expired");
    }

    println!("🧑‍⚖️ Invoking merge-readiness judge for PR #{}...", pr_number);

    let prompt = build_judge_prompt(
        pr_number,
        &context,
        state.consecutive_waits(),
        confidence_threshold,
    );

    let response = invoke_judge_cli(worktree_path, &prompt).await?;

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

    Ok(response)
}

/// Apply the `gru:needs-human-review` label to a PR.
pub async fn add_needs_human_review_label(owner: &str, repo: &str, pr_number: &str) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let gh_cmd = gh_command_for_repo(&repo_full);
    let output = TokioCommand::new(gh_cmd)
        .args([
            "pr",
            "edit",
            pr_number,
            "--add-label",
            NEEDS_HUMAN_REVIEW_LABEL,
            "-R",
            &repo_full,
        ])
        .output()
        .await
        .context("Failed to add gru:needs-human-review label")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to add gru:needs-human-review label to PR #{}: {}",
            pr_number,
            stderr
        );
    }

    Ok(())
}

/// Ensure the `gru:needs-human-review` label exists in the repository.
pub async fn ensure_needs_human_review_label(owner: &str, repo: &str) -> Result<()> {
    let repo_full = format!("{owner}/{repo}");
    let gh_cmd = gh_command_for_repo(&repo_full);
    let endpoint = format!("repos/{repo_full}/labels");
    let name_field = format!("name={NEEDS_HUMAN_REVIEW_LABEL}");

    let output = TokioCommand::new(gh_cmd)
        .args([
            "api",
            &endpoint,
            "-X",
            "POST",
            "-f",
            &name_field,
            "-f",
            "color=d93f0b",
            "-f",
            "description=Gru merge judge needs human review before merging",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("already_exists") {
            log::warn!(
                "Failed to create gru:needs-human-review label: {}",
                stderr.trim()
            );
        }
    }

    Ok(())
}

/// Check if the `gru:needs-human-review` label is present on a PR.
pub async fn has_needs_human_review_label(
    owner: &str,
    repo: &str,
    pr_number: &str,
) -> Result<bool> {
    let repo_full = format!("{owner}/{repo}");
    let gh_cmd = gh_command_for_repo(&repo_full);
    let endpoint = format!("repos/{repo_full}/issues/{pr_number}/labels");

    let output = TokioCommand::new(gh_cmd)
        .args(["api", &endpoint])
        .output()
        .await
        .context("Failed to fetch labels")?;

    if !output.status.success() {
        return Ok(false);
    }

    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let labels: Vec<Label> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    Ok(labels.iter().any(|l| l.name == NEEDS_HUMAN_REVIEW_LABEL))
}

/// Post an escalation comment explaining why the judge escalated.
pub async fn post_judge_escalation_comment(
    owner: &str,
    repo: &str,
    pr_number: &str,
    response: &JudgeResponse,
) {
    let repo_full = format!("{owner}/{repo}");
    let gh_cmd = gh_command_for_repo(&repo_full);
    let body = format!(
        "🧑‍⚖️ **Merge readiness: {}/10 — needs human review**\n\n{}\n\n\
         _To proceed, remove the `gru:needs-human-review` label. \
         The judge will re-evaluate on the next PR state change._",
        response.confidence, response.reasoning
    );

    let result = TokioCommand::new(gh_cmd)
        .args([
            "pr", "comment", pr_number, "--repo", &repo_full, "--body", &body,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!("Posted merge judge escalation comment on PR #{}", pr_number);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "Failed to post judge escalation comment on PR #{}: {}",
                pr_number,
                stderr.trim()
            );
        }
        Err(e) => {
            log::warn!("Failed to run gh pr comment: {}", e);
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
        assert!(result.starts_with("h"));
        assert!(result.contains("[Content truncated"));
    }

    #[test]
    fn test_count_json_array_items() {
        assert_eq!(count_json_array_items("[]"), 0);
        assert_eq!(count_json_array_items("[1,2,3]"), 3);
        assert_eq!(count_json_array_items("not json"), 0);
    }

    #[test]
    fn test_build_judge_prompt_includes_threshold() {
        let ctx = PrContext {
            head_sha: "abc123".to_string(),
            diff: "some diff".to_string(),
            comments: "[]".to_string(),
            reviews: "[]".to_string(),
            review_comments: "[]".to_string(),
            comment_count: 0,
        };
        let prompt = build_judge_prompt("42", &ctx, 0, 8);
        assert!(prompt.contains("confidence threshold is 8/10"));
        assert!(!prompt.contains("IMPORTANT: This is your final evaluation"));
    }

    #[test]
    fn test_build_judge_prompt_force_decide_on_max_waits() {
        let ctx = PrContext {
            head_sha: "abc123".to_string(),
            diff: "diff".to_string(),
            comments: "[]".to_string(),
            reviews: "[]".to_string(),
            review_comments: "[]".to_string(),
            comment_count: 0,
        };
        // MAX_CONSECUTIVE_WAITS - 1 = 2, so at 2 consecutive waits, force decide.
        let prompt = build_judge_prompt("42", &ctx, 2, 8);
        assert!(prompt.contains("IMPORTANT: This is your final evaluation"));
    }
}
