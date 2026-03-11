//! Agent backend registry.
//!
//! Maps agent names (e.g., "claude") to `AgentBackend` implementations.
//! Initialized at startup with `ClaudeBackend` as the only registered backend.
//! The registry is configured via the `[agent]` section of `config.toml`.

use crate::agent::AgentBackend;
use crate::claude_backend::ClaudeBackend;
use crate::config::AgentConfig;
use anyhow::{bail, Result};
use std::collections::HashMap;

/// Registry that maps agent names to their `AgentBackend` implementations.
pub struct AgentRegistry {
    backends: HashMap<String, Box<dyn AgentBackend>>,
    default_name: String,
}

impl AgentRegistry {
    /// Create a new registry from configuration.
    ///
    /// Registers `ClaudeBackend` under the name "claude". If the configured
    /// default agent name doesn't match any registered backend, returns an error.
    pub fn from_config(config: &AgentConfig) -> Result<Self> {
        let mut backends: HashMap<String, Box<dyn AgentBackend>> = HashMap::new();

        // Register the Claude backend (the only one in Phase 1)
        backends.insert("claude".to_string(), Box::new(ClaudeBackend::new()));

        let default_name = config.default.clone();

        // Validate that the default agent exists
        if !backends.contains_key(&default_name) {
            let available: Vec<&str> = backends.keys().map(|k| k.as_str()).collect();
            bail!(
                "Unknown agent '{}'. Available: {}",
                default_name,
                available.join(", ")
            );
        }

        Ok(Self {
            backends,
            default_name,
        })
    }

    /// Create a registry with default configuration (claude as default).
    #[allow(dead_code)] // Phase 2: used when --agent CLI flag is added
    pub fn default_registry() -> Self {
        let mut backends: HashMap<String, Box<dyn AgentBackend>> = HashMap::new();
        backends.insert("claude".to_string(), Box::new(ClaudeBackend::new()));
        Self {
            backends,
            default_name: "claude".to_string(),
        }
    }

    /// Get the default agent backend.
    pub fn default_backend(&self) -> &dyn AgentBackend {
        self.backends
            .get(&self.default_name)
            .expect("default backend must exist (validated at construction)")
            .as_ref()
    }

    /// Get an agent backend by name.
    #[allow(dead_code)] // Phase 2: used when --agent CLI flag is added
    pub fn get(&self, name: &str) -> Result<&dyn AgentBackend> {
        match self.backends.get(name) {
            Some(backend) => Ok(backend.as_ref()),
            None => {
                let available: Vec<&str> = self.backends.keys().map(|k| k.as_str()).collect();
                bail!(
                    "Unknown agent '{}'. Available: {}",
                    name,
                    available.join(", ")
                );
            }
        }
    }

    /// Returns the name of the default agent.
    #[allow(dead_code)] // Phase 2: used when --agent CLI flag is added
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Returns the names of all registered backends.
    #[cfg(test)]
    pub fn available_backends(&self) -> Vec<&str> {
        self.backends.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    #[test]
    fn test_default_registry() {
        let registry = AgentRegistry::default_registry();
        assert_eq!(registry.default_name(), "claude");
        assert_eq!(registry.default_backend().name(), "claude-code");
    }

    #[test]
    fn test_from_config_default() {
        let config = AgentConfig::default();
        let registry = AgentRegistry::from_config(&config).unwrap();
        assert_eq!(registry.default_name(), "claude");
        assert_eq!(registry.default_backend().name(), "claude-code");
    }

    #[test]
    fn test_from_config_unknown_default_errors() {
        let config = AgentConfig {
            default: "foo".to_string(),
            ..Default::default()
        };
        let result = AgentRegistry::from_config(&config);
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("Unknown agent 'foo'"), "got: {}", msg);
        assert!(msg.contains("claude"), "got: {}", msg);
    }

    #[test]
    fn test_get_known_backend() {
        let registry = AgentRegistry::default_registry();
        let backend = registry.get("claude").unwrap();
        assert_eq!(backend.name(), "claude-code");
    }

    #[test]
    fn test_get_unknown_backend_errors() {
        let registry = AgentRegistry::default_registry();
        let result = registry.get("aider");
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("Unknown agent 'aider'"), "got: {}", msg);
        assert!(msg.contains("claude"), "got: {}", msg);
    }

    #[test]
    fn test_available_backends() {
        let registry = AgentRegistry::default_registry();
        let backends = registry.available_backends();
        assert_eq!(backends, vec!["claude"]);
    }
}
