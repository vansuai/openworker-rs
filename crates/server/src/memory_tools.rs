//! Agent memory tools — Rust mirror of `coworker/memory/tools.py`.
//!
//! The agent's explicit paths into memory:
//! - `remember` saves a new fact (gated by the live `saving_enabled` callable).
//! - `memory_read` fetches full bodies by id (never gated — off = stop learning,
//!   not amnesia).
//! - `memory_update` rewrites an existing memory by id.
//! - `memory_forget` deletes a memory by id.
//!
//! `on_saved` is the save-notice hook (spec §5.1): the manager passes a callback
//! that pushes a `memory_saved` event to the session's surface. This plan wires
//! `None` (toast deferred). Failures in the callback never fail the write.

use std::sync::Arc;

use ocw_data::{MemoryBackend, MemoryItem, Scope};
use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};

/// Verbatim copy of Python `_OFF_ERROR` (`coworker/memory/tools.py`). The exact
/// wording is part of the contract — the agent is told to tell the user plainly
/// instead of implying it remembered.
const OFF_ERROR: &str = "Saving memories is turned off in the user's Settings (they can turn it \
back on in Settings ▸ Memory). Nothing was saved — tell the user plainly instead of \
implying you remembered it.";

/// Hook fired after a successful `remember`/`memory_update`. Carries the
/// previous content for `memory_update` so the surface's Undo can restore it.
/// Best-effort: failures never fail the write.
pub(crate) type OnSaved = Arc<dyn Fn(MemoryItem, Option<String>) + Send + Sync>;

/// Register the four agent-facing memory tools into `registry`.
///
/// - `store`: the backing memory store (SQLite in production, in-memory in tests).
/// - `workspace`: the project key for `Scope::Workspace` writes — `None` means
///   "no workspace" (memories are saved with a `None` workspace column).
/// - `saving_enabled`: a LIVE callable checked on each write, so the Settings
///   switch applies to conversations already running in both directions.
/// - `on_saved`: optional save-notice hook (spec §5.1). Pass `None` in this plan.
///
/// Mirrors `coworker/memory/tools.py::memory_tools` and the
/// `register_scheduling_tools` Arc-closure style in `automations.rs`.
pub(crate) fn register_memory_tools(
    registry: &mut ToolRegistry,
    store: Arc<dyn MemoryBackend>,
    workspace: Option<String>,
    saving_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    on_saved: Option<OnSaved>,
) {
    register_remember(
        registry,
        Arc::clone(&store),
        workspace.clone(),
        Arc::clone(&saving_enabled),
        on_saved.clone(),
    );
    register_memory_read(registry, Arc::clone(&store));
    register_memory_update(
        registry,
        Arc::clone(&store),
        Arc::clone(&saving_enabled),
        on_saved.clone(),
    );
    register_memory_forget(registry, store, saving_enabled);
}

fn register_remember(
    registry: &mut ToolRegistry,
    store: Arc<dyn MemoryBackend>,
    workspace: Option<String>,
    saving_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    on_saved: Option<OnSaved>,
) {
    let remember: ToolFn = Arc::new(move |args: Map<String, Value>| {
        if !saving_enabled() {
            return ToolResult::ok(json!({ "saved": false, "error": OFF_ERROR }));
        }
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let scope_str = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("workspace");
        // Python: chosen = Scope(scope) if scope in _SCOPES else Scope.WORKSPACE;
        // SESSION is a dead scope — never save to it (spec §3).
        let mut chosen = match scope_str {
            "global" => Scope::Global,
            "workspace" => Scope::Workspace,
            "session" => Scope::Session,
            _ => Scope::Workspace,
        };
        if chosen == Scope::Session {
            chosen = Scope::Workspace;
        }
        let ws_arg = if chosen == Scope::Workspace {
            workspace.as_deref()
        } else {
            None
        };
        match store.add(&content, chosen, None, summary.as_deref(), ws_arg, None) {
            Ok(item) => {
                fire_on_saved(&on_saved, item.clone(), None);
                ToolResult::ok(json!({
                    "id": item.id,
                    "scope": item.scope.as_str(),
                    "saved": true,
                }))
            }
            Err(e) => ToolResult::ok(json!({
                "saved": false,
                "error": format!("failed to save memory: {e}"),
            })),
        }
    });
    registry.register(
        "remember",
        remember,
        ToolSpec {
            risk_level: "low",
            category: "memory",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "remember",
            Some(
                "Save a durable memory (a fact or preference) to recall in future sessions. \
                 Check the known-memories list first: if one already covers this, use \
                 memory_update instead of saving a near-duplicate.",
            ),
            Some(json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The thing to remember, with the why." },
                    "summary": { "type": "string", "description": "One-line gist (15 words max) shown in compact listings." },
                    "scope": {
                        "type": "string",
                        "enum": ["global", "workspace"],
                        "description": "\"global\" (facts about the user — applies everywhere) or \"workspace\" (facts about this project only)."
                    }
                },
                "required": ["content"]
            })),
        )),
    );
}

