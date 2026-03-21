//! MCP (Model Context Protocol) server for Gru.
//!
//! Exposes Gru tools and resources to any MCP-compatible client (e.g., Claude Code)
//! via stdio transport. Started by `gru mcp` and registered via `gru mcp install`.

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::BufRead;

use crate::minion_registry::{self, MinionInfo};

// Embedded skill/guide content (compile-time)
const GUIDE_CONTENT: &str = include_str!("../.claude/skills/gru-guide/SKILL.md");
const PM_SKILL_CONTENT: &str = include_str!("../.claude/skills/product-manager/SKILL.md");
const TPM_SKILL_CONTENT: &str = include_str!("../.claude/skills/project-manager/SKILL.md");

/// Resource URIs
const GUIDE_URI: &str = "gru://guide";
const PM_SKILL_URI: &str = "gru://skills/pm";
const TPM_SKILL_URI: &str = "gru://skills/tpm";

/// Maximum number of log lines returned by `gru_logs`.
const MAX_LOG_LINES: usize = 5_000;

/// Serializable minion summary for MCP tool output.
#[derive(Debug, Serialize)]
struct MinionSummary {
    id: String,
    repo: String,
    issue: Option<u64>,
    command: String,
    branch: String,
    mode: String,
    phase: String,
    pr: Option<String>,
    agent: String,
    is_running: bool,
    started_at: String,
}

impl MinionSummary {
    fn from_registry(id: String, info: &MinionInfo) -> Self {
        Self {
            id,
            repo: info.repo.clone(),
            issue: info.issue,
            command: info.command.clone(),
            branch: info.branch.clone(),
            mode: info.mode.to_string(),
            phase: format!("{:?}", info.orchestration_phase),
            pr: info.pr.clone(),
            agent: info.agent_name.clone(),
            is_running: info.is_running(),
            started_at: info.started_at.to_rfc3339(),
        }
    }
}

/// Parameters for `gru_status` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    /// Optional minion ID, issue number, or PR number to filter by
    #[serde(default)]
    pub filter: Option<String>,
}

/// Parameters for `gru_logs` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogsParams {
    /// Minion ID to get logs for (e.g., "M001")
    pub minion_id: String,
    /// Number of recent events to return (default: 50, max: 5000)
    #[serde(default)]
    pub lines: Option<usize>,
}

/// The Gru MCP server.
#[derive(Clone)]
pub struct GruMcpServer {
    tool_router: ToolRouter<GruMcpServer>,
}

#[tool_router]
impl GruMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Returns the current Minion registry status as JSON.
    #[tool(
        description = "Get the status of all Gru Minions (autonomous coding agents). Returns JSON with each Minion's ID, repo, issue, branch, mode, orchestration phase, PR number, and whether it's running. Optionally filter by minion ID, issue number, or PR number."
    )]
    async fn gru_status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let filter = params.filter;
        let result = minion_registry::with_registry(move |registry| {
            let minions = registry.list();
            let mut summaries: Vec<MinionSummary> = minions
                .iter()
                .map(|(id, info)| MinionSummary::from_registry(id.clone(), info))
                .collect();

            // Apply filter if provided (case-insensitive for minion IDs)
            if let Some(ref filter) = filter {
                if let Ok(num) = filter.parse::<u64>() {
                    summaries.retain(|m| {
                        m.issue == Some(num)
                            || m.pr.as_ref().and_then(|pr| pr.parse::<u64>().ok()) == Some(num)
                    });
                } else {
                    let filter_upper = filter.to_uppercase();
                    let with_prefix = format!("M{}", filter_upper);
                    summaries.retain(|m| m.id.eq_ignore_ascii_case(filter) || m.id == with_prefix);
                }
            }

            Ok(summaries)
        })
        .await;

        match result {
            Ok(summaries) => {
                let json = serde_json::to_string_pretty(&summaries)
                    .unwrap_or_else(|e| json!({"error": e.to_string()}).to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(
                json!({"error": format!("Failed to load registry: {}", e)}).to_string(),
            )])),
        }
    }

    /// Returns recent log events for a Minion.
    #[tool(
        description = "Get recent log events from a Gru Minion's event stream. Returns the last N events (default 50, max 5000) from the Minion's events.jsonl file. Each line is a timestamped agent event in JSONL format. Returns \"No events recorded yet\" if the file is empty."
    )]
    async fn gru_logs(
        &self,
        Parameters(params): Parameters<LogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let lines = params.lines.unwrap_or(50).min(MAX_LOG_LINES);
        let minion_id = params.minion_id;

        // Look up the minion's worktree to find events.jsonl
        let events_path =
            minion_registry::with_registry(move |registry| match registry.get(&minion_id) {
                Some(info) => Ok(info.worktree.join("events.jsonl")),
                None => anyhow::bail!("Minion '{}' not found in registry", minion_id),
            })
            .await;

        let events_path = match events_path {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(
                    json!({"error": e.to_string()}).to_string(),
                )]));
            }
        };

        // Read events.jsonl (blocking I/O in spawn_blocking)
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            tail_lines(&events_path, lines)
        })
        .await;

        match result {
            Ok(Ok(content)) if content.is_empty() => {
                Ok(CallToolResult::success(vec![Content::text(
                    "No events recorded yet",
                )]))
            }
            Ok(Ok(content)) => Ok(CallToolResult::success(vec![Content::text(content)])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(
                json!({"error": e.to_string()}).to_string(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(
                json!({"error": format!("Task failed: {}", e)}).to_string(),
            )])),
        }
    }
}

