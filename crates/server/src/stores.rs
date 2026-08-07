//! Small durable JSON-backed stores for the server — the Rust counterparts of
//! `coworker/subscriptions.py`, `coworker/unrouted.py`, `coworker/audit.py` and
//! `subscriptions.ChannelBuffer`:
//!
//! - `SubscriptionStore`: persisted `(session_id, channel)` inbound-listening records.
//! - `UnroutedStore`: dead-letter items (inbound messages with no destination +
//!   background-turn failures), capped newest-kept.
//! - `AuditStore`: durable audit log (JSONL append), secret-masked, queryable.
//! - `ChannelBuffer`: last-N messages per channel + display names (the channel
//!   picker's "recently-seen" source).
//! - `BrowserController`: the browser-session state contract. The Rust server has
//!   no Playwright, so it reports an honest "not available" while keeping the
//!   exact response shape the GUI expects.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cap on `UnroutedStore` entries (newest kept).
pub(crate) const UNROUTED_CAP: usize = 200;
/// Upper bound for a single audit query (mirrors the Python `max(1, min(limit, 500))`).
const AUDIT_MAX_QUERY: usize = 500;
/// Messages kept per channel in `ChannelBuffer`.
pub(crate) const BUFFER_CAP: usize = 50;
/// Length cap for audit preview/resource/reason strings.
const TRUNCATE_LIMIT: usize = 500;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_utc_string() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn load_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn save_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(value).unwrap_or_default());
}

fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn truncate(text: &str) -> String {
    let t = text.replace('\n', "\\n");
    if t.chars().count() <= TRUNCATE_LIMIT {
        t
    } else {
        let head: String = t.chars().take(TRUNCATE_LIMIT - 3).collect();
        format!("{head}...")
    }
}

// ---------------------------------------------------------------------------
// Channel reference parsing (`resolve_channel`)
// ---------------------------------------------------------------------------

/// Turn a user/agent-supplied channel reference into a `<platform>:<chat_id>`
/// address, mirroring `subscriptions.resolve_channel`:
/// - `<#C0123|name>` mention token → `slack:C0123`
/// - `slack.com/archives/C0123ABC` copy-link URL → `slack:C0123ABC`
/// - a bare `#name` resolves to `""` (names can't be looked up locally)
/// - anything already containing `:` is returned as-is
/// - a bare id is assumed to be on `default_platform`
pub fn resolve_channel(ref_: &str, default_platform: &str) -> String {
    let r = ref_.trim();
    if r.is_empty() {
        return String::new();
    }
    // Slack encoded `#channel` as `<#C0123|name>`.
    if let Some(inner) = r.strip_prefix("<#") {
        if let Some(end) = inner.find('>') {
            let id = inner[..end].split('|').next().unwrap_or("");
            if !id.is_empty()
                && id.starts_with('C')
                && id.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                return format!("slack:{id}");
            }
        }
    }
    // Slack "Copy link": https://acme.slack.com/archives/C0123ABC
    const MARKER: &str = "slack.com/archives/";
    if let Some(pos) = r.find(MARKER) {
        let rest = &r[pos + MARKER.len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !id.is_empty() {
            return format!("slack:{}", id.to_uppercase());
        }
    }
    if r.starts_with('#') {
        return String::new();
    }
    if r.contains(':') {
        return r.to_string();
    }
    format!("{default_platform}:{r}")
}

// ---------------------------------------------------------------------------
// SubscriptionStore
// ---------------------------------------------------------------------------

/// A persisted `(session_id, channel)` inbound-listening record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub session_id: String,
    /// Address form `"<platform>:<chat_id>"` (e.g. `"slack:C0123"`).
    pub channel: String,
    /// Reserved for a later "all" vs "mentions" refinement; v1 always "all".
    #[serde(default = "default_filter")]
    pub filter: String,
}

fn default_filter() -> String {
    "all".into()
}

#[derive(Default, Serialize)]
struct SubscriptionFile {
    subscriptions: Vec<Subscription>,
}

/// Durable `(session_id, channel)` records; mirror of `SubscriptionStore`.
pub struct SubscriptionStore {
    path: Option<PathBuf>,
    subs: Mutex<Vec<Subscription>>,
}

