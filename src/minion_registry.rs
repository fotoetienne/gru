use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::workspace::Workspace;

/// Cached workspace instance for production state directory access.
static WORKSPACE: Lazy<io::Result<Workspace>> = Lazy::new(Workspace::new);

/// Metadata about a Minion tracked by the Lab
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinionInfo {
    /// Repository the Minion is working on (e.g., "fotoetienne/gru")
    pub repo: String,
    /// Issue number the Minion is addressing
    pub issue: u64,
    /// Command that started the Minion (e.g., "fix", "review", "respond", "rebase")
    pub command: String,
    /// The prompt that was given to the Minion
    pub prompt: String,
    /// When the Minion was started (ISO 8601 timestamp)
    pub started_at: DateTime<Utc>,
    /// Git branch the Minion is working on
    pub branch: String,
    /// Worktree path where the Minion is working
    pub worktree: PathBuf,
    /// Current status (e.g., "active", "idle")
    pub status: String,
    /// PR number associated with this Minion (if any)
    pub pr: Option<String>,
}

/// Root structure for the minions registry file
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryData {
    minions: HashMap<String, MinionInfo>,
}

/// Registry for tracking all Minions managed by this Lab
///
/// The registry stores persistent metadata about Minions including:
/// - What command spawned them
/// - What repo and issue they're working on
/// - Associated PR numbers
/// - Start time and status
///
/// The registry is stored at `~/.gru/state/minions.json` and uses atomic
/// writes (temp file + rename) to prevent corruption.
pub struct MinionRegistry {
    /// Path to the registry file
    registry_path: PathBuf,
    /// In-memory registry data
    data: RegistryData,
}

impl MinionRegistry {
    /// Loads the registry from disk, or creates a new empty registry if the file doesn't exist
    ///
    /// # Arguments
    ///
    /// * `state_dir` - Optional custom state directory path. If None, uses `~/.gru/state/`.
    ///   This parameter is primarily for testing with isolated temp directories.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The state directory cannot be accessed
    /// - The registry file exists but cannot be read
    /// - The registry file contains invalid JSON
    pub fn load(state_dir: Option<&Path>) -> Result<Self> {
        let state_path = if let Some(custom_dir) = state_dir {
            // Test path: use provided directory and ensure it exists
            fs::create_dir_all(custom_dir)
                .with_context(|| format!("Failed to create state directory: {:?}", custom_dir))?;
            custom_dir.to_path_buf()
        } else {
            // Production path: use cached workspace
            let workspace = WORKSPACE.as_ref().map_err(|e| {
                io::Error::new(e.kind(), format!("Failed to initialize workspace: {}", e))
            })?;
            workspace.state().to_path_buf()
        };

        let registry_path = state_path.join("minions.json");

        // Load existing registry or create new one
        let data = if registry_path.exists() {
            let contents = fs::read_to_string(&registry_path)
                .with_context(|| format!("Failed to read registry file: {:?}", registry_path))?;

            serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse registry JSON: {:?}", registry_path))?
        } else {
            RegistryData {
                minions: HashMap::new(),
            }
        };

        Ok(MinionRegistry {
            registry_path,
            data,
        })
    }

    /// Saves the registry to disk using atomic writes (temp file + rename)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The temp file cannot be created
    /// - The JSON cannot be serialized
    /// - The file cannot be written
    /// - The rename operation fails
    pub fn save(&self) -> Result<()> {
        // Serialize to pretty JSON
        let json = serde_json::to_string_pretty(&self.data)
            .context("Failed to serialize registry to JSON")?;

        // Write to temporary file in the same directory
        let temp_path = self.registry_path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .with_context(|| format!("Failed to create temp file: {:?}", temp_path))?;

        file.write_all(json.as_bytes())
            .with_context(|| format!("Failed to write to temp file: {:?}", temp_path))?;

        file.sync_all()
            .with_context(|| format!("Failed to sync temp file: {:?}", temp_path))?;

        drop(file); // Close the file before renaming

        // Atomically rename temp file to registry file
        fs::rename(&temp_path, &self.registry_path).with_context(|| {
            format!(
                "Failed to rename temp file {:?} to {:?}",
                temp_path, self.registry_path
            )
        })?;

        Ok(())
    }

