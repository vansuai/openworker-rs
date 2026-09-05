//! Board HTTP surface — minimal Rust port of `/v1/board/*` from `coworker/server/app.py`.
//!
//! Identity is the board token (actor+role bound at mint); authority is the store.
//! Session-scoped `/v1/sessions/{id}/board/*` routes use sidecar session auth and
//! act as `Actor::new("user", Role::User)` — no board bearer required.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ocw_data::{space_for_workspace, Actor, AttachmentStore, BoardError, BoardItem, Role, TeamStore};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::state::AppState;

const TOKEN_PREFIX: &str = "owb_";

// ---------------------------------------------------------------------------
// Board tokens (hash-only registry)
// ---------------------------------------------------------------------------

pub struct BoardTokens {
    path: PathBuf,
    lock: Mutex<()>,
}

impl BoardTokens {
    pub fn open(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    pub fn mint(&self, actor: &str, role: &str, label: &str) -> Result<String, BoardError> {
        let actor = actor.trim();
        if actor.is_empty() {
            return Err(BoardError::Bad("actor is required".into()));
        }
        Role::parse(role)?;
        let token = format!("{TOKEN_PREFIX}{}", random_token());
        let digest = digest_token(&token);
        let _g = self.lock.lock();
        let mut entries = self.load();
        entries.insert(
            digest,
            json!({
                "actor": actor,
                "role": role,
                "label": label,
                "prefix": &token[..token.len().min(12)],
                "created_ts": chrono::Utc::now().to_rfc3339(),
            }),
        );
        self.save(&entries)?;
        Ok(token)
    }

    pub fn resolve(&self, token: &str) -> Option<Actor> {
        if token.is_empty() {
            return None;
        }
        let _g = self.lock.lock();
        let entries = self.load();
        let entry = entries.get(&digest_token(token))?;
        let id = entry.get("actor")?.as_str()?.to_string();
        let role = Role::parse(entry.get("role")?.as_str()?).ok()?;
        Some(Actor::new(id, role))
    }

    fn load(&self) -> HashMap<String, Value> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    fn save(&self, entries: &HashMap<String, Value>) -> Result<(), BoardError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BoardError::Bad(e.to_string()))?;
        }
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(entries).map_err(|e| BoardError::Bad(e.to_string()))?;
        std::fs::write(&tmp, text).map_err(|e| BoardError::Bad(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| BoardError::Bad(e.to_string()))?;
        Ok(())
    }
}

