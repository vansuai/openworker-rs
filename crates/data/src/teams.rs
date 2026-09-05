//! Minimal board / teams store — Rust scaffolding for `coworker/teams/`.
//!
//! Security constraints mirrored from upstream (PRs #585/#586):
//! - Worker item reads enforce the same visibility slice as list reads.
//! - Attachment reads require a board `space` and an actor-visible item that
//!   carries an authoritative attachment reference.

use crate::Error;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Errors / roles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardError {
    /// Illegal transition, bad input, missing attachment name, etc.
    Bad(String),
    /// Missing *or* not visible to the actor (same contract on purpose).
    NotFound(String),
    /// Role does not permit the verb.
    Authority(String),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::Bad(s) | BoardError::NotFound(s) | BoardError::Authority(s) => {
                write!(f, "{s}")
            }
        }
    }
}

impl std::error::Error for BoardError {}

impl From<rusqlite::Error> for BoardError {
    fn from(e: rusqlite::Error) -> Self {
        BoardError::Bad(format!("sqlite: {e}"))
    }
}

impl From<serde_json::Error> for BoardError {
    fn from(e: serde_json::Error) -> Self {
        BoardError::Bad(format!("json: {e}"))
    }
}

impl From<BoardError> for Error {
    fn from(e: BoardError) -> Self {
        match e {
            BoardError::NotFound(s) => Error::NotFound(s),
            BoardError::Bad(s) | BoardError::Authority(s) => Error::Invalid(s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Lead,
    Worker,
    System,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Lead => "lead",
            Role::Worker => "worker",
            Role::System => "system",
        }
    }

    pub fn parse(s: &str) -> Result<Self, BoardError> {
        match s {
            "user" => Ok(Role::User),
            "lead" => Ok(Role::Lead),
            "worker" => Ok(Role::Worker),
            "system" => Ok(Role::System),
            other => Err(BoardError::Bad(format!("unknown role: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub role: Role,
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub session_id: String,
}

impl Actor {
    pub fn new(id: impl Into<String>, role: Role) -> Self {
        Self {
            id: id.into(),
            role,
            persona: String::new(),
            model: String::new(),
            session_id: String::new(),
        }
    }
}

/// Materialized board item (projection row + optional read-side fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardItem {
    pub space: String,
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub criteria: String,
    pub state: String,
    #[serde(default)]
    pub assignee: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub case_id: String,
    #[serde(default)]
    pub refs: Vec<String>,
    pub created_ts: String,
    pub updated_seq: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

const ATTACHMENT_SCHEME: &str = "attachment://";

/// `attachment://<hash>.<ext>#<name>` → `<hash>.<ext>`; None for other refs.
pub fn stored_name(ref_str: &str) -> Option<&str> {
    let rest = ref_str.strip_prefix(ATTACHMENT_SCHEME)?;
    Some(rest.split('#').next().unwrap_or(rest))
}

/// Match upstream `[0-9a-f]{64}\.[a-z0-9]{1,5}` without pulling in `regex`.
fn is_valid_stored_name(stored: &str) -> bool {
    let Some((hash, ext)) = stored.split_once('.') else {
        return false;
    };
    hash.len() == 64
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && (1..=5).contains(&ext.len())
        && ext.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
}

pub fn validate_stored_name(stored: &str) -> Result<String, BoardError> {
    let stored = stored.trim();
    if !is_valid_stored_name(stored) {
        return Err(BoardError::Bad(format!(
            "not an attachment name: {stored:?}"
        )));
    }
    Ok(stored.to_string())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Spaces are keyed to the project/workspace (boards are views over a space).
/// Mirrors `coworker/teams/model.py::space_for_workspace`.
pub fn space_for_workspace(workspace: &str) -> String {
    let expanded = expand_user(workspace);
    let path = PathBuf::from(&expanded);
    if let Ok(canon) = path.canonicalize() {
        return canon.to_string_lossy().into_owned();
    }
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn expand_user(s: &str) -> String {
    if s == "~" {
        dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| s.to_string())
    } else if let Some(rest) = s.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().into_owned())
            .unwrap_or_else(|| s.to_string())
    } else {
        s.to_string()
    }
}

const ITEM_CREATED: &str = "item_created";
const ITEM_ASSIGNED: &str = "item_assigned";
const ITEM_TRANSITIONED: &str = "item_transitioned";
const ITEM_COMMENTED: &str = "item_commented";

/// Legal state-machine edges (mirrors `coworker/teams/model.py::EDGES`).
fn edges(from: &str) -> &'static [&'static str] {
    match from {
        "open" => &["in_progress", "canceled"],
        "in_progress" => &["blocked", "review", "canceled"],
        "blocked" => &["in_progress", "canceled"],
        "review" => &["done", "in_progress", "canceled"],
        "done" => &[],
        "canceled" => &["open"],
        _ => &[],
    }
}

/// Targets a worker may move its own item to.
const WORKER_TARGETS: &[&str] = &["in_progress", "blocked", "review"];

// ---------------------------------------------------------------------------
// TeamStore
// ---------------------------------------------------------------------------

pub struct TeamStore {
    conn: Mutex<Connection>,
}

impl TeamStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BoardError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BoardError::Bad(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, BoardError> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), BoardError> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS team_items (
                space TEXT NOT NULL,
                id INTEGER NOT NULL,
                title TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                criteria TEXT NOT NULL,
                state TEXT NOT NULL,
                assignee TEXT DEFAULT '',
                creator TEXT NOT NULL DEFAULT '',
                case_id TEXT DEFAULT '',
                refs TEXT NOT NULL DEFAULT '[]',
                created_ts TEXT NOT NULL,
                updated_seq INTEGER NOT NULL,
                PRIMARY KEY (space, id)
            );
            CREATE TABLE IF NOT EXISTS team_links (
                space TEXT NOT NULL,
                src INTEGER NOT NULL,
                kind TEXT NOT NULL,
                dst INTEGER NOT NULL,
                UNIQUE (space, src, kind, dst)
            );
            CREATE TABLE IF NOT EXISTS team_attachment_refs (
                space TEXT NOT NULL,
                stored TEXT NOT NULL,
                item_id INTEGER NOT NULL,
                event_seq INTEGER NOT NULL,
                PRIMARY KEY (space, stored, item_id)
            );
            CREATE TABLE IF NOT EXISTS team_settings (
                space TEXT PRIMARY KEY,
                claims TEXT NOT NULL DEFAULT 'open'
            );
            CREATE TABLE IF NOT EXISTS team_seq (
                space TEXT PRIMARY KEY,
                next_seq INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS team_events (
                space TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                actor TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT '',
                item_id INTEGER,
                case_id TEXT,
                payload TEXT NOT NULL DEFAULT '{}',
                ts TEXT NOT NULL,
                PRIMARY KEY (space, seq)
            );
            CREATE TABLE IF NOT EXISTS team_cursors (
                cursor_key TEXT PRIMARY KEY,
                consumed_seq INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        Ok(())
    }

    fn next_seq(conn: &Connection, space: &str) -> Result<i64, BoardError> {
        let current: i64 = conn
            .query_row(
                "SELECT next_seq FROM team_seq WHERE space = ?",
                params![space],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = current + 1;
        conn.execute(
            "INSERT INTO team_seq (space, next_seq) VALUES (?, ?)
             ON CONFLICT(space) DO UPDATE SET next_seq = excluded.next_seq",
            params![space, next],
        )?;
        Ok(next)
    }

    fn next_item_id(conn: &Connection, space: &str) -> Result<i64, BoardError> {
        let top: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM team_items WHERE space = ?",
                params![space],
                |r| r.get(0),
            )?;
        Ok(top + 1)
    }

    /// Append one event row (allocates the next seq). Used by create/assign/transition/comment.
    pub fn append_event(
        &self,
        space: &str,
        kind: &str,
        actor: &Actor,
        item_id: Option<i64>,
        case_id: Option<&str>,
        payload: Value,
    ) -> Result<Value, BoardError> {
        if space.is_empty() {
            return Err(BoardError::Bad("space is required".into()));
        }
        let conn = self.conn.lock();
        Self::append_event_locked(&conn, space, kind, actor, item_id, case_id, payload)
    }

    fn append_event_locked(
        conn: &Connection,
        space: &str,
        kind: &str,
        actor: &Actor,
        item_id: Option<i64>,
        case_id: Option<&str>,
        payload: Value,
    ) -> Result<Value, BoardError> {
        let seq = Self::next_seq(conn, space)?;
        Self::write_event(conn, space, seq, kind, actor, item_id, case_id, payload)
    }

    fn write_event(
        conn: &Connection,
        space: &str,
        seq: i64,
        kind: &str,
        actor: &Actor,
        item_id: Option<i64>,
        case_id: Option<&str>,
        payload: Value,
    ) -> Result<Value, BoardError> {
        let ts = now_iso();
        let payload_str = serde_json::to_string(&payload)?;
        let role = actor.role.as_str();
        conn.execute(
            "INSERT INTO team_events
                (space, seq, kind, actor, role, item_id, case_id, payload, ts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                space,
                seq,
                kind,
                actor.id,
                role,
                item_id,
                case_id,
                payload_str,
                ts
            ],
        )?;
        Ok(json!({
            "space": space,
            "seq": seq,
            "kind": kind,
            "actor": actor.id,
            "role": role,
            "item_id": item_id,
            "case_id": case_id,
            "payload": payload,
            "ts": ts,
        }))
    }

    /// List events for a space, optionally filtered to one item (ascending seq).
    pub fn events(&self, space: &str, item_id: Option<i64>) -> Result<Vec<Value>, BoardError> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        if let Some(id) = item_id {
            let mut stmt = conn.prepare(
                "SELECT space, seq, kind, actor, role, item_id, case_id, payload, ts
                 FROM team_events WHERE space = ? AND item_id = ? ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![space, id], Self::row_to_event)?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT space, seq, kind, actor, role, item_id, case_id, payload, ts
                 FROM team_events WHERE space = ? ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![space], Self::row_to_event)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    fn row_to_event(row: &rusqlite::Row<'_>) -> Result<Value, rusqlite::Error> {
        let payload_raw: String = row.get(7)?;
        let payload: Value = serde_json::from_str(&payload_raw).unwrap_or_else(|_| json!({}));
        Ok(json!({
            "space": row.get::<_, String>(0)?,
            "seq": row.get::<_, i64>(1)?,
            "kind": row.get::<_, String>(2)?,
            "actor": row.get::<_, String>(3)?,
            "role": row.get::<_, String>(4)?,
            "item_id": row.get::<_, Option<i64>>(5)?,
            "case_id": row.get::<_, Option<String>>(6)?,
            "payload": payload,
            "ts": row.get::<_, String>(8)?,
        }))
    }

    fn require(actor: &Actor, roles: &[Role], verb: &str) -> Result<(), BoardError> {
        if roles.contains(&actor.role) {
            Ok(())
        } else {
            let allowed: Vec<&str> = roles.iter().map(|r| r.as_str()).collect();
            Err(BoardError::Authority(format!(
                "{verb} requires one of {allowed:?} (actor {} is {})",
                actor.id,
                actor.role.as_str()
            )))
        }
    }

    pub fn policy(&self, space: &str) -> Result<Value, BoardError> {
        let conn = self.conn.lock();
        let claims: String = conn
            .query_row(
                "SELECT claims FROM team_settings WHERE space = ?",
                params![space],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "open".to_string());
        Ok(json!({ "claims": claims }))
    }

    pub fn set_policy(&self, space: &str, actor: &Actor, claims: &str) -> Result<Value, BoardError> {
        Self::require(actor, &[Role::User, Role::Lead], "set_policy")?;
        if claims != "open" && claims != "lead-only" {
            return Err(BoardError::Bad(format!(
                "unknown claim policy: {claims} (use one of (\"open\", \"lead-only\"))"
            )));
        }
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO team_settings (space, claims) VALUES (?, ?)
             ON CONFLICT(space) DO UPDATE SET claims = excluded.claims",
            params![space, claims],
        )?;
        Ok(json!({ "claims": claims }))
    }

    fn claims_open(conn: &Connection, space: &str) -> Result<bool, BoardError> {
        let claims: String = conn
            .query_row(
                "SELECT claims FROM team_settings WHERE space = ?",
                params![space],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "open".to_string());
        Ok(claims == "open")
    }

    fn worker_slice(conn: &Connection, space: &str, worker_id: &str) -> Result<HashSet<i64>, BoardError> {
        let mut stmt = conn.prepare(
            "SELECT id FROM team_items WHERE space = ? AND (assignee = ? OR creator = ?)",
        )?;
        let mine: HashSet<i64> = stmt
            .query_map(params![space, worker_id, worker_id], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        if mine.is_empty() {
            return Ok(HashSet::new());
        }
        let mut linked = conn.prepare("SELECT src, dst FROM team_links WHERE space = ?")?;
        let mut out = mine.clone();
        for row in linked.query_map(params![space], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (src, dst) = row?;
            if mine.contains(&src) {
                out.insert(dst);
            }
            if mine.contains(&dst) {
                out.insert(src);
            }
        }
        Ok(out)
    }

    fn item_visible_to(
        conn: &Connection,
        space: &str,
        actor: &Actor,
        item: &BoardItem,
        worker_slice: Option<&HashSet<i64>>,
        claims_open: Option<bool>,
    ) -> Result<bool, BoardError> {
        if actor.role != Role::Worker {
            return Ok(true);
        }
        let owned = match worker_slice {
            Some(s) => s.clone(),
            None => Self::worker_slice(conn, space, &actor.id)?,
        };
        if owned.contains(&item.id) {
            return Ok(true);
        }
        let open = match claims_open {
            Some(v) => v,
            None => Self::claims_open(conn, space)?,
        };
        Ok(open && item.state == "open" && item.assignee.is_empty())
    }

    fn row_to_item(row: &rusqlite::Row<'_>) -> Result<BoardItem, rusqlite::Error> {
        let refs_raw: String = row.get(9)?;
        let refs: Vec<String> = serde_json::from_str(&refs_raw).unwrap_or_default();
        Ok(BoardItem {
            space: row.get(0)?,
            id: row.get(1)?,
            title: row.get(2)?,
            description: row.get(3)?,
            criteria: row.get(4)?,
            state: row.get(5)?,
            assignee: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            creator: row.get(7)?,
            case_id: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            refs,
            created_ts: row.get(10)?,
            updated_seq: row.get(11)?,
            links: Vec::new(),
            comments: Vec::new(),
            seq: None,
        })
    }

    fn load_item(conn: &Connection, space: &str, item_id: i64) -> Result<BoardItem, BoardError> {
        conn.query_row(
            "SELECT space, id, title, description, criteria, state, assignee, creator,
                    case_id, refs, created_ts, updated_seq
             FROM team_items WHERE space = ? AND id = ?",
            params![space, item_id],
            Self::row_to_item,
        )
        .optional()?
        .ok_or_else(|| BoardError::Bad(format!("no item #{item_id} in space '{space}'")))
    }

    fn links_of(conn: &Connection, space: &str, item_id: i64) -> Result<Vec<Value>, BoardError> {
        let mut stmt = conn.prepare(
            "SELECT src, kind, dst FROM team_links WHERE space = ? AND (src = ? OR dst = ?)",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![space, item_id, item_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })? {
            let (src, kind, dst) = row?;
            if src == item_id {
                out.push(json!({ "kind": kind, "item": dst }));
            } else {
                let inverse = if kind == "parent" { "child" } else { "blocked_by" };
                out.push(json!({ "kind": inverse, "item": src }));
            }
        }
        Ok(out)
    }

    pub fn create_item(
        &self,
        space: &str,
        actor: &Actor,
        title: &str,
        criteria: &str,
        description: &str,
        parent: Option<i64>,
        case: Option<&str>,
    ) -> Result<BoardItem, BoardError> {
        Self::require(actor, &[Role::User, Role::Lead, Role::Worker], "create_item")?;
        if space.is_empty() {
            return Err(BoardError::Bad("space is required".into()));
        }
        let title = title.trim();
        let criteria = criteria.trim();
        if title.is_empty() {
            return Err(BoardError::Bad("title is required".into()));
        }
        if criteria.is_empty() {
            return Err(BoardError::Bad(
                "acceptance criteria are required — they are what gets verified at review".into(),
            ));
        }

        let conn = self.conn.lock();
        let mut case_id = case.unwrap_or("").to_string();
        if let Some(parent_id) = parent {
            let parent_item = match Self::load_item(&conn, space, parent_id) {
                Ok(item) => item,
                Err(_) => {
                    return Err(BoardError::NotFound(format!(
                        "no visible item #{parent_id} in space {space:?}"
                    )));
                }
            };
            if !Self::item_visible_to(&conn, space, actor, &parent_item, None, None)? {
                return Err(BoardError::NotFound(format!(
                    "no visible item #{parent_id} in space {space:?}"
                )));
            }
            if case_id.is_empty() && !parent_item.case_id.is_empty() {
                case_id = parent_item.case_id.clone();
            }
        }

        let item_id = Self::next_item_id(&conn, space)?;
        let seq = Self::next_seq(&conn, space)?;
        let ts = now_iso();
        conn.execute(
            "INSERT INTO team_items
                (space, id, title, description, criteria, state, assignee, creator,
                 case_id, refs, created_ts, updated_seq)
             VALUES (?, ?, ?, ?, ?, 'open', '', ?, ?, '[]', ?, ?)",
            params![
                space,
                item_id,
                title,
                description,
                criteria,
                actor.id,
                case_id,
                ts,
                seq
            ],
        )?;
        if let Some(parent_id) = parent {
            conn.execute(
                "INSERT OR IGNORE INTO team_links (space, src, kind, dst) VALUES (?, ?, 'parent', ?)",
                params![space, item_id, parent_id],
            )?;
        }
        let _ = Self::write_event(
            &conn,
            space,
            seq,
            ITEM_CREATED,
            actor,
            Some(item_id),
            if case_id.is_empty() {
                None
            } else {
                Some(case_id.as_str())
            },
            json!({
                "title": title,
                "description": description,
                "criteria": criteria,
                "parent": parent,
                "case": if case_id.is_empty() { Value::Null } else { json!(case_id) },
            }),
        )?;
        drop(conn);
        self.get_item(space, item_id, actor).map(|mut item| {
            item.seq = Some(seq);
            item
        })
    }

    pub fn list_items(
        &self,
        space: &str,
        actor: &Actor,
        state: Option<&str>,
        assignee: Option<&str>,
    ) -> Result<Vec<BoardItem>, BoardError> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT space, id, title, description, criteria, state, assignee, creator,
                    case_id, refs, created_ts, updated_seq
             FROM team_items WHERE space = ?",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(space.to_string())];
        if let Some(state) = state.filter(|s| !s.is_empty()) {
            sql.push_str(" AND state = ?");
            params_vec.push(Box::new(state.to_string()));
        }
        if let Some(assignee) = assignee.filter(|s| !s.is_empty()) {
            sql.push_str(" AND assignee = ?");
            params_vec.push(Box::new(assignee.to_string()));
        }
        sql.push_str(" ORDER BY id");

        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), Self::row_to_item)?;
        let mut items: Vec<BoardItem> = rows.filter_map(|r| r.ok()).collect();

        let (worker_slice, claims_open) = if actor.role == Role::Worker {
            (
                Some(Self::worker_slice(&conn, space, &actor.id)?),
                Some(Self::claims_open(&conn, space)?),
            )
        } else {
            (None, None)
        };

        items.retain(|item| {
            Self::item_visible_to(
                &conn,
                space,
                actor,
                item,
                worker_slice.as_ref(),
                claims_open,
            )
            .unwrap_or(false)
        });

        for item in &mut items {
            item.links = Self::links_of(&conn, space, item.id)?;
        }
        Ok(items)
    }

    pub fn get_item(&self, space: &str, item_id: i64, actor: &Actor) -> Result<BoardItem, BoardError> {
        let conn = self.conn.lock();
        let mut item = match Self::load_item(&conn, space, item_id) {
            Ok(item) => item,
            Err(_) => {
                return Err(BoardError::NotFound(format!(
                    "no visible item #{item_id} in space {space:?}"
                )));
            }
        };
        if !Self::item_visible_to(&conn, space, actor, &item, None, None)? {
            return Err(BoardError::NotFound(format!(
                "no visible item #{item_id} in space {space:?}"
            )));
        }
        item.links = Self::links_of(&conn, space, item_id)?;
        // Comments live on the event log upstream; scaffolding leaves them empty.
        Ok(item)
    }

    pub fn assign(
        &self,
        space: &str,
        actor: &Actor,
        item_id: i64,
        assignee: &str,
    ) -> Result<BoardItem, BoardError> {
        Self::require(actor, &[Role::User, Role::Lead], "assign")?;
        let assignee = assignee.trim();
        if assignee.is_empty() {
            return Err(BoardError::Bad("assignee is required".into()));
        }
        let conn = self.conn.lock();
        let item = Self::load_item(&conn, space, item_id)?;
        if item.state == "done" || item.state == "canceled" {
            return Err(BoardError::Bad(format!(
                "cannot assign an item in state {} — reopen it first",
                item.state
            )));
        }
        let seq = Self::next_seq(&conn, space)?;
        let previous = item.assignee.clone();
        conn.execute(
            "UPDATE team_items SET assignee = ?, updated_seq = ? WHERE space = ? AND id = ?",
            params![assignee, seq, space, item_id],
        )?;
        let _ = Self::write_event(
            &conn,
            space,
            seq,
            ITEM_ASSIGNED,
            actor,
            Some(item_id),
            if item.case_id.is_empty() {
                None
            } else {
                Some(item.case_id.as_str())
            },
            json!({
                "assignee": assignee,
                "previous": previous,
            }),
        )?;
        drop(conn);
        self.get_item(space, item_id, actor).map(|mut item| {
            item.seq = Some(seq);
            item
        })
    }

    fn check_transition_authority(
        conn: &Connection,
        space: &str,
        actor: &Actor,
        item: &BoardItem,
        target: &str,
    ) -> Result<(), BoardError> {
        if actor.role == Role::System {
            return Err(BoardError::Authority(
                "system events cannot transition items".into(),
            ));
        }
        if target == "done" && actor.role == Role::Worker {
            return Err(BoardError::Authority(
                "workers finish by moving to review — done is the verdict after verification"
                    .into(),
            ));
        }
        if actor.role == Role::Worker {
            let slice = Self::worker_slice(conn, space, &actor.id)?;
            if !slice.contains(&item.id) {
                return Err(BoardError::Authority(format!(
                    "worker {} may only transition items on its assigned slice",
                    actor.id
                )));
            }
            if !WORKER_TARGETS.contains(&target) {
                return Err(BoardError::Authority(format!(
                    "workers may move their item to {WORKER_TARGETS:?} only"
                )));
            }
        }
        Ok(())
    }

    pub fn transition(
        &self,
        space: &str,
        actor: &Actor,
        item_id: i64,
        to: &str,
        comment: &str,
    ) -> Result<BoardItem, BoardError> {
        let to = to.trim();
        if to.is_empty() {
            return Err(BoardError::Bad("transition target is required".into()));
        }
        let conn = self.conn.lock();
        let item = Self::load_item(&conn, space, item_id)?;
        let current = item.state.as_str();
        if !edges(current).contains(&to) {
            return Err(BoardError::Bad(format!(
                "illegal transition {current} → {to}"
            )));
        }
        Self::check_transition_authority(&conn, space, actor, &item, to)?;
        let seq = Self::next_seq(&conn, space)?;
        conn.execute(
            "UPDATE team_items SET state = ?, updated_seq = ? WHERE space = ? AND id = ?",
            params![to, seq, space, item_id],
        )?;
        let _ = Self::write_event(
            &conn,
            space,
            seq,
            ITEM_TRANSITIONED,
            actor,
            Some(item_id),
            if item.case_id.is_empty() {
                None
            } else {
                Some(item.case_id.as_str())
            },
            json!({
                "from": current,
                "to": to,
                "comment": comment,
                "refs": [],
            }),
        )?;
        drop(conn);
        self.get_item(space, item_id, actor).map(|mut item| {
            item.seq = Some(seq);
            item
        })
    }

    pub fn comment(
        &self,
        space: &str,
        actor: &Actor,
        item_id: i64,
        body: &str,
    ) -> Result<Value, BoardError> {
        if body.trim().is_empty() {
            return Err(BoardError::Bad("comment body is required".into()));
        }
        let conn = self.conn.lock();
        let item = Self::load_item(&conn, space, item_id)?;
        if actor.role == Role::Worker {
            let slice = Self::worker_slice(&conn, space, &actor.id)?;
            if !slice.contains(&item_id) {
                return Err(BoardError::Authority(format!(
                    "worker {} may only comment on its assigned items and items linked to them",
                    actor.id
                )));
            }
        }
        Self::append_event_locked(
            &conn,
            space,
            ITEM_COMMENTED,
            actor,
            Some(item_id),
            if item.case_id.is_empty() {
                None
            } else {
                Some(item.case_id.as_str())
            },
            json!({
                "body": body,
                "refs": [],
            }),
        )
    }

    pub fn link(
        &self,
        space: &str,
        actor: &Actor,
        src: i64,
        kind: &str,
        dst: i64,
    ) -> Result<(), BoardError> {
        Self::require(actor, &[Role::User, Role::Lead], "link")?;
        if kind != "parent" && kind != "blocks" {
            return Err(BoardError::Bad(format!(
                "unknown link kind: {kind} (use one of (\"parent\", \"blocks\"))"
            )));
        }
        if src == dst {
            return Err(BoardError::Bad("an item cannot link to itself".into()));
        }
        let conn = self.conn.lock();
        Self::load_item(&conn, space, src)?;
        Self::load_item(&conn, space, dst)?;
        conn.execute(
            "INSERT OR IGNORE INTO team_links (space, src, kind, dst) VALUES (?, ?, ?, ?)",
            params![space, src, kind, dst],
        )?;
        Ok(())
    }

    /// Record an authoritative attachment reference on a visible item.
    pub fn attach_ref(
        &self,
        space: &str,
        actor: &Actor,
        item_id: i64,
        body: &str,
        ref_str: &str,
    ) -> Result<Value, BoardError> {
        if body.trim().is_empty() {
            return Err(BoardError::Bad("comment body is required".into()));
        }
        let stored = stored_name(ref_str)
            .ok_or_else(|| BoardError::Bad(format!("not an attachment ref: {ref_str:?}")))?;
        let stored = validate_stored_name(stored)?;

        let conn = self.conn.lock();
        let item = Self::load_item(&conn, space, item_id)?;
        if actor.role == Role::Worker {
            let slice = Self::worker_slice(&conn, space, &actor.id)?;
            if !slice.contains(&item_id) {
                return Err(BoardError::Authority(format!(
                    "worker {} may only comment on its assigned items and items linked to them",
                    actor.id
                )));
            }
        }
        let seq = Self::next_seq(&conn, space)?;
        conn.execute(
            "INSERT OR IGNORE INTO team_attachment_refs (space, stored, item_id, event_seq)
             VALUES (?, ?, ?, ?)",
            params![space, stored, item_id, seq],
        )?;
        // Also fold the attachment:// ref onto the item for introspection.
        let mut refs = item.refs;
        if !refs.iter().any(|r| r == ref_str) {
            refs.push(ref_str.to_string());
        }
        conn.execute(
            "UPDATE team_items SET refs = ?, updated_seq = ? WHERE space = ? AND id = ?",
            params![serde_json::to_string(&refs)?, seq, space, item_id],
        )?;
        Ok(json!({
            "seq": seq,
            "payload": {
                "body": body,
                "refs": [ref_str],
                "attachments": [stored],
            }
        }))
    }

    /// Require an actor-visible item in `space` to carry an authoritative attachment.
    ///
    /// Upstream name: `require_attachment_access`. Space is mandatory — attachment
    /// bytes are never readable across spaces even for leads.
    pub fn require_attachment_access(
        &self,
        space: &str,
        actor: &Actor,
        stored: &str,
    ) -> Result<(), BoardError> {
        if space.is_empty() {
            return Err(BoardError::Bad("space is required".into()));
        }
        let stored = validate_stored_name(stored)?;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT item.space, item.id, item.title, item.description, item.criteria,
                    item.state, item.assignee, item.creator, item.case_id, item.refs,
                    item.created_ts, item.updated_seq
             FROM team_items AS item
             JOIN team_attachment_refs AS attachment
               ON attachment.space = item.space AND attachment.item_id = item.id
             WHERE attachment.space = ? AND attachment.stored = ?
             ORDER BY item.id",
        )?;
        let rows = stmt.query_map(params![space, stored], Self::row_to_item)?;
        let (worker_slice, claims_open) = if actor.role == Role::Worker {
            (
                Some(Self::worker_slice(&conn, space, &actor.id)?),
                Some(Self::claims_open(&conn, space)?),
            )
        } else {
            (None, None)
        };
        for row in rows {
            let item = row?;
            if Self::item_visible_to(
                &conn,
                space,
                actor,
                &item,
                worker_slice.as_ref(),
                claims_open,
            )? {
                return Ok(());
            }
        }
        Err(BoardError::NotFound("attachment not found".into()))
    }

