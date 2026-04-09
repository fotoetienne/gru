use chrono::{DateTime, Utc};

/// Returns true if `body` ends with the signature for `minion_id`.
///
/// Trims trailing whitespace before checking, so `"Done!\n\n<sub>🤖 M1by</sub>  \n"`
/// matches `"M1by"`.  Only matches when the tag is at the *tail* of the body,
/// preventing false positives when a reviewer quotes a Minion comment mid-body
/// (e.g. `"as <sub>🤖 M1by</sub> noted above..."`).
///
/// Avoids heap allocation by stripping the constant prefix/suffix rather than
/// constructing a temporary `String` to compare against.
pub(crate) fn has_minion_signature_for(body: &str, minion_id: &str) -> bool {
    let body = body.trim_end();
    body.strip_suffix("</sub>")
        .and_then(|b| b.strip_suffix(minion_id))
        .is_some_and(|b| b.ends_with("<sub>🤖 "))
}

/// Extracts the Minion ID from a trailing Minion signature in `body`.
///
/// Returns `Some("M1cu")` when body ends with `<sub>🤖 M1cu</sub>` (after
/// trimming trailing whitespace), `None` otherwise.
///
/// Mirrors `has_minion_signature_for` but extracts the ID rather than matching
/// against a known ID.  Only matches when the tag is at the *tail* of the body;
/// a signature quoted mid-body (e.g. `"as <sub>🤖 M1by</sub> noted..."`)
/// returns `None`.
pub(crate) fn extract_minion_id_from_signature(body: &str) -> Option<&str> {
    let body = body.trim_end();
    let prefix = "<sub>🤖 ";
    // Strip "</sub>" from the end — if not present, body has no trailing signature.
    let without_closing = body.strip_suffix("</sub>")?;
    // Find the last "<sub>🤖 " prefix.
    let prefix_pos = without_closing.rfind(prefix)?;
    let id = &without_closing[prefix_pos + prefix.len()..];
    // If the extracted ID itself contains "</sub>", the prefix we found was
    // mid-body (the real closing tag belongs to some other </sub>), not a tail
    // signature.  Reject it.
    if id.contains("</sub>") {
        return None;
    }
    Some(id)
}

/// Represents the phase of Minion execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MinionPhase {
    Planning,
    Implementing,
    Testing,
    Completed,
}

impl MinionPhase {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            MinionPhase::Planning => "planning",
            MinionPhase::Implementing => "implementing",
            MinionPhase::Testing => "testing",
            MinionPhase::Completed => "completed",
        }
    }
}

/// Progress update data
#[derive(Debug, Clone)]
pub(crate) struct ProgressUpdate {
    pub(crate) minion_id: String,
    pub(crate) phase: MinionPhase,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) message: String,
}

impl ProgressUpdate {
    /// Format as a GitHub comment with YAML frontmatter
    pub(crate) fn format_comment(&self) -> String {
        let mut comment = String::new();

        // Header
        comment.push_str(&format!(
            "🤖 **Minion {} progress update**\n\n",
            self.minion_id
        ));

        // YAML frontmatter (wrapped in code block to prevent GitHub markdown interpretation)
        comment.push_str("```yaml\n");
        comment.push_str("event: minion:progress\n");
        comment.push_str(&format!("minion_id: {}\n", self.minion_id));
        comment.push_str(&format!("phase: {}\n", self.phase.as_str()));
        comment.push_str(&format!("timestamp: {}\n", self.timestamp.to_rfc3339()));
        comment.push_str("```\n\n");

        // Body message
        comment.push_str(&self.message);

        comment
    }
}

/// Returns an attribution footer for Minion-generated GitHub posts.
///
/// Renders as small subscript text on GitHub. A blank line before prevents
/// it from blending into the last line of content.
pub(crate) fn minion_signature(id: &str) -> String {
    format!("\n\n<sub>🤖 {}</sub>", id)
}