fn digest_token(token: &str) -> String {
    let hash = Sha256::digest(token.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_token() -> String {
    // URL-safe token body (matches Python `secrets.token_urlsafe(32)` shape closely enough).
    uuid::Uuid::new_v4().simple().to_string()
        + &uuid::Uuid::new_v4().simple().to_string()
}

// ---------------------------------------------------------------------------
// Shared board services on AppState
// ---------------------------------------------------------------------------

pub struct BoardServices {
    pub store: Arc<TeamStore>,
    pub attachments: Arc<AttachmentStore>,
    pub tokens: Arc<BoardTokens>,
}

impl BoardServices {
    pub fn open(data_dir: &Path) -> Self {
        let store = TeamStore::open(data_dir.join("teams.db")).unwrap_or_else(|_| {
            TeamStore::open_in_memory().expect("in-memory team store")
        });
        Self {
            store: Arc::new(store),
            attachments: Arc::new(AttachmentStore::new(data_dir.join("attachments"))),
            tokens: Arc::new(BoardTokens::open(data_dir.join("board-tokens.json"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth + error mapping
// ---------------------------------------------------------------------------

fn board_actor(headers: &HeaderMap, tokens: &BoardTokens) -> Option<Actor> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .unwrap_or("");
    tokens.resolve(token)
}

fn board_unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "board token required (Authorization: Bearer …) — mint one with `ocw board token` on the serving machine"
        })),
    )
        .into_response()
}

fn map_board_err(err: BoardError) -> Response {
    let (status, msg) = match &err {
        BoardError::NotFound(s) => (StatusCode::NOT_FOUND, s.clone()),
        BoardError::Authority(s) => (StatusCode::FORBIDDEN, s.clone()),
        BoardError::Bad(s) => (StatusCode::BAD_REQUEST, s.clone()),
    };
    (status, Json(json!({ "error": msg }))).into_response()
}

fn with_actor<F>(headers: &HeaderMap, state: &AppState, f: F) -> Response
where
    F: FnOnce(Actor) -> Response,
{
    match board_actor(headers, &state.board.tokens) {
        Some(actor) => f(actor),
        None => board_unauthorized(),
    }
}

fn user_actor() -> Actor {
    Actor::new("user", Role::User)
}

/// Resolve the session workspace from in-memory meta, then conversation store.
fn session_workspace(state: &AppState, session_id: &str) -> Option<String> {
    if let Some(meta) = state.get_session_sync(session_id) {
        if let Some(ws) = meta
            .workspace
            .filter(|w| !w.trim().is_empty() && w.trim() != "/")
        {
            return Some(ws);
        }
    }
    if let Ok(Some(record)) = state.conversation_store.load(session_id) {
        let ws = record.workspace.trim();
        if !ws.is_empty() && ws != "/" {
            return Some(record.workspace);
        }
    }
    None
}

fn empty_board() -> Value {
    json!({ "space": Value::Null, "name": "", "items": [] })
}

fn clamp_chars(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit).collect();
    format!("{}…", truncated.trim_end())
}

fn item_to_value(item: &BoardItem) -> Value {
    serde_json::to_value(item).unwrap_or(json!({}))
}

fn project_timeline(events: &[Value]) -> Vec<Value> {
    let mut timeline = Vec::new();
    for event in events {
        let payload = event.get("payload").cloned().unwrap_or(json!({}));
        let mut row = json!({
            "seq": event.get("seq"),
            "ts": event.get("ts"),
            "actor": event.get("actor"),
        });
        let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "item_created" => {
                row["kind"] = json!("created");
            }
            "item_assigned" => {
                let claimed = payload
                    .get("claimed")
                    .and_then(|c| c.as_bool())
                    .unwrap_or(false);
                row["kind"] = json!(if claimed { "claimed" } else { "assigned" });
                row["assignee"] =
                    json!(payload.get("assignee").and_then(|a| a.as_str()).unwrap_or(""));
            }
            "item_transitioned" => {
                row["kind"] = json!("moved");
                row["to"] = json!(payload.get("to").and_then(|t| t.as_str()).unwrap_or(""));
                if let Some(comment) = payload.get("comment").and_then(|c| c.as_str()) {
                    if !comment.is_empty() {
                        row["body"] = json!(comment);
                    }
                }
                if let Some(refs) = payload.get("refs") {
                    if refs.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                        row["refs"] = refs.clone();
                    }
                }
            }
            "item_commented" => {
                row["kind"] = json!("comment");
                row["body"] = json!(payload.get("body").and_then(|b| b.as_str()).unwrap_or(""));
                if let Some(refs) = payload.get("refs") {
                    if refs.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                        row["refs"] = refs.clone();
                    }
                }
            }
            _ => continue,
        }
        timeline.push(row);
    }
    timeline
}

// ---------------------------------------------------------------------------
// Token-auth board handlers (`/v1/board/*`)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ItemsQuery {
    pub space: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub assignee: String,
}

