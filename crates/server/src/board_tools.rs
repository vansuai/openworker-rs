//! Board + journal agent tools — Rust port of `coworker/teams/tools.py`
//! registered the same way as `register_scheduling_tools`.
//!
//! Role gate mirrors Python `_team_tools_for`: persona `team:` trait (or
//! `OPENWORKER_TEAM_BOARD=1` → lead), worker verbs vs lead verbs, journal for both.

use std::sync::Arc;

use ocw_data::{
    space_for_workspace, Actor, BoardError, JournalStore, Role, TeamRegistry, TeamStore,
};
use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};

const LEAD_VERBS: &[&str] = &[
    "create_item",
    "list_items",
    "transition",
    "comment",
    "assign",
    "link",
];
const WORKER_VERBS: &[&str] = &[
    "create_item",
    "list_items",
    "transition",
    "comment",
    "claim",
];
const JOURNAL_VERBS: &[&str] = &["journal_append", "journal_read"];

/// Inputs needed to bind board/journal tools for one live session engine.
pub struct BoardToolsArgs {
    pub store: Arc<TeamStore>,
    pub journal: Arc<JournalStore>,
    pub team_registry: Arc<TeamRegistry>,
    pub session_id: String,
    pub persona: String,
    pub workspace: String,
    /// From persona manifest `team:` (`"lead"` / `"worker"`).
    pub team_role: Option<String>,
    /// Fired after a successful mutating board/journal call.
    pub on_mutate: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Tool names the role gate would register (board + journal), for tests.
pub fn board_tool_names_for_role(role: &str) -> Vec<&'static str> {
    let verbs = if role == "worker" {
        WORKER_VERBS
    } else {
        LEAD_VERBS
    };
    let mut names: Vec<&'static str> = verbs.to_vec();
    names.extend_from_slice(JOURNAL_VERBS);
    names
}

/// Resolve the effective team role for a session (persona trait or env override).
pub fn resolve_team_role(team_role: Option<&str>, has_workspace: bool) -> Option<&'static str> {
    if let Some(role) = team_role {
        return match role {
            "worker" => Some("worker"),
            "lead" => Some("lead"),
            _ => None,
        };
    }
    if has_workspace && std::env::var("OPENWORKER_TEAM_BOARD").ok().as_deref() == Some("1") {
        return Some("lead");
    }
    None
}

fn short_session(session_id: &str) -> &str {
    let end = session_id.len().min(8);
    &session_id[..end]
}

fn resolve_actor_space(args: &BoardToolsArgs, role: &str) -> (Actor, String) {
    let default_space = space_for_workspace(&args.workspace);
    if role == "worker" {
        if let Some((team, worker)) = args.team_registry.for_worker_session(&args.session_id) {
            let mut actor = Actor::new(worker.actor, Role::Worker);
            actor.persona = args.persona.clone();
            actor.session_id = args.session_id.clone();
            return (actor, team.space);
        }
        let id = format!("{}:{}", args.persona, short_session(&args.session_id));
        let mut actor = Actor::new(id, Role::Worker);
        actor.persona = args.persona.clone();
        actor.session_id = args.session_id.clone();
        return (actor, default_space);
    }
    // lead
    if let Some(team) = args.team_registry.for_lead_session(&args.session_id) {
        let mut actor = Actor::new(
            if team.lead_actor.is_empty() {
                format!("{}:{}", args.persona, short_session(&args.session_id))
            } else {
                team.lead_actor.clone()
            },
            Role::Lead,
        );
        actor.persona = args.persona.clone();
        actor.session_id = args.session_id.clone();
        return (actor, team.space);
    }
    let id = format!("{}:{}", args.persona, short_session(&args.session_id));
    let mut actor = Actor::new(id, Role::Lead);
    actor.persona = args.persona.clone();
    actor.session_id = args.session_id.clone();
    (actor, default_space)
}

fn map_err(e: BoardError) -> ToolResult {
    ToolResult::ok(json!({ "error": e.to_string() }))
}

fn ok_value(v: impl Into<Value>) -> ToolResult {
    ToolResult::ok(v.into())
}