fn register_memory_read(registry: &mut ToolRegistry, store: Arc<dyn MemoryBackend>) {
    let memory_read: ToolFn = Arc::new(move |args: Map<String, Value>| {
        let ids = args
            .get("memory_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect::<Vec<i64>>())
            .unwrap_or_default();
        let mut found: Vec<Value> = Vec::new();
        let mut missing: Vec<i64> = Vec::new();
        for mid in ids {
            match store.get(mid) {
                Ok(Some(item)) => found.push(json!({
                    "id": item.id,
                    "scope": item.scope.as_str(),
                    "content": item.content,
                })),
                Ok(None) => missing.push(mid),
                Err(_) => missing.push(mid),
            }
        }
        let mut result = json!({ "memories": found });
        if !missing.is_empty() {
            result["missing"] = json!(missing);
        }
        ToolResult::ok(result)
    });
    registry.register(
        "memory_read",
        memory_read,
        ToolSpec {
            risk_level: "low",
            category: "memory",
            parallel_safe: true,
        },
        Some(ToolSchema::new(
            "memory_read",
            Some(
                "Read the full content of memories by id (use when the known-memories list \
                 shows only a one-line summary and you need the details before acting).",
            ),
            Some(json!({
                "type": "object",
                "properties": {
                    "memory_ids": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "The [#id]s to fetch."
                    }
                },
                "required": ["memory_ids"]
            })),
        )),
    );
}

fn register_memory_update(
    registry: &mut ToolRegistry,
    store: Arc<dyn MemoryBackend>,
    saving_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    on_saved: Option<OnSaved>,
) {
    let memory_update: ToolFn = Arc::new(move |args: Map<String, Value>| {
        if !saving_enabled() {
            return ToolResult::ok(json!({ "updated": false, "error": OFF_ERROR }));
        }
        let memory_id = args.get("memory_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // Capture the previous content BEFORE the write so the surface's Undo
        // can restore it (mirror of Python's `existing.content` capture).
        let previous = match store.get(memory_id) {
            Ok(Some(existing)) => Some(existing.content),
            _ => None,
        };
        match store.update(memory_id, &content, summary.as_deref()) {
            Ok(Some(item)) => {
                fire_on_saved(&on_saved, item.clone(), previous);
                ToolResult::ok(json!({ "updated": true, "id": item.id }))
            }
            Ok(None) => ToolResult::ok(json!({
                "updated": false,
                "error": format!("no memory with id {memory_id}"),
            })),
            Err(e) => ToolResult::ok(json!({
                "updated": false,
                "error": format!("failed to update memory: {e}"),
            })),
        }
    });
    registry.register(
        "memory_update",
        memory_update,
        ToolSpec {
            risk_level: "low",
            category: "memory",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "memory_update",
            Some("Rewrite an existing memory with corrected or refined content."),
            Some(json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "integer",
                        "description": "The memory's id, from the [#id] in the known-memories list."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full corrected memory text (replaces the old text)."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Corrected one-line gist (15 words max)."
                    }
                },
                "required": ["memory_id", "content"]
            })),
        )),
    );
}

