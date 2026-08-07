//! Shared helpers for the integration tools (GitHub/Gmail/GCal/HubSpot/etc.).
//!
//! Mirrors the helpers in `coworker/connectors/integration_tools.py`:
//! `_request`, `_github_headers`, `_github_base`, `_google_headers`, etc.
//! Re-implemented in Rust against `reqwest::blocking` — the tool callbacks run
//! in a Tokio worker thread (not on the async runtime directly), so blocking
//! HTTP is fine here and avoids the cost of pinning a per-call runtime.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolves connector credentials by key.
///
/// The Python equivalent (`SecretStore.get`) returns a dict like
/// `{"bot_token": "xoxb-…", "team_id": "T…"}`. The Rust impl is a closure
/// injected by the server when registering the tools — typically wraps
/// `SettingsManager::get_provider_config`. Returning `None` means "not
/// configured", which surfaces as an actionable error to the model.
pub type SecretResolver = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;

/// Resolves the per-session workspace root — many tools (`github_clone`,
/// `dropbox_*`, etc.) need to know where they're allowed to write.
pub type WorkspaceRoot = Arc<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>;

/// Per-tool execution context. Held inside `Arc` by the registered tools.
#[derive(Clone)]
pub struct IntegrationContext {
    pub secrets: SecretResolver,
    pub workspace: WorkspaceRoot,
}

impl IntegrationContext {
    pub fn new(secrets: SecretResolver, workspace: WorkspaceRoot) -> Self {
        Self { secrets, workspace }
    }

    /// Look up a sub-field of a stored profile. Returns `None` if either the
    /// profile is missing or the sub-field is missing.
    pub fn secret_str(&self, key: &str, field: &str) -> Option<String> {
        (self.secrets)(key)
            .and_then(|v| v.get(field).and_then(|f| f.as_str()).map(String::from))
    }

    pub fn secret_value(&self, key: &str) -> Option<Value> {
        (self.secrets)(key)
    }
}

/// Common block-on HTTP client (30 s timeout). Built fresh per call so the
/// tool runs don't share a client across Tokio worker threads.
fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent("OpenWorker/1.0 (integration)")
        .build()
        .map_err(|e| e.to_string())
}

pub(crate) fn client() -> Result<reqwest::blocking::Client, String> {
    blocking_client(TIMEOUT)
}

/// Longer-timeout client for uploads / large downloads. Reserved for future
/// tools (drive/dropbox file transfers); kept alive to avoid a rebuild when
/// those land.
#[allow(dead_code)]
pub(crate) fn file_client() -> Result<reqwest::blocking::Client, String> {
    blocking_client(FILE_TIMEOUT)
}

/// JSON request helper used by every provider.
pub(crate) fn request_json(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> Result<Value, String> {
    let client = client()?;
    let mut req = match method.to_ascii_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        other => return Err(format!("unsupported HTTP method: {other}")),
    };
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", truncate(&text, 200)));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("decode failed: {e} (body: {})", truncate(&text, 200)))
}

/// Convenience for `Authorization: Bearer …`.
#[allow(dead_code)]
pub(crate) fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {token}"))]
}

/// Truncate for error messages.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(max);
        format!("{}…", &s[..cut])
    }
}

/// Build a ToolSchema with the standard function shape.
pub(crate) fn schema(name: &str, description: &str, params: Value) -> ocw_engine::ToolSchema {
    ocw_engine::ToolSchema::new(name, Some(description), Some(params))
}

/// Small helper to pull an argument from the tool args map. Always returns
/// `""` (not an error) so callers can use it inline.
pub(crate) fn arg_string<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

pub(crate) fn arg_str_opt<'a>(args: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

pub(crate) fn arg_i64(args: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

/// Reserved for future tools with boolean switches.
#[allow(dead_code)]
pub(crate) fn arg_bool(args: &serde_json::Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Wrap a string error in the standard `{"error": ...}` shape used by every
/// Python tool, so the model sees the same shape whether it called a Rust or
/// Python tool.
pub(crate) fn err(message: impl Into<String>) -> Value {
    serde_json::json!({ "error": message.into() })
}

/// Wrap an "ok" payload (anything serialisable).
pub(crate) fn ok<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Clamp helper used across several integration tools (search result caps).
pub(crate) fn clamp(value: Option<i64>, default: i64, ceiling: i64) -> i64 {
    match value {
        Some(n) if n > 0 => n.min(ceiling),
        _ => default,
    }
}
