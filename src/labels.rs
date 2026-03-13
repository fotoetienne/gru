//! Centralized label name constants for Gru's GitHub label state machine.
//!
//! All labels use a `gru:` prefix for consistent identification.
//! During migration, both old and new label names are accepted when reading.

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

// ============================================================================
// Migration: old name → new name
// ============================================================================

/// Mapping of old label names to their new replacements.
/// Used by `gru init` to rename existing labels.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("ready-for-minion", TODO),
    ("in-progress", IN_PROGRESS),
    ("minion:done", DONE),
    ("minion:failed", FAILED),
    ("minion:blocked", BLOCKED),
    ("ready-to-merge", READY_TO_MERGE),
];

// ============================================================================
// Backward-compatible label matching
// ============================================================================

/// Old names for TODO (issue ready for minion).
const TODO_OLD: &[&str] = &["ready-for-minion"];
/// Old names for IN_PROGRESS.
const IN_PROGRESS_OLD: &[&str] = &["in-progress"];
/// Old names for DONE.
const DONE_OLD: &[&str] = &["minion:done"];
/// Old names for FAILED.
const FAILED_OLD: &[&str] = &["minion:failed"];
/// Old names for BLOCKED.
const BLOCKED_OLD: &[&str] = &["minion:blocked"];
/// Old names for READY_TO_MERGE.
const READY_TO_MERGE_OLD: &[&str] = &["ready-to-merge"];

/// Look up the color and description for a label by its canonical name.
/// Returns `Some((color, description))` if found in `ALL_LABELS`.
pub fn get_label_info(canonical: &str) -> Option<(&'static str, &'static str)> {
    ALL_LABELS
        .iter()
        .find(|(name, _, _)| *name == canonical)
        .map(|(_, color, desc)| (*color, *desc))
}

/// Return the counterpart label name for backward-compatible querying.
///
/// If given a new name, returns the old name (if one exists).
/// If given an old name, returns the new name.
/// Returns `None` if the label has no counterpart (e.g., unchanged labels).
pub fn counterpart_label(label: &str) -> Option<&'static str> {
    // Check if it's a new name → return old name
    for (old, new) in MIGRATIONS {
        if label == *new {
            return Some(old);
        }
        if label == *old {
            return Some(new);
        }
    }
    None
}

/// Check if a label name matches the given canonical label (new or old name).
pub fn matches_label(actual: &str, canonical: &str) -> bool {
    if actual == canonical {
        return true;
    }
    let old_names: &[&str] = match canonical {
        TODO => TODO_OLD,
        IN_PROGRESS => IN_PROGRESS_OLD,
        DONE => DONE_OLD,
        FAILED => FAILED_OLD,
        BLOCKED => BLOCKED_OLD,
        READY_TO_MERGE => READY_TO_MERGE_OLD,
        _ => return false,
    };
    old_names.contains(&actual)
}

/// Check if any label in the list matches the given canonical label.
pub fn has_label(labels: &[String], canonical: &str) -> bool {
    labels.iter().any(|l| matches_label(l, canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_label_new_names() {
        assert!(matches_label("gru:todo", TODO));
        assert!(matches_label("gru:in-progress", IN_PROGRESS));
        assert!(matches_label("gru:done", DONE));
        assert!(matches_label("gru:failed", FAILED));
        assert!(matches_label("gru:blocked", BLOCKED));
        assert!(matches_label("gru:ready-to-merge", READY_TO_MERGE));
        assert!(matches_label("gru:auto-merge", AUTO_MERGE));
        assert!(matches_label("gru:needs-human-review", NEEDS_HUMAN_REVIEW));
    }

    #[test]
    fn test_matches_label_old_names() {
        assert!(matches_label("ready-for-minion", TODO));
        assert!(matches_label("in-progress", IN_PROGRESS));
        assert!(matches_label("minion:done", DONE));
        assert!(matches_label("minion:failed", FAILED));
        assert!(matches_label("minion:blocked", BLOCKED));
        assert!(matches_label("ready-to-merge", READY_TO_MERGE));
    }

    #[test]
    fn test_matches_label_no_false_positives() {
        assert!(!matches_label("gru:todo", IN_PROGRESS));
        assert!(!matches_label("in-progress", TODO));
        assert!(!matches_label("random-label", TODO));
    }

    #[test]
    fn test_has_label_mixed() {
        let labels = vec![
            "enhancement".to_string(),
            "ready-for-minion".to_string(), // old name
        ];
        assert!(has_label(&labels, TODO));
        assert!(!has_label(&labels, IN_PROGRESS));
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
    fn test_counterpart_label_new_to_old() {
        assert_eq!(counterpart_label(TODO), Some("ready-for-minion"));
        assert_eq!(counterpart_label(IN_PROGRESS), Some("in-progress"));
        assert_eq!(counterpart_label(DONE), Some("minion:done"));
        assert_eq!(counterpart_label(BLOCKED), Some("minion:blocked"));
        assert_eq!(counterpart_label(READY_TO_MERGE), Some("ready-to-merge"));
    }

    #[test]
    fn test_counterpart_label_old_to_new() {
        assert_eq!(counterpart_label("ready-for-minion"), Some(TODO));
        assert_eq!(counterpart_label("in-progress"), Some(IN_PROGRESS));
        assert_eq!(counterpart_label("minion:done"), Some(DONE));
    }

    #[test]
    fn test_counterpart_label_unchanged_returns_none() {
        assert_eq!(counterpart_label(AUTO_MERGE), None);
        assert_eq!(counterpart_label(NEEDS_HUMAN_REVIEW), None);
        assert_eq!(counterpart_label("random-label"), None);
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

    #[test]
    fn test_migrations_map_to_valid_labels() {
        let label_names: Vec<&str> = ALL_LABELS.iter().map(|(n, _, _)| *n).collect();
        for (_, new_name) in MIGRATIONS {
            assert!(
                label_names.contains(new_name),
                "Migration target '{}' is not in ALL_LABELS",
                new_name
            );
        }
    }
}
