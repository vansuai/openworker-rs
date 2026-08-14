//! Inbox store — Rust reimplementation of `coworker/inbox.py`.
//!
//! The Inbox is the canonical, cross-session human-attention queue. While a user works
//! in one session (or is away with a session running Unattended), the Inbox holds what
//! other agents need from them: an **approval**, a **question**, or a **notification**.
//!
//! Storage layout: a single JSON file with all items. Idempotent by
//! (session_id, tool_call_id) so durable resume can rebuild the suspension and
//! continue the turn safely.
//!
//! Item state machine: `pending → resolved`, resolved **once**, idempotent + first
//! responder wins — answering from any surface (in-app, Slack, composer after resume)
//! is safe.

use crate::Error;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Notify;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const KIND_APPROVAL: &str = "approval";
pub const KIND_QUESTION: &str = "question";
pub const KIND_NOTIFICATION: &str = "notification";
pub const KIND_DIRECTORY: &str = "directory";
pub const KIND_PLAN: &str = "plan";

pub const STATE_PENDING: &str = "pending";
pub const STATE_RESOLVED: &str = "resolved";

// Where a pending prompt surfaces. INLINE = an attended session answers it in the
// composer (parked server-side, redelivered on reconnect, never in the cross-session
// list). INBOX = the user set the session Unattended, so it joins the cross-session
// Inbox queue.
pub const VIS_INLINE: &str = "inline";
pub const VIS_INBOX: &str = "inbox";

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single inbox item awaiting (or already recorded) human attention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub state: String,
    /// approval: "allow"/"deny"/"always"; question: answer text.
    pub resolution: Option<String>,
    #[serde(default = "default_inbox_name")]
    pub inbox: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    /// inline (attended) vs inbox (unattended).
    pub visibility: String,
    /// The tool call this prompt is blocking. Makes an item idempotent by
    /// (session_id, tool_call_id) for durable resume.
    pub tool_call_id: Option<String>,
    /// ask_user quick-reply options.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_text: bool,
    #[serde(default)]
    pub multi: bool,
    /// Kind-specific payload (directory: suggested path/writable; plan: the plan text).
    #[serde(default)]
    pub data: serde_json::Value,
}

