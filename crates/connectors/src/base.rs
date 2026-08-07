//! Messaging connector core — the platform-agnostic adapter contract + value types.
//!
//! Mirrors `coworker/connectors/base.py`. An adapter connects to a platform
//! (Slack/Telegram/...), receives inbound messages and dispatches them via
//! [`BasePlatformAdapter::handle_message`], and can [`BasePlatformAdapter::send`]
//! outbound. Inbound identity is carried by [`SessionSource`]; a `target` token
//! (`platform:chat_id[:thread]`) is the opaque handle the agent passes back to
//! reply.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// MessageType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    Text,
    Command,
    Media,
}

impl MessageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageType::Text => "text",
            MessageType::Command => "command",
            MessageType::Media => "media",
        }
    }
}

// ---------------------------------------------------------------------------
// Target tokens
// ---------------------------------------------------------------------------

/// Format a target token: `"platform:chat_id[:thread]"`.
pub fn format_target(platform: &str, chat_id: &str, thread_id: Option<&str>) -> String {
    let base = format!("{platform}:{chat_id}");
    match thread_id {
        Some(t) => format!("{base}:{t}"),
        None => base,
    }
}

/// Parse a target token: `"platform:chat_id[:thread]"` → `(platform, chat_id, thread_id)`.
pub fn parse_target(target: &str) -> Result<(String, String, Option<String>), String> {
    let parts: Vec<&str> = target.split(':').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!(
            "invalid target {target:?} (expected 'platform:chat_id[:thread]')"
        ));
    }
    let thread = if parts.len() > 2 {
        Some(parts[2..].join(":"))
    } else {
        None
    };
    Ok((parts[0].to_string(), parts[1].to_string(), thread))
}

// ---------------------------------------------------------------------------
// SessionSource
// ---------------------------------------------------------------------------

/// Inbound message identity — where a message came from. The `target` token is
/// what the agent passes back to `send_message` so the reply lands on the same
/// surface (and thread, if any).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: String,
    pub chat_id: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub chat_name: Option<String>,
    /// "dm" | "group" | "channel"
    pub chat_type: String,
    pub thread_id: Option<String>,
    pub team_id: Option<String>,
}

impl SessionSource {
    pub fn target(&self) -> String {
        format_target(&self.platform, &self.chat_id, self.thread_id.as_deref())
    }

    /// Short human label: `"slack channel · @Rohit"`.
    pub fn label(&self) -> String {
        let who = self.user_name.clone().unwrap_or_else(|| {
            self.user_id
                .clone()
                .unwrap_or_else(|| "?".to_string())
        });
        let where_ = match self.chat_type.as_str() {
            "dm" => "DM",
            "group" => "group",
            "channel" => "channel",
            other => other,
        };
        format!("{} {} · {}", self.platform, where_, who)
    }
}

// ---------------------------------------------------------------------------
// MessageSource
// ---------------------------------------------------------------------------

/// Structured sidecar for a connector inbound message (UI-REFRESH §3.1).
///
/// Attached (as a plain dict via `to_dict`) to the persisted user message for
/// DISPLAY only — the GUI renders a rich card from it. The model-facing
/// `content` stays the framed text and this sidecar is stripped before the
/// message reaches any provider. `text` is the RAW message (what the card
/// shows), distinct from the framed `content`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSource {
    pub connector: String,
    /// "channel" | "dm"
    pub kind: String,
    pub channel_id: String,
    /// Resolved display name; falls back to channel_id.
    pub channel_name: String,
    pub sender_id: String,
    /// Resolved display name; falls back to sender_id.
    pub sender_name: String,
    /// Epoch seconds.
    pub ts: f64,
    /// RAW message (what the card shows).
    pub text: String,
}

impl MessageSource {
    pub fn to_dict(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

// ---------------------------------------------------------------------------
// MessageEvent
// ---------------------------------------------------------------------------

/// Inbound platform message, normalized across adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEvent {
    pub text: String,
    pub source: SessionSource,
    pub message_id: Option<String>,
    pub message_type: MessageType,
    pub reply_to_message_id: Option<String>,
    /// Original raw event from the platform SDK (kept for debugging / adapter-specific lookups).
    pub raw: Option<serde_json::Value>,
    /// The bot itself was @-mentioned. Computed from the RAW platform text at
    /// mapping time — mention tokens are rewritten for display afterwards.
    pub mentions_me: bool,
}

impl MessageEvent {
    /// How the message enters the super-agent thread: source + reply handle + text.
    ///
    /// The local GUI owner (`"gui"`) is answered with plain assistant text (no
    /// `send_message`); messaging platforms carry a reply handle the agent
    /// passes back to `send_message`.
    pub fn tagged_text(&self) -> String {
        if self.source.platform == "gui" {
            format!("[Owner, in the app]: {}", self.text)
        } else {
            format!("[{} | reply→{}]: {}", self.source.label(), self.source.target(), self.text)
        }
    }
}

// ---------------------------------------------------------------------------
// SendResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub ok: bool,
    pub message_id: Option<String>,
    pub error: Option<String>,
}

