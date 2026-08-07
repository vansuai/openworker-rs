//! Shared server state.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use tokio::sync::broadcast;

use crate::automations::AutomationStore;
use crate::connectors::ConnectorStore;
use crate::mcp::McpStore;
use crate::mcp_runtime::McpRuntime;
use crate::personas::PersonaStore;
use crate::stores::{
    AuditStore, BrowserController, ChannelBuffer, PersonaConnectionStore, SessionConnectionStore,
    SubscriptionStore, UnattendedStore, UnroutedStore,
};
use ocw_data::ConversationStore;
use ocw_data::InboxRouting;
use ocw_data::InboxStore;
use ocw_data::MemoryStore;
use ocw_provider::Provider;
use ocw_provider::{self, model_context_windows, model_labels, models_for_provider};
use ocw_shell::LocalExecutor;
use ocw_skills::{SessionSkillStore, SkillStore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::RwLock as TokioRwLock;

pub type Shared<T> = Arc<TokioRwLock<T>>;
/// Standard library RwLock — safe to block on from async contexts.
pub type StdShared<T> = Arc<StdRwLock<T>>;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub api_token: String,
    pub data_dir: PathBuf,
    pub default_model: String,
    pub host: String,
    pub port: u16,
    /// OpenWorker Cloud base URL (managed OAuth broker); `None` falls back
    /// to the public cloud default.
    pub cloud_url: Option<String>,
    /// Permission mode: "interactive" | "discuss" | "plan" | "custom".
    pub mode: String,
    /// Max turns per session run.
    pub max_iterations: u32,
    /// Command prefixes auto-run without an approval prompt (global-only).
    pub allowed_commands: Vec<String>,
    /// Tools auto-approved in "custom" permission mode (global-only).
    pub auto_allow: Vec<String>,
    /// Web search provider: "duckduckgo" (keyless default) | "tavily" | "brave".
    pub web_search_provider: String,
    /// Auth0 tenant domain for cloud sign-in.
    pub cloud_auth_domain: String,
    /// Auth0 client id for cloud sign-in.
    pub cloud_client_id: String,
    /// Auth0 API audience for cloud sign-in.
    pub cloud_audience: String,
    /// Managed relay WebSocket endpoint (Slack/GitHub inbound); empty disables.
    pub cloud_relay_ws_url: String,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = std::env::var("COWORKER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                if cfg!(target_os = "windows") {
                    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("coworker")
                } else {
                    // macOS/Linux: use XDG_CONFIG_HOME or $HOME/.config to match the Python version.
                    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
                    std::env::var("XDG_CONFIG_HOME")
                        .map(PathBuf::from)
                        .ok()
                        .unwrap_or_else(|| home.join(".config"))
                        .join("coworker")
                }
            });
        Self {
            api_token: std::env::var("COWORKER_API_TOKEN").unwrap_or_default(),
            data_dir,
            default_model: std::env::var("COWORKER_MODEL").unwrap_or_default(),
            host: std::env::var("COWORKER_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("COWORKER_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8765),
            cloud_url: std::env::var("COWORKER_CLOUD_URL").ok(),
            mode: "interactive".into(),
            max_iterations: 150,
            allowed_commands: Vec::new(),
            auto_allow: Vec::new(),
            web_search_provider: "duckduckgo".into(),
            cloud_auth_domain: std::env::var("COWORKER_CLOUD_AUTH_DOMAIN")
                .unwrap_or_else(|_| "opencoworker.us.auth0.com".into()),
            cloud_client_id: std::env::var("COWORKER_CLOUD_CLIENT_ID")
                .unwrap_or_else(|_| "g1l4Q1lhYWmyS03qPSf4KEJGrgq02Qam".into()),
            cloud_audience: std::env::var("COWORKER_CLOUD_AUDIENCE")
                .unwrap_or_else(|_| "https://api.opencoworker.app".into()),
            cloud_relay_ws_url: std::env::var("COWORKER_CLOUD_RELAY_WS_URL").unwrap_or_else(|_| {
                "wss://l4z1paxb83.execute-api.us-east-1.amazonaws.com/ocw-connect".into()
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Prefs (persistent user preferences)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Prefs {
    default_model: String,
    onboarded: bool,
    hidden_models: Vec<String>,
    custom_models: Vec<String>,
    show_chat: bool,
    show_code: bool,
    nav_layout: String,
    scratch_base: Option<String>,
    sessions_peek: Option<u32>,
    pdf_fallback: String,
    pdf_max_pages: u32,
    pdf_max_mb: u32,
    /// The session a DM to the bot is routed to (user-designated); `None` → DMs park as unrouted.
    dm_session: Option<String>,
}

impl Prefs {
    fn load(path: &PathBuf) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let obj = value.as_object().cloned().unwrap_or_default();
        let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
        let get_bool = |k: &str| obj.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let get_u32 = |k: &str| obj.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
        let get_arr = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            default_model: get_str("default_model").unwrap_or_default(),
            onboarded: get_bool("onboarded"),
            hidden_models: get_arr("hidden_models"),
            custom_models: get_arr("models"),
            show_chat: get_bool("show_chat"),
            show_code: get_bool("show_code"),
            nav_layout: get_str("nav_layout").unwrap_or_else(|| "flat".into()),
            scratch_base: get_str("scratch_base"),
            sessions_peek: get_u32("sessions_peek"),
            pdf_fallback: get_str("pdf_fallback").unwrap_or_else(|| "text".into()),
            pdf_max_pages: get_u32("pdf_max_pages").unwrap_or(20),
            pdf_max_mb: get_u32("pdf_max_mb").unwrap_or(10),
            dm_session: get_str("dm_session"),
        }
    }

    fn save(&self, path: &PathBuf) {
        let mut obj = Map::new();
        obj.insert(
            "default_model".into(),
            serde_json::json!(self.default_model),
        );
        obj.insert("onboarded".into(), serde_json::json!(self.onboarded));
        if !self.hidden_models.is_empty() {
            obj.insert(
                "hidden_models".into(),
                serde_json::json!(self.hidden_models),
            );
        }
        if !self.custom_models.is_empty() {
            obj.insert("models".into(), serde_json::json!(self.custom_models));
        }
        if self.show_chat {
            obj.insert("show_chat".into(), serde_json::json!(true));
        }
        if self.show_code {
            obj.insert("show_code".into(), serde_json::json!(true));
        }
        if self.nav_layout != "flat" {
            obj.insert("nav_layout".into(), serde_json::json!(self.nav_layout));
        }
        if let Some(ref sb) = self.scratch_base {
            obj.insert("scratch_base".into(), serde_json::json!(sb));
        }
        if let Some(sp) = self.sessions_peek {
            obj.insert("sessions_peek".into(), serde_json::json!(sp));
        }
        obj.insert("pdf_fallback".into(), serde_json::json!(self.pdf_fallback));
        obj.insert(
            "pdf_max_pages".into(),
            serde_json::json!(self.pdf_max_pages),
        );
        obj.insert("pdf_max_mb".into(), serde_json::json!(self.pdf_max_mb));
        if let Some(ref dm) = self.dm_session {
            obj.insert("dm_session".into(), serde_json::json!(dm));
        }
        let text = serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    }
}

// ---------------------------------------------------------------------------
// Provider secrets store
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SecretsStore {
    providers: Map<String, Value>,
}

impl SecretsStore {
    fn load(path: &PathBuf) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let obj = value.as_object().cloned().unwrap_or_default();
        Self { providers: obj }
    }

    fn save(&self, path: &PathBuf) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = serde_json::to_string_pretty(
            &serde_json::to_value(&self.providers).unwrap_or(Value::Null),
        )
        .unwrap_or_default();
        let _ = std::fs::write(path, text);
    }
}

// ---------------------------------------------------------------------------
// Secret `${VAR}` reference resolution (mirror of `coworker/secrets.py`)
// ---------------------------------------------------------------------------

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expand `${VAR}` references from `env` (process env + local `.env`), leaving
/// unresolved references untouched — mirror of `SecretStore.resolve`.
fn expand_env_refs(text: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = text[i + 2..].find('}') {
                let name = &text[i + 2..i + 2 + close];
                if is_valid_env_name(name) {
                    if let Some(v) = env.get(name) {
                        out.push_str(v);
                        i += 2 + close + 1;
                        continue;
                    }
                }
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Recursively resolve `${VAR}` refs in a secret value.
fn resolve_secret_value(value: &Value, env: &HashMap<String, String>) -> Value {
    match value {
        Value::String(s) => Value::String(expand_env_refs(s, env)),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, v) in o {
                out.insert(k.clone(), resolve_secret_value(v, env));
            }
            Value::Object(out)
        }
        Value::Array(a) => {
            Value::Array(a.iter().map(|v| resolve_secret_value(v, env)).collect())
        }
        other => other.clone(),
    }
}