/// Format an escalation comment with a consistent structure.
///
/// All escalation paths (CI failures, rebase conflicts, etc.) share this
/// envelope so that downstream tooling can identify escalations uniformly.
///
/// - `reason` — short heading shown after the 🚨 emoji (e.g. "CI Fix Escalation")
/// - `detail` — the full markdown body (check lists, error messages, etc.)
/// - `minion_id` — appended as the standard minion signature
pub(crate) fn format_escalation_comment(reason: &str, detail: &str, minion_id: &str) -> String {
    format!(
        "## 🚨 {}\n\n{}{}",
        reason,
        detail.trim_end_matches('\n'),
        minion_signature(minion_id)
    )
}

/// Tracks progress and manages comment posting
pub(crate) struct ProgressCommentTracker {
    minion_id: String,
    current_phase: MinionPhase,
}

impl ProgressCommentTracker {
    /// Create a new progress comment tracker
    pub(crate) fn new(minion_id: String) -> Self {
        Self {
            minion_id,
            current_phase: MinionPhase::Planning,
        }
    }

    /// Update the current phase
    pub(crate) fn set_phase(&mut self, phase: MinionPhase) {
        self.current_phase = phase;
    }

    /// Create a progress update (without posting it)
    pub(crate) fn create_update(&self, message: String) -> ProgressUpdate {
        ProgressUpdate {
            minion_id: self.minion_id.clone(),
            phase: self.current_phase,
            timestamp: Utc::now(),
            message,
        }
    }

