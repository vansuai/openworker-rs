//! Team chat store — Rust port of `coworker/teams/chat.py`.
//!
//! A GROUP is `{group_id, name, members[]}` plus an append-only message log and
//! per-member unread cursors. Wake semantics: agent posts wake @mentioned
//! members; user posts wake every member.

use crate::teams::BoardError;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMember {
    pub name: String,
    #[serde(default)]
    pub persona: String,
    #[serde(default = "default_worker_role")]
    pub role: String,
}

fn default_worker_role() -> String {
    "worker".into()
}

pub struct ChatStore {
    conn: Mutex<Connection>,
}

impl ChatStore {
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
            CREATE TABLE IF NOT EXISTS chat_groups (
                group_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                members TEXT NOT NULL,
                created_ts TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS chat_messages (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                group_id TEXT NOT NULL,
                ts TEXT NOT NULL,
                author TEXT NOT NULL,
                author_role TEXT NOT NULL,
                text TEXT NOT NULL,
                mentions TEXT NOT NULL DEFAULT '[]'
            );
            CREATE INDEX IF NOT EXISTS idx_chat_group ON chat_messages (group_id, seq);
            CREATE TABLE IF NOT EXISTS chat_cursors (
                cursor_key TEXT PRIMARY KEY,
                read_seq INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn create_group(
        &self,
        name: &str,
        members: Vec<ChatMember>,
    ) -> Result<Value, BoardError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(BoardError::Bad("group name is required".into()));
        }
        let handles: Vec<&str> = members.iter().map(|m| m.name.trim()).collect();
        if handles.iter().any(|h| h.is_empty())
            || handles.iter().collect::<BTreeSet<_>>().len() != handles.len()
        {
            return Err(BoardError::Bad("every member needs a unique name".into()));
        }
        let members: Vec<ChatMember> = members
            .into_iter()
            .map(|m| ChatMember {
                name: m.name.trim().to_string(),
                persona: m.persona,
                role: if m.role.is_empty() {
                    "worker".into()
                } else {
                    m.role
                },
            })
            .collect();
        let group_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let created_ts = chrono::Utc::now().to_rfc3339();
        let members_json = serde_json::to_string(&members)?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO chat_groups (group_id, name, members, created_ts) VALUES (?, ?, ?, ?)",
            params![group_id, name, members_json, created_ts],
        )?;
        Ok(json!({
            "group_id": group_id,
            "name": name,
            "members": members,
            "created_ts": created_ts,
        }))
    }

    pub fn get_group(&self, group_id: &str) -> Option<Value> {
        let conn = self.conn.lock();
        let row: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT group_id, name, members, created_ts FROM chat_groups WHERE group_id = ?",
                params![group_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (group_id, name, members_raw, created_ts) = row?;
        let members: Value = serde_json::from_str(&members_raw).unwrap_or(json!([]));
        Some(json!({
            "group_id": group_id,
            "name": name,
            "members": members,
            "created_ts": created_ts,
        }))
    }

    pub fn post(
        &self,
        group_id: &str,
        author: &str,
        text: &str,
        author_role: &str,
    ) -> Result<Value, BoardError> {
        let group = self
            .get_group(group_id)
            .ok_or_else(|| BoardError::Bad(format!("no chat group '{group_id}'")))?;
        let text = text.trim();
        if text.is_empty() {
            return Err(BoardError::Bad("message text is required".into()));
        }
        let handles: BTreeSet<String> = group
            .get("members")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mentions = parse_mentions(text, &handles);
        let ts = chrono::Utc::now().to_rfc3339();
        let mentions_json = serde_json::to_string(&mentions)?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO chat_messages (group_id, ts, author, author_role, text, mentions)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![group_id, ts, author, author_role, text, mentions_json],
        )?;
        let seq = conn.last_insert_rowid();
        Ok(json!({
            "group_id": group_id,
            "seq": seq,
            "ts": ts,
            "author": author,
            "author_role": author_role,
            "text": text,
            "mentions": mentions,
        }))
    }

    pub fn messages(&self, group_id: &str, since_seq: i64, limit: i64) -> Vec<Value> {
        let cap = limit.clamp(1, 2000);
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT seq, group_id, ts, author, author_role, text, mentions
             FROM chat_messages WHERE group_id = ? AND seq > ?
             ORDER BY seq LIMIT ?",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(params![group_id, since_seq, cap], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .ok();
        let Some(rows) = rows else {
            return Vec::new();
        };
        rows.filter_map(|r| r.ok())
            .map(|(seq, group_id, ts, author, author_role, text, mentions_raw)| {
                let mentions: Value =
                    serde_json::from_str(&mentions_raw).unwrap_or(json!([]));
                json!({
                    "seq": seq,
                    "group_id": group_id,
                    "ts": ts,
                    "author": author,
                    "author_role": author_role,
                    "text": text,
                    "mentions": mentions,
                })
            })
            .collect()
    }

    pub fn unread_for(&self, group_id: &str, member: &str) -> Vec<Value> {
        let since = self.cursor(group_id, member);
        self.messages(group_id, since, 200)
            .into_iter()
            .filter(|m| {
                let author = m.get("author").and_then(|a| a.as_str()).unwrap_or("");
                if author == member {
                    return false;
                }
                let role = m.get("author_role").and_then(|r| r.as_str()).unwrap_or("");
                if role == "user" {
                    return true;
                }
                m.get("mentions")
                    .and_then(|ms| ms.as_array())
                    .map(|arr| arr.iter().any(|v| v.as_str() == Some(member)))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn unread_count(&self, group_id: &str, member: &str) -> i64 {
        let since = self.cursor(group_id, member);
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM chat_messages
             WHERE group_id = ? AND seq > ? AND author != ?",
            params![group_id, since, member],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    pub fn consume(&self, group_id: &str, member: &str, upto_seq: i64) {
        let key = format!("{group_id}:{member}");
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT INTO chat_cursors (cursor_key, read_seq) VALUES (?, ?)
             ON CONFLICT(cursor_key) DO UPDATE SET read_seq = MAX(read_seq, excluded.read_seq)",
            params![key, upto_seq],
        );
    }

    fn cursor(&self, group_id: &str, member: &str) -> i64 {
        let key = format!("{group_id}:{member}");
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT read_seq FROM chat_cursors WHERE cursor_key = ?",
            params![key],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }
}

/// Parse `@handle` tokens against known member handles (ASCII word chars + `.-_`).
fn parse_mentions(text: &str, handles: &BTreeSet<String>) -> Vec<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() {
                let c = bytes[end];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-' {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > start {
                let name = &text[start..end];
                if handles.contains(name) {
                    found.insert(name.to_string());
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_parses_mentions_and_unread_wakes() {
        let store = ChatStore::open_in_memory().unwrap();
        let group = store
            .create_group(
                "team chat",
                vec![
                    ChatMember {
                        name: "nia".into(),
                        persona: "swe-worker".into(),
                        role: "worker".into(),
                    },
                    ChatMember {
                        name: "lead".into(),
                        persona: "swe-lead".into(),
                        role: "lead".into(),
                    },
                ],
            )
            .unwrap();
        let gid = group["group_id"].as_str().unwrap();
        let msg = store
            .post(gid, "lead", "hey @nia look", "lead")
            .unwrap();
        assert_eq!(msg["mentions"], json!(["nia"]));
        let unread = store.unread_for(gid, "nia");
        assert_eq!(unread.len(), 1);
        assert!(store.unread_for(gid, "lead").is_empty());
        store.consume(gid, "nia", msg["seq"].as_i64().unwrap());
        assert!(store.unread_for(gid, "nia").is_empty());
    }
}
