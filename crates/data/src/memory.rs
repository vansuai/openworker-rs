//! SQLite-backed memory store — Rust reimplementation of `coworker/memory/sqlite_store.py`.

use crate::types::{MemoryItem, Scope};
use crate::Error;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// In-memory store for tests / ephemeral use.
pub struct MemoryStore {
    items: Mutex<Vec<MemoryItem>>,
    next_id: Mutex<i64>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        }
    }

    pub fn add(
        &self,
        content: &str,
        scope: Scope,
        key: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> MemoryItem {
        let mut items = self.items.lock();
        let mut next_id = self.next_id.lock();
        let id = *next_id;
        *next_id += 1;
        let created_at = chrono::Utc::now().to_rfc3339();
        let item = MemoryItem {
            id,
            scope,
            content: content.to_string(),
            key: key.map(String::from),
            workspace: workspace.map(String::from),
            session_id: session_id.map(String::from),
            created_at: Some(created_at),
        };
        items.push(item.clone());
        item
    }

    pub fn get(&self, item_id: i64) -> Option<MemoryItem> {
        let items = self.items.lock();
        items.iter().find(|i| i.id == item_id).cloned()
    }

    pub fn list(
        &self,
        scope: Option<Scope>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> Vec<MemoryItem> {
        let items = self.items.lock();
        items
            .iter()
            .filter(|i| {
                scope.map_or(true, |s| i.scope == s)
                    && workspace.map_or(true, |w| i.workspace.as_deref() == Some(w))
                    && session_id.map_or(true, |s| i.session_id.as_deref() == Some(s))
            })
            .cloned()
            .collect()
    }

    pub fn update(&self, item_id: i64, content: &str) -> Option<MemoryItem> {
        let mut items = self.items.lock();
        if let Some(item) = items.iter_mut().find(|i| i.id == item_id) {
            item.content = content.to_string();
            return Some(item.clone());
        }
        None
    }

    pub fn delete(&self, item_id: i64) -> bool {
        let mut items = self.items.lock();
        let len_before = items.len();
        items.retain(|i| i.id != item_id);
        items.len() < len_before
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// SQLite-backed persistent memory store.
pub struct SQLiteMemoryStore {
    conn: Mutex<Connection>,
}

impl SQLiteMemoryStore {
    /// Open (or create) a memory store at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL,
                key TEXT,
                content TEXT NOT NULL,
                workspace TEXT,
                session_id TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            )",
            [],
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a new memory and return the full item.
    pub fn add(
        &self,
        content: &str,
        scope: Scope,
        key: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<MemoryItem, Error> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO memories (scope, key, content, workspace, session_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![scope.as_str(), key, content, workspace, session_id],
        )?;

        let row = conn
            .query_row(
                "SELECT id, scope, key, content, workspace, session_id, created_at FROM memories ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                let scope_str: String = row.get(1)?;
                let scope = Scope::try_from(scope_str).unwrap_or(Scope::Workspace);
                Ok(MemoryItem {
                    id: row.get(0)?,
                    scope,
                    content: row.get(3)?,
                    key: row.get(2)?,
                    workspace: row.get(4)?,
                    session_id: row.get(5)?,
                    created_at: row.get(6)?,
                })
            },
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;

        Ok(row)
    }

    /// Fetch a memory by id, or None.
    pub fn get(&self, item_id: i64) -> Result<Option<MemoryItem>, Error> {
        let conn = self.conn.lock();
        let result = conn
            .query_row(
                "SELECT id, scope, key, content, workspace, session_id, created_at FROM memories WHERE id = ?",
                [item_id],
                |row| {
                let scope_str: String = row.get(1)?;
                let scope = Scope::try_from(scope_str).unwrap_or(Scope::Workspace);
                Ok(MemoryItem {
                    id: row.get(0)?,
                    scope,
                    content: row.get(3)?,
                    key: row.get(2)?,
                    workspace: row.get(4)?,
                    session_id: row.get(5)?,
                    created_at: row.get(6)?,
                })
            },
            )
            .optional();

        match result {
            Ok(item) => Ok(item),
            Err(e) => Err(Error::Sqlite(e.to_string())),
        }
    }

    /// List memories filtered by scope/workspace/session_id.
    pub fn list(
        &self,
        scope: Option<Scope>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryItem>, Error> {
        let conn = self.conn.lock();
        let mut sql = "SELECT id, scope, key, content, workspace, session_id, created_at FROM memories WHERE 1=1".to_string();
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(s) = &scope {
            sql.push_str(" AND scope = ?");
            params_vec.push(Box::new(s.as_str().to_string()));
        }
        if let Some(w) = workspace {
            sql.push_str(" AND workspace = ?");
            params_vec.push(Box::new(w.to_string()));
        }
        if let Some(sid) = session_id {
            sql.push_str(" AND session_id = ?");
            params_vec.push(Box::new(sid.to_string()));
        }
        sql.push_str(" ORDER BY id");

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let scope_str: String = row.get(1)?;
            let scope = Scope::try_from(scope_str).unwrap_or(Scope::Workspace);
            Ok(MemoryItem {
                id: row.get(0)?,
                scope,
                content: row.get(3)?,
                key: row.get(2)?,
                workspace: row.get(4)?,
                session_id: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(|e| Error::Sqlite(e.to_string()))?);
        }
        Ok(items)
    }

    /// Update a memory's content by id. Returns the updated item.
    pub fn update(&self, item_id: i64, content: &str) -> Result<Option<MemoryItem>, Error> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE memories SET content = ? WHERE id = ?",
            params![content, item_id],
        )?;
        drop(conn);
        self.get(item_id)
    }

    /// Delete a memory by id. Returns true if a row was deleted.
    pub fn delete(&self, item_id: i64) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM memories WHERE id = ?", [item_id])?;
        Ok(n > 0)
    }
}

