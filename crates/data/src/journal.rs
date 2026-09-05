//! Case-keyed journal store — Rust port of `coworker/teams/journal.py`.
//!
//! Cases outlive boards and teams: append-only, hash-chained per case, with a
//! grant table so assignment can feed access ("sharing rides assignment").

use crate::teams::{Actor, BoardError, Role};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const GENESIS: &str = "genesis";

pub const JOURNAL_KINDS: &[&str] = &["finding", "evidence", "decision", "note", "raw"];
pub const JOURNAL_BODY_LIMIT: usize = 16_000;

const HASHED_FIELDS: &[&str] = &[
    "ts",
    "case_id",
    "kind",
    "actor",
    "actor_role",
    "space",
    "item_id",
    "payload",
    "taint",
    "prev_hash",
];

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Compact JSON with sorted object keys — mirrors Python `_canonical`.
fn canonical(value: &Value) -> String {
    canonical_value(value).to_string()
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<&str, &Value> =
                map.iter().map(|(k, v)| (k.as_str(), v)).collect();
            let mut out = Map::new();
            for (k, v) in ordered {
                out.insert(k.to_string(), canonical_value(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

fn hash_record(record: &BTreeMap<&str, Value>) -> String {
    let mut map = Map::new();
    for key in HASHED_FIELDS {
        if let Some(v) = record.get(key) {
            map.insert((*key).to_string(), v.clone());
        }
    }
    let material = canonical(&Value::Object(map));
    let digest = Sha256::digest(material.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_valid_kind(kind: &str) -> bool {
    JOURNAL_KINDS.contains(&kind)
}

pub struct JournalStore {
    conn: Mutex<Connection>,
}

impl JournalStore {
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
            CREATE TABLE IF NOT EXISTS journal_entries (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                case_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                actor TEXT NOT NULL,
                actor_role TEXT NOT NULL,
                persona TEXT DEFAULT '',
                model TEXT DEFAULT '',
                session_id TEXT DEFAULT '',
                space TEXT,
                item_id INTEGER,
                payload TEXT NOT NULL,
                taint INTEGER NOT NULL DEFAULT 0,
                prev_hash TEXT NOT NULL,
                hash TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_journal_case
                ON journal_entries (case_id, seq);
            CREATE INDEX IF NOT EXISTS idx_journal_item
                ON journal_entries (case_id, space, item_id, seq);
            CREATE TABLE IF NOT EXISTS journal_grants (
                case_id TEXT NOT NULL,
                principal TEXT NOT NULL,
                source TEXT NOT NULL,
                space TEXT DEFAULT '',
                item_id INTEGER,
                UNIQUE (case_id, principal, source, space, item_id)
            );
            CREATE TABLE IF NOT EXISTS journal_meta (
                case_id TEXT PRIMARY KEY,
                head_hash TEXT NOT NULL,
                created_ts TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn append(
        &self,
        actor: &Actor,
        case: &str,
        body: &str,
        kind: &str,
        space: Option<&str>,
        item: Option<i64>,
        entities: Option<&[String]>,
        refs: Option<&[String]>,
    ) -> Result<Value, BoardError> {
        self.append_full(actor, case, body, kind, space, item, entities, refs, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_full(
        &self,
        actor: &Actor,
        case: &str,
        body: &str,
        kind: &str,
        space: Option<&str>,
        item: Option<i64>,
        entities: Option<&[String]>,
        refs: Option<&[String]>,
        taint: bool,
    ) -> Result<Value, BoardError> {
        let case = case.trim();
        if case.is_empty() {
            return Err(BoardError::Bad("case is required".into()));
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(BoardError::Bad("entry body is required".into()));
        }
        if !is_valid_kind(kind) {
            return Err(BoardError::Bad(format!(
                "unknown entry kind: {kind} (use one of {JOURNAL_KINDS:?})"
            )));
        }
        if body.chars().count() > JOURNAL_BODY_LIMIT {
            return Err(BoardError::Bad(format!(
                "entry body over {JOURNAL_BODY_LIMIT} chars — save the full\
                 capture to a file and journal an excerpt that references it"
            )));
        }

        let entity_list: Vec<String> = {
            let set: BTreeSet<String> = entities
                .unwrap_or(&[])
                .iter()
                .cloned()
                .collect();
            set.into_iter().collect()
        };
        let ref_list: Vec<String> = refs
            .unwrap_or(&[])
            .iter()
            .map(|r| r.to_string())
            .collect();

        let payload = canonical(&json!({
            "body": body,
            "entities": entity_list,
            "refs": ref_list,
        }));

        let conn = self.conn.lock();
        let exists = Self::case_exists(&conn, case)?;
        if exists {
            Self::check_access(&conn, actor, case)?;
        }

        let ts = now_iso();
        let prev = Self::head_hash(&conn, case)?;
        let space_val = space.filter(|s| !s.is_empty());

        let mut record: BTreeMap<&str, Value> = BTreeMap::new();
        record.insert("ts", json!(ts));
        record.insert("case_id", json!(case));
        record.insert("kind", json!(kind));
        record.insert("actor", json!(actor.id));
        record.insert("actor_role", json!(actor.role.as_str()));
        record.insert(
            "space",
            space_val.map(|s| json!(s)).unwrap_or(Value::Null),
        );
        record.insert(
            "item_id",
            item.map(|i| json!(i)).unwrap_or(Value::Null),
        );
        record.insert("payload", json!(payload));
        record.insert("taint", json!(if taint { 1 } else { 0 }));
        record.insert("prev_hash", json!(prev));
        let entry_hash = hash_record(&record);
        record.insert("hash", json!(entry_hash.clone()));

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| BoardError::Bad(format!("sqlite: {e}")))?;
        tx.execute(
            "INSERT INTO journal_entries
                (ts, case_id, kind, actor, actor_role, persona, model,
                 session_id, space, item_id, payload, taint, prev_hash, hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ts,
                case,
                kind,
                actor.id,
                actor.role.as_str(),
                actor.persona,
                actor.model,
                actor.session_id,
                space_val,
                item,
                payload,
                if taint { 1 } else { 0 },
                prev,
                entry_hash,
            ],
        )?;
        let seq = tx.last_insert_rowid();

        if !exists {
            tx.execute(
                "INSERT INTO journal_meta (case_id, head_hash, created_ts) VALUES (?, ?, ?)",
                params![case, entry_hash, ts],
            )?;
            Self::grant_locked(&tx, case, &actor.id, "creator", "", None)?;
        } else {
            tx.execute(
                "UPDATE journal_meta SET head_hash = ? WHERE case_id = ?",
                params![entry_hash, case],
            )?;
        }
        tx.commit()
            .map_err(|e| BoardError::Bad(format!("sqlite: {e}")))?;

        let mut out = Map::new();
        for (k, v) in record {
            out.insert(k.to_string(), v);
        }
        out.insert("seq".into(), json!(seq));
        Ok(Value::Object(out))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn read(
        &self,
        actor: &Actor,
        case: &str,
        item: Option<i64>,
        author: Option<&str>,
        kind: Option<&str>,
        entity: Option<&str>,
        since_seq: i64,
        include_raw: bool,
        limit: i64,
    ) -> Result<Vec<Value>, BoardError> {
        let conn = self.conn.lock();
        Self::check_access(&conn, actor, case)?;

        let mut where_clauses = vec!["case_id = ?", "seq > ?"];
        let mut params_owned: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(case.to_string()), Box::new(since_seq)];

        if let Some(item_id) = item {
            where_clauses.push("item_id = ?");
            params_owned.push(Box::new(item_id));
        }
        if let Some(author) = author.filter(|a| !a.is_empty()) {
            where_clauses.push("actor = ?");
            params_owned.push(Box::new(author.to_string()));
        }
        if let Some(kind) = kind.filter(|k| !k.is_empty()) {
            if !is_valid_kind(kind) {
                return Err(BoardError::Bad(format!("unknown entry kind: {kind}")));
            }
            where_clauses.push("kind = ?");
            params_owned.push(Box::new(kind.to_string()));
        } else if !include_raw {
            where_clauses.push("kind != 'raw'");
        }

        let sql = format!(
            "SELECT seq, ts, case_id, kind, actor, actor_role, persona, model,
                    session_id, space, item_id, payload, taint, prev_hash, hash
             FROM journal_entries WHERE {} ORDER BY seq",
            where_clauses.join(" AND ")
        );
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params_owned.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6).unwrap_or_default(),
                row.get::<_, String>(7).unwrap_or_default(),
                row.get::<_, String>(8).unwrap_or_default(),
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
            ))
        })?;

        let cap = limit.clamp(1, 1000) as usize;
        let mut out = Vec::new();
        for row in rows {
            let (
                seq,
                ts,
                case_id,
                kind,
                actor_id,
                actor_role,
                persona,
                model,
                session_id,
                space,
                item_id,
                payload,
                taint,
                prev_hash,
                hash,
            ) = row?;
            let entry = row_to_entry(
                seq,
                &ts,
                &case_id,
                &kind,
                &actor_id,
                &actor_role,
                &persona,
                &model,
                &session_id,
                space.as_deref(),
                item_id,
                &payload,
                taint,
                &prev_hash,
                &hash,
            );
            if let Some(entity) = entity.filter(|e| !e.is_empty()) {
                let entities = entry
                    .get("entities")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !entities.iter().any(|e| e.as_str() == Some(entity)) {
                    continue;
                }
            }
            out.push(entry);
            if out.len() >= cap {
                break;
            }
        }
        Ok(out)
    }

    pub fn overview(&self, actor: &Actor) -> Result<Vec<Value>, BoardError> {
        let visible = self.cases(actor)?;
        if visible.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT case_id, COUNT(*) AS entries, MAX(ts) AS last_ts
             FROM journal_entries GROUP BY case_id",
        )?;
        let mut counts: BTreeMap<String, (i64, String)> = BTreeMap::new();
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (case_id, entries, last_ts) = row?;
            counts.insert(case_id, (entries, last_ts));
        }
        Ok(visible
            .into_iter()
            .map(|case| {
                let (entries, last_ts) = counts
                    .get(&case)
                    .cloned()
                    .unwrap_or((0, String::new()));
                json!({
                    "case": case,
                    "entries": entries,
                    "last_ts": last_ts,
                })
            })
            .collect())
    }

    pub fn cases(&self, actor: &Actor) -> Result<Vec<String>, BoardError> {
        let conn = self.conn.lock();
        if actor.role == Role::User {
            let mut stmt =
                conn.prepare("SELECT case_id FROM journal_meta ORDER BY case_id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        } else {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT case_id FROM journal_grants WHERE principal = ?
                 ORDER BY case_id",
            )?;
            let rows = stmt.query_map(params![actor.id], |r| r.get(0))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }
    }

    pub fn grant(&self, actor: &Actor, case: &str, principal: &str) -> Result<(), BoardError> {
        if actor.role == Role::Worker || actor.role == Role::System {
            return Err(BoardError::Authority(
                "only the user or a lead may grant a case".into(),
            ));
        }
        let conn = self.conn.lock();
        if !Self::case_exists(&conn, case)? {
            return Err(BoardError::Bad(format!("no case '{case}'")));
        }
        if actor.role == Role::Lead {
            Self::check_access(&conn, actor, case)?;
        }
        Self::grant_locked(&conn, case, principal, "grant", "", None)?;
        Ok(())
    }

    pub fn revoke(&self, actor: &Actor, case: &str, principal: &str) -> Result<(), BoardError> {
        if actor.role == Role::Worker || actor.role == Role::System {
            return Err(BoardError::Authority(
                "only the user or a lead may revoke a case grant".into(),
            ));
        }
        let conn = self.conn.lock();
        if actor.role == Role::Lead {
            Self::check_access(&conn, actor, case)?;
        }
        conn.execute(
            "DELETE FROM journal_grants WHERE case_id = ? AND principal = ?
             AND source = 'grant'",
            params![case, principal],
        )?;
        Ok(())
    }

    /// Create a case (empty, chain at genesis) if missing, granting its creator.
    pub fn ensure_case(&self, case: &str, creator: &str) -> Result<(), BoardError> {
        let case = case.trim();
        if case.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        if !Self::case_exists(&conn, case)? {
            conn.execute(
                "INSERT INTO journal_meta (case_id, head_hash, created_ts) VALUES (?, ?, ?)",
                params![case, GENESIS, now_iso()],
            )?;
            Self::grant_locked(&conn, case, creator, "creator", "", None)?;
        }
        Ok(())
    }

    /// Access rides assignment: previous assignee loses this item's grant;
    /// new assignee gains it.
    pub fn sync_assignment(
        &self,
        case: &str,
        space: &str,
        item_id: i64,
        old_assignee: &str,
        new_assignee: &str,
    ) -> Result<(), BoardError> {
        if case.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        if !old_assignee.is_empty() {
            conn.execute(
                "DELETE FROM journal_grants WHERE case_id = ? AND principal = ?
                 AND source = 'assignment' AND space = ? AND item_id = ?",
                params![case, old_assignee, space, item_id],
            )?;
        }
        Self::grant_locked(
            &conn,
            case,
            new_assignee,
            "assignment",
            space,
            Some(item_id),
        )?;
        Ok(())
    }

    pub fn verify_chain(&self, case: &str) -> Result<usize, BoardError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, case_id, kind, actor, actor_role, space, item_id,
                    payload, taint, prev_hash, hash
             FROM journal_entries WHERE case_id = ? ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![case], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })?;

        let mut prev = GENESIS.to_string();
        let mut count = 0usize;
        for row in rows {
            let (
                seq,
                ts,
                case_id,
                kind,
                actor,
                actor_role,
                space,
                item_id,
                payload,
                taint,
                prev_hash,
                hash,
            ) = row?;
            if prev_hash != prev {
                return Err(BoardError::Bad(format!(
                    "entry {seq}: chain linkage broken"
                )));
            }
            let mut record: BTreeMap<&str, Value> = BTreeMap::new();
            record.insert("ts", json!(ts));
            record.insert("case_id", json!(case_id));
            record.insert("kind", json!(kind));
            record.insert("actor", json!(actor));
            record.insert("actor_role", json!(actor_role));
            record.insert(
                "space",
                space.map(|s| json!(s)).unwrap_or(Value::Null),
            );
            record.insert(
                "item_id",
                item_id.map(|i| json!(i)).unwrap_or(Value::Null),
            );
            record.insert("payload", json!(payload));
            record.insert("taint", json!(taint));
            record.insert("prev_hash", json!(prev_hash));
            if hash_record(&record) != hash {
                return Err(BoardError::Bad(format!(
                    "entry {seq}: content does not match hash"
                )));
            }
            prev = hash;
            count += 1;
        }
        if count > 0 {
            let head = Self::head_hash(&conn, case)?;
            if prev != head {
                return Err(BoardError::Bad(
                    "case log ends before the recorded head — tail deleted".into(),
                ));
            }
        }
        Ok(count)
    }

    // ------------------------------------------------------------------ internals

    fn check_access(conn: &Connection, actor: &Actor, case: &str) -> Result<(), BoardError> {
        if actor.role == Role::User {
            return Ok(());
        }
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM journal_grants WHERE case_id = ? AND principal = ? LIMIT 1",
                params![case, actor.id],
                |r| r.get(0),
            )
            .optional()?;
        if found.is_none() {
            return Err(BoardError::Authority(format!(
                "{} has no grant on case '{case}'",
                actor.id
            )));
        }
        Ok(())
    }

    fn grant_locked(
        conn: &Connection,
        case: &str,
        principal: &str,
        source: &str,
        space: &str,
        item_id: Option<i64>,
    ) -> Result<(), BoardError> {
        conn.execute(
            "INSERT OR IGNORE INTO journal_grants
             (case_id, principal, source, space, item_id) VALUES (?, ?, ?, ?, ?)",
            params![case, principal, source, space, item_id],
        )?;
        Ok(())
    }

    fn case_exists(conn: &Connection, case: &str) -> Result<bool, BoardError> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM journal_meta WHERE case_id = ?",
                params![case],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn head_hash(conn: &Connection, case: &str) -> Result<String, BoardError> {
        let row: Option<String> = conn
            .query_row(
                "SELECT head_hash FROM journal_meta WHERE case_id = ?",
                params![case],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.unwrap_or_else(|| GENESIS.to_string()))
    }
}

