use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration for a GitHub Enterprise host
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhHostConfig {
    /// Hostname for GH_HOST, gh --hostname, and git remote matching
    pub host: String,
    /// Web UI URL (defaults to https://{host}). Only needed when the web UI
    /// is on a different domain than the git/API host.
    pub web_url: Option<String>,
}

/// Configuration for Gru Lab daemon mode
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabConfig {
    #[serde(default)]
    pub github_hosts: HashMap<String, GhHostConfig>,

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

    /// Maximum resume attempts before marking a Minion as failed (default: 3)
    #[serde(default = "default_max_resume_attempts")]
    pub max_resume_attempts: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            max_slots: default_max_slots(),
            label: default_label(),
            max_resume_attempts: default_max_resume_attempts(),
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

/// Load a `HostRegistry` from the default config file.
///
/// Returns a registry with just `github.com` if the config can't be loaded.
/// This is a convenience for callers that need host info but don't require
/// full daemon config validation.
pub fn load_host_registry() -> HostRegistry {
    let path = match LabConfig::default_path() {
        Ok(p) => p,
        Err(_) => return HostRegistry::from_config(&LabConfig::default()),
    };
    match LabConfig::load_partial(&path) {
        Ok(cfg) => HostRegistry::from_config(&cfg),
        Err(_) => HostRegistry::from_config(&LabConfig::default()),
    }
}

/// Check that the `gh` CLI binary is available on PATH.
///
/// Returns `Ok(())` if `gh` is found, or an error with a clear message if not.
/// Call this early in `gru init` and `gru lab` startup.
#[allow(dead_code)]
pub fn check_gh_available() -> Result<()> {
    match std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => anyhow::bail!(
            "The `gh` CLI was found but returned an error. Please verify your `gh` installation."
        ),
        Err(_) => anyhow::bail!(
            "The `gh` CLI is not installed or not on PATH. Install it from https://cli.github.com/"
        ),
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

/// Default maximum resume attempts (exposed for use in resume.rs when no config is available)
pub const DEFAULT_MAX_RESUME_ATTEMPTS: u32 = 3;

fn default_max_resume_attempts() -> u32 {
    DEFAULT_MAX_RESUME_ATTEMPTS
}

/// Parse a repo entry from the config into `(host, owner, repo)`.
///
/// Accepts three formats:
/// - `"owner/repo"` → `("github.com", "owner", "repo")`
/// - `"host/owner/repo"` → `("host", "owner", "repo")` (legacy, host must contain `.`)
/// - `"name:owner/repo"` → resolves name via `github_hosts` map (e.g., `"netflix:corp/service"`)
///
/// Pass `&HashMap::new()` for `github_hosts` if named references aren't needed.
pub fn parse_repo_entry_with_hosts(
    spec: &str,
    github_hosts: &HashMap<String, GhHostConfig>,
) -> Option<(String, String, String)> {
    // Check for "name:owner/repo" format.
    // Only treat as named reference if name looks like an identifier (no dots/slashes),
    // to avoid matching SSH URLs like "git@github.com:owner/repo".
    if let Some((name, rest)) = spec.split_once(':') {
        if name.is_empty() {
            // Reject empty prefix like ":owner/repo"
            return None;
        }
        if !name.contains('.') && !name.contains('/') {
            // Looks like a named host reference (identifier, no dots/slashes)
            let host_config = github_hosts.get(name)?;
            let parts: Vec<&str> = rest.splitn(3, '/').collect();
            if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                return None;
            }
            return Some((
                host_config.host.clone(),
                parts[0].to_string(),
                parts[1].to_string(),
            ));
        }
        // Contains dots/slashes (e.g., SSH URL "git@host:owner/repo") — fall through
    }

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

/// Registry of known GitHub hosts, built from config.
///
/// Always includes an implicit `github.com` entry. Additional hosts come from
/// `[github_hosts.*]` config sections and legacy `host/owner/repo` entries in `daemon.repos`.
#[derive(Debug, Clone)]
pub struct HostRegistry {
    /// Map from hostname to optional web_url override
    hosts: HashMap<String, Option<String>>,
    /// Map from config name to hostname (e.g., "netflix" → "git.netflix.net")
    names: HashMap<String, String>,
}

impl HostRegistry {
    /// Build a `HostRegistry` from a `LabConfig`.
    ///
    /// Includes hosts from `[github_hosts.*]` sections and legacy
    /// `host/owner/repo` entries in `daemon.repos`.
    pub fn from_config(config: &LabConfig) -> Self {
        let mut hosts: HashMap<String, Option<String>> = HashMap::new();
        let mut names: HashMap<String, String> = HashMap::new();

        // Always include github.com
        hosts.insert("github.com".to_string(), None);

        // Add hosts from [github_hosts.*] sections
        for (name, gh_host) in &config.github_hosts {
            hosts
                .entry(gh_host.host.clone())
                .or_insert_with(|| gh_host.web_url.clone());
            names.insert(name.clone(), gh_host.host.clone());
        }

        // Add hosts from legacy daemon.repos entries (host/owner/repo format)
        for repo in &config.daemon.repos {
            if let Some((host, _, _)) = parse_repo_entry_with_hosts(repo, &config.github_hosts) {
                hosts.entry(host).or_insert(None);
            }
        }

        Self { hosts, names }
    }

    /// All known hostnames (always includes `github.com`).
    pub fn all_hosts(&self) -> Vec<String> {
        self.hosts.keys().cloned().collect()
    }

    /// Resolve a config name (e.g., `"netflix"`) to its hostname (e.g., `"git.netflix.net"`).
    /// Used by later phases (Phase 2+) for resolving named host references.
    #[allow(dead_code)]
    pub fn host_for_name(&self, name: &str) -> Option<&str> {
        self.names.get(name).map(|s| s.as_str())
    }

    /// Web URL for a given host. Returns the configured `web_url` if set,
    /// otherwise defaults to `https://{host}`.
    /// Used by later phases (Phase 3+) for building web links in comments/output.
    #[allow(dead_code)]
    pub fn web_url_for(&self, host: &str) -> String {
        if let Some(Some(web_url)) = self.hosts.get(host) {
            web_url.clone()
        } else {
            format!("https://{host}")
        }
    }
}

impl LabConfig {
    /// Generate default config file content with commented-out options.
    ///
    /// Comment convention:
    /// - `# [section]` — single `#` for TOML section headers
    /// - `# # description` / `# key = value` — double `#` for descriptions,
    ///   single `#` + space for option lines (so uncommenting removes one `#` layer)
    pub fn default_config_toml() -> &'static str {
        r#"# Gru configuration file
# Uncomment and modify options as needed.

# # GitHub Enterprise host definitions.
# # Define named hosts, then reference them in daemon.repos as "name:owner/repo".
# [github_hosts.myhost]
# host = "ghe.example.com"
# # web_url = "https://ghe.example.com"  # Optional: only if web UI is on a different domain

# [daemon]
# # Repositories to monitor (required for `gru lab`).
# # Formats: "owner/repo" (github.com), "name:owner/repo" (uses github_hosts), "host/owner/repo" (legacy)
# repos = ["owner/repo", "myhost:org/repo"]
#
# # Polling interval in seconds (default: 30)
# poll_interval_secs = 30
#
# # Maximum concurrent Minion slots (default: 2)
# max_slots = 2
#
# # Label to watch for issues (default: "gru:todo")
# label = "gru:todo"
#
# # Maximum resume attempts before marking a Minion as failed (default: 3)
# max_resume_attempts = 3

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

        if self.daemon.max_resume_attempts == 0 {
            anyhow::bail!("max_resume_attempts must be at least 1");
        }

        // Validate github_hosts entries
        let mut seen_hosts: HashMap<&str, &str> = HashMap::new();
        for (name, gh_host) in &self.github_hosts {
            if gh_host.host.is_empty() {
                anyhow::bail!("[github_hosts.{}]: 'host' must not be empty", name);
            }
            if !gh_host.host.contains('.') {
                anyhow::bail!(
                    "[github_hosts.{}]: 'host' value '{}' does not look like a hostname (no dot)",
                    name,
                    gh_host.host
                );
            }
            if let Some(existing_name) = seen_hosts.get(gh_host.host.as_str()) {
                anyhow::bail!(
                    "[github_hosts.{}]: duplicate host '{}' (already defined by [github_hosts.{}])",
                    name,
                    gh_host.host,
                    existing_name
                );
            }
            seen_hosts.insert(&gh_host.host, name);
        }

        // Validate repo format and host name references
        for repo in &self.daemon.repos {
            // Check for "name:owner/repo" format — validate that the name exists in github_hosts.
            // Only treat as a named reference if the prefix looks like an identifier (no dots/slashes),
            // matching the logic in parse_repo_entry_with_hosts() to avoid rejecting SSH-style URLs.
            if let Some((name, _)) = repo.split_once(':') {
                if name.is_empty() {
                    anyhow::bail!(
                        "Invalid repo format: '{}'. Empty host name prefix before ':'",
                        repo
                    );
                }
                if !name.contains('.')
                    && !name.contains('/')
                    && !self.github_hosts.contains_key(name)
                {
                    anyhow::bail!(
                        "Unknown host name '{}' in repo '{}'. Add a [github_hosts.{}] section to config.toml",
                        name,
                        repo,
                        name
                    );
                }
            }
            if parse_repo_entry_with_hosts(repo, &self.github_hosts).is_none() {
                anyhow::bail!(
                    "Invalid repo format: '{}'. Expected 'owner/repo', 'host/owner/repo', or 'name:owner/repo'",
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
        assert_eq!(config.daemon.max_resume_attempts, 3);
    }

    #[test]
    fn test_max_resume_attempts_custom() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
max_resume_attempts = 5
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.daemon.max_resume_attempts, 5);
    }

    #[test]
    fn test_default_max_resume_attempts_constant() {
        assert_eq!(DEFAULT_MAX_RESUME_ATTEMPTS, 3);
    }

    #[test]
    fn test_validate_zero_max_resume_attempts() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo".to_string()];
        config.daemon.max_resume_attempts = 0;

        assert!(config.validate().is_err());
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
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
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
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["ghe.example.com", "git.corp.net", "github.com"]);
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
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["ghe.example.com", "github.com"]);
    }

    // --- parse_repo_entry tests ---

    /// Convenience wrapper for tests that don't need named host resolution.
    fn parse_repo_entry(spec: &str) -> Option<(String, String, String)> {
        parse_repo_entry_with_hosts(spec, &HashMap::new())
    }

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
    fn test_parse_repo_entry_named_host() {
        let mut hosts = HashMap::new();
        hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        let result = parse_repo_entry_with_hosts("netflix:corp/service", &hosts);
        assert_eq!(
            result,
            Some((
                "git.netflix.net".to_string(),
                "corp".to_string(),
                "service".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_repo_entry_named_host_unknown_name() {
        let hosts = HashMap::new();
        assert_eq!(
            parse_repo_entry_with_hosts("netflix:corp/service", &hosts),
            None
        );
    }

    #[test]
    fn test_parse_repo_entry_named_host_empty_prefix() {
        let hosts = HashMap::new();
        assert_eq!(parse_repo_entry_with_hosts(":owner/repo", &hosts), None);
    }

    #[test]
    fn test_parse_repo_entry_named_host_invalid_rest() {
        let mut hosts = HashMap::new();
        hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: None,
            },
        );
        // Missing repo part
        assert_eq!(parse_repo_entry_with_hosts("netflix:corp", &hosts), None);
        // Empty owner
        assert_eq!(parse_repo_entry_with_hosts("netflix:/repo", &hosts), None);
        // Empty repo
        assert_eq!(parse_repo_entry_with_hosts("netflix:corp/", &hosts), None);
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

    // --- HostRegistry tests ---

    #[test]
    fn test_host_registry_default_config() {
        let config = LabConfig::default();
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["github.com"]);
    }

    #[test]
    fn test_host_registry_from_config_with_github_hosts() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["git.netflix.net", "github.com"]);
    }

    #[test]
    fn test_host_registry_includes_legacy_repo_hosts() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["ghe.example.com/org/repo".to_string()];
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["ghe.example.com", "github.com"]);
    }

    #[test]
    fn test_host_registry_includes_named_repo_hosts() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: None,
            },
        );
        config.daemon.repos = vec!["netflix:corp/service".to_string()];
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["git.netflix.net", "github.com"]);
    }

    #[test]
    fn test_host_registry_web_url_for_default() {
        let config = LabConfig::default();
        let registry = HostRegistry::from_config(&config);
        assert_eq!(registry.web_url_for("github.com"), "https://github.com");
        assert_eq!(registry.web_url_for("unknown.host"), "https://unknown.host");
    }

    #[test]
    fn test_host_registry_web_url_for_custom() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        let registry = HostRegistry::from_config(&config);
        assert_eq!(
            registry.web_url_for("git.netflix.net"),
            "https://github.netflix.net"
        );
    }

    #[test]
    fn test_host_registry_web_url_for_no_override() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "corp".to_string(),
            GhHostConfig {
                host: "ghe.corp.com".to_string(),
                web_url: None,
            },
        );
        let registry = HostRegistry::from_config(&config);
        assert_eq!(registry.web_url_for("ghe.corp.com"), "https://ghe.corp.com");
    }

    // --- host_for_name tests ---

    #[test]
    fn test_host_for_name_found() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: None,
            },
        );
        let registry = HostRegistry::from_config(&config);
        assert_eq!(registry.host_for_name("netflix"), Some("git.netflix.net"));
    }

    #[test]
    fn test_host_for_name_not_found() {
        let config = LabConfig::default();
        let registry = HostRegistry::from_config(&config);
        assert_eq!(registry.host_for_name("netflix"), None);
    }

    // --- Validation tests for named host references ---

    #[test]
    fn test_validate_named_host_reference_valid() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: None,
            },
        );
        config.daemon.repos = vec!["netflix:corp/service".to_string()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_named_host_reference_unknown() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["netflix:corp/service".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Unknown host name 'netflix'"));
    }

    #[test]
    fn test_validate_named_host_reference_empty_prefix() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec![":owner/repo".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Empty host name prefix"));
    }

    #[test]
    fn test_validate_github_host_empty_host() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "bad".to_string(),
            GhHostConfig {
                host: "".to_string(),
                web_url: None,
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("'host' must not be empty"));
    }

    #[test]
    fn test_validate_github_host_no_dot() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "bad".to_string(),
            GhHostConfig {
                host: "localhost".to_string(),
                web_url: None,
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("does not look like a hostname"));
    }

    #[test]
    fn test_validate_github_host_duplicate_host() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "alpha".to_string(),
            GhHostConfig {
                host: "ghe.example.com".to_string(),
                web_url: None,
            },
        );
        config.github_hosts.insert(
            "beta".to_string(),
            GhHostConfig {
                host: "ghe.example.com".to_string(),
                web_url: Some("https://ghe.example.com".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate host 'ghe.example.com'"));
    }

    // --- Config parsing with github_hosts ---

    #[test]
    fn test_parse_config_with_github_hosts() {
        let config_toml = r#"
[github_hosts.netflix]
host = "git.netflix.net"
web_url = "https://github.netflix.net"

[daemon]
repos = ["netflix:corp/service"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.github_hosts.len(), 1);
        let netflix = config.github_hosts.get("netflix").unwrap();
        assert_eq!(netflix.host, "git.netflix.net");
        assert_eq!(
            netflix.web_url.as_deref(),
            Some("https://github.netflix.net")
        );
        assert_eq!(config.daemon.repos, vec!["netflix:corp/service"]);
    }

    #[test]
    fn test_parse_config_github_hosts_without_web_url() {
        let config_toml = r#"
[github_hosts.corp]
host = "ghe.corp.com"

[daemon]
repos = ["corp:team/app"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        let corp = config.github_hosts.get("corp").unwrap();
        assert_eq!(corp.host, "ghe.corp.com");
        assert!(corp.web_url.is_none());
    }

    #[test]
    fn test_default_config_toml_contains_github_hosts_example() {
        let template = LabConfig::default_config_toml();
        assert!(template.contains("[github_hosts."));
        assert!(template.contains("host ="));
        assert!(template.contains("web_url ="));
    }

    #[test]
    fn config_example_parses() {
        let content = include_str!("../docs/config.example.toml");
        // Stop before the "Full GHES example" section to avoid duplicate table headers
        assert!(
            content.contains("Full GHES example"),
            "docs/config.example.toml must contain a 'Full GHES example' section; \
             update the sentinel if the heading changed"
        );
        let content = content.split("Full GHES example").next().unwrap();
        // Uncomment lines (strip "# " prefix), then keep only TOML-meaningful lines.
        // This filters out descriptive prose while preserving section headers,
        // key = value pairs, array elements, and comments.
        let uncommented: String = content
            .lines()
            .map(|l| l.strip_prefix("# ").unwrap_or(l))
            .filter(|l| {
                let t = l.trim();
                if t.is_empty() || t.starts_with('#') || t.starts_with('[') || t == "]" {
                    return true;
                }
                // key = value: key must be a bare TOML key, value must start
                // with a TOML value token
                if let Some(eq_pos) = t.find(" = ") {
                    let key = t[..eq_pos].trim();
                    let val = t[eq_pos + 3..].trim();
                    return !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                        && (val.starts_with('"')
                            || val.starts_with('\'')
                            || val.starts_with('[')
                            || val.starts_with('{')
                            || val == "true"
                            || val == "false"
                            || val.starts_with(|c: char| c.is_ascii_digit()));
                }
                // Array string element: indented "value", or "value"
                if l.starts_with(' ') && t.starts_with('"') {
                    return t.ends_with("\",") || t.ends_with('"');
                }
                false
            })
            .collect::<Vec<_>>()
            .join("\n");
        let _config: LabConfig = toml::from_str(&uncommented)
            .expect("docs/config.example.toml should parse against LabConfig");
    }
}