#[tool_handler]
impl ServerHandler for GruMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("gru-mcp", env!("CARGO_PKG_VERSION")))
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "Gru MCP server — provides tools to query Minion status and logs, \
             plus resources with Gru usage guides and skill content."
                .to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new(GUIDE_URI, "Gru Guide".to_string())
                    .with_description("Usage guide for Gru — install, configure, troubleshoot, and understand Gru commands and concepts")
                    .with_mime_type("text/markdown")
                    .no_annotation(),
                RawResource::new(PM_SKILL_URI, "Product Manager Skill".to_string())
                    .with_description("Product management skill — shapes features, writes PRDs, evaluates designs against Gru's core principles")
                    .with_mime_type("text/markdown")
                    .no_annotation(),
                RawResource::new(TPM_SKILL_URI, "Project Manager Skill".to_string())
                    .with_description("Project management skill — understands issue dependencies, critical path, and helps prioritize work")
                    .with_mime_type("text/markdown")
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = &request.uri;
        match uri.as_str() {
            GUIDE_URI => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                GUIDE_CONTENT,
                uri.clone(),
            )])),
            PM_SKILL_URI => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                PM_SKILL_CONTENT,
                uri.clone(),
            )])),
            TPM_SKILL_URI => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                TPM_SKILL_CONTENT,
                uri.clone(),
            )])),
            _ => Err(McpError::resource_not_found(
                "resource_not_found",
                Some(serde_json::json!({ "uri": uri })),
            )),
        }
    }
}

/// Reads the last `n` lines from a file using a memory-efficient ring buffer.
///
/// Only keeps `n` lines in memory at a time rather than loading the entire file.
/// For a 100K-line file with n=50, memory is proportional to 50 lines, not 100K.
fn tail_lines(path: &std::path::Path, n: usize) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::collections::VecDeque;

    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    // Ring buffer: keep only the last `n` lines
    let mut ring: VecDeque<String> = VecDeque::with_capacity(n);
    for line_result in reader.lines() {
        let line = line_result?;
        if ring.len() == n {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    Ok(ring.into_iter().collect::<Vec<_>>().join("\n"))
}

/// Start the MCP server on stdio.
pub async fn run_server() -> anyhow::Result<()> {
    let server = GruMcpServer::new();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_content_not_empty() {
        assert!(
            !GUIDE_CONTENT.is_empty(),
            "Guide content should be embedded"
        );
        assert!(
            !PM_SKILL_CONTENT.is_empty(),
            "PM skill content should be embedded"
        );
        assert!(
            !TPM_SKILL_CONTENT.is_empty(),
            "TPM skill content should be embedded"
        );
    }

    #[test]
    fn test_minion_summary_serialization() {
        let summary = MinionSummary {
            id: "M001".to_string(),
            repo: "owner/repo".to_string(),
            issue: Some(42),
            command: "do".to_string(),
            branch: "minion/issue-42-M001".to_string(),
            mode: "autonomous".to_string(),
            phase: "RunningAgent".to_string(),
            pr: Some("123".to_string()),
            agent: "claude".to_string(),
            is_running: true,
            started_at: "2024-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("M001"));
        assert!(json.contains("owner/repo"));
    }

    #[test]
    fn test_tail_lines() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut std::fs::File::create(tmp.path()).unwrap(),
            b"line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();

        let result = tail_lines(tmp.path(), 3).unwrap();
        assert_eq!(result, "line3\nline4\nline5");

        let result = tail_lines(tmp.path(), 100).unwrap();
        assert_eq!(result, "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn test_tail_lines_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let result = tail_lines(tmp.path(), 10).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_tail_lines_missing_file() {
        let result = tail_lines(std::path::Path::new("/nonexistent/file"), 10);
        assert!(result.is_err());
    }

    #[test]
    fn test_server_info() {
        let server = GruMcpServer::new();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
    }
}
