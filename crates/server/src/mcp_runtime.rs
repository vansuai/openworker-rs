//! Minimal MCP client runtime — real stdio / HTTP JSON-RPC (no fake success).
//!
//! Framing matches the official Python MCP SDK (`mcp.client.stdio`): newline-delimited
//! JSON-RPC over stdin/stdout (not LSP Content-Length). Lifecycle mirrors
//! `coworker/mcp/client.py`: `initialize` → `notifications/initialized` → `tools/list`.

use crate::mcp::McpServerDef;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "openworker";
const CLIENT_VERSION: &str = "0.1.0";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cached tool discovery result for one MCP server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Option<Value>,
}

impl McpToolInfo {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "name": self.name,
            "description": self.description,
        });
        if let Some(schema) = &self.input_schema {
            obj.as_object_mut()
                .unwrap()
                .insert("inputSchema".into(), schema.clone());
        }
        obj
    }
}

/// In-memory MCP runtime: connect + list tools, cache by server name.
pub struct McpRuntime {
    cache: RwLock<HashMap<String, Vec<McpToolInfo>>>,
    next_id: AtomicU64,
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRuntime {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn cached_tools(&self, name: &str) -> Option<Vec<McpToolInfo>> {
        self.cache.read().await.get(name).cloned()
    }

    pub async fn invalidate(&self, name: &str) {
        self.cache.write().await.remove(name);
    }

    /// Connect (stdio spawn or HTTP), run initialize + tools/list, cache and return tools.
    pub async fn connect_and_list(&self, server: &McpServerDef) -> Result<Vec<McpToolInfo>, String> {
        let tools = match server.transport.as_str() {
            "stdio" => self.connect_stdio(server).await?,
            "http" => self.connect_http(server).await?,
            other => return Err(format!("unsupported MCP transport: {other}")),
        };
        self.cache
            .write()
            .await
            .insert(server.name.clone(), tools.clone());
        Ok(tools)
    }

    /// Return cached tools, or connect+list if missing.
    pub async fn tools_for(&self, server: &McpServerDef) -> Result<Vec<McpToolInfo>, String> {
        if let Some(cached) = self.cached_tools(&server.name).await {
            return Ok(cached);
        }
        self.connect_and_list(server).await
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn init_params() -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION,
            }
        })
    }

    // -- stdio ----------------------------------------------------------------

    async fn connect_stdio(&self, server: &McpServerDef) -> Result<Vec<McpToolInfo>, String> {
        let command = server
            .command
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                format!(
                    "MCP server '{}' is stdio but has no command",
                    server.name
                )
            })?;

        let mut cmd = Command::new(command);
        cmd.args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        if !server.env.is_empty() {
            // Inherit ambient env, then overlay configured vars (matches SDK behaviour).
            for (k, v) in &server.env {
                cmd.env(k, v);
            }
        }
        if let Some(cwd) = &server.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn MCP server '{command}': {e}"))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP process missing stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP process missing stdout".to_string())?;
        let mut reader = BufReader::new(stdout);

        let init_id = self.next_request_id();
        write_ndjson(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": init_id,
                "method": "initialize",
                "params": Self::init_params(),
            }),
        )
        .await?;

        let _init = read_response(&mut reader, init_id, DEFAULT_TIMEOUT).await?;

        // notifications/initialized — no response expected
        write_ndjson(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }),
        )
        .await?;

        let list_id = self.next_request_id();
        write_ndjson(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": list_id,
                "method": "tools/list",
            }),
        )
        .await?;

        let listed = read_response(&mut reader, list_id, DEFAULT_TIMEOUT).await?;
        let tools = parse_tools_result(&listed);

        // Best-effort shutdown: close stdin so the server can exit.
        drop(stdin);
        let _ = timeout(Duration::from_secs(2), child.wait()).await;

        Ok(tools)
    }

    // -- http -----------------------------------------------------------------

    async fn connect_http(&self, server: &McpServerDef) -> Result<Vec<McpToolInfo>, String> {
        if server.auth.as_deref() == Some("oauth") {
            return Err(
                "OAuth MCP connect is not yet available in the Rust server".into(),
            );
        }

        let url = server
            .url
            .as_deref()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                format!(
                    "MCP server '{}' is http but has no url",
                    server.name
                )
            })?;

        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| format!("http client error: {e}"))?;

        let mut session_id: Option<String> = None;

        let init_id = self.next_request_id();
        let init_result = http_rpc(
            &client,
            url,
            &server.headers,
            &mut session_id,
            init_id,
            "initialize",
            Some(Self::init_params()),
        )
        .await?;
        let _ = init_result;

        // Notification — ignore failures (spec allows servers to ignore/timeout).
        let _ = http_notify(
            &client,
            url,
            &server.headers,
            &session_id,
            "notifications/initialized",
        )
        .await;

        let list_id = self.next_request_id();
        let listed = http_rpc(
            &client,
            url,
            &server.headers,
            &mut session_id,
            list_id,
            "tools/list",
            None,
        )
        .await?;

        Ok(parse_tools_result(&listed))
    }
}