fn arg_str(args: &Map<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn arg_i64(args: &Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn arg_str_list(args: &Map<String, Value>, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn kick(on_mutate: &Option<Arc<dyn Fn() + Send + Sync>>) {
    if let Some(f) = on_mutate {
        f();
    }
}

fn team_spec(risk: &'static str) -> ToolSpec {
    ToolSpec {
        risk_level: risk,
        category: "team",
        parallel_safe: false,
    }
}

fn schema(name: &str, description: &str, parameters: Value) -> Option<ToolSchema> {
    Some(ToolSchema::new(name, Some(description), Some(parameters)))
}

/// Register board + journal tools into the engine registry when gated in.
pub fn register_board_tools(registry: &mut ToolRegistry, args: BoardToolsArgs) {
    let role = match resolve_team_role(args.team_role.as_deref(), !args.workspace.is_empty()) {
        Some(r) => r,
        None => return,
    };
    if args.workspace.is_empty() {
        return;
    }
    let (actor, space) = resolve_actor_space(&args, role);
    let verbs = if role == "worker" {
        WORKER_VERBS
    } else {
        LEAD_VERBS
    };

    for name in verbs {
        register_one_board(registry, name, &args, &actor, &space);
    }
    for name in JOURNAL_VERBS {
        register_one_journal(registry, name, &args, &actor, &space);
    }
}

fn register_one_board(
    registry: &mut ToolRegistry,
    name: &str,
    args: &BoardToolsArgs,
    actor: &Actor,
    space: &str,
) {
    let store = Arc::clone(&args.store);
    let actor = actor.clone();
    let space = space.to_string();
    let on_mutate = args.on_mutate.clone();

    match name {
        "create_item" => {
            let f: ToolFn = Arc::new(move |a| {
                let title = arg_str(&a, "title");
                let criteria = arg_str(&a, "criteria");
                let description = arg_str(&a, "description");
                let parent = arg_i64(&a, "parent");
                let case = arg_str(&a, "case");
                match store.create_item(
                    &space,
                    &actor,
                    &title,
                    &criteria,
                    &description,
                    parent,
                    if case.is_empty() { None } else { Some(case.as_str()) },
                ) {
                    Ok(item) => {
                        kick(&on_mutate);
                        ok_value(serde_json::to_value(item).unwrap_or(json!({"ok": true})))
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "create_item",
                f,
                team_spec("low"),
                schema(
                    "create_item",
                    "Create a work item (open, unassigned — work starts when it is assigned). \
                     `criteria` is the acceptance criteria — what gets verified before the item \
                     can be done; required. `parent` links it under another item; `case` names \
                     its journal case (children inherit the parent's case by default).",
                    json!({
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "criteria": {"type": "string"},
                            "description": {"type": "string"},
                            "parent": {"type": "integer"},
                            "case": {"type": "string"}
                        },
                        "required": ["title", "criteria"]
                    }),
                ),
            );
        }
        "list_items" => {
            let f: ToolFn = Arc::new(move |a| {
                let state = arg_str(&a, "state");
                let assignee = arg_str(&a, "assignee");
                match store.list_items(
                    &space,
                    &actor,
                    if state.is_empty() { None } else { Some(state.as_str()) },
                    if assignee.is_empty() {
                        None
                    } else {
                        Some(assignee.as_str())
                    },
                ) {
                    Ok(items) => ok_value(json!({ "items": items })),
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "list_items",
                f,
                team_spec("low"),
                schema(
                    "list_items",
                    "List work items on the board, optionally filtered by state \
                     (open/in_progress/blocked/review/done/canceled) or assignee.",
                    json!({
                        "type": "object",
                        "properties": {
                            "state": {"type": "string"},
                            "assignee": {"type": "string"}
                        }
                    }),
                ),
            );
        }
        "transition" => {
            let f: ToolFn = Arc::new(move |a| {
                let Some(item) = arg_i64(&a, "item") else {
                    return ToolResult::ok(json!({"error": "item is required"}));
                };
                let to = arg_str(&a, "to");
                let comment = arg_str(&a, "comment");
                match store.transition(&space, &actor, item, &to, &comment) {
                    Ok(item) => {
                        kick(&on_mutate);
                        ok_value(serde_json::to_value(item).unwrap_or(json!({"ok": true})))
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "transition",
                f,
                team_spec("low"),
                schema(
                    "transition",
                    "Move a work item to a new state. Workers move their own item to \
                     in_progress, blocked, or review; done requires review verification first.",
                    json!({
                        "type": "object",
                        "properties": {
                            "item": {"type": "integer"},
                            "to": {"type": "string"},
                            "comment": {"type": "string"},
                            "refs": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["item", "to"]
                    }),
                ),
            );
        }
        "comment" => {
            let f: ToolFn = Arc::new(move |a| {
                let Some(item) = arg_i64(&a, "item") else {
                    return ToolResult::ok(json!({"error": "item is required"}));
                };
                let body = arg_str(&a, "body");
                match store.comment(&space, &actor, item, &body) {
                    Ok(v) => {
                        kick(&on_mutate);
                        ok_value(v)
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "comment",
                f,
                team_spec("low"),
                schema(
                    "comment",
                    "Add a comment to a work item. Comments are durable and attributed — \
                     answers that matter belong here, not in chat.",
                    json!({
                        "type": "object",
                        "properties": {
                            "item": {"type": "integer"},
                            "body": {"type": "string"},
                            "refs": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["item", "body"]
                    }),
                ),
            );
        }
        "claim" => {
            let f: ToolFn = Arc::new(move |a| {
                let Some(item) = arg_i64(&a, "item") else {
                    return ToolResult::ok(json!({"error": "item is required"}));
                };
                match store.claim(&space, &actor, item) {
                    Ok(item) => {
                        kick(&on_mutate);
                        ok_value(serde_json::to_value(item).unwrap_or(json!({"ok": true})))
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "claim",
                f,
                team_spec("low"),
                schema(
                    "claim",
                    "Claim an open, unassigned work item for yourself. First claim wins.",
                    json!({
                        "type": "object",
                        "properties": {
                            "item": {"type": "integer"}
                        },
                        "required": ["item"]
                    }),
                ),
            );
        }
        "assign" => {
            let f: ToolFn = Arc::new(move |a| {
                let Some(item) = arg_i64(&a, "item") else {
                    return ToolResult::ok(json!({"error": "item is required"}));
                };
                let assignee = arg_str(&a, "assignee");
                match store.assign(&space, &actor, item, &assignee) {
                    Ok(item) => {
                        kick(&on_mutate);
                        ok_value(serde_json::to_value(item).unwrap_or(json!({"ok": true})))
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "assign",
                f,
                team_spec("medium"),
                schema(
                    "assign",
                    "Assign a work item to a worker coworker. The item itself becomes the \
                     worker's assignment — write the description and criteria accordingly.",
                    json!({
                        "type": "object",
                        "properties": {
                            "item": {"type": "integer"},
                            "assignee": {"type": "string"}
                        },
                        "required": ["item", "assignee"]
                    }),
                ),
            );
        }
        "link" => {
            let f: ToolFn = Arc::new(move |a| {
                let Some(src) = arg_i64(&a, "src") else {
                    return ToolResult::ok(json!({"error": "src is required"}));
                };
                let Some(dst) = arg_i64(&a, "dst") else {
                    return ToolResult::ok(json!({"error": "dst is required"}));
                };
                let kind = arg_str(&a, "kind");
                match store.link(&space, &actor, src, &kind, dst) {
                    Ok(()) => {
                        kick(&on_mutate);
                        ok_value(json!({"ok": true}))
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "link",
                f,
                team_spec("low"),
                schema(
                    "link",
                    "Link two work items: kind `parent` (dst becomes src's parent) or \
                     `blocks` (src blocks dst).",
                    json!({
                        "type": "object",
                        "properties": {
                            "src": {"type": "integer"},
                            "kind": {"type": "string"},
                            "dst": {"type": "integer"}
                        },
                        "required": ["src", "kind", "dst"]
                    }),
                ),
            );
        }
        _ => {}
    }
}

fn register_one_journal(
    registry: &mut ToolRegistry,
    name: &str,
    args: &BoardToolsArgs,
    actor: &Actor,
    space: &str,
) {
    let journal = Arc::clone(&args.journal);
    let actor = actor.clone();
    let space = space.to_string();
    let on_mutate = args.on_mutate.clone();

    match name {
        "journal_append" => {
            let f: ToolFn = Arc::new(move |a| {
                let case = arg_str(&a, "case");
                let body = arg_str(&a, "body");
                let kind = {
                    let k = arg_str(&a, "kind");
                    if k.is_empty() {
                        "note".into()
                    } else {
                        k
                    }
                };
                let item = arg_i64(&a, "item");
                let entities = arg_str_list(&a, "entities");
                let refs = arg_str_list(&a, "refs");
                match journal.append_full(
                    &actor,
                    &case,
                    &body,
                    &kind,
                    Some(space.as_str()),
                    item,
                    Some(&entities),
                    Some(&refs),
                    false,
                ) {
                    Ok(v) => {
                        kick(&on_mutate);
                        ok_value(v)
                    }
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "journal_append",
                f,
                team_spec("low"),
                schema(
                    "journal_append",
                    "Append an entry to a journal case as you work: kind is finding, \
                     evidence, decision, note, or raw.",
                    json!({
                        "type": "object",
                        "properties": {
                            "case": {"type": "string"},
                            "body": {"type": "string"},
                            "kind": {"type": "string"},
                            "item": {"type": "integer"},
                            "entities": {"type": "array", "items": {"type": "string"}},
                            "refs": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["case", "body"]
                    }),
                ),
            );
        }
        "journal_read" => {
            let f: ToolFn = Arc::new(move |a| {
                let case = arg_str(&a, "case");
                let item = arg_i64(&a, "item");
                let author = arg_str(&a, "author");
                let kind = arg_str(&a, "kind");
                let entity = arg_str(&a, "entity");
                let include_raw = a
                    .get("include_raw")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let limit = arg_i64(&a, "limit").unwrap_or(50);
                match journal.read(
                    &actor,
                    &case,
                    item,
                    if author.is_empty() {
                        None
                    } else {
                        Some(author.as_str())
                    },
                    if kind.is_empty() {
                        None
                    } else {
                        Some(kind.as_str())
                    },
                    if entity.is_empty() {
                        None
                    } else {
                        Some(entity.as_str())
                    },
                    0,
                    include_raw,
                    limit,
                ) {
                    Ok(entries) => ok_value(json!({ "entries": entries })),
                    Err(e) => map_err(e),
                }
            });
            registry.register(
                "journal_read",
                f,
                team_spec("low"),
                schema(
                    "journal_read",
                    "Read a journal case, filtered by item, author, entry kind, or entity.",
                    json!({
                        "type": "object",
                        "properties": {
                            "case": {"type": "string"},
                            "item": {"type": "integer"},
                            "author": {"type": "string"},
                            "kind": {"type": "string"},
                            "entity": {"type": "string"},
                            "include_raw": {"type": "boolean"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["case"]
                    }),
                ),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocw_engine::ToolRegistry;

    #[test]
    fn worker_registry_lacks_assign_lead_has_assign() {
        let worker_names = board_tool_names_for_role("worker");
        assert!(worker_names.contains(&"claim"));
        assert!(!worker_names.contains(&"assign"));
        assert!(!worker_names.contains(&"link"));
        assert!(worker_names.contains(&"journal_append"));

        let lead_names = board_tool_names_for_role("lead");
        assert!(lead_names.contains(&"assign"));
        assert!(lead_names.contains(&"link"));
        assert!(!lead_names.contains(&"claim"));
        assert!(lead_names.contains(&"journal_read"));

        let store = Arc::new(TeamStore::open_in_memory().unwrap());
        let journal = Arc::new(JournalStore::open_in_memory().unwrap());
        let team_registry = Arc::new(TeamRegistry::in_memory());

        let mut worker_reg = ToolRegistry::new();
        register_board_tools(
            &mut worker_reg,
            BoardToolsArgs {
                store: Arc::clone(&store),
                journal: Arc::clone(&journal),
                team_registry: Arc::clone(&team_registry),
                session_id: "sess-worker-1".into(),
                persona: "swe-worker".into(),
                workspace: "/tmp/ws".into(),
                team_role: Some("worker".into()),
                on_mutate: None,
            },
        );
        assert!(worker_reg.contains("claim"));
        assert!(!worker_reg.contains("assign"));
        assert!(!worker_reg.contains("link"));
        assert!(worker_reg.contains("journal_append"));

        let mut lead_reg = ToolRegistry::new();
        register_board_tools(
            &mut lead_reg,
            BoardToolsArgs {
                store,
                journal,
                team_registry,
                session_id: "sess-lead-1".into(),
                persona: "swe-lead".into(),
                workspace: "/tmp/ws".into(),
                team_role: Some("lead".into()),
                on_mutate: None,
            },
        );
        assert!(lead_reg.contains("assign"));
        assert!(lead_reg.contains("link"));
        assert!(!lead_reg.contains("claim"));
        assert!(lead_reg.contains("create_item"));
    }

    #[test]
    fn solo_persona_registers_nothing() {
        let mut reg = ToolRegistry::new();
        register_board_tools(
            &mut reg,
            BoardToolsArgs {
                store: Arc::new(TeamStore::open_in_memory().unwrap()),
                journal: Arc::new(JournalStore::open_in_memory().unwrap()),
                team_registry: Arc::new(TeamRegistry::in_memory()),
                session_id: "s1".into(),
                persona: "cowork".into(),
                workspace: "/tmp/ws".into(),
                team_role: None,
                on_mutate: None,
            },
        );
        assert!(!reg.contains("create_item"));
    }
}
