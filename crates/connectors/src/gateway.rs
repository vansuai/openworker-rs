//! Inbound messaging gateway — orchestrates platform adapters.
//!
//! Mirrors `coworker/connectors/gateway.py` + `manager.py::_build_and_start_gateway`.

use crate::adapters::{SlackAdapter, TelegramAdapter};
use crate::base::{BasePlatformAdapter, MessageEvent, MessageHandler};
use crate::relay::{
    GitHubRelayInbound, RelayHub, SlackRelayInbound, SlackTeamInfo, TokenProvider,
};
use crate::BoxFuture;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

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

struct GatewayState {
    status: HashMap<String, AdapterStatus>,
    tasks: HashMap<String, JoinHandle<()>>,
    telegram: Option<Arc<TelegramAdapter>>,
    slack: Option<Arc<SlackAdapter>>,
    relay_hub: Option<Arc<RelayHub>>,
    relay_slack: Option<Arc<SlackRelayInbound>>,
    relay_github: Option<Arc<GitHubRelayInbound>>,
}

/// Inbound message gateway. Owns adapters and dispatches to a shared handler.
pub struct Gateway {
    handler: MessageHandler,
    /// Tried before the handler (Inbox reply token). Stub for now — always returns false.
    reply_resolver: Option<Arc<dyn Fn(MessageEvent) -> bool + Send + Sync>>,
    relay_url: RwLock<Option<String>>,
    token_provider: RwLock<Option<TokenProvider>>,
    state: RwLock<GatewayState>,
}

impl Gateway {
    pub fn new(handler: MessageHandler) -> Self {
        Self {
            handler,
            reply_resolver: None,
            relay_url: RwLock::new(None),
            token_provider: RwLock::new(None),
            state: RwLock::new(GatewayState {
                status: HashMap::new(),
                tasks: HashMap::new(),
                telegram: None,
                slack: None,
                relay_hub: None,
                relay_slack: None,
                relay_github: None,
            }),
        }
    }

    /// Wire managed-relay endpoint + cloud JWT provider (required for relay mode).
    pub async fn set_relay(
        &self,
        relay_url: String,
        token_provider: TokenProvider,
    ) {
        *self.relay_url.write().await = Some(relay_url);
        *self.token_provider.write().await = Some(token_provider);
    }

    /// Stub hook for Inbox reply resolution (mirrors Python `reply_resolver`).
    pub fn set_reply_resolver(
        &mut self,
        resolver: Arc<dyn Fn(MessageEvent) -> bool + Send + Sync>,
    ) {
        self.reply_resolver = Some(resolver);
    }

    fn inbound_handler(&self) -> MessageHandler {
        let handler = Arc::clone(&self.handler);
        let reply_resolver = self.reply_resolver.clone();
        Arc::new(move |event: MessageEvent| {
            let handler = Arc::clone(&handler);
            let reply_resolver = reply_resolver.clone();
            Box::pin(async move {
                if let Some(resolver) = reply_resolver {
                    if resolver(event.clone()) {
                        return;
                    }
                }
                handler(event).await;
            }) as BoxFuture
        })
    }

