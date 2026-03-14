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
    /// Repositories to monitor for gru:todo issues
    #[serde(default)]
    pub repos: Vec<String>,

    /// Polling interval in seconds (default: 30s)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Maximum concurrent Minion slots (default: 2)
    #[serde(default = "default_max_slots")]
    pub max_slots: usize,

    /// Label to watch for issues (default: "gru:todo")
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

/// Try to load the config file, returning `None` on any error.
///
/// This is a convenience for callers that want to inspect config
/// but can gracefully handle its absence.
pub fn try_load_config() -> Option<LabConfig> {
    let path = LabConfig::default_path().ok()?;
    LabConfig::load_partial(&path).ok()
}

/// Load just the `github_hosts` from config, returning `["github.com"]` on any error.
///
/// This is a convenience for callers that need host info but don't require
/// full daemon config validation.
pub fn load_github_hosts() -> Vec<String> {
    let path = match LabConfig::default_path() {
        Ok(p) => p,
        Err(_) => return vec!["github.com".to_string()],
    };
    match LabConfig::load_partial(&path) {
        Ok(cfg) => cfg.all_github_hosts(),
        Err(_) => vec!["github.com".to_string()],
    }
}

fn default_poll_interval() -> u64 {
    30
}

fn default_max_slots() -> usize {
    2
}

fn default_label() -> String {
    crate::labels::TODO.to_string()
}

/// Parse a repo entry from the config into `(host, owner, repo)`.
///
/// Accepts two formats:
/// - `"owner/repo"` → `("github.com", "owner", "repo")`
/// - `"host/owner/repo"` → `("host", "owner", "repo")`
///
/// A first segment containing a dot (`.`) is treated as a hostname.
/// Returns `None` if the format is invalid.
pub fn parse_repo_entry(spec: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = spec.splitn(4, '/').collect();
    match parts.len() {
        2 => {
            let (owner, repo) = (parts[0], parts[1]);
            if owner.is_empty() || repo.is_empty() {
                return None;
            }
            Some((
                "github.com".to_string(),
                owner.to_string(),
                repo.to_string(),
            ))
        }
        3 => {
            let (host, owner, repo) = (parts[0], parts[1], parts[2]);
            if host.is_empty() || owner.is_empty() || repo.is_empty() {
                return None;
            }
            // Require the first segment to look like a hostname (contains a dot)
            if !host.contains('.') {
                return None;
            }
            Some((host.to_string(), owner.to_string(), repo.to_string()))
        }
        _ => None,
    }
}

