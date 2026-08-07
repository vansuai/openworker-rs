//! Real inbound adapters + pure mappers.
//!
//! Mirrors `coworker/connectors/adapters.py`. The heavy SDKs (slack-bolt,
//! python-telegram-bot) are Python-only, so on the Rust side the adapters talk
//! to the platform HTTP API directly:
//!
//! - **Slack** uses `chat.postMessage` / `users.info` / `conversations.info` over
//!   HTTPS — Socket Mode (long-lived WebSocket) is intentionally out of scope for
//!   this port. The adapter still receives inbound messages via the gateway
//!   (e.g. delivered by an external Socket Mode relay or FakeSlack harness) and
//!   maps the JSON envelope into [`MessageEvent`].
//! - **Telegram** uses long-poll `getUpdates` against the Bot HTTP API.
//! - **Email** is a stub that exposes the same surface but always returns
//!   `disconnected` — the third-party API lives in `integration.rs`.
//!
//! The raw-event → MessageEvent mappers are pure functions (testable with plain
//! JSON values, no SDK).

use crate::base::{
    BasePlatformAdapter, InteractionEvent, InteractionHandler, MessageEvent, MessageHandler,
    MessageSource, MessageType, SendResult, SessionSource,
};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

/// Slack encodes an @-mention in message text as `<@U0123>` (legacy: `<@U0123|name>`) — a token,
/// not the display name. Resolved at ingestion so every surface (parked cards, transcripts, the
/// channel buffer) shows `@name` instead of the raw id.
pub fn slack_mention_re() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<@([UW][A-Z0-9]+)(?:\|[^>]*)?>").unwrap())
}

/// Convert a Slack `message` event JSON envelope into a normalized [`MessageEvent`].
pub fn slack_event_to_event(event: &Value, bot_user_id: Option<&str>) -> Option<MessageEvent> {
    // Skip bot echoes / message edits / joins etc. (reply-loop guard).
    if event.get("bot_id").is_some() || event.get("subtype").is_some() {
        return None;
    }
    if let Some(bot) = bot_user_id {
        if event.get("user").and_then(|v| v.as_str()) == Some(bot) {
            return None;
        }
    }
    let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        return None;
    }
    let chat_type = if event.get("channel_type").and_then(|v| v.as_str()) == Some("im") {
        "dm"
    } else {
        "channel"
    };
    let source = SessionSource {
        platform: "slack".into(),
        chat_id: event
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        user_id: event.get("user").and_then(|v| v.as_str()).map(String::from),
        user_name: None,
        chat_name: None,
        chat_type: chat_type.to_string(),
        thread_id: event.get("thread_ts").and_then(|v| v.as_str()).map(String::from),
        team_id: event.get("team").and_then(|v| v.as_str()).map(String::from),
    };
    // Mention detection runs on the RAW text (the `<@U…>` token form, legacy
    // `<@U…|name>` included) — callers rewrite mentions to `@display-name` only
    // after mapping.
    let mentions_me = if let Some(bot) = bot_user_id {
        let pattern = format!(r"<@{}(?:\|[^>]*)?>", regex::escape(bot));
        Regex::new(&pattern)
            .map(|re| re.is_match(text))
            .unwrap_or(false)
    } else {
        false
    };
    Some(MessageEvent {
        text: text.to_string(),
        source,
        message_id: event.get("ts").and_then(|v| v.as_str()).map(String::from),
        message_type: MessageType::Text,
        reply_to_message_id: None,
        raw: Some(event.clone()),
        mentions_me,
    })
}