fn default_inbox_name() -> String {
    "default".to_string()
}
fn default_true() -> bool {
    true
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Compact one-line preview of a tool call's arguments for an approval card body.
pub fn args_preview(arguments: Option<&serde_json::Value>, limit: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(obj) = arguments.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            let s = if v.is_string() {
                v.as_str().unwrap_or("").to_string()
            } else {
                v.to_string()
            };
            let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            // Char-based truncation: tool args carry CJK content, and a byte slice
            // mid-sequence panics (same class as the `preview()` crash).
            let truncated = if collapsed.chars().count() > 80 {
                format!("{}…", crate::truncate_chars(&collapsed, 79))
            } else {
                collapsed
            };
            parts.push(format!("{k}: {truncated}"));
        }
    }
    let out = parts.join(" · ");
    if out.chars().count() > limit {
        format!("{}…", crate::truncate_chars(&out, limit.saturating_sub(1)))
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFile {
    #[serde(default)]
    items: Vec<InboxItem>,
}

/// The Inbox store. Thread-safe (parking_lot::Mutex) and async-waitable
/// (tokio::sync::Notify for wakeups).
pub struct InboxStore {
    path: Option<PathBuf>,
    state: Mutex<HashMap<String, InboxItem>>,
    waiters: Mutex<HashMap<String, Arc<Notify>>>,
}

impl InboxStore {
    /// Create a new in-memory store. If `path` is provided, load existing items and
    /// persist on every mutation.
    pub fn new(path: Option<impl AsRef<Path>>) -> Result<Self, Error> {
        let store = Self {
            path: path.map(|p| p.as_ref().to_path_buf()),
            state: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
        };
        store.load_from_disk();
        Ok(store)
    }

    fn load_from_disk(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if !path.is_file() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<PersistedFile>(&text) else {
            return;
        };
        let mut state = self.state.lock();
        for item in parsed.items {
            state.insert(item.id.clone(), item);
        }
    }

    fn save_to_disk(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let state = self.state.lock();
        let payload = PersistedFile {
            items: state.values().cloned().collect(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(path, text);
        }
    }

    fn waiter_for(&self, item_id: &str) -> Arc<Notify> {
        let mut waiters = self.waiters.lock();
        waiters
            .entry(item_id.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    // -- queries ---------------------------------------------------------------

    pub fn get(&self, item_id: &str) -> Option<InboxItem> {
        self.state.lock().get(item_id).cloned()
    }

    pub fn list(
        &self,
        session_id: Option<&str>,
        state: Option<&str>,
        inbox: Option<&str>,
        visibility: Option<&str>,
    ) -> Vec<InboxItem> {
        let snapshot = self.state.lock();
        let mut out: Vec<InboxItem> = snapshot
            .values()
            .filter(|i| session_id.map_or(true, |s| i.session_id == s))
            .filter(|i| state.map_or(true, |s| i.state == s))
            .filter(|i| inbox.map_or(true, |n| i.inbox == n))
            .filter(|i| visibility.map_or(true, |v| i.visibility == v))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    pub fn pending(&self, session_id: Option<&str>) -> Vec<InboxItem> {
        self.list(session_id, Some(STATE_PENDING), None, None)
    }

    pub fn for_tool_call(&self, session_id: &str, tool_call_id: &str) -> Option<InboxItem> {
        self.state
            .lock()
            .values()
            .find(|i| i.session_id == session_id && i.tool_call_id.as_deref() == Some(tool_call_id))
            .cloned()
    }

    // -- adding ----------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        session_id: &str,
        kind: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
        data: serde_json::Value,
        options: Vec<String>,
        allow_text: bool,
        multi: bool,
        tool_call_id: Option<&str>,
    ) -> InboxItem {
        // Idempotent by (session_id, tool_call_id): a durable resume re-raises the
        // same prompt and must reuse the existing (possibly already-resolved) item.
        if let Some(tcid) = tool_call_id {
            if let Some(existing) = self.for_tool_call(session_id, tcid) {
                return existing;
            }
        }
        let item = InboxItem {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            body,
            state: STATE_PENDING.to_string(),
            resolution: None,
            inbox: inbox.to_string(),
            created_at: now_iso(),
            resolved_at: None,
            visibility: visibility.to_string(),
            tool_call_id: tool_call_id.map(|s| s.to_string()),
            options,
            allow_text,
            multi,
            data,
        };
        {
            let mut state = self.state.lock();
            state.insert(item.id.clone(), item.clone());
        }
        self.save_to_disk();
        item
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_approval(
        &self,
        session_id: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
        data: serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> InboxItem {
        self.add(
            session_id,
            KIND_APPROVAL,
            title,
            body,
            inbox,
            visibility,
            data,
            Vec::new(),
            true,
            false,
            tool_call_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_question(
        &self,
        session_id: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
        options: Vec<String>,
        allow_text: bool,
        multi: bool,
        tool_call_id: Option<&str>,
    ) -> InboxItem {
        self.add(
            session_id,
            KIND_QUESTION,
            title,
            body,
            inbox,
            visibility,
            serde_json::Value::Null,
            options,
            allow_text,
            multi,
            tool_call_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_directory(
        &self,
        session_id: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
        data: serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> InboxItem {
        self.add(
            session_id,
            KIND_DIRECTORY,
            title,
            body,
            inbox,
            visibility,
            data,
            Vec::new(),
            true,
            false,
            tool_call_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_plan(
        &self,
        session_id: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
        data: serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> InboxItem {
        self.add(
            session_id,
            KIND_PLAN,
            title,
            body,
            inbox,
            visibility,
            data,
            Vec::new(),
            true,
            false,
            tool_call_id,
        )
    }

    pub fn add_notification(
        &self,
        session_id: &str,
        title: &str,
        body: String,
        inbox: &str,
        visibility: &str,
    ) -> InboxItem {
        self.add(
            session_id,
            KIND_NOTIFICATION,
            title,
            body,
            inbox,
            visibility,
            serde_json::Value::Null,
            Vec::new(),
            true,
            false,
            None,
        )
    }

    // -- state machine ---------------------------------------------------------

    /// Resolve an item exactly once. First responder wins; later attempts are no-ops
    /// (return false). Awakens any suspended waiter.
    pub fn resolve(&self, item_id: &str, resolution: &str) -> bool {
        let notify = {
            let mut state = self.state.lock();
            let Some(item) = state.get_mut(item_id) else {
                return false;
            };
            if item.state == STATE_RESOLVED {
                return false;
            }
            item.state = STATE_RESOLVED.to_string();
            item.resolution = Some(resolution.to_string());
            item.resolved_at = Some(now_iso());
            self.waiter_for(item_id)
        }; // lock released here; save_to_disk acquires it again safely
        self.save_to_disk();
        notify.notify_waiters();
        true
    }

    /// Resolve every still-pending item of a session (called when a session is deleted).
    pub fn resolve_session(&self, session_id: &str, resolution: &str) -> usize {
        let pending_ids: Vec<String> = self
            .pending(Some(session_id))
            .into_iter()
            .map(|i| i.id)
            .collect();
        let mut closed = 0;
        for id in pending_ids {
            if self.resolve(&id, resolution) {
                closed += 1;
            }
        }
        closed
    }

    /// Await an item's resolution; returns the resolution string.
    pub async fn wait(&self, item_id: &str) -> String {
        // Acquire the notify handle first, then re-check state. tokio::Notify does
        // not latch (unlike Python's asyncio.Event): a resolve() that fires between
        // get() and notified().await would otherwise be lost.
        let notify = self.waiter_for(item_id);
        {
            let state = self.state.lock();
            if let Some(item) = state.get(item_id) {
                if item.state == STATE_RESOLVED {
                    return item.resolution.clone().unwrap_or_default();
                }
            }
        }
        loop {
            notify.notified().await;
            if let Some(item) = self.get(item_id) {
                if item.state == STATE_RESOLVED {
                    return item.resolution.unwrap_or_default();
                }
            }
        }
    }

    /// When a user resumes attended control, surface this session's still-pending items
    /// inline plus a recap of what was answered while away.
    pub fn reconcile_on_resume(&self, session_id: &str) -> serde_json::Value {
        let pending = self.pending(Some(session_id));
        let recap = self.list(Some(session_id), Some(STATE_RESOLVED), None, None);
        serde_json::json!({
            "pending": pending,
            "recap": recap,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> InboxStore {
        InboxStore::new(None::<&str>).expect("in-memory store")
    }

    #[test]
    fn add_and_get() {
        let s = store();
        let item = s.add_notification("sess", "hello", "body".into(), "default", VIS_INBOX);
        assert_eq!(item.kind, KIND_NOTIFICATION);
        assert_eq!(item.state, STATE_PENDING);
        let fetched = s.get(&item.id).expect("present");
        assert_eq!(fetched.title, "hello");
    }

    #[test]
    fn resolve_is_idempotent() {
        let s = store();
        let item = s.add_notification("sess", "x", String::new(), "default", VIS_INBOX);
        assert!(s.resolve(&item.id, "ack"));
        assert!(!s.resolve(&item.id, "again"));
        let fetched = s.get(&item.id).expect("present");
        assert_eq!(fetched.state, STATE_RESOLVED);
        assert_eq!(fetched.resolution.as_deref(), Some("ack"));
    }

    #[test]
    fn add_is_idempotent_by_tool_call_id() {
        let s = store();
        let first = s.add_approval(
            "sess",
            "Run `write_file`?",
            String::new(),
            "default",
            VIS_INBOX,
            serde_json::Value::Null,
            Some("call-1"),
        );
        let second = s.add_approval(
            "sess",
            "Run `write_file`?",
            String::new(),
            "default",
            VIS_INBOX,
            serde_json::Value::Null,
            Some("call-1"),
        );
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn resolve_session_closes_all_pending() {
        let s = store();
        s.add_question(
            "a",
            "q1",
            String::new(),
            "default",
            VIS_INBOX,
            vec![],
            true,
            false,
            None,
        );
        s.add_question(
            "a",
            "q2",
            String::new(),
            "default",
            VIS_INBOX,
            vec![],
            true,
            false,
            None,
        );
        s.add_question(
            "b",
            "other",
            String::new(),
            "default",
            VIS_INBOX,
            vec![],
            true,
            false,
            None,
        );
        let closed = s.resolve_session("a", "session deleted");
        assert_eq!(closed, 2);
        assert_eq!(s.pending(Some("a")).len(), 0);
        assert_eq!(s.pending(Some("b")).len(), 1);
    }

    #[test]
    fn list_filters() {
        let s = store();
        s.add_question(
            "s1",
            "q",
            String::new(),
            "inb-a",
            VIS_INBOX,
            vec![],
            true,
            false,
            None,
        );
        s.add_question(
            "s2",
            "q",
            String::new(),
            "inb-b",
            VIS_INBOX,
            vec![],
            true,
            false,
            None,
        );
        let only_a = s.list(None, None, Some("inb-a"), None);
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].inbox, "inb-a");
    }

    #[test]
    fn args_preview_truncates() {
        let args = serde_json::json!({"path": "/tmp/x", "content": "a".repeat(200)});
        let preview = args_preview(Some(&args), 240);
        assert!(preview.contains("path: /tmp/x"));
        assert!(preview.contains('…'));
    }

    #[test]
    fn args_preview_cjk_does_not_panic() {
        // Regression (owner-hit 2026-08-08): byte-index slicing into CJK arg
        // content panicked (79 is not a char boundary of 3-byte sequences).
        let args = serde_json::json!({"content": "中".repeat(200)});
        let preview = args_preview(Some(&args), 240);
        assert!(preview.contains('…'));
        assert!(preview.chars().count() <= 241); // 240 + the ellipsis
    }

    #[test]
    fn reconcile_returns_pending_and_recap() {
        let s = store();
        let item = s.add_notification("s", "x", String::new(), "default", VIS_INBOX);
        s.resolve(&item.id, "ok");
        let report = s.reconcile_on_resume("s");
        assert_eq!(report["pending"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(report["recap"].as_array().map(|a| a.len()), Some(1));
    }

    // -- file-backed store regression: resolve must not deadlock when save_to_disk
    //    acquires self.state.lock() (parking_lot::Mutex is non-reentrant).

    fn file_store() -> InboxStore {
        let dir = std::env::temp_dir().join("ocw-inbox-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("test-{}.json", std::process::id()));
        InboxStore::new(Some(&path)).expect("file-backed store")
    }

    #[test]
    fn resolve_with_file_backed_store_does_not_deadlock() {
        let s = file_store();
        let item = s.add_question(
            "s",
            "q?",
            String::new(),
            "default",
            VIS_INBOX,
            vec!["A".into(), "B".into()],
            true,
            false,
            None,
        );
        // Before the fix this would deadlock: resolve() held state.lock()
        // and save_to_disk() tried to acquire it again.
        assert!(s.resolve(&item.id, "A"));
        // Verify the resolution was persisted in-memory.
        let fetched = s.get(&item.id).expect("still present");
        assert_eq!(fetched.state, STATE_RESOLVED);
        assert_eq!(fetched.resolution.as_deref(), Some("A"));
    }

    #[test]
    fn wait_returns_immediately_when_already_resolved() {
        // Aligns with Python asyncio.Event latch semantics: resolve before wait
        // must still succeed (tokio::Notify does not latch without the re-check).
        let s = store();
        let item = s.add_question(
            "s",
            "city?",
            String::new(),
            "default",
            VIS_INBOX,
            vec!["北京".into()],
            true,
            false,
            None,
        );
        assert!(s.resolve(&item.id, "北京"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let answer = rt.block_on(s.wait(&item.id));
        assert_eq!(answer, "北京");
    }
}
