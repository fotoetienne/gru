use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration for Gru Lab daemon mode
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabConfig {
    #[serde(default)]
    pub daemon: DaemonConfig,

    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub merge: MergeConfig,
}

/// Merge judge configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConfig {
    /// Confidence threshold (1-10) for the merge-readiness judge.
    /// Only merge when the judge's confidence >= this value.
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: u8,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: default_confidence_threshold(),
        }
    }
}

fn default_confidence_threshold() -> u8 {
    8
}

/// Agent backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Which agent backend to use by default (e.g., "claude")
    #[serde(default = "default_agent_name")]
    pub default: String,

    /// Claude-specific configuration ([agent.claude] in TOML)
    #[serde(default)]
    pub claude: ClaudeAgentConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default: default_agent_name(),
            claude: ClaudeAgentConfig::default(),
        }
    }
}

/// Claude-specific agent configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeAgentConfig {
    /// Override the binary path for Claude Code CLI
    #[serde(default)]
    pub binary: Option<String>,
}

fn default_agent_name() -> String {
    "claude".to_string()
}

/// Daemon configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Repositories to monitor for ready-for-minion issues
    #[serde(default)]
    pub repos: Vec<String>,

    /// Polling interval in seconds (default: 30s)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Maximum concurrent Minion slots (default: 2)
    #[serde(default = "default_max_slots")]
    pub max_slots: usize,

    /// Label to watch for issues (default: "ready-for-minion")
    #[serde(default = "default_label")]
    pub label: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            max_slots: default_max_slots(),
            label: default_label(),
        }
    }
}

fn default_poll_interval() -> u64 {
    30
}

fn default_max_slots() -> usize {
    2
}

fn default_label() -> String {
    "ready-for-minion".to_string()
}

impl LabConfig {
    /// Load configuration from file
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: LabConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        config.validate()?;

        Ok(config)
    }

    /// Get default config file path (~/.gru/config.toml)
    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        Ok(home.join(".gru").join("config.toml"))
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        if self.daemon.repos.is_empty() {
            anyhow::bail!(
                "No repositories configured. Add repos to config.toml or use --repos flag"
            );
        }

        if self.daemon.max_slots == 0 {
            anyhow::bail!("max_slots must be at least 1");
        }

        if self.daemon.poll_interval_secs == 0 {
            anyhow::bail!("poll_interval_secs must be at least 1");
        }

        // Validate repo format (owner/repo)
        for repo in &self.daemon.repos {
            let parts: Vec<&str> = repo.split('/').collect();
            if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                anyhow::bail!("Invalid repo format: '{}'. Expected 'owner/repo'", repo);
            }
        }

        Ok(())
    }

    /// Get poll interval as Duration
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.daemon.poll_interval_secs)
    }

    /// Merge with CLI overrides
    pub fn with_overrides(
        mut self,
        repos: Option<Vec<String>>,
        poll_interval_secs: Option<u64>,
        max_slots: Option<usize>,
    ) -> Self {
        if let Some(repos) = repos {
            self.daemon.repos = repos;
        }

        if let Some(interval) = poll_interval_secs {
            self.daemon.poll_interval_secs = interval;
        }

        if let Some(slots) = max_slots {
            self.daemon.max_slots = slots;
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_valid_config() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo1", "owner/repo2"]
poll_interval_secs = 60
max_slots = 4
label = "gru-ready"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();

        assert_eq!(config.daemon.repos, vec!["owner/repo1", "owner/repo2"]);
        assert_eq!(config.daemon.poll_interval_secs, 60);
        assert_eq!(config.daemon.max_slots, 4);
        assert_eq!(config.daemon.label, "gru-ready");
    }

    #[test]
    fn test_default_values() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();

        assert_eq!(config.daemon.poll_interval_secs, 30);
        assert_eq!(config.daemon.max_slots, 2);
        assert_eq!(config.daemon.label, "ready-for-minion");
    }

    #[test]
    fn test_validate_empty_repos() {
        let config = LabConfig::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_repo_format() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["invalid-repo".to_string()];

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_zero_max_slots() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo".to_string()];
        config.daemon.max_slots = 0;

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_with_overrides() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo1".to_string()];

        let config =
            config.with_overrides(Some(vec!["owner/repo2".to_string()]), Some(60), Some(4));

        assert_eq!(config.daemon.repos, vec!["owner/repo2"]);
        assert_eq!(config.daemon.poll_interval_secs, 60);
        assert_eq!(config.daemon.max_slots, 4);
    }

    #[test]
    fn test_poll_interval() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo".to_string()];
        config.daemon.poll_interval_secs = 45;

        assert_eq!(config.poll_interval(), Duration::from_secs(45));
    }

    #[test]
    fn test_agent_config_defaults() {
        let config = AgentConfig::default();
        assert_eq!(config.default, "claude");
        assert!(config.claude.binary.is_none());
    }

    #[test]
    fn test_agent_config_missing_section_defaults_to_claude() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.agent.default, "claude");
        assert!(config.agent.claude.binary.is_none());
    }

    #[test]
    fn test_agent_config_with_agent_section() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]

[agent]
default = "claude"

[agent.claude]
binary = "/usr/local/bin/claude"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.agent.default, "claude");
        assert_eq!(
            config.agent.claude.binary.as_deref(),
            Some("/usr/local/bin/claude")
        );
    }

    #[test]
    fn test_agent_config_custom_default() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]

[agent]
default = "aider"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        // Parsing succeeds (validation happens in AgentRegistry, not config parsing)
        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.agent.default, "aider");
    }

    #[test]
    fn test_merge_config_defaults() {
        let config = MergeConfig::default();
        assert_eq!(config.confidence_threshold, 8);
    }

    #[test]
    fn test_merge_config_missing_section_uses_defaults() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.merge.confidence_threshold, 8);
    }

    #[test]
    fn test_merge_config_custom_threshold() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]

[merge]
confidence_threshold = 6
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.merge.confidence_threshold, 6);
    }
}
