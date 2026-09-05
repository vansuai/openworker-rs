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
use ocw_data::{
    space_for_workspace, Actor, AttachmentStore, BoardError, BoardItem, ChatMember, ChatStore,
    JournalStore, Role, TeamRegistry, TeamStore, TeamWorker,
};
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
    pub journal: Arc<JournalStore>,
    pub chat: Arc<ChatStore>,
    pub attachments: Arc<AttachmentStore>,
    pub tokens: Arc<BoardTokens>,
    pub registry: Arc<TeamRegistry>,
}

impl BoardServices {
    pub fn open(data_dir: &Path) -> Self {
        let store = TeamStore::open(data_dir.join("teams.db")).unwrap_or_else(|_| {
            TeamStore::open_in_memory().expect("in-memory team store")
        });
        let journal = JournalStore::open(data_dir.join("journal.db")).unwrap_or_else(|_| {
            JournalStore::open_in_memory().expect("in-memory journal store")
        });
        let chat = ChatStore::open(data_dir.join("chat.db")).unwrap_or_else(|_| {
            ChatStore::open_in_memory().expect("in-memory chat store")
        });
        Self {
            store: Arc::new(store),
            journal: Arc::new(journal),
            chat: Arc::new(chat),
            attachments: Arc::new(AttachmentStore::new(data_dir.join("attachments"))),
            tokens: Arc::new(BoardTokens::open(data_dir.join("board-tokens.json"))),
            registry: Arc::new(TeamRegistry::open(data_dir.join("teams.json"))),
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
            Ok(item) => {
                if !item.case_id.is_empty() {
                    let _ = state.board.journal.ensure_case(&item.case_id, &actor.id);
                }
                crate::team_tick::kick_team_tick(state.clone());
                Json(item).into_response()
            }
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

pub async fn handler_board_spaces(State(state): State<AppState>, headers: HeaderMap) -> Response {
    with_actor(&headers, &state, |_actor| {
        match state.board.store.spaces() {
            Ok(spaces) => Json(json!({ "spaces": spaces })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_transition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let to = body.get("to").and_then(|v| v.as_str()).unwrap_or("");
        let comment = body.get("comment").and_then(|v| v.as_str()).unwrap_or("");
        match state.board.store.transition(space, &actor, id, to, comment) {
            Ok(item) => {
                crate::team_tick::kick_team_tick(state.clone());
                Json(item).into_response()
            }
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let text = body.get("body").and_then(|v| v.as_str()).unwrap_or("");
        match state.board.store.comment(space, &actor, id, text) {
            Ok(event) => {
                crate::team_tick::kick_team_tick(state.clone());
                Json(event).into_response()
            }
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_assign(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let assignee = body.get("assignee").and_then(|v| v.as_str()).unwrap_or("");
        let previous = state
            .board
            .store
            .get_item(space, id, &actor)
            .map(|item| item.assignee)
            .unwrap_or_default();
        match state.board.store.assign(space, &actor, id, assignee) {
            Ok(item) => {
                if !item.case_id.is_empty() {
                    let _ = state.board.journal.sync_assignment(
                        &item.case_id,
                        space,
                        item.id,
                        &previous,
                        assignee,
                    );
                }
                crate::team_tick::kick_team_tick(state.clone());
                Json(item).into_response()
            }
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        match state.board.store.claim(space, &actor, id) {
            Ok(item) => {
                if !item.case_id.is_empty() {
                    let _ = state.board.journal.sync_assignment(
                        &item.case_id,
                        space,
                        item.id,
                        "",
                        &actor.id,
                    );
                }
                crate::team_tick::kick_team_tick(state.clone());
                Json(item).into_response()
            }
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let src = body.get("src").and_then(|v| v.as_i64()).unwrap_or(0);
        let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let dst = body.get("dst").and_then(|v| v.as_i64()).unwrap_or(0);
        match state.board.store.link(space, &actor, src, kind, dst) {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_attach(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let raw = body.get("data_b64").and_then(|v| v.as_str()).unwrap_or("");
        if raw.len() > 15 * 1024 * 1024 {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "attachment exceeds 10MB" })))
                .into_response();
        }
        use base64::Engine as _;
        let data = match base64::engine::general_purpose::STANDARD.decode(raw) {
            Ok(d) => d,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "data_b64 is not valid base64" })),
                )
                    .into_response();
            }
        };
        let filename = body
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ext = std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let ext = if (1..=5).contains(&ext.len())
            && ext.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
        {
            ext
        } else {
            "bin".to_string()
        };
        let hash = Sha256::digest(&data);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let stored = format!("{hex}.{ext}");
        let ref_str = match state.board.attachments.put_bytes(&data, filename, &stored) {
            Ok(r) => r,
            Err(e) => return map_board_err(e),
        };
        let caption = body
            .get("caption")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attached {filename}"));
        match state
            .board
            .store
            .attach_ref(space, &actor, id, &caption, &ref_str)
        {
            Ok(event) => Json(json!({
                "ref": ref_str,
                "seq": event.get("seq").and_then(|s| s.as_i64()).unwrap_or(0),
            }))
            .into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct PolicyQuery {
    pub space: String,
}

pub async fn handler_board_policy_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PolicyQuery>,
) -> Response {
    with_actor(&headers, &state, |_actor| {
        match state.board.store.policy(&q.space) {
            Ok(policy) => Json(policy).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_policy_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let claims = body.get("claims").and_then(|v| v.as_str()).unwrap_or("");
        match state.board.store.set_policy(space, &actor, claims) {
            Ok(policy) => Json(policy).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    pub space: String,
    #[serde(default = "default_pending_limit")]
    pub limit: i64,
}

fn default_pending_limit() -> i64 {
    200
}

pub async fn handler_board_pending(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PendingQuery>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        match state.board.store.feed_for(&q.space, &actor.id, q.limit) {
            Ok(events) => Json(json!({ "events": events })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_consume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
        let upto = body.get("upto_seq").and_then(|v| v.as_i64()).unwrap_or(0);
        match state.board.store.consume_feed(space, &actor.id, upto) {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_journal_cases(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    with_actor(&headers, &state, |actor| {
        match state.board.journal.overview(&actor) {
            Ok(cases) => Json(json!({ "cases": cases })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct JournalQuery {
    pub case: String,
    #[serde(default)]
    pub item: Option<i64>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub entity: String,
    #[serde(default)]
    pub include_raw: String,
    #[serde(default = "default_pending_limit")]
    pub limit: i64,
}

pub async fn handler_board_journal_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<JournalQuery>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let author = if q.author.is_empty() {
            None
        } else {
            Some(q.author.as_str())
        };
        let kind = if q.kind.is_empty() {
            None
        } else {
            Some(q.kind.as_str())
        };
        let entity = if q.entity.is_empty() {
            None
        } else {
            Some(q.entity.as_str())
        };
        match state.board.journal.read(
            &actor,
            &q.case,
            q.item,
            author,
            kind,
            entity,
            0,
            !q.include_raw.is_empty(),
            q.limit,
        ) {
            Ok(entries) => Json(json!({ "entries": entries })).into_response(),
            Err(e) => map_board_err(e),
        }
    })
}

pub async fn handler_board_journal_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    with_actor(&headers, &state, |actor| {
        let case = body.get("case").and_then(|v| v.as_str()).unwrap_or("");
        let text = body.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let kind = body
            .get("kind")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("note");
        let space = body
            .get("space")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let item = body.get("item").and_then(|v| v.as_i64());
        let entities: Vec<String> = body
            .get("entities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let refs: Vec<String> = body
            .get("refs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        match state.board.journal.append(
            &actor,
            case,
            text,
            kind,
            space,
            item,
            Some(&entities),
            Some(&refs),
        ) {
            Ok(entry) => Json(entry).into_response(),
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
        Ok(event) => {
            crate::team_tick::kick_team_tick(state.clone());
            Json(json!({
                "ok": true,
                "seq": event.get("seq").and_then(|s| s.as_i64()).unwrap_or(0),
            }))
            .into_response()
        }
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
        Ok(item) => {
            crate::team_tick::kick_team_tick(state.clone());
            Json(item).into_response()
        }
        Err(e) => Json(json!({ "error": e.to_string() })).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Team chat / journal stubs
// ---------------------------------------------------------------------------

/// Register a team in the wake roster (tests / kick path before Phase D create_team tool).
pub async fn handler_create_team(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let space = body.get("space").and_then(|v| v.as_str()).unwrap_or("");
    let lead_session = body
        .get("lead_session")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let lead_actor = body
        .get("lead_actor")
        .and_then(|v| v.as_str())
        .unwrap_or("lead");
    if space.is_empty() || lead_session.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "space and lead_session are required" })),
        )
            .into_response();
    }
    let workers: Vec<TeamWorker> = body
        .get("workers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|w| {
                    let actor = w.get("actor")?.as_str()?.to_string();
                    let persona = w
                        .get("persona")
                        .and_then(|p| p.as_str())
                        .unwrap_or(&actor)
                        .to_string();
                    let session_id = w.get("session_id")?.as_str()?.to_string();
                    Some(TeamWorker {
                        actor,
                        persona,
                        session_id,
                        model: w
                            .get("model")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_string(),
                        reason: w
                            .get("reason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // MVP: enable chat by default for registered teams unless explicitly disabled.
    let mut chat_enabled = body
        .get("chat_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let mut chat_group = body
        .get("chat_group")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if chat_enabled && chat_group.is_empty() {
        let mut members: Vec<ChatMember> = workers
            .iter()
            .map(|w| ChatMember {
                name: w.actor.clone(),
                persona: w.persona.clone(),
                role: "worker".into(),
            })
            .collect();
        members.push(ChatMember {
            name: "lead".into(),
            persona: lead_actor.to_string(),
            role: "lead".into(),
        });
        match state.board.chat.create_group("team chat", members) {
            Ok(group) => {
                chat_group = group
                    .get("group_id")
                    .and_then(|g| g.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            Err(e) => {
                tracing::warn!("chat group create failed: {e}");
                chat_enabled = false;
            }
        }
    }
    let team = state.board.registry.create(
        space,
        lead_session,
        lead_actor,
        workers,
        chat_enabled,
        &chat_group,
    );
    Json(team).into_response()
}

pub async fn handler_list_teams(State(state): State<AppState>) -> Response {
    Json(json!({ "teams": state.board.registry.all() })).into_response()
}

pub async fn handler_team_chat_get(
    State(state): State<AppState>,
    AxumPath(team_id): AxumPath<String>,
) -> Response {
    let Some(team) = state.board.registry.get(&team_id) else {
        return Json(json!({ "enabled": false, "messages": [], "members": [] })).into_response();
    };
    if !team.chat_enabled || team.chat_group.is_empty() {
        return Json(json!({ "enabled": false, "messages": [], "members": [] })).into_response();
    }
    let group = state
        .board
        .chat
        .get_group(&team.chat_group)
        .unwrap_or_else(|| json!({ "members": [] }));
    let messages = state.board.chat.messages(&team.chat_group, 0, 200);
    if let Some(last) = messages
        .last()
        .and_then(|m| m.get("seq").and_then(|s| s.as_i64()))
    {
        // Viewing IS reading for the user (Python team_chat mark_read).
        state.board.chat.consume(&team.chat_group, "user", last);
    }
    let members = group.get("members").cloned().unwrap_or(json!([]));
    Json(json!({
        "enabled": true,
        "team_id": team_id,
        "members": members,
        "messages": messages,
    }))
    .into_response()
}

pub async fn handler_team_chat_post(
    State(state): State<AppState>,
    AxumPath(team_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(team) = state.board.registry.get(&team_id) else {
        return Json(json!({ "error": "unknown team" })).into_response();
    };
    if !team.chat_enabled || team.chat_group.is_empty() {
        return Json(json!({ "error": "chat is not enabled for this team" })).into_response();
    }
    let text = body.get("text").and_then(|t| t.as_str()).unwrap_or("");
    match state
        .board
        .chat
        .post(&team.chat_group, "user", text, "user")
    {
        Ok(message) => {
            crate::team_tick::kick_team_tick(state.clone());
            Json(message).into_response()
        }
        Err(e) => Json(json!({ "error": e.to_string() })).into_response(),
    }
}

pub async fn handler_teams_journal(State(state): State<AppState>) -> Response {
    match state.board.journal.overview(&user_actor()) {
        Ok(cases) => Json(json!({ "cases": cases })).into_response(),
        Err(e) => map_board_err(e),
    }
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