impl SubscriptionStore {
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let subs = match &path {
            Some(p) => load_json(p)
                .get("subscriptions")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| serde_json::from_value::<Subscription>(e.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Self {
            path,
            subs: Mutex::new(subs),
        }
    }

    fn save_locked(&self, subs: &[Subscription]) {
        if let Some(path) = &self.path {
            let file = SubscriptionFile {
                subscriptions: subs.to_vec(),
            };
            let v = serde_json::to_value(&file).unwrap_or(Value::Null);
            save_json(path, &v);
        }
    }

    /// Subscribe (idempotent — re-subscribing updates the filter). Returns the record.
    pub fn subscribe(&self, session_id: &str, channel: &str, filter: &str) -> Subscription {
        let mut subs = self.subs.lock().unwrap();
        if let Some(existing) = subs
            .iter_mut()
            .find(|s| s.session_id == session_id && s.channel == channel)
        {
            existing.filter = filter.to_string();
            let updated = existing.clone();
            self.save_locked(&subs);
            return updated;
        }
        let sub = Subscription {
            session_id: session_id.to_string(),
            channel: channel.to_string(),
            filter: filter.to_string(),
        };
        subs.push(sub.clone());
        self.save_locked(&subs);
        sub
    }

    /// Unsubscribe; returns true when a record was actually removed.
    pub fn unsubscribe(&self, session_id: &str, channel: &str) -> bool {
        let mut subs = self.subs.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| !(s.session_id == session_id && s.channel == channel));
        let changed = subs.len() != before;
        if changed {
            self.save_locked(&subs);
        }
        changed
    }

    /// Drop all of a session's subscriptions (called when the session is deleted).
    pub fn remove_session(&self, session_id: &str) {
        let mut subs = self.subs.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| s.session_id != session_id);
        if subs.len() != before {
            self.save_locked(&subs);
        }
    }

    pub fn for_channel(&self, channel: &str) -> Vec<Subscription> {
        self.subs
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.channel == channel)
            .cloned()
            .collect()
    }

    pub fn for_session(&self, session_id: &str) -> Vec<Subscription> {
        self.subs
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.session_id == session_id)
            .cloned()
            .collect()
    }

    pub fn all(&self) -> Vec<Subscription> {
        self.subs.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// UnroutedStore
// ---------------------------------------------------------------------------

/// A dead-letter entry: an inbound message (or failed turn) that had nowhere to go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnroutedItem {
    /// Message origin / session id (e.g. `"slack:D123"` or a session id).
    pub source: String,
    /// Who sent it (`"-"` when not applicable, e.g. a turn failure).
    pub sender: String,
    /// The message (or the failing instruction).
    pub text: String,
    /// Why it landed here ("no DM session designated", an error string, …).
    pub reason: String,
    #[serde(default)]
    pub ts: f64,
}

/// Capped, newest-kept dead-letter store; mirror of `UnroutedStore`.
pub struct UnroutedStore {
    path: Option<PathBuf>,
    cap: usize,
    items: Mutex<Vec<UnroutedItem>>,
}