impl SendResult {
    pub fn ok(message_id: Option<String>) -> Self {
        Self {
            ok: true,
            message_id,
            error: None,
        }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message_id: None,
            error: Some(message.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// InteractionEvent — button click on an interactive prompt
// ---------------------------------------------------------------------------

/// A button click on an interactive prompt (Slack "Allow" / "Deny" button,
/// HubSpot inline approval, ...).
///
/// Stable actor/workspace ids are security inputs; display names are
/// presentation only. `response_url` is Slack's short-lived reply capability
/// for a private rejection notice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionEvent {
    pub platform: String,
    pub chat_id: String,
    /// The clicked message's id/ts (to update it).
    pub message_id: Option<String>,
    pub value: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub team_id: Option<String>,
    pub response_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Adapter contract
// ---------------------------------------------------------------------------

/// Async message handler (set by the gateway on each adapter). Wrapped in an
/// `Arc` so adapters can stash it behind a `Mutex` and still clone it out for
/// the dispatch call without taking the lock for the full call duration.
pub type MessageHandler = std::sync::Arc<dyn Fn(MessageEvent) -> BoxFuture + Send + Sync>;

/// Async interaction handler.
pub type InteractionHandler = std::sync::Arc<dyn Fn(InteractionEvent) -> BoxFuture + Send + Sync>;

/// Type-erased boxed future for handler return types.
pub type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// One messaging platform. Subclasses implement `connect`/`disconnect`/`send`
/// and call `handle_message` for inbound events.
#[async_trait::async_trait]
pub trait BasePlatformAdapter: Send + Sync {
    fn platform(&self) -> &'static str;

    async fn connect(&self) -> Result<bool, String>;
    async fn disconnect(&self) -> Result<(), String>;

    /// Send an outbound message.
    async fn send(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
    ) -> SendResult;

    /// Default: plain text (adapters without interactive support just show the
    /// text — the user answers in the app).
    async fn send_interactive(
        &self,
        chat_id: &str,
        text: &str,
        _buttons: &[ButtonSpec],
        thread_id: Option<&str>,
    ) -> SendResult {
        self.send(chat_id, text, thread_id).await
    }

    fn set_message_handler(&self, handler: MessageHandler);
    fn set_interaction_handler(&self, handler: InteractionHandler);

    async fn handle_message(&self, event: MessageEvent) {
        // Adapters may keep their handler behind a Mutex — but since this trait
        // is dyn-compatible we cannot guarantee it. Callers wire the handler in
        // via the concrete adapter; default no-op behavior is to do nothing.
        let _ = event;
    }

    async fn handle_interaction(&self, event: InteractionEvent) {
        let _ = event;
    }
}

/// Minimal button spec shared by interactive adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonSpec {
    pub text: String,
    pub value: String,
    /// "primary" | "danger" | "default"
    #[serde(default)]
    pub style: Option<String>,
}

impl fmt::Display for SendResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.error {
            Some(e) => write!(f, "SendResult(error: {})", e),
            None => write!(f, "SendResult(ok, id={:?})", self.message_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_target() {
        let t = format_target("slack", "C123", Some("123.456"));
        assert_eq!(t, "slack:C123:123.456");
        let (p, c, th) = parse_target(&t).unwrap();
        assert_eq!(p, "slack");
        assert_eq!(c, "C123");
        assert_eq!(th.as_deref(), Some("123.456"));

        let t2 = format_target("telegram", "12345", None);
        assert_eq!(t2, "telegram:12345");
        let (p, c, th) = parse_target(&t2).unwrap();
        assert_eq!(p, "telegram");
        assert_eq!(c, "12345");
        assert!(th.is_none());
    }

    #[test]
    fn invalid_target() {
        assert!(parse_target("slack").is_err());
        assert!(parse_target(":C123").is_err());
        assert!(parse_target("slack:").is_err());
    }

    #[test]
    fn session_source_label() {
        let s = SessionSource {
            platform: "slack".into(),
            chat_id: "C123".into(),
            user_id: Some("U123".into()),
            user_name: Some("Rohit".into()),
            chat_name: None,
            chat_type: "channel".into(),
            thread_id: None,
            team_id: None,
        };
        assert_eq!(s.label(), "slack channel · Rohit");
        assert_eq!(s.target(), "slack:C123");
    }

    #[test]
    fn message_event_tagged_text() {
        let gui = SessionSource {
            platform: "gui".into(),
            chat_id: "local".into(),
            user_id: None,
            user_name: None,
            chat_name: None,
            chat_type: "dm".into(),
            thread_id: None,
            team_id: None,
        };
        let ev = MessageEvent {
            text: "hello".into(),
            source: gui,
            message_id: None,
            message_type: MessageType::Text,
            reply_to_message_id: None,
            raw: None,
            mentions_me: false,
        };
        assert_eq!(ev.tagged_text(), "[Owner, in the app]: hello");

        let slack_src = SessionSource {
            platform: "slack".into(),
            chat_id: "C123".into(),
            user_id: Some("U1".into()),
            user_name: Some("Bob".into()),
            chat_name: None,
            chat_type: "dm".into(),
            thread_id: Some("1234.5".into()),
            team_id: None,
        };
        let ev2 = MessageEvent {
            text: "ping".into(),
            source: slack_src,
            message_id: None,
            message_type: MessageType::Text,
            reply_to_message_id: None,
            raw: None,
            mentions_me: false,
        };
        assert_eq!(ev2.tagged_text(), "[slack DM · Bob | reply→slack:C123:1234.5]: ping");
    }
}