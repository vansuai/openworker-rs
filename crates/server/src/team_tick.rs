//! Team wake plumbing — drain board feeds into lead/worker sessions.
//!
//! Port of `SessionManager.team_tick` / `_team_digest` / `_board_source` from
//! `coworker/server/manager.py`. After board mutations, [`kick_team_tick`]
//! schedules a drain so digests reach sessions now (not only on the 30s tick).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use ocw_data::{Actor, Role, Team, TeamStore};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::state::AppState;

/// Automatic wakes per team per UTC hour (mirrors Python `TEAM_WAKE_CAP_PER_HOUR`).
pub const TEAM_WAKE_CAP_PER_HOUR: i32 = 60;

const DIGEST_CLAMP_MODEL: usize = 300;
const DIGEST_CLAMP_UI: usize = 600;

/// Sessions currently being delivered to (skip-on-overlap).
static TEAM_INFLIGHT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn clamp(text: &str, limit: usize, suffix: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit).collect();
    format!("{}{suffix}", truncated.trim_end())
}

fn user_actor() -> Actor {
    Actor::new("user", Role::User)
}

/// Display-only MessageSource sidecar for board deliveries.
pub fn board_source(team: &Team, message: &str, rows: &[Value]) -> Value {
    json!({
        "connector": "board",
        "kind": "channel",
        "channel_id": team.space,
        "channel_name": "Team board",
        "sender_id": "board",
        "sender_name": "Board",
        "ts": epoch_now(),
        "text": message,
        "board": { "rows": rows },
    })
}

fn roster_note(team: &Team) -> String {
    if team.workers.is_empty() {
        return String::new();
    }
    let mates: Vec<String> = team
        .workers
        .iter()
        .map(|w| {
            if w.reason.is_empty() {
                format!("{} ({})", w.actor, w.persona)
            } else {
                format!("{} ({} — {})", w.actor, w.persona, w.reason)
            }
        })
        .collect();
    let reach = if team.chat_enabled {
        " Reach them or the lead with @name in # team chat (post_chat)."
    } else {
        " Coordinate through item comments; the lead reads the board."
    };
    format!(
        "\n\nYour team: {}; lead (coordinator).{reach}",
        mates.join("; ")
    )
}

