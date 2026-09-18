//! Agent backend registry for resolving agent names to backend implementations.
//!
//! Provides validation and construction of `AgentBackend` instances from
//! user-provided agent names (e.g., `--agent claude`).

use crate::agent::AgentBackend;
use crate::claude_backend::ClaudeBackend;
use crate::codex_backend::CodexBackend;
use crate::pi_backend::PiBackend;

/// Known agent backend names.
pub(crate) const AVAILABLE_AGENTS: &[&str] = &["claude", "codex", "pi"];

/// Default agent name when none is specified.
pub(crate) const DEFAULT_AGENT: &str = "claude";

/// Per-backend config overrides threaded through `construct_backend`.
///
/// Fields are ignored by backends that don't use them (e.g. `pi_model` is
/// ignored when constructing the `claude` backend).
#[derive(Default)]
struct AgentOverrides {
    claude_ci_fix_max_turns: Option<u32>,
    claude_binary: Option<String>,
    pi_binary: Option<String>,
    pi_model: Option<String>,
    pi_thinking: Option<String>,
}

/// Constructs a backend for `agent_name` with the given per-backend config overrides.
///
/// This is the single source of truth for mapping an `AVAILABLE_AGENTS` entry to
/// its `AgentBackend` implementation; both `resolve_backend` and
/// `all_process_names` route through it so adding a backend only requires
/// updating `AVAILABLE_AGENTS` and this match.
fn construct_backend(agent_name: &str, overrides: AgentOverrides) -> Option<Box<dyn AgentBackend>> {
    match agent_name {
        "claude" => Some(Box::new(ClaudeBackend::new(
            overrides.claude_ci_fix_max_turns,
            overrides.claude_binary,
        ))),
        "codex" => Some(Box::new(CodexBackend)),
        "pi" => Some(Box::new(PiBackend::new(
            overrides.pi_binary,
            overrides.pi_model,
            overrides.pi_thinking,
        ))),
        _ => None,
    }
}

