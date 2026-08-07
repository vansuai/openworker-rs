//! Global event WebSocket — mirrors Python `/ws/events`.
//!
//! Clients connect to receive server-wide events:
//! - `automation_run_started` / `automation_run_finished`
//! - `health_check` results
//! - Connector self-test events

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tracing;

use crate::state::AppState;

pub async fn ws_events_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events(socket, state))
}

async fn handle_events(ws: WebSocket, state: AppState) {
    let mut ws = ws;
    let mut rx = state.event_broadcast.subscribe();

    // Send a welcome frame so GUI knows the stream is live.
    let welcome = serde_json::json!({
        "type": "ready",
        "data": {
            "stream": "events",
            "model": state.default_model_or_configured(),
        }
    });
    let welcome_str = serde_json::to_string(&welcome).unwrap_or_default();
    if ws.send(Message::Text(welcome_str.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(d))) => {
                        let _ = ws.send(Message::Pong(d)).await;
                    }
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(value) => {
                        let text = serde_json::to_string(&value).unwrap_or_default();
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("events ws lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