/// Parse a `KEY=VALUE` dotenv file into a map (mirror of `_load_dotenv`).
fn load_dotenv(path: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return env;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(idx) = line.find('=') {
            let key = line[..idx].trim().to_string();
            let val = line[idx + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            env.insert(key, val);
        }
    }
    env
}

// ---------------------------------------------------------------------------
// SettingsManager
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SettingsManager {
    prefs_path: PathBuf,
    prefs: Arc<tokio::sync::RwLock<Prefs>>,
    secrets_path: PathBuf,
    dotenv_path: PathBuf,
    secrets: Arc<tokio::sync::RwLock<SecretsStore>>,
}

impl SettingsManager {
    fn new(data_dir: PathBuf) -> Self {
        let prefs_path = data_dir.join("prefs.json");
        let secrets_path = data_dir.join("secrets.json");
        let dotenv_path = data_dir.join(".env");
        let mut prefs = Prefs::load(&prefs_path);
        let secrets = SecretsStore::load(&secrets_path);

        // If prefs.default_model's provider has no API key configured, clear it
        // so get_settings() will fall back to the first configured provider.
        if !prefs.default_model.is_empty() {
            let provider: &str = prefs.default_model.split(':').next().unwrap_or("");
            if provider != "ollama" {
                let env_ok = match provider {
                    "openai" => std::env::var("OPENAI_API_KEY").is_ok(),
                    "anthropic" => std::env::var("ANTHROPIC_API_KEY").is_ok(),
                    "gemini" => std::env::var("GEMINI_API_KEY").is_ok(),
                    "deepseek" => std::env::var("DEEPSEEK_API_KEY").is_ok(),
                    _ => false,
                };
                let store_ok = secrets
                    .providers
                    .get(&format!("provider:{provider}"))
                    .and_then(|v| v.get("api_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !env_ok && !store_ok {
                    prefs.default_model = String::new();
                    prefs.save(&prefs_path);
                }
            }
        }

        Self {
            prefs_path,
            prefs: Arc::new(tokio::sync::RwLock::new(prefs)),
            secrets_path,
            dotenv_path,
            secrets: Arc::new(tokio::sync::RwLock::new(secrets)),
        }
    }

    /// Open a fresh settings store rooted at `data_dir` (test helper; the
    /// real server uses the private `new`).
    #[cfg(test)]
    pub fn open_for_tests(data_dir: PathBuf) -> Self {
        Self::new(data_dir)
    }

    // --- prefs helpers ---

    pub async fn get_default_model(&self) -> String {
        self.prefs.read().await.default_model.clone()
    }

    pub async fn set_default_model(&self, model: String) {
        let mut p = self.prefs.write().await;
        p.default_model = model.clone();
        p.save(&self.prefs_path);
    }

    pub async fn is_onboarded(&self) -> bool {
        self.prefs.read().await.onboarded
    }

    pub async fn set_onboarded(&self, value: bool) {
        let mut p = self.prefs.write().await;
        p.onboarded = value;
        p.save(&self.prefs_path);
    }

    pub async fn set_scratch_base(&self, path: String) {
        let mut p = self.prefs.write().await;
        p.scratch_base = Some(path);
        p.save(&self.prefs_path);
    }

    pub async fn set_sessions_peek(&self, n: u32) {
        let mut p = self.prefs.write().await;
        p.sessions_peek = Some(n);
        p.save(&self.prefs_path);
    }

    pub async fn set_pdf_settings(&self, fallback: String, max_pages: u32, max_mb: u32) {
        let mut p = self.prefs.write().await;
        p.pdf_fallback = fallback;
        p.pdf_max_pages = max_pages;
        p.pdf_max_mb = max_mb;
        p.save(&self.prefs_path);
    }

    pub async fn set_nav_layout(&self, layout: String) {
        let mut p = self.prefs.write().await;
        p.nav_layout = layout;
        p.save(&self.prefs_path);
    }

    pub async fn set_surfaces(&self, chat: bool, code: bool) {
        let mut p = self.prefs.write().await;
        p.show_chat = chat;
        p.show_code = code;
        p.save(&self.prefs_path);
    }

    /// The session a DM to the bot is routed to (user-designated).
    /// `None` → DMs are parked (as unrouted).
    pub async fn get_dm_session(&self) -> Option<String> {
        self.prefs.read().await.dm_session.clone()
    }

    /// Designate (or clear, with an empty id) the session that handles incoming DMs.
    pub async fn set_dm_session(&self, session_id: Option<&str>) {
        let mut p = self.prefs.write().await;
        let sid = session_id.map(|s| s.trim().to_string()).unwrap_or_default();
        if sid.is_empty() {
            p.dm_session = None;
        } else {
            p.dm_session = Some(sid);
        }
        p.save(&self.prefs_path);
    }

    pub async fn add_model(&self, model: String) -> bool {
        let mut p = self.prefs.write().await;
        if !p.custom_models.contains(&model) {
            p.custom_models.push(model.clone());
        }
        p.hidden_models.retain(|m| m != &model);
        p.save(&self.prefs_path);
        true
    }

    pub async fn remove_model(&self, model: &str) {
        let mut p = self.prefs.write().await;
        p.custom_models.retain(|m| m != model);
        if ocw_provider::MATRIX.iter().any(|(k, _)| *k == model)
            && !p.hidden_models.contains(&model.to_string())
        {
            p.hidden_models.push(model.to_string());
        }
        p.save(&self.prefs_path);
    }

    // --- secrets helpers ---

    pub async fn get_provider_config(&self, name: &str) -> Option<Map<String, Value>> {
        self.secrets
            .read()
            .await
            .providers
            .get(&format!("provider:{name}"))
            .and_then(|v| v.as_object().cloned())
    }

    pub async fn set_provider(&self, name: &str, fields: Map<String, Value>) {
        self.secrets
            .write()
            .await
            .providers
            .insert(format!("provider:{name}"), Value::Object(fields));
        let secrets = self.secrets.read().await;
        secrets.save(&self.secrets_path);
    }

    pub async fn delete_provider(&self, name: &str) {
        self.secrets
            .write()
            .await
            .providers
            .remove(&format!("provider:{name}"));
        let secrets = self.secrets.read().await;
        secrets.save(&self.secrets_path);
    }

    pub async fn has_api_key_async(&self) -> bool {
        let key = std::env::var("OPENAI_API_KEY").is_ok();
        if key {
            return true;
        }
        self.secrets
            .read()
            .await
            .providers
            .get("provider:openai")
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    /// Return a clone of all stored secrets (keyed "provider:{name}") so the
    /// [`Router`](ocw_provider::Router) can inject API keys into model calls.
    /// Synchronous variant — uses `try_read()` to avoid any risk of blocking
    /// the Tokio runtime thread.
    pub fn secrets_providers(&self) -> HashMap<String, serde_json::Value> {
        let env = self.secret_env();
        self.secrets
            .try_read()
            .map(|s| {
                s.providers
                    .iter()
                    .map(|(k, v)| {
                        (k.clone(), resolve_secret_value(v, &env))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Async variant for use inside async handlers.
    pub async fn secrets_providers_async(&self) -> HashMap<String, serde_json::Value> {
        let env = self.secret_env();
        self.secrets
            .read()
            .await
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), resolve_secret_value(v, &env)))
            .collect()
    }

    /// The resolution environment: the local `.env` overlaid with the process
    /// env (process env wins, matching the Python lookup order).
    fn secret_env(&self) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = load_dotenv(&self.dotenv_path);
        env.extend(std::env::vars());
        env
    }

    pub async fn secrets_has_key(&self, provider: &str) -> bool {
        let key = match provider {
            "openai" => std::env::var("OPENAI_API_KEY").is_ok(),
            "anthropic" => std::env::var("ANTHROPIC_API_KEY").is_ok(),
            "gemini" => std::env::var("GEMINI_API_KEY").is_ok(),
            "deepseek" => std::env::var("DEEPSEEK_API_KEY").is_ok(),
            _ => false,
        };
        if key {
            return true;
        }
        self.secrets
            .read()
            .await
            .providers
            .get(&format!("provider:{provider}"))
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    pub fn secrets_path(&self) -> PathBuf {
        self.secrets_path.clone()
    }

    /// Read a value from the secrets store by key (e.g. "experimental:settings").
    pub async fn secrets_get(&self, key: &str) -> Option<Map<String, Value>> {
        let raw = self
            .secrets
            .read()
            .await
            .providers
            .get(key)
            .and_then(|v| v.as_object().cloned());
        raw.map(|m| {
            resolve_secret_value(&Value::Object(m), &self.secret_env())
                .as_object()
                .cloned()
                .unwrap_or_default()
        })
    }

    /// Write a value to the secrets store under the given key.
    pub async fn secrets_put(&self, key: &str, value: Map<String, Value>) {
        self.secrets
            .write()
            .await
            .providers
            .insert(key.to_string(), Value::Object(value));
        let secrets = self.secrets.read().await;
        secrets.save(&self.secrets_path);
    }

    /// Delete a key from the secrets store. Returns true when the key existed.
    pub async fn secrets_delete(&self, key: &str) -> bool {
        let removed = self
            .secrets
            .write()
            .await
            .providers
            .remove(key)
            .is_some();
        if removed {
            let secrets = self.secrets.read().await;
            secrets.save(&self.secrets_path);
        }
        removed
    }

    /// Snapshot of every secrets entry (key -> object), e.g. to enumerate
    /// account profiles with a shared prefix.
    pub async fn secrets_all(&self) -> HashMap<String, Map<String, Value>> {
        let env = self.secret_env();
        self.secrets
            .read()
            .await
            .providers
            .iter()
            .filter_map(|(k, v)| {
                v.as_object().cloned().map(|o| {
                    let resolved = resolve_secret_value(&Value::Object(o), &env);
                    (
                        k.clone(),
                        resolved.as_object().cloned().unwrap_or_default(),
                    )
                })
            })
            .collect()
    }

    /// Whether experimental connectors are enabled.
    pub async fn experimental_connectors_enabled(&self) -> bool {
        self.secrets_get("experimental:settings")
            .await
            .and_then(|m| m.get("enabled").and_then(|v| v.as_bool()))
            .unwrap_or(false)
    }

    /// Enable or disable experimental connectors.
    pub async fn set_experimental_connectors(&self, value: bool) {
        let mut m = Map::new();
        m.insert("enabled".into(), Value::Bool(value));
        self.secrets_put("experimental:settings", m).await;
    }

    // --- full settings payload (matches ModelSettings TS interface) ---

    pub async fn get_settings(&self) -> Value {
        let prefs = self.prefs.read().await;
        let secrets = self.secrets.read().await;

        // Default model: use prefs value if set, otherwise the first configured provider's recommended model.
        let default_model = if !prefs.default_model.is_empty() {
            prefs.default_model.clone()
        } else {
            // Find first configured provider, preferring the one with a stored key
            let descriptors: Vec<_> = ocw_provider::all_descriptors();
            let first_configured = descriptors.iter().find(|d| {
                let env_ok = d
                    .env_key
                    .as_ref()
                    .map(|k| env::var(k).is_ok())
                    .unwrap_or(false);
                let store_ok = secrets
                    .providers
                    .get(&format!("provider:{}", d.name))
                    .and_then(|v| v.get("api_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                env_ok || store_ok
            });
            first_configured
                .and_then(|d| d.recommended_model.clone())
                .unwrap_or_else(|| "deepseek:deepseek-v4-flash".into())
        };

        // Determine source and has_key for the effective default model
        let model_provider = self._model_provider(&default_model);
        let source = {
            if ocw_provider::get_descriptor(&model_provider)
                .and_then(|d| d.env_key.as_ref())
                .map(|k| env::var(k).is_ok())
                .unwrap_or(false)
            {
                Some("env")
            } else if secrets
                .providers
                .get(&format!("provider:{model_provider}"))
                .and_then(|v| v.get("api_key"))
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            {
                Some("store")
            } else {
                None
            }
        };

        let has_key = source.is_some();

        // Build selectable models list:
        // 1. Always include the current default model (so it can be used even before a key is set)
        // 2. Include curated matrix models whose provider is configured
        // 3. Include all custom models
        let hidden: HashMap<String, ()> = prefs
            .hidden_models
            .iter()
            .map(|k| (k.clone(), ()))
            .collect();

        let mut selectable = Vec::new();
        // Always include the default model
        if !hidden.contains_key(&default_model) {
            selectable.push(default_model.clone());
        }
        // Add matrix models whose provider is configured (skip the default model which we already added)
        for (k, _) in ocw_provider::MATRIX.iter() {
            if !hidden.contains_key(*k) && *k != default_model {
                let provider = self._model_provider(k);
                if provider == "ollama" || self.secrets_has_key_async_sync(&secrets, &provider) {
                    selectable.push((*k).to_string());
                }
            }
        }
        // Add custom models (skip ones already in the list)
        let selectable_set: HashMap<String, ()> =
            selectable.iter().map(|m| (m.clone(), ())).collect();
        for m in &prefs.custom_models {
            if !hidden.contains_key(m) && !selectable_set.contains_key(m) {
                selectable.push(m.clone());
            }
        }

        let model_labels = model_labels();
        let model_context_windows = model_context_windows();
        let surfaces = serde_json::json!({
            "cowork": true,
            "chat": prefs.show_chat,
            "code": prefs.show_code,
        });

        drop(secrets);

        serde_json::json!({
            "provider": model_provider,
            "model": default_model,
            "models": selectable,
            "model_labels": model_labels,
            "model_context_windows": model_context_windows,
            "has_key": has_key,
            "model_ready": self.secrets_has_key(&model_provider).await,
            "source": source,
            "onboarded": prefs.onboarded,
            "experimental_connectors": self.experimental_connectors_enabled().await,
            "surfaces": surfaces,
            "nav_layout": if prefs.nav_layout == "grouped" { "grouped" } else { "flat" },
            "sessions_peek": prefs.sessions_peek,
            "scratch_base": prefs.scratch_base.clone().unwrap_or_else(|| {
                dirs::home_dir().map(|p| p.join("OpenWorker").to_string_lossy().to_string()).unwrap_or_default()
            }),
            "secrets_path": self.secrets_path.to_string_lossy(),
            "pdf_fallback": prefs.pdf_fallback,
            "pdf_max_pages": prefs.pdf_max_pages,
            "pdf_max_mb": prefs.pdf_max_mb,
        })
    }

    fn secrets_has_key_async_sync(&self, secrets: &SecretsStore, provider: &str) -> bool {
        let key = match provider {
            "openai" => std::env::var("OPENAI_API_KEY").is_ok(),
            "anthropic" => std::env::var("ANTHROPIC_API_KEY").is_ok(),
            "gemini" => std::env::var("GEMINI_API_KEY").is_ok(),
            "deepseek" => std::env::var("DEEPSEEK_API_KEY").is_ok(),
            _ => false,
        };
        if key {
            return true;
        }
        secrets
            .providers
            .get(&format!("provider:{provider}"))
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    fn _model_provider(&self, model: &str) -> String {
        if let Some(idx) = model.find(':') {
            let prefix = &model[..idx];
            if ocw_provider::get_descriptor(prefix).is_some() {
                return prefix.to_string();
            }
        }
        "openai".to_string()
    }

    // Extra suggested models for providers not fully covered by the curated matrix.
    // Matches upstream `manager.py COMPAT_MODELS`.
    fn compat_models() -> HashMap<&'static str, Vec<&'static str>> {
        HashMap::from([
            ("zai", vec!["glm-5.2", "glm-4.6"]),
            ("deepseek", vec!["deepseek-v4-flash", "deepseek-v4-pro"]),
            ("kimi", vec!["kimi-k2.6", "kimi-k2.5"]),
            (
                "minimax",
                vec!["MiniMax-M2.5", "MiniMax-M2.5-highspeed", "MiniMax-M3"],
            ),
            ("qwen", vec!["qwen3-max", "qwen3-coder-plus", "qwen-plus"]),
            ("xai", vec!["grok-4.3", "grok-4"]),
            (
                "mistral",
                vec!["mistral-large-latest", "mistral-small-latest"],
            ),
        ])
    }

    fn compat_models_once() -> &'static HashMap<&'static str, Vec<&'static str>> {
        static ONCE: std::sync::OnceLock<HashMap<&'static str, Vec<&'static str>>> =
            std::sync::OnceLock::new();
        ONCE.get_or_init(Self::compat_models)
    }

    pub async fn get_providers(&self) -> Vec<Value> {
        let descriptors = ocw_provider::all_descriptors();
        let secrets = self.secrets.read().await;
        descriptors
            .iter()
            .map(|d| {
                let configured = secrets
                    .providers
                    .get(&format!("provider:{}", d.name))
                    .and_then(|v| v.get("api_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                    || d.env_key
                        .as_ref()
                        .map(|k| env::var(k).is_ok())
                        .unwrap_or(false);

                let stored = secrets
                    .providers
                    .get(&format!("provider:{}", d.name))
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                let values: Map<String, Value> = stored
                    .into_iter()
                    .filter(|(k, v)| k != "api_key" && v.is_string())
                    .collect();

                let suggested: Vec<String> = {
                    let matrix_suggested = models_for_provider(&d.name);
                    let extra: &[&str] = Self::compat_models_once()
                        .get(&d.name as &str)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    matrix_suggested
                        .into_iter()
                        .chain(extra.iter().map(|s| (*s).to_string()))
                        .collect()
                };

                serde_json::json!({
                    "name": d.name,
                    "title": d.title,
                    "needs_key": d.needs_key,
                    "fields": d.fields,
                    "configured": configured,
                    "values": values,
                    "suggested_models": suggested,
                    "recommended_model": d.recommended_model,
                    "blurb": d.blurb,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Workspace trust store
// ---------------------------------------------------------------------------

/// Mirrors `coworker/workspace_trust.py` — a simple JSON file tracking trusted workspaces.
#[derive(Debug)]
pub struct WorkspaceTrustStore {
    path: PathBuf,
}

impl WorkspaceTrustStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("workspace_trust.json"),
        }
    }

    /// Canonical path: expand ~ and resolve symlinks.
    fn canonical(p: &str) -> String {
        let expanded = if p.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                p.replacen('~', &home.to_string_lossy(), 1)
            } else {
                p.to_string()
            }
        } else {
            p.to_string()
        };
        Path::new(&expanded)
            .canonicalize()
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or(expanded)
    }

    fn load(&self) -> std::collections::HashSet<String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(_) => return std::collections::HashSet::new(),
        };
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        value
            .get("trusted_workspaces")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save(&self, trusted: &std::collections::HashSet<String>) {
        let mut sorted: Vec<&String> = trusted.iter().collect();
        sorted.sort();
        let obj = json!({"trusted_workspaces": sorted});
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = serde_json::to_string_pretty(&obj).unwrap_or_default();
        let tmp = self.path.with_file_name(format!(
            ".{}.{}.tmp",
            self.path.file_stem().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let _ = std::fs::write(&tmp, &text);
        let _ = std::fs::rename(&tmp, &self.path);
    }

    pub fn is_trusted(&self, workspace: &str) -> bool {
        self.load().contains(&Self::canonical(workspace))
    }

    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = self.load().into_iter().collect();
        v.sort();
        v
    }

    pub fn set_trusted(&self, workspace: &str, trusted: bool) -> String {
        let canonical = Self::canonical(workspace);
        let mut values = self.load();
        if trusted {
            values.insert(canonical.clone());
        } else {
            values.remove(&canonical);
        }
        self.save(&values);
        canonical
    }

    /// List trusted workspaces with additional metadata (command allowances).
    pub fn list_detailed(&self) -> Vec<Value> {
        self.list()
            .into_iter()
            .map(|ws| {
                let p = Path::new(&ws);
                let exists = p.is_dir();
                let requested_commands: Vec<String> = if exists {
                    Self::read_workspace_allowed_commands(&ws)
                } else {
                    Vec::new()
                };
                json!({
                    "workspace": ws,
                    "exists": exists,
                    "requested_commands": requested_commands,
                })
            })
            .collect()
    }

    /// Read `.coworker/config.toml` for `allowed_commands` (mirror of
    /// `config.py::workspace_allowed_commands`).
    fn read_workspace_allowed_commands(workspace: &str) -> Vec<String> {
        crate::config::workspace_allowed_commands(workspace)
    }
}

// ---------------------------------------------------------------------------
// Session metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionMeta {
    pub session_id: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub message_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_agent() -> String {
    "code".to_string()
}

impl SessionMeta {
    pub fn new(session_id: String, workspace: Option<String>, agent: &str, model: &str) -> Self {
        Self {
            session_id,
            workspace,
            agent: agent.to_string(),
            model: model.to_string(),
            mode: "interactive".to_string(),
            title: None,
            pinned: false,
            archived: false,
            message_count: 0,
            updated_at: None,
        }
    }
}

/// In-memory message history per session.
#[derive(Default, Clone)]
pub struct SessionMessages {
    pub messages: Vec<Value>,
}

/// A workspace root entry (directory accessible to a session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootEntry {
    pub path: String,
    pub writable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub exists: bool,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub provider: Arc<dyn Provider>,
    pub memory_store: Arc<MemoryStore>,
    pub conversation_store: Arc<ConversationStore>,
    pub sessions: StdShared<HashMap<String, SessionMeta>>,
    pub session_messages: StdShared<HashMap<String, SessionMessages>>,
    pub ws_sessions: StdShared<HashMap<String, tokio::sync::broadcast::Sender<Value>>>,
    pub skill_store: Arc<SkillStore>,
    pub session_skills: Arc<SessionSkillStore>,
    pub session_roots: StdShared<HashMap<String, Vec<RootEntry>>>,
    pub settings: SettingsManager,
    pub automations: Arc<tokio::sync::RwLock<Arc<AutomationStore>>>,
    pub shell_executors: StdShared<HashMap<String, Arc<LocalExecutor>>>,
    /// Global event broadcast for `/ws/events` (automation_run_started, etc.)
    pub event_broadcast: broadcast::Sender<Value>,
    /// Workspace trust store (mirrors `coworker/workspace_trust.py`).
    pub trust_store: Arc<WorkspaceTrustStore>,
    /// Persona store (mirrors `coworker/personas/registry.py`).
    pub persona_store: Arc<PersonaStore>,
    /// MCP server config store (mirrors `coworker/mcp/config.py`).
    pub mcp_store: Arc<McpStore>,
    /// Live MCP client runtime (stdio/HTTP connect + tools/list cache).
    pub mcp_runtime: Arc<McpRuntime>,
    /// Connector store (descriptors + connection state).
    pub connector_store: Arc<ConnectorStore>,
    /// Cross-session inbox for human-attention items (approvals/questions/notifications).
    pub inbox_store: Arc<InboxStore>,
    /// Named inboxes + delivery bindings (Slack/Telegram mirroring).
    pub inbox_routing: Arc<InboxRouting>,
    /// Parked unauthorized messages awaiting human resolution
    /// (mirrors `coworker/unrouted.py` + `manager.parked`).
    pub parked_messages: StdShared<HashMap<String, Value>>,
    /// Durable channel subscriptions (inbound listening records).
    pub subscriptions: Arc<SubscriptionStore>,
    /// Per-session unattended flags (mirrors `coworker/unattended.py`).
    pub unattended: Arc<UnattendedStore>,
    /// Per-persona connector defaults (mirrors `coworker/connections.py`).
    pub persona_connections: Arc<PersonaConnectionStore>,
    /// Per-session connector overrides (mirrors `coworker/connections.py`).
    pub session_connections: Arc<SessionConnectionStore>,
    /// Dead-letter store (unrouted messages + failed background turns).
    pub unrouted: Arc<UnroutedStore>,
    /// Durable audit log for connector/tool actions.
    pub audit: Arc<AuditStore>,
    /// Recently-seen channels (the picker's "recently-seen" source).
    pub channel_buffer: Arc<ChannelBuffer>,
    /// Browser-session state contract (no Playwright in this build).
    pub browser: Arc<BrowserController>,
}

impl AppState {
    pub fn new(config: Config, provider: Arc<dyn Provider>) -> Self {
        let data_dir = config.data_dir.clone();
        let conversation_store = ConversationStore::open(&data_dir).unwrap_or_else(|_| {
            // Fallback: create in temp dir if main dir fails
            let fallback = std::env::temp_dir().join("coworker");
            ConversationStore::open(&fallback).unwrap()
        });

        // Restore sessions from persistent store on startup
        let mut restored_sessions: HashMap<String, SessionMeta> = HashMap::new();
        let mut restored_roots: HashMap<String, Vec<RootEntry>> = HashMap::new();
        for s in conversation_store.list(None).unwrap_or_default() {
            let meta = SessionMeta {
                session_id: s.session_id.clone(),
                workspace: if s.workspace.is_empty() {
                    None
                } else {
                    Some(s.workspace)
                },
                agent: s.agent,
                model: s.model,
                mode: s.mode,
                title: s.title,
                pinned: s.pinned,
                archived: s.archived,
                message_count: s.message_count,
                updated_at: s.updated_at,
            };
            restored_sessions.insert(s.session_id.clone(), meta);
            // Restore extra (granted) roots — mirror of `manager.py::get_roots`,
            // which rebuilds them from `record.extra_roots` after a restart.
            if let Ok(Some(record)) = conversation_store.load(&s.session_id) {
                let extra: Vec<RootEntry> = record
                    .extra_roots
                    .iter()
                    .filter_map(|v: &Value| {
                        let path = v.get("path")?.as_str()?.to_string();
                        let writable =
                            v.get("writable").and_then(|b| b.as_bool()).unwrap_or(false);
                        let label = v
                            .get("label")
                            .and_then(|l| l.as_str())
                            .map(|s| s.to_string());
                        Some(RootEntry {
                            path: path.clone(),
                            writable,
                            label,
                            primary: false,
                            exists: Path::new(&path).is_dir(),
                        })
                    })
                    .collect();
                if !extra.is_empty() {
                    restored_roots.insert(s.session_id, extra);
                }
            }
        }
        let sessions = restored_sessions;

        let settings = SettingsManager::new(data_dir.clone());
        // Inject loaded secrets into the provider router so API calls use real keys.
        provider.update_secrets(settings.secrets_providers());

        let (event_broadcast, _) = broadcast::channel(256);

        Self {
            config,
            provider,
            memory_store: Arc::new(MemoryStore::new()),
            conversation_store: Arc::new(conversation_store),
            sessions: Arc::new(StdRwLock::new(sessions)),
            session_messages: Arc::new(StdRwLock::new(HashMap::new())),
            ws_sessions: Arc::new(StdRwLock::new(HashMap::new())),
            skill_store: Arc::new(SkillStore::new(data_dir.clone())),
            session_skills: Arc::new(SessionSkillStore::new(Some(
                data_dir.join("session-skills.json"),
            ))),
            session_roots: Arc::new(StdRwLock::new(restored_roots)),
            settings,
            automations: Arc::new(TokioRwLock::new(Arc::new(AutomationStore::new(
                data_dir.clone(),
            )))),
            shell_executors: Arc::new(StdRwLock::new(HashMap::new())),
            event_broadcast,
            trust_store: Arc::new(WorkspaceTrustStore::new(&data_dir)),
            persona_store: Arc::new(PersonaStore::new(&data_dir)),
            mcp_store: Arc::new(McpStore::new(&data_dir)),
            mcp_runtime: Arc::new(McpRuntime::new()),
            connector_store: Arc::new(ConnectorStore::new()),
            inbox_store: Arc::new(
                InboxStore::new(Some(data_dir.join("inbox.json")))
                    .unwrap_or_else(|_| InboxStore::new(None::<&str>).expect("in-memory inbox")),
            ),
            inbox_routing: Arc::new(InboxRouting::new(Some(data_dir.join("inbox_routing.json")))),
            parked_messages: Arc::new(StdRwLock::new(HashMap::new())),
            subscriptions: Arc::new(SubscriptionStore::new(Some(
                data_dir.join("subscriptions.json"),
            ))),
            unattended: Arc::new(UnattendedStore::open(&data_dir)),
            persona_connections: Arc::new(PersonaConnectionStore::open(&data_dir)),
            session_connections: Arc::new(SessionConnectionStore::open(&data_dir)),
            unrouted: Arc::new(UnroutedStore::new(
                Some(data_dir.join("unrouted.json")),
                crate::stores::UNROUTED_CAP,
            )),
            audit: Arc::new(AuditStore::open(data_dir.join("audit.jsonl"))),
            channel_buffer: Arc::new(ChannelBuffer::new(
                crate::stores::BUFFER_CAP,
                Some(data_dir.join("channels.json")),
            )),
            browser: Arc::new(BrowserController::new()),
        }
    }

    /// Get or create a per-workspace shell executor and register its tools.
    pub fn register_shell_tools_for_workspace(
        &self,
        registry: &mut ocw_engine::ToolRegistry,
        workspace: &str,
    ) {
        let ws_path = std::path::PathBuf::from(workspace);
        let ws_key = ws_path
            .canonicalize()
            .unwrap_or_else(|_| ws_path.clone())
            .display()
            .to_string();

        let executor = {
            let mut execs = self.shell_executors.write().unwrap();
            execs
                .entry(ws_key.clone())
                .or_insert_with(|| {
                    Arc::new(
                        LocalExecutor::new(
                            &ws_path, None, // use default env
                            None, // default shell
                            None, // default timeout
                            None, // default max output
                        )
                        .unwrap(),
                    )
                })
                .clone()
        };
        ocw_shell::register_all(registry, executor);
    }

    /// Build initial system messages for a new turn engine.
    /// Includes agent instructions, memory, skill catalog, and current date.
    pub fn build_system_messages(
        &self,
        agent: &str,
        workspace: &str,
        _model: &str,
    ) -> Vec<ocw_engine::Message> {
        let mut messages = Vec::new();
        let now = chrono::Local::now();
        let date_str = now.format("%Y-%m-%d").to_string();

        let base_instructions = crate::agents::get_agent(agent).system_prompt;

        let ws_info = format!("Current date: {date_str}\nWorkspace: {workspace}",);

        // Inject memory entries
        let memory_text = self.format_memory_for_prompt(workspace);

        // Inject skill catalog
        let skill_text = self.format_skills_for_prompt(workspace);

        let mut system = String::new();
        system.push_str(&base_instructions);
        system.push_str("\n\n");
        system.push_str(&ws_info);
        if !workspace.trim().is_empty() {
            // AGENTS.md conventions (global + project root) — mirror of
            // `agent.py`'s `conventions = load_agents_md(ws)` branch.
            let conventions = crate::project::load_agents_md(workspace, &self.config.data_dir);
            if !conventions.is_empty() {
                system.push_str("\n\n");
                system.push_str(&conventions);
            }
        }
        if !memory_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&memory_text);
        }
        if !skill_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&skill_text);
        }

        messages.push(ocw_engine::Message::system(system));
        messages
    }

    /// Format memory entries for system prompt injection.
    fn format_memory_for_prompt(&self, workspace: &str) -> String {
        let entries = self.memory_store.list(None, Some(workspace), None);
        if entries.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = entries.iter().map(|e| format!("- {}", e.content)).collect();
        format!(
            "Memory (what the user has asked you to remember):\n{}",
            lines.join("\n")
        )
    }

    /// Format skill catalog for system prompt injection.
    fn format_skills_for_prompt(&self, workspace: &str) -> String {
        let skills = self.list_skills(Some(workspace));
        if skills.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        format!(
            "Available skills (use load_skill to see full instructions):\n{}",
            lines.join("\n")
        )
    }

    /// Returns config.default_model if set, otherwise finds the first
    /// provider with a configured API key (env var or secret store) and uses
    /// its recommended model.
    pub(crate) fn default_model_or_configured(&self) -> String {
        if !self.config.default_model.is_empty() {
            return self.config.default_model.clone();
        }
        let secrets = self.settings.secrets_providers();
        ocw_provider::all_descriptors()
            .iter()
            .find(|d| {
                let env_ok = d
                    .env_key
                    .as_ref()
                    .map(|k| std::env::var(k).is_ok())
                    .unwrap_or(false);
                let store_ok = secrets
                    .get(&format!("provider:{}", d.name))
                    .and_then(|v| v.get("api_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                env_ok || store_ok
            })
            .and_then(|d| d.recommended_model.clone())
            .unwrap_or_default()
    }

    pub async fn create_session(&self, workspace: Option<&str>, agent: &str) -> SessionMeta {
        let session_id = uuid::Uuid::new_v4().to_string();
        let model = self.default_model_or_configured();
        let meta = SessionMeta::new(
            session_id.clone(),
            workspace.map(String::from),
            agent,
            &model,
        );
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), meta.clone());
        self.session_messages
            .write()
            .unwrap()
            .insert(session_id, SessionMessages::default());
        // Persist to SQLite
        let _ = self.persist_session_meta(&meta, &[]);
        meta
    }

    /// Persist a SessionMeta to ConversationStore (creates or updates the SQLite row).
    fn persist_session_meta(&self, meta: &SessionMeta, messages: &[Value]) -> Result<(), String> {
        use ocw_data::SessionRecord;
        // Mirror of `manager.py::_extra_roots_of`: non-primary roots snapshot.
        let extra_roots: Vec<Value> = {
            let roots = self.session_roots.read().unwrap();
            roots
                .get(&meta.session_id)
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|r| !r.primary)
                        .map(|r| {
                            json!({
                                "path": r.path,
                                "writable": r.writable,
                                "label": r.label,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let record = SessionRecord {
            session_id: meta.session_id.clone(),
            workspace: meta.workspace.clone().unwrap_or_default(),
            model: meta.model.clone(),
            mode: meta.mode.clone(),
            messages: messages.to_vec(),
            title: meta.title.clone(),
            agent: meta.agent.clone(),
            message_count: meta.message_count,
            updated_at: meta.updated_at.clone(),
            extra_roots,
            grants: serde_json::json!({}),
            pinned: meta.pinned,
            archived: meta.archived,
            origin: None,
            origin_label: None,
            auto_title: None,
            renamed: false,
        };
        self.conversation_store
            .save(&record)
            .map_err(|e| e.to_string())
    }

    /// Get an existing session or create a new one with a specific id.
    /// Used by automation `__run__` sessions that need an explicit ID.
    pub fn get_or_create_session(
        &self,
        session_id: &str,
        agent: &str,
        workspace: Option<&str>,
    ) -> SessionMeta {
        {
            let sessions = self.sessions.read().unwrap();
            if let Some(s) = sessions.get(session_id) {
                return s.clone();
            }
        }
        let model = self.default_model_or_configured();
        let meta = SessionMeta::new(
            session_id.to_string(),
            workspace.map(String::from),
            agent,
            &model,
        );
        {
            let mut sessions = self.sessions.write().unwrap();
            if let Some(existing) = sessions.get(session_id) {
                return existing.clone();
            }
            sessions.insert(session_id.to_string(), meta.clone());
        }
        {
            let mut msgs = self.session_messages.write().unwrap();
            msgs.insert(session_id.to_string(), SessionMessages::default());
        }
        meta
    }

    pub async fn get_session(&self, session_id: &str) -> Option<SessionMeta> {
        self.sessions.read().unwrap().get(session_id).cloned()
    }

    pub async fn session_exists(&self, session_id: &str) -> bool {
        self.sessions.read().unwrap().contains_key(session_id)
    }

    pub async fn patch_session(
        &self,
        session_id: &str,
        title: Option<&str>,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> Option<SessionMeta> {
        let mut sessions = self.sessions.write().unwrap();
        let meta = sessions.get_mut(session_id)?;
        if let Some(t) = title {
            meta.title = Some(t.to_string());
        }
        if let Some(p) = pinned {
            meta.pinned = p;
        }
        if let Some(a) = archived {
            meta.archived = a;
        }
        Some(meta.clone())
    }

    pub async fn list_sessions(&self, workspace: Option<&str>) -> Vec<SessionMeta> {
        let sessions = self.sessions.read().unwrap();
        match workspace {
            Some(w) => sessions
                .values()
                .filter(|s| s.workspace.as_deref() == Some(w))
                .cloned()
                .collect(),
            None => sessions.values().cloned().collect(),
        }
    }

    pub async fn delete_session(&self, session_id: &str) {
        self.sessions.write().unwrap().remove(session_id);
        self.session_messages.write().unwrap().remove(session_id);
        self.ws_sessions.write().unwrap().remove(session_id);
        let _ = self.conversation_store.delete(session_id);
    }

    pub async fn list_messages(&self, session_id: &str) -> Vec<Value> {
        self.session_messages
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.messages.clone())
            .unwrap_or_default()
    }

    pub async fn push_message(&self, session_id: &str, msg: Value) {
        if let Some(data) = self.session_messages.write().unwrap().get_mut(session_id) {
            data.messages.push(msg.clone());
        }
        // Append to JSONL for persistence
        let _ = self.conversation_store.append_jsonl(session_id, &[msg]);
    }

    pub async fn broadcast(&self, session_id: &str, msg: Value) {
        let senders: Vec<_> = self
            .ws_sessions
            .read()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == session_id)
            .map(|(_, tx)| tx.clone())
            .collect();
        for tx in senders {
            let _ = tx.send(msg.clone());
        }
    }

    pub fn register_ws(&self, session_id: &str) -> tokio::sync::broadcast::Receiver<Value> {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        self.ws_sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), tx);
        rx
    }

    pub fn unregister_ws(&self, session_id: &str) {
        self.ws_sessions.write().unwrap().remove(session_id);
    }

    pub async fn register_ws_async(
        &self,
        session_id: &str,
    ) -> tokio::sync::broadcast::Receiver<Value> {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        self.ws_sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), tx);
        rx
    }

    pub async fn unregister_ws_async(&self, session_id: &str) {
        self.ws_sessions.write().unwrap().remove(session_id);
    }

    // Sync helpers (for non-async contexts)
    pub fn get_session_sync(&self, session_id: &str) -> Option<SessionMeta> {
        self.sessions.read().unwrap().get(session_id).cloned()
    }

    pub fn list_sessions_sync(&self, workspace: Option<&str>) -> Vec<SessionMeta> {
        let sessions = self.sessions.read().unwrap();
        match workspace {
            Some(w) => sessions
                .values()
                .filter(|s| s.workspace.as_deref() == Some(w))
                .cloned()
                .collect(),
            None => sessions.values().cloned().collect(),
        }
    }

    pub fn list_messages_sync(&self, session_id: &str) -> Vec<Value> {
        self.session_messages
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.messages.clone())
            .unwrap_or_default()
    }

    pub fn delete_session_sync(&self, session_id: &str) {
        self.sessions.write().unwrap().remove(session_id);
        self.session_messages.write().unwrap().remove(session_id);
        self.ws_sessions.write().unwrap().remove(session_id);
        let _ = self.conversation_store.delete(session_id);
    }

    pub fn broadcast_sync(&self, session_id: &str, msg: Value) {
        let senders: Vec<_> = self
            .ws_sessions
            .read()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == session_id)
            .map(|(_, tx)| tx.clone())
            .collect();
        for tx in senders {
            let _ = tx.send(msg.clone());
        }
    }

    pub fn push_message_sync(&self, session_id: &str, msg: Value) {
        if let Some(data) = self.session_messages.write().unwrap().get_mut(session_id) {
            data.messages.push(msg.clone());
        }
        // Append to JSONL for persistence
        let _ = self.conversation_store.append_jsonl(session_id, &[msg]);
    }

    // Skills helpers
    pub fn get_session_skills(&self, session_id: &str) -> std::collections::HashMap<String, bool> {
        self.session_skills.get(session_id)
    }

    pub fn set_session_skill(&self, session_id: &str, skill: &str, enabled: bool) {
        self.session_skills.set(session_id, skill, enabled);
    }

    pub fn list_skills(&self, workspace: Option<&str>) -> Vec<ocw_skills::SkillRow> {
        let ws = workspace.map(std::path::PathBuf::from);
        self.skill_store.rows(ws.as_deref())
    }

    // --- session roots ---

    pub fn get_roots(&self, session_id: &str) -> Vec<RootEntry> {
        let roots = self.session_roots.read().unwrap();
        if let Some(entries) = roots.get(session_id) {
            let refreshed: Vec<RootEntry> = entries
                .iter()
                .map(|r| {
                    let exists = Path::new(&r.path).is_dir();
                    RootEntry {
                        exists,
                        ..r.clone()
                    }
                })
                .collect();
            if refreshed.iter().any(|r| r.primary) {
                return refreshed;
            }
            // Stored entries carry no primary scratch: prepend the workspace
            // as the primary root (mirror of `manager.py::get_roots`, which
            // always reports workspace/scratch first, then extra roots).
            let mut out: Vec<RootEntry> = Vec::new();
            {
                let sessions = self.sessions.read().unwrap();
                if let Some(meta) = sessions.get(session_id) {
                    if let Some(ws) = &meta.workspace {
                        let exists = Path::new(ws).is_dir();
                        out.push(RootEntry {
                            path: ws.clone(),
                            writable: true,
                            label: Some("scratch".into()),
                            primary: true,
                            exists,
                        });
                    }
                }
            }
            out.extend(refreshed);
            out
        } else {
            // Return workspace as primary root if available
            let sessions = self.sessions.read().unwrap();
            if let Some(meta) = sessions.get(session_id) {
                if let Some(ref ws) = meta.workspace {
                    let exists = Path::new(ws).is_dir();
                    return vec![RootEntry {
                        path: ws.clone(),
                        writable: true,
                        label: Some("scratch".into()),
                        primary: true,
                        exists,
                    }];
                }
            }
            Vec::new()
        }
    }

    /// Persist non-primary roots to the conversation store — mirror of
    /// `manager.py::add_root`'s `session_store.set_extra_roots(...)`.
    fn persist_extra_roots(&self, session_id: &str) {
        let extra: Vec<serde_json::Value> = {
            let roots = self.session_roots.read().unwrap();
            roots
                .get(session_id)
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|r| !r.primary)
                        .map(|r| {
                            serde_json::json!({
                                "path": r.path,
                                "writable": r.writable,
                                "label": r.label,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let _ = self.conversation_store.set_extra_roots(session_id, extra);
    }

    pub fn add_root(
        &self,
        session_id: &str,
        path: &str,
        writable: bool,
    ) -> Result<Vec<RootEntry>, String> {
        let p = Path::new(path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        let resolved = p
            .canonicalize()
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());

        let mut roots = self.session_roots.write().unwrap();
        let entries = roots.entry(session_id.to_string()).or_default();

        if let Some(existing) = entries.iter_mut().find(|r| r.path == resolved) {
            existing.writable = writable;
        } else {
            entries.push(RootEntry {
                path: resolved.clone(),
                writable,
                label: Some(
                    Path::new(&resolved)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                ),
                primary: false,
                exists: true,
            });
        }
        drop(roots);
        self.persist_extra_roots(session_id);
        Ok(self.get_roots(session_id))
    }

    pub fn remove_root(&self, session_id: &str, path: &str) -> Result<Vec<RootEntry>, String> {
        let resolved = Path::new(path)
            .canonicalize()
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());

        let mut roots = self.session_roots.write().unwrap();
        if let Some(entries) = roots.get_mut(session_id) {
            // Don't allow removing primary/scratch root
            if entries.iter().any(|r| r.primary && r.path == resolved) {
                return Err("cannot remove the primary scratch directory".into());
            }
            entries.retain(|r| r.path != resolved);
        }
        drop(roots);
        self.persist_extra_roots(session_id);
        Ok(self.get_roots(session_id))
    }

    /// List artifacts (files) in the session's workspace directory.
    pub fn list_artifacts(&self, session_id: &str) -> Vec<Value> {
        let sessions = self.sessions.read().unwrap();
        let workspace = sessions.get(session_id).and_then(|m| m.workspace.clone());
        drop(sessions);

        let Some(ws) = workspace else {
            return Vec::new();
        };
        let root = PathBuf::from(&ws);
        if !root.is_dir() {
            return Vec::new();
        }

        let suffixes: std::collections::HashSet<&str> = [
            ".md",
            ".markdown",
            ".html",
            ".htm",
            ".txt",
            ".json",
            ".csv",
            ".tsv",
            ".py",
            ".js",
            ".ts",
            ".tsx",
            ".css",
            ".png",
            ".jpg",
            ".jpeg",
            ".webp",
            ".gif",
            ".pdf",
            ".xlsx",
            ".xls",
            ".pptx",
            ".ppt",
            ".pptm",
            ".docx",
            ".doc",
            ".docm",
        ]
        .iter()
        .copied()
        .collect();
        let skip_dirs: std::collections::HashSet<&str> =
            ["node_modules", "target", "dist", "__pycache__", ".git"]
                .iter()
                .copied()
                .collect();

        let mut artifacts: Vec<_> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                let rel = path.strip_prefix(&root).ok()?;
                // Skip hidden files/dirs and skip-dirs
                if rel.components().any(|c| {
                    let s = c.as_os_str().to_string_lossy();
                    s.starts_with('.') || skip_dirs.contains(s.as_ref())
                }) {
                    return None;
                }
                if !path.is_file() { return None; }
                let ext = path.extension()?.to_str()?;
                if !suffixes.contains(ext) && !suffixes.contains(&format!(".{ext}").as_str()) { return None; }
                let meta = path.metadata().ok()?;
                let size = meta.len();
                let modified = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                Some(serde_json::json!({
                    "path": rel.to_string_lossy(),
                    "abs_path": path.to_string_lossy(),
                    "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "kind": "text",
                    "size": size,
                    "modified_at": modified,
                }))
            })
            .collect();
        artifacts.sort_by(|a: &Value, b: &Value| {
            let ma = a.get("modified_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let mb = b.get("modified_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
            mb.partial_cmp(&ma).unwrap_or(std::cmp::Ordering::Equal)
        });
        artifacts.truncate(80);
        artifacts
    }

    /// Read an artifact file, returning a data URL for binary/image files or text content.
    pub fn read_artifact(&self, session_id: &str, path: &str) -> Value {
        let sessions = self.sessions.read().unwrap();
        let workspace = sessions.get(session_id).and_then(|m| m.workspace.clone());
        drop(sessions);

        let Some(ws) = workspace else {
            return json!({"ok": false, "error": "no workspace"});
        };
        let root = PathBuf::from(&ws);
        let target = root.join(path);

        // Security: path must be within workspace
        if target
            .canonicalize()
            .map_or(true, |t| !t.starts_with(&root))
        {
            return json!({"ok": false, "error": "path escapes workspace"});
        }
        if !target.is_file() {
            return json!({"ok": false, "error": "not found"});
        }

        let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
        let binary_exts = ["png", "jpg", "jpeg", "webp", "gif", "pdf", "xlsx", "xls"];

        if binary_exts.contains(&ext) {
            let Ok(data) = std::fs::read(&target) else {
                return json!({"ok": false, "error": "failed to read file"});
            };
            if data.len() > 25 * 1024 * 1024 {
                return json!({"ok": false, "error": "file too large to preview"});
            }
            let mime = match ext {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                "gif" => "image/gif",
                "pdf" => "application/pdf",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xls" => "application/vnd.ms-excel",
                _ => "application/octet-stream",
            };
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            return json!({
                "ok": true,
                "path": path,
                "kind": if ext == "pdf" { "pdf" } else if ext.starts_with("xls") { "sheet" } else { "image" },
                "data_url": format!("data:{mime};base64,{b64}"),
            });
        }

        match std::fs::read_to_string(&target) {
            Ok(text) => {
                let truncated = text.len() > 500_000;
                json!({
                    "ok": true,
                    "path": path,
                    "kind": "text",
                    "content": if truncated { &text[..500_000] } else { &text },
                    "truncated": truncated,
                })
            }
            Err(_) => json!({"ok": false, "error": "binary file cannot be previewed"}),
        }
    }

    /// Reveal a file in the OS file manager or open with default app.
    pub fn reveal_artifact(&self, session_id: &str, path: &str, mode: &str) -> Value {
        let sessions = self.sessions.read().unwrap();
        let workspace = sessions.get(session_id).and_then(|m| m.workspace.clone());
        drop(sessions);

        let Some(ws) = workspace else {
            return json!({"ok": false, "error": "no workspace"});
        };
        let root = PathBuf::from(&ws);
        let target = root.join(path);

        if target
            .canonicalize()
            .map_or(true, |t| !t.starts_with(&root))
        {
            return json!({"ok": false, "error": "path escapes workspace"});
        }
        if !target.is_file() {
            return json!({"ok": false, "error": "not found"});
        }

        let result = if cfg!(target_os = "macos") {
            let target_str = target.to_string_lossy().to_string();
            if mode == "reveal" {
                std::process::Command::new("open")
                    .args(["-R", &target_str])
                    .spawn()
            } else {
                std::process::Command::new("open").arg(&target_str).spawn()
            }
        } else if cfg!(target_os = "windows") {
            if mode == "reveal" {
                std::process::Command::new("explorer")
                    .arg(format!("/select,{}", target.display()))
                    .spawn()
            } else {
                std::process::Command::new("cmd")
                    .args(["/c", "start", "", target.to_string_lossy().as_ref()])
                    .spawn()
            }
        } else {
            let tgt = if mode == "reveal" {
                target
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                target.to_string_lossy().to_string()
            };
            std::process::Command::new("xdg-open").arg(&tgt).spawn()
        };

        match result {
            Ok(_) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// Session connections drawer payload (mirror of
    /// `manager.session_connections_view`, UI-REFRESH §6): every
    /// account-connected connector with its effective on/off state (muted ones
    /// stay VISIBLE as off), the persona's connector recommends that aren't yet
    /// account-connected, and the attention count.
    pub fn get_connections(&self, session_id: &str, dm_session: Option<&str>) -> Value {
        let persona = self.persona_of(session_id);

        // Layer 1: account-connected connector names + descriptor lookup.
        let mut by_name: HashMap<String, Value> = HashMap::new();
        let mut connected_names: BTreeSet<String> = BTreeSet::new();
        for c in self.connector_store.list() {
            let name = c
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            by_name.insert(name.clone(), c.clone());
            if c.get("connected").and_then(|v| v.as_bool()).unwrap_or(false) {
                connected_names.insert(name);
            }
        }

        // Layer 2: persona defaults (seeded from manifest recommends on first
        // read), Layer 3: session overrides.
        let defaults = self.persona_defaults(&persona);
        let overrides = self.session_connections.get(session_id);

        // Effective = connected AND (override if present, else persona default
        // if present, else inherit-on).
        let mut effective: BTreeSet<String> = BTreeSet::new();
        for name in &connected_names {
            let enabled = if let Some(&v) = overrides.get(name) {
                v
            } else if let Some(&v) = defaults.get(name) {
                v
            } else {
                true
            };
            if enabled {
                effective.insert(name.clone());
            }
        }

        let connected: Vec<Value> = connected_names
            .iter()
            .map(|name| {
                json!({
                    "connector": name,
                    "enabled": effective.contains(name),
                    "detail": self.connection_detail(
                        session_id, name, dm_session, by_name.get(name),
                    ),
                })
            })
            .collect();

        let mut recommended: Vec<Value> = Vec::new();
        if let Some(entry) = self.persona_store.get(&persona) {
            for rec in &entry.recommends {
                if rec.kind == "connector" && !connected_names.contains(&rec.r#ref) {
                    recommended.push(json!({
                        "connector": rec.r#ref,
                        "reason": rec.reason,
                        "tier": rec.tier,
                        "connected": false,
                    }));
                }
            }
        }

        json!({
            "connected": connected,
            "recommended": recommended,
            "attention": recommended.len(),
        })
    }

    /// The session's persona: the record's agent, else the store default
    /// (mirror of `manager._persona_of`).
    fn persona_of(&self, session_id: &str) -> String {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .map(|s| s.agent.clone())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| self.persona_store.default_persona())
    }

    /// The persona's default connector map, seeding it from the manifest's
    /// connector recommends on first read (core → on, optional → off; mirror of
    /// `PersonaConnectionStore.defaults_for`).
    pub(crate) fn persona_defaults(&self, persona: &str) -> HashMap<String, bool> {
        let existing = self.persona_connections.get(persona);
        if !existing.is_empty() {
            return existing;
        }
        let mut seeded = HashMap::new();
        if let Some(entry) = self.persona_store.get(persona) {
            for rec in &entry.recommends {
                if rec.kind == "connector" {
                    seeded.insert(rec.r#ref.clone(), rec.tier == "core");
                }
            }
        }
        self.persona_connections.defaults_for(persona, seeded)
    }

    /// The persona's default connections as a list annotated with
    /// account-connectedness (UI-REFRESH §5).
    pub fn persona_default_connections(&self, persona: &str) -> Vec<Value> {
        let connected = self.connected_connectors();
        let defaults = self.persona_defaults(persona);
        let mut out: Vec<Value> = defaults
            .iter()
            .map(|(c, enabled)| {
                json!({
                    "connector": c,
                    "enabled": enabled,
                    "connected": connected.contains(c),
                })
            })
            .collect();
        out.sort_by_key(|v| {
            v.get("connector")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });
        out
    }

    fn connected_connectors(&self) -> BTreeSet<String> {
        self.connector_store
            .list()
            .iter()
            .filter(|c| c.get("connected").and_then(|v| v.as_bool()).unwrap_or(false))
            .filter_map(|c| {
                c.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    /// A short human description of WHY a connector is live for a session: the
    /// chat ids it's subscribed to on that platform, plus "DMs" if this is the
    /// designated DM session (mirror of `manager._connection_detail`).
    fn connection_detail(
        &self,
        session_id: &str,
        connector: &str,
        dm_session: Option<&str>,
        info: Option<&Value>,
    ) -> String {
        let prefix = format!("{connector}:");
        let mut parts: Vec<String> = self
            .subscriptions
            .for_session(session_id)
            .iter()
            .filter(|s| s.channel.starts_with(&prefix))
            .filter_map(|s| s.channel.split_once(':').map(|(_, rest)| rest.to_string()))
            .collect();
        if dm_session == Some(session_id) {
            parts.push("DMs".to_string());
        }
        if !parts.is_empty() {
            return parts.join(" · ");
        }
        info.and_then(|v| v.get("title"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| connector.to_string())
    }

    /// Session unattended flag (mirror of `UnattendedRegistry.is_unattended`).
    pub fn get_unattended(&self, session_id: &str) -> Value {
        json!({"unattended": self.unattended.is_unattended(session_id)})
    }

    /// Set the session's unattended flag (mirror of `UnattendedRegistry.set`).
    pub fn set_unattended(&self, session_id: &str, on: bool) {
        self.unattended.set(session_id, on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ocw-state-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_state(dir: PathBuf) -> AppState {
        let config = Config {
            data_dir: dir,
            ..Config::default()
        };
        let provider: Arc<dyn ocw_provider::Provider> =
            Arc::new(ocw_provider::Router::new("anthropic"));
        AppState::new(config, provider)
    }

    #[tokio::test]
    async fn add_root_persists_and_restores_extra_roots() {
        let dir = temp_data_dir("roots");
        let state = make_state(dir.clone());
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();

        // Create a real directory to grant.
        let target = dir.join("granted-dir");
        std::fs::create_dir_all(&target).unwrap();
        let target_str = target.to_string_lossy().to_string();

        // add_root → session roots + SQLite extra_roots
        let roots = state.add_root(&sid, &target_str, true).unwrap();
        assert!(roots.iter().any(|r| r.path.ends_with("granted-dir") && r.writable));
        let record = state.conversation_store.load(&sid).unwrap().unwrap();
        assert_eq!(record.extra_roots.len(), 1);
        assert_eq!(
            record.extra_roots[0].get("path").and_then(|v| v.as_str()).unwrap(),
            roots.iter().find(|r| r.path.ends_with("granted-dir")).unwrap().path
        );

        // Rebuild AppState from the same data dir → roots restored.
        let state2 = make_state(dir.clone());
        let restored = state2.get_roots(&sid);
        assert!(restored.iter().any(|r| r.path.ends_with("granted-dir")));

        // remove_root → no longer persisted.
        let path_to_remove = roots.iter().find(|r| r.path.ends_with("granted-dir")).unwrap().path.clone();
        let roots2 = state2.remove_root(&sid, &path_to_remove).unwrap();
        assert!(!roots2.iter().any(|r| r.path == path_to_remove));
        let record2 = state2.conversation_store.load(&sid).unwrap().unwrap();
        assert!(record2.extra_roots.is_empty());
    }

    #[tokio::test]
    async fn get_roots_prepends_workspace_primary() {
        let dir = temp_data_dir("prim");
        let state = make_state(dir.clone());
        let ws = dir.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let meta = state
            .create_session(Some(ws.to_string_lossy().as_ref()), "code")
            .await;
        let sid = meta.session_id.clone();

        // No extra roots: workspace is reported as the primary scratch root.
        let roots = state.get_roots(&sid);
        assert_eq!(roots.len(), 1);
        assert!(roots[0].primary);
        assert!(roots[0].path.ends_with("workspace"));
        assert!(roots[0].writable);

        // With a granted root, the workspace primary comes first.
        let target = dir.join("granted");
        std::fs::create_dir_all(&target).unwrap();
        state
            .add_root(&sid, &target.to_string_lossy(), false)
            .unwrap();
        let roots = state.get_roots(&sid);
        assert_eq!(roots.len(), 2);
        assert!(roots[0].primary);
        assert!(!roots[1].primary);
        assert!(roots[1].path.ends_with("granted"));
    }
}