    /// Registers a new Minion in the registry
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier (e.g., "M001")
    /// * `info` - Metadata about the Minion
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A Minion with the same ID already exists
    /// - The registry cannot be saved to disk
    pub fn register(&mut self, minion_id: String, info: MinionInfo) -> Result<()> {
        if self.data.minions.contains_key(&minion_id) {
            anyhow::bail!("Minion {} is already registered", minion_id);
        }

        self.data.minions.insert(minion_id, info);
        self.save()
            .context("Failed to save registry after registering Minion")?;
        Ok(())
    }

    /// Updates an existing Minion's metadata
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier
    /// * `update_fn` - Function that takes a mutable reference to the MinionInfo and updates it
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The Minion ID does not exist in the registry
    /// - The registry cannot be saved to disk
    pub fn update<F>(&mut self, minion_id: &str, update_fn: F) -> Result<()>
    where
        F: FnOnce(&mut MinionInfo),
    {
        let info = self
            .data
            .minions
            .get_mut(minion_id)
            .with_context(|| format!("Minion {} not found in registry", minion_id))?;

        update_fn(info);

        self.save()
            .context("Failed to save registry after updating Minion")?;
        Ok(())
    }

    /// Returns all Minions in the registry
    pub fn list(&self) -> Vec<(String, MinionInfo)> {
        self.data
            .minions
            .iter()
            .map(|(id, info)| (id.clone(), info.clone()))
            .collect()
    }

    /// Checks if a Minion exists in the registry
    pub fn exists(&self, minion_id: &str) -> bool {
        self.data.minions.contains_key(minion_id)
    }

    /// Gets a Minion's metadata by ID
    #[allow(dead_code)]
    pub fn get(&self, minion_id: &str) -> Option<&MinionInfo> {
        self.data.minions.get(minion_id)
    }

    /// Removes a Minion from the registry
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be saved to disk
    #[allow(dead_code)]
    pub fn remove(&mut self, minion_id: &str) -> Result<Option<MinionInfo>> {
        let removed = self.data.minions.remove(minion_id);
        if removed.is_some() {
            self.save()
                .context("Failed to save registry after removing Minion")?;
        }
        Ok(removed)
    }

    /// Migrates existing worktrees into the registry
    ///
    /// This function scans all worktrees in `~/.gru/work/` and populates the registry
    /// with any Minions that don't already exist. This is useful for migrating from
    /// filesystem-only tracking to the registry system.
    ///
    /// For migrated Minions, the command is set to "unknown" and status to "idle".
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The workspace cannot be initialized
    /// - The work directory cannot be scanned
    /// - Git operations fail
    /// - The registry cannot be saved
    pub fn migrate_from_worktrees(&mut self) -> Result<usize> {
        let workspace = WORKSPACE.as_ref().map_err(|e| {
            io::Error::new(e.kind(), format!("Failed to initialize workspace: {}", e))
        })?;

        let work_path = workspace.work();
        if !work_path.exists() {
            return Ok(0);
        }

        let mut migrated_count = 0;

        // Iterate over owner directories
        for owner_entry in std::fs::read_dir(work_path)? {
            let owner_entry = owner_entry?;
            if !owner_entry.path().is_dir() {
                continue;
            }

            // Iterate over repo directories
            for repo_entry in std::fs::read_dir(owner_entry.path())? {
                let repo_entry = repo_entry?;
                if !repo_entry.path().is_dir() {
                    continue;
                }

                // Iterate over minion directories (should start with 'M')
                for minion_entry in std::fs::read_dir(repo_entry.path())? {
                    let minion_entry = minion_entry?;
                    let minion_path = minion_entry.path();

                    if !minion_path.is_dir() {
                        continue;
                    }

                    let minion_id = minion_entry.file_name().to_string_lossy().to_string();

                    // Validate minion ID
                    if minion_id.len() < 2
                        || !minion_id.starts_with('M')
                        || !minion_id.chars().all(|c| c.is_alphanumeric())
                    {
                        continue;
                    }

                    // Skip if already in registry
                    if self.exists(&minion_id) {
                        continue;
                    }

                    // Check if this is a valid git worktree
                    let git_dir = minion_path.join(".git");
                    if !git_dir.exists() {
                        continue;
                    }

                    // Get branch name
                    let branch_output = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&minion_path)
                        .arg("branch")
                        .arg("--show-current")
                        .output()
                        .context("Failed to get branch name")?;

                    let branch = String::from_utf8_lossy(&branch_output.stdout)
                        .trim()
                        .to_string();

                    // Parse issue number from branch (format: minion/issue-<num>-<id>)
                    let issue = parse_issue_from_branch(&branch).unwrap_or(0);

                    // Get worktree creation time as started_at
                    let metadata = std::fs::metadata(&minion_path)
                        .context("Failed to get worktree metadata")?;
                    let started_at = metadata
                        .created()
                        .or_else(|_| metadata.modified())
                        .context("Failed to get worktree creation time")?
                        .into();

                    // Build repo name
                    let owner = owner_entry.file_name().to_string_lossy().to_string();
                    let repo = repo_entry.file_name().to_string_lossy().to_string();
                    let repo_name = format!("{}/{}", owner, repo);

                    // Create MinionInfo with "unknown" command
                    let info = MinionInfo {
                        repo: repo_name,
                        issue,
                        command: "unknown".to_string(),
                        prompt: "Migrated from existing worktree".to_string(),
                        started_at,
                        branch,
                        worktree: minion_path,
                        status: "idle".to_string(),
                        pr: None,
                    };

                    // Add to registry (in-memory only, will save later)
                    self.data.minions.insert(minion_id, info);
                    migrated_count += 1;
                }
            }
        }

