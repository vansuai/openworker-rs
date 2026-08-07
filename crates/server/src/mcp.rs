//! MCP server config store — mirrors `coworker/mcp/config.py`.
//!
//! Reads/writes the standard `mcp.json` file (`mcpServers` format compatible with
//! Claude Desktop / Cursor). Global config lives at `<data_dir>/mcp.json`.
//! Workspace-level overrides in `<workspace>/.coworker/mcp.json` are merged at
//! read time (workspace wins on name clash).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    pub transport: String, // "stdio" | "http"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_tools: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub requires_approval: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

fn default_true() -> bool {
    true
}

const HTTP_TYPES: &[&str] = &["http", "https", "sse", "streamable-http", "streamable_http"];

// ---------------------------------------------------------------------------
// McpStore
// ---------------------------------------------------------------------------

pub struct McpStore {
    global_path: PathBuf,
    cached: RwLock<Vec<McpServerDef>>,
}

impl McpStore {
    pub fn new(data_dir: &Path) -> Self {
        let store = Self {
            global_path: data_dir.join("mcp.json"),
            cached: RwLock::new(Vec::new()),
        };
        store.reload();
        store
    }

    // -- I/O ------------------------------------------------------------------

    fn global_path(&self) -> &Path {
        &self.global_path
    }

    fn read_file(path: &Path) -> HashMap<String, Value> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let v: Value = serde_json::from_str(&text).unwrap_or_default();
                v.get("mcpServers")
                    .and_then(|s| s.as_object())
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default()
            }
            Err(_) => HashMap::new(),
        }
    }

    fn write_global(&self, servers: &HashMap<String, Value>) -> Result<(), String> {
        let path = self.global_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content = json!({"mcpServers": servers});
        let text =
            serde_json::to_string_pretty(&content).map_err(|e| format!("serialize error: {e}"))?;
        // Atomic write
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &text).map_err(|e| format!("write error: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename error: {e}"))?;
        Ok(())
    }

    // -- parsing --------------------------------------------------------------

    fn parse_server(name: &str, raw: &Value) -> McpServerDef {
        let obj = raw.as_object();
        let declared = obj
            .and_then(|o| o.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let has_url = obj
            .and_then(|o| o.get("url"))
            .and_then(|v| v.as_str())
            .is_some();
        let is_http = HTTP_TYPES.contains(&declared.as_str()) || has_url;

        McpServerDef {
            name: name.to_string(),
            transport: if is_http {
                "http".into()
            } else {
                "stdio".into()
            },
            command: obj
                .and_then(|o| o.get("command"))
                .and_then(|v| v.as_str())
                .map(String::from),
            args: obj
                .and_then(|o| o.get("args"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            env: obj
                .and_then(|o| o.get("env"))
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            cwd: obj
                .and_then(|o| o.get("cwd"))
                .and_then(|v| v.as_str())
                .map(String::from),
            url: obj
                .and_then(|o| o.get("url"))
                .and_then(|v| v.as_str())
                .map(String::from),
            headers: obj
                .and_then(|o| o.get("headers"))
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            enabled: obj
                .and_then(|o| o.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            include_tools: obj
                .and_then(|o| o.get("include_tools"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                }),
            exclude_tools: obj
                .and_then(|o| o.get("exclude_tools"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                }),
            requires_approval: obj
                .and_then(|o| o.get("requires_approval"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            auth: obj
                .and_then(|o| o.get("auth"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase()),
        }
    }

    // -- queries --------------------------------------------------------------

    pub fn reload(&self) {
        let servers = Self::read_file(self.global_path());
        // For now, just global config. Workspace merges can be added later.
        let defs: Vec<McpServerDef> = servers
            .iter()
            .map(|(name, raw)| Self::parse_server(name, raw))
            .collect();
        *self.cached.write().unwrap() = defs;
    }

    pub fn list(&self) -> Vec<Value> {
        self.cached
            .read()
            .unwrap()
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "transport": s.transport,
                    "command": s.command,
                    "args": s.args,
                    "url": s.url,
                    "enabled": s.enabled,
                    "requires_approval": s.requires_approval,
                    "auth": s.auth,
                })
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<McpServerDef> {
        self.cached
            .read()
            .unwrap()
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    pub fn read_global_raw(&self) -> HashMap<String, Value> {
        Self::read_file(self.global_path())
    }

    // -- mutations (write to global file) -------------------------------------

    pub fn create(&self, name: &str, config: Value) -> Result<(), String> {
        let mut servers = self.read_global_raw();
        servers.insert(name.to_string(), config.clone());
        self.write_global(&servers)?;
        self.reload();
        Ok(())
    }

    pub fn update(&self, name: &str, changes: Value) -> Result<(), String> {
        let mut servers = self.read_global_raw();
        let existing = servers
            .get(name)
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        if let (Value::Object(mut base), Value::Object(overlay)) = (existing, changes) {
            for (k, v) in overlay {
                base.insert(k, v);
            }
            servers.insert(name.to_string(), Value::Object(base));
            self.write_global(&servers)?;
            self.reload();
            Ok(())
        } else {
            Err("invalid config format".into())
        }
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let mut servers = self.read_global_raw();
        if !servers.contains_key(name) {
            return Err(format!("server '{name}' not found"));
        }
        servers.remove(name);
        self.write_global(&servers)?;
        self.reload();
        Ok(())
    }
}
