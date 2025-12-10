use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Represents the state of a pull request being worked on by a Minion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrState {
    /// PR number (as string to match GitHub API)
    pub pr_number: String,
    /// Issue number this PR is fixing
    pub issue_number: String,
    /// Current status of the PR
    pub status: PrStatus,
}

/// Status of a pull request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    /// PR has been created as a draft
    Draft,
    /// PR is ready for review
    ReadyForReview,
}

impl PrState {
    /// Create a new PR state
    pub fn new(pr_number: String, issue_number: String) -> Self {
        Self {
            pr_number,
            issue_number,
            status: PrStatus::Draft,
        }
    }

    /// Load PR state from a file
    ///
    /// # Arguments
    /// * `worktree_path` - Path to the worktree directory
    ///
    /// Returns None if the file doesn't exist
    #[allow(dead_code)]
    pub fn load(worktree_path: &Path) -> Result<Option<Self>> {
        let pr_state_path = worktree_path.join("pr_state.json");

        if !pr_state_path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&pr_state_path).context(format!(
            "Failed to read PR state from {}",
            pr_state_path.display()
        ))?;

        let state: PrState =
            serde_json::from_str(&contents).context("Failed to parse PR state JSON")?;

        Ok(Some(state))
    }

    /// Save PR state to a file
    ///
    /// # Arguments
    /// * `worktree_path` - Path to the worktree directory
    pub fn save(&self, worktree_path: &Path) -> Result<()> {
        let pr_state_path = worktree_path.join("pr_state.json");

        let contents =
            serde_json::to_string_pretty(self).context("Failed to serialize PR state")?;

        fs::write(&pr_state_path, contents).context(format!(
            "Failed to write PR state to {}",
            pr_state_path.display()
        ))?;

        Ok(())
    }

    /// Mark the PR as ready for review
    #[allow(dead_code)]
    pub fn mark_ready(&mut self) {
        self.status = PrStatus::ReadyForReview;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_pr_state_new() {
        let state = PrState::new("42".to_string(), "15".to_string());
        assert_eq!(state.pr_number, "42");
        assert_eq!(state.issue_number, "15");
        assert_eq!(state.status, PrStatus::Draft);
    }

    #[test]
    fn test_pr_state_mark_ready() {
        let mut state = PrState::new("42".to_string(), "15".to_string());
        assert_eq!(state.status, PrStatus::Draft);

        state.mark_ready();
        assert_eq!(state.status, PrStatus::ReadyForReview);
    }

    #[test]
    fn test_pr_state_save_and_load() {
        let temp_dir = env::temp_dir().join("gru-test-pr-state");
        let _ = fs::create_dir_all(&temp_dir);

        // Create and save a PR state
        let state = PrState::new("42".to_string(), "15".to_string());
        let result = state.save(&temp_dir);
        assert!(result.is_ok(), "Failed to save PR state: {:?}", result);

        // Load the state back
        let loaded = PrState::load(&temp_dir);
        assert!(loaded.is_ok(), "Failed to load PR state: {:?}", loaded);

        let loaded_state = loaded.unwrap();
        assert!(loaded_state.is_some(), "PR state should exist");

        let loaded_state = loaded_state.unwrap();
        assert_eq!(loaded_state.pr_number, "42");
        assert_eq!(loaded_state.issue_number, "15");
        assert_eq!(loaded_state.status, PrStatus::Draft);

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_pr_state_load_nonexistent() {
        let temp_dir = env::temp_dir().join("gru-test-pr-state-nonexistent");
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::create_dir_all(&temp_dir);

        let result = PrState::load(&temp_dir);
        assert!(result.is_ok(), "Load should not error for missing file");
        assert!(
            result.unwrap().is_none(),
            "Should return None for missing file"
        );

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_pr_state_serialization() {
        let state = PrState::new("42".to_string(), "15".to_string());

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"pr_number\":\"42\""));
        assert!(json.contains("\"issue_number\":\"15\""));
        assert!(json.contains("\"status\":\"draft\""));

        let deserialized: PrState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pr_number, "42");
        assert_eq!(deserialized.issue_number, "15");
        assert_eq!(deserialized.status, PrStatus::Draft);
    }
}
