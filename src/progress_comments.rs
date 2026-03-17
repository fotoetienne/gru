use chrono::{DateTime, Utc};

/// Represents the phase of Minion execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinionPhase {
    Planning,
    Implementing,
    Testing,
    Completed,
}

impl MinionPhase {
    pub fn as_str(&self) -> &'static str {
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
pub struct ProgressUpdate {
    pub minion_id: String,
    pub phase: MinionPhase,
    pub timestamp: DateTime<Utc>,
    pub message: String,
}

impl ProgressUpdate {
    /// Format as a GitHub comment with YAML frontmatter
    pub fn format_comment(&self) -> String {
        let mut comment = String::new();

        // Header
        comment.push_str(&format!(
            "🤖 **Minion {} progress update**\n\n",
            self.minion_id
        ));

        // YAML frontmatter
        comment.push_str("---\n");
        comment.push_str("event: minion:progress\n");
        comment.push_str(&format!("minion_id: {}\n", self.minion_id));
        comment.push_str(&format!("phase: {}\n", self.phase.as_str()));
        comment.push_str(&format!("timestamp: {}\n", self.timestamp.to_rfc3339()));
        comment.push_str("---\n\n");

        // Body message
        comment.push_str(&self.message);

        comment
    }
}

/// Returns an attribution footer for Minion-generated GitHub posts.
///
/// Renders as small subscript text on GitHub. A blank line before prevents
/// it from blending into the last line of content.
pub fn minion_signature(id: &str) -> String {
    format!("\n\n<sub>🤖 {}</sub>", id)
}

/// Tracks progress and manages comment posting
pub struct ProgressCommentTracker {
    minion_id: String,
    current_phase: MinionPhase,
}

impl ProgressCommentTracker {
    /// Create a new progress comment tracker
    pub fn new(minion_id: String) -> Self {
        Self {
            minion_id,
            current_phase: MinionPhase::Planning,
        }
    }

    /// Update the current phase
    pub fn set_phase(&mut self, phase: MinionPhase) {
        self.current_phase = phase;
    }

    /// Create a progress update (without posting it)
    pub fn create_update(&self, message: String) -> ProgressUpdate {
        ProgressUpdate {
            minion_id: self.minion_id.clone(),
            phase: self.current_phase,
            timestamp: Utc::now(),
            message,
        }
    }

    /// Get the current phase
    pub fn current_phase(&self) -> MinionPhase {
        self.current_phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minion_signature() {
        assert_eq!(minion_signature("M042"), "\n\n<sub>🤖 M042</sub>");
        assert_eq!(minion_signature("M0ug"), "\n\n<sub>🤖 M0ug</sub>");
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

        // Check YAML frontmatter
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
