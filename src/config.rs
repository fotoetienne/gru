use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration for a GitHub Enterprise host
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GhHostConfig {
    /// Hostname for GH_HOST, gh --hostname, and git remote matching
    pub(crate) host: String,
    /// Web UI URL (defaults to https://{host}). Only needed when the web UI
    /// is on a different domain than the git/API host.
    pub(crate) web_url: Option<String>,
}

/// Configuration for Gru Lab daemon mode
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct LabConfig {
    #[serde(default)]
    pub(crate) github_hosts: HashMap<String, GhHostConfig>,

    #[serde(default)]
    pub(crate) daemon: DaemonConfig,

    #[serde(default)]
    pub(crate) agent: AgentConfig,

    #[serde(default)]
    pub(crate) merge: MergeConfig,
}

/// Merge judge configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeConfig {
    /// Confidence threshold (1-10) for the merge-readiness judge.
    /// Only merge when the judge's confidence >= this value.
    #[serde(default = "default_confidence_threshold")]
    pub(crate) confidence_threshold: u8,
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
pub(crate) struct AgentConfig {
    /// Which agent backend to use by default (e.g., "claude")
    #[serde(default = "default_agent_name")]
    pub(crate) default: String,

    /// Claude-specific configuration ([agent.claude] in TOML)
    #[serde(default)]
    pub(crate) claude: ClaudeAgentConfig,
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
pub(crate) struct ClaudeAgentConfig {
    /// Override the binary path for Claude Code CLI
    #[serde(default)]
    pub(crate) binary: Option<String>,
}

fn default_agent_name() -> String {
    "claude".to_string()
}

/// Daemon configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonConfig {
    /// Repositories to monitor for gru:todo issues
    #[serde(default)]
    pub(crate) repos: Vec<String>,

    /// Polling interval in seconds (default: 30s)
    #[serde(default = "default_poll_interval")]
    pub(crate) poll_interval_secs: u64,

    /// Maximum concurrent Minion slots (default: 2)
    #[serde(default = "default_max_slots")]
    pub(crate) max_slots: usize,

    /// Label to watch for issues (default: "gru:todo")
    #[serde(default = "default_label")]
    pub(crate) label: String,

    /// Maximum resume attempts before marking a Minion as failed (default: 3)
    #[serde(default = "default_max_resume_attempts")]
    pub(crate) max_resume_attempts: u32,

    /// Maximum failure retry attempts before giving up (default: 3, 0 = no failure retries).
    /// Continuation retries are always enabled regardless of this setting.
    #[serde(default = "default_max_retry_attempts")]
    pub(crate) max_retry_attempts: u32,

    /// Maximum backoff delay in seconds for failure retries (default: 300 = 5 minutes).
    #[serde(default = "default_max_retry_backoff_secs")]
    pub(crate) max_retry_backoff_secs: u64,

    /// Maximum poll interval in seconds for adaptive backoff (default: 300 = 5 minutes)
    #[serde(default = "default_poll_interval_max")]
    pub(crate) poll_interval_max_secs: u64,

    /// Hours before a stopped Minion with no archivable signal is auto-archived (default: 24).
    /// Applies to stopped Minions that have no PR and whose issue is still open (or has no issue).
    /// Set to 0 to disable TTL-based archiving.
    #[serde(default = "default_archive_ttl_hours")]
    pub(crate) archive_ttl_hours: u64,

    /// Minutes an issue can be gru:in-progress without a live Minion before
    /// auto-recovery resets it to the configured daemon pickup label (`daemon.label`).
    /// Set to 0 to disable (recommended for multi-lab deployments where another
    /// machine may hold the issue). Default: 30 minutes.
    ///
    /// Note: the recovery scan runs every 5 minutes, so values smaller than 5
    /// are accepted but offer no finer detection granularity.
    ///
    /// Note: `updatedAt` is used as a proxy for when the issue was claimed.
    /// Any activity on the issue (including manual comments) will reset this
    /// clock and delay auto-recovery by the threshold duration.
    #[serde(default = "default_recovery_threshold_mins")]
    pub(crate) recovery_threshold_mins: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            max_slots: default_max_slots(),
            label: default_label(),
            max_resume_attempts: default_max_resume_attempts(),
            max_retry_attempts: default_max_retry_attempts(),
            max_retry_backoff_secs: default_max_retry_backoff_secs(),
            poll_interval_max_secs: default_poll_interval_max(),
            archive_ttl_hours: default_archive_ttl_hours(),
            recovery_threshold_mins: default_recovery_threshold_mins(),
        }
    }
}

