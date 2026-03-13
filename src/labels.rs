//! Centralized label name constants for Gru's GitHub label state machine.
//!
//! All labels use a `gru:` prefix for consistent identification.

// ============================================================================
// Issue lifecycle labels
// ============================================================================

/// Issue ready for a Minion to claim.
pub const TODO: &str = "gru:todo";
/// Minion actively working on an issue.
pub const IN_PROGRESS: &str = "gru:in-progress";
/// Minion completed successfully.
pub const DONE: &str = "gru:done";
/// Minion encountered failure.
pub const FAILED: &str = "gru:failed";
/// Needs human intervention.
pub const BLOCKED: &str = "gru:blocked";

// ============================================================================
// PR labels
// ============================================================================

/// All merge-readiness checks pass.
pub const READY_TO_MERGE: &str = "gru:ready-to-merge";
/// Auto-merge when checks pass.
pub const AUTO_MERGE: &str = "gru:auto-merge";
/// LLM judge escalated for human review.
pub const NEEDS_HUMAN_REVIEW: &str = "gru:needs-human-review";

// ============================================================================
// Label definitions: (name, color_hex, description)
// ============================================================================

/// All labels that `gru init` should create.
pub const ALL_LABELS: &[(&str, &str, &str)] = &[
    (TODO, "0075ca", "Issue ready for autonomous agent"),
    (IN_PROGRESS, "fbca04", "Agent actively working"),
    (DONE, "0ecab9", "Agent completed successfully"),
    (FAILED, "d73a4a", "Agent encountered failure"),
    (BLOCKED, "b60205", "Agent blocked, needs human"),
    (READY_TO_MERGE, "0e8a16", "All merge-readiness checks pass"),
    (
        AUTO_MERGE,
        "5319e7",
        "Gru will auto-merge this PR when all checks pass",
    ),
    (
        NEEDS_HUMAN_REVIEW,
        "d93f0b",
        "Gru merge judge needs human review before merging",
    ),
];

/// Look up the color and description for a label by its canonical name.
/// Returns `Some((color, description))` if found in `ALL_LABELS`.
pub fn get_label_info(canonical: &str) -> Option<(&'static str, &'static str)> {
    ALL_LABELS
        .iter()
        .find(|(name, _, _)| *name == canonical)
        .map(|(_, color, desc)| (*color, *desc))
}

/// Check if any label in the list matches the given canonical label.
pub fn has_label(labels: &[String], canonical: &str) -> bool {
    labels.iter().any(|l| l == canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_label_found() {
        let labels = vec!["enhancement".to_string(), "gru:todo".to_string()];
        assert!(has_label(&labels, TODO));
        assert!(!has_label(&labels, IN_PROGRESS));
    }

    #[test]
    fn test_has_label_not_found() {
        let labels = vec!["enhancement".to_string()];
        assert!(!has_label(&labels, TODO));
    }

    #[test]
    fn test_all_labels_unique_colors() {
        let colors: Vec<&str> = ALL_LABELS.iter().map(|(_, c, _)| *c).collect();
        let unique: std::collections::HashSet<&str> = colors.iter().copied().collect();
        assert_eq!(
            colors.len(),
            unique.len(),
            "Duplicate colors found in ALL_LABELS"
        );
    }

    #[test]
    fn test_all_labels_count() {
        assert_eq!(ALL_LABELS.len(), 8);
    }

    #[test]
    fn test_get_label_info_found() {
        let (color, desc) = get_label_info(TODO).unwrap();
        assert_eq!(color, "0075ca");
        assert!(!desc.is_empty());
    }

    #[test]
    fn test_get_label_info_not_found() {
        assert!(get_label_info("nonexistent").is_none());
    }
}
