//! Managed cloud relay hub — one authenticated WebSocket fans frames by `provider`.
//!
//! Mirrors `coworker/connectors/relay_client.py`. Adapters register per-provider
//! handlers; the hub owns reconnect and transport.

use crate::adapters::{slack_event_to_event, SlackAdapter};
use crate::base::{MessageEvent, MessageHandler, SessionSource};
use crate::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub type TokenProvider = Arc<dyn Fn() -> String + Send + Sync>;
pub type RelayFrameHandler = Arc<dyn Fn(Value) -> BoxFuture + Send + Sync>;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Team-qualified Slack reply handle: `T…/C…`.
pub fn qualify_slack_chat(team_id: &str, channel: &str) -> String {
    if team_id.is_empty() {
        channel.to_string()
    } else {
        format!("{team_id}/{channel}")
    }
}

/// The ONE desktop↔cloud relay socket, shared by every provider adapter.
pub struct RelayHub {
    relay_url: String,
    token_provider: TokenProvider,
    handlers: RwLock<HashMap<String, RelayFrameHandler>>,
    closing: watch::Sender<bool>,
    closing_rx: watch::Receiver<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    connected: RwLock<bool>,
    last_error: RwLock<String>,
    connections: Mutex<u32>,
}

impl RelayHub {
    pub fn new(relay_url: String, token_provider: TokenProvider) -> Arc<Self> {
        let (closing, closing_rx) = watch::channel(false);
        Arc::new(Self {
            relay_url,
            token_provider,
            handlers: RwLock::new(HashMap::new()),
            closing,
            closing_rx,
            task: Mutex::new(None),
            connected: RwLock::new(false),
            last_error: RwLock::new(String::new()),
            connections: Mutex::new(0),
        })
    }

    pub async fn register_handler(&self, provider: &str, handler: RelayFrameHandler) {
        self.handlers
            .write()
            .await
            .insert(provider.to_string(), handler);
    }

    pub async fn release(&self, provider: &str) {
        self.handlers.write().await.remove(provider);
        if self.handlers.read().await.is_empty() {
            self.stop().await;
        }
    }

    /// Open the socket (idempotent). Returns true when up or already running.
    pub async fn start(self: &Arc<Self>) -> bool {
        if self.task.lock().as_ref().is_some_and(|t| !t.is_finished()) {
            return true;
        }
        let _ = self.closing.send(false);
        match self.connect_once().await {
            Ok(()) => {
                *self.connections.lock() = 1;
                *self.connected.write().await = true;
                *self.last_error.write().await = String::new();
                let hub = Arc::clone(self);
                let handle = tokio::spawn(async move { hub.run_loop().await });
                *self.task.lock() = Some(handle);
                true
            }
            Err(e) => {
                *self.last_error.write().await = e;
                false
            }
        }
    }

    pub async fn stop(&self) {
        let _ = self.closing.send(true);
        *self.connected.write().await = false;
        if let Some(handle) = self.task.lock().take() {
            handle.abort();
        }
    }

    pub async fn is_connected(&self) -> bool {
        *self.connected.read().await
    }

    pub async fn last_error(&self) -> String {
        self.last_error.read().await.clone()
    }

    fn relay_request(relay_url: &str, token: &str) -> Result<Request<()>, String> {
        if token.is_empty() {
            return Err("relay: empty cloud sign-in token".into());
        }
        let auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("invalid auth header: {e}"))?;
        Request::builder()
            .uri(relay_url)
            .header(AUTHORIZATION, auth)
            .body(())
            .map_err(|e| e.to_string())
    }

    async fn connect_once(&self) -> Result<(), String> {
        let token = (self.token_provider)();
        let request = Self::relay_request(&self.relay_url, &token)?;
        let (_ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| e.to_string())?;
        // Connection validated; run_loop opens a fresh socket for the read loop.
        Ok(())
    }

    async fn open_transport(
        relay_url: &str,
        token_provider: &TokenProvider,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    > {
        let token = token_provider();
        let request = Self::relay_request(relay_url, &token)?;
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| e.to_string())?;
        Ok(ws)
    }

    async fn run_loop(self: Arc<Self>) {
        let mut closing_rx = self.closing_rx.clone();
        let mut ws = match Self::open_transport(&self.relay_url, &self.token_provider).await {
            Ok(w) => w,
            Err(e) => {
                *self.last_error.write().await = e;
                *self.connected.write().await = false;
                return;
            }
        };
        *self.connected.write().await = true;
        *self.last_error.write().await = String::new();

        loop {
            if *closing_rx.borrow() {
                break;
            }
            tokio::select! {
                msg = ws.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Ok(frame) = serde_json::from_str::<Value>(&text) {
                                let provider = frame
                                    .get("provider")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("slack")
                                    .to_string();
                                let handler = self.handlers.read().await.get(&provider).cloned();
                                if let Some(h) = handler {
                                    h(frame).await;
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(payload))) => {
                            let _ = ws.send(WsMessage::Pong(payload)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            *self.connected.write().await = false;
                            if *closing_rx.borrow() {
                                break;
                            }
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            match Self::open_transport(&self.relay_url, &self.token_provider).await {
                                Ok(new_ws) => {
                                    ws = new_ws;
                                    *self.connections.lock() += 1;
                                    *self.connected.write().await = true;
                                    *self.last_error.write().await = String::new();
                                }
                                Err(e) => {
                                    *self.last_error.write().await = e;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            *self.last_error.write().await = e.to_string();
                            *self.connected.write().await = false;
                            if *closing_rx.borrow() {
                                break;
                            }
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            if let Ok(new_ws) =
                                Self::open_transport(&self.relay_url, &self.token_provider).await
                            {
                                ws = new_ws;
                                *self.connections.lock() += 1;
                                *self.connected.write().await = true;
                                *self.last_error.write().await = String::new();
                            }
                        }
                        _ => {}
                    }
                }
                _ = closing_rx.changed() => {
                    if *closing_rx.borrow() {
                        break;
                    }
                }
            }
        }
        *self.connected.write().await = false;
    }
}