        // Save if any migrations occurred
        if migrated_count > 0 {
            self.save()
                .context("Failed to save registry after migration")?;
        }

        Ok(migrated_count)
    }
}

/// Parses the issue number from a branch name
/// Expected format: minion/issue-<num>-<id>
fn parse_issue_from_branch(branch: &str) -> Option<u64> {
    if let Some(issue_part) = branch.strip_prefix("minion/issue-") {
        // Extract the number before the next hyphen
        if let Some(pos) = issue_part.find('-') {
            if let Ok(num) = issue_part[..pos].parse::<u64>() {
                return Some(num);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_creates_empty_registry() {
        let temp_dir = tempdir().unwrap();
        let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
        assert_eq!(registry.list().len(), 0);
    }

    #[test]
    fn test_register_and_list() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info.clone()).unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "M001");
        assert_eq!(list[0].1.repo, "fotoetienne/gru");
        assert_eq!(list[0].1.issue, 42);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info.clone()).unwrap();
        let result = registry.register("M001".to_string(), info);
        assert!(result.is_err());
    }

    #[test]
    fn test_update() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info).unwrap();

        registry
            .update("M001", |info| {
                info.pr = Some("71".to_string());
                info.status = "idle".to_string();
            })
            .unwrap();

        let updated = registry.get("M001").unwrap();
        assert_eq!(updated.pr, Some("71".to_string()));
        assert_eq!(updated.status, "idle");
    }

    #[test]
    fn test_update_nonexistent_fails() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let result = registry.update("M999", |info| {
            info.status = "idle".to_string();
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_persistence() {
        let temp_dir = tempdir().unwrap();

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        // Create registry and register a minion
        {
            let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            registry.register("M001".to_string(), info).unwrap();
        }

        // Load registry again and verify data persisted
        {
            let registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();
            let list = registry.list();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].0, "M001");
            assert_eq!(list[0].1.repo, "fotoetienne/gru");
        }
    }

    #[test]
    fn test_exists() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        assert!(!registry.exists("M001"));

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info).unwrap();
        assert!(registry.exists("M001"));
    }

    #[test]
    fn test_get() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        assert!(registry.get("M001").is_none());

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info).unwrap();
        let retrieved = registry.get("M001").unwrap();
        assert_eq!(retrieved.repo, "fotoetienne/gru");
        assert_eq!(retrieved.issue, 42);
    }

    #[test]
    fn test_remove() {
        let temp_dir = tempdir().unwrap();
        let mut registry = MinionRegistry::load(Some(temp_dir.path())).unwrap();

        let info = MinionInfo {
            repo: "fotoetienne/gru".to_string(),
            issue: 42,
            command: "fix".to_string(),
            prompt: "Fix issue #42".to_string(),
            started_at: Utc::now(),
            branch: "minion/issue-42-M001".to_string(),
            worktree: PathBuf::from("/tmp/test"),
            status: "active".to_string(),
            pr: None,
        };

        registry.register("M001".to_string(), info).unwrap();
        assert!(registry.exists("M001"));

        let removed = registry.remove("M001").unwrap();
        assert!(removed.is_some());
        assert!(!registry.exists("M001"));

        // Removing again should return None
        let removed2 = registry.remove("M001").unwrap();
        assert!(removed2.is_none());
    }
}