#[allow(clippy::too_many_arguments)]
fn row_to_entry(
    seq: i64,
    ts: &str,
    case_id: &str,
    kind: &str,
    actor: &str,
    actor_role: &str,
    persona: &str,
    model: &str,
    session_id: &str,
    space: Option<&str>,
    item_id: Option<i64>,
    payload_raw: &str,
    taint: i64,
    prev_hash: &str,
    hash: &str,
) -> Value {
    let payload: Value = serde_json::from_str(payload_raw).unwrap_or_else(|_| json!({}));
    let body = payload.get("body").cloned().unwrap_or(Value::Null);
    let entities = payload
        .get("entities")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let refs = payload.get("refs").cloned().unwrap_or_else(|| json!([]));
    json!({
        "seq": seq,
        "ts": ts,
        "case_id": case_id,
        "kind": kind,
        "author": actor,
        "role": actor_role,
        "persona": persona,
        "model": model,
        "session_id": session_id,
        "space": space,
        "item": item_id,
        "body": body,
        "entities": entities,
        "refs": refs,
        "taint": taint,
        "prev_hash": prev_hash,
        "hash": hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead() -> Actor {
        Actor::new("lead-1", Role::Lead)
    }

    fn worker() -> Actor {
        Actor::new("worker-1", Role::Worker)
    }

    fn other() -> Actor {
        Actor::new("worker-2", Role::Worker)
    }

    fn user() -> Actor {
        Actor::new("user", Role::User)
    }

    #[test]
    fn append_grants_creator_and_overview() {
        let journal = JournalStore::open_in_memory().unwrap();
        let entry = journal
            .append(
                &lead(),
                "findings",
                "case opened",
                "note",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(entry.get("case_id").and_then(|c| c.as_str()), Some("findings"));
        assert!(entry.get("seq").and_then(|s| s.as_i64()).unwrap() >= 1);

        let cases = journal.cases(&lead()).unwrap();
        assert_eq!(cases, vec!["findings".to_string()]);
        assert!(journal.cases(&other()).unwrap().is_empty());

        let overview = journal.overview(&lead()).unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(
            overview[0].get("case").and_then(|c| c.as_str()),
            Some("findings")
        );
        assert_eq!(overview[0].get("entries").and_then(|e| e.as_i64()), Some(1));

        let user_overview = journal.overview(&user()).unwrap();
        assert_eq!(user_overview.len(), 1);

        let err = journal
            .read(
                &other(),
                "findings",
                None,
                None,
                None,
                None,
                0,
                false,
                100,
            )
            .unwrap_err();
        assert!(matches!(err, BoardError::Authority(_)));
    }

    #[test]
    fn assignment_grant_feeds_access() {
        let journal = JournalStore::open_in_memory().unwrap();
        journal.ensure_case("findings", "lead-1").unwrap();
        journal
            .sync_assignment("findings", "proj", 1, "", "worker-1")
            .unwrap();

        journal
            .append(
                &worker(),
                "findings",
                "assignee writes",
                "note",
                Some("proj"),
                Some(1),
                None,
                None,
            )
            .unwrap();

        assert!(journal
            .read(
                &other(),
                "findings",
                None,
                None,
                None,
                None,
                0,
                false,
                100,
            )
            .is_err());

        journal
            .sync_assignment("findings", "proj", 1, "worker-1", "worker-2")
            .unwrap();
        journal
            .append(
                &other(),
                "findings",
                "successor picks up",
                "note",
                Some("proj"),
                Some(1),
                None,
                None,
            )
            .unwrap();
        assert!(journal
            .append(
                &worker(),
                "findings",
                "predecessor lost access",
                "note",
                None,
                None,
                None,
                None,
            )
            .is_err());

        let overview = journal.overview(&other()).unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(
            overview[0].get("entries").and_then(|e| e.as_i64()),
            Some(2)
        );
    }

    #[test]
    fn hash_chain_verifies() {
        let journal = JournalStore::open_in_memory().unwrap();
        journal
            .append(&lead(), "findings", "one", "note", None, None, None, None)
            .unwrap();
        journal
            .append(&lead(), "findings", "two", "note", None, None, None, None)
            .unwrap();
        assert_eq!(journal.verify_chain("findings").unwrap(), 2);
    }
}