// ---------------------------------------------------------------------------
// Slack managed relay inbound
// ---------------------------------------------------------------------------

/// Slack frames from the managed relay → [`MessageEvent`].
pub struct SlackRelayInbound {
    teams: Mutex<HashMap<String, SlackTeamInfo>>,
    slack_api: SlackAdapter,
    handler: MessageHandler,
}

#[derive(Clone)]
pub struct SlackTeamInfo {
    pub bot_token: String,
    pub bot_user_id: Option<String>,
}

impl SlackRelayInbound {
    pub fn new(
        teams: HashMap<String, SlackTeamInfo>,
        handler: MessageHandler,
    ) -> Arc<Self> {
        let bot_token = teams
            .values()
            .next()
            .map(|t| t.bot_token.clone())
            .unwrap_or_default();
        Arc::new(Self {
            teams: Mutex::new(teams),
            slack_api: SlackAdapter::new(bot_token, String::new()),
            handler,
        })
    }

    pub fn frame_handler(self: &Arc<Self>) -> RelayFrameHandler {
        let this = Arc::clone(self);
        Arc::new(move |frame| {
            let this = Arc::clone(&this);
            Box::pin(async move { this.dispatch(frame).await })
        })
    }

    async fn dispatch(&self, frame: Value) {
        let kind = frame.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "revoked" => {
                let team_id = frame.get("team_id").and_then(|v| v.as_str()).unwrap_or("");
                self.teams.lock().remove(team_id);
            }
            "interactivity" | "missed" => {
                // MVP: skip history replay and button handling.
            }
            _ => {
                let team_id = frame.get("team_id").and_then(|v| v.as_str()).unwrap_or("");
                let event = frame.get("event").cloned().unwrap_or(Value::Null);
                self.dispatch_slack_event(team_id, event).await;
            }
        }
    }

    async fn dispatch_slack_event(&self, team_id: &str, event: Value) {
        let bot_user_id = self
            .teams
            .lock()
            .get(team_id)
            .and_then(|t| t.bot_user_id.clone());
        let mut mapped = match slack_event_to_event(&event, bot_user_id.as_deref()) {
            Some(e) => e,
            None => return,
        };
        let channel = mapped.source.chat_id.clone();
        if mapped.source.user_name.is_none() {
            mapped.source.user_name = self.slack_api.resolve_user_name(mapped.source.user_id.as_deref()).await;
        }
        if mapped.source.chat_name.is_none() {
            mapped.source.chat_name = self.slack_api.resolve_channel_name(Some(&channel)).await;
        }
        mapped.text = self.slack_api.resolve_mentions(&mapped.text).await;
        mapped.source.chat_id = qualify_slack_chat(team_id, &channel);
        mapped.source.team_id = Some(team_id.to_string());
        (self.handler)(mapped).await;
    }
}

// ---------------------------------------------------------------------------
// GitHub managed relay inbound
// ---------------------------------------------------------------------------

pub struct GitHubRelayInbound {
    handler: MessageHandler,
}

impl GitHubRelayInbound {
    pub fn new(handler: MessageHandler) -> Arc<Self> {
        Arc::new(Self { handler })
    }

    pub fn frame_handler(self: &Arc<Self>) -> RelayFrameHandler {
        let this = Arc::clone(self);
        Arc::new(move |frame| {
            let this = Arc::clone(&this);
            Box::pin(async move { this.dispatch(frame).await })
        })
    }

    async fn dispatch(&self, frame: Value) {
        let kind = frame.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "revoked" || kind == "missed" {
            return;
        }
        let installation_id = frame
            .get("installation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let owner_repo = frame.get("owner_repo").and_then(|v| v.as_str()).unwrap_or("");
        if owner_repo.is_empty() {
            return;
        }
        let number = frame.get("number").and_then(|v| v.as_str()).unwrap_or("");
        let chat_id = if number.is_empty() {
            owner_repo.to_string()
        } else {
            format!("{owner_repo}#{number}")
        };
        let title = frame.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let body = frame.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let event_kind = frame.get("kind").and_then(|v| v.as_str()).unwrap_or("mention");
        let header = if title.is_empty() {
            format!("[{event_kind} in {owner_repo}#{number}]")
        } else {
            format!("[{event_kind} in {owner_repo}#{number}: {title}]")
        };
        let text = format!("{header} {body}").trim().to_string();
        let sender = frame.get("sender").and_then(|v| v.as_str()).unwrap_or("");
        let event = MessageEvent {
            text,
            source: SessionSource {
                platform: "github".into(),
                chat_id,
                user_id: Some(sender.to_string()),
                user_name: Some(sender.to_string()),
                chat_name: None,
                chat_type: "channel".into(),
                thread_id: None,
                team_id: Some(installation_id.to_string()),
            },
            message_id: None,
            message_type: crate::base::MessageType::Text,
            reply_to_message_id: None,
            raw: Some(frame),
            mentions_me: false,
        };
        (self.handler)(event).await;
    }
}
