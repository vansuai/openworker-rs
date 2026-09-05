//! Conversation/Session store — Rust reimplementation of `coworker/conversations.py`.
//!
//! Storage layout under base_dir (default `~/.config/coworker/`):
//!   coworker.db              SQLite index
//!   conversations/<id>.jsonl append-only message log, one file per session
//!
//! Writes append only the new messages each turn (no rewriting history).
//! Legacy rows that stored messages inline are lazily migrated on first load/save.

use crate::types::{SessionRecord, SessionSummary};
use crate::Error;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Resolve the platform-appropriate config directory.
pub fn default_base_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("coworker"))
}

/// The conversation store manages session metadata + append-only message logs.
pub struct ConversationStore {
    #[allow(dead_code)]
    base: PathBuf,
    conv_dir: PathBuf,
    conn: Mutex<Connection>,
}

impl ConversationStore {
    /// Open (or create) a conversation store at `base_dir`.
    pub fn open(base_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let base = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&base)?;
        let conv_dir = base.join("conversations");
        fs::create_dir_all(&conv_dir)?;

        let db_path = base.join("coworker.db");
        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                workspace TEXT,
                model TEXT,
                mode TEXT,
                title TEXT,
                agent TEXT NOT NULL DEFAULT 'code',
                n_msgs INTEGER NOT NULL DEFAULT 0,
                messages TEXT,
                extra_roots TEXT,
                grants TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                origin TEXT,
                origin_label TEXT,
                auto_title TEXT,
                renamed INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS workspaces (
                path TEXT PRIMARY KEY,
                last_used TEXT DEFAULT (datetime('now'))
            );
            "#,
        )?;

        // Add columns that may not exist in legacy databases (no-op if already present)
        let migrations = [
            "ALTER TABLE sessions ADD COLUMN title TEXT",
            "ALTER TABLE sessions ADD COLUMN n_msgs INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN agent TEXT DEFAULT 'code'",
            "ALTER TABLE sessions ADD COLUMN extra_roots TEXT",
            "ALTER TABLE sessions ADD COLUMN pinned INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN archived INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN origin TEXT",
            "ALTER TABLE sessions ADD COLUMN origin_label TEXT",
            "ALTER TABLE sessions ADD COLUMN auto_title TEXT",
            "ALTER TABLE sessions ADD COLUMN renamed INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN grants TEXT",
            "ALTER TABLE sessions ADD COLUMN compaction TEXT",
        ];
        for ddl in &migrations {
            let _ = conn.execute(ddl, []);
        }

        Ok(Self {
            base,
            conv_dir,
            conn: Mutex::new(conn),
        })
    }

    /// Path to the JSONL file for a session — single chokepoint that rejects
    /// path-traversal session ids (mirrors Python `_file` / `is_safe_session_id`).
    fn jsonl_path(&self, session_id: &str) -> Result<PathBuf, Error> {
        if !is_safe_session_id(session_id) {
            return Err(Error::Invalid(format!("unsafe session id: {session_id:?}")));
        }
        let path = self.conv_dir.join(format!("{session_id}.jsonl"));
        let canon_dir = self
            .conv_dir
            .canonicalize()
            .unwrap_or_else(|_| self.conv_dir.clone());
        // Before the file exists, join + normalize without requiring canonicalize of the file.
        let parent = path
            .parent()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
            .unwrap_or_else(|| canon_dir.clone());
        if parent != canon_dir {
            return Err(Error::Invalid(format!("unsafe session id: {session_id:?}")));
        }
        Ok(path)
    }

    /// Read all messages from a session's JSONL file (public so the server layer can fall back
    /// to disk when its in-memory cache is empty after a restart).
    pub fn read_jsonl(&self, session_id: &str) -> Vec<serde_json::Value> {
        let path = match self.jsonl_path(session_id) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        if !path.exists() {
            return Vec::new();
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let reader = BufReader::new(file);
        reader
            .lines()
            .filter_map(|l| {
                let l = l.ok()?;
                let l = l.trim();
                if l.is_empty() {
                    return None;
                }
                serde_json::from_str(l).ok()
            })
            .collect()
    }

    /// Count lines in a session's JSONL file (public so the server layer can diff
    /// engine messages against disk for incremental persistence).
    pub fn count_jsonl(&self, session_id: &str) -> usize {
        let path = match self.jsonl_path(session_id) {
            Ok(p) => p,
            Err(_) => return 0,
        };
        if !path.exists() {
            return 0;
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return 0,
        };
        let reader = BufReader::new(file);
        reader
            .lines()
            .filter(|l| l.as_ref().is_ok_and(|s| !s.trim().is_empty()))
            .count()
    }

    /// Append messages to a session's JSONL file (public for server push_message).
    pub fn append_jsonl(
        &self,
        session_id: &str,
        messages: &[serde_json::Value],
    ) -> Result<(), Error> {
        let path = self.jsonl_path(session_id)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for msg in messages {
            let line = serde_json::to_string(msg).map_err(|e| Error::Json(e.to_string()))?;
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    /// Atomically rewrite the JSONL file (tmp + rename) so a mid-write crash
    /// cannot leave a truncated history (mirrors Python `_rewrite`).
    pub fn rewrite_jsonl(&self, session_id: &str, messages: &[serde_json::Value]) -> Result<(), Error> {
        let path = self.jsonl_path(session_id)?;
        let tmp = path.with_extension("jsonl.tmp");
        {
            let file = fs::File::create(&tmp)?;
            let mut writer = std::io::BufWriter::new(file);
            for msg in messages {
                let line = serde_json::to_string(msg).map_err(|e| Error::Json(e.to_string()))?;
                writeln!(writer, "{}", line)?;
            }
            writer.flush()?;
        }
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Error::Io(e.to_string())
        })?;
        Ok(())
    }

    /// Derive a title from the first user message's first line.
    pub fn title_from_messages(messages: &[serde_json::Value]) -> String {
        for msg in messages {
            if msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                if let Some(content) = msg.get("content") {
                    let text = extract_text(content);
                    let first_line = text.trim().split('\n').next().unwrap_or("");
                    if !first_line.is_empty() {
                        return first_line.chars().take(60).collect();
                    }
                }
            }
        }
        "New session".to_string()
    }

    /// Save a session record. Append-only for messages; updates metadata in SQLite.
    pub fn save(&self, record: &SessionRecord) -> Result<(), Error> {
        // Lazily migrate legacy inline blob into .jsonl
        let jsonl_path = self.jsonl_path(&record.session_id)?;
        if !jsonl_path.exists() {
            let messages_json: Option<String> = {
                let conn = self.conn.lock();
                // `messages` is NULL for rows written by the summary upsert below
                // (it omits the column) — read as Option<String> so NULL doesn't
                // abort the whole `save` with "Invalid column type Null".
                let res: Option<Option<String>> = conn
                    .query_row(
                        "SELECT messages FROM sessions WHERE session_id = ?",
                        [&record.session_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(|e| Error::Sqlite(e.to_string()))?;
                res.flatten()
            };
            if let Some(json_str) = messages_json {
                if !json_str.is_empty() {
                    if let Ok(legacy) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
                        if !legacy.is_empty() {
                            self.append_jsonl(&record.session_id, &legacy)?;
                            let conn = self.conn.lock();
                            conn.execute(
                                "UPDATE sessions SET messages = NULL WHERE session_id = ?",
                                [&record.session_id],
                            )
                            .ok();
                        }
                    }
                }
            }
        }

        let existing_count = self.count_jsonl(&record.session_id);
        let new_count = record.messages.len();

        if new_count > existing_count {
            self.append_jsonl(&record.session_id, &record.messages[existing_count..])?;
            let conn = self.conn.lock();
            conn.execute(
                "UPDATE sessions SET messages = NULL WHERE session_id = ?",
                [&record.session_id],
            )
            .ok();
        } else if new_count < existing_count {
            // Rare: history shrank — rewrite
            self.rewrite_jsonl(&record.session_id, &record.messages)?;
            let conn = self.conn.lock();
            conn.execute("COMMIT", []).ok();
        }

        // Upsert the summary row unconditionally (mirror of
        // `conversations.py::save`, which always INSERT ... ON CONFLICT DO
        // UPDATE). A brand-new session with zero messages must still land in
        // the table so `load`/`list` see it.
        let title = record
            .title
            .as_ref()
            .or(record.auto_title.as_ref())
            .cloned()
            .unwrap_or_else(|| Self::title_from_messages(&record.messages));
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO sessions (session_id, workspace, model, mode, title, agent, n_msgs, extra_roots, grants, compaction, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, datetime('now'))",
            params![
                record.session_id,
                record.workspace,
                record.model,
                record.mode,
                title,
                record.agent,
                new_count,
                serde_json::to_string(&record.extra_roots).unwrap_or_default(),
                serde_json::to_string(&record.grants).unwrap_or_default(),
                record
                    .compaction
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
                    .unwrap_or_else(|| "{}".into()),
            ],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        drop(conn);

        self.touch_workspace(&record.workspace)?;
        Ok(())
    }

    /// Load a full session record including messages.
    pub fn load(&self, session_id: &str) -> Result<Option<SessionRecord>, Error> {
        let conn = self.conn.lock();
        let row: Option<SessionRow> = conn
            .query_row(
                "SELECT * FROM sessions WHERE session_id = ?",
                [session_id],
                |row| {
                    Ok(SessionRow {
                        session_id: row.get("session_id")?,
                        workspace: row.get("workspace")?,
                        model: row.get("model")?,
                        mode: row.get("mode")?,
                        title: row.get("title")?,
                        agent: row.get("agent")?,
                        n_msgs: row.get::<_, Option<i64>>("n_msgs")?.unwrap_or(0),
                        messages: row.get("messages")?,
                        extra_roots: row.get("extra_roots")?,
                        pinned: row.get::<_, Option<i64>>("pinned")?.unwrap_or(0) != 0,
                        archived: row.get::<_, Option<i64>>("archived")?.unwrap_or(0) != 0,
                        origin: row.get("origin")?,
                        origin_label: row.get("origin_label")?,
                        auto_title: row.get("auto_title")?,
                        renamed: row.get::<_, Option<i64>>("renamed")?.unwrap_or(0) != 0,
                        updated_at: row.get("updated_at")?,
                        grants: row.get("grants")?,
                        compaction: row.get("compaction").ok().flatten(),
                    })
                },
            )
            .optional()
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        drop(conn);

        let Some(row) = row else {
            return Ok(None);
        };

        let messages = repair_tool_pairing(self.read_jsonl(session_id));
        let (extra_roots, grants) = (
            parse_json_opt(&row.extra_roots).unwrap_or_default(),
            parse_json_opt(&row.grants).unwrap_or(serde_json::json!({})),
        );
        let compaction = parse_json_opt::<serde_json::Value>(&row.compaction).and_then(|v| {
            // Treat empty object / null as "never compacted" → None
            match &v {
                serde_json::Value::Object(m) if m.is_empty() => None,
                serde_json::Value::Null => None,
                other => Some(other.clone()),
            }
        });

        let display_title = if row.renamed {
            row.title.clone()
        } else {
            row.auto_title.clone().or(row.title.clone())
        };

        Ok(Some(SessionRecord {
            session_id: row.session_id,
            workspace: row.workspace,
            model: row.model,
            mode: row.mode,
            messages: messages.clone(),
            title: display_title,
            agent: row.agent.unwrap_or_else(|| "code".to_string()),
            message_count: if messages.is_empty() {
                row.n_msgs as _
            } else {
                messages.len() as _
            },
            updated_at: row.updated_at,
            extra_roots,
            grants,
            compaction,
            pinned: row.pinned,
            archived: row.archived,
            origin: row.origin,
            origin_label: row.origin_label,
            auto_title: row.auto_title,
            renamed: row.renamed,
        }))
    }

    /// List all sessions (summary only, no messages), optionally filtered by workspace.
    pub fn list(&self, workspace: Option<&str>) -> Result<Vec<SessionSummary>, Error> {
        let conn = self.conn.lock();
        let rows: Vec<SessionRow> = if let Some(ws) = workspace {
            let mut stmt = conn.prepare(
                "SELECT * FROM sessions WHERE workspace = ? ORDER BY pinned DESC, updated_at DESC",
            )?;
            let result = stmt
                .query_map([ws], Self::map_row)?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            result
        } else {
            let mut stmt =
                conn.prepare("SELECT * FROM sessions ORDER BY pinned DESC, updated_at DESC")?;
            let result = stmt
                .query_map([], Self::map_row)?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            result
        };
        drop(conn);

        Ok(rows
            .into_iter()
            .map(|row| {
                let display_title = if row.renamed {
                    row.title.clone()
                } else {
                    row.auto_title.clone().or(row.title.clone())
                };
                SessionSummary {
                    session_id: row.session_id,
                    workspace: row.workspace,
                    model: row.model,
                    mode: row.mode,
                    title: display_title,
                    agent: row.agent.unwrap_or_else(|| "code".to_string()),
                    message_count: row.n_msgs,
                    updated_at: row.updated_at,
                    pinned: row.pinned,
                    archived: row.archived,
                    origin: row.origin,
                    origin_label: row.origin_label,
                    auto_title: row.auto_title,
                    renamed: row.renamed,
                }
            })
            .collect())
    }

    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            session_id: row.get("session_id")?,
            workspace: row.get("workspace")?,
            model: row.get("model")?,
            mode: row.get("mode")?,
            title: row.get("title")?,
            agent: row.get("agent")?,
            n_msgs: row.get::<_, Option<i64>>("n_msgs")?.unwrap_or(0),
            messages: row.get("messages")?,
            extra_roots: row.get("extra_roots")?,
            pinned: row.get::<_, Option<i64>>("pinned")?.unwrap_or(0) != 0,
            archived: row.get::<_, Option<i64>>("archived")?.unwrap_or(0) != 0,
            origin: row.get("origin")?,
            origin_label: row.get("origin_label")?,
            auto_title: row.get("auto_title")?,
            renamed: row.get::<_, Option<i64>>("renamed")?.unwrap_or(0) != 0,
            updated_at: row.get("updated_at")?,
            grants: row.get("grants")?,
            compaction: row.get("compaction").unwrap_or(None),
        })
    }

    /// Update extra_roots for a session.
    pub fn set_extra_roots(
        &self,
        session_id: &str,
        extra_roots: Vec<serde_json::Value>,
    ) -> Result<(), Error> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET extra_roots = ?, updated_at = datetime('now') WHERE session_id = ?",
            params![
                serde_json::to_string(&extra_roots).unwrap_or_default(),
                session_id
            ],
        )?;
        Ok(())
    }

    /// Update pin/archive flags without touching updated_at.
    pub fn set_flags(
        &self,
        session_id: &str,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let mut sets = Vec::new();
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = pinned {
            sets.push("pinned = ?");
            params_vec.push(Box::new(if p { 1i64 } else { 0i64 }));
        }
        if let Some(a) = archived {
            sets.push("archived = ?");
            params_vec.push(Box::new(if a { 1i64 } else { 0i64 }));
        }
        if sets.is_empty() {
            return Ok(false);
        }
        params_vec.push(Box::new(session_id.to_string()));
        let sql = format!(
            "UPDATE sessions SET {} WHERE session_id = ?",
            sets.join(", ")
        );
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let n = conn.execute(&sql, params_refs.as_slice())?;
        Ok(n > 0)
    }

    /// The auto-title guard inputs (mirror of Python's `title_state`): whether the user
    /// renamed the session and whether a generated title already exists. None when the
    /// session has no row yet.
    pub fn title_state(&self, session_id: &str) -> Result<Option<(bool, Option<String>)>, Error> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT renamed, auto_title FROM sessions WHERE session_id = ?",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>("renamed")?.unwrap_or(0) != 0,
                    row.get::<_, Option<String>>("auto_title")?,
                ))
            },
        )
        .optional()
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    /// Per-turn snapshot update (mirror of `manager.py`'s post-turn save): refresh the
    /// first-line title, message count, and recency. A targeted UPDATE — the whole-row
    /// replace in `save()` would clobber auto_title/renamed. `title = None` (a manual
    /// rename is in place) leaves the title column untouched.
    pub fn update_turn_meta(
        &self,
        session_id: &str,
        title: Option<&str>,
        n_msgs: i64,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = match title {
            Some(t) => conn.execute(
                "UPDATE sessions SET title = ?1, n_msgs = ?2, updated_at = datetime('now') WHERE session_id = ?3",
                params![t, n_msgs, session_id],
            )?,
            None => conn.execute(
                "UPDATE sessions SET n_msgs = ?1, updated_at = datetime('now') WHERE session_id = ?2",
                params![n_msgs, session_id],
            )?,
        };
        Ok(n > 0)
    }

    /// Update the grants JSON blob for a session (called after each turn so
    /// approved tools survive a restart).
    pub fn update_grants(
        &self,
        session_id: &str,
        grants: &serde_json::Value,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE sessions SET grants = ?1 WHERE session_id = ?2",
            params![serde_json::to_string(grants).unwrap_or_default(), session_id],
        )?;
        Ok(n > 0)
    }

    /// Persist auto-compaction state (OPE-27). `None` clears the column.
    pub fn update_compaction(
        &self,
        session_id: &str,
        compaction: Option<&serde_json::Value>,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let blob = compaction
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let n = conn.execute(
            "UPDATE sessions SET compaction = ?1, updated_at = datetime('now') WHERE session_id = ?2",
            params![blob, session_id],
        )?;
        Ok(n > 0)
    }

    /// Rename a session (sets renamed=1 so auto-titleing skips it).
    pub fn rename(&self, session_id: &str, title: &str) -> Result<bool, Error> {
        // Collapse whitespace but keep single spaces — mirror of Python's `" ".join(split())`.
        let clean: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return Ok(false);
        }
        let clean: String = clean.chars().take(120).collect();
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE sessions SET title = ?, renamed = 1, updated_at = datetime('now') WHERE session_id = ?",
            params![clean, session_id],
        )?;
        Ok(n > 0)
    }

    /// Set auto-generated title (never overwrites a manual rename).
    pub fn set_auto_title(&self, session_id: &str, title: &str) -> Result<bool, Error> {
        let clean: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return Ok(false);
        }
        let clean: String = clean.chars().take(60).collect();
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE sessions SET auto_title = ? WHERE session_id = ? AND renamed = 0",
            params![clean, session_id],
        )?;
        Ok(n > 0)
    }

    /// Mark session origin (spawned from a connector, etc.).
    pub fn set_origin(
        &self,
        session_id: &str,
        origin: &str,
        origin_label: &str,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE sessions SET origin = ?, origin_label = ? WHERE session_id = ?",
            params![
                origin,
                if origin_label.is_empty() {
                    None
                } else {
                    Some(origin_label)
                },
                session_id
            ],
        )?;
        Ok(n > 0)
    }

    /// Update the model and mode for a session.
    pub fn update_model_and_mode(
        &self,
        session_id: &str,
        model: &str,
        mode: &str,
    ) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE sessions SET model = ?, mode = ?, updated_at = datetime('now') WHERE session_id = ?",
            params![model, mode, session_id],
        )?;
        Ok(n > 0)
    }

    /// Delete a session and its message log.
    pub fn delete(&self, session_id: &str) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM sessions WHERE session_id = ?", [session_id])?;
        drop(conn);
        if let Ok(path) = self.jsonl_path(session_id) {
            if path.exists() {
                fs::remove_file(path).map_err(|e| Error::Io(e.to_string()))?;
            }
        }
        Ok(n > 0)
    }

    /// Record workspace access time.
    pub fn touch_workspace(&self, path: &str) -> Result<(), Error> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO workspaces (path, last_used) VALUES (?, datetime('now')) ON CONFLICT(path) DO UPDATE SET last_used = datetime('now')",
            [path],
        )?;
        Ok(())
    }

    /// List recently-used workspaces.
    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<String>, Error> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT path FROM workspaces ORDER BY last_used DESC LIMIT ?")?;
        let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for path in rows.flatten() {
            result.push(path);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helper types
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SessionRow {
    session_id: String,
    workspace: String,
    model: String,
    mode: String,
    title: Option<String>,
    agent: Option<String>,
    n_msgs: i64,
    #[allow(dead_code)]
    messages: Option<String>,
    extra_roots: Option<String>,
    pinned: bool,
    archived: bool,
    origin: Option<String>,
    origin_label: Option<String>,
    auto_title: Option<String>,
    renamed: bool,
    updated_at: Option<String>,
    grants: Option<String>,
    compaction: Option<String>,
}

fn parse_json_opt<T: for<'de> serde::Deserialize<'de>>(raw: &Option<String>) -> Option<T> {
    let s = raw.as_ref()?;
    if s.is_empty() {
        return None;
    }
    serde_json::from_str(s).ok()
}

/// Session ids must be a single safe path component — reject traversal (`../`, `/`, `\`).
pub fn is_safe_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 128
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Reorder messages so every tool result immediately follows its call.
/// Trailing pending tool_calls (assistant is last message) are left alone for durable resume.
pub fn repair_tool_pairing(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if messages.is_empty() {
        return messages;
    }

    let mut pending_calls: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        pending_calls.insert(id.to_string(), i);
                    }
                }
            }
        }
    }
    if pending_calls.is_empty() {
        return messages;
    }

    let mut found_results: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        if let Some(id) = m.get("tool_call_id").and_then(|v| v.as_str()) {
            if pending_calls.contains_key(id) {
                found_results.entry(id.to_string()).or_insert(i);
            }
        }
    }

    let last_msg_idx = messages.len() - 1;
    let trailing_calls: std::collections::HashSet<String> = pending_calls
        .iter()
        .filter(|(_, &idx)| idx == last_msg_idx)
        .map(|(id, _)| id.clone())
        .collect();

    let mut needs_repair = false;
    for (call_id, &call_idx) in &pending_calls {
        if trailing_calls.contains(call_id) && !found_results.contains_key(call_id) {
            continue;
        }
        match found_results.get(call_id) {
            Some(&result_idx) if result_idx == call_idx + 1 => {}
            _ => {
                needs_repair = true;
                break;
            }
        }
    }
    if !needs_repair {
        return messages;
    }

    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut repaired: Vec<serde_json::Value> = Vec::new();

    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) == Some("assistant")
            && m.get("tool_calls").and_then(|v| v.as_array()).is_some()
        {
            repaired.push(m.clone());
            if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let Some(call_id) = tc.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if let Some(&result_idx) = found_results.get(call_id) {
                        if consumed.insert(result_idx) {
                            repaired.push(messages[result_idx].clone());
                        }
                    } else if !trailing_calls.contains(call_id) {
                        repaired.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": "{\"error\": \"tool result was lost during an interrupted turn\"}",
                        }));
                    }
                }
            }
        } else if consumed.contains(&i) {
            continue;
        } else {
            repaired.push(m.clone());
        }
    }
    repaired
}