/// Render memories for injection into the system prompt.
pub fn format_memories(items: &[MemoryItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = items
        .iter()
        .map(|item| format!("- [#{id}] {content}", id = item.id, content = item.content))
        .collect();
    format!(
        "Known memories (from earlier sessions):\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn memory_store_in_memory() {
        let store = MemoryStore::new();
        let item = store.add("hello world", Scope::Workspace, None, Some("/tmp"), None);
        assert_eq!(item.content, "hello world");
        assert_eq!(store.list(None, Some("/tmp"), None).len(), 1);
        assert!(store.delete(item.id));
        assert!(!store.delete(item.id));
    }

    #[test]
    fn memory_store_sqlite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memories.db");
        let store = SQLiteMemoryStore::open(&path).unwrap();

        let item = store
            .add("test memory", Scope::Global, Some("key1"), None, None)
            .unwrap();
        assert_eq!(item.content, "test memory");

        let found = store.get(item.id).unwrap().unwrap();
        assert_eq!(found.content, "test memory");

        let updated = store.update(item.id, "updated").unwrap().unwrap();
        assert_eq!(updated.content, "updated");

        let listed = store.list(Some(Scope::Global), None, None).unwrap();
        assert_eq!(listed.len(), 1);
        // Verify content and key are correctly returned (P0-4 regression test)
        assert_eq!(listed[0].content, "updated");
        assert_eq!(listed[0].key, Some("key1".to_string()));

        assert!(store.delete(item.id).unwrap());
        assert!(store.get(item.id).unwrap().is_none());
    }

    #[test]
    fn format_memories() {
        let store = MemoryStore::new();
        let a = store.add("a", Scope::Workspace, None, None, None);
        let b = store.add("b", Scope::Global, None, None, None);
        let text = super::format_memories(&[a, b]);
        assert!(text.contains("#1] a"));
        assert!(text.contains("#2] b"));
    }
}