fn parse_tools_result(result: &Value) -> Vec<McpToolInfo> {
    result
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_schema = t.get("inputSchema").cloned();
                    Some(McpToolInfo {
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn write_ndjson(
    stdin: &mut tokio::process::ChildStdin,
    msg: &Value,
) -> Result<(), String> {
    let mut line = serde_json::to_string(msg).map_err(|e| format!("serialize: {e}"))?;
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("write stdin: {e}"))?;
    stdin.flush().await.map_err(|e| format!("flush stdin: {e}"))?;
    Ok(())
}

/// Read NDJSON lines until a JSON-RPC response matching `id` arrives.
async fn read_response(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
    deadline: Duration,
) -> Result<Value, String> {
    timeout(deadline, async {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("read stdout: {e}"))?;
            if n == 0 {
                return Err("MCP server closed stdout before responding".into());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // ignore non-JSON noise
            };
            // Skip notifications / requests from server; wait for our response.
            if msg.get("id").and_then(|v| v.as_u64()) != Some(id)
                && msg.get("id").and_then(|v| v.as_i64()) != Some(id as i64)
            {
                continue;
            }
            if let Some(err) = msg.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("MCP error");
                return Err(message.to_string());
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for MCP response id={id}"))?
}

fn mcp_http_headers(
    extra: &HashMap<String, String>,
    session_id: Option<&str>,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(sid) = session_id {
        if let Ok(v) = HeaderValue::from_str(sid) {
            headers.insert("Mcp-Session-Id", v);
        }
    }
    for (k, v) in extra {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            headers.insert(name, val);
        }
    }
    headers
}

async fn http_rpc(
    client: &reqwest::Client,
    url: &str,
    extra_headers: &HashMap<String, String>,
    session_id: &mut Option<String>,
    id: u64,
    method: &str,
    params: Option<Value>,
) -> Result<Value, String> {
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    });
    if let Some(p) = params {
        body.as_object_mut().unwrap().insert("params".into(), p);
    }

    let headers = mcp_http_headers(extra_headers, session_id.as_deref());
    let resp = client
        .post(url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if let Some(sid) = resp.headers().get("mcp-session-id") {
        if let Ok(s) = sid.to_str() {
            *session_id = Some(s.to_string());
        }
    }

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if content_type.contains("text/event-stream") {
        let text = resp
            .text()
            .await
            .map_err(|e| format!("read SSE body: {e}"))?;
        return parse_sse_result(&text, id);
    }

    let msg: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON response: {e}"))?;

    if let Some(err) = msg.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("MCP error");
        return Err(message.to_string());
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

async fn http_notify(
    client: &reqwest::Client,
    url: &str,
    extra_headers: &HashMap<String, String>,
    session_id: &Option<String>,
    method: &str,
) -> Result<(), String> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
    });
    let headers = mcp_http_headers(extra_headers, session_id.as_deref());
    let _ = client
        .post(url)
        .headers(headers)
        .json(&body)
        .send()
        .await;
    Ok(())
}

fn parse_sse_result(body: &str, id: u64) -> Result<Value, String> {
    for line in body.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if msg.get("id").and_then(|v| v.as_u64()) != Some(id)
            && msg.get("id").and_then(|v| v.as_i64()) != Some(id as i64)
        {
            continue;
        }
        if let Some(err) = msg.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("MCP error");
            return Err(message.to_string());
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
    Err("SSE stream ended without a matching JSON-RPC response".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Tiny NDJSON MCP mock: initialize + tools/list over stdio.
    #[tokio::test]
    async fn stdio_connect_lists_tools() {
        let server = McpServerDef {
            name: "mock".into(),
            transport: "stdio".into(),
            command: Some("python3".into()),
            args: vec![
                "-c".into(),
                r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0"}}} )
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"Echo tool","inputSchema":{"type":"object"}}]}})
        break
"#
                .into(),
            ],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
        };

        let runtime = McpRuntime::new();
        let tools = runtime.connect_and_list(&server).await.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo tool");

        let cached = runtime.cached_tools("mock").await.unwrap();
        assert_eq!(cached[0].name, "echo");
    }

    #[tokio::test]
    async fn http_oauth_returns_clear_error() {
        let server = McpServerDef {
            name: "oauth-svc".into(),
            transport: "http".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: Some("https://example.com/mcp".into()),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: Some("oauth".into()),
        };
        let runtime = McpRuntime::new();
        let err = runtime.connect_and_list(&server).await.unwrap_err();
        assert!(err.contains("OAuth"), "err={err}");
    }

    #[tokio::test]
    async fn http_missing_url_errors() {
        let server = McpServerDef {
            name: "no-url".into(),
            transport: "http".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
        };
        let runtime = McpRuntime::new();
        let err = runtime.connect_and_list(&server).await.unwrap_err();
        assert!(err.contains("no url"), "err={err}");
    }
}