fn register_memory_forget(
    registry: &mut ToolRegistry,
    store: Arc<dyn MemoryBackend>,
    saving_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let memory_forget: ToolFn = Arc::new(move |args: Map<String, Value>| {
        if !saving_enabled() {
            return ToolResult::ok(json!({ "deleted": false, "error": OFF_ERROR }));
        }
        let memory_id = args.get("memory_id").and_then(|v| v.as_i64()).unwrap_or(0);
        match store.delete(memory_id) {
            Ok(true) => ToolResult::ok(json!({ "deleted": true, "id": memory_id })),
            Ok(false) => ToolResult::ok(json!({
                "deleted": false,
                "error": format!("no memory with id {memory_id}"),
            })),
            Err(e) => ToolResult::ok(json!({
                "deleted": false,
                "error": format!("failed to forget memory: {e}"),
            })),
        }
    });
    registry.register(
        "memory_forget",
        memory_forget,
        ToolSpec {
            risk_level: "low",
            category: "memory",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "memory_forget",
            Some("Delete a memory that turned out to be wrong or is no longer true."),
            Some(json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "integer",
                        "description": "The memory's id, from the [#id] in the known-memories list."
                    }
                },
                "required": ["memory_id"]
            })),
        )),
    );
}

/// Best-effort fire of the `on_saved` hook. The notice is never worth failing a
/// write that already succeeded (mirror of Python's `_announce`).
fn fire_on_saved(on_saved: &Option<OnSaved>, item: MemoryItem, previous: Option<String>) {
    if let Some(cb) = on_saved {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cb(item, previous);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocw_data::MemoryStore;

    fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn remember_tool_persists_when_saving_on() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let mut reg = ToolRegistry::new();
        register_memory_tools(
            &mut reg,
            store.clone(),
            Some("/tmp/ws".into()),
            Arc::new(|| true),
            None,
        );
        assert!(reg.contains("remember"));
        assert!(reg.contains("memory_read"));
        assert!(reg.contains("memory_update"));
        assert!(reg.contains("memory_forget"));

        let result = reg
            .execute(
                "remember",
                args(&[
                    ("content", json!("likes dark mode")),
                    ("scope", json!("global")),
                ]),
            )
            .expect("remember registered");
        let v = result.value;
        assert_eq!(v["saved"], true);
        let id = v["id"].as_i64().unwrap();

        let listed = store.list(Some(Scope::Global), None, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].content, "likes dark mode");

        let _ = reg
            .execute(
                "remember",
                args(&[
                    ("content", json!("uses postgres")),
                    ("scope", json!("workspace")),
                ]),
            )
            .unwrap();
        let ws_listed = store
            .list(Some(Scope::Workspace), Some("/tmp/ws"), None)
            .unwrap();
        assert_eq!(ws_listed.len(), 1);
        assert_eq!(ws_listed[0].content, "uses postgres");
    }

    #[test]
    fn remember_tool_refuses_when_saving_off() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let mut reg = ToolRegistry::new();
        register_memory_tools(
            &mut reg,
            store.clone(),
            Some("/tmp/ws".into()),
            Arc::new(|| false),
            None,
        );
        let result = reg
            .execute("remember", args(&[("content", json!("likes tea"))]))
            .unwrap();
        let v = result.value;
        assert_eq!(v["saved"], false);
        let err = v["error"].as_str().unwrap();
        assert!(
            err.contains("turned off"),
            "expected the verbatim OFF_ERROR, got: {err}"
        );
        assert!(store.list(None, None, None).unwrap().is_empty());
    }

    #[test]
    fn memory_read_returns_full_bodies_and_missing() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("full body", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store, None, Arc::new(|| true), None);
        let result = reg
            .execute(
                "memory_read",
                args(&[("memory_ids", json!([item.id, 9999]))]),
            )
            .unwrap();
        let v = result.value;
        let mems = v["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0]["id"], item.id);
        assert_eq!(mems[0]["content"], "full body");
        let missing = v["missing"].as_array().unwrap();
        assert_eq!(missing, &vec![json!(9999)]);
    }

    #[test]
    fn memory_read_works_when_saving_off() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("kept", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store, None, Arc::new(|| false), None);
        let result = reg
            .execute("memory_read", args(&[("memory_ids", json!([item.id]))]))
            .unwrap();
        assert_eq!(result.value["memories"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn memory_update_rewrites_existing() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("old", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store.clone(), None, Arc::new(|| true), None);
        let result = reg
            .execute(
                "memory_update",
                args(&[
                    ("memory_id", json!(item.id)),
                    ("content", json!("new text")),
                    ("summary", json!("g")),
                ]),
            )
            .unwrap();
        assert_eq!(result.value["updated"], true);
        let got = store.get(item.id).unwrap().unwrap();
        assert_eq!(got.content, "new text");
        assert_eq!(got.summary.as_deref(), Some("g"));
    }

    #[test]
    fn memory_update_refuses_when_saving_off() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("old", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store.clone(), None, Arc::new(|| false), None);
        let result = reg
            .execute(
                "memory_update",
                args(&[
                    ("memory_id", json!(item.id)),
                    ("content", json!("new")),
                ]),
            )
            .unwrap();
        assert_eq!(result.value["updated"], false);
        assert!(result.value["error"].as_str().unwrap().contains("turned off"));
        let got = store.get(item.id).unwrap().unwrap();
        assert_eq!(got.content, "old");
    }

    #[test]
    fn memory_update_unknown_id() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store, None, Arc::new(|| true), None);
        let result = reg
            .execute(
                "memory_update",
                args(&[
                    ("memory_id", json!(4242)),
                    ("content", json!("x")),
                ]),
            )
            .unwrap();
        assert_eq!(result.value["updated"], false);
        assert!(result.value["error"].as_str().unwrap().contains("4242"));
    }

    #[test]
    fn memory_forget_deletes_existing() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("bye", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store.clone(), None, Arc::new(|| true), None);
        let result = reg
            .execute("memory_forget", args(&[("memory_id", json!(item.id))]))
            .unwrap();
        assert_eq!(result.value["deleted"], true);
        assert!(store.get(item.id).unwrap().is_none());
    }

    #[test]
    fn memory_forget_refuses_when_saving_off() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("bye", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store.clone(), None, Arc::new(|| false), None);
        let result = reg
            .execute("memory_forget", args(&[("memory_id", json!(item.id))]))
            .unwrap();
        assert_eq!(result.value["deleted"], false);
        assert!(result.value["error"].as_str().unwrap().contains("turned off"));
        // Still present.
        assert!(store.get(item.id).unwrap().is_some());
    }

    #[test]
    fn memory_forget_unknown_id() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, store, None, Arc::new(|| true), None);
        let result = reg
            .execute("memory_forget", args(&[("memory_id", json!(7777))]))
            .unwrap();
        assert_eq!(result.value["deleted"], false);
        assert!(result.value["error"].as_str().unwrap().contains("7777"));
    }

    #[test]
    fn remember_session_scope_falls_back_to_workspace() {
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let mut reg = ToolRegistry::new();
        register_memory_tools(
            &mut reg,
            store.clone(),
            Some("/tmp/ws".into()),
            Arc::new(|| true),
            None,
        );
        let result = reg
            .execute(
                "remember",
                args(&[
                    ("content", json!("ephemeral")),
                    ("scope", json!("session")),
                ]),
            )
            .unwrap();
        assert_eq!(result.value["saved"], true);
        // Filed under Workspace, not Session.
        assert_eq!(
            store.list(Some(Scope::Session), None, None).unwrap().len(),
            0
        );
        assert_eq!(
            store
                .list(Some(Scope::Workspace), Some("/tmp/ws"), None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn on_saved_fires_for_remember_and_update() {
        use std::sync::Mutex;
        let calls: Arc<Mutex<Vec<(MemoryItem, Option<String>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let calls_clone = Arc::clone(&calls);
        let on_saved: OnSaved = Arc::new(move |item, previous| {
            calls_clone.lock().unwrap().push((item, previous));
        });
        let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
        let item = store
            .add("old", Scope::Global, None, None, None, None)
            .unwrap();
        let mut reg = ToolRegistry::new();
        register_memory_tools(
            &mut reg,
            store.clone(),
            None,
            Arc::new(|| true),
            Some(on_saved),
        );
        let _ = reg
            .execute("remember", args(&[("content", json!("new fact"))]))
            .unwrap();
        let _ = reg
            .execute(
                "memory_update",
                args(&[
                    ("memory_id", json!(item.id)),
                    ("content", json!("updated")),
                ]),
            )
            .unwrap();
        let logged = calls.lock().unwrap();
        assert_eq!(logged.len(), 2);
        // remember: no previous.
        assert_eq!(logged[0].1, None);
        assert_eq!(logged[0].0.content, "new fact");
        // update: previous = "old".
        assert_eq!(logged[1].1.as_deref(), Some("old"));
        assert_eq!(logged[1].0.content, "updated");
    }
}
