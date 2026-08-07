//! Inbound messaging gateway — orchestrates platform adapters.
//!
//! Mirrors `coworker/connectors/gateway.py`. This module currently provides the
//! type surface and lifecycle hooks so `ocw-server` can wire start/stop without
//! claiming Slack Socket Mode / Telegram / Email inbound parity yet.
//!
//! Status (Phase D):
//! - Structure: present
//! - Slack Socket Mode / relay inbound: **not yet**
//! - Telegram long-poll: **not yet**
//! - Email polling: **not yet** (adapter remains stub)
//!
//! Callers must treat [`Gateway::start`] returning an empty platform list as
//! "no inbound listeners" — never as silent success for message delivery.

use crate::adapters::{EmailAdapter, SlackAdapter, TelegramAdapter};
use crate::base::{MessageEvent, MessageHandler};
use crate::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Platforms the gateway knows how to host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Slack,
    Telegram,
    Email,
    Github,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::Email => "email",
            Self::Github => "github",
        }
    }
}

/// Lifecycle status for one adapter.
#[derive(Debug, Clone)]
pub struct AdapterStatus {
    pub platform: String,
    pub running: bool,
    pub last_error: Option<String>,
}

/// Inbound message gateway. Owns adapters and dispatches to a shared handler.
pub struct Gateway {
    handler: MessageHandler,
    running: RwLock<HashMap<String, AdapterStatus>>,
    /// Held for future Socket Mode / poll loops — currently unused beyond type surface.
    _slack: Option<SlackAdapter>,
    _telegram: Option<TelegramAdapter>,
    _email: Option<EmailAdapter>,
}

impl Gateway {
    pub fn new(handler: MessageHandler) -> Self {
        Self {
            handler,
            running: RwLock::new(HashMap::new()),
            _slack: None,
            _telegram: None,
            _email: None,
        }
    }

    /// Attempt to start enabled inbound listeners.
    ///
    /// Returns the list of platforms that actually came up. Today this always
    /// returns an empty vec — inbound orchestration is not ported yet. Callers
    /// (and HTTP status endpoints) must report offline honestly.
    pub async fn start(&self, _enabled: &[Platform]) -> Result<Vec<String>, String> {
        let _ = &self.handler;
        Ok(Vec::new())
    }

    pub async fn stop(&self) {
        self.running.write().await.clear();
    }

    pub async fn status(&self) -> Vec<AdapterStatus> {
        self.running.read().await.values().cloned().collect()
    }

    /// Inject an inbound event (tests / debug). Does not imply a live adapter.
    pub async fn inject(&self, event: MessageEvent) {
        (self.handler)(event).await;
    }
}

/// No-op handler used when the server has not registered a real router yet.
pub fn noop_handler() -> MessageHandler {
    Arc::new(|_event: MessageEvent| -> BoxFuture { Box::pin(async move {}) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_reports_no_platforms_yet() {
        let gw = Gateway::new(noop_handler());
        let started = gw
            .start(&[Platform::Slack, Platform::Telegram])
            .await
            .unwrap();
        assert!(started.is_empty());
        assert!(gw.status().await.is_empty());
    }
}