#[derive(Debug, Deserialize)]
pub struct ItemQuery {
    pub space: String,
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct AttachmentQuery {
    /// Required — attachment bytes are never readable without a board space.
    pub space: String,
    pub name: String,
}

pub async fn handler_whoami(State(state): State<AppState>, headers: HeaderMap) -> Response {
    with_actor(&headers, &state, |actor| {
        Json(json!({ "actor": actor.id, "role": actor.role.as_str() })).into_response()
    })
}

pub async fn handler_list_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ItemsQuery>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let state_filter = if q.state.is_empty() {
            None
        } else {
            Some(q.state.as_str())
        };
        let assignee_filter = if q.assignee.is_empty() {
            None
        } else {
            Some(q.assignee.as_str())
        };
        match state
            .board
            .store
            .list_items(&q.space, &actor, state_filter, assignee_filter)
        {
            Ok(items) => Json(json!({ "items": items })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_get_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ItemQuery>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        match state.board.store.get_item(&q.space, q.id, &actor) {
            Ok(item) => Json(item).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_create_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let criteria = body.get("criteria").and_then(|v| v.as_str()).unwrap_or("");
        let description = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let parent = body.get("parent").and_then(|v| v.as_i64());
        let case = body
            .get("case")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match state.board.store.create_item(
            space,
            &actor,
            title,
            criteria,
            description,
            parent,
            case,
        ) {
            Ok(item) => Json(item).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AttachmentQuery>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        // Space is required by the query schema; store also enforces it.
        if let Err(e) = state
            .board
            .store
            .require_attachment_access(&q.space, &actor, &q.name)
        {
            return map_board_err(e);
        }
        match state.board.attachments.path_for(&q.name) {
            Ok(path) => match std::fs::read(&path) {
                Ok(bytes) => {
                    let mime = AttachmentStore::mime_for(&q.name);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime)
                        .body(Body::from(bytes))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                Err(_) => map_board_err(BoardError::NotFound("attachment not found".into())),
            },
            Err(e) => map_board_err(e),
        }
    })
}

// ---------------------------------------------------------------------------
// Session-auth board handlers (`/v1/sessions/{id}/board/*`)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SessionItemQuery {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct SessionAttachmentQuery {
    pub name: String,
}

pub async fn handler_session_board(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
) -> Response {
    let Some(workspace) = session_workspace(&state, &session_id) else {
        return Json(empty_board()).into_response();
    };
    let space = space_for_workspace(&workspace);
    let actor = user_actor();
    match state.board.store.list_items(&space, &actor, None, None) {
        Ok(items) if items.is_empty() => Json(empty_board()).into_response(),
        Ok(items) => {
            let mut out_items = Vec::with_capacity(items.len());
            for item in items {
                let mut value = item_to_value(&item);
                if item.state == "blocked" {
                    if let Ok(events) = state.board.store.events(&space, Some(item.id)) {
                        for event in events.iter().rev() {
                            let payload = event.get("payload").cloned().unwrap_or(json!({}));
                            if event.get("kind").and_then(|k| k.as_str())
                                == Some("item_transitioned")
                                && payload.get("to").and_then(|t| t.as_str()) == Some("blocked")
                            {
                                if let Some(comment) =
                                    payload.get("comment").and_then(|c| c.as_str())
                                {
                                    if !comment.is_empty() {
                                        value["blocker"] = json!(clamp_chars(comment, 120));
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
                out_items.push(value);
            }
            let name = Path::new(&space)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            Json(json!({ "space": space, "name": name, "items": out_items })).into_response()
        }
        Err(e) => Json(json!({ "error": e.to_string() })).into_response(),
    }
}

pub async fn handler_session_board_item(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
    Query(q): Query<SessionItemQuery>,
) -> Response {
    let Some(workspace) = session_workspace(&state, &session_id) else {
        return Json(json!({ "error": "no board for this session" })).into_response();
    };
    let space = space_for_workspace(&workspace);
    let actor = user_actor();
    let item = match state.board.store.get_item(&space, q.id, &actor) {
        Ok(item) => item,
        Err(e) => return Json(json!({ "error": e.to_string() })).into_response(),
    };
    let events = state
        .board
        .store
        .events(&space, Some(q.id))
        .unwrap_or_default();
    let mut value = item_to_value(&item);
    value["timeline"] = json!(project_timeline(&events));
    Json(value).into_response()
}

pub async fn handler_session_board_attachment(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
    Query(q): Query<SessionAttachmentQuery>,
) -> Response {
    let Some(workspace) = session_workspace(&state, &session_id) else {
        return map_board_err(BoardError::NotFound("attachment not found".into()));
    };
    let space = space_for_workspace(&workspace);
    let actor = user_actor();
    if let Err(e) = state
        .board
        .store
        .require_attachment_access(&space, &actor, &q.name)
    {
        return map_board_err(e);
    }
    match state.board.attachments.path_for(&q.name) {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => {
                let mime = AttachmentStore::mime_for(&q.name);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, mime)
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
            Err(_) => map_board_err(BoardError::NotFound("attachment not found".into())),
        },
        Err(e) => map_board_err(e),
    }
}

pub async fn handler_session_board_comment(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(workspace) = session_workspace(&state, &session_id) else {
        return Json(json!({ "error": "no board for this session" })).into_response();
    };
    let space = space_for_workspace(&workspace);
    let item_id = body.get("item").and_then(|v| v.as_i64()).unwrap_or(0);
    let text = body.get("body").and_then(|v| v.as_str()).unwrap_or("");
    match state
        .board
        .store
        .comment(&space, &user_actor(), item_id, text)
    {
        Ok(event) => Json(json!({
            "ok": true,
            "seq": event.get("seq").and_then(|s| s.as_i64()).unwrap_or(0),
        }))
        .into_response(),
        Err(e) => Json(json!({ "error": e.to_string() })).into_response(),
    }
}

pub async fn handler_session_board_transition(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(workspace) = session_workspace(&state, &session_id) else {
        return Json(json!({ "error": "this session has no board" })).into_response();
    };
    let space = space_for_workspace(&workspace);
    let item_id = body.get("item").and_then(|v| v.as_i64()).unwrap_or(0);
    let to = body.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let comment = body.get("comment").and_then(|v| v.as_str()).unwrap_or("");
    match state
        .board
        .store
        .transition(&space, &user_actor(), item_id, to, comment)
    {
        Ok(item) => Json(item).into_response(),
        Err(e) => Json(json!({ "error": e.to_string() })).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Team chat / journal stubs
// ---------------------------------------------------------------------------

pub async fn handler_team_chat_get(AxumPath(_team_id): AxumPath<String>) -> Response {
    Json(json!({ "enabled": false, "messages": [], "members": [] })).into_response()
}

pub async fn handler_team_chat_post(
    AxumPath(_team_id): AxumPath<String>,
    Json(_body): Json<Value>,
) -> Response {
    Json(json!({ "error": "team chat not configured" })).into_response()
}

pub async fn handler_teams_journal() -> Response {
    Json(json!({ "cases": [] })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip_binds_actor_and_role() {
        let dir = std::env::temp_dir().join(format!("ocw-board-tokens-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let tokens = BoardTokens::open(dir.join("board-tokens.json"));
        let token = tokens.mint("nia", "worker", "test").unwrap();
        let actor = tokens.resolve(&token).unwrap();
        assert_eq!(actor.id, "nia");
        assert_eq!(actor.role, Role::Worker);
        assert!(tokens.resolve("owb_bogus").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_board_shape_for_missing_workspace() {
        let board = empty_board();
        assert!(board.get("space").unwrap().is_null());
        assert_eq!(board.get("name").and_then(|n| n.as_str()), Some(""));
        assert_eq!(
            board
                .get("items")
                .and_then(|i| i.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn timeline_projects_event_kinds() {
        let events = vec![
            json!({
                "seq": 1, "ts": "t1", "actor": "lead", "kind": "item_created",
                "payload": {}
            }),
            json!({
                "seq": 2, "ts": "t2", "actor": "lead", "kind": "item_assigned",
                "payload": {"assignee": "nia", "claimed": false}
            }),
            json!({
                "seq": 3, "ts": "t3", "actor": "nia", "kind": "item_transitioned",
                "payload": {"to": "blocked", "comment": "need tfvars"}
            }),
            json!({
                "seq": 4, "ts": "t4", "actor": "user", "kind": "item_commented",
                "payload": {"body": "looking"}
            }),
        ];
        let timeline = project_timeline(&events);
        assert_eq!(timeline.len(), 4);
        assert_eq!(timeline[0]["kind"], "created");
        assert_eq!(timeline[1]["kind"], "assigned");
        assert_eq!(timeline[2]["kind"], "moved");
        assert_eq!(timeline[2]["body"], "need tfvars");
        assert_eq!(timeline[3]["kind"], "comment");
    }
}