/// Coalesce one queue batch into (model text, structured BoardWake rows).
pub fn team_digest(
    store: &TeamStore,
    team: &Team,
    directs: &[Value],
    subs: &[Value],
    chats: &[Value],
    is_lead: bool,
    reader: &str,
) -> (String, Vec<Value>) {
    let mut lines: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let viewer = user_actor();

    for event in directs.iter().chain(subs.iter()) {
        let item_id = event.get("item_id").and_then(|i| i.as_i64());
        let payload = event.get("payload").cloned().unwrap_or(json!({}));
        let actor = event.get("actor").and_then(|a| a.as_str()).unwrap_or("");
        let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");

        let item = item_id.and_then(|id| store.get_item(&team.space, id, &viewer).ok());
        let title = match (&item, item_id) {
            (Some(it), Some(id)) => format!("#{id} {}", it.title),
            (_, Some(id)) => format!("#{id}"),
            _ => "#?".into(),
        };
        let mut row = json!({
            "item": item_id,
            "title": item.as_ref().map(|i| i.title.as_str()).unwrap_or(""),
            "actor": actor,
        });

        match kind {
            "item_assigned" => {
                let Some(item) = item else { continue };
                let assignee = payload
                    .get("assignee")
                    .and_then(|a| a.as_str())
                    .unwrap_or("");
                if payload
                    .get("claimed")
                    .and_then(|c| c.as_bool())
                    .unwrap_or(false)
                {
                    lines.push(format!(
                        "{actor} claimed {title} — it's theirs now; reassign or cancel if that's wrong."
                    ));
                    row["kind"] = json!("claimed");
                    rows.push(row);
                    continue;
                }
                let previous = payload
                    .get("previous")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                if !reader.is_empty() && previous == reader && assignee != reader {
                    lines.push(format!(
                        "{title} was reassigned to {assignee} by {actor} — stop any work on it; hand off context via a comment if useful."
                    ));
                    row["kind"] = json!("assigned");
                    row["assignee"] = json!(assignee);
                    rows.push(row);
                    continue;
                }
                if !reader.is_empty() && assignee != reader {
                    lines.push(format!("{title} assigned to {assignee} by {actor}"));
                    row["kind"] = json!("assigned");
                    row["assignee"] = json!(assignee);
                    rows.push(row);
                    continue;
                }
                let mut line = format!(
                    "You've been assigned work item {title}.\n  Done when: {}",
                    item.criteria
                );
                if !item.description.is_empty() {
                    line.push_str(&format!("\n  Details: {}", item.description));
                }
                lines.push(line);
                row["kind"] = json!("assigned");
                row["assignee"] = json!(assignee);
                rows.push(row);
            }
            "item_transitioned" => {
                let to = payload.get("to").and_then(|t| t.as_str()).unwrap_or("?");
                let comment = clamp(
                    payload.get("comment").and_then(|c| c.as_str()).unwrap_or(""),
                    DIGEST_CLAMP_MODEL,
                    " … (full text on the board)",
                );
                let note = if comment.is_empty() {
                    String::new()
                } else {
                    format!(" — “{comment}”")
                };
                if to == "canceled" && !is_lead {
                    lines.push(format!(
                        "{title} was CANCELED by {actor}{note} — stop any work on it and pick up your other assignments."
                    ));
                } else {
                    lines.push(format!("{title} moved to {to} by {actor}{note}"));
                }
                row["kind"] = json!("moved");
                row["to"] = json!(to);
                row["note"] = json!(clamp(
                    payload.get("comment").and_then(|c| c.as_str()).unwrap_or(""),
                    DIGEST_CLAMP_UI,
                    "…"
                ));
                rows.push(row);
            }
            "item_created" => {
                lines.push(format!("New item filed by {actor}: {title}"));
                row["kind"] = json!("filed");
                rows.push(row);
            }
            "item_commented" => {
                let body = payload.get("body").and_then(|b| b.as_str()).unwrap_or("");
                lines.push(format!(
                    "Comment on {title} by {actor}: {}",
                    clamp(body, DIGEST_CLAMP_MODEL, " … (full text on the board)")
                ));
                row["kind"] = json!("comment");
                row["note"] = json!(clamp(body, DIGEST_CLAMP_UI, "…"));
                rows.push(row);
            }
            _ => {}
        }
    }

    for chat in chats {
        let author = chat.get("author").and_then(|a| a.as_str()).unwrap_or("");
        let role = chat
            .get("author_role")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let who = if role == "user" { "[User]" } else { author };
        let text = chat.get("text").and_then(|t| t.as_str()).unwrap_or("");
        lines.push(format!(
            "# team chat — {who}: {}",
            clamp(text, DIGEST_CLAMP_MODEL, " … (full text on the board)")
        ));
        rows.push(json!({
            "kind": "chat",
            "actor": who,
            "note": clamp(text, DIGEST_CLAMP_UI, "…"),
        }));
    }

    let body = if lines.is_empty() {
        "- (no detail)".to_string()
    } else {
        lines
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let message = if is_lead {
        format!(
            "⏰ Board wake — your team needs decisions:\n{body}\n\n\
Full hand-off comments live on the board (get_item). Verify review items against their \
acceptance criteria (then done, or send back with a comment), unblock or reassign blocked \
items, and triage new filings. Steer only where needed."
        )
    } else {
        format!(
            "[Lead] Board update:\n{body}{}\n\n\
Move your item to in_progress when you start; blocked (with a comment) if stuck; review with a \
hand-off comment when finished. Journal evidence as you go.",
            roster_note(team)
        )
    };
    (message, rows)
}

fn is_session_running(state: &AppState, session_id: &str) -> bool {
    state
        .running_engines
        .read()
        .get(session_id)
        .map(|run| *run.running.read())
        .unwrap_or(false)
}

/// Persist a board-wake user message + broadcast transcript-friendly events so
/// the GUI shows [`BoardWakeCard`] on refresh (and live via `turn_start.source`).
fn deliver_wake_message(state: &AppState, session_id: &str, message: &str, source: Value) {
    let ts = source
        .get("ts")
        .and_then(|t| t.as_f64())
        .unwrap_or_else(epoch_now);
    let msg = json!({
        "role": "user",
        "content": message,
        "ts": ts,
        "source": source.clone(),
    });
    state.push_message_sync(session_id, msg);
    // Live GUI: turn_start with source → connector/BoardWakeCard; turn_done clears running.
    state.broadcast_sync(
        session_id,
        json!({
            "type": "turn_start",
            "data": {
                "input": message,
                "source": source,
            }
        }),
    );
    state.broadcast_sync(session_id, json!({"type": "turn_done", "data": {}}));
    // Keep SQLite metadata in sync so GET /messages after restart still sees the wake.
    state.persist_turn(session_id);
}

async fn drain_team_member(
    state: &AppState,
    team: &Team,
    session_id: &str,
    actor: &str,
    is_lead: bool,
) -> i32 {
    let store = &state.board.store;
    let directs = store.feed_for(&team.space, actor, 200).unwrap_or_default();
    let mut directs = directs;
    let subs = if is_lead {
        store
            .subscribed_events(&team.space, actor, 200)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if !subs.is_empty() {
        let seen: HashSet<i64> = subs
            .iter()
            .filter_map(|e| e.get("seq").and_then(|s| s.as_i64()))
            .collect();
        directs.retain(|e| {
            e.get("seq")
                .and_then(|s| s.as_i64())
                .map(|s| !seen.contains(&s))
                .unwrap_or(true)
        });
    }
    let chat_handle = if is_lead { "lead" } else { actor };
    let chats = if team.chat_enabled && !team.chat_group.is_empty() {
        state.board.chat.unread_for(&team.chat_group, chat_handle)
    } else {
        Vec::new()
    };
    if directs.is_empty() && subs.is_empty() && chats.is_empty() {
        return 0;
    }
    if is_session_running(state, session_id) || TEAM_INFLIGHT.lock().contains(session_id) {
        return 0;
    }
    if !state
        .board
        .registry
        .count_wake(&team.team_id, TEAM_WAKE_CAP_PER_HOUR)
    {
        tracing::warn!(team_id = %team.team_id, "team paused for budget this hour");
        return 0;
    }

    let (message, rows) = team_digest(store, team, &directs, &subs, &chats, is_lead, actor);
    let source = board_source(team, &message, &rows);
    TEAM_INFLIGHT.lock().insert(session_id.to_string());

    deliver_wake_message(state, session_id, &message, source);

    let mut delivered_seqs: Vec<i64> = directs
        .iter()
        .filter_map(|e| e.get("seq").and_then(|s| s.as_i64()))
        .collect();
    delivered_seqs.extend(
        subs.iter()
            .filter_map(|e| e.get("seq").and_then(|s| s.as_i64())),
    );
    if let Some(max_seq) = delivered_seqs.iter().copied().max() {
        let _ = store.consume_feed(&team.space, actor, max_seq);
    }
    if let Some(last) = subs.last().and_then(|e| e.get("seq").and_then(|s| s.as_i64())) {
        let _ = store.consume_subscription(&team.space, actor, last);
    }
    if let Some(last) = chats
        .last()
        .and_then(|m| m.get("seq").and_then(|s| s.as_i64()))
    {
        state
            .board
            .chat
            .consume(&team.chat_group, chat_handle, last);
    }

    TEAM_INFLIGHT.lock().remove(session_id);
    1
}

/// Drain all non-paused teams. Returns number of deliveries dispatched.
pub async fn team_tick(state: &AppState) -> i32 {
    let teams = state.board.registry.all();
    if teams.is_empty() {
        return 0;
    }
    let mut delivered = 0;
    for team in teams {
        if team.paused {
            continue;
        }
        for worker in &team.workers {
            delivered += drain_team_member(
                state,
                &team,
                &worker.session_id,
                &worker.actor,
                false,
            )
            .await;
        }
        delivered += drain_team_member(
            state,
            &team,
            &team.lead_session,
            &team.lead_actor,
            true,
        )
        .await;
    }
    delivered
}

/// Nudge wake plumbing after an external board write (HTTP mutation).
pub fn kick_team_tick(state: AppState) {
    tokio::spawn(async move {
        let _ = team_tick(&state).await;
    });
}

/// Convenience for scheduler (`Arc<AppState>`).
pub fn kick_team_tick_arc(state: Arc<AppState>) {
    kick_team_tick((*state).clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocw_data::{TeamStore, TeamWorker};
    use crate::state::Config;

    fn lead() -> Actor {
        Actor::new("lead-1", Role::Lead)
    }
    fn worker(id: &str) -> Actor {
        Actor::new(id, Role::Worker)
    }

    #[test]
    fn digest_rows_carry_structured_kinds() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let w = worker("nia");
        let item = store
            .create_item(space, &lead, "Big item", "c", "", None, None)
            .unwrap();
        store.assign(space, &lead, item.id, "nia").unwrap();
        let essay = "verified the endpoint thoroughly. ".repeat(40);
        store
            .transition(space, &w, item.id, "in_progress", "")
            .unwrap();
        store
            .transition(space, &w, item.id, "review", &essay)
            .unwrap();

        let team = Team {
            team_id: "t1".into(),
            space: space.into(),
            lead_session: "s".into(),
            lead_actor: "lead-1".into(),
            workers: vec![TeamWorker {
                actor: "nia".into(),
                persona: "swe".into(),
                session_id: "w1".into(),
                model: String::new(),
                reason: String::new(),
            }],
            chat_enabled: false,
            chat_group: String::new(),
            paused: false,
            created_at: String::new(),
            wake_hour: String::new(),
            wakes_this_hour: 0,
        };
        let subs = store.subscribed_events(space, "lead-1", 200).unwrap();
        let (message, rows) = team_digest(&store, &team, &[], &subs, &[], true, "lead-1");
        assert!(!message.contains(&essay));
        assert!(message.contains("(full text on the board)"));
        assert!(message.len() < 1200);
        let moved: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.get("kind").and_then(|k| k.as_str()) == Some("moved")
                    && r.get("to").and_then(|t| t.as_str()) == Some("review")
            })
            .collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].get("item").and_then(|i| i.as_i64()), Some(item.id));
        let note = moved[0].get("note").and_then(|n| n.as_str()).unwrap();
        assert!(note.ends_with('…'));
        assert!(note.len() < 700);

        let source = board_source(&team, &message, &rows);
        assert_eq!(source.get("connector").and_then(|c| c.as_str()), Some("board"));
        assert!(source.get("board").and_then(|b| b.get("rows")).is_some());
    }

    #[tokio::test]
    async fn create_item_kick_delivers_board_source_message() {
        let dir = std::env::temp_dir().join(format!(
            "ocw-team-tick-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        let provider: Arc<dyn ocw_provider::Provider> =
            Arc::new(ocw_provider::Router::new("anthropic"));
        let state = AppState::new(config, provider);

        let lead_sid = state.create_session(None, "cowork").await.session_id;
        let space = "wake-proj";
        state.board.registry.create(
            space,
            &lead_sid,
            "lead-1",
            vec![],
            false,
            "",
        );

        let w = worker("nia");
        // Worker filing is a lead-subscription event (own verbs never wake the lead).
        state
            .board
            .store
            .create_item(space, &w, "Found a bug", "fix", "", None, None)
            .unwrap();

        let delivered = team_tick(&state).await;
        assert!(delivered >= 1, "expected at least one board wake delivery");

        let messages = state.list_messages(&lead_sid).await;
        let wake = messages.iter().rev().find(|m| {
            m.get("source")
                .and_then(|s| s.get("connector"))
                .and_then(|c| c.as_str())
                == Some("board")
        });
        let wake = wake.expect("expected board wake message");
        assert_eq!(
            wake.get("source")
                .and_then(|s| s.get("connector"))
                .and_then(|c| c.as_str()),
            Some("board")
        );
        let rows = wake
            .get("source")
            .and_then(|s| s.get("board"))
            .and_then(|b| b.get("rows"))
            .and_then(|r| r.as_array())
            .expect("board.rows");
        assert!(rows.iter().any(|r| r.get("kind").and_then(|k| k.as_str()) == Some("filed")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_registry_kick_is_noop() {
        let dir = std::env::temp_dir().join(format!(
            "ocw-team-tick-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        let provider: Arc<dyn ocw_provider::Provider> =
            Arc::new(ocw_provider::Router::new("anthropic"));
        let state = AppState::new(config, provider);
        assert_eq!(team_tick(&state).await, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
