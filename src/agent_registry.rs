//! Agent backend registry for resolving agent names to backend implementations.
//!
//! Provides validation and construction of `AgentBackend` instances from
//! user-provided agent names (e.g., `--agent claude`).

use crate::agent::AgentBackend;
use crate::claude_backend::ClaudeBackend;
use crate::codex_backend::CodexBackend;

/// Known agent backend names.
const AVAILABLE_AGENTS: &[&str] = &["claude", "codex"];

/// Default agent name when none is specified.
pub const DEFAULT_AGENT: &str = "claude";

/// Resolves an agent name to a concrete `AgentBackend` implementation.
///
/// Returns an error with available agents listed if the name is unknown.
pub fn resolve_backend(agent_name: &str) -> anyhow::Result<Box<dyn AgentBackend>> {
    match agent_name {
        "claude" => Ok(Box::new(ClaudeBackend::new())),
        "codex" => Ok(Box::new(CodexBackend::new())),
        unknown => {
            let available = AVAILABLE_AGENTS.join(", ");
            anyhow::bail!("Unknown agent '{}'. Available: {}", unknown, available);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_claude() {
        assert!(resolve_backend("claude").is_ok());
    }

    #[test]
    fn test_resolve_codex() {
        let backend = resolve_backend("codex").unwrap();
        assert_eq!(backend.name(), "codex");
    }

    #[test]
    fn test_resolve_unknown_fails() {
        let result = resolve_backend("foo");
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("Unknown agent 'foo'"));
        assert!(msg.contains("Available: claude, codex"));
    }

    #[test]
    fn test_default_agent_is_valid() {
        assert!(resolve_backend(DEFAULT_AGENT).is_ok());
    }

    #[test]
    fn test_available_agents_contains_default() {
        assert!(AVAILABLE_AGENTS.contains(&DEFAULT_AGENT));
    }
}