// Thread-local override for the config path in tests.
// When set, `try_load_config()` loads from this path instead of `~/.gru/config.toml`.
#[cfg(test)]
thread_local! {
    static TEST_CONFIG_PATH_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Sets a thread-local config path override for the current test.
///
/// While the returned guard is alive, `try_load_config()` on this thread
/// will load from the provided path instead of `~/.gru/config.toml`.
/// The override is automatically cleared when the guard is dropped.
#[cfg(test)]
pub(crate) fn set_test_config_path(path: PathBuf) -> TestConfigGuard {
    TEST_CONFIG_PATH_OVERRIDE.with(|cell| {
        *cell.borrow_mut() = Some(path);
    });
    TestConfigGuard { _private: () }
}

/// RAII guard that clears the thread-local config path override on drop.
#[cfg(test)]
pub(crate) struct TestConfigGuard {
    _private: (),
}

#[cfg(test)]
impl Drop for TestConfigGuard {
    fn drop(&mut self) {
        TEST_CONFIG_PATH_OVERRIDE.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Try to load the config file, returning `None` when no config is usable.
///
/// Distinguishes two cases:
/// - The config file doesn't exist — returns `None` silently (normal for
///   users who never ran `gru init`).
/// - The config file exists but fails to parse or validate — logs a
///   `warn!` with the error and returns `None`. Callers that fall back to
///   defaults (like [`load_host_registry`]) won't exit, but the user sees
///   why their configured `[github_hosts.*]` entries were ignored.
///
/// In test builds, checks for a thread-local path override set via
/// [`set_test_config_path`] before falling back to the default path.
pub(crate) fn try_load_config() -> Option<LabConfig> {
    #[cfg(test)]
    {
        let override_path = TEST_CONFIG_PATH_OVERRIDE.with(|cell| cell.borrow().clone());
        if let Some(path) = override_path {
            return load_or_warn(&path);
        }
    }
    let path = LabConfig::default_path().ok()?;
    load_or_warn(&path)
}

/// Loads config from `path` if it exists, logging any parse/validation error.
///
/// Returns `None` both when the file is absent and when loading fails —
/// the caller can't distinguish the two, which matches the silent-fallback
/// contract — but failures are surfaced via the log rather than swallowed.
fn load_or_warn(path: &Path) -> Option<LabConfig> {
    if !path.exists() {
        return None;
    }
    match LabConfig::load_partial(path) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            log::warn!(
                "Failed to load config file {}: {:#}. Falling back to defaults; \
                 configured [github_hosts.*] entries will be ignored until this is fixed.",
                path.display(),
                err
            );
            None
        }
    }
}

/// Load a `HostRegistry` from the default config file.
///
/// Returns a registry with just `github.com` if the config can't be loaded.
/// This is a convenience for callers that need host info but don't require
/// full daemon config validation.
///
/// Respects the test config path override set via [`set_test_config_path`].
pub(crate) fn load_host_registry() -> HostRegistry {
    match try_load_config() {
        Some(cfg) => HostRegistry::from_config(&cfg),
        None => HostRegistry::from_config(&LabConfig::default()),
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
pub(crate) const DEFAULT_MAX_RESUME_ATTEMPTS: u32 = 3;

fn default_max_resume_attempts() -> u32 {
    DEFAULT_MAX_RESUME_ATTEMPTS
}

fn default_max_retry_attempts() -> u32 {
    3
}

fn default_max_retry_backoff_secs() -> u64 {
    300
}

fn default_poll_interval_max() -> u64 {
    300
}

/// Default TTL (in hours) before a stopped Minion with no signal is auto-archived.
pub(crate) const DEFAULT_ARCHIVE_TTL_HOURS: u64 = 24;

fn default_archive_ttl_hours() -> u64 {
    DEFAULT_ARCHIVE_TTL_HOURS
}

/// Default recovery threshold in minutes before a stuck gru:in-progress issue is reset.
pub(crate) const DEFAULT_RECOVERY_THRESHOLD_MINS: u64 = 30;

fn default_recovery_threshold_mins() -> u64 {
    DEFAULT_RECOVERY_THRESHOLD_MINS
}

/// Parse a repo entry from the config into `(host, owner, repo)`.
///
/// Accepts three formats:
/// - `"owner/repo"` → `("github.com", "owner", "repo")`
/// - `"host/owner/repo"` → `("host", "owner", "repo")` (legacy, host must contain `.`)
/// - `"name:owner/repo"` → resolves name via `github_hosts` map (e.g., `"netflix:corp/service"`)
///
/// Pass `&HashMap::new()` for `github_hosts` if named references aren't needed.
pub(crate) fn parse_repo_entry_with_hosts(
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
pub(crate) struct HostRegistry {
    /// Map from hostname to optional web_url override
    hosts: HashMap<String, Option<String>>,
}

impl HostRegistry {
    /// Build a `HostRegistry` from a `LabConfig`.
    ///
    /// Includes hosts from `[github_hosts.*]` sections and legacy
    /// `host/owner/repo` entries in `daemon.repos`.
    pub(crate) fn from_config(config: &LabConfig) -> Self {
        let mut hosts: HashMap<String, Option<String>> = HashMap::new();

        // Always include github.com
        hosts.insert("github.com".to_string(), None);

        // Add hosts from [github_hosts.*] sections
        for gh_host in config.github_hosts.values() {
            hosts
                .entry(gh_host.host.clone())
                .or_insert_with(|| gh_host.web_url.clone());
        }

        // Add hosts from legacy daemon.repos entries (host/owner/repo format)
        for repo in &config.daemon.repos {
            if let Some((host, _, _)) = parse_repo_entry_with_hosts(repo, &config.github_hosts) {
                hosts.entry(host).or_insert(None);
            }
        }

        Self { hosts }
    }

    /// All hostnames recognized when matching a GitHub URL: every API host plus
    /// every configured `web_url` host.
    ///
    /// Web UI and API hosts may differ (e.g. Netflix has the API at
    /// `git.netflix.net` and the web UI at `github.netflix.net`). A URL using
    /// either form is a legitimate reference to the same repository, so URL
    /// parsers match against this broader list. After a match, resolve the
    /// matched host back to the API host via [`HostRegistry::canonical_host`].
    pub(crate) fn all_url_hosts(&self) -> Vec<String> {
        let mut result: Vec<String> = self.hosts.keys().cloned().collect();
        for web_url in self.hosts.values().flatten() {
            if let Some(host) = web_url_to_host(web_url) {
                if !result.iter().any(|h| h == &host) {
                    result.push(host);
                }
            }
        }
        result
    }

    /// Maps any recognized hostname back to its canonical API host.
    ///
    /// If `host` is already a known API host, returns it. If it matches a
    /// configured `web_url` hostname, returns the associated API host. Returns
    /// `None` when `host` is unknown.
    pub(crate) fn canonical_host(&self, host: &str) -> Option<String> {
        if self.hosts.contains_key(host) {
            return Some(host.to_string());
        }
        for (api_host, web_url_opt) in &self.hosts {
            if let Some(web_url) = web_url_opt {
                if let Some(web_host) = web_url_to_host(web_url) {
                    if web_host == host {
                        return Some(api_host.clone());
                    }
                }
            }
        }
        None
    }
}

/// Extracts the hostname from a configured `web_url` value.
///
/// Accepts values like `"https://github.netflix.net"`,
/// `"https://github.netflix.net/"`, or `"https://github.netflix.net:8080"`,
/// returning `"github.netflix.net"`. Returns `None` when the value is missing
/// an `http(s)://` prefix or the hostname is empty.
fn web_url_to_host(web_url: &str) -> Option<String> {
    let s = web_url.trim();
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))?;
    let end = rest.find(['/', ':']).unwrap_or(rest.len());
    let host = &rest[..end];
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Validates a `[github_hosts.<name>].web_url` value, failing fast on shapes
/// that `web_url_to_host` would silently discard (missing scheme, empty
/// hostname, no dot). Runs at config-load time so misconfigurations surface
/// immediately instead of causing URL parsing to quietly ignore the entry.
fn validate_web_url(host_name: &str, web_url: &str) -> Result<()> {
    let trimmed = web_url.trim();
    if trimmed.is_empty() {
        anyhow::bail!("[github_hosts.{}]: 'web_url' must not be empty", host_name);
    }
    let host = web_url_to_host(trimmed).ok_or_else(|| {
        anyhow::anyhow!(
            "[github_hosts.{}]: 'web_url' value '{}' is not a valid URL \
             (expected 'https://<hostname>' or 'http://<hostname>')",
            host_name,
            web_url
        )
    })?;
    if !host.contains('.') {
        anyhow::bail!(
            "[github_hosts.{}]: 'web_url' hostname '{}' does not look like a hostname (no dot)",
            host_name,
            host
        );
    }
    Ok(())
}

impl LabConfig {
    /// Generate default config file content with commented-out options.
    ///
    /// Comment convention:
    /// - `# [section]` — single `#` for TOML section headers
    /// - `# # description` / `# key = value` — double `#` for descriptions,
    ///   single `#` + space for option lines (so uncommenting removes one `#` layer)
    pub(crate) fn default_config_toml() -> &'static str {
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
#
# # Hours before a stopped Minion with no signal is auto-archived (default: 24)
# archive_ttl_hours = 24

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
    pub(crate) fn write_default_config(path: &Path) -> Result<bool> {
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

    /// Add a repository entry to `daemon.repos` in an existing config file,
    /// preserving comments, formatting, and all other settings.
    ///
    /// Uses `toml_edit` for read-modify-write to keep user-edited config intact.
    /// Returns `Ok(true)` if the entry was added, `Ok(false)` if already present.
    pub fn add_repo_to_config(config_path: &Path, repo_entry: &str) -> Result<bool> {
        use std::io::Write;
        let contents = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config: {}", config_path.display()))?;

        let mut doc = contents
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("Failed to parse config: {}", config_path.display()))?;

        if !doc.contains_key("daemon") {
            // When the document is all-comments (e.g., default template), toml_edit
            // would insert the new table before the comments. Instead, we append the
            // section as raw TOML at the end of the file after building it.
            let repo_value = toml_edit::value(repo_entry);
            let output = format!(
                "{}\n[daemon]\nrepos = [{}]\n",
                contents.trim_end(),
                repo_value
            );
            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(config_path)
                .with_context(|| {
                    format!(
                        "Failed to open config for writing: {}",
                        config_path.display()
                    )
                })?;
            file.write_all(output.as_bytes())
                .with_context(|| format!("Failed to write config: {}", config_path.display()))?;
            return Ok(true);
        }

        let daemon = doc["daemon"]
            .as_table_mut()
            .context("'daemon' is not a table")?;

        // Ensure repos array exists
        if !daemon.contains_key("repos") {
            daemon["repos"] = toml_edit::value(toml_edit::Array::new());
        }

        let repos = daemon["repos"]
            .as_array_mut()
            .context("'daemon.repos' is not an array")?;

        // Check if already present
        for item in repos.iter() {
            if item.as_str() == Some(repo_entry) {
                return Ok(false);
            }
        }

        repos.push(repo_entry);

        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(config_path)
            .with_context(|| {
                format!(
                    "Failed to open config for writing: {}",
                    config_path.display()
                )
            })?;
        file.write_all(doc.to_string().as_bytes())
            .with_context(|| format!("Failed to write config: {}", config_path.display()))?;

        Ok(true)
    }

    /// Load configuration from file (validates daemon config — use for `gru lab`).
    pub(crate) fn load(path: &Path) -> Result<Self> {
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
    /// Still validates `[github_hosts.*]` since those entries drive URL
    /// parsing used by every command.
    pub(crate) fn load_partial(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: LabConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        config.validate_github_hosts()?;

        Ok(config)
    }

    /// Get default config file path (~/.gru/config.toml)
    pub(crate) fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        Ok(home.join(".gru").join("config.toml"))
    }

    /// Validate configuration
    pub(crate) fn validate(&self) -> Result<()> {
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

        if self.daemon.poll_interval_max_secs < self.daemon.poll_interval_secs {
            anyhow::bail!(
                "poll_interval_max_secs ({}) must be >= poll_interval_secs ({})",
                self.daemon.poll_interval_max_secs,
                self.daemon.poll_interval_secs,
            );
        }

        if self.daemon.max_resume_attempts == 0 {
            anyhow::bail!("max_resume_attempts must be at least 1");
        }

        self.validate_github_hosts()?;

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

    /// Validate the `[github_hosts.*]` section.
    ///
    /// Runs independently of daemon validation because these entries drive
    /// URL parsing used by every command — a misconfigured `web_url` would
    /// silently cause web UI URLs to be rejected without the user learning
    /// why, so this runs in both `load()` and `load_partial()`.
    ///
    /// Rejects:
    /// - Empty / malformed `host` or `web_url` values.
    /// - Two entries with the same API `host`.
    /// - Two entries whose derived `web_url` hostnames collide — either with
    ///   each other, or with a different entry's API `host`. Such collisions
    ///   make [`HostRegistry::canonical_host`] ambiguous and the resolution
    ///   order-dependent on `HashMap` iteration.
    fn validate_github_hosts(&self) -> Result<()> {
        // Visit entries in sorted order so error messages are deterministic
        // regardless of the underlying `HashMap` iteration order.
        let mut names: Vec<&String> = self.github_hosts.keys().collect();
        names.sort();

        // Pass 1: validate each entry in isolation and collect the API host
        // owner map. DNS hostnames are case-insensitive, so duplicates are
        // detected using a lowercased key to match how callers compare hosts
        // (see `build_repo_entry` in `src/commands/init.rs`).
        let mut api_hosts: HashMap<String, &str> = HashMap::new();
        for name in &names {
            let gh_host = &self.github_hosts[*name];
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
            let host_key = gh_host.host.to_ascii_lowercase();
            if let Some(existing_name) = api_hosts.get(&host_key) {
                anyhow::bail!(
                    "[github_hosts.{}]: duplicate host '{}' (already defined by [github_hosts.{}])",
                    name,
                    gh_host.host,
                    existing_name
                );
            }
            api_hosts.insert(host_key, name);

            if let Some(web_url) = &gh_host.web_url {
                validate_web_url(name, web_url)?;
            }
        }

        // Pass 2: check that derived web_url hostnames don't collide with
        // other entries' API or web_url hosts.
        let mut web_url_hosts: HashMap<String, &str> = HashMap::new();
        for name in &names {
            let gh_host = &self.github_hosts[*name];
            let Some(web_url) = &gh_host.web_url else {
                continue;
            };
            // Pass 1 already validated the web_url, so the extraction succeeds.
            let web_host = web_url_to_host(web_url)
                .expect("validate_web_url ensures web_url_to_host returns Some");

            let web_host_key = web_host.to_ascii_lowercase();

            // Trivial: a web_url whose hostname equals this entry's own API
            // host is redundant but unambiguous — allow it.
            if web_host_key == gh_host.host.to_ascii_lowercase() {
                continue;
            }

            // Collides with a different entry's API host?
            if let Some(other_name) = api_hosts.get(&web_host_key) {
                if *other_name != name.as_str() {
                    anyhow::bail!(
                        "[github_hosts.{}]: 'web_url' hostname '{}' collides with the 'host' of [github_hosts.{}]",
                        name,
                        web_host,
                        other_name
                    );
                }
            }

            // Collides with another entry's web_url hostname?
            if let Some(other_name) = web_url_hosts.get(&web_host_key) {
                anyhow::bail!(
                    "[github_hosts.{}]: 'web_url' hostname '{}' is also used by [github_hosts.{}]",
                    name,
                    web_host,
                    other_name
                );
            }
            web_url_hosts.insert(web_host_key, name);
        }

        Ok(())
    }

    /// Get poll interval as Duration
    pub(crate) fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.daemon.poll_interval_secs)
    }

    /// Get maximum poll interval as Duration for adaptive backoff
    pub fn poll_interval_max(&self) -> Duration {
        Duration::from_secs(self.daemon.poll_interval_max_secs)
    }

    /// Merge with CLI overrides
    pub(crate) fn with_overrides(
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
            // Ensure max stays >= base so validation won't reject the override
            if self.daemon.poll_interval_max_secs < interval {
                self.daemon.poll_interval_max_secs = interval;
            }
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
        assert_eq!(config.daemon.archive_ttl_hours, 24);
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
    fn test_default_archive_ttl_hours_constant() {
        assert_eq!(DEFAULT_ARCHIVE_TTL_HOURS, 24);
    }

    #[test]
    fn test_archive_ttl_hours_default() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.daemon.archive_ttl_hours, 24);
    }

    #[test]
    fn test_archive_ttl_hours_custom() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
archive_ttl_hours = 48
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load(temp_file.path()).unwrap();
        assert_eq!(config.daemon.archive_ttl_hours, 48);
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
    fn test_validate_poll_interval_max_less_than_base() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo".to_string()];
        config.daemon.poll_interval_secs = 60;
        config.daemon.poll_interval_max_secs = 30;

        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("poll_interval_max_secs"),
            "expected validation error about poll_interval_max_secs, got: {err}"
        );
    }

    #[test]
    fn test_default_poll_interval_max() {
        let config = DaemonConfig::default();
        assert_eq!(config.poll_interval_max_secs, 300);
    }

    #[test]
    fn test_with_overrides_bumps_max_when_base_exceeds_it() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["owner/repo".to_string()];
        config.daemon.poll_interval_max_secs = 300;

        // CLI sets base higher than max — max should be bumped automatically
        let config = config.with_overrides(None, Some(600), None);
        assert_eq!(config.daemon.poll_interval_secs, 600);
        assert_eq!(config.daemon.poll_interval_max_secs, 600);
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
        let mut hosts = registry.all_url_hosts();
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
        let mut hosts = registry.all_url_hosts();
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
        let mut hosts = registry.all_url_hosts();
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
        let mut hosts = registry.all_url_hosts();
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
        // all_url_hosts includes both API host (git.netflix.net) and web UI host (github.netflix.net)
        let mut hosts = registry.all_url_hosts();
        hosts.sort();
        assert_eq!(
            hosts,
            vec!["git.netflix.net", "github.com", "github.netflix.net"]
        );
    }

    #[test]
    fn test_canonical_host_resolves_web_url_to_api_host() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        let registry = HostRegistry::from_config(&config);

        // Web UI hostname resolves to API host
        assert_eq!(
            registry.canonical_host("github.netflix.net").as_deref(),
            Some("git.netflix.net")
        );
        // API host resolves to itself
        assert_eq!(
            registry.canonical_host("git.netflix.net").as_deref(),
            Some("git.netflix.net")
        );
        // github.com is always recognized
        assert_eq!(
            registry.canonical_host("github.com").as_deref(),
            Some("github.com")
        );
        // Unknown hosts return None
        assert_eq!(registry.canonical_host("unknown.example.com"), None);
    }

    #[test]
    fn test_canonical_host_without_web_url() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "ghe".to_string(),
            GhHostConfig {
                host: "ghe.example.com".to_string(),
                web_url: None,
            },
        );
        let registry = HostRegistry::from_config(&config);

        // Configured API host resolves to itself
        assert_eq!(
            registry.canonical_host("ghe.example.com").as_deref(),
            Some("ghe.example.com")
        );
        // No web URL is configured, so nothing maps here
        assert_eq!(registry.canonical_host("ghe-web.example.com"), None);
    }

    #[test]
    fn test_all_url_hosts_without_web_url() {
        // Without web_url, all_url_hosts returns the same set as the API hosts
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "ghe".to_string(),
            GhHostConfig {
                host: "ghe.example.com".to_string(),
                web_url: None,
            },
        );
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_url_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["ghe.example.com", "github.com"]);
    }

    #[test]
    fn test_all_url_hosts_web_url_with_port_and_path() {
        // Hostname extraction strips scheme, path, and port
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net:443/".to_string()),
            },
        );
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_url_hosts();
        hosts.sort();
        assert_eq!(
            hosts,
            vec!["git.netflix.net", "github.com", "github.netflix.net"]
        );
    }

    #[test]
    fn test_host_registry_includes_legacy_repo_hosts() {
        let mut config = LabConfig::default();
        config.daemon.repos = vec!["ghe.example.com/org/repo".to_string()];
        let registry = HostRegistry::from_config(&config);
        let mut hosts = registry.all_url_hosts();
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
        let mut hosts = registry.all_url_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["git.netflix.net", "github.com"]);
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

    #[test]
    fn test_validate_github_host_duplicate_host_case_insensitive() {
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
                host: "GHE.Example.COM".to_string(),
                web_url: None,
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate host"));
    }

    #[test]
    fn test_validate_github_host_rejects_web_url_missing_scheme() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("github.netflix.net".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("is not a valid URL"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_github_host_rejects_empty_web_url() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some(String::new()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "unexpected error: {err}");
    }

    #[test]
    fn test_validate_github_host_rejects_web_url_empty_hostname() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https:///path".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("is not a valid URL"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_github_host_rejects_web_url_no_dot() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "local".to_string(),
            GhHostConfig {
                host: "git.local.example.com".to_string(),
                web_url: Some("https://localhost".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("does not look like a hostname"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_github_host_accepts_valid_web_url() {
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://github.netflix.net".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_github_host_rejects_duplicate_web_url_hosts() {
        // Two entries claim the same web_url hostname → canonical_host()
        // would resolve non-deterministically.
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "alpha".to_string(),
            GhHostConfig {
                host: "git.alpha.example.com".to_string(),
                web_url: Some("https://web.example.com".to_string()),
            },
        );
        config.github_hosts.insert(
            "beta".to_string(),
            GhHostConfig {
                host: "git.beta.example.com".to_string(),
                web_url: Some("https://web.example.com".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("'web_url' hostname 'web.example.com' is also used by"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_github_host_rejects_web_url_colliding_with_other_api_host() {
        // One entry's web_url hostname equals another entry's API host →
        // ambiguous mapping.
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "alpha".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: None,
            },
        );
        config.github_hosts.insert(
            "beta".to_string(),
            GhHostConfig {
                host: "git.other.net".to_string(),
                web_url: Some("https://git.netflix.net".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("'web_url' hostname 'git.netflix.net' collides with the 'host'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_github_host_allows_web_url_equal_to_own_api_host() {
        // Redundant but unambiguous: web_url hostname matches this entry's
        // own API host. Allowed because it doesn't create ambiguity.
        let mut config = LabConfig::default();
        config.github_hosts.insert(
            "netflix".to_string(),
            GhHostConfig {
                host: "git.netflix.net".to_string(),
                web_url: Some("https://git.netflix.net".to_string()),
            },
        );
        config.daemon.repos = vec!["owner/repo".to_string()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_load_partial_validates_web_url() {
        // load_partial() must also reject a malformed web_url because URL
        // parsing is used by non-daemon commands like `gru do`.
        let config_toml = r#"
[github_hosts.netflix]
host = "git.netflix.net"
web_url = "github.netflix.net"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let err = LabConfig::load_partial(temp_file.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is not a valid URL"),
            "unexpected error: {err}"
        );
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
        // Uncomment lines (strip repeated "# " prefixes to handle double-commented
        // lines like `# # web_url = ...`), then keep only TOML-meaningful lines.
        // This filters out descriptive prose while preserving section headers,
        // key = value pairs, array elements, and comments.
        let uncommented: String = content
            .lines()
            .map(|l| {
                let mut s = l;
                while let Some(stripped) = s.strip_prefix("# ") {
                    s = stripped;
                }
                s
            })
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
        // Use serde_ignored to detect fields in the example that don't exist
        // in LabConfig (e.g., renamed or stale fields). Plain toml::from_str
        // silently ignores unknown fields since the structs don't use
        // #[serde(deny_unknown_fields)].
        let mut ignored = Vec::new();
        let deserializer = toml::Deserializer::new(&uncommented);
        let _config: LabConfig = serde_ignored::deserialize(deserializer, |path| {
            ignored.push(path.to_string());
        })
        .expect("docs/config.example.toml should parse against LabConfig");
        assert!(
            ignored.is_empty(),
            "docs/config.example.toml contains fields not recognized by LabConfig: {:?}",
            ignored
        );
    }

    #[test]
    fn test_load_partial_loads_config() {
        let config_toml = r#"
[agent]
default = "codex"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = LabConfig::load_partial(temp_file.path()).unwrap();
        assert_eq!(config.agent.default, "codex");
    }

    #[test]
    fn test_load_partial_nonexistent_file_returns_error() {
        let result = LabConfig::load_partial(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_set_test_config_path_overrides_try_load() {
        let config_toml = r#"
[agent]
default = "test-agent"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let _guard = super::set_test_config_path(temp_file.path().to_path_buf());
        let config = super::try_load_config().expect("should load from override path");
        assert_eq!(config.agent.default, "test-agent");
    }

    #[test]
    fn test_try_load_config_missing_file_returns_none() {
        // A path that doesn't exist returns None without any log output.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created.toml");

        let _guard = super::set_test_config_path(missing);
        assert!(super::try_load_config().is_none());
    }

    #[test]
    fn test_try_load_config_invalid_file_returns_none_and_logs() {
        // A file that exists but fails validation returns None. load_or_warn
        // emits the error via log::warn rather than swallowing it silently —
        // we can't easily capture the log in a unit test, but we can at
        // least confirm invalid config doesn't masquerade as valid.
        let mut temp_file = NamedTempFile::new().unwrap();
        let bad_config = r#"
[github_hosts.netflix]
host = "git.netflix.net"
web_url = "github.netflix.net"
"#;
        temp_file.write_all(bad_config.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let _guard = super::set_test_config_path(temp_file.path().to_path_buf());
        assert!(
            super::try_load_config().is_none(),
            "try_load_config should return None when the file fails validation"
        );
    }

    #[test]
    fn test_load_host_registry_falls_back_when_config_invalid() {
        // When the config file is present but invalid, load_host_registry
        // falls back to the default registry rather than panicking.
        let mut temp_file = NamedTempFile::new().unwrap();
        let bad_config = r#"
[github_hosts.netflix]
host = "git.netflix.net"
web_url = ""
"#;
        temp_file.write_all(bad_config.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let _guard = super::set_test_config_path(temp_file.path().to_path_buf());
        let registry = super::load_host_registry();
        // Default registry only has github.com — the malformed entry is
        // dropped rather than silently accepted.
        let mut hosts = registry.all_url_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["github.com"]);
    }

    #[test]
    fn test_set_test_config_path_guard_clears_on_drop() {
        let config_toml = "[agent]\ndefault = \"test-agent\"\n";
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        {
            let _guard = super::set_test_config_path(temp_file.path().to_path_buf());
            assert!(super::try_load_config().is_some());
        }

        // After guard drops, the override should be cleared
        super::TEST_CONFIG_PATH_OVERRIDE.with(|cell| {
            assert!(cell.borrow().is_none());
        });
    }

    #[test]
    fn test_add_repo_to_config_new_daemon_section() {
        // Config exists but has no [daemon] section
        let config_toml = "# My config\n";
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = LabConfig::add_repo_to_config(temp_file.path(), "owner/repo").unwrap();
        assert!(result, "should return true when repo was added");

        let contents = fs::read_to_string(temp_file.path()).unwrap();
        assert!(contents.contains("# My config"), "should preserve comments");
        let reloaded: LabConfig = toml::from_str(&contents).unwrap();
        assert_eq!(reloaded.daemon.repos, vec!["owner/repo"]);
    }

    #[test]
    fn test_add_repo_to_config_existing_repos() {
        let config_toml = r#"
[daemon]
repos = ["foo/bar"]
poll_interval_secs = 60
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = LabConfig::add_repo_to_config(temp_file.path(), "baz/qux").unwrap();
        assert!(result);

        let contents = fs::read_to_string(temp_file.path()).unwrap();
        let reloaded: LabConfig = toml::from_str(&contents).unwrap();
        assert_eq!(reloaded.daemon.repos, vec!["foo/bar", "baz/qux"]);
        assert_eq!(
            reloaded.daemon.poll_interval_secs, 60,
            "should preserve other settings"
        );
    }

    #[test]
    fn test_add_repo_to_config_already_present() {
        let config_toml = r#"
[daemon]
repos = ["owner/repo"]
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = LabConfig::add_repo_to_config(temp_file.path(), "owner/repo").unwrap();
        assert!(!result, "should return false when repo already present");

        let contents = fs::read_to_string(temp_file.path()).unwrap();
        let reloaded: LabConfig = toml::from_str(&contents).unwrap();
        assert_eq!(reloaded.daemon.repos, vec!["owner/repo"]);
    }

    #[test]
    fn test_add_repo_to_config_with_host() {
        let config_toml = "# empty config\n";
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_toml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = LabConfig::add_repo_to_config(temp_file.path(), "ghe.co/owner/repo").unwrap();
        assert!(result);

        let contents = fs::read_to_string(temp_file.path()).unwrap();
        let reloaded: LabConfig = toml::from_str(&contents).unwrap();
        assert_eq!(reloaded.daemon.repos, vec!["ghe.co/owner/repo"]);
    }

    #[test]
    fn test_add_repo_to_config_default_config_template() {
        // Simulate the flow: write_default_config then add_repo_to_config
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        LabConfig::write_default_config(&config_path).unwrap();
        let result = LabConfig::add_repo_to_config(&config_path, "owner/repo").unwrap();
        assert!(result);

        let contents = fs::read_to_string(&config_path).unwrap();
        // Comments from default template should be preserved
        assert!(contents.contains("# Gru configuration file"));
        // [daemon] section should appear after the header comment, not before
        let comment_pos = contents.find("# Gru configuration file").unwrap();
        let daemon_pos = contents.find("[daemon]").unwrap();
        assert!(
            comment_pos < daemon_pos,
            "[daemon] should appear after the file header comment"
        );
        let reloaded: LabConfig = toml::from_str(&contents).unwrap();
        assert_eq!(reloaded.daemon.repos, vec!["owner/repo"]);
    }
}
