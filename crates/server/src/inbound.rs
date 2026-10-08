//! Inbound connector routing — allowlist → park → inbox reply → channel/DM fan-out.
//!
//! Mirrors `coworker/connectors/gateway.py::_on_inbound` +
//! `coworker/server/manager.py::_dispatch_inbound` at MVP depth: unauthorized
//! senders are parked; `[ow:]` replies resolve inbox items; channel traffic is
//! buffered and fanned to subscriptions; DMs go to the designated session or
//! the unrouted dead-letter store.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ocw_connectors::{format_target, MessageEvent, MessageHandler};
use ocw_data::{resolve_from_reply, InboxStore};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::state::SettingsManager;
use crate::stores::{ChannelBuffer, SubscriptionStore, UnroutedStore};

pub type ParkedMap = Arc<StdRwLock<HashMap<String, Value>>>;

/// Dependencies filled after `AppState` is constructed (avoids init cycles).
#[derive(Clone)]
pub struct InboundDeps {
    pub settings: SettingsManager,
    pub channel_buffer: Arc<ChannelBuffer>,
    pub subscriptions: Arc<SubscriptionStore>,
    pub unrouted: Arc<UnroutedStore>,
    pub parked: ParkedMap,
    pub inbox_store: Arc<InboxStore>,
    /// Current designated DM session id.
    pub get_dm_session: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Deliver a framed user message into a session (persist + broadcast).
    pub deliver: Arc<dyn Fn(&str /*session*/, &str /*text*/, Value /*source*/) + Send + Sync>,
    /// Whether `session` still accepts inbound from `connector`.
    pub connector_allowed:
        Arc<dyn Fn(&str /*session*/, &str /*connector*/) -> bool + Send + Sync>,
}

pub type InboundSlot = Arc<StdRwLock<Option<InboundDeps>>>;

pub fn new_slot() -> InboundSlot {
    Arc::new(StdRwLock::new(None))
}

/// Install deps (called once from `AppState::new`).
pub fn install(slot: &InboundSlot, deps: InboundDeps) {
    *slot.write().unwrap() = Some(deps);
}

/// Build the gateway message handler. Until [`InboundDeps`] is installed the
/// handler still buffers channel traffic so catch-up history is not lost.
pub fn make_handler(slot: InboundSlot, fallback_buffer: Arc<ChannelBuffer>) -> MessageHandler {
    Arc::new(move |event: MessageEvent| {
        let slot = Arc::clone(&slot);
        let fallback_buffer = Arc::clone(&fallback_buffer);
        Box::pin(async move {
            let deps = slot.read().unwrap().clone();
            if let Some(deps) = deps {
                dispatch(&deps, event).await;
            } else {
                let src = &event.source;
                let channel = format!("{}:{}", src.platform, src.chat_id);
                let who = src
                    .user_name
                    .as_deref()
                    .or(src.user_id.as_deref())
                    .unwrap_or("unknown");
                fallback_buffer.record(&channel, who, &event.text, src.chat_name.as_deref());
                tracing::info!(
                    platform = %src.platform,
                    "inbound before router wired — buffered only"
                );
            }
        })
    })
}

async fn is_authorized(settings: &SettingsManager, event: &MessageEvent) -> bool {
    let platform = &event.source.platform;
    let profile = settings
        .secrets_get(&format!("{platform}:default"))
        .await
        .unwrap_or_default();

    // Managed Slack: per-team allow lists.
    if let Some(team_id) = event.source.team_id.as_deref() {
        if !team_id.is_empty() {
            let team = settings
                .secrets_get(&format!("slack:team:{team_id}"))
                .await
                .unwrap_or_default();
            if team.is_empty() && platform == "slack" {
                return false;
            }
            if team
                .get("allow_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return true;
            }
            let allowed = team
                .get("allowed_users")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let uid = event.source.user_id.as_deref().unwrap_or("");
            return !uid.is_empty()
                && allowed.iter().any(|v| v.as_str() == Some(uid));
        }
    }

    if profile
        .get("allow_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let allowed = profile
        .get("allowed_users")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let uid = event.source.user_id.as_deref().unwrap_or("");
    !uid.is_empty() && allowed.iter().any(|v| v.as_str() == Some(uid))
}

fn park_unauthorized(parked: &ParkedMap, event: &MessageEvent) {
    let id = Uuid::new_v4().to_string();
    let s = &event.source;
    let mut item = Map::new();
    item.insert("id".into(), json!(id.clone()));
    item.insert("platform".into(), json!(s.platform));
    item.insert("chat_id".into(), json!(s.chat_id));
    item.insert("chat_name".into(), json!(s.chat_name));
    item.insert("user_id".into(), json!(s.user_id.clone().unwrap_or_else(|| "?".into())));
    item.insert("user_name".into(), json!(s.user_name));
    item.insert("chat_type".into(), json!(s.chat_type));
    item.insert("thread_id".into(), json!(s.thread_id));
    item.insert("team_id".into(), json!(s.team_id));
    item.insert("text".into(), json!(event.text));
    item.insert(
        "ts".into(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)),
    );
    parked.write().unwrap().insert(id, Value::Object(item));
}

