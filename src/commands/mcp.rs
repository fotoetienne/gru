//! `gru mcp` command — start an MCP server or manage its registration.
//!
//! Subcommands:
//! - `gru mcp` (no subcommand) — start the stdio MCP server
//! - `gru mcp install` — register Gru as an MCP server in `~/.claude.json`
//! - `gru mcp uninstall` — remove Gru from `~/.claude.json`

use anyhow::{Context, Result};
use serde_json::json;
use std::path::PathBuf;

/// Path to Claude Code's global MCP config.
fn claude_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".claude.json"))
}

/// Start the MCP server on stdio.
pub async fn handle_mcp_server() -> Result<i32> {
    crate::mcp::run_server().await?;
    Ok(0)
}

/// Register Gru as an MCP server in `~/.claude.json`.
pub async fn handle_mcp_install() -> Result<i32> {
    let config_path = claude_config_path()?;

    // Read existing config or start fresh
    let mut config: serde_json::Value = if config_path.exists() {
        let content = tokio::fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?
    } else {
        json!({})
    };

    // Ensure mcpServers object exists
    let mcp_servers = config
        .as_object_mut()
        .context("Expected JSON object in config")?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));

    let servers = mcp_servers
        .as_object_mut()
        .context("Expected mcpServers to be a JSON object")?;

    // Check if already registered
    if servers.contains_key("gru") {
        println!(
            "Gru MCP server is already registered in {}",
            config_path.display()
        );
        return Ok(0);
    }

    // Register: use command-based config (survives binary updates)
    servers.insert(
        "gru".to_string(),
        json!({
            "command": "gru",
            "args": ["mcp"]
        }),
    );

    // Write back
    let content = serde_json::to_string_pretty(&config)?;
    tokio::fs::write(&config_path, content)
        .await
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    println!("Registered Gru MCP server in {}", config_path.display());
    println!("Restart Claude Code to activate.");
    Ok(0)
}

/// Remove Gru from `~/.claude.json`.
pub async fn handle_mcp_uninstall() -> Result<i32> {
    let config_path = claude_config_path()?;

    if !config_path.exists() {
        println!("No config file found at {}", config_path.display());
        return Ok(0);
    }

    let content = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let mut config: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    // Remove gru entry from mcpServers
    let removed = config
        .as_object_mut()
        .and_then(|obj| obj.get_mut("mcpServers"))
        .and_then(|servers| servers.as_object_mut())
        .and_then(|servers| servers.remove("gru"))
        .is_some();

    if !removed {
        println!(
            "Gru MCP server is not registered in {}",
            config_path.display()
        );
        return Ok(0);
    }

    let content = serde_json::to_string_pretty(&config)?;
    tokio::fs::write(&config_path, content)
        .await
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    println!("Removed Gru MCP server from {}", config_path.display());
    println!("Restart Claude Code to apply.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_config_path() {
        let path = claude_config_path().unwrap();
        assert!(path.ends_with(".claude.json"));
    }

    #[tokio::test]
    async fn test_install_creates_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join(".claude.json");

        // Simulate install by writing directly (avoids touching real ~/.claude.json)
        let config = json!({
            "mcpServers": {
                "gru": {
                    "command": "gru",
                    "args": ["mcp"]
                }
            }
        });
        let content = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, &content).unwrap();

        // Verify structure
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["gru"]["command"], "gru");
        assert_eq!(parsed["mcpServers"]["gru"]["args"][0], "mcp");
    }

    #[tokio::test]
    async fn test_install_preserves_existing_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join(".claude.json");

        // Pre-existing config with another server
        let existing = json!({
            "mcpServers": {
                "other-server": {
                    "command": "other",
                    "args": []
                }
            }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        // Add gru entry
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut config: serde_json::Value = serde_json::from_str(&content).unwrap();
        config["mcpServers"]["gru"] = json!({
            "command": "gru",
            "args": ["mcp"]
        });
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        // Verify both exist
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["other-server"].is_object());
        assert!(parsed["mcpServers"]["gru"].is_object());
    }

    #[tokio::test]
    async fn test_uninstall_removes_gru_only() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join(".claude.json");

        let config = json!({
            "mcpServers": {
                "gru": { "command": "gru", "args": ["mcp"] },
                "other": { "command": "other", "args": [] }
            }
        });
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        // Remove gru
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        parsed["mcpServers"].as_object_mut().unwrap().remove("gru");
        std::fs::write(&config_path, serde_json::to_string_pretty(&parsed).unwrap()).unwrap();

        // Verify gru removed but other remains
        let final_config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(final_config["mcpServers"]["gru"].is_null());
        assert!(final_config["mcpServers"]["other"].is_object());
    }
}
