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
    /// * `dir` - Path to search for the PR state file. Searches the given directory
    ///   first, then falls back to the parent directory. This handles both:
    ///   - New layout: state in `minion_dir/`, caller passes `minion_dir`
    ///   - Legacy layout: state in the same directory as the git checkout
    ///   - Checkout subdir: caller passes `checkout/`, state is in parent `minion_dir/`
    ///
    /// Returns None if the file doesn't exist in either location
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let pr_state_path = dir.join(".gru_pr_state.json");

        if pr_state_path.exists() {
            let contents = fs::read_to_string(&pr_state_path).context(format!(
                "Failed to read PR state from {}",
                pr_state_path.display()
            ))?;
            let state: PrState =
                serde_json::from_str(&contents).context("Failed to parse PR state JSON")?;
            return Ok(Some(state));
        }

        // Fallback: check parent directory (handles case where caller passes checkout_path
        // but state lives in the parent minion_dir)
        if let Some(parent) = dir.parent() {
            let parent_path = parent.join(".gru_pr_state.json");
            if parent_path.exists() {
                let contents = fs::read_to_string(&parent_path).context(format!(
                    "Failed to read PR state from {}",
                    parent_path.display()
                ))?;
                let state: PrState =
                    serde_json::from_str(&contents).context("Failed to parse PR state JSON")?;
                return Ok(Some(state));
            }
        }

        Ok(None)
    }

    /// Save PR state to a file
    ///
    /// # Arguments
    /// * `worktree_path` - Path to the worktree directory
    ///
    /// Note: Uses hidden file (.gru_pr_state.json) to avoid git status pollution
    pub fn save(&self, worktree_path: &Path) -> Result<()> {
        let pr_state_path = worktree_path.join(".gru_pr_state.json");

        let contents =
            serde_json::to_string_pretty(self).context("Failed to serialize PR state")?;

        fs::write(&pr_state_path, contents).context(format!(
            "Failed to write PR state to {}",
            pr_state_path.display()
        ))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_state_save_and_load() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Create and save a PR state
        let state = PrState::new("42".to_string(), "15".to_string());
        let result = state.save(temp_dir.path());
        assert!(result.is_ok(), "Failed to save PR state: {:?}", result);

        // Load the state back
        let loaded = PrState::load(temp_dir.path());
        assert!(loaded.is_ok(), "Failed to load PR state: {:?}", loaded);

        let loaded_state = loaded.unwrap();
        assert!(loaded_state.is_some(), "PR state should exist");

        let loaded_state = loaded_state.unwrap();
        assert_eq!(loaded_state.pr_number, "42");
        assert_eq!(loaded_state.issue_number, "15");
        assert_eq!(loaded_state.status, PrStatus::Draft);
    }

    #[test]
    fn test_pr_state_load_nonexistent() {
        let temp_dir = tempfile::tempdir().unwrap();

        let result = PrState::load(temp_dir.path());
        assert!(result.is_ok(), "Load should not error for missing file");
        assert!(
            result.unwrap().is_none(),
            "Should return None for missing file"
        );
    }

    #[test]
    fn test_pr_state_load_parent_fallback() {
        let temp_dir = tempfile::tempdir().unwrap();
        let checkout_dir = temp_dir.path().join("checkout");
        std::fs::create_dir_all(&checkout_dir).unwrap();

        // Save state in parent (minion_dir), load from child (checkout/)
        let state = PrState::new("42".to_string(), "15".to_string());
        state.save(temp_dir.path()).unwrap();

        let loaded = PrState::load(&checkout_dir).unwrap();
        assert!(loaded.is_some(), "Should find PR state in parent directory");
        assert_eq!(loaded.unwrap().pr_number, "42");
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

    #[test]
    fn test_pr_state_self_review_sha_ignored_in_old_json() {
        // Existing PR state files with self_review_sha should still deserialize fine
        // (serde ignores unknown fields by default)
        let json =
            r#"{"pr_number":"42","issue_number":"15","status":"draft","self_review_sha":"abc123"}"#;
        let state: PrState = serde_json::from_str(json).unwrap();
        assert_eq!(state.pr_number, "42");
        assert_eq!(state.status, PrStatus::Draft);
    }
}