fn message_source_json(event: &MessageEvent) -> Value {
    let s = &event.source;
    let kind = if s.chat_type == "channel" || s.chat_type == "group" {
        "channel"
    } else {
        "dm"
    };
    json!({
        "connector": s.platform,
        "kind": kind,
        "channel_id": s.chat_id,
        "channel_name": s.chat_name.as_deref().unwrap_or(&s.chat_id),
        "sender_id": s.user_id.as_deref().unwrap_or(""),
        "sender_name": s.user_name.as_deref().or(s.user_id.as_deref()).unwrap_or("?"),
        "ts": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        "text": event.text,
    })
}

async fn dispatch(deps: &InboundDeps, event: MessageEvent) {
    if !is_authorized(&deps.settings, &event).await {
        tracing::info!(
            platform = %event.source.platform,
            user = ?event.source.user_id,
            "parking unauthorized inbound"
        );
        park_unauthorized(&deps.parked, &event);
        return;
    }

    // Inbox reply-token consume (approve/deny via channel).
    let resolved = resolve_from_reply(&event.text, |item_id, resolution| {
        deps.inbox_store.resolve(item_id, resolution)
    });
    if resolved == Some(true) {
        tracing::info!("inbound consumed as inbox reply");
        return;
    }

    let src = &event.source;
    let text = event.text.as_str();
    let who = src
        .user_name
        .as_deref()
        .or(src.user_id.as_deref())
        .unwrap_or("?");
    let channel = format!("{}:{}", src.platform, src.chat_id);
    let ms = message_source_json(&event);

    if src.chat_type == "channel" || src.chat_type == "group" {
        deps.channel_buffer
            .record(&channel, who, text, src.chat_name.as_deref());
        let subs = deps.subscriptions.for_channel(&channel);

        if event.mentions_me {
            let thread_key = src
                .thread_id
                .clone()
                .or_else(|| event.message_id.clone());
            let thread_target =
                format_target(&src.platform, &src.chat_id, thread_key.as_deref());
            let chan = src
                .chat_name
                .as_ref()
                .map(|n| format!("#{n}"))
                .unwrap_or_else(|| src.chat_id.clone());
            let msg = if subs.is_empty() {
                format!(
                    "🔔 You were tagged by {who} in {chan}: {text}\n\
                     (Reply in the thread with the send_message tool, target \"{thread_target}\".)"
                )
            } else {
                format!(
                    "🔔 You were tagged by {who} in {chan}: {text}\n\
                     (You are subscribed to this channel and were mentioned directly — you must \
                     respond. Reply in the thread with the send_message tool, target \
                     \"{thread_target}\".)"
                )
            };
            if subs.is_empty() {
                // No subscription yet — park as unrouted mention for visibility.
                deps.unrouted.record(
                    &format_target(&src.platform, &src.chat_id, src.thread_id.as_deref()),
                    who,
                    text,
                    "mention with no subscribed session",
                );
            } else {
                for sub in &subs {
                    if !(deps.connector_allowed)(&sub.session_id, &src.platform) {
                        continue;
                    }
                    (deps.deliver)(&sub.session_id, &msg, ms.clone());
                }
            }
            return;
        }

        if subs.is_empty() {
            return;
        }
        let msg = format!(
            "💬 New message on {} from {who}: {text}\n\
             (You're subscribed to this channel but were NOT mentioned. Use your judgement: \
             stay silent unless the message clearly concerns your job. If you reply, use \
             send_message with target \"{channel}\".)",
            src.chat_name.as_deref().unwrap_or(&channel)
        );
        for sub in &subs {
            if !(deps.connector_allowed)(&sub.session_id, &src.platform) {
                continue;
            }
            (deps.deliver)(&sub.session_id, &msg, ms.clone());
        }
        return;
    }

    // DM / non-channel
    let dm = (deps.get_dm_session)();
    if let Some(ref sid) = dm {
        if (deps.connector_allowed)(sid, &src.platform) {
            (deps.deliver)(sid, &event.tagged_text(), ms);
        } else {
            deps.unrouted.record(
                &src.target(),
                who,
                text,
                "connector muted for DM session",
            );
        }
    } else {
        deps.unrouted
            .record(&src.target(), who, text, "no DM session designated");
    }
}

/// Re-inject a parked message through the normal inbound path (allow_deliver).
pub async fn reinject(slot: &InboundSlot, event: MessageEvent) {
    let deps = slot.read().unwrap().clone();
    if let Some(deps) = deps {
        dispatch(&deps, event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocw_connectors::{MessageType, SessionSource};

    #[test]
    fn park_stores_item() {
        let parked: ParkedMap = Arc::new(StdRwLock::new(HashMap::new()));
        let event = MessageEvent {
            text: "hi".into(),
            source: SessionSource {
                platform: "telegram".into(),
                chat_id: "1".into(),
                user_id: Some("U1".into()),
                user_name: Some("alice".into()),
                chat_name: None,
                chat_type: "dm".into(),
                thread_id: None,
                team_id: None,
            },
            message_id: None,
            message_type: MessageType::Text,
            reply_to_message_id: None,
            raw: None,
            mentions_me: false,
        };
        park_unauthorized(&parked, &event);
        assert_eq!(parked.read().unwrap().len(), 1);
        let item = parked.read().unwrap().values().next().unwrap().clone();
        assert_eq!(item["platform"], "telegram");
        assert_eq!(item["user_id"], "U1");
        assert_eq!(item["text"], "hi");
    }
}