impl LabConfig {
    /// Returns the full list of GitHub hosts, always including `github.com`.
    ///
    /// Hosts are derived from `daemon.repos` entries: `host/owner/repo` entries
    /// contribute the host part, while plain `owner/repo` entries imply `github.com`.
    pub fn all_github_hosts(&self) -> Vec<String> {
        let mut hosts = vec!["github.com".to_string()];
        for repo in &self.daemon.repos {
            if let Some((host, _, _)) = parse_repo_entry(repo) {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        hosts
    }

    /// Generate default config file content with commented-out options.
    ///
    /// Comment convention:
    /// - `# [section]` — single `#` for TOML section headers
    /// - `# # description` / `# key = value` — double `#` for descriptions,
    ///   single `#` + space for option lines (so uncommenting removes one `#` layer)
    pub fn default_config_toml() -> &'static str {
        r#"# Gru configuration file
# Uncomment and modify options as needed.

# [daemon]
# # Repositories to monitor (required for `gru lab`).
# # Use "owner/repo" for github.com, or "host/owner/repo" for GitHub Enterprise.
# repos = ["owner/repo", "ghe.example.com/org/repo"]
#
# # Polling interval in seconds (default: 30)
# poll_interval_secs = 30
#
# # Maximum concurrent Minion slots (default: 2)
# max_slots = 2
#
# # Label to watch for issues (default: "gru:todo")
# label = "gru:todo"

# [agent]
# # Which agent backend to use (default: "claude")
# default = "claude"

# [agent.claude]
# # Override the Claude Code CLI binary path
# binary = "/usr/local/bin/claude"

# [merge]
# # Confidence threshold (1-10) for the merge-readiness judge (default: 8)
# confidence_threshold = 8
"#
    }

    /// Write the default config file to the given path if it doesn't exist.
    /// Returns Ok(true) if the file was created, Ok(false) if it already existed.
    /// Uses create_new to atomically check existence and create in one operation.
    pub fn write_default_config(path: &Path) -> Result<bool> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(Self::default_config_toml().as_bytes())
                    .with_context(|| format!("Failed to write config file: {}", path.display()))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => {
                Err(e).with_context(|| format!("Failed to create config file: {}", path.display()))
            }
        }
    }

    /// Load configuration from file (validates daemon config — use for `gru lab`).
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: LabConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        config.validate()?;

        Ok(config)
    }

    /// Load configuration without daemon validation.
    ///
    /// Use this for non-daemon commands (e.g., `gru do`) that only need
    /// agent/merge config but may not have `[daemon].repos` configured.
    pub fn load_partial(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: LabConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

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

        // Validate repo format: "owner/repo" or "host/owner/repo"
        for repo in &self.daemon.repos {
            if parse_repo_entry(repo).is_none() {
                anyhow::bail!(
                    "Invalid repo format: '{}'. Expected 'owner/repo' or 'host/owner/repo'",
                    repo
                );
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
        assert_eq!(config.daemon.label, "gru:todo");
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
    fn test_default_config_toml_is_valid_when_uncommented() {
        // Verify the config options parse as valid TOML when uncommented.
        // Skip header/description comments that aren't TOML config lines.
        let template = LabConfig::default_config_toml();
        let uncommented: String = template
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if let Some(stripped) = trimmed.strip_prefix("# ") {
                    // Only uncomment lines that look like TOML (key=val or [section])
                    let s = stripped.trim();
                    if s.starts_with('[') || s.contains(" = ") {
                        return stripped.to_string();
                    }
                    // Keep description comments as-is
                    format!("# {}", stripped)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let config: LabConfig = toml::from_str(&uncommented)
            .expect("Default config template should be valid TOML when uncommented");
        assert_eq!(config.daemon.poll_interval_secs, 30);
        assert_eq!(config.daemon.max_slots, 2);
        assert_eq!(config.daemon.label, "gru:todo");
        assert_eq!(config.agent.default, "claude");
        assert_eq!(config.merge.confidence_threshold, 8);
    }

    #[test]
    fn test_write_default_config_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let created = LabConfig::write_default_config(&config_path).unwrap();
        assert!(created);
        assert!(config_path.exists());

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("[daemon]"));
        assert!(contents.contains("poll_interval_secs"));
    }

    #[test]
    fn test_write_default_config_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        // Write custom content first
        fs::write(&config_path, "custom config").unwrap();

        let created = LabConfig::write_default_config(&config_path).unwrap();
        assert!(!created);

        // Verify content was not overwritten
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents, "custom config");
    }

    #[test]
    fn test_write_default_config_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nested").join("dir").join("config.toml");

        let created = LabConfig::write_default_config(&config_path).unwrap();
        assert!(created);
        assert!(config_path.exists());
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

    #[test]
    fn test_github_hosts_default() {
        let config = LabConfig::default();
        let hosts = config.all_github_hosts();
        assert_eq!(hosts, vec!["github.com"]);
    }

    #[test]
    fn test_github_hosts_derived_from_repos() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo", "ghe.example.com/org/service", "git.corp.net/team/app"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        let hosts = config.all_github_hosts();
        assert_eq!(hosts, vec!["github.com", "ghe.example.com", "git.corp.net"]);
    }

    #[test]
    fn test_github_hosts_deduplicates() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo1", "ghe.example.com/org/svc1", "ghe.example.com/org/svc2"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        let hosts = config.all_github_hosts();
        assert_eq!(hosts, vec!["github.com", "ghe.example.com"]);
    }

    // --- parse_repo_entry tests ---

    #[test]
    fn test_parse_repo_entry_owner_repo() {
        let result = parse_repo_entry("owner/repo");
        assert_eq!(
            result,
            Some((
                "github.com".to_string(),
                "owner".to_string(),
                "repo".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_repo_entry_host_owner_repo() {
        let result = parse_repo_entry("ghe.example.com/org/service");
        assert_eq!(
            result,
            Some((
                "ghe.example.com".to_string(),
                "org".to_string(),
                "service".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_repo_entry_no_dot_in_three_parts_rejected() {
        // "a/b/c" where "a" has no dot should be rejected (not a valid host)
        assert_eq!(parse_repo_entry("a/b/c"), None);
    }

    #[test]
    fn test_parse_repo_entry_empty_parts() {
        assert_eq!(parse_repo_entry(""), None);
        assert_eq!(parse_repo_entry("/repo"), None);
        assert_eq!(parse_repo_entry("owner/"), None);
        assert_eq!(parse_repo_entry("host.com//repo"), None);
        assert_eq!(parse_repo_entry("host.com/owner/"), None);
    }

    #[test]
    fn test_parse_repo_entry_too_many_slashes() {
        assert_eq!(parse_repo_entry("a/b/c/d"), None);
    }

    #[test]
    fn test_parse_repo_entry_single_segment() {
        assert_eq!(parse_repo_entry("justrepo"), None);
    }
}