/// Returns the deduplicated process names declared by every registered agent
/// backend, without loading user config (backends are constructed with no
/// overrides since only their static `process_names()` are needed here).
/// Because this routes through `construct_backend` against `AVAILABLE_AGENTS`,
/// adding a backend there automatically covers it here too.
///
/// Used to build a process-scan pattern (e.g., for `gru stop`'s fallback path)
/// that covers all backends instead of hardcoding specific process names.
pub(crate) fn all_process_names() -> Vec<String> {
    let mut names: Vec<String> = AVAILABLE_AGENTS
        .iter()
        .filter_map(|name| construct_backend(name, AgentOverrides::default()))
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

/// Resolves the agent name to use when the user did not pass `--agent`.
///
/// Prefers `config.agent.default` (from `~/.gru/config.toml`); falls back to
/// `DEFAULT_AGENT` when no config is present.
pub(crate) fn resolve_default_agent_name() -> String {
    crate::config::try_load_config()
        .map(|c| c.agent.default)
        .unwrap_or_else(|| DEFAULT_AGENT.to_string())
}

/// Resolves the Claude Code CLI binary path/name to invoke.
///
/// Used by entry points that spawn `claude` directly for a generic interactive
/// session (`gru chat`, `gru pm`/`gru tpm`, and the legacy no-session-id
/// `gru attach` fallback) rather than going through `resolve_backend`'s
/// `ClaudeBackend`. Reads `[agent.claude] binary` from config, falling back to
/// `"claude"` (resolved via `$PATH`) when unset or no config is present.
pub(crate) fn configured_claude_binary() -> String {
    crate::config::try_load_config()
        .and_then(|c| c.agent.claude.binary)
        .unwrap_or_else(|| "claude".to_string())
}

/// Resolves an agent name to a concrete `AgentBackend` implementation.
///
/// Returns an error with available agents listed if the name is unknown.
pub(crate) fn resolve_backend(agent_name: &str) -> anyhow::Result<Box<dyn AgentBackend>> {
    if !AVAILABLE_AGENTS.contains(&agent_name) {
        let available = AVAILABLE_AGENTS.join(", ");
        anyhow::bail!("Unknown agent '{}'. Available: {}", agent_name, available);
    }

    // Load config once and reuse it for every field below — don't call
    // try_load_config() a second time here.
    let config = if agent_name == "claude" || agent_name == "pi" {
        crate::config::try_load_config()
    } else {
        None
    };
    let overrides = AgentOverrides {
        claude_ci_fix_max_turns: config
            .as_ref()
            .and_then(|c| c.agent.claude.ci_fix_max_turns),
        claude_binary: config.as_ref().and_then(|c| c.agent.claude.binary.clone()),
        pi_binary: config.as_ref().and_then(|c| c.agent.pi.binary.clone()),
        pi_model: config.as_ref().and_then(|c| c.agent.pi.model.clone()),
        pi_thinking: config.and_then(|c| c.agent.pi.thinking),
    };
    Ok(construct_backend(agent_name, overrides)
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
    fn test_resolve_pi() {
        let backend = resolve_backend("pi").unwrap();
        assert_eq!(backend.name(), "pi");
    }

    #[test]
    fn test_resolve_unknown_fails() {
        let result = resolve_backend("foo");
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("Unknown agent 'foo'"));
        assert!(msg.contains("Available: claude, codex, pi"));
    }

    #[test]
    fn test_default_agent_is_valid() {
        assert!(resolve_backend(DEFAULT_AGENT).is_ok());
    }

    #[test]
    fn test_available_agents_contains_default() {
        assert!(AVAILABLE_AGENTS.contains(&DEFAULT_AGENT));
    }

    #[test]
    fn test_resolve_default_agent_name_uses_config_default() {
        use std::io::Write;

        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        temp_file
            .write_all(b"[agent]\ndefault = \"codex\"\n")
            .unwrap();
        temp_file.flush().unwrap();

        let _guard = crate::config::set_test_config_path(temp_file.path().to_path_buf());

        let agent_name = resolve_default_agent_name();
        assert_eq!(agent_name, "codex");

        let backend = resolve_backend(&agent_name).unwrap();
        assert_eq!(backend.name(), "codex");
    }

    #[test]
    fn test_resolve_default_agent_name_falls_back_without_config() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created.toml");
        let _guard = crate::config::set_test_config_path(missing);

        assert_eq!(resolve_default_agent_name(), DEFAULT_AGENT);
    }

    #[test]
    fn test_resolve_backend_forwards_configured_binary() {
        use std::io::Write;
        use uuid::Uuid;

        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        temp_file
            .write_all(b"[agent.claude]\nbinary = \"/opt/tools/claude\"\n")
            .unwrap();
        temp_file.flush().unwrap();

        let _guard = crate::config::set_test_config_path(temp_file.path().to_path_buf());

        // Exercise resolve_backend() itself (not ClaudeBackend::new() directly) so
        // this test fails if resolve_backend ever stops reading
        // config.agent.claude.binary before constructing the backend.
        let backend = resolve_backend("claude").unwrap();
        let cmd = backend.build_command(
            std::path::Path::new("/tmp/worktree"),
            &Uuid::nil(),
            "prompt",
            "github.com",
        );
        assert_eq!(cmd.as_std().get_program(), "/opt/tools/claude");
    }

    #[test]
    fn test_resolve_backend_forwards_configured_pi_settings() {
        use std::io::Write;
        use uuid::Uuid;

        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        temp_file
            .write_all(
                b"[agent.pi]\nbinary = \"/opt/tools/pi\"\nmodel = \"anthropic/claude-sonnet-5\"\nthinking = \"high\"\n",
            )
            .unwrap();
        temp_file.flush().unwrap();

        let _guard = crate::config::set_test_config_path(temp_file.path().to_path_buf());

        let backend = resolve_backend("pi").unwrap();
        let cmd = backend.build_command(
            std::path::Path::new("/tmp/worktree"),
            &Uuid::nil(),
            "prompt",
            "github.com",
        );
        let inner = cmd.as_std();
        assert_eq!(inner.get_program(), "/opt/tools/pi");
        let args: Vec<&std::ffi::OsStr> = inner.get_args().collect();
        assert!(args.contains(&"--model".as_ref()));
        assert!(args.contains(&"anthropic/claude-sonnet-5".as_ref()));
        assert!(args.contains(&"--thinking".as_ref()));
        assert!(args.contains(&"high".as_ref()));
    }
}