/// Build a [`MessageSource`] sidecar for a Slack inbound (UI-REFRESH §3.1).
pub fn slack_to_message_source(event: &Value, sender_name: &str, channel_name: &str) -> Option<MessageSource> {
    let channel_id = event.get("channel").and_then(|v| v.as_str())?;
    let sender_id = event.get("user").and_then(|v| v.as_str()).unwrap_or("");
    let ts = event
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = if event.get("channel_type").and_then(|v| v.as_str()) == Some("im") {
        "dm"
    } else {
        "channel"
    };
    Some(MessageSource {
        connector: "slack".into(),
        kind: kind.to_string(),
        channel_id: channel_id.to_string(),
        channel_name: channel_name.to_string(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        ts,
        text,
    })
}

/// Slack adapter — bot + app token based. Inbound messages are fed in by the
/// gateway (typically an external Socket Mode listener or the FakeSlack
/// harness); outbound `send` uses the `chat.postMessage` HTTP API.
pub struct SlackAdapter {
    bot_token: String,
    bot_user_id: Arc<Mutex<Option<String>>>,
    name_cache: Arc<Mutex<std::collections::HashMap<String, String>>>,
    channel_cache: Arc<Mutex<std::collections::HashMap<String, String>>>,
    message_handler: Arc<Mutex<Option<MessageHandler>>>,
    interaction_handler: Arc<Mutex<Option<InteractionHandler>>>,
    client: reqwest::Client,
    api_base: String,
}

impl SlackAdapter {
    pub fn new(bot_token: String, app_token: String) -> Self {
        // app_token is required for Socket Mode (long-lived WebSocket). For
        // this Rust port we run inbound from an external relay, so we still
        // accept both but only `bot_token` is used at runtime.
        let _ = app_token;
        Self {
            bot_token,
            bot_user_id: Arc::new(Mutex::new(None)),
            name_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            channel_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            message_handler: Arc::new(Mutex::new(None)),
            interaction_handler: Arc::new(Mutex::new(None)),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
            api_base: std::env::var("SLACK_API_URL")
                .unwrap_or_else(|_| "https://slack.com/api/".into()),
        }
    }

    /// Inject the resolved `bot_user_id` after `auth.test` succeeds. Without it
    /// the adapter cannot skip its own echo or detect @-mentions of itself.
    pub fn set_bot_user_id(&self, id: String) {
        *self.bot_user_id.lock() = Some(id);
    }

    pub fn bot_user_id(&self) -> Option<String> {
        self.bot_user_id.lock().clone()
    }

    /// Cached user-name resolution (best-effort). Returns `None` on failure —
    /// caller falls back to the id.
    pub async fn resolve_user_name(&self, user_id: Option<&str>) -> Option<String> {
        let uid = user_id?;
        if let Some(name) = self.name_cache.lock().get(uid).cloned() {
            return Some(name);
        }
        let resp = self
            .client
            .post(format!("{}users.info", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&[("user", uid)])
            .send()
            .await
            .ok()?;
        let json: Value = resp.json().await.ok()?;
        let prof = json.get("user").and_then(|u| u.get("profile"));
        let name = prof
            .and_then(|p| p.get("display_name"))
            .and_then(|v| v.as_str())
            .or_else(|| prof.and_then(|p| p.get("real_name")).and_then(|v| v.as_str()))
            .or_else(|| {
                json.get("user")
                    .and_then(|u| u.get("real_name"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                json.get("user")
                    .and_then(|u| u.get("name"))
                    .and_then(|v| v.as_str())
            })
            .map(String::from);
        if let Some(ref n) = name {
            self.name_cache.lock().insert(uid.to_string(), n.clone());
        }
        name
    }

    /// Rewrite `<@U…>` mention tokens in `text` to `@display-name`. Best-effort:
    /// an id that won't resolve keeps its token.
    pub async fn resolve_mentions(&self, text: &str) -> String {
        let re = slack_mention_re();
        let ids: Vec<String> = re
            .captures_iter(text)
            .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mut out = text.to_string();
        for uid in ids {
            if let Some(name) = self.resolve_user_name(Some(&uid)).await {
                let pat = format!("<@{}(?:\\|[^>]*)?>", regex::escape(&uid));
                if let Ok(re) = Regex::new(&pat) {
                    out = re.replace_all(&out, format!("@{name}")).to_string();
                }
            }
        }
        out
    }

    /// Cached channel-name resolution.
    pub async fn resolve_channel_name(&self, chat_id: Option<&str>) -> Option<String> {
        let cid = chat_id?;
        if let Some(name) = self.channel_cache.lock().get(cid).cloned() {
            return Some(name);
        }
        let resp = self
            .client
            .post(format!("{}conversations.info", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&[("channel", cid)])
            .send()
            .await
            .ok()?;
        let json: Value = resp.json().await.ok()?;
        let name = json
            .get("channel")
            .and_then(|c| c.get("name").or_else(|| c.get("name_normalized")))
            .and_then(|v| v.as_str())
            .map(String::from);
        if let Some(ref n) = name {
            self.channel_cache.lock().insert(cid.to_string(), n.clone());
        }
        name
    }

    /// Replace a resolved prompt's buttons with a plain-text outcome.
    pub async fn update_message(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), String> {
        let resp = self
            .client
            .post(format!("{}chat.update", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&[
                ("channel", chat_id.to_string()),
                ("ts", message_id.to_string()),
                ("text", text.to_string()),
                ("blocks", "[]".to_string()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("chat.update HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Post a plain text message via `chat.postMessage`.
    pub async fn post_message(&self, chat_id: &str, text: &str, thread_id: Option<&str>) -> SendResult {
        let mut form: Vec<(&str, String)> = vec![("channel", chat_id.into()), ("text", text.into())];
        if let Some(t) = thread_id {
            form.push(("thread_ts", t.into()));
        }
        let resp = self
            .client
            .post(format!("{}chat.postMessage", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&form)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: Value = r.json().await.unwrap_or(Value::Null);
                let ts = json.get("ts").and_then(|v| v.as_str()).map(String::from);
                SendResult::ok(ts)
            }
            Ok(r) => SendResult::err(format!("HTTP {}", r.status())),
            Err(e) => SendResult::err(e.to_string()),
        }
    }

    /// Post an interactive message (blocks + buttons). Best-effort: this is a
    /// thin wrapper around `chat.postMessage` with a `blocks` payload.
    pub async fn post_interactive(
        &self,
        chat_id: &str,
        text: &str,
        buttons: &[crate::base::ButtonSpec],
        thread_id: Option<&str>,
    ) -> SendResult {
        let blocks = serde_json::json!({
            "blocks": [{
                "type": "section",
                "text": {"type": "mrkdwn", "text": text}
            }, {
                "type": "actions",
                "elements": buttons.iter().map(|b| {
                    let mut btn = serde_json::json!({
                        "type": "button",
                        "text": {"type": "plain_text", "text": b.text},
                        "action_id": format!("ocw_{}", b.value),
                        "value": b.value,
                    });
                    if let Some(style) = &b.style {
                        btn["style"] = serde_json::json!(style);
                    }
                    btn
                }).collect::<Vec<_>>()
            }]
        });
        let blocks_str = serde_json::to_string(&blocks).unwrap_or_default();
        let mut form: Vec<(&str, String)> = vec![
            ("channel", chat_id.into()),
            ("text", text.into()),
            ("blocks", blocks_str),
        ];
        if let Some(t) = thread_id {
            form.push(("thread_ts", t.into()));
        }
        let resp = self
            .client
            .post(format!("{}chat.postMessage", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&form)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: Value = r.json().await.unwrap_or(Value::Null);
                let ts = json.get("ts").and_then(|v| v.as_str()).map(String::from);
                SendResult::ok(ts)
            }
            Ok(r) => SendResult::err(format!("HTTP {}", r.status())),
            Err(e) => SendResult::err(e.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl BasePlatformAdapter for SlackAdapter {
    fn platform(&self) -> &'static str {
        "slack"
    }

    async fn connect(&self) -> Result<bool, String> {
        // auth.test to discover the bot's user id + validate the token.
        let resp = self
            .client
            .post(format!("{}auth.test", self.api_base))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("slack auth.test HTTP {}", resp.status()));
        }
        let json: Value = resp.json().await.map_err(|e| e.to_string())?;
        if json.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = json
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!("slack auth.test failed: {err}"));
        }
        // Mirror into bot_user_id field; we keep it on `self` (not `&mut`) so
        // it's a write-once mutation handled by the gateway on the first
        // successful inbound event.
        Ok(true)
    }

    async fn disconnect(&self) -> Result<(), String> {
        Ok(())
    }

    async fn send(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
    ) -> SendResult {
        self.post_message(chat_id, text, thread_id).await
    }

    async fn send_interactive(
        &self,
        chat_id: &str,
        text: &str,
        buttons: &[crate::base::ButtonSpec],
        thread_id: Option<&str>,
    ) -> SendResult {
        self.post_interactive(chat_id, text, buttons, thread_id).await
    }

    fn set_message_handler(&self, handler: MessageHandler) {
        *self.message_handler.lock() = Some(handler);
    }

    fn set_interaction_handler(&self, handler: InteractionHandler) {
        *self.interaction_handler.lock() = Some(handler);
    }

    async fn handle_message(&self, event: MessageEvent) {
        let handler = self.message_handler.lock().clone();
        if let Some(h) = handler {
            h(event).await;
        }
    }

    async fn handle_interaction(&self, event: InteractionEvent) {
        let handler = self.interaction_handler.lock().clone();
        if let Some(h) = handler {
            h(event).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

/// Convert a Telegram `Message` JSON envelope into a normalized [`MessageEvent`].
pub fn telegram_message_to_event(msg: &Value) -> Option<MessageEvent> {
    let text = msg.get("text").and_then(|v| v.as_str())?;
    if text.is_empty() {
        return None;
    }
    let chat = msg.get("chat")?;
    let chat_id = chat.get("id").and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(String::from)))?;
    let chat_type = if chat
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.ends_with("private"))
        .unwrap_or(false)
    {
        "dm"
    } else {
        "group"
    };
    let user = msg.get("from");
    let source = SessionSource {
        platform: "telegram".into(),
        chat_id,
        user_id: user
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_i64().map(|n| n.to_string())),
        user_name: user
            .and_then(|u| u.get("first_name"))
            .and_then(|v| v.as_str())
            .map(String::from),
        chat_name: chat.get("title").and_then(|v| v.as_str()).map(String::from),
        chat_type: chat_type.to_string(),
        thread_id: msg
            .get("message_thread_id")
            .and_then(|v| v.as_i64().map(|n| n.to_string())),
        team_id: None,
    };
    Some(MessageEvent {
        text: text.to_string(),
        source,
        message_id: msg
            .get("message_id")
            .and_then(|v| v.as_i64().map(|n| n.to_string())),
        message_type: MessageType::Text,
        reply_to_message_id: msg
            .get("reply_to_message")
            .and_then(|r| r.get("message_id"))
            .and_then(|v| v.as_i64().map(|n| n.to_string())),
        raw: Some(msg.clone()),
        mentions_me: false,
    })
}

/// Telegram adapter — long-polls `getUpdates` against the Bot HTTP API.
pub struct TelegramAdapter {
    token: String,
    message_handler: Arc<Mutex<Option<MessageHandler>>>,
    interaction_handler: Arc<Mutex<Option<InteractionHandler>>>,
    client: reqwest::Client,
    offset: Arc<Mutex<i64>>,
    running: Arc<Mutex<bool>>,
}

impl TelegramAdapter {
    pub fn new(token: String) -> Self {
        Self {
            token,
            message_handler: Arc::new(Mutex::new(None)),
            interaction_handler: Arc::new(Mutex::new(None)),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(35))
                .build()
                .unwrap(),
            offset: Arc::new(Mutex::new(0)),
            running: Arc::new(Mutex::new(false)),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    /// Long-poll loop. Each iteration asks for updates with `timeout=30` and a
    /// 1-second foreground loop pause on empty responses.
    pub async fn run_loop(&self) {
        *self.running.lock() = true;
        while *self.running.lock() {
            let offset = *self.offset.lock();
            let mut form: Vec<(String, String)> = vec![("timeout".into(), "30".into())];
            if offset > 0 {
                form.push(("offset".into(), offset.to_string()));
            }
            let resp = self
                .client
                .post(self.url("getUpdates"))
                .form(&form)
                .send()
                .await;
            if let Ok(r) = resp {
                if let Ok(json) = r.json::<Value>().await {
                    if let Some(results) = json.get("result").and_then(|v| v.as_array()) {
                        for upd in results {
                            if let Some(update_id) = upd.get("update_id").and_then(|v| v.as_i64()) {
                                *self.offset.lock() = update_id + 1;
                            }
                            if let Some(msg) = upd.get("message") {
                                if let Some(event) = telegram_message_to_event(msg) {
                                    self.handle_message(event).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn stop(&self) {
        *self.running.lock() = false;
    }
}

#[async_trait::async_trait]
impl BasePlatformAdapter for TelegramAdapter {
    fn platform(&self) -> &'static str {
        "telegram"
    }

    async fn connect(&self) -> Result<bool, String> {
        // Lightweight handshake: getMe.
        let resp = self
            .client
            .post(self.url("getMe"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("telegram getMe HTTP {}", resp.status()));
        }
        let json: Value = resp.json().await.map_err(|e| e.to_string())?;
        if json.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err("telegram getMe: ok=false".to_string());
        }
        Ok(true)
    }

    async fn disconnect(&self) -> Result<(), String> {
        self.stop();
        Ok(())
    }

    async fn send(
        &self,
        chat_id: &str,
        text: &str,
        _thread_id: Option<&str>,
    ) -> SendResult {
        let resp = self
            .client
            .post(self.url("sendMessage"))
            .form(&[("chat_id", chat_id), ("text", text)])
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: Value = r.json().await.unwrap_or(Value::Null);
                let mid = json
                    .get("result")
                    .and_then(|r| r.get("message_id"))
                    .and_then(|v| v.as_i64())
                    .map(|n| n.to_string());
                SendResult::ok(mid)
            }
            Ok(r) => SendResult::err(format!("HTTP {}", r.status())),
            Err(e) => SendResult::err(e.to_string()),
        }
    }

    fn set_message_handler(&self, handler: MessageHandler) {
        *self.message_handler.lock() = Some(handler);
    }

    fn set_interaction_handler(&self, handler: InteractionHandler) {
        *self.interaction_handler.lock() = Some(handler);
    }

    async fn handle_message(&self, event: MessageEvent) {
        let handler = self.message_handler.lock().clone();
        if let Some(h) = handler {
            h(event).await;
        }
    }

    async fn handle_interaction(&self, event: InteractionEvent) {
        let handler = self.interaction_handler.lock().clone();
        if let Some(h) = handler {
            h(event).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Email (stub — Gmail API lives in integration.rs)
// ---------------------------------------------------------------------------

/// Email adapter — placeholder. Real inbound / outbound for Gmail lives in
/// `integration::gmail`; this stub exists so the gateway can route `email`
/// platform events without panicking.
pub struct EmailAdapter;

impl EmailAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmailAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BasePlatformAdapter for EmailAdapter {
    fn platform(&self) -> &'static str {
        "email"
    }

    async fn connect(&self) -> Result<bool, String> {
        Ok(false)
    }

    async fn disconnect(&self) -> Result<(), String> {
        Ok(())
    }

    async fn send(
        &self,
        _chat_id: &str,
        _text: &str,
        _thread_id: Option<&str>,
    ) -> SendResult {
        SendResult::err("email adapter is a stub — wire Gmail OAuth in integration.rs")
    }

    fn set_message_handler(&self, _handler: MessageHandler) {}

    fn set_interaction_handler(&self, _handler: InteractionHandler) {}

    async fn handle_message(&self, _event: MessageEvent) {}

    async fn handle_interaction(&self, _event: InteractionEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slack_event_basic() {
        let event = json!({
            "type": "message",
            "channel": "C123",
            "user": "U999",
            "text": "hello world",
            "ts": "1234.567",
            "channel_type": "channel",
        });
        let ev = slack_event_to_event(&event, Some("UBOT")).unwrap();
        assert_eq!(ev.text, "hello world");
        assert_eq!(ev.source.platform, "slack");
        assert_eq!(ev.source.chat_id, "C123");
        assert_eq!(ev.source.chat_type, "channel");
        assert!(!ev.mentions_me);
    }

    #[test]
    fn slack_event_skips_bot_echo() {
        let event = json!({
            "type": "message",
            "bot_id": "B123",
            "text": "ignore me",
            "channel": "C1",
        });
        assert!(slack_event_to_event(&event, None).is_none());
    }

    #[test]
    fn slack_event_skips_subtype() {
        let event = json!({
            "type": "message",
            "subtype": "message_changed",
            "text": "edit",
            "channel": "C1",
        });
        assert!(slack_event_to_event(&event, None).is_none());
    }

    #[test]
    fn slack_event_skips_self_user() {
        let event = json!({
            "type": "message",
            "user": "UBOT",
            "text": "loopback",
            "channel": "C1",
        });
        assert!(slack_event_to_event(&event, Some("UBOT")).is_none());
    }

    #[test]
    fn slack_event_detects_mention() {
        let event = json!({
            "type": "message",
            "channel": "C1",
            "user": "U999",
            "text": "hi <@UBOT> please help",
        });
        let ev = slack_event_to_event(&event, Some("UBOT")).unwrap();
        assert!(ev.mentions_me);
    }

    #[test]
    fn telegram_event_dm() {
        let msg = json!({
            "message_id": 42,
            "text": "ping",
            "chat": {"id": 12345, "type": "private"},
            "from": {"id": 99, "first_name": "Rohit"},
        });
        let ev = telegram_message_to_event(&msg).unwrap();
        assert_eq!(ev.text, "ping");
        assert_eq!(ev.source.platform, "telegram");
        assert_eq!(ev.source.chat_id, "12345");
        assert_eq!(ev.source.chat_type, "dm");
        assert_eq!(ev.source.user_id.as_deref(), Some("99"));
        assert_eq!(ev.message_id.as_deref(), Some("42"));
    }

    #[test]
    fn telegram_event_group() {
        let msg = json!({
            "message_id": 7,
            "text": "group message",
            "chat": {"id": -1001, "type": "supergroup", "title": "team-chat"},
            "from": {"id": 1, "first_name": "Alice"},
            "message_thread_id": 11,
        });
        let ev = telegram_message_to_event(&msg).unwrap();
        assert_eq!(ev.source.chat_type, "group");
        assert_eq!(ev.source.chat_name.as_deref(), Some("team-chat"));
        assert_eq!(ev.source.thread_id.as_deref(), Some("11"));
    }

    #[test]
    fn telegram_event_no_text() {
        let msg = json!({
            "message_id": 1,
            "chat": {"id": 1, "type": "private"},
        });
        assert!(telegram_message_to_event(&msg).is_none());
    }

    #[test]
    fn slack_to_message_source_basic() {
        let event = json!({
            "type": "message",
            "channel": "C123",
            "user": "U1",
            "text": "hi",
            "ts": "1234.5",
            "channel_type": "channel",
        });
        let m = slack_to_message_source(&event, "Bob", "general").unwrap();
        assert_eq!(m.connector, "slack");
        assert_eq!(m.kind, "channel");
        assert_eq!(m.channel_name, "general");
        assert_eq!(m.sender_name, "Bob");
        assert_eq!(m.ts, 1234.5);
    }
}