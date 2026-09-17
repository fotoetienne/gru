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
pub(crate) const DEFAULT_AGENT: &str = "claude";

/// Constructs a backend for `agent_name` with the given Claude `ci_fix_max_turns`
/// override (ignored by backends other than `claude`).
///
/// This is the single source of truth for mapping an `AVAILABLE_AGENTS` entry to
/// its `AgentBackend` implementation; both `resolve_backend` and
/// `all_process_names` route through it so adding a backend only requires
/// updating `AVAILABLE_AGENTS` and this match.
fn construct_backend(
    agent_name: &str,
    ci_fix_max_turns: Option<u32>,
) -> Option<Box<dyn AgentBackend>> {
    match agent_name {
        "claude" => Some(Box::new(ClaudeBackend::new(ci_fix_max_turns))),
        "codex" => Some(Box::new(CodexBackend)),
        _ => None,
    }
}

/// Returns the deduplicated process names declared by every registered agent
/// backend, without loading user config (backends are constructed with
/// `ci_fix_max_turns: None` since only their static `process_names()` are
/// needed here). Because this routes through `construct_backend` against
/// `AVAILABLE_AGENTS`, adding a backend there automatically covers it here too.
///
/// Used to build a process-scan pattern (e.g., for `gru stop`'s fallback path)
/// that covers all backends instead of hardcoding specific process names.
pub(crate) fn all_process_names() -> Vec<String> {
    let mut names: Vec<String> = AVAILABLE_AGENTS
        .iter()
        .filter_map(|name| construct_backend(name, None))
        .flat_map(|backend| {
            backend
                .process_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Resolves an agent name to a concrete `AgentBackend` implementation.
///
/// Returns an error with available agents listed if the name is unknown.
pub(crate) fn resolve_backend(agent_name: &str) -> anyhow::Result<Box<dyn AgentBackend>> {
    if !AVAILABLE_AGENTS.contains(&agent_name) {
        let available = AVAILABLE_AGENTS.join(", ");
        anyhow::bail!("Unknown agent '{}'. Available: {}", agent_name, available);
    }

    let ci_fix_max_turns = if agent_name == "claude" {
        crate::config::try_load_config().and_then(|c| c.agent.claude.ci_fix_max_turns)
    } else {
        None
    };
    Ok(construct_backend(agent_name, ci_fix_max_turns)
        .expect("agent_name validated against AVAILABLE_AGENTS above"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_claude() {
        let backend = resolve_backend("claude").unwrap();
        assert_eq!(backend.name(), "claude-code");
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
