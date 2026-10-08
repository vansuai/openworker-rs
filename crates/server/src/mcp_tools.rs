//! Register live MCP session tools into the engine ToolRegistry.
//!
//! Mirrors `coworker/mcp/tools.py` + `manager.prepare_mcp_tools`: each MCP tool
//! becomes `mcp__{server}__{tool}` with category `connector` (or `mcp` for raw servers).

use crate::mcp::McpServerDef;
use crate::mcp_oauth;
use crate::mcp_runtime::{McpRuntime, McpToolInfo};
use crate::state::SettingsManager;
use crate::mcp::McpStore;
use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_NAME_LEN: usize = 64;

/// Sanitized registry name: `mcp__{server}__{tool}` (OpenAI 64-char limit).
pub fn tool_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let base = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if base.len() > MAX_NAME_LEN {
        base[..MAX_NAME_LEN].to_string()
    } else {
        base
    }
}

fn filtered_tools(tools: &[McpToolInfo], server: &McpServerDef) -> Vec<McpToolInfo> {
    let mut out: Vec<McpToolInfo> = tools.to_vec();
    if let Some(allow) = &server.include_tools {
        let set: std::collections::HashSet<_> = allow.iter().cloned().collect();
        out.retain(|t| set.contains(&t.name));
    }
    if let Some(block) = &server.exclude_tools {
        let set: std::collections::HashSet<_> = block.iter().cloned().collect();
        out.retain(|t| !set.contains(&t.name));
    }
    out
}

fn merge_headers(server: &McpServerDef, extra: HashMap<String, String>) -> McpServerDef {
    let mut merged = server.clone();
    for (k, v) in extra {
        merged.headers.insert(k, v);
    }
    merged
}

fn mcp_tool_result(value: Value) -> ToolResult {
    // MCP tools/call returns `{ content: [...], isError?: bool }`.
    if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<String> = content
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return ToolResult::ok(json!(texts.join("\n")));
        }
    }
    ToolResult::ok(value)
}

/// Register MCP tools from all enabled, authed servers into `registry`.
///
/// Skips OAuth servers without stored tokens (never starts a browser from a turn).
/// Connector-backed servers use category `connector`; others use `mcp`.
pub async fn register_mcp_session_tools(
    registry: &mut ToolRegistry,
    mcp_store: &McpStore,
    mcp_runtime: &Arc<McpRuntime>,
    settings: &SettingsManager,
) {
    let servers: Vec<McpServerDef> = mcp_store
        .list()
        .iter()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?;
            let enabled = s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            if !enabled {
                return None;
            }
            mcp_store.get(name)
        })
        .collect();

    for server in servers {
        if server.auth.as_deref() == Some("oauth")
            && !mcp_oauth::has_tokens(settings, &server.name).await
        {
            continue;
        }
        let oauth_h = mcp_oauth::auth_headers(settings, &server.name).await;
        let merged = merge_headers(&server, oauth_h);
        let tools = match mcp_runtime.tools_for(&merged).await {
            Ok(t) => t,
            Err(_) => continue,
        };
        let backed = ocw_connectors::get_descriptor(&server.name)
            .map(|d| !d.mcp_url.is_empty())
            .unwrap_or(false);
        let category = if backed { "connector" } else { "mcp" };

        for tool in filtered_tools(&tools, &server) {
            let reg_name = tool_name(&server.name, &tool.name);
            let remote = tool.name.clone();
            let runtime = Arc::clone(mcp_runtime);
            let srv = merged.clone();
            let handle = tokio::runtime::Handle::current();

            let schema = ToolSchema::new(
                &reg_name,
                Some(&tool.description),
                tool.input_schema.clone().or(Some(json!({
                    "type": "object",
                    "properties": {},
                }))),
            );
            let spec = ToolSpec {
                risk_level: "medium",
                category,
                parallel_safe: false,
            };

            let f: ToolFn = Arc::new(move |args: Map<String, Value>| {
                let rt = Arc::clone(&runtime);
                let srv = srv.clone();
                let remote = remote.clone();
                let result = tokio::task::block_in_place(|| {
                    handle.block_on(async { rt.call_tool(&srv, &remote, Value::Object(args)).await })
                });
                match result {
                    Ok(v) => mcp_tool_result(v),
                    Err(e) => ToolResult::ok(json!({ "error": e })),
                }
            });
            registry.register(&reg_name, f, spec, Some(schema));
        }
    }
}