    /// Alias used by the HTTP layer / dialect seam.
    pub fn attachment_read(
        &self,
        space: &str,
        actor: &Actor,
        stored: &str,
    ) -> Result<(), BoardError> {
        self.require_attachment_access(space, actor, stored)
    }

    pub fn spaces(&self) -> Result<Vec<String>, BoardError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT DISTINCT space FROM team_items ORDER BY space")?;
        let spaces = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(spaces)
    }

    /// Self-assign an open, unassigned item (store arbitrates under the write lock).
    pub fn claim(
        &self,
        space: &str,
        actor: &Actor,
        item_id: i64,
    ) -> Result<BoardItem, BoardError> {
        Self::require(actor, &[Role::User, Role::Lead, Role::Worker], "claim")?;
        let conn = self.conn.lock();
        if actor.role == Role::Worker && !Self::claims_open(&conn, space)? {
            return Err(BoardError::Authority(
                "claims are lead-only on this board — ask the lead to assign the item to you"
                    .into(),
            ));
        }
        let item = Self::load_item(&conn, space, item_id)?;
        if item.state != "open" {
            return Err(BoardError::Bad(format!(
                "item #{item_id} is {} — only open items can be claimed",
                item.state
            )));
        }
        if !item.assignee.is_empty() {
            return Err(BoardError::Bad(format!(
                "item #{item_id} is already claimed by {}",
                item.assignee
            )));
        }
        let seq = Self::next_seq(&conn, space)?;
        conn.execute(
            "UPDATE team_items SET assignee = ?, updated_seq = ? WHERE space = ? AND id = ?",
            params![actor.id, seq, space, item_id],
        )?;
        let _ = Self::write_event(
            &conn,
            space,
            seq,
            ITEM_ASSIGNED,
            actor,
            Some(item_id),
            if item.case_id.is_empty() {
                None
            } else {
                Some(item.case_id.as_str())
            },
            json!({
                "assignee": actor.id,
                "previous": "",
                "claimed": true,
            }),
        )?;
        drop(conn);
        self.get_item(space, item_id, actor).map(|mut item| {
            item.seq = Some(seq);
            item
        })
    }

    fn cursor(&self, conn: &Connection, key: &str) -> Result<i64, BoardError> {
        Ok(conn
            .query_row(
                "SELECT consumed_seq FROM team_cursors WHERE cursor_key = ?",
                params![key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    fn set_cursor(conn: &Connection, key: &str, upto_seq: i64) -> Result<(), BoardError> {
        conn.execute(
            "INSERT INTO team_cursors (cursor_key, consumed_seq) VALUES (?, ?)
             ON CONFLICT(cursor_key) DO UPDATE SET consumed_seq =
               MAX(consumed_seq, excluded.consumed_seq)",
            params![key, upto_seq],
        )?;
        Ok(())
    }

    /// Unconsumed events this actor is subscribed to (assignment-relation feed).
    pub fn feed_for(
        &self,
        space: &str,
        actor_id: &str,
        limit: i64,
    ) -> Result<Vec<Value>, BoardError> {
        let key = format!("feed:{actor_id}:{space}");
        let conn = self.conn.lock();
        let since = self.cursor(&conn, &key)?;
        let slice = Self::worker_slice(&conn, space, actor_id)?;
        let mut stmt = conn.prepare(
            "SELECT space, seq, kind, actor, role, item_id, case_id, payload, ts
             FROM team_events WHERE space = ? AND seq > ? ORDER BY seq LIMIT ?",
        )?;
        let rows = stmt.query_map(params![space, since, limit], Self::row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            let event = row?;
            let actor = event.get("actor").and_then(|a| a.as_str()).unwrap_or("");
            if actor == actor_id {
                continue;
            }
            let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let payload = event.get("payload").cloned().unwrap_or(json!({}));
            if kind == ITEM_ASSIGNED
                && (payload.get("assignee").and_then(|a| a.as_str()) == Some(actor_id)
                    || payload.get("previous").and_then(|a| a.as_str()) == Some(actor_id))
            {
                out.push(event);
                continue;
            }
            if let Some(item_id) = event.get("item_id").and_then(|i| i.as_i64()) {
                if slice.contains(&item_id) {
                    out.push(event);
                }
            }
        }
        Ok(out)
    }

    pub fn consume_feed(&self, space: &str, actor_id: &str, upto_seq: i64) -> Result<(), BoardError> {
        let key = format!("feed:{actor_id}:{space}");
        let conn = self.conn.lock();
        Self::set_cursor(&conn, &key, upto_seq)
    }

    /// Lead subscription allowlist: review/blocked transitions, new filings, claims.
    pub const SUBSCRIBED_TRANSITIONS: &'static [&'static str] = &["review", "blocked"];

    /// Unconsumed subscription-worthy events on a space for one subscriber (lead).
    pub fn subscribed_events(
        &self,
        space: &str,
        subscriber: &str,
        limit: i64,
    ) -> Result<Vec<Value>, BoardError> {
        let key = format!("sub:{subscriber}:{space}");
        let conn = self.conn.lock();
        let since = self.cursor(&conn, &key)?;
        let mut stmt = conn.prepare(
            "SELECT space, seq, kind, actor, role, item_id, case_id, payload, ts
             FROM team_events WHERE space = ? AND seq > ? ORDER BY seq LIMIT ?",
        )?;
        let rows = stmt.query_map(params![space, since, limit], Self::row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            let event = row?;
            let actor = event.get("actor").and_then(|a| a.as_str()).unwrap_or("");
            if actor == subscriber {
                continue;
            }
            let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let payload = event.get("payload").cloned().unwrap_or(json!({}));
            match kind {
                ITEM_TRANSITIONED => {
                    let to = payload.get("to").and_then(|t| t.as_str()).unwrap_or("");
                    if !Self::SUBSCRIBED_TRANSITIONS.contains(&to) {
                        continue;
                    }
                }
                ITEM_CREATED => {}
                ITEM_ASSIGNED => {
                    if !payload
                        .get("claimed")
                        .and_then(|c| c.as_bool())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                _ => continue,
            }
            out.push(event);
        }
        Ok(out)
    }

    pub fn consume_subscription(
        &self,
        space: &str,
        subscriber: &str,
        upto_seq: i64,
    ) -> Result<(), BoardError> {
        let key = format!("sub:{subscriber}:{space}");
        let conn = self.conn.lock();
        Self::set_cursor(&conn, &key, upto_seq)
    }
}

// ---------------------------------------------------------------------------
// Attachment blob store (content-addressed files)
// ---------------------------------------------------------------------------

pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn path_for(&self, stored: &str) -> Result<PathBuf, BoardError> {
        let stored = validate_stored_name(stored)?;
        let path = self.root.join(&stored);
        if !path.exists() {
            return Err(BoardError::NotFound("attachment not found".into()));
        }
        Ok(path)
    }

    pub fn mime_for(stored: &str) -> &'static str {
        match stored.rsplit('.').next().unwrap_or("") {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "application/octet-stream",
        }
    }

    /// Write bytes under a caller-provided stored name (tests / scaffolding).
    pub fn put_named(&self, stored: &str, data: &[u8]) -> Result<String, BoardError> {
        let stored = validate_stored_name(stored)?;
        std::fs::create_dir_all(&self.root).map_err(|e| BoardError::Bad(e.to_string()))?;
        let target = self.root.join(&stored);
        if !target.exists() {
            std::fs::write(&target, data).map_err(|e| BoardError::Bad(e.to_string()))?;
        }
        Ok(format!("{ATTACHMENT_SCHEME}{stored}#blob"))
    }

    /// Store one attachment with a content-addressed name; returns `attachment://` ref.
    pub fn put_bytes(&self, data: &[u8], filename: &str, stored: &str) -> Result<String, BoardError> {
        let stored = validate_stored_name(stored)?;
        if data.is_empty() {
            return Err(BoardError::Bad("empty attachment".into()));
        }
        if data.len() > 10 * 1024 * 1024 {
            return Err(BoardError::Bad("attachment exceeds 10MB".into()));
        }
        std::fs::create_dir_all(&self.root).map_err(|e| BoardError::Bad(e.to_string()))?;
        let target = self.root.join(&stored);
        if !target.exists() {
            let tmp = self.root.join(format!("{stored}.tmp"));
            std::fs::write(&tmp, data).map_err(|e| BoardError::Bad(e.to_string()))?;
            std::fs::rename(&tmp, &target).map_err(|e| BoardError::Bad(e.to_string()))?;
        }
        let safe_name = Path::new(filename)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("blob")
            .replace('#', "_");
        Ok(format!("{ATTACHMENT_SCHEME}{stored}#{safe_name}"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lead() -> Actor {
        Actor::new("lead-1", Role::Lead)
    }
    fn worker(id: &str) -> Actor {
        Actor::new(id, Role::Worker)
    }

    fn fake_stored(tag: u8) -> String {
        format!("{:0>64}.png", format!("{tag:x}"))
    }

    #[test]
    fn worker_sees_only_its_slice_on_list_and_get() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let w1 = worker("worker-1");
        let w2 = worker("worker-2");

        let mine = store
            .create_item(space, &lead, "Mine", "c", "", None, None)
            .unwrap();
        store.assign(space, &lead, mine.id, "worker-1").unwrap();

        let theirs = store
            .create_item(space, &lead, "Theirs", "c", "", None, None)
            .unwrap();
        store.assign(space, &lead, theirs.id, "worker-2").unwrap();

        let linked = store
            .create_item(space, &lead, "Dep", "c", "", None, None)
            .unwrap();
        store
            .link(space, &lead, linked.id, "blocks", mine.id)
            .unwrap();

        let claimable = store
            .create_item(space, &lead, "Available", "c", "", None, None)
            .unwrap();

        let visible: HashSet<i64> = store
            .list_items(space, &w1, None, None)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(
            visible,
            HashSet::from([mine.id, linked.id, claimable.id])
        );
        assert!(!visible.contains(&theirs.id));

        assert_eq!(store.get_item(space, mine.id, &w1).unwrap().id, mine.id);
        assert_eq!(
            store.get_item(space, linked.id, &w1).unwrap().id,
            linked.id
        );
        assert_eq!(
            store.get_item(space, claimable.id, &w1).unwrap().id,
            claimable.id
        );
        let err = store.get_item(space, theirs.id, &w1).unwrap_err();
        assert!(matches!(err, BoardError::NotFound(_)));
        assert!(err.to_string().contains("no visible item"));

        // Hidden from worker-2's perspective too for worker-1's item.
        let err = store.get_item(space, mine.id, &w2).unwrap_err();
        assert!(matches!(err, BoardError::NotFound(_)));
    }

    #[test]
    fn lead_only_claims_hide_unassigned_open_items_from_workers() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let w = worker("nia");

        let held = store
            .create_item(space, &lead, "Held", "c", "", None, None)
            .unwrap();
        assert!(store.get_item(space, held.id, &w).is_ok());

        store.set_policy(space, &lead, "lead-only").unwrap();
        let err = store.get_item(space, held.id, &w).unwrap_err();
        assert!(matches!(err, BoardError::NotFound(_)));
    }

    #[test]
    fn attachment_read_requires_space_and_visible_item() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let nia = worker("nia");
        let webb = worker("webb");

        let private = store
            .create_item(space, &lead, "Webb evidence", "c", "", None, None)
            .unwrap();
        store.assign(space, &lead, private.id, "webb").unwrap();

        let stored = fake_stored(0xa);
        let ref_str = format!("{ATTACHMENT_SCHEME}{stored}#private.png");
        store
            .attach_ref(space, &lead, private.id, "private", &ref_str)
            .unwrap();

        // Lead can read in the correct space.
        store
            .require_attachment_access(space, &lead, &stored)
            .unwrap();
        // Wrong space → not found (even for lead).
        let err = store
            .require_attachment_access("another-space", &lead, &stored)
            .unwrap_err();
        assert_eq!(err.to_string(), "attachment not found");

        // Assignee worker can read.
        store
            .require_attachment_access(space, &webb, &stored)
            .unwrap();

        // Foreign worker cannot — same 404 contract.
        let err = store
            .require_attachment_access(space, &nia, &stored)
            .unwrap_err();
        assert_eq!(err.to_string(), "attachment not found");

        // Empty space refused.
        let err = store
            .require_attachment_access("", &lead, &stored)
            .unwrap_err();
        assert!(err.to_string().contains("space is required"));

        // Malformed name refused.
        let err = store
            .require_attachment_access(space, &lead, "../../private.png")
            .unwrap_err();
        assert!(err.to_string().contains("not an attachment name"));

        // Unreferenced blob → not found.
        let orphan = fake_stored(0xb);
        let err = store
            .require_attachment_access(space, &lead, &orphan)
            .unwrap_err();
        assert_eq!(err.to_string(), "attachment not found");
    }

    #[test]
    fn attachment_read_respects_claim_pool_visibility() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let nia = worker("nia");

        let claimable = store
            .create_item(space, &lead, "Available", "c", "", None, None)
            .unwrap();
        let stored = fake_stored(0xc);
        let ref_str = format!("{ATTACHMENT_SCHEME}{stored}#available.png");
        store
            .attach_ref(space, &lead, claimable.id, "available", &ref_str)
            .unwrap();

        // Open claims: worker can read attachment on the claim pool item.
        store
            .require_attachment_access(space, &nia, &stored)
            .unwrap();

        store.set_policy(space, &lead, "lead-only").unwrap();
        let err = store
            .require_attachment_access(space, &nia, &stored)
            .unwrap_err();
        assert_eq!(err.to_string(), "attachment not found");
    }

    #[test]
    fn transition_respects_edges_and_writes_events() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let user = Actor::new("user", Role::User);
        let w = worker("nia");

        let item = store
            .create_item(space, &lead, "Task", "done when shipped", "", None, None)
            .unwrap();
        store.assign(space, &lead, item.id, "nia").unwrap();

        // Illegal: open → review
        let err = store
            .transition(space, &user, item.id, "review", "")
            .unwrap_err();
        assert!(err.to_string().contains("illegal transition"));

        // Legal: open → in_progress
        let moved = store
            .transition(space, &user, item.id, "in_progress", "starting")
            .unwrap();
        assert_eq!(moved.state, "in_progress");

        // Worker cannot go to done
        store
            .transition(space, &w, item.id, "review", "")
            .unwrap();
        let err = store
            .transition(space, &w, item.id, "done", "")
            .unwrap_err();
        assert!(matches!(err, BoardError::Authority(_)));

        // Lead can finish
        let done = store
            .transition(space, &lead, item.id, "done", "lgtm")
            .unwrap();
        assert_eq!(done.state, "done");

        // Done has no outbound edges
        let err = store
            .transition(space, &lead, item.id, "open", "")
            .unwrap_err();
        assert!(err.to_string().contains("illegal transition"));

        let events = store.events(space, Some(item.id)).unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .filter_map(|e| e.get("kind").and_then(|k| k.as_str()))
            .collect();
        assert!(kinds.contains(&"item_created"));
        assert!(kinds.contains(&"item_assigned"));
        assert!(kinds.contains(&"item_transitioned"));

        let note = store.comment(space, &user, item.id, "shipped").unwrap();
        assert!(note.get("seq").and_then(|s| s.as_i64()).unwrap() > 0);
        assert_eq!(
            note.get("kind").and_then(|k| k.as_str()),
            Some("item_commented")
        );
    }

    #[test]
    fn space_for_workspace_resolves_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let space = space_for_workspace(dir.path().to_str().unwrap());
        assert!(PathBuf::from(&space).is_absolute());
        assert_eq!(
            PathBuf::from(&space).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn lead_subscriptions_are_an_allowlist() {
        let store = TeamStore::open_in_memory().unwrap();
        let space = "proj";
        let lead = lead();
        let w = worker("swe-worker");
        let item = store
            .create_item(space, &lead, "Task", "tests pass", "", None, None)
            .unwrap();
        store.assign(space, &lead, item.id, "swe-worker").unwrap();
        store
            .transition(space, &w, item.id, "in_progress", "")
            .unwrap();
        store.comment(space, &w, item.id, "halfway").unwrap();
        store
            .transition(space, &w, item.id, "review", "done, please check")
            .unwrap();
        let _filed = store
            .create_item(space, &w, "Found a bug", "fix", "", None, None)
            .unwrap();
        let subs = store.subscribed_events(space, "lead-1", 200).unwrap();
        assert!(subs
            .iter()
            .all(|e| e.get("actor").and_then(|a| a.as_str()) != Some("lead-1")));
        let pairs: std::collections::HashSet<(String, Option<String>)> = subs
            .iter()
            .map(|e| {
                let kind = e.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
                let to = e
                    .get("payload")
                    .and_then(|p| p.get("to"))
                    .and_then(|t| t.as_str())
                    .map(String::from);
                (kind, to)
            })
            .collect();
        assert!(pairs.contains(&("item_transitioned".into(), Some("review".into()))));
        assert!(pairs.contains(&("item_created".into(), None)));
        let last = subs.last().unwrap().get("seq").and_then(|s| s.as_i64()).unwrap();
        store.consume_subscription(space, "lead-1", last).unwrap();
        assert!(store.subscribed_events(space, "lead-1", 200).unwrap().is_empty());
    }
}