    /// Hot-reload listeners from `(platform, profile)` pairs.
    ///
    /// Recognizes:
    /// - `telegram` + `bot_token` → long-poll loop
    /// - `slack` + `bot_token` + `app_token` (non-relay) → Socket Mode
    /// - `slack` + `mode=relay` → managed relay (needs relay URL + sign-in)
    /// - `slack:team:<team_id>` → per-workspace bot token for relay mode
    /// - `github` + `mode=relay` → managed relay
    pub async fn refresh(&self, platforms: Vec<(String, Value)>) -> Result<Vec<String>, String> {
        self.stop_inner().await;

        let mut teams: HashMap<String, SlackTeamInfo> = HashMap::new();
        let mut slack_default: Option<Value> = None;
        let mut github_default: Option<Value> = None;
        let mut telegram_profile: Option<Value> = None;

        for (platform, profile) in &platforms {
            if let Some(team_id) = platform.strip_prefix("slack:team:") {
                if let Some(token) = profile.get("bot_token").and_then(|v| v.as_str()) {
                    if !token.is_empty() {
                        teams.insert(
                            team_id.to_string(),
                            SlackTeamInfo {
                                bot_token: token.to_string(),
                                bot_user_id: profile
                                    .get("bot_user_id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            },
                        );
                    }
                }
                continue;
            }
            match platform.as_str() {
                "telegram" => telegram_profile = Some(profile.clone()),
                "slack" => slack_default = Some(profile.clone()),
                "github" => github_default = Some(profile.clone()),
                _ => {}
            }
        }

        let mut live = Vec::new();
        let inbound = self.inbound_handler();

        // Telegram long-poll
        if let Some(profile) = telegram_profile {
            if let Some(token) = profile.get("bot_token").and_then(|v| v.as_str()) {
                if !token.is_empty() {
                    let adapter = Arc::new(TelegramAdapter::new(token.to_string()));
                    adapter.set_message_handler(inbound.clone());
                    match adapter.connect().await {
                        Ok(true) => {
                            let a = Arc::clone(&adapter);
                            let handle = tokio::spawn(async move { a.run_loop().await });
                            self.mark_running("telegram", handle, Some(adapter), None)
                                .await;
                            live.push("telegram".into());
                        }
                        Ok(false) | Err(_) => {
                            self.mark_error("telegram", "telegram connect failed").await;
                        }
                    }
                }
            }
        }

        // Slack: relay or Socket Mode
        if let Some(profile) = slack_default {
            let mode = profile
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if mode == "relay" {
                if self.start_slack_relay(inbound.clone(), teams).await {
                    live.push("slack".into());
                }
            } else if let (Some(bot), Some(app)) = (
                profile.get("bot_token").and_then(|v| v.as_str()),
                profile.get("app_token").and_then(|v| v.as_str()),
            ) {
                if !bot.is_empty() && !app.is_empty() {
                    if self
                        .start_slack_socket(inbound.clone(), bot.to_string(), app.to_string())
                        .await
                    {
                        live.push("slack".into());
                    }
                }
            }
        }

        // GitHub relay
        if let Some(profile) = github_default {
            let mode = profile
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if mode == "relay" && self.start_github_relay(inbound).await {
                live.push("github".into());
            }
        }

        Ok(live)
    }

    async fn ensure_relay_hub(&self) -> Option<Arc<RelayHub>> {
        let url = self.relay_url.read().await.clone()?;
        if url.is_empty() {
            return None;
        }
        let token_provider = self.token_provider.read().await.clone()?;
        let mut state = self.state.write().await;
        if let Some(hub) = state.relay_hub.clone() {
            return Some(hub);
        }
        let hub = RelayHub::new(url, token_provider);
        state.relay_hub = Some(Arc::clone(&hub));
        Some(hub)
    }

    async fn start_slack_relay(
        &self,
        handler: MessageHandler,
        teams: HashMap<String, SlackTeamInfo>,
    ) -> bool {
        let Some(hub) = self.ensure_relay_hub().await else {
            self.mark_error("slack", "relay URL or cloud sign-in unavailable")
                .await;
            return false;
        };
        let inbound = SlackRelayInbound::new(teams, handler);
        hub.register_handler("slack", inbound.frame_handler()).await;
        if !hub.start().await {
            let err = hub.last_error().await;
            self.mark_error("slack", &err).await;
            return false;
        }
        let mut state = self.state.write().await;
        state.relay_slack = Some(inbound);
        state.status.insert(
            "slack".into(),
            AdapterStatus {
                platform: "slack".into(),
                running: true,
                last_error: None,
            },
        );
        true
    }

    async fn start_slack_socket(
        &self,
        handler: MessageHandler,
        bot_token: String,
        app_token: String,
    ) -> bool {
        let adapter = Arc::new(SlackAdapter::new(bot_token, app_token));
        adapter.set_message_handler(handler);
        match adapter.connect().await {
            Ok(true) => {
                let a = Arc::clone(&adapter);
                let handle = tokio::spawn(async move { a.run_socket_mode_loop().await });
                self.mark_running("slack", handle, None, Some(adapter)).await;
                true
            }
            Ok(false) => {
                self.mark_error("slack", "slack connect returned false").await;
                false
            }
            Err(e) => {
                self.mark_error("slack", &e).await;
                false
            }
        }
    }

    async fn start_github_relay(&self, handler: MessageHandler) -> bool {
        let Some(hub) = self.ensure_relay_hub().await else {
            self.mark_error("github", "relay URL or cloud sign-in unavailable")
                .await;
            return false;
        };
        let inbound = GitHubRelayInbound::new(handler);
        hub.register_handler("github", inbound.frame_handler()).await;
        if !hub.start().await {
            let err = hub.last_error().await;
            self.mark_error("github", &err).await;
            return false;
        }
        let mut state = self.state.write().await;
        state.relay_github = Some(inbound);
        state.status.insert(
            "github".into(),
            AdapterStatus {
                platform: "github".into(),
                running: true,
                last_error: None,
            },
        );
        true
    }

    async fn mark_running(
        &self,
        platform: &str,
        handle: JoinHandle<()>,
        telegram: Option<Arc<TelegramAdapter>>,
        slack: Option<Arc<SlackAdapter>>,
    ) {
        let mut state = self.state.write().await;
        if let Some(t) = telegram {
            state.telegram = Some(t);
        }
        if let Some(s) = slack {
            state.slack = Some(s);
        }
        state.tasks.insert(platform.to_string(), handle);
        state.status.insert(
            platform.to_string(),
            AdapterStatus {
                platform: platform.to_string(),
                running: true,
                last_error: None,
            },
        );
    }

    async fn mark_error(&self, platform: &str, err: &str) {
        let mut state = self.state.write().await;
        state.status.insert(
            platform.to_string(),
            AdapterStatus {
                platform: platform.to_string(),
                running: false,
                last_error: Some(err.to_string()),
            },
        );
    }

    /// Start from platform enum slice (legacy). Prefer [`Gateway::start_from_profiles`].
    pub async fn start(&self, enabled: &[Platform]) -> Result<Vec<String>, String> {
        let profiles: Vec<(String, Value)> = enabled
            .iter()
            .map(|p| (p.as_str().to_string(), Value::Object(Default::default())))
            .collect();
        self.refresh(profiles).await
    }

    /// Start inbound listeners from SecretStore-style profiles.
    pub async fn start_from_profiles(
        &self,
        platforms: Vec<(String, Value)>,
    ) -> Result<Vec<String>, String> {
        self.refresh(platforms).await
    }

    pub async fn stop(&self) {
        self.stop_inner().await;
    }

    async fn stop_inner(&self) {
        let mut state = self.state.write().await;
        if let Some(t) = state.telegram.take() {
            t.stop();
        }
        if let Some(s) = state.slack.take() {
            s.stop();
        }
        if let Some(hub) = state.relay_hub.take() {
            hub.stop().await;
        }
        state.relay_slack = None;
        state.relay_github = None;
        for (_, handle) in state.tasks.drain() {
            handle.abort();
        }
        for status in state.status.values_mut() {
            status.running = false;
        }
    }

    pub async fn status(&self) -> Vec<AdapterStatus> {
        self.state.read().await.status.values().cloned().collect()
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn refresh_without_profiles_reports_empty() {
        let gw = Gateway::new(noop_handler());
        let started = gw.refresh(vec![]).await.unwrap();
        assert!(started.is_empty());
        assert!(gw.status().await.is_empty());
    }

    #[tokio::test]
    async fn stop_clears_running_status() {
        let gw = Gateway::new(noop_handler());
        gw.stop().await;
        assert!(gw.status().await.is_empty());
    }

    #[tokio::test]
    async fn inject_calls_handler() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let handler: MessageHandler = Arc::new(move |_event| {
            let c = Arc::clone(&c);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });
        let gw = Gateway::new(handler);
        let event = MessageEvent {
            text: "hi".into(),
            source: crate::base::SessionSource {
                platform: "telegram".into(),
                chat_id: "1".into(),
                user_id: None,
                user_name: None,
                chat_name: None,
                chat_type: "dm".into(),
                thread_id: None,
                team_id: None,
            },
            message_id: None,
            message_type: crate::base::MessageType::Text,
            reply_to_message_id: None,
            raw: None,
            mentions_me: false,
        };
        gw.inject(event).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
