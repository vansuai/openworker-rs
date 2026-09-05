//! Shared server state.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use parking_lot::RwLock as PlRwLock;
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

// When-to-remember rules, injected whenever the memory store is wired. Without these,
// models either never call `remember` or save noise the repo already records.
// Mirror of `coworker/agent.py::_MEMORY_GUIDANCE`.
const MEMORY_GUIDANCE: &str = "Memory:\n\
- You have persistent memory across sessions. Use `remember` for durable facts: the user's \
corrections and stated preferences (include the why), and project context you couldn't \
rederive from the code. Don't save what the repo already records (code structure, git \
history, AGENTS.md) or details that only matter to the current task. Use absolute dates, \
never \"yesterday\".\n\
- Before saving, check the known-memories list: if an entry already covers it, revise that \
entry with `memory_update` instead of adding a near-duplicate; retire wrong or obsolete \
entries with `memory_forget`.\n\
- Memories reflect when they were written. If one names a file, flag, or URL, verify it \
still exists before relying on it.";

// The GUI interleaves narration lines with humanized tool rows inside a collapsed "turn" —
// they're what the user reads while the agent works. Universal (appended for every agent).
// Mirror of `coworker/agent.py::_NARRATION_GUIDANCE`.
const NARRATION_GUIDANCE: &str = "Narration: before each batch of tool calls, write ONE short \
plain sentence saying what you're doing and why (e.g. \"Checking what merged since \
yesterday's digest.\"). It is shown to the user as live progress. Don't narrate trivial \
single-call follow-ups, don't repeat the previous line, and never let narration replace \
your final answer.";

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
    /// Composer context-window fill bar (OFF by default).
    context_bar: bool,
    /// Auto-Approve feature flag (prefs win over config.toml).
    auto_approve: Option<bool>,
    /// Shadow-eval sibling of Auto-Approve (audit-only later).
    auto_approve_shadow: Option<bool>,
    /// Auto-compaction overrides (OPE-27).
    compaction_threshold_pct: Option<f64>,
    compaction_cap_tokens: Option<i64>,
    compaction_model: Option<String>,
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
            context_bar: get_bool("context_bar"),
            auto_approve: obj.get("auto_approve").and_then(|v| v.as_bool()),
            auto_approve_shadow: obj.get("auto_approve_shadow").and_then(|v| v.as_bool()),
            compaction_threshold_pct: obj
                .get("compaction_threshold_pct")
                .and_then(|v| v.as_f64()),
            compaction_cap_tokens: obj
                .get("compaction_cap_tokens")
                .and_then(|v| v.as_i64()),
            compaction_model: get_str("compaction_model"),
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
        if self.context_bar {
            obj.insert("context_bar".into(), serde_json::json!(true));
        }
        if let Some(v) = self.auto_approve {
            obj.insert("auto_approve".into(), serde_json::json!(v));
        }
        if let Some(v) = self.auto_approve_shadow {
            obj.insert("auto_approve_shadow".into(), serde_json::json!(v));
        }
        if let Some(pct) = self.compaction_threshold_pct {
            obj.insert("compaction_threshold_pct".into(), serde_json::json!(pct));
        }
        if let Some(cap) = self.compaction_cap_tokens {
            obj.insert("compaction_cap_tokens".into(), serde_json::json!(cap));
        }
        if let Some(ref model) = self.compaction_model {
            obj.insert("compaction_model".into(), serde_json::json!(model));
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

/// Result of a non-blocking prefs read for `default_model`.
enum PrefsDefaultRead {
    Value(String),
    /// `try_read()` failed — caller should fall back to the cached effective default.
    Contended,
}

#[derive(Clone)]
pub struct SettingsManager {
    prefs_path: PathBuf,
    prefs: Arc<tokio::sync::RwLock<Prefs>>,
    secrets_path: PathBuf,
    dotenv_path: PathBuf,
    secrets: Arc<tokio::sync::RwLock<SecretsStore>>,
    /// Last resolved default model — used when prefs lock is contended so scheduler
    /// paths don't silently fall back to the first configured provider (DeepSeek).
    effective_default_model: Arc<StdRwLock<String>>,
}

/// Whether a provider has a usable API key (env var or secrets store).
fn provider_has_configured_key(provider: &str, secrets: &SecretsStore) -> bool {
    if provider == "ollama" {
        return true;
    }
    let env_ok = ocw_provider::get_descriptor(provider)
        .and_then(|d| d.env_key.as_ref())
        .map(|k| std::env::var(k).is_ok())
        .unwrap_or(false);
    if env_ok {
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
            if !provider_has_configured_key(provider, &secrets) {
                prefs.default_model = String::new();
                prefs.save(&prefs_path);
            }
        }

        let effective_default_model = Arc::new(StdRwLock::new(prefs.default_model.clone()));

        Self {
            prefs_path,
            prefs: Arc::new(tokio::sync::RwLock::new(prefs)),
            secrets_path,
            dotenv_path,
            secrets: Arc::new(tokio::sync::RwLock::new(secrets)),
            effective_default_model,
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

    /// Non-blocking read of prefs.default_model for hot paths.
    fn read_default_model_prefs(&self) -> PrefsDefaultRead {
        match self.prefs.try_read() {
            Ok(p) => PrefsDefaultRead::Value(p.default_model.clone()),
            Err(_) => PrefsDefaultRead::Contended,
        }
    }

    pub fn effective_default_model_cached(&self) -> String {
        self.effective_default_model
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_effective_default_model_cache(&self, model: &str) {
        if let Ok(mut guard) = self.effective_default_model.write() {
            *guard = model.to_string();
        }
    }

    pub async fn set_default_model(&self, model: String) {
        let mut p = self.prefs.write().await;
        p.default_model = model.clone();
        p.save(&self.prefs_path);
        self.set_effective_default_model_cache(&model);
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
    pub async fn context_bar(&self) -> bool {
        self.prefs.read().await.context_bar
    }

    pub async fn set_context_bar(&self, shown: bool) {
        let mut p = self.prefs.write().await;
        p.context_bar = shown;
        p.save(&self.prefs_path);
    }

    pub async fn auto_approve(&self) -> bool {
        self.prefs.read().await.auto_approve.unwrap_or(false)
    }

    pub async fn auto_approve_shadow(&self) -> bool {
        self.prefs
            .read()
            .await
            .auto_approve_shadow
            .unwrap_or(false)
    }

    pub async fn set_auto_approve(&self, on: bool) {
        let mut p = self.prefs.write().await;
        p.auto_approve = Some(on);
        p.save(&self.prefs_path);
    }

    pub async fn set_auto_approve_shadow(&self, on: bool) {
        let mut p = self.prefs.write().await;
        p.auto_approve_shadow = Some(on);
        p.save(&self.prefs_path);
    }

    /// Live auto-compaction knobs (OPE-27) — engine reads these per check.
    pub async fn compaction_settings(&self) -> serde_json::Map<String, Value> {
        let p = self.prefs.read().await;
        let mut m = serde_json::Map::new();
        m.insert(
            "threshold_pct".into(),
            json!(p
                .compaction_threshold_pct
                .unwrap_or(ocw_engine::DEFAULT_THRESHOLD_PCT)),
        );
        m.insert(
            "cap_tokens".into(),
            json!(p
                .compaction_cap_tokens
                .unwrap_or(ocw_engine::DEFAULT_CAP_TOKENS)),
        );
        m.insert(
            "model".into(),
            json!(p.compaction_model.clone().unwrap_or_default()),
        );
        m.insert("enabled".into(), json!(true));
        m
    }

    pub async fn compaction_settings_payload(&self) -> Value {
        let s = self.compaction_settings().await;
        json!({
            "compaction_threshold_pct": s.get("threshold_pct"),
            "compaction_cap_tokens": s.get("cap_tokens"),
            "compaction_model": s.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        })
    }

    pub async fn set_compaction_settings(
        &self,
        threshold_pct: Option<f64>,
        cap_tokens: Option<i64>,
        model: Option<String>,
    ) -> Result<Value, String> {
        let mut p = self.prefs.write().await;
        if let Some(pct) = threshold_pct {
            if !(0.10..=0.95).contains(&pct) {
                return Err("compaction_threshold_pct must be between 0.10 and 0.95".into());
            }
            p.compaction_threshold_pct = Some(pct);
        }
        if let Some(cap) = cap_tokens {
            p.compaction_cap_tokens = Some(cap.clamp(10_000, 2_000_000));
        }
        if let Some(m) = model {
            p.compaction_model = Some(m);
        }
        p.save(&self.prefs_path);
        drop(p);
        let s = self.compaction_settings().await;
        Ok(json!({
            "ok": true,
            "threshold_pct": s.get("threshold_pct"),
            "cap_tokens": s.get("cap_tokens"),
            "model": s.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    /// Sync compaction map for engine hot paths (try_read; falls back to defaults).
    pub fn compaction_settings_sync(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        match self.prefs.try_read() {
            Ok(p) => {
                m.insert(
                    "threshold_pct".into(),
                    json!(p
                        .compaction_threshold_pct
                        .unwrap_or(ocw_engine::DEFAULT_THRESHOLD_PCT)),
                );
                m.insert(
                    "cap_tokens".into(),
                    json!(p
                        .compaction_cap_tokens
                        .unwrap_or(ocw_engine::DEFAULT_CAP_TOKENS)),
                );
                m.insert(
                    "model".into(),
                    json!(p.compaction_model.clone().unwrap_or_default()),
                );
                m.insert("enabled".into(), json!(true));
            }
            Err(_) => {
                m.insert(
                    "threshold_pct".into(),
                    json!(ocw_engine::DEFAULT_THRESHOLD_PCT),
                );
                m.insert("cap_tokens".into(), json!(ocw_engine::DEFAULT_CAP_TOKENS));
                m.insert("model".into(), json!(""));
                m.insert("enabled".into(), json!(true));
            }
        }
        m
    }

    pub fn auto_approve_sync(&self) -> bool {
        self.prefs
            .try_read()
            .map(|p| p.auto_approve.unwrap_or(false))
            .unwrap_or(false)
    }

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
                .and_then(|d| {
                    let rec = d.recommended_model.clone()?;
                    Some(if d.name == "openai" { rec } else { format!("{}:{}", d.name, rec) })
                })
                .unwrap_or_else(|| "gpt-5.6-sol".into())
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
            "context_bar": prefs.context_bar,
            "auto_approve": prefs.auto_approve.unwrap_or(false),
            "auto_approve_shadow": prefs.auto_approve_shadow.unwrap_or(false),
            "compaction_threshold_pct": prefs
                .compaction_threshold_pct
                .unwrap_or(ocw_engine::DEFAULT_THRESHOLD_PCT),
            "compaction_cap_tokens": prefs
                .compaction_cap_tokens
                .unwrap_or(ocw_engine::DEFAULT_CAP_TOKENS),
            "compaction_model": prefs.compaction_model.clone().unwrap_or_default(),
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

    pub(crate) fn _model_provider(&self, model: &str) -> String {
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
// SessionRunState — shared per-session engine + run/cancel state
// ---------------------------------------------------------------------------

/// Shared per-session runtime state. Stored in `running_engines` and shared
/// across all WS connections to the same session. Mirrors Python's
/// `SessionManager._engines` + the running/cancel flags that were previously
/// per-WS on `SessionCtx`.
pub struct SessionRunState {
    pub engine: Arc<PlRwLock<Option<ocw_engine::TurnEngine>>>,
    pub running: PlRwLock<bool>,
    pub cancel: Arc<std::sync::Mutex<bool>>,
}

impl SessionRunState {
    pub fn new() -> Self {
        Self {
            engine: Arc::new(PlRwLock::new(None)),
            running: PlRwLock::new(false),
            cancel: Arc::new(std::sync::Mutex::new(false)),
        }
    }
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
    /// Shared engine registry — mirrors Python's `SessionManager._engines`.
    /// When a WS reconnects to a session, it reuses the existing engine + run state
    /// instead of building a new one.
    pub running_engines: Arc<PlRwLock<HashMap<String, Arc<SessionRunState>>>>,
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
    /// LLM auto-title bookkeeping (mirror of `manager.py`'s in-memory counters):
    /// per-session attempt count and the IDs currently generating.
    pub autotitle_attempts: StdShared<HashMap<String, u32>>,
    pub autotitle_inflight: StdShared<HashSet<String>>,
    /// Agent-teams board store + attachment blobs + join tokens (`/v1/board`).
    pub board: Arc<crate::teams::BoardServices>,
    /// Project names + session bindings (UX-044).
    pub project_store: Arc<crate::projects::ProjectStore>,
    /// Memory on/off + user rules (MEMORY-SPEC).
    pub memory_settings: Arc<crate::projects::MemorySettingsStore>,
}

/// The auto-title system prompt (verbatim mirror of `manager.py::_AUTOTITLE_PROMPT`).
const AUTOTITLE_PROMPT: &str = "You title chat sessions. Given the user's opening message(s), \
reply with ONLY a 4-5 word title for the session — no quotes or punctuation wrapping it. If \
the opening is merely a greeting or small-talk with no topic (\"hey\", \"how are you\", \"hi \
there\"), reply with exactly: small-talk";

/// Sanitize a generated title: surrounding quotes off, whitespace collapsed, capped at 60.
/// Returns None for empty/oversized titles or the small-talk sentinel — the sentinel leaves
/// auto_title unset so the next turn's retry can run.
fn sanitize_autotitle(raw: &str) -> Option<String> {
    let stripped = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '\u{201c}' | '\u{201d}' | '\u{2018}' | '\u{2019}'));
    let title: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    // Sentinel tolerance: models riff on the exact token ("Small talk.", quoted, trailing
    // period) — normalize before comparing, else the riff becomes the title.
    let norm: String = title
        .to_lowercase()
        .trim_matches(|c| matches!(c, '.' | '!' | ',' | ';' | ':' | '\'' | '"'))
        .chars()
        .map(|c| if c == ' ' || c == '_' { '-' } else { c })
        .collect();
    if norm == "small-talk" || norm == "smalltalk" {
        return None;
    }
    if title.is_empty() || title.chars().count() > 80 {
        return None;
    }
    Some(title.chars().take(60).collect())
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

        let state = Self {
            config,
            provider,
            memory_store: Arc::new(MemoryStore::new()),
            conversation_store: Arc::new(conversation_store),
            sessions: Arc::new(StdRwLock::new(sessions)),
            session_messages: Arc::new(StdRwLock::new(HashMap::new())),
            running_engines: Arc::new(PlRwLock::new(HashMap::new())),
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
            autotitle_attempts: Arc::new(StdRwLock::new(HashMap::new())),
            autotitle_inflight: Arc::new(StdRwLock::new(HashSet::new())),
            board: Arc::new(crate::teams::BoardServices::open(&data_dir)),
            project_store: Arc::new(crate::projects::ProjectStore::open(&data_dir)),
            memory_settings: Arc::new(crate::projects::MemorySettingsStore::open(
                data_dir.join("memory-settings.json"),
            )),
        };
        if state.settings.effective_default_model_cached().is_empty() {
            let _ = state.default_model_or_configured();
        }
        state
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
    /// Includes agent instructions, narration guidance, environment snapshot, AGENTS.md,
    /// memory guidance + entries, skill catalog, MCP tools, and connector tools.
    /// Mirrors the assembly order of `coworker/agent.py::build_engine`.
    pub async fn build_system_messages(
        &self,
        agent: &str,
        workspace: &str,
        _model: &str,
    ) -> Vec<ocw_engine::Message> {
        let mut messages = Vec::new();

        let base_instructions = crate::agents::get_agent(agent).system_prompt;

        // Inject memory entries
        let memory_text = self.format_memory_for_prompt(workspace);

        // Inject skill catalog
        let skill_text = self.format_skills_for_prompt(workspace);

        // Inject MCP tool descriptions (cached only — does not block on connect)
        let mcp_text = self.format_mcp_tools_for_prompt().await;

        // Inject connector tool descriptions
        let connector_text = self.format_connector_tools_for_prompt();

        let mut system = String::new();
        system.push_str(&base_instructions);
        system.push_str("\n\n");
        system.push_str(NARRATION_GUIDANCE);
        // Always inject the current date — even without a workspace (Chat agent).
        // The per-turn <system-context> provides the most current date; this baseline
        // ensures the model has at least a session-start anchor.
        let today = chrono::Local::now().format("%Y-%m-%d");
        system.push_str(&format!("\n\nCurrent date: {today}"));
        if !workspace.trim().is_empty() {
            // Environment snapshot (workspace/platform/date/git) + folder-scope warning —
            // mirror of `agent.py`'s `environment_context(ws)`.
            system.push_str("\n\n");
            system.push_str(&crate::environment::environment_context(workspace).await);
            // AGENTS.md conventions (global + project root) — mirror of
            // `agent.py`'s `conventions = load_agents_md(ws)` branch.
            let conventions = crate::project::load_agents_md(workspace, &self.config.data_dir);
            if !conventions.is_empty() {
                system.push_str("\n\n");
                system.push_str(&conventions);
            }
        }
        system.push_str("\n\n");
        system.push_str(MEMORY_GUIDANCE);
        if !memory_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&memory_text);
        }
        if !skill_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&skill_text);
        }
        if !mcp_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&mcp_text);
        }
        if !connector_text.is_empty() {
            system.push_str("\n\n");
            system.push_str(&connector_text);
        }

        messages.push(ocw_engine::Message::system(system));
        messages
    }

    /// Format MCP server tools for the system prompt.
    async fn format_mcp_tools_for_prompt(&self) -> String {
        let servers = self.mcp_store.list();
        if servers.is_empty() {
            return String::new();
        }
        let mut blocks = Vec::new();
        for s in &servers {
            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            if let Some(tools) = self.mcp_runtime.cached_tools(name).await {
                if tools.is_empty() {
                    continue;
                }
                let tool_lines: Vec<String> = tools
                    .iter()
                    .map(|t| format!("- {}: {}", t.name, t.description))
                    .collect();
                blocks.push(format!(
                    "MCP server \"{name}\" tools:\n{}",
                    tool_lines.join("\n")
                ));
            }
        }
        if blocks.is_empty() {
            return String::new();
        }
        format!(
            "Connected MCP servers (use mcp__{{server}}__{{tool}} to call):\n{blocks}",
            blocks = blocks.join("\n\n")
        )
    }

    /// Format connected connector tools for the system prompt.
    fn format_connector_tools_for_prompt(&self) -> String {
        let descriptors = self.connector_store.list_descriptors();
        let connected: Vec<_> = descriptors
            .iter()
            .filter(|d| self.connector_store.is_connected(&d.name))
            .collect();
        if connected.is_empty() {
            return String::new();
        }
        let mut blocks = Vec::new();
        for d in &connected {
            if d.instructions.is_empty() {
                continue;
            }
            let lines: Vec<String> = d.instructions.iter().map(|i| format!("- {i}")).collect();
            blocks.push(format!("{}:\n{}", d.title, lines.join("\n")));
        }
        if blocks.is_empty() {
            return String::new();
        }
        format!(
            "Connected services (you can interact with these):\n{}",
            blocks.join("\n\n")
        )
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

    /// Effective default model — mirrors `get_settings()` priority:
    /// 1. prefs.default_model (GUI settings)
    /// 2. config.default_model (config.toml / COWORKER_MODEL)
    /// 3. first configured provider's recommended model
    pub(crate) fn default_model_or_configured(&self) -> String {
        match self.settings.read_default_model_prefs() {
            PrefsDefaultRead::Value(ref m) if !m.is_empty() => {
                self.settings.set_effective_default_model_cache(m);
                return m.clone();
            }
            PrefsDefaultRead::Contended => {
                let cached = self.settings.effective_default_model_cached();
                if !cached.is_empty() {
                    return cached;
                }
            }
            PrefsDefaultRead::Value(_) => {}
        }
        if !self.config.default_model.is_empty() {
            let m = self.config.default_model.clone();
            self.settings.set_effective_default_model_cache(&m);
            return m;
        }
        let m = self.first_configured_provider_model();
        self.settings.set_effective_default_model_cache(&m);
        m
    }

    /// First provider with a usable API key and its recommended model id.
    fn first_configured_provider_model(&self) -> String {
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
            .and_then(|d| {
                let rec = d.recommended_model.clone()?;
                Some(if d.name == "openai" {
                    rec
                } else {
                    format!("{}:{}", d.name, rec)
                })
            })
            .unwrap_or_else(|| "gpt-5.6-sol".into())
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
            compaction: None,
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

    /// Snapshot of a session's messages — in-memory cache first, JSONL fallback when the
    /// cache is empty (typical right after a restart until the first post-restart push
    /// repopulates the entry). JSONL is the authoritative source: when it has more messages
    /// than the in-memory cache, the cache is incomplete and JSONL wins.
    fn messages_snapshot(&self, session_id: &str) -> Vec<Value> {
        let in_mem = self
            .session_messages
            .read()
            .unwrap()
            .get(session_id)
            .map(|d| d.messages.clone())
            .unwrap_or_default();
        let from_disk = self.conversation_store.read_jsonl(session_id);
        // JSONL is the authoritative source: prefer it when it has more messages
        // than the in-memory cache (which may be incomplete after a restart).
        if from_disk.len() > in_mem.len() {
            return from_disk;
        }
        if !in_mem.is_empty() {
            return in_mem;
        }
        from_disk
    }

    /// Post-turn persistence — mirror of Python's `_persist_session`: the first line of the
    /// first user message becomes the title snapshot, and message_count/updated_at keep the
    /// sidebar order live. Uses the targeted `update_turn_meta` — the whole-row replace in
    /// `save()` would clobber auto_title/renamed.
    ///
    /// Also ensures the JSONL file is up-to-date (safety net — the ws/scheduler spawn blocks
    /// write JSONL directly, but this covers edge cases like restarts).
    pub fn persist_turn(&self, session_id: &str) {
        let messages = self.messages_snapshot(session_id);

        // Ensure JSONL has every message (defence-in-depth — the spawn blocks
        // also write JSONL directly, but a restart or timing gap could leave
        // the file behind the in-memory cache).
        let existing = self.conversation_store.count_jsonl(session_id);
        if messages.len() > existing {
            let values: Vec<Value> = messages[existing..].to_vec();
            let _ = self.conversation_store.append_jsonl(session_id, &values);
        }

        let n_msgs = messages.len() as i64;

        let (renamed, auto_title) = self
            .conversation_store
            .title_state(session_id)
            .ok()
            .flatten()
            .unwrap_or((false, None));

        let first_line = ConversationStore::title_from_messages(&messages);
        // In-memory meta backs `/v1/sessions`: keep its title at the display precedence
        // (renamed > auto_title > first-line snapshot). A renamed session keeps whatever
        // `patch_session` stored.
        let display = (!renamed).then(|| auto_title.unwrap_or_else(|| first_line.clone()));
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        {
            let mut sessions = self.sessions.write().unwrap();
            if let Some(meta) = sessions.get_mut(session_id) {
                meta.message_count = n_msgs;
                meta.updated_at = Some(now);
                if let Some(t) = display {
                    meta.title = Some(t);
                }
            }
        }
        // The title column only moves while un-renamed, so a manual rename survives
        // later turns (Python's save() COALESCEs against the existing title).
        let sql_title = (!renamed).then_some(first_line.as_str());
        let _ = self
            .conversation_store
            .update_turn_meta(session_id, sql_title, n_msgs);
    }

    /// Persist session grants to the SQLite row so approved tools survive a
    /// restart. Called after every turn from ws.rs / scheduler.rs.
    pub fn persist_grants(&self, session_id: &str, grants: &Value) {
        let _ = self.conversation_store.update_grants(session_id, grants);
    }

    /// Persist compaction state (OPE-27) after a turn.
    pub fn persist_compaction(&self, session_id: &str, compaction: Option<&Value>) {
        let _ = self
            .conversation_store
            .update_compaction(session_id, compaction);
    }

    // -- LLM auto-titles (FB-010, mirror of `manager.py::_maybe_autotitle`) ----

    /// Kick off title generation after a turn completes, fire-and-forget. Only while the
    /// session has neither a manual rename nor a generated title, at most twice: attempt 1
    /// rides turn 1, and the second window exists solely for the small-talk retry.
    pub fn maybe_autotitle(&self, session_id: &str) {
        // Automation runs (`__run__*`) are titled by their task; internal sessions skip.
        if session_id.starts_with("__") {
            return;
        }
        if self.autotitle_inflight.read().unwrap().contains(session_id) {
            return;
        }
        let attempts = self
            .autotitle_attempts
            .read()
            .unwrap()
            .get(session_id)
            .copied()
            .unwrap_or(0);
        if attempts >= 2 {
            return;
        }
        let Some((renamed, auto_title)) = self
            .conversation_store
            .title_state(session_id)
            .ok()
            .flatten()
        else {
            return;
        };
        if renamed || auto_title.is_some() {
            return;
        }
        let model = self
            .sessions
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.model.clone())
            .unwrap_or_default();
        if model.is_empty() {
            return;
        }
        // The first two user openers feed the titling call.
        let openers: Vec<String> = self
            .messages_snapshot(session_id)
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
            .filter_map(|m| {
                let text = crate::attachments::content_to_text(
                    m.get("content").unwrap_or(&Value::Null),
                    "",
                )
                .trim()
                .to_string();
                (!text.is_empty()).then_some(text)
            })
            .take(2)
            .collect();
        if openers.is_empty() {
            return;
        }
        self.autotitle_attempts
            .write()
            .unwrap()
            .insert(session_id.to_string(), attempts + 1);
        self.autotitle_inflight
            .write()
            .unwrap()
            .insert(session_id.to_string());

        let state = self.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            // One cheap non-streaming completion on the session's own provider/model.
            // Every failure is swallowed — the first-line snapshot stays.
            let provider = state.provider.clone();
            let turn = tokio::task::spawn_blocking(move || {
                provider.complete(
                    &model,
                    vec![
                        json!({"role": "system", "content": AUTOTITLE_PROMPT}),
                        json!({"role": "user", "content": openers.join("\n\n")}),
                    ],
                    None,
                    json!({"temperature": 0.2, "max_tokens": 64, "reasoning_effort": "none"}),
                )
            })
            .await
            .ok()
            .and_then(|r| r.ok());

            if let Some(title) = turn
                .as_ref()
                .and_then(|t| t.text.as_deref())
                .and_then(sanitize_autotitle)
            {
                if state
                    .conversation_store
                    .set_auto_title(&sid, &title)
                    .unwrap_or(false)
                {
                    // The store guard (renamed=0) passed, so the display title may move.
                    {
                        let mut sessions = state.sessions.write().unwrap();
                        if let Some(meta) = sessions.get_mut(&sid) {
                            meta.title = Some(title.clone());
                        }
                    }
                    // Best-effort nudge for live viewers; the sidebar's poll picks the
                    // new title up regardless.
                    state.broadcast_sync(
                        &sid,
                        json!({"type": "session_title", "data": {"session_id": sid, "title": title}}),
                    );
                }
            }
            state.autotitle_inflight.write().unwrap().remove(&sid);
        });
    }

    /// Get an existing session or create a new one with a specific id.
    /// Used by automation `__run__` sessions that need an explicit ID.
    /// When `model` is set it is used for new sessions (automation runs pin the
    /// resolved model at creation time); otherwise `default_model_or_configured()`.
    pub fn get_or_create_session(
        &self,
        session_id: &str,
        agent: &str,
        workspace: Option<&str>,
        model: Option<&str>,
    ) -> SessionMeta {
        // Normalize "/" (a polluted legacy workspace value persisted by older sessions) to
        // None everywhere, so it never lands in a new SessionMeta and the legacy-adoption
        // branch below sees it as empty.
        let workspace = workspace.filter(|w| !w.trim().is_empty() && *w != "/");
        {
            let existing = self.sessions.read().unwrap().get(session_id).cloned();
            if let Some(s) = existing {
                let ws_empty = s
                    .workspace
                    .as_deref()
                    .map_or(true, |w| w.trim().is_empty() || w == "/");
                // Defensive: legacy rows persisted with an empty workspace (pre
                // scratch-provision fix) come back as None after a restart — adopt
                // the caller's workspace so artifact reads don't fail with
                // "no workspace". Persist via the message snapshot so the JSONL
                // history is preserved (an empty `messages` slice would rewrite
                // and shrink it).
                if ws_empty {
                    if let Some(provided) = workspace.filter(|w| !w.trim().is_empty()) {
                        let mut sessions = self.sessions.write().unwrap();
                        if let Some(meta) = sessions.get_mut(session_id) {
                            meta.workspace = Some(provided.to_string());
                            let msgs = self.messages_snapshot(session_id);
                            let _ = self.persist_session_meta(meta, &msgs);
                            return meta.clone();
                        }
                    }
                }
                return s;
            }
        }
        let model = model
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.default_model_or_configured());
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
        // Persist to SQLite — mirror of `create_session`. Without this row the
        // session lives only in memory and vanishes from `/v1/sessions` on the
        // next restart (the WS connect path is the most common creator, and
        // `list_sessions` reads from the in-memory HashMap that startup
        // rebuilds from SQLite). Side-effect of the miss: `session_exists`
        // returns false on restart, `getSessionMessages` 404s, and the GUI's
        // `selectSession` catch-all clears the transcript.
        let _ = self.persist_session_meta(&meta, &[]);
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
            // Persist the rename (sets renamed=1 so first-line snapshots and auto-titles
            // stop displacing it) — mirror of `manager.py::rename_session`.
            let _ = self.conversation_store.rename(session_id, t);
            meta.title = Some(t.to_string());
        }
        if pinned.is_some() || archived.is_some() {
            let _ = self
                .conversation_store
                .set_flags(session_id, pinned, archived);
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
        self.running_engines.write().remove(session_id);
        let _ = self.conversation_store.delete(session_id);
    }

    pub async fn list_messages(&self, session_id: &str) -> Vec<Value> {
        // Check running engine first — mirrors Python's `session_messages`
        // which prefers the live engine's in-memory thread over persisted records.
        // During a turn the engine is taken out, so this falls through to JSONL —
        // mid-turn checkpoints keep JSONL current.
        if let Some(run) = self.running_engines.read().get(session_id) {
            if let Some(engine) = run.engine.read().as_ref() {
                let msgs: Vec<Value> = engine
                    .messages()
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or_default())
                    .collect();
                if !msgs.is_empty() {
                    return msgs;
                }
            }
        }

        let in_mem = self
            .session_messages
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.messages.clone())
            .unwrap_or_default();

        // Always check JSONL as the authoritative source: the in-memory cache
        // may have been populated with partial data by push_message_sync before
        // the full history was loaded (race after a restart).
        let from_disk = self.conversation_store.read_jsonl(session_id);

        if from_disk.len() > in_mem.len() {
            // JSONL has more messages — it is the authoritative source.
            // Repopulate the in-memory cache with the full history so
            // subsequent reads are O(1) and push_message_sync appends to
            // the complete list.
            let mut guard = self.session_messages.write().unwrap();
            guard.insert(
                session_id.to_string(),
                SessionMessages {
                    messages: from_disk.clone(),
                },
            );
            return from_disk;
        }

        if !in_mem.is_empty() {
            return in_mem;
        }

        // Both empty — may be a brand-new session with no messages yet.
        from_disk
    }

    /// Persist engine messages to JSONL via the shared message mirror.
    /// Called from the live event pump at checkpoint events during a turn
    /// (mirrors Python's `manager.save(session_id, engine)` at checkpoints).
    pub fn checkpoint_engine_messages(
        &self,
        session_id: &str,
        mirror: &Arc<StdRwLock<Vec<ocw_engine::Message>>>,
    ) {
        let messages: Vec<Value> = mirror
            .read()
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect();
        self.persist_engine_messages_inner(session_id, &messages);
    }

    /// Core persistence: write engine messages to JSONL + the in-memory cache.
    /// Mirror of Python's `manager.save(session_id, engine)`. Shared by the
    /// normal turn path, retry path, checkpoint path, and panic guard.
    pub fn persist_engine_messages_inner(&self, session_id: &str, all_messages: &[Value]) {
        let existing = self.conversation_store.count_jsonl(session_id);
        if all_messages.len() > existing {
            // If the JSONL is missing the system message that the engine has
            // (legacy files from before the format fix), do a full rewrite
            // so system lands at position 0 — matching Python.
            let disk_missing_system = if existing > 0 {
                let disk = self.conversation_store.read_jsonl(session_id);
                !disk
                    .first()
                    .and_then(|v| v.get("role"))
                    .and_then(|v| v.as_str())
                    .map(|r| r == "system")
                    .unwrap_or(false)
            } else {
                false // new file: system will be written in first append
            };

            if disk_missing_system {
                if let Err(e) = self
                    .conversation_store
                    .rewrite_jsonl(session_id, all_messages)
                {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Failed to rewrite JSONL with system message"
                    );
                }
            } else if let Err(e) = self
                .conversation_store
                .append_jsonl(session_id, &all_messages[existing..])
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to persist messages to JSONL"
                );
            }
        }
        // Sync the in-memory cache with the authoritative engine message list.
        // Never let a shorter snapshot (e.g. an incomplete turn-delta mirror)
        // clobber a longer cache — that produced orphan tool_calls histories.
        if let Ok(mut guard) = self.session_messages.write() {
            let entry = guard.entry(session_id.to_string()).or_default();
            if all_messages.len() >= entry.messages.len() {
                entry.messages = all_messages.to_vec();
            }
        }
    }

    pub async fn push_message(&self, session_id: &str, msg: Value) {
        {
            // Create the entry on first touch — mirror of push_message_sync.
            // Previously the in-memory push was silently dropped when no entry
            // existed (typical after a restart).
            let mut guard = self.session_messages.write().unwrap();
            guard
                .entry(session_id.to_string())
                .or_default()
                .messages
                .push(msg.clone());
        }
        // Append to JSONL for persistence
        if let Err(e) = self.conversation_store.append_jsonl(session_id, &[msg]) {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to persist message to JSONL — data will be lost on restart"
            );
        }
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

    /// Sync variant of `list_messages` with JSONL fallback — mirror of the async version.
    pub fn list_messages_sync(&self, session_id: &str) -> Vec<Value> {
        let in_mem = self
            .session_messages
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.messages.clone())
            .unwrap_or_default();

        // JSONL is the authoritative source: fall back when the in-memory cache
        // is empty or incomplete (typical after a restart).
        let from_disk = self.conversation_store.read_jsonl(session_id);
        if from_disk.len() > in_mem.len() {
            let mut guard = self.session_messages.write().unwrap();
            guard.insert(
                session_id.to_string(),
                SessionMessages {
                    messages: from_disk.clone(),
                },
            );
            return from_disk;
        }
        if !in_mem.is_empty() {
            return in_mem;
        }
        from_disk
    }

    pub fn delete_session_sync(&self, session_id: &str) {
        self.sessions.write().unwrap().remove(session_id);
        self.session_messages.write().unwrap().remove(session_id);
        self.ws_sessions.write().unwrap().remove(session_id);
        self.running_engines.write().remove(session_id);
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
        {
            // Create the entry on first touch — previously the in-memory push was silently
            // dropped when no entry existed (typical after a restart), which left persist_turn
            // and the auto-title openers looking at an empty message list.
            let mut guard = self.session_messages.write().unwrap();
            guard
                .entry(session_id.to_string())
                .or_default()
                .messages
                .push(msg.clone());
        }
        if let Err(e) = self.conversation_store.append_jsonl(session_id, &[msg]) {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to persist message to JSONL — data will be lost on restart"
            );
        }
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

        // Dot-less lowercase suffix set (mirrors Python's `path.suffix.lower()`
        // whitelist, which is case-insensitive).
        let suffixes: HashSet<String> = [
            "md",
            "markdown",
            "html",
            "htm",
            "txt",
            "json",
            "csv",
            "tsv",
            "py",
            "js",
            "ts",
            "tsx",
            "css",
            "png",
            "jpg",
            "jpeg",
            "webp",
            "gif",
            "pdf",
            "xlsx",
            "xls",
            "pptx",
            "ppt",
            "pptm",
            "docx",
            "doc",
            "docm",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let skip_dirs: HashSet<&str> = ["node_modules", "target", "dist", "__pycache__", ".git"]
            .iter()
            .copied()
            .collect();

        // Recursive walk (mirrors Python's `root.rglob("*")` — the previous
        // read_dir-only version missed every file under a subdirectory).
        let mut artifacts: Vec<_> = walk_files(&root, &skip_dirs)
            .into_iter()
            .filter_map(|path| {
                let rel = path.strip_prefix(&root).ok()?;
                let ext = path.extension()?.to_str()?.to_lowercase();
                if !suffixes.contains(&ext) {
                    return None;
                }
                let meta = path.metadata().ok()?;
                let size = meta.len();
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                Some(serde_json::json!({
                    "path": rel.to_string_lossy(),
                    "abs_path": path.to_string_lossy(),
                    "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "kind": artifact_kind(&path),
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

        let ext = target
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // Classify by lowercased extension (mirrors Python `_artifact_kind` /
        // `manager.read_artifact`). `artifact_kind` lowercases, so `PPTX` etc.
        // classify correctly instead of falling into the text branch.
        let kind = artifact_kind(&target);
        if kind == "office" {
            // PowerPoint/Word binaries can't be previewed inline; the UI offers
            // "Open in default app" instead of trying to render them.
            return json!({"ok": true, "path": path, "kind": "office"});
        }

        if kind == "image" || kind == "pdf" || kind == "sheet" {
            let Ok(data) = std::fs::read(&target) else {
                return json!({"ok": false, "error": "failed to read file"});
            };
            if data.len() > 25 * 1024 * 1024 {
                return json!({"ok": false, "error": "file too large to preview"});
            }
            let mime = match ext.as_str() {
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
                "kind": kind,
                "data_url": format!("data:{mime};base64,{b64}"),
            });
        }

        match std::fs::read_to_string(&target) {
            Ok(text) => {
                let truncated = text.len() > 500_000;
                json!({
                    "ok": true,
                    "path": path,
                    "kind": kind, // real kind: markdown/csv/code/html/text
                    // clip_utf8 rounds down to a char boundary — a raw byte slice
                    // panics on CJK content (owner-hit 2026-08-08 panic class).
                    "content": if truncated { ocw_data::clip_utf8(&text, 500_000) } else { &text },
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

/// Recursively collect files under `root` (stack DFS). Skips any entry whose name
/// starts with `.` (hidden) or is in `skip_dirs` — mirrors Python's
/// `any(part.startswith(".") for part in rel.parts)` skip logic.
fn walk_files(root: &Path, skip_dirs: &HashSet<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || skip_dirs.contains(name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// File-kind classification for artifact previews/iconography.
/// Mirror of `coworker/server/manager.py::_artifact_kind`.
fn artifact_kind(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match format!(".{ext}").as_str() {
        ".md" | ".markdown" => "markdown",
        ".html" | ".htm" => "html",
        ".png" | ".jpg" | ".jpeg" | ".webp" | ".gif" => "image",
        ".pdf" => "pdf",
        ".xlsx" | ".xls" => "sheet",
        ".pptx" | ".ppt" | ".pptm" | ".docx" | ".doc" | ".docm" => "office",
        ".csv" | ".tsv" => "csv",
        ".py" | ".js" | ".ts" | ".tsx" | ".css" | ".json" => "code",
        _ => "text",
    }
}

/// Files in the task workspace modified during the run — the run's artifacts.
/// Mirror of `coworker/server/manager.py::_recent_files` (hidden paths skipped,
/// `mtime >= since - 1`, capped at `limit`).
pub(crate) fn recent_files(workspace: &str, since: f64, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let root = PathBuf::from(workspace);
    if !root.is_dir() {
        return out;
    }
    let no_skip_dirs: HashSet<&str> = HashSet::new();
    for path in walk_files(&root, &no_skip_dirs) {
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                let secs = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if secs >= since - 1.0 {
                    if let Ok(rel) = path.strip_prefix(&root) {
                        out.push(rel.to_string_lossy().into_owned());
                    }
                }
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    out
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

    fn make_state_with(
        dir: PathBuf,
        provider: Arc<dyn ocw_provider::Provider>,
        default_model: &str,
    ) -> AppState {
        let config = Config {
            data_dir: dir,
            default_model: default_model.to_string(),
            ..Config::default()
        };
        AppState::new(config, provider)
    }

    /// Fake provider for auto-title tests — every completion returns the same text.
    struct TitleProvider(&'static str);
    impl ocw_provider::Provider for TitleProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: Vec<Value>,
            _tools: Option<Vec<Value>>,
            _settings: Value,
        ) -> Result<ocw_provider::AssistantTurn, ocw_provider::Error> {
            Ok(ocw_provider::AssistantTurn {
                text: Some(self.0.to_string()),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                reasoning: None,
                usage: None,
            })
        }
        fn capabilities(&self, _model: &str) -> ocw_provider::ModelCapabilities {
            ocw_provider::ModelCapabilities::default()
        }
        fn name(&self) -> &str {
            "fake"
        }
    }

    #[tokio::test]
    async fn default_model_or_configured_prefers_prefs_over_config() {
        let dir = temp_data_dir("prefs-default-model");
        let state = make_state_with(
            dir,
            Arc::new(ocw_provider::Router::new("anthropic")),
            "deepseek:deepseek-v4-flash",
        );
        state
            .settings
            .set_default_model("anthropic:claude-sonnet-4-6".into())
            .await;
        assert_eq!(
            state.default_model_or_configured(),
            "anthropic:claude-sonnet-4-6"
        );
    }

    #[tokio::test]
    async fn startup_preserves_minimax_default_when_key_in_secrets() {
        let dir = temp_data_dir("minimax-default-persist");
        std::fs::write(
            dir.join("prefs.json"),
            r#"{"default_model":"minimax:MiniMax-M2.5"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("secrets.json"),
            r#"{"provider:minimax":{"api_key":"test-key"}}"#,
        )
        .unwrap();
        let settings = SettingsManager::open_for_tests(dir);
        assert_eq!(
            settings.get_default_model().await,
            "minimax:MiniMax-M2.5"
        );
    }

    #[tokio::test]
    async fn get_or_create_session_pins_explicit_model() {
        let dir = temp_data_dir("session-explicit-model");
        let state = make_state(dir);
        state
            .settings
            .set_default_model("deepseek:deepseek-v4-flash".into())
            .await;
        let meta = state.get_or_create_session(
            "run-explicit-model",
            "cowork",
            None,
            Some("minimax:MiniMax-M2.5"),
        );
        assert_eq!(meta.model, "minimax:MiniMax-M2.5");
    }

    #[tokio::test]
    async fn set_default_model_updates_effective_cache() {
        let dir = temp_data_dir("default-model-cache");
        let state = make_state(dir);
        state
            .settings
            .set_default_model("minimax:MiniMax-M2.5".into())
            .await;
        assert_eq!(
            state.default_model_or_configured(),
            "minimax:MiniMax-M2.5"
        );
        assert_eq!(
            state.settings.effective_default_model_cached(),
            "minimax:MiniMax-M2.5"
        );
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

    // -- Title persistence (FB-010 parity) ------------------------------------

    #[tokio::test]
    async fn persist_turn_snapshots_first_line_title() {
        let dir = temp_data_dir("persist-turn");
        let state = make_state(dir.clone());
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();

        state.push_message_sync(
            &sid,
            json!({"role": "user", "content": "Fix the login bug\nsecond line"}),
        );
        state.push_message_sync(&sid, json!({"role": "assistant", "content": "On it."}));
        state.persist_turn(&sid);

        let meta = state.get_session(&sid).await.unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix the login bug"));
        assert_eq!(meta.message_count, 2);
        assert!(meta.updated_at.is_some());

        // Restart: the title survives via SQLite.
        let state2 = make_state(dir);
        let restored = state2.get_session(&sid).await.unwrap();
        assert_eq!(restored.title.as_deref(), Some("Fix the login bug"));
        assert_eq!(restored.message_count, 2);
    }

    #[tokio::test]
    async fn rename_persists_and_wins_over_turn_snapshot() {
        let dir = temp_data_dir("rename");
        let state = make_state(dir.clone());
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();

        state.push_message_sync(&sid, json!({"role": "user", "content": "first line snapshot"}));
        state
            .patch_session(&sid, Some("Prod incident 42"), None, None)
            .await
            .unwrap();

        // A later turn must not displace the manual rename.
        state.push_message_sync(&sid, json!({"role": "assistant", "content": "ok"}));
        state.persist_turn(&sid);
        assert_eq!(
            state.get_session(&sid).await.unwrap().title.as_deref(),
            Some("Prod incident 42")
        );
        assert!(state.conversation_store.load(&sid).unwrap().unwrap().renamed);

        // Restart keeps the rename.
        let state2 = make_state(dir);
        assert_eq!(
            state2.get_session(&sid).await.unwrap().title.as_deref(),
            Some("Prod incident 42")
        );
    }

    async fn wait_for_auto_title(state: &AppState, sid: &str) -> Option<String> {
        for _ in 0..200 {
            if let Ok(Some((_, Some(t)))) = state.conversation_store.title_state(sid) {
                return Some(t);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        state
            .conversation_store
            .title_state(sid)
            .ok()
            .flatten()
            .and_then(|(_, t)| t)
    }

    #[tokio::test]
    async fn maybe_autotitle_stores_generated_title() {
        let dir = temp_data_dir("autotitle");
        let state = make_state_with(
            dir,
            Arc::new(TitleProvider("Login Bug Investigation")),
            "test-model",
        );
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();
        state.push_message_sync(&sid, json!({"role": "user", "content": "the login page 500s"}));

        state.maybe_autotitle(&sid);
        assert_eq!(
            wait_for_auto_title(&state, &sid).await.as_deref(),
            Some("Login Bug Investigation")
        );
        assert_eq!(
            state.get_session(&sid).await.unwrap().title.as_deref(),
            Some("Login Bug Investigation")
        );
    }

    #[tokio::test]
    async fn maybe_autotitle_small_talk_retries_then_gives_up() {
        let dir = temp_data_dir("smalltalk");
        let state = make_state_with(dir, Arc::new(TitleProvider("small-talk")), "test-model");
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();
        state.push_message_sync(&sid, json!({"role": "user", "content": "hey"}));

        state.maybe_autotitle(&sid); // attempt 1 — sentinel, no title
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(state
            .conversation_store
            .title_state(&sid)
            .unwrap()
            .unwrap()
            .1
            .is_none());

        state.maybe_autotitle(&sid); // attempt 2 — the single retry
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        state.maybe_autotitle(&sid); // guarded: attempts exhausted
        assert_eq!(
            state.autotitle_attempts.read().unwrap().get(&sid).copied(),
            Some(2)
        );
        assert!(state
            .conversation_store
            .title_state(&sid)
            .unwrap()
            .unwrap()
            .1
            .is_none());
    }

    #[tokio::test]
    async fn maybe_autotitle_never_displaces_a_rename() {
        let dir = temp_data_dir("autotitle-rename");
        let state = make_state_with(dir, Arc::new(TitleProvider("sneaky")), "test-model");
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();
        state.push_message_sync(&sid, json!({"role": "user", "content": "the login page 500s"}));
        state
            .patch_session(&sid, Some("My Thread"), None, None)
            .await
            .unwrap();

        state.maybe_autotitle(&sid); // renamed guard stops it before any provider call
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(state
            .conversation_store
            .title_state(&sid)
            .unwrap()
            .unwrap()
            .1
            .is_none());
        assert_eq!(
            state.get_session(&sid).await.unwrap().title.as_deref(),
            Some("My Thread")
        );
    }

    #[test]
    fn sanitize_autotitle_rules() {
        assert_eq!(
            sanitize_autotitle("\"Morning   Briefing\""),
            Some("Morning Briefing".to_string())
        );
        assert_eq!(sanitize_autotitle("small-talk"), None);
        assert_eq!(sanitize_autotitle("Small talk."), None);
        assert_eq!(sanitize_autotitle(""), None);
        assert_eq!(sanitize_autotitle(&"x".repeat(81)), None);
        assert_eq!(sanitize_autotitle(&"y".repeat(80)), Some("y".repeat(60)));
    }

    // -- JSONL fallback when the in-memory cache is empty post-restart -------

    #[tokio::test]
    async fn list_messages_falls_back_to_jsonl_after_restart() {
        let dir = temp_data_dir("restart-msg");
        let state = make_state(dir.clone());
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();
        state.push_message_sync(&sid, json!({"role": "user", "content": "first user line"}));
        state.push_message_sync(&sid, json!({"role": "assistant", "content": "hello"}));

        // Fresh AppState simulates a restart: session_messages is empty, only JSONL persists.
        let state2 = make_state(dir);
        assert!(state2
            .session_messages
            .read()
            .unwrap()
            .get(&sid)
            .is_none());
        let msgs = state2.list_messages(&sid).await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("user"));
        // The fallback repopulates the in-memory cache so subsequent reads are O(1).
        assert_eq!(
            state2.session_messages.read().unwrap().get(&sid).map(|d| d.messages.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn push_message_sync_creates_entry_when_missing() {
        let dir = temp_data_dir("push-missing");
        let state = make_state(dir);
        // No create_session: session_messages has no entry for this id yet.
        let fake_id = "fake-session-id";
        assert!(state.session_messages.read().unwrap().get(fake_id).is_none());

        state.push_message_sync(fake_id, json!({"role": "user", "content": "hi"}));
        assert_eq!(
            state
                .session_messages
                .read()
                .unwrap()
                .get(fake_id)
                .map(|d| d.messages.len()),
            Some(1)
        );
        // And of course JSONL persisted.
        let from_disk = state.conversation_store.read_jsonl(fake_id);
        assert_eq!(from_disk.len(), 1);
    }

    // Regression: `get_or_create_session` (the WS-connect / scheduler /
    // automation path) used to only insert into the in-memory HashMap. After a
    // restart the row was missing from SQLite, `list_sessions` returned an
    // empty list, and clicking a Rust-created session title 404'd, which the
    // GUI's `selectSession` catch-all translated into `setItems([])`.
    #[tokio::test]
    async fn get_or_create_session_persists_to_sqlite() {
        let dir = temp_data_dir("get-or-create-persist");
        let state = make_state(dir.clone());
        let _ = state.get_or_create_session("abc-123", "code", None, None);

        // Fresh AppState simulates a restart: session_messages is empty and
        // `self.sessions` is rebuilt from SQLite via conversation_store.list.
        let state2 = make_state(dir);
        assert!(
            state2.session_exists("abc-123").await,
            "session created via get_or_create_session must survive a restart"
        );
        let sessions = state2.list_sessions(None).await;
        assert!(
            sessions.iter().any(|s| s.session_id == "abc-123"),
            "list_sessions should include the session after restart; got {sessions:?}"
        );
    }

    #[tokio::test]
    async fn persist_turn_uses_jsonl_after_restart() {
        let dir = temp_data_dir("persist-restart");
        let state = make_state(dir.clone());
        let meta = state.create_session(None, "code").await;
        let sid = meta.session_id.clone();
        state.push_message_sync(&sid, json!({"role": "user", "content": "Restart title works"}));

        // Fresh AppState — in-memory cache empty.
        let state2 = make_state(dir);
        assert!(state2
            .session_messages
            .read()
            .unwrap()
            .get(&sid)
            .is_none());

        state2.persist_turn(&sid);
        assert_eq!(
            state2.get_session(&sid).await.unwrap().title.as_deref(),
            Some("Restart title works")
        );
        assert_eq!(state2.get_session(&sid).await.unwrap().message_count, 1);
    }

    /// The cowork system prompt must carry the same blocks the Python engine assembles
    /// (`coworker/agent.py::build_engine`): narration guidance, the environment snapshot
    /// with folder scope, memory guidance, and the full outcome-oriented instructions.
    #[tokio::test]
    async fn build_system_messages_includes_guidance_blocks() {
        let dir = temp_data_dir("sysprompt");
        let state = make_state(dir.clone());
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let messages = state
            .build_system_messages("cowork", workspace.to_str().unwrap(), "test-model")
            .await;
        assert_eq!(messages.len(), 1);
        let ocw_engine::Message::System { content } = &messages[0] else {
            panic!("expected a system message");
        };

        // Base instructions (full Python text, incl. artifact-link rule).
        assert!(content.contains("You are a Cowork agent"));
        assert!(content.contains("artifact:relative/path"));
        assert!(content.contains("no heredocs"));
        // Narration guidance.
        assert!(content.contains("Narration:"));
        // Environment snapshot + folder scope.
        assert!(content.contains("<environment>"));
        assert!(content.contains("Workspace:"));
        assert!(content.contains("Platform:"));
        assert!(content.contains("Session started"));
        assert!(content.contains("per-turn <system-context>"));
        assert!(content.contains("Folder scope:"));
        // Memory guidance.
        assert!(content.contains("Memory:"));
        assert!(content.contains("memory_update"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `recent_files` must mirror Python's `_recent_files`: only files modified
    /// since the run started (mtime window) and no hidden-path entries.
    #[test]
    fn recent_files_filters_hidden_and_since() {
        let dir = temp_data_dir("recent-files");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();

        // Pre-existing file from before the run window. Python's `_recent_files`
        // uses `mtime >= since - 1` (one-second grace), so sleep past that
        // window to make "old" genuinely stale.
        std::fs::write(dir.join("old.md"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // Files produced during the run (one hidden, one in a subdirectory).
        std::fs::write(dir.join("sub").join("new.md"), "new").unwrap();
        std::fs::write(dir.join(".hidden").join("secret.md"), "secret").unwrap();

        let files = recent_files(&dir.to_string_lossy(), since, 20);
        assert_eq!(files, vec!["sub/new.md".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `list_artifacts` must recurse into subdirectories (Python `rglob("*")`)
    /// and classify file kinds by suffix.
    #[test]
    fn list_artifacts_includes_subdirectories() {
        let dir = temp_data_dir("artifacts-subdir");
        let state = make_state(dir.clone());
        std::fs::create_dir_all(dir.join("output")).unwrap();
        std::fs::write(dir.join("output").join("report.md"), "# report").unwrap();
        std::fs::write(dir.join("root.txt"), "root").unwrap();
        std::fs::write(dir.join("output").join("skip.bin"), "bin").unwrap();

        let sid = "artifacts-sub-session".to_string();
        state.get_or_create_session(&sid, "cowork", Some(&dir.to_string_lossy()), None);

        let artifacts = state.list_artifacts(&sid);
        let paths: Vec<String> = artifacts
            .iter()
            .filter_map(|a| a.get("path").and_then(|p| p.as_str()).map(String::from))
            .collect();
        assert!(
            paths.iter().any(|p| p == "output/report.md"),
            "subdirectory file missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "root.txt"),
            "root file missing: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p == "output/skip.bin"),
            "non-whitelisted suffix included: {paths:?}"
        );
        let report = artifacts
            .iter()
            .find(|a| a.get("path").and_then(|p| p.as_str()) == Some("output/report.md"))
            .expect("report artifact present");
        assert_eq!(report.get("kind").and_then(|k| k.as_str()), Some("markdown"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Legacy sessions persisted with an empty workspace (pre scratch-provision
    /// fix) come back as None after a restart — `get_or_create_session` must
    /// adopt the caller's workspace so artifact reads don't fail with
    /// "no workspace".
    #[tokio::test]
    async fn get_or_create_session_adopts_workspace_for_legacy_rows() {
        let dir = temp_data_dir("legacy-ws");
        let state = make_state(dir.clone());
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // Legacy row: session exists with an empty workspace (as created pre-fix).
        let _ = state.get_or_create_session("run-legacy-1", "cowork", None, None);
        // Simulate the restart rebuild path where the empty workspace became None.
        {
            let mut sessions = state.sessions.write().unwrap();
            sessions.get_mut("run-legacy-1").unwrap().workspace = None;
        }

        let meta = state.get_or_create_session(
            "run-legacy-1",
            "cowork",
            Some(&ws.to_string_lossy()),
            None,
        );
        assert_eq!(meta.workspace.as_deref(), Some(ws.to_str().unwrap()));

        // A fresh AppState (restart) must read the adopted workspace back.
        let state2 = make_state(dir.clone());
        let s2 = state2.get_session("run-legacy-1").await.unwrap();
        assert_eq!(s2.workspace.as_deref(), Some(ws.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `get_or_create_session` must treat "/" (a polluted legacy workspace value the
    /// GUI can carry for old sessions) as "no workspace" everywhere: it never lands
    /// in a new SessionMeta, and a legacy row persisted with "/" adopts the caller's
    /// real workspace (same fix as the empty-workspace legacy branch).
    #[tokio::test]
    async fn get_or_create_session_normalizes_slash_workspace() {
        let dir = temp_data_dir("slash-ws");
        let state = make_state(dir.clone());
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // Fresh create with "/" → treated as no workspace.
        let meta = state.get_or_create_session("run-slash-1", "cowork", Some("/"), None);
        assert_eq!(meta.workspace.as_deref(), None);

        // Legacy row persisted with workspace "/" → adopted with the caller's workspace.
        {
            let mut sessions = state.sessions.write().unwrap();
            sessions.get_mut("run-slash-1").unwrap().workspace = Some("/".to_string());
        }
        let meta = state.get_or_create_session(
            "run-slash-1",
            "cowork",
            Some(&ws.to_string_lossy()),
            None,
        );
        assert_eq!(meta.workspace.as_deref(), Some(ws.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `provision_scratch` must allocate `scratch_base/{session_id}` — the Rust
    /// mirror of Python's `_provision_scratch` used when creating automations.
    #[tokio::test]
    async fn provision_scratch_creates_task_workspace() {
        let dir = temp_data_dir("provision");
        let state = make_state(dir.clone());
        state
            .settings
            .set_scratch_base(dir.join("scratch").to_string_lossy().to_string())
            .await;

        let ws = crate::automations::provision_scratch(&state, "__task__t1").await;
        assert!(!ws.is_empty(), "provision_scratch must return a path");
        assert!(std::path::Path::new(&ws).is_dir(), "workspace dir must exist: {ws}");
        assert!(
            ws.contains("scratch") && ws.ends_with("__task__t1"),
            "workspace must be scratch_base/task_session_id: {ws}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `read_artifact` kind semantics (mirror of Python `manager.read_artifact`):
    /// office files → `kind: "office"` (no inline content, GUI offers
    /// "Open in default app"), text files → their real artifact kind
    /// (markdown/csv/...). Extensions must match lowercased (`PPTX` included).
    #[tokio::test]
    async fn read_artifact_kinds_match_python() {
        let dir = temp_data_dir("artifact-kinds");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("deck.PPTX"), b"dummy-pptx").unwrap();
        std::fs::write(ws.join("doc.docx"), b"dummy-docx").unwrap();
        std::fs::write(ws.join("data.csv"), "a,b\n1,2\n").unwrap();
        std::fs::write(ws.join("notes.md"), "# hi\n").unwrap();
        // Canonicalize so the workspace-root containment check inside
        // `read_artifact` matches (macOS temp dirs are symlinked).
        let ws = ws.canonicalize().unwrap();
        let state = make_state(dir.clone());
        let meta = state
            .create_session(Some(ws.to_string_lossy().as_ref()), "code")
            .await;

        // Office (even uppercase extension) → kind "office", no inline content.
        let office = state.read_artifact(&meta.session_id, "deck.PPTX");
        assert_eq!(office["ok"], true);
        assert_eq!(office["kind"], "office");
        assert!(office.get("content").is_none());
        let docx = state.read_artifact(&meta.session_id, "doc.docx");
        assert_eq!(docx["kind"], "office");

        // CSV → real kind (GUI renders the table view).
        let csv = state.read_artifact(&meta.session_id, "data.csv");
        assert_eq!(csv["ok"], true);
        assert_eq!(csv["kind"], "csv");
        assert!(csv["content"].as_str().unwrap().contains("1,2"));

        // Markdown → markdown kind.
        let md = state.read_artifact(&meta.session_id, "notes.md");
        assert_eq!(md["kind"], "markdown");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