/// Extract plain text from an OpenAI message content value.
///
/// Skips non-text content parts (image_url, etc.) — only extracts from
/// parts that have a "text" key with a string value.
fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                // Only extract text from text-type parts; skip image_url etc.
                p.get("text").and_then(|v| v.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn conversation_store_basic() {
        let dir = tempdir().unwrap();
        let store = ConversationStore::open(dir.path()).unwrap();

        let record = SessionRecord {
            session_id: "test-1".to_string(),
            workspace: dir.path().to_str().unwrap().to_string(),
            model: "gpt-5".to_string(),
            mode: "interactive".to_string(),
            messages: vec![
                serde_json::json!({"role": "user", "content": "Hello world"}),
                serde_json::json!({"role": "assistant", "content": "Hi there!"}),
            ],
            ..Default::default()
        };

        store.save(&record).unwrap();
        assert_eq!(store.count_jsonl("test-1"), 2);

        let loaded = store.load("test-1").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.title.as_deref(), Some("Hello world"));

        let listed = store.list(None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].message_count, 2);

        store.rename("test-1", "Renamed Session").unwrap();
        let reloaded = store.load("test-1").unwrap().unwrap();
        assert_eq!(reloaded.title.as_deref(), Some("Renamed Session"));

        store.delete("test-1").unwrap();
        assert!(store.load("test-1").unwrap().is_none());
    }

    #[test]
    fn title_from_messages() {
        assert_eq!(
            ConversationStore::title_from_messages(&[
                serde_json::json!({"role": "user", "content": "First line\nSecond line"}),
            ]),
            "First line"
        );
        assert_eq!(
            ConversationStore::title_from_messages(&[
                serde_json::json!({"role": "system", "content": "You are a helpful assistant"}),
                serde_json::json!({"role": "user", "content": "Hello"}),
            ]),
            "Hello"
        );
        assert_eq!(ConversationStore::title_from_messages(&[]), "New session");
    }

    #[test]
    fn rejects_path_traversal_session_id() {
        assert!(!is_safe_session_id("../evil"));
        assert!(!is_safe_session_id("a/b"));
        assert!(!is_safe_session_id(""));
        assert!(is_safe_session_id("abc-123_XYZ"));
        let dir = tempdir().unwrap();
        let store = ConversationStore::open(dir.path()).unwrap();
        let record = SessionRecord {
            session_id: "../evil".to_string(),
            workspace: dir.path().to_str().unwrap().to_string(),
            model: "gpt-5".to_string(),
            mode: "interactive".to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "x"})],
            ..Default::default()
        };
        assert!(store.save(&record).is_err());
    }

    #[test]
    fn repair_moves_result_and_skips_trailing_pending() {
        // Out-of-order: user between assistant tool_calls and tool result.
        let msgs = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
            serde_json::json!({"role": "user", "content": "interrupt"}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let fixed = repair_tool_pairing(msgs);
        assert_eq!(fixed[1]["role"], "assistant");
        assert_eq!(fixed[2]["role"], "tool");
        assert_eq!(fixed[2]["tool_call_id"], "c1");
        assert_eq!(fixed[3]["role"], "user");

        // Trailing pending — leave alone (durable resume).
        let pending = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{"id": "p1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
        ];
        let same = repair_tool_pairing(pending.clone());
        assert_eq!(same.len(), 2);
        assert_eq!(same[1]["tool_calls"][0]["id"], "p1");
    }
}