impl UnroutedStore {
    pub fn new(path: Option<impl AsRef<Path>>, cap: usize) -> Self {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let items = match &path {
            Some(p) => load_json(p)
                .get("items")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| serde_json::from_value::<UnroutedItem>(e.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Self {
            path,
            cap,
            items: Mutex::new(items),
        }
    }

    fn save_locked(&self, items: &[UnroutedItem]) {
        if let Some(path) = &self.path {
            save_json(path, &json!({ "items": items }));
        }
    }

    pub fn record(&self, source: &str, sender: &str, text: &str, reason: &str) -> UnroutedItem {
        let item = UnroutedItem {
            source: if source.is_empty() { "?".into() } else { source.to_string() },
            sender: if sender.is_empty() { "-".into() } else { sender.to_string() },
            text: text.to_string(),
            reason: reason.to_string(),
            ts: now_ts(),
        };
        let mut items = self.items.lock().unwrap();
        items.push(item.clone());
        if items.len() > self.cap {
            *items = items[items.len() - self.cap..].to_vec();
        }
        self.save_locked(&items);
        item
    }

    /// Most-recent-first, for the GUI panel.
    pub fn list(&self, n: usize) -> Vec<Value> {
        let items = self.items.lock().unwrap();
        items
            .iter()
            .rev()
            .take(n.max(1))
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// AuditStore
// ---------------------------------------------------------------------------

/// Keys whose values are never logged in full.
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "api_key",
    "access_token",
    "bot_token",
    "app_token",
    "raw",
];
const BODY_KEYS: &[&str] = &["body", "content", "html"];
const RESOURCE_KEYS: &[&str] = &[
    "url",
    "owner",
    "repo",
    "issue_key",
    "page_id",
    "ticket_id",
    "calendar_id",
    "message_id",
];

/// Best-effort tool-name → connector mapping for audit rows that don't carry one.
fn connector_for_tool(tool: &str) -> String {
    for (prefix, connector) in [
        ("google_calendar", "google_calendar"),
        ("gmail", "gmail"),
        ("hubspot", "hubspot"),
        ("github", "github"),
        ("slack", "slack"),
        ("notion", "notion"),
        ("attio", "attio"),
        ("outlook", "outlook"),
        ("telegram", "telegram"),
        ("zendesk", "zendesk"),
    ] {
        if tool.starts_with(prefix) {
            return connector.to_string();
        }
    }
    String::new()
}

fn summarize(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(truncate(s)),
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
        Value::Array(a) => json!(a.iter().take(10).map(summarize).collect::<Vec<Value>>()),
        Value::Object(o) => json!(o
            .iter()
            .take(20)
            .map(|(k, v)| (k.clone(), summarize(v)))
            .collect::<Map<String, Value>>()),
    }
}

/// Mask secret/bulk fields before logging, mirroring `audit._sanitize_args`.
fn sanitize_args(tool: &str, args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return json!({});
    };
    let mut out = Map::new();
    for (k, v) in obj {
        let lk = k.to_lowercase();
        let is_secret = SECRET_KEYS.iter().any(|s| lk.contains(s));
        let is_input = tool == "browser_type" && lk == "text";
        let is_body = BODY_KEYS.iter().any(|b| lk == *b || lk.ends_with(&format!("_{b}")));
        if is_secret {
            out.insert(k.clone(), json!("[redacted]"));
        } else if is_input {
            out.insert(k.clone(), json!("[redacted input]"));
        } else if is_body {
            out.insert(k.clone(), json!("[redacted body]"));
        } else {
            out.insert(k.clone(), summarize(v));
        }
    }
    Value::Object(out)
}

fn resource_for(args: &Value, result: &Value) -> String {
    for key in RESOURCE_KEYS {
        if let Some(s) = args.get(*key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Some(s) = args.get("subdomain").and_then(|v| v.as_str()) {
        return format!("{s}.zendesk.com");
    }
    if let Some(s) = result.get("url").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Durable append-only audit log (JSONL) with an in-memory mirror for queries.
/// Mirror of `AuditStore` (the Python side uses SQLite; JSONL keeps the Rust
/// server dependency-light — the query surface is the same).
pub struct AuditStore {
    path: PathBuf,
    events: Mutex<Vec<Value>>,
}

impl AuditStore {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let events = std::fs::read_to_string(&path)
            .ok()
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path,
            events: Mutex::new(events),
        }
    }

    /// Append one event. Never blocks the caller on the filesystem: the write
    /// happens while holding the in-memory lock (traffic is human-rate).
    pub fn append(&self, event: &Map<String, Value>) {
        let tool = get_str(&Value::Object(event.clone()), "tool")
            .or_else(|| get_str(&Value::Object(event.clone()), "tool_name"))
            .unwrap_or_default();
        let connector = get_str(&Value::Object(event.clone()), "connector")
            .unwrap_or_else(|| connector_for_tool(&tool));
        let args = event
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = event.get("result").cloned().unwrap_or(Value::Null);

        let mut record = Map::new();
        let mut events = self.events.lock().unwrap();
        let id = events.len() as u64 + 1;
        record.insert("id".into(), json!(id));
        record.insert("timestamp".into(), json!(now_utc_string()));
        for field in [
            "session_id",
            "agent",
            "workspace",
            "stage",
            "status",
            "approval",
            "reason",
        ] {
            let value = event
                .get(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            record.insert(field.into(), json!(value));
        }
        record.insert("connector".into(), json!(connector));
        record.insert("tool".into(), json!(tool));
        record.insert("args".into(), sanitize_args(&tool, &args));
        record.insert(
            "result_preview".into(),
            json!(truncate(
                event
                    .get("result_preview")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            )),
        );
        record.insert(
            "resource".into(),
            json!(truncate(&resource_for(&args, &result))),
        );
        let record_value = Value::Object(record);
        events.push(record_value.clone());

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{}", serde_json::to_string(&record_value).unwrap_or_default());
        }
    }

    /// Most-recent-first with optional filters; `limit` is clamped to 1..=500.
    pub fn list(
        &self,
        limit: usize,
        session_id: Option<&str>,
        connector: Option<&str>,
        tool: Option<&str>,
    ) -> Vec<Value> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .rev()
            .filter(|e| {
                session_id.is_none_or(|s| get_str(e, "session_id").as_deref() == Some(s))
            })
            .filter(|e| {
                connector.is_none_or(|c| get_str(e, "connector").as_deref() == Some(c))
            })
            .filter(|e| tool.is_none_or(|t| get_str(e, "tool").as_deref() == Some(t)))
            .take(limit.clamp(1, AUDIT_MAX_QUERY))
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ChannelBuffer
// ---------------------------------------------------------------------------

/// Last-N messages per channel + display names; the picker's "recently-seen"
/// list. Persisted best-effort (a suggestion list that empties on restart is
/// useless). Mirror of `subscriptions.ChannelBuffer`.
pub struct ChannelBuffer {
    path: Option<PathBuf>,
    cap: usize,
    by_channel: Mutex<HashMap<String, VecDeque<Value>>>,
    names: Mutex<HashMap<String, String>>,
}

impl ChannelBuffer {
    pub fn new(cap: usize, path: Option<impl AsRef<Path>>) -> Self {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let mut by_channel = HashMap::new();
        let mut names = HashMap::new();
        if let Some(p) = &path {
            let data = load_json(p);
            if data.is_object() {
                // Current format: {"messages": {...}, "names": {...}}; the first
                // shipped format was the bare messages dict — accept both.
                let msgs = if data.get("messages").is_some() {
                    data.get("messages").cloned()
                } else {
                    Some(data.clone())
                };
                if let Some(names_obj) = data.get("names").and_then(|v| v.as_object()) {
                    for (k, v) in names_obj {
                        if let Some(s) = v.as_str() {
                            names.insert(k.clone(), s.to_string());
                        }
                    }
                }
                if let Some(msgs_value) = msgs {
                    if let Some(msgs_obj) = msgs_value.as_object() {
                        for (chan, msgs) in msgs_obj {
                            if let Some(list) = msgs.as_array() {
                                let deque: VecDeque<Value> =
                                    list.iter().take(cap).cloned().collect();
                                by_channel.insert(chan.clone(), deque);
                            }
                        }
                    }
                }
            }
        }
        Self {
            path,
            cap,
            by_channel: Mutex::new(by_channel),
            names: Mutex::new(names),
        }
    }

    fn save_locked(&self, by_channel: &HashMap<String, VecDeque<Value>>, names: &HashMap<String, String>) {
        if let Some(path) = &self.path {
            // Atomic-ish write: temp file + rename.
            let mut messages = Map::new();
            for (chan, msgs) in by_channel {
                let list: Vec<Value> = msgs.iter().cloned().collect();
                messages.insert(chan.clone(), Value::Array(list));
            }
            let payload = json!({
                "messages": messages,
                "names": names,
            });
            let tmp = path.with_extension("tmp");
            if let Some(parent) = tmp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&tmp, serde_json::to_string(&payload).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Record one inbound message; `name` (optional) is the channel's display name.
    pub fn record(&self, channel: &str, who: &str, text: &str, name: Option<&str>) {
        let mut by_channel = self.by_channel.lock().unwrap();
        let mut names = self.names.lock().unwrap();
        let deque = by_channel
            .entry(channel.to_string())
            .or_insert_with(|| VecDeque::with_capacity(self.cap));
        deque.push_back(json!({ "from": who, "text": text }));
        if deque.len() > self.cap {
            deque.pop_front();
        }
        if let Some(n) = name {
            names.insert(channel.to_string(), n.to_string());
        }
        self.save_locked(&by_channel, &names);
    }

    pub fn recent(&self, channel: &str, n: usize) -> Vec<Value> {
        let by_channel = self.by_channel.lock().unwrap();
        let msgs = by_channel.get(channel).cloned().unwrap_or_default();
        let n = n.clamp(1, self.cap);
        msgs.iter().rev().take(n).rev().cloned().collect()
    }

    /// The channel's resolved display name, if any inbound message carried one.
    pub fn name_for(&self, channel: &str) -> Option<String> {
        self.names.lock().unwrap().get(channel).cloned()
    }

    /// Channels seen so far (the picker's "recently-seen" list), newest last.
    /// Sorted by channel address for deterministic output.
    pub fn channels(&self) -> Vec<Value> {
        let by_channel = self.by_channel.lock().unwrap();
        let names = self.names.lock().unwrap();
        let mut out: Vec<Value> = by_channel
            .iter()
            .map(|(chan, msgs)| {
                let last = msgs.back().cloned().unwrap_or_else(|| json!({}));
                json!({
                    "channel": chan,
                    "name": names.get(chan),
                    "last_from": last.get("from"),
                    "last_text": last.get("text"),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            get_str(a, "channel")
                .unwrap_or_default()
                .cmp(&get_str(b, "channel").unwrap_or_default())
        });
        out
    }
}

// ---------------------------------------------------------------------------
// BrowserController
// ---------------------------------------------------------------------------

fn default_browser_state() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("open".into(), json!(false));
    m.insert("url".into(), json!(""));
    m.insert("title".into(), json!(""));
    m.insert("status".into(), json!("closed"));
    m.insert("last_action".into(), json!(""));
    m.insert("last_result".into(), json!(""));
    m.insert("last_error".into(), json!(""));
    m.insert("screenshot_data_url".into(), json!(""));
    m.insert("updated_at".into(), Value::Null);
    m.insert("controls".into(), json!([]));
    m
}

/// The browser-session state contract (mirror of `browser_automation._BROWSER`).
/// The Rust server has no Playwright, so the controller honestly reports
/// "not available" for actions while keeping the exact GUI response shape.
pub struct BrowserController {
    state: Mutex<Map<String, Value>>,
}

impl BrowserController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(default_browser_state()),
        }
    }

    pub fn state(&self) -> Value {
        let state = self.state.lock().unwrap();
        Value::Object(state.clone())
    }

    /// No session exists in this build — Python's `_close_locked` also returns
    /// `{"ok": true}` when there is nothing to close.
    pub fn close(&self) -> Value {
        let mut state = self.state.lock().unwrap();
        *state = default_browser_state();
        json!({ "ok": true })
    }

    pub fn screenshot(&self) -> Value {
        let mut state = self.state.lock().unwrap();
        state.insert("last_action".into(), json!("screenshot"));
        state.insert("last_result".into(), json!("error"));
        state.insert(
            "last_error".into(),
            json!("browser automation is not available in this build"),
        );
        state.insert("updated_at".into(), json!(now_utc_string()));
        json!({ "ok": false, "error": "browser automation is not available in this build" })
    }
}

impl Default for BrowserController {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PDF inspection (attach-time page/size probe)
// ---------------------------------------------------------------------------

const PDF_DATA_URL_PREFIX: &str = "data:application/pdf;base64,";

/// Page count + size for a PDF data URL — the attach-time threshold check.
/// Never fails loudly: `{"ok": false, "error": ...}` for anything unreadable.
/// Mirror of `pdf_support.inspect`.
pub fn inspect_pdf(data_url: &str) -> Value {
    use base64::Engine as _;
    if !data_url.starts_with(PDF_DATA_URL_PREFIX) {
        return json!({ "ok": false, "error": "not a PDF data URL" });
    }
    let raw = match base64::engine::general_purpose::STANDARD
        .decode(&data_url[PDF_DATA_URL_PREFIX.len()..])
    {
        Ok(b) => b,
        Err(_) => return json!({ "ok": false, "error": "not a PDF data URL" }),
    };
    match lopdf::Document::load_mem(&raw) {
        Ok(doc) => json!({ "ok": true, "pages": doc.get_pages().len(), "bytes": raw.len() }),
        Err(_) => json!({ "ok": false, "error": "could not read PDF" }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_channel_mention_token() {
        assert_eq!(
            resolve_channel("<#C0123|general>", "slack"),
            "slack:C0123"
        );
        assert_eq!(resolve_channel("<#C0123>", "slack"), "slack:C0123");
    }

    #[test]
    fn resolve_channel_archives_link() {
        assert_eq!(
            resolve_channel("https://acme.slack.com/archives/C0123ABC", "slack"),
            "slack:C0123ABC"
        );
        assert_eq!(
            resolve_channel("https://acme.slack.com/archives/c0123abc", "slack"),
            "slack:C0123ABC"
        );
    }

    #[test]
    fn resolve_channel_forms() {
        // Bare #name cannot be looked up locally.
        assert_eq!(resolve_channel("#general", "slack"), "");
        // Full address passes through.
        assert_eq!(resolve_channel("slack:C0123", "slack"), "slack:C0123");
        assert_eq!(resolve_channel("telegram:12345", "slack"), "telegram:12345");
        // Bare id assumes the default platform.
        assert_eq!(resolve_channel("C0123", "slack"), "slack:C0123");
        assert_eq!(resolve_channel("", "slack"), "");
        assert_eq!(resolve_channel("  ", "slack"), "");
    }

    #[test]
    fn subscription_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ocw-sub-{}", std::process::id()));
        let path = dir.join("subscriptions.json");
        let store = SubscriptionStore::new(Some(&path));
        store.subscribe("s1", "slack:C0123", "all");
        store.subscribe("s1", "slack:C0124", "all");
        store.subscribe("s2", "slack:C0123", "all");
        // Idempotent re-subscribe updates filter.
        let again = store.subscribe("s1", "slack:C0123", "mentions");
        assert_eq!(again.filter, "mentions");
        assert_eq!(store.all().len(), 3);
        assert_eq!(store.for_channel("slack:C0123").len(), 2);
        assert_eq!(store.for_session("s1").len(), 2);

        // Persistence across reload.
        let store2 = SubscriptionStore::new(Some(&path));
        assert_eq!(store2.all().len(), 3);

        assert!(store.unsubscribe("s1", "slack:C0123"));
        assert!(!store.unsubscribe("s1", "slack:C0123"));
        store.remove_session("s2");
        assert_eq!(store.all().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrouted_cap_and_reverse_order() {
        let dir = std::env::temp_dir().join(format!("ocw-unr-{}", std::process::id()));
        let path = dir.join("unrouted.json");
        let store = UnroutedStore::new(Some(&path), 3);
        store.record("slack:D1", "alice", "hello", "no DM session designated");
        store.record("sess-2", "-", "boom", "ERROR: engine failed");
        store.record("slack:D1", "bob", "again", "no DM session designated");
        store.record("slack:D2", "carol", "later", "no DM session designated");
        // Cap 3 → oldest dropped.
        let items = store.list(10);
        assert_eq!(items.len(), 3);
        assert_eq!(get_str(&items[0], "source").as_deref(), Some("slack:D2"));
        assert_eq!(get_str(&items[1], "sender").as_deref(), Some("bob"));

        // Persistence across reload.
        let store2 = UnroutedStore::new(Some(&path), 3);
        assert_eq!(store2.list(10).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_sanitize_and_filter() {
        let dir = std::env::temp_dir().join(format!("ocw-aud-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let store = AuditStore::open(&path);
        let mut ev1 = Map::new();
        ev1.insert(
            "session_id".into(),
            json!("s1"),
        );
        ev1.insert("tool".into(), json!("gmail_send"));
        ev1.insert("status".into(), json!("ok"));
        ev1.insert(
            "arguments".into(),
            json!({"to": "a@b.c", "api_key": "sk-secret", "body": "long text", "message_id": "m1"}),
        );
        store.append(&ev1);

        let mut ev2 = Map::new();
        ev2.insert("session_id".into(), json!("s2"));
        ev2.insert("tool".into(), json!("slack_post"));
        ev2.insert("status".into(), json!("error"));
        ev2.insert("arguments".into(), json!({"channel": "C1", "token": "xoxb-1"}));
        store.append(&ev2);

        // Secret masking + connector inference + resource extraction.
        let all = store.list(100, None, None, None);
        assert_eq!(all.len(), 2);
        let first = &all[0];
        assert_eq!(get_str(first, "tool").as_deref(), Some("slack_post"));
        assert_eq!(get_str(first, "connector").as_deref(), Some("slack"));
        assert_eq!(first["args"]["token"], json!("[redacted]"));
        let second = &all[1];
        assert_eq!(second["args"]["api_key"], json!("[redacted]"));
        assert_eq!(second["args"]["body"], json!("[redacted body]"));
        assert_eq!(second["args"]["to"], json!("a@b.c"));
        assert_eq!(get_str(second, "resource").as_deref(), Some("m1"));
        assert_eq!(second["timestamp"].as_str().unwrap().len(), 19);

        // Filters.
        assert_eq!(store.list(100, Some("s1"), None, None).len(), 1);
        assert_eq!(store.list(100, None, Some("gmail"), None).len(), 1);
        assert_eq!(store.list(100, None, None, Some("slack_post")).len(), 1);
        assert_eq!(store.list(100, Some("nope"), None, None).len(), 0);

        // Persistence across reload.
        let store2 = AuditStore::open(&path);
        assert_eq!(store2.list(100, None, None, None).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_truncates_long_values() {
        let dir = std::env::temp_dir().join(format!("ocw-aud2-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let store = AuditStore::open(&path);
        let mut ev = Map::new();
        ev.insert("tool".into(), json!("github_create_issue"));
        ev.insert(
            "arguments".into(),
            json!({"title": "x".repeat(600)}),
        );
        store.append(&ev);
        let all = store.list(1, None, None, None);
        let title = all[0]["args"]["title"].as_str().unwrap();
        assert!(title.ends_with("..."));
        assert!(title.len() <= 500);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_buffer_recent_and_names() {
        let dir = std::env::temp_dir().join(format!("ocw-buf-{}", std::process::id()));
        let path = dir.join("channels.json");
        let buffer = ChannelBuffer::new(50, Some(&path));
        buffer.record("slack:C0123", "alice", "hi", Some("#general"));
        buffer.record("slack:C0123", "bob", "yo", Some("#general"));
        buffer.record("slack:C9999", "carol", "other", Some("#random"));
        assert_eq!(buffer.name_for("slack:C0123").as_deref(), Some("#general"));
        let recent = buffer.recent("slack:C0123", 10);
        assert_eq!(recent.len(), 2);
        assert_eq!(get_str(&recent[0], "from").as_deref(), Some("alice"));
        assert_eq!(get_str(&recent[1], "text").as_deref(), Some("yo"));

        let channels = buffer.channels();
        assert_eq!(channels.len(), 2);
        let first = &channels[0];
        assert_eq!(get_str(first, "channel").as_deref(), Some("slack:C0123"));
        assert_eq!(get_str(first, "name").as_deref(), Some("#general"));
        assert_eq!(get_str(first, "last_from").as_deref(), Some("bob"));

        // Persistence across reload.
        let buffer2 = ChannelBuffer::new(50, Some(&path));
        assert_eq!(buffer2.recent("slack:C0123", 10).len(), 2);
        assert_eq!(buffer2.name_for("slack:C9999").as_deref(), Some("#random"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_state_contract() {
        let browser = BrowserController::new();
        let state = browser.state();
        assert_eq!(state["open"], json!(false));
        assert_eq!(state["status"], json!("closed"));
        assert!(state.get("controls").is_some());
        assert!(state.get("screenshot_data_url").is_some());
        assert!(state.get("updated_at").is_some());

        assert_eq!(browser.close(), json!({"ok": true}));
        let shot = browser.screenshot();
        assert_eq!(shot["ok"], json!(false));
        assert!(shot["error"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn inspect_pdf_errors_are_soft() {
        assert_eq!(inspect_pdf("not a data url"), json!({"ok": false, "error": "not a PDF data URL"}));
        assert_eq!(
            inspect_pdf("data:application/pdf;base64,@@@notbase64@@@"),
            json!({"ok": false, "error": "not a PDF data URL"})
        );
        // A genuinely valid minimal single-page PDF (built with lopdf itself)
        // round-trips through base64.
        use lopdf::dictionary;
        use lopdf::Object;
        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let catalog_id = doc.new_object_id();
        let pages = lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        };
        let page = lopdf::dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(612),
                Object::Integer(792),
            ],
        };
        let catalog = lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        doc.objects.insert(page_id, Object::Dictionary(page));
        doc.objects.insert(catalog_id, Object::Dictionary(catalog));
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let pdf = buf;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pdf);
        let out = inspect_pdf(&format!("{PDF_DATA_URL_PREFIX}{b64}"));
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["pages"], json!(1));
        assert_eq!(out["bytes"], json!(pdf.len()));
    }
}

// ---------------------------------------------------------------------------
// UnattendedStore — per-session unattended flag (mirror of `unattended.py`)
// ---------------------------------------------------------------------------

/// Unattended mode — a per-session toggle for *where the human is reached*. It
/// does **not** change the autonomy ceiling (the permission mode does). When a
/// session is unattended, anything that would prompt inline (approval /
/// question) is routed to the Inbox. This store just persists the flag.
pub struct UnattendedStore {
    path: PathBuf,
    flags: Mutex<HashMap<String, bool>>,
}

impl UnattendedStore {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("unattended.json");
        let mut flags = HashMap::new();
        if let Some(obj) = load_json(&path).as_object() {
            for (k, v) in obj {
                flags.insert(k.clone(), v.as_bool().unwrap_or(false));
            }
        }
        Self {
            path,
            flags: Mutex::new(flags),
        }
    }

    fn save(&self, flags: &HashMap<String, bool>) {
        save_json(&self.path, &json!(flags));
    }

    pub fn is_unattended(&self, session_id: &str) -> bool {
        self.flags.lock().unwrap().get(session_id).copied().unwrap_or(false)
    }

    pub fn set(&self, session_id: &str, unattended: bool) {
        let mut flags = self.flags.lock().unwrap();
        if unattended {
            flags.insert(session_id.to_string(), true);
        } else {
            flags.remove(session_id);
        }
        self.save(&flags);
    }

    pub fn sessions(&self) -> Vec<String> {
        self.flags
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(sid, on)| if *on { Some(sid.clone()) } else { None })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// PersonaConnectionStore — per-persona connector defaults (connections.py)
// ---------------------------------------------------------------------------

/// `{persona_id: {connector: bool}}` — the per-persona default on/off for each
/// connector (UI-REFRESH §4). Seeded from the persona manifest's connector
/// recommends on first read, then user-editable.
pub struct PersonaConnectionStore {
    path: PathBuf,
    rows: Mutex<HashMap<String, HashMap<String, bool>>>,
}

impl PersonaConnectionStore {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("persona_connections.json");
        let mut rows = HashMap::new();
        if let Some(personas) = load_json(&path).get("personas").and_then(|v| v.as_object()) {
            for (pid, row) in personas {
                let mut conns = HashMap::new();
                if let Some(r) = row.as_object() {
                    for (c, v) in r {
                        conns.insert(c.clone(), v.as_bool().unwrap_or(false));
                    }
                }
                rows.insert(pid.clone(), conns);
            }
        }
        Self {
            path,
            rows: Mutex::new(rows),
        }
    }

    fn save(&self, rows: &HashMap<String, HashMap<String, bool>>) {
        save_json(&self.path, &json!({ "personas": rows }));
    }

    /// The persona's stored row (a copy). Empty dict if it was never
    /// seeded/edited — this does NOT seed.
    pub fn get(&self, persona_id: &str) -> HashMap<String, bool> {
        self.rows
            .lock()
            .unwrap()
            .get(persona_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Seed the persona's defaults on first read (only when no row exists),
    /// then return the effective row. `seeded` maps connector → default-on
    /// (core recommends True, others False).
    pub fn defaults_for(
        &self,
        persona_id: &str,
        seeded: HashMap<String, bool>,
    ) -> HashMap<String, bool> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.get(persona_id) {
            return row.clone();
        }
        rows.insert(persona_id.to_string(), seeded.clone());
        self.save(&rows);
        seeded
    }

    pub fn set(&self, persona_id: &str, connector: &str, enabled: bool) {
        let mut rows = self.rows.lock().unwrap();
        rows.entry(persona_id.to_string())
            .or_default()
            .insert(connector.to_string(), enabled);
        self.save(&rows);
    }
}

// ---------------------------------------------------------------------------
// SessionConnectionStore — per-session connector overrides (connections.py)
// ---------------------------------------------------------------------------

/// `{session_id: {connector: bool}}` — per-session overrides only; an absent
/// entry means the session inherits the persona default.
pub struct SessionConnectionStore {
    path: PathBuf,
    rows: Mutex<HashMap<String, HashMap<String, bool>>>,
}

impl SessionConnectionStore {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("session_connections.json");
        let mut rows = HashMap::new();
        if let Some(sessions) = load_json(&path).get("sessions").and_then(|v| v.as_object()) {
            for (sid, row) in sessions {
                let mut conns = HashMap::new();
                if let Some(r) = row.as_object() {
                    for (c, v) in r {
                        conns.insert(c.clone(), v.as_bool().unwrap_or(false));
                    }
                }
                rows.insert(sid.clone(), conns);
            }
        }
        Self {
            path,
            rows: Mutex::new(rows),
        }
    }

    fn save(&self, rows: &HashMap<String, HashMap<String, bool>>) {
        save_json(&self.path, &json!({ "sessions": rows }));
    }

    /// The session's stored overrides (a copy). Absent entry = inherit.
    pub fn get(&self, session_id: &str) -> HashMap<String, bool> {
        self.rows
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set(&self, session_id: &str, connector: &str, enabled: bool) {
        let mut rows = self.rows.lock().unwrap();
        rows.entry(session_id.to_string())
            .or_default()
            .insert(connector.to_string(), enabled);
        self.save(&rows);
    }

    /// Drop a single override so the session inherits the persona default again.
    pub fn clear(&self, session_id: &str, connector: &str) {
        let mut rows = self.rows.lock().unwrap();
        let mut remove_row = false;
        if let Some(row) = rows.get_mut(session_id) {
            row.remove(connector);
            remove_row = row.is_empty();
        }
        if remove_row {
            rows.remove(session_id);
        }
        self.save(&rows);
    }

    /// Drop all of a session's overrides (called when the session is deleted).
    pub fn remove_session(&self, session_id: &str) {
        let mut rows = self.rows.lock().unwrap();
        if rows.remove(session_id).is_some() {
            self.save(&rows);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: connection stores (mirror of `tests/test_connections.py`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod connection_store_tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ocw-conn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unattended_flag_roundtrips_and_persists() {
        let dir = temp_dir("unattended");
        let s = UnattendedStore::open(&dir);
        assert!(!s.is_unattended("s1"));
        s.set("s1", true);
        assert!(s.is_unattended("s1"));
        assert_eq!(s.sessions(), vec!["s1".to_string()]);
        s.set("s1", false);
        assert!(!s.is_unattended("s1"));
        assert!(s.sessions().is_empty());
        // Persisted: a fresh store on the same dir sees the flag.
        s.set("s2", true);
        let reopened = UnattendedStore::open(&dir);
        assert!(reopened.is_unattended("s2"));
        assert!(!reopened.is_unattended("s1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persona_defaults_seed_core_on_optional_off() {
        let dir = temp_dir("persona");
        let s = PersonaConnectionStore::open(&dir);
        assert!(s.get("ops").is_empty());
        let mut seeded = HashMap::new();
        seeded.insert("github".to_string(), true);
        seeded.insert("slack".to_string(), true);
        seeded.insert("pagerduty".to_string(), false);
        let got = s.defaults_for("ops", seeded);
        assert!(got["github"]);
        assert!(!got["pagerduty"]);
        // Second call returns the stored row (seed is stable).
        let again = s.defaults_for("ops", HashMap::new());
        assert!(again["github"]);
        // Edit overlays the row.
        s.set("ops", "github", false);
        assert!(!s.get("ops")["github"]);
        // Persistence across reopen.
        let reopened = PersonaConnectionStore::open(&dir);
        assert!(!reopened.get("ops")["github"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_overrides_set_clear_remove() {
        let dir = temp_dir("session");
        let s = SessionConnectionStore::open(&dir);
        assert!(s.get("s1").is_empty());
        s.set("s1", "slack", true);
        s.set("s1", "github", false);
        let row = s.get("s1");
        assert!(row["slack"]);
        assert!(!row["github"]);
        // clear drops one override; the other stays.
        s.clear("s1", "slack");
        assert!(!s.get("s1").contains_key("slack"));
        assert!(s.get("s1").contains_key("github"));
        // clearing the last override removes the whole row.
        s.clear("s1", "github");
        assert!(s.get("s1").is_empty());
        // remove_session drops everything.
        s.set("s1", "slack", true);
        s.set("s2", "github", true);
        s.remove_session("s1");
        assert!(s.get("s1").is_empty());
        assert!(!s.get("s2").is_empty());
        // Persistence across reopen.
        let reopened = SessionConnectionStore::open(&dir);
        assert!(!reopened.get("s2").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}