    /// Get the current phase
    pub(crate) fn current_phase(&self) -> MinionPhase {
        self.current_phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_escalation_comment() {
        let comment =
            format_escalation_comment("CI Fix Escalation", "Something went wrong.\n", "M042");
        // Structural checks: heading, reason, detail, and signature present
        assert!(comment.contains("## 🚨 CI Fix Escalation"));
        assert!(comment.contains("Something went wrong."));
        assert!(comment.contains("<sub>🤖 M042</sub>"));
        // Detail appears before signature (structural ordering)
        let detail_pos = comment.find("Something went wrong.").unwrap();
        let sig_pos = comment.find("<sub>🤖 M042</sub>").unwrap();
        assert!(detail_pos < sig_pos);
        // Trailing newline in detail is trimmed so the signature lands cleanly
        assert!(!comment.contains("Something went wrong.\n\n\n"));
    }

    #[test]
    fn test_format_escalation_comment_no_trailing_newline() {
        let comment = format_escalation_comment("Minion Escalation", "Rebase failed.", "M001");
        // Structural checks: heading, detail, and signature present
        assert!(comment.contains("## 🚨 Minion Escalation"));
        assert!(comment.contains("Rebase failed."));
        assert!(comment.contains("<sub>🤖 M001</sub>"));
        // Detail appears before signature (structural ordering)
        let detail_pos = comment.find("Rebase failed.").unwrap();
        let sig_pos = comment.find("<sub>🤖 M001</sub>").unwrap();
        assert!(detail_pos < sig_pos);
    }

    #[test]
    fn test_minion_signature() {
        assert_eq!(minion_signature("M042"), "\n\n<sub>🤖 M042</sub>");
        assert_eq!(minion_signature("M0ug"), "\n\n<sub>🤖 M0ug</sub>");
    }

    #[test]
    fn test_extract_minion_id_from_signature() {
        // Basic match
        assert_eq!(
            extract_minion_id_from_signature("Done!\n\n<sub>🤖 M001</sub>"),
            Some("M001")
        );
        // Trailing whitespace is trimmed
        assert_eq!(
            extract_minion_id_from_signature("Done!\n\n<sub>🤖 M1cu</sub>  \n"),
            Some("M1cu")
        );
        // Multi-character IDs
        assert_eq!(
            extract_minion_id_from_signature("Work done.\n\n<sub>🤖 M1cx</sub>"),
            Some("M1cx")
        );
        // No signature → None
        assert_eq!(extract_minion_id_from_signature("Please fix this."), None);
        assert_eq!(extract_minion_id_from_signature(""), None);
        // Signature mid-body (quoted), not at the end → None
        assert_eq!(
            extract_minion_id_from_signature(
                "As noted in <sub>🤖 M001</sub>, please fix the typo."
            ),
            None
        );
        // Different </sub> at end but Minion marker mid-body → None
        assert_eq!(
            extract_minion_id_from_signature("See <sub>🤖 M001</sub> for context. <sub>note</sub>"),
            None
        );
        // Two Minion signatures: only the tail one is returned
        assert_eq!(
            extract_minion_id_from_signature(
                "Reply to <sub>🤖 M001</sub> and then <sub>🤖 M002</sub>"
            ),
            Some("M002")
        );
    }

    #[test]
    fn test_has_minion_signature_for() {
        assert!(has_minion_signature_for(
            "Done!\n\n<sub>🤖 M001</sub>",
            "M001"
        ));
        // trailing whitespace is trimmed
        assert!(has_minion_signature_for(
            "Done!\n\n<sub>🤖 M001</sub>  \n",
            "M001"
        ));
        // wrong ID → false even though a Minion signature is present
        assert!(!has_minion_signature_for(
            "Done!\n\n<sub>🤖 M001</sub>",
            "M002"
        ));
        assert!(!has_minion_signature_for("Please fix this.", "M001"));
        assert!(!has_minion_signature_for("", "M001"));
        // Signature mid-body (quoted), not at the end → must NOT match.
        assert!(!has_minion_signature_for(
            "As noted in <sub>🤖 M001</sub>, please fix the typo.",
            "M001"
        ));
        // Different </sub> at the end, Minion marker mid-body → must NOT match.
        assert!(!has_minion_signature_for(
            "See <sub>🤖 M001</sub> for context. <sub>note</sub>",
            "M001"
        ));
    }

    #[test]
    fn test_minion_phase_as_str() {
        assert_eq!(MinionPhase::Planning.as_str(), "planning");
        assert_eq!(MinionPhase::Implementing.as_str(), "implementing");
        assert_eq!(MinionPhase::Testing.as_str(), "testing");
        assert_eq!(MinionPhase::Completed.as_str(), "completed");
    }

    #[test]
    fn test_progress_update_format() {
        let update = ProgressUpdate {
            minion_id: "M042".to_string(),
            phase: MinionPhase::Implementing,
            timestamp: DateTime::parse_from_rfc3339("2025-01-30T14:45:00Z")
                .unwrap()
                .with_timezone(&Utc),
            message: "Working on implementation".to_string(),
        };

        let formatted = update.format_comment();

        // Check header
        assert!(formatted.contains("🤖 **Minion M042 progress update**"));

        // Check YAML frontmatter in code block
        assert!(formatted.contains("```yaml\n"));
        assert!(formatted.contains("event: minion:progress"));
        assert!(formatted.contains("minion_id: M042"));
        assert!(formatted.contains("phase: implementing"));
        assert!(formatted.contains("timestamp: 2025-01-30T14:45:00+00:00"));

        // Check message
        assert!(formatted.contains("Working on implementation"));
    }

    #[test]
    fn test_progress_tracker_initialization() {
        let tracker = ProgressCommentTracker::new("M001".to_string());
        assert_eq!(tracker.minion_id, "M001");
        assert_eq!(tracker.current_phase, MinionPhase::Planning);
    }

    #[test]
    fn test_progress_tracker_phase_changes() {
        let mut tracker = ProgressCommentTracker::new("M001".to_string());
        assert_eq!(tracker.current_phase(), MinionPhase::Planning);

        tracker.set_phase(MinionPhase::Implementing);
        assert_eq!(tracker.current_phase(), MinionPhase::Implementing);

        tracker.set_phase(MinionPhase::Testing);
        assert_eq!(tracker.current_phase(), MinionPhase::Testing);
    }

    #[test]
    fn test_create_update() {
        let mut tracker = ProgressCommentTracker::new("M001".to_string());
        tracker.set_phase(MinionPhase::Implementing);

        let update = tracker.create_update("Working on implementation".to_string());

        assert_eq!(update.minion_id, "M001");
        assert_eq!(update.phase, MinionPhase::Implementing);
        assert_eq!(update.message, "Working on implementation");
    }
}
