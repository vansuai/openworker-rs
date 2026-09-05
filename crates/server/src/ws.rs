//! WebSocket session handler for `/ws/session/{session_id}`.

use std::collections::VecDeque;
use std::sync::Arc as StdArc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{FutureExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;
use ocw_data::VIS_INBOX;
use ocw_data::VIS_INLINE;
use ocw_engine::{
    parse_reviewer_response, build_review_prompt, ApprovalOutcome, Approver,
    PermissionRequest, ReviewerDecision, ReviewerFn, REVIEWER_INSTRUCTIONS,
};
use ocw_provider::Provider;
use ocw_skills::{LoadSkillTool, SkillLoader};
use ocw_tools;

const RATE_LIMIT_COUNT: usize = 30;
const RATE_LIMIT_WINDOW_SECS: f64 = 10.0;
const MAX_MESSAGE_TEXT_CHARS: usize = 200_000;
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENTS_BYTES: usize = 15_000_000;
const MAX_IMAGE_CHARS: usize = 12_000_000;

/// Live Auto-Approve reviewer: same model as the session, fail-closed to Unsure.
fn make_session_reviewer(provider: StdArc<dyn Provider>, model: String) -> ReviewerFn {
    StdArc::new(move |tool, args, users, provenance| {
        let provider = StdArc::clone(&provider);
        let model = model.clone();
        Box::pin(async move {
            let mut prompt = build_review_prompt(&users, &tool, &args, &[]);
            if !provenance.is_empty() {
                prompt.push_str("\n\nPROVENANCE:\n");
                prompt.push_str(&provenance);
            }
            let messages = vec![
                json!({"role": "system", "content": REVIEWER_INSTRUCTIONS}),
                json!({"role": "user", "content": prompt}),
            ];
            let settings = json!({"max_tokens": 256});
            match tokio::task::spawn_blocking(move || {
                provider.complete(&model, messages, None, settings)
            })
            .await
            {
                Ok(Ok(turn)) => {
                    parse_reviewer_response(turn.text.as_deref().unwrap_or(""))
                }
                _ => ReviewerDecision {
                    verdict: ocw_engine::ReviewerVerdict::Unsure,
                    reason: "reviewer unavailable".into(),
                },
            }
        })
    })
}
const MAX_PDF_CHARS: usize = 15_000_000;
const MAX_TEXT_CHARS: usize = 200_000;

/// Conservative UTF-8 byte size of a parsed JSON value without re-serializing.
/// Mirrors Python's `_json_value_size` in app.py.
fn json_value_size(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Object(m) => m.iter().map(|(k, v)| k.len() + json_value_size(v)).sum(),
        serde_json::Value::Array(arr) => arr.iter().map(json_value_size).sum(),
        _ => 8,
    }
}

/// Validate user_message attachments against the same limits the Python server enforces.
/// Returns `Ok(())` if the message is acceptable, otherwise a human-readable reject reason.
fn validate_attachments(attachments: &[serde_json::Value]) -> Result<(), String> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(format!(
            "Too many attachments ({}; limit {_max}).",
            attachments.len(),
            _max = MAX_ATTACHMENTS
        ));
    }
    if json_value_size(&serde_json::Value::Array(attachments.to_vec())) > MAX_ATTACHMENTS_BYTES {
        return Err("Attachments too large (limit 15 MB per message).".to_string());
    }
    for att in attachments {
        let Some(obj) = att.as_object() else {
            return Err("Invalid attachment: expected an object.".to_string());
        };
        let kind = obj.get("kind").and_then(|v| v.as_str());
        if !matches!(kind, Some("image") | Some("pdf") | Some("text")) {
            return Err("Invalid attachment kind.".to_string());
        }
        if let Some(name) = obj.get("name") {
            if !name.is_string() || name.as_str().unwrap().len() > 1024 {
                return Err("Invalid attachment name.".to_string());
            }
        }
        if let Some(mime) = obj.get("mime") {
            if !mime.is_string() || mime.as_str().unwrap().len() > 255 {
                return Err("Invalid attachment MIME type.".to_string());
            }
        }
        match kind {
            Some("image") => {
                let data = obj.get("data_url").and_then(|v| v.as_str()).unwrap_or("");
                if !data.starts_with("data:image/")
                    || !data.contains(";base64,")
                    || data.len() > MAX_IMAGE_CHARS
                {
                    return Err("Invalid or oversized image attachment.".to_string());
                }
            }
            Some("pdf") => {
                let data = obj.get("data_url").and_then(|v| v.as_str()).unwrap_or("");
                if !data.starts_with("data:application/pdf;base64,") || data.len() > MAX_PDF_CHARS {
                    return Err("Invalid or oversized PDF attachment.".to_string());
                }
            }
            Some("text") => {
                let body = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if body.len() > MAX_TEXT_CHARS {
                    return Err("Invalid or oversized text attachment.".to_string());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inbound types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UserMessage {
    text: Option<String>,
    attachments: Option<Vec<serde_json::Value>>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApprovalMsg {
    decision: String,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
    /// Optional pre-mapped outcome — if the client already did the
    /// once/always_tool/deny mapping server-side, the server still routes the
    /// resolution through the inbox (durable resume) but trusts the outcome.
    #[serde(default)]
    resolution: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DirectoryResponse {
    granted: bool,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    writable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PlanResponse {
    approved: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    feedback: Option<String>,
}

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct OutMsg {
    #[serde(rename = "type")]
    type_: String,
    data: serde_json::Value,
}

impl OutMsg {
    fn new(type_: &str, data: serde_json::Value) -> Self {
        Self {
            type_: type_.into(),
            data,
        }
    }
}

async fn send_ws(ws: &mut WebSocket, type_: &str, data: serde_json::Value) {
    let msg = OutMsg::new(type_, data);
    let json_str = serde_json::to_string(&msg).unwrap_or_default();
    let _ = ws.send(Message::Text(json_str.into())).await;
}

/// Handshake payload for a reconnecting client. `running` is server truth so a
/// mid-turn sidebar revisit can restore Stop + the waiting row (Python parity).
fn ready_payload(session: &crate::state::SessionMeta, running: bool) -> serde_json::Value {
    json!({
        "session_id": session.session_id,
        "agent": session.agent,
        "model": session.model,
        "mode": session.mode,
        "workspace": session.workspace,
        "running": running,
    })
}

// ---------------------------------------------------------------------------
// Per-socket session context
// ---------------------------------------------------------------------------

struct SessionCtx {
    session_id: String,
    agent: String,
    workspace: Option<String>,
    /// Shared per-session state (engine, running/cancel flags). Survives WS reconnects.
    run: StdArc<crate::state::SessionRunState>,
    /// Per-session shared task list; mutated by `todo_write` and read by the GUI.
    todo_list: StdArc<ocw_tools::TodoList>,
}

impl SessionCtx {
    /// Create a SessionCtx that reuses shared per-session run state from the
    /// registry (mirrors Python's `SessionManager.get_engine`).
    fn with_engine(
        session_id: String,
        agent: String,
        workspace: Option<String>,
        run: StdArc<crate::state::SessionRunState>,
    ) -> Self {
        Self {
            session_id,
            agent,
            workspace,
            run,
            todo_list: ocw_tools::TodoList::new(),
        }
    }

    #[allow(dead_code)]
    fn mark_idle(&self) {
        *self.run.running.write() = false;
    }

    #[allow(dead_code)]
    fn try_claim(&self) -> bool {
        let mut r = self.run.running.write();
        if *r {
            false
        } else {
            *r = true;
            true
        }
    }
}

impl Clone for SessionCtx {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            agent: self.agent.clone(),
            workspace: self.workspace.clone(),
            run: StdArc::clone(&self.run),
            todo_list: StdArc::clone(&self.todo_list),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    agent: Option<String>,
    workspace: Option<String>,
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    origin.starts_with("tauri://localhost")
        || origin.starts_with("http://localhost")
        || origin.starts_with("http://127.0.0.1")
        || origin.starts_with("https://localhost")
        || origin.starts_with("https://tauri.localhost")
}

struct RateLimiter {
    times: VecDeque<f64>,
    window_secs: f64,
    max_count: usize,
}

impl RateLimiter {
    fn new(window_secs: f64, max_count: usize) -> Self {
        Self {
            times: VecDeque::new(),
            window_secs,
            max_count,
        }
    }
    fn check(&mut self, now: f64) -> bool {
        while self
            .times
            .front()
            .is_some_and(|&t| now - t > self.window_secs)
        {
            self.times.pop_front();
        }
        if self.times.len() >= self.max_count {
            return false;
        }
        self.times.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// Engine helpers
// ---------------------------------------------------------------------------

pub(crate) fn build_builtin_registry(
    workspace_root: &str,
    todo_list: StdArc<ocw_tools::TodoList>,
    provider: StdArc<dyn ocw_provider::Provider>,
    model: &str,
    agent: &str,
    skill_store: &ocw_skills::SkillStore,
    // (store, origin_session_id) — Some only for live sessions. Scheduled-run
    // engines pass None (Python's `_build_task_engine` uses `task_store=None`)
    // so an automation cannot recursively create automations.
    automations: Option<(StdArc<crate::automations::AutomationStore>, String)>,
    // Board/journal tools for team personas (None for scheduled runs / tests).
    board: Option<crate::board_tools::BoardToolsArgs>,
) -> StdArc<ocw_engine::ToolRegistry> {
    let mut reg = ocw_engine::ToolRegistry::new();
    let agent_config = crate::agents::get_agent(agent);
    let context = crate::agents::AgentContext {
        workspace: Some(std::path::PathBuf::from(workspace_root)),
        provider,
        model: model.to_string(),
        todo_list,
    };
    agent_config.register_tools(&mut reg, &context);

    // Register load_skill tool for skill progressive disclosure.
    // Mirrors Python's agent.py: skill_loader = SkillLoader(_skill_dirs(ws));
    //                    registry.register_all(skill_tools(skill_loader));
    let mut skill_dirs = vec![skill_store.global_dir().to_path_buf()];
    let project_dir = skill_store.project_dir_for(std::path::Path::new(workspace_root));
    skill_dirs.push(project_dir); // SkillLoader::new skips non-existent dirs
    let skill_loader = SkillLoader::new(skill_dirs);
    let load_skill_tool = LoadSkillTool::new(
        skill_loader,
        StdArc::new(|| None), // allow all skills; session filtering can be added later
    );
    load_skill_tool.register(&mut reg);

    // Agent-facing scheduling tools (create/list/update/delete scheduled
    // tasks) — only for live knowledge-family sessions that have a workspace,
    // mirroring Python's agent.py gating (`family == "knowledge" and ws`).
    if let Some((store, origin_session_id)) = automations {
        if agent_config.family == "knowledge" && agent_config.needs_workspace {
            crate::automations::register_scheduling_tools(
                &mut reg,
                store,
                crate::automations::SchedulingOrigin {
                    workspace: workspace_root.to_string(),
                    session_id: origin_session_id,
                    surface: agent.to_string(),
                    agent: agent.to_string(),
                },
            );
        }
    }

    if let Some(board_args) = board {
        crate::board_tools::register_board_tools(&mut reg, board_args);
    }

    // Shell executor is managed separately (persistent per-workspace) and registered
    // in init_engine where we have access to the shell executor map.
    StdArc::new(reg)
}

#[allow(dead_code)]
async fn init_engine(state: &AppState, ctx: &SessionCtx) {
    if ctx.run.engine.read().is_some() {
        return;
    }
    let session = state
        .get_session_sync(&ctx.session_id)
        .expect("session must exist");
    let provider = StdArc::clone(&state.provider);
    let workspace = ctx.workspace.as_deref().unwrap_or(".");
    let mut registry = ocw_engine::ToolRegistry::new();
    let agent_config = crate::agents::get_agent(&ctx.agent);
    let context = crate::agents::AgentContext {
        workspace: Some(std::path::PathBuf::from(workspace)),
        provider: StdArc::clone(&provider),
        model: session.model.clone(),
        todo_list: StdArc::clone(&ctx.todo_list),
    };
    agent_config.register_tools(&mut registry, &context);
    // Register load_skill tool for skill progressive disclosure
    let mut skill_dirs = vec![state.skill_store.global_dir().to_path_buf()];
    let project_dir = state.skill_store.project_dir_for(std::path::Path::new(workspace));
    skill_dirs.push(project_dir);
    let skill_loader = SkillLoader::new(skill_dirs);
    LoadSkillTool::new(skill_loader, StdArc::new(|| None)).register(&mut registry);
    // Shell: reuse or create per-workspace executor, register shell tools
    state.register_shell_tools_for_workspace(&mut registry, workspace);
    let registry = StdArc::new(registry);
    let permissions = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
        state.config.data_dir.join("permissions.json"),
    )));
    // Build system prompt messages
    let system_messages = state.build_system_messages(&ctx.agent, workspace, &session.model).await;
    let mut eng = ocw_engine::TurnEngine::new(
        provider,
        registry,
        permissions,
        session.model.clone(),
        12,
        serde_json::Map::new(),
        system_messages,
    )
    .with_audit_sink(make_audit_sink(state.clone()))
    .with_workspace_root(workspace.to_string())
    .with_compaction_settings({
        let settings = state.settings.clone();
        move || settings.compaction_settings_sync()
    })
    .with_context_provider(|| {
        chrono::Local::now()
            .format("Current date: %Y-%m-%d")
            .to_string()
    });
    if let Ok(Some(record)) = state.conversation_store.load(&ctx.session_id) {
        if let Some(raw) = record.compaction {
            if let Some(cs) = ocw_engine::CompactionState::from_value(&raw) {
                eng.set_compaction_state(Some(cs));
            }
        }
    }
    {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert("session_id".into(), serde_json::Value::String(ctx.session_id.clone()));
        ctx_map.insert("agent".into(), serde_json::Value::String(ctx.agent.clone()));
        ctx_map.insert("workspace".into(), serde_json::Value::String(workspace.to_string()));
        eng.set_audit_context(ctx_map);
    }
    *ctx.run.engine.write() = Some(eng);
}

/// Build an `AuditSink` callback that appends tool lifecycle events to the
/// durable audit log. Mirrors Python's `manager.py:432` passing
/// `audit_sink=self.audit_store.append`.
fn make_audit_sink(state: AppState) -> ocw_engine::AuditSink {
    StdArc::new(move |event: serde_json::Map<String, serde_json::Value>| {
        state.audit.append(&event);
    })
}

/// Build an `Approver` callback that routes a permission request through the
/// cross-session Inbox. Mirrors `coworker/inbox.py:inbox_approver` and
/// `coworker/server/manager.py:SessionManager.inbox_approver`:
///
/// 1. Create an `InboxItem` of kind `approval` (idempotent by
///    `(session_id, tool_call_id)`).
/// 2. Mirror the new item to the WS clients as a `permission_required` event so
///    the GUI can render the approval card.
/// 3. Suspend until a surface calls `inbox_store.resolve(item.id, ...)` — the
///    approval message handler does that today, and so will Inbox/Slack/Telegram
///    bindings once they're ported.
/// 4. Map the resolution vocabulary (`once`/`always_tool`/`always_command`/
///    `allow`/`always` → success; `always_task` mints a standing rule on the
///    owning automation task; everything else → `Deny`).
fn make_inbox_approver(
    state: AppState,
    session_id: String,
    persona_id: Option<String>,
    perms: StdArc<tokio::sync::Mutex<ocw_engine::PermissionEngine>>,
) -> Approver {
    StdArc::new(move |request: PermissionRequest| {
        let state = state.clone();
        let session_id = session_id.clone();
        let persona_id = persona_id.clone();
        let perms = StdArc::clone(&perms);
        let req = request.clone();
        Box::pin(async move {
            // Run-session awareness (§25): bind the owning task and compute the
            // standing-rule target so the GUI card can offer "Allow every time".
            // Mirrors Python's `approval_prompt_data`.
            let owning_task = {
                let store = state.automations.read().await;
                store.task_for_run_session(&session_id)
            };
            let task_id = owning_task.as_ref().map(|t| t.id.clone());
            let task_title = owning_task.as_ref().map(|t| t.title.clone());
            let meta = if !req.category.is_empty() {
                Some(serde_json::json!({
                    "category": req.category,
                    "requires_approval": true,
                }))
            } else if matches!(req.tool_name.as_str(), "send_message" | "send_file") {
                Some(serde_json::json!({"requires_approval": true}))
            } else {
                None
            };
            let standing_target = ocw_engine::standing_target_candidate_with(
                &req.tool_name,
                &req.arguments,
                meta.as_ref(),
            );
            let args_value = permission_args_to_value(&req);
            let title = format!("Run `{}`?", req.tool_name);
            let mut parts: Vec<String> = Vec::new();
            if !req.reason.trim().is_empty() {
                parts.push(req.reason.trim().to_string());
            }
            let preview = ocw_data::args_preview(Some(&args_value), 240);
            if !preview.is_empty() {
                parts.push(preview);
            }
            let body = parts.join("\n");
            let inbox_name = state
                .inbox_routing
                .route_for(&session_id, persona_id.as_deref());
            let visibility = if state_unattended(&state, &session_id) {
                VIS_INBOX
            } else {
                VIS_INLINE
            };
            let data = serde_json::json!({
                "tool": req.tool_name,
                "arguments": args_value,
                "task_id": task_id,
                "task_title": task_title,
                "standing_target": standing_target,
            });
            let item = state.inbox_store.add_approval(
                &session_id,
                &title,
                body,
                &inbox_name,
                visibility,
                data,
                req.tool_call_id.as_deref(),
            );
            // Attended sessions: do NOT broadcast a second permission_required.
            // The engine already emits the live card through the event pump; the
            // parked Inbox item is durable-only — a reconnect, a second device,
            // or a restart resolves it via the GUI's getInbox poll, which
            // round-trips item_id back through the approval WS message. Mirrors
            // Python's app.py approver ("park so the answer can also come from
            // the Inbox / a reconnect / after a restart"). Broadcasting a second
            // copy here stacks a duplicate card in the GUI when tool_call_id is
            // empty (dedupe key) — the double-popup bug.
            // Unattended sessions: the GUI suppresses live permission_required
            // events, so broadcast here for the Inbox panel / other clients.
            let args_for_wire = permission_args_to_value(&req);
            if visibility == VIS_INBOX {
                state.broadcast_sync(
                    &session_id,
                    serde_json::json!({
                        "type": "permission_required",
                        "data": {
                            "name": req.tool_name,
                            "arguments": args_for_wire,
                            "reason": req.reason,
                            "category": req.category,
                            "item_id": item.id,
                            "tool_call_id": req.tool_call_id,
                            "standing_target": standing_target,
                        },
                    }),
                );
            }
            // Suspend until a surface resolves the item.
            let resolution = state.inbox_store.wait(&item.id).await;
            if resolution == "always_task" {
                // Mint a standing rule on the owning task ("Allow every time",
                // §25). Mirrors Python's `mint_task_rule`: run session + rule
                // eligibility + dedupe are all re-checked server-side, and the
                // mint result never changes this call's outcome — `approval_outcome`
                // returns ONCE regardless.
                if let Some(mut task) = owning_task {
                    if let Some(target) = standing_target {
                        if task.add_rule(&req.tool_name, &target) {
                            {
                                let store = state.automations.read().await;
                                store.save_task(task.clone());
                            }
                            // Hot-update the live engine so the run's next call
                            // to this target auto-allows.
                            let mut guard = perms.lock().await;
                            guard.add_task_rule(req.tool_name.clone(), target.clone());
                            drop(guard);
                            let mut event = serde_json::Map::new();
                            event.insert("session_id".into(), session_id.clone().into());
                            event.insert("tool".into(), req.tool_name.clone().into());
                            event.insert(
                                "arguments".into(),
                                permission_args_to_value(&req),
                            );
                            event.insert("stage".into(), "standing_rule_minted".into());
                            event.insert("status".into(), "granted".into());
                            event.insert(
                                "reason".into(),
                                format!(
                                    "allow every time: {} → {} (task {})",
                                    req.tool_name, target, task.id
                                )
                                .into(),
                            );
                            state.audit.append(&event);
                        }
                    }
                }
                return Ok(ApprovalOutcome::Once);
            }
            Ok(map_approval_resolution(&resolution))
        })
    })
}

fn permission_args_to_value(req: &PermissionRequest) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in req.arguments.iter() {
        obj.insert(k.clone(), v.clone());
    }
    serde_json::Value::Object(obj)
}

/// Map a free-form approval resolution string (from any surface — live card,
/// Inbox, channel) to a typed `ApprovalOutcome`.
fn map_approval_resolution(resolution: &str) -> ApprovalOutcome {
    match resolution {
        "once" | "allow" => ApprovalOutcome::Once,
        "always_tool" | "always" => ApprovalOutcome::AlwaysTool,
        "always_command" => ApprovalOutcome::AlwaysCommand,
        "always_domain" => ApprovalOutcome::AlwaysDomain,
        "readonly_session" => ApprovalOutcome::ReadonlySession,
        "always_trust" => ApprovalOutcome::AlwaysTrust,
        "this_run" => ApprovalOutcome::ThisRun,
        _ => ApprovalOutcome::Deny,
    }
}

/// Inverse of `map_approval_resolution`: a WS decision vocabulary
/// (`once`/`always_tool`/...) → the canonical inbox resolution string.
fn approval_decision_to_resolution(decision: &str) -> String {
    match decision {
        "once" => "once".to_string(),
        "always_tool" => "always_tool".to_string(),
        "always_command" => "always_command".to_string(),
        "always_domain" => "always_domain".to_string(),
        "readonly_session" => "readonly_session".to_string(),
        "always_trust" => "always_trust".to_string(),
        "this_run" => "this_run".to_string(),
        // "Allow every time" on a run-session approval card (§25): pass through
        // so the approver's `wait` sees it and mints the standing task rule.
        "always_task" => "always_task".to_string(),
        _ => "deny".to_string(),
    }
}

/// Best-effort "is this session unattended?" check used to pick the inbox
/// item's visibility (mirror of `manager.unattended.is_unattended`).
fn state_unattended(state: &AppState, session_id: &str) -> bool {
    state.unattended.is_unattended(session_id)
}

/// Build a `QuestionAsker` callback that routes an `ask_user` tool call through
/// the cross-session Inbox. Mirrors Python's `manager.py:question_asker`:
///
/// 1. Create an `InboxItem` of kind `question`.
/// 2. Broadcast to WS clients as a `question_requested` event with `item_id`.
/// 3. Suspend on `inbox_store.wait(item.id)`.
/// 4. Return the user's answer string.
fn make_inbox_question_asker(
    state: AppState,
    session_id: String,
    persona_id: Option<String>,
) -> ocw_engine::QuestionAsker {
    StdArc::new(move |args: serde_json::Map<String, Value>, tool_call_id: Option<String>| {
        let state = state.clone();
        let session_id = session_id.clone();
        let persona_id = persona_id.clone();
        Box::pin(async move {
            // Mirrors Python `app.py:question_asker` — extract options/header, broadcast
            // once when attended, return `{"answer": ...}` (never a bare string).
            let question = args
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let options: Vec<String> = args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let allow_text = args
                .get("allow_text")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let multi = args.get("multi").and_then(|v| v.as_bool()).unwrap_or(false);
            let header = args
                .get("header")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let title = if question.len() > 80 {
                format!("{}…", &question[..80])
            } else {
                question.clone()
            };
            let inbox_name = state
                .inbox_routing
                .route_for(&session_id, persona_id.as_deref());
            let visibility = if state_unattended(&state, &session_id) {
                ocw_data::VIS_INBOX
            } else {
                ocw_data::VIS_INLINE
            };
            let item = state.inbox_store.add_question(
                &session_id,
                &title,
                question.clone(),
                &inbox_name,
                visibility,
                options.clone(),
                allow_text,
                multi,
                tool_call_id.as_deref(),
            );
            // Durable resume: already-answered prompt returns immediately.
            if item.state != ocw_data::STATE_PENDING {
                return serde_json::json!({
                    "answer": item.resolution.unwrap_or_default(),
                })
                .to_string();
            }
            // Attended sessions get a live WS card; unattended ones stay Inbox-only
            // (mirroring is handled elsewhere / future work).
            if visibility != ocw_data::VIS_INBOX {
                state.broadcast_sync(
                    &session_id,
                    serde_json::json!({
                        "type": "question_requested",
                        "data": {
                            "question": question,
                            "options": options,
                            "allow_text": allow_text,
                            "multi": multi,
                            "header": header,
                            "item_id": item.id,
                            "tool_call_id": tool_call_id,
                        },
                    }),
                );
            }
            let answer = state.inbox_store.wait(&item.id).await;
            serde_json::json!({ "answer": answer }).to_string()
        })
    })
}

/// Build a `DirectoryRequester` callback that routes a `request_directory` tool
/// call through the cross-session Inbox. Mirrors Python's
/// `manager.py:directory_requester`.
fn make_inbox_directory_requester(
    state: AppState,
    session_id: String,
    persona_id: Option<String>,
) -> ocw_engine::DirectoryRequester {
    StdArc::new(move |args: serde_json::Map<String, Value>, tool_call_id: Option<String>| {
        let state = state.clone();
        let session_id = session_id.clone();
        let persona_id = persona_id.clone();
        Box::pin(async move {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let writable = args
                .get("writable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let title = format!("Access `{path}`?");
            let inbox_name = state
                .inbox_routing
                .route_for(&session_id, persona_id.as_deref());
            let visibility = if state_unattended(&state, &session_id) {
                ocw_data::VIS_INBOX
            } else {
                ocw_data::VIS_INLINE
            };
            let data = serde_json::json!({
                "path": path,
                "reason": reason,
                "writable": writable,
            });
            let item = state.inbox_store.add_directory(
                &session_id,
                &title,
                reason.to_string(),
                &inbox_name,
                visibility,
                data,
                tool_call_id.as_deref(),
            );
            // Broadcast to WS clients so the GUI can render the directory card.
            state.broadcast_sync(
                &session_id,
                serde_json::json!({
                    "type": "directory_requested",
                    "data": {
                        "path": path,
                        "reason": reason,
                        "writable": writable,
                        "item_id": item.id,
                        "tool_call_id": tool_call_id,
                    },
                }),
            );
            // Suspend until a surface resolves the item.
            let resolution = state.inbox_store.wait(&item.id).await;
            // Parse resolution back to DirectoryResult.
            match serde_json::from_str::<serde_json::Value>(&resolution) {
                Ok(v) => ocw_engine::DirectoryResult {
                    granted: v.get("granted").and_then(|x| x.as_bool()).unwrap_or(false),
                    path: v
                        .get("path")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    writable: v.get("writable").and_then(|x| x.as_bool()).unwrap_or(false),
                    error: v.get("error").and_then(|x| x.as_str()).map(|s| s.to_string()),
                },
                Err(_) => ocw_engine::DirectoryResult {
                    granted: false,
                    path: String::new(),
                    writable: false,
                    error: Some("failed to parse directory resolution".into()),
                },
            }
        })
    })
}

/// Build a `PlanApprover` callback that routes a `propose_plan` tool call
/// through the cross-session Inbox. Mirrors Python's `manager.py:plan_approver`.
fn make_inbox_plan_approver(
    state: AppState,
    session_id: String,
    persona_id: Option<String>,
) -> ocw_engine::PlanApprover {
    StdArc::new(move |args: serde_json::Map<String, Value>, tool_call_id: Option<String>| {
        let state = state.clone();
        let session_id = session_id.clone();
        let persona_id = persona_id.clone();
        Box::pin(async move {
            let plan = args
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = "Plan proposal".to_string();
            let inbox_name = state
                .inbox_routing
                .route_for(&session_id, persona_id.as_deref());
            let visibility = if state_unattended(&state, &session_id) {
                ocw_data::VIS_INBOX
            } else {
                ocw_data::VIS_INLINE
            };
            let data = serde_json::json!({"plan": plan});
            let item = state.inbox_store.add_plan(
                &session_id,
                &title,
                plan.clone(),
                &inbox_name,
                visibility,
                data,
                tool_call_id.as_deref(),
            );
            // Broadcast to WS clients so the GUI can render the plan card.
            state.broadcast_sync(
                &session_id,
                serde_json::json!({
                    "type": "plan_proposed",
                    "data": {
                        "plan": plan,
                        "item_id": item.id,
                        "tool_call_id": tool_call_id,
                    },
                }),
            );
            // Suspend until a surface resolves the item.
            let resolution = state.inbox_store.wait(&item.id).await;
            // Parse resolution back to PlanResult.
            match serde_json::from_str::<serde_json::Value>(&resolution) {
                Ok(v) => ocw_engine::PlanResult {
                    approved: v.get("approved").and_then(|x| x.as_bool()).unwrap_or(false),
                    mode: v
                        .get("mode")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    feedback: v
                        .get("feedback")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                Err(_) => ocw_engine::PlanResult {
                    approved: false,
                    mode: String::new(),
                    feedback: "failed to parse plan resolution".into(),
                },
            }
        })
    })
}

// ---------------------------------------------------------------------------
// Upgrade handler
// ---------------------------------------------------------------------------

pub async fn ws_session_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let config_token = &state.config.api_token;
    let auth_ok = if config_token.is_empty() {
        true
    } else {
        headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').nth(1).map(str::trim))
            == Some(config_token.as_str())
    };
    eprintln!(
        "WS_AUTH: config_token='{}' auth_ok={}",
        config_token, auth_ok
    );
    if !auth_ok {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let agent = query.agent.unwrap_or_else(|| "code".into());
    // Reuse shared per-session run state from the registry — mirrors Python's
    // `SessionManager.get_engine`. Engine + running/cancel flags persist across
    // WS reconnects so state (engine, grants, permissions, todo list) survives
    // a disconnect.
    let run = state
        .running_engines
        .write()
        .entry(session_id.clone())
        .or_insert_with(|| StdArc::new(crate::state::SessionRunState::new()))
        .clone();
    let ctx = SessionCtx::with_engine(session_id, agent, query.workspace, run);
    ws.on_upgrade(move |socket| handle_socket(socket, state, ctx))
}

// ---------------------------------------------------------------------------
// Main receive loop
// ---------------------------------------------------------------------------

async fn handle_socket(ws: WebSocket, state: AppState, ctx: SessionCtx) {
    let mut ws = ws;
    let mut rate_limiter = RateLimiter::new(RATE_LIMIT_WINDOW_SECS, RATE_LIMIT_COUNT);

    // Lazily create sessions at WS connect time (matches Python server behavior).
    let session =
        state.get_or_create_session(&ctx.session_id, &ctx.agent, ctx.workspace.as_deref(), None);
    let running = *ctx.run.running.read();
    send_ws(&mut ws, "ready", ready_payload(&session, running)).await;

    let mut broadcast_rx = state.register_ws_async(&ctx.session_id).await;
    state.broadcast_sync(
        &ctx.session_id,
        json!({"type": "session_joined", "data": {"session_id": ctx.session_id}}),
    );

    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                match msg {
                    Ok(Message::Text(text)) => {
                        let now = tokio::time::Instant::now().elapsed().as_secs_f64();
                        if !rate_limiter.check(now) {
                            send_ws(&mut ws, "input_rejected",
                                json!({"error": "Too many messages; reconnect and try again."})).await;
                            break;
                        }
                        on_text(&mut ws, &ctx, &state, &text).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Ping(d)) => { let _ = ws.send(Message::Pong(d)).await; }
                    Ok(_) => {}
                }
            }
            msg = broadcast_rx.recv() => {
                if let Ok(msg) = msg {
                    let json_str = serde_json::to_string(&msg).unwrap_or_default();
                    let _ = ws.send(Message::Text(json_str.into())).await;
                }
            }
        }
    }

    state.unregister_ws_async(&ctx.session_id).await;
    state.broadcast_sync(
        &ctx.session_id,
        json!({"type": "session_left", "data": {"session_id": ctx.session_id}}),
    );
}

// ---------------------------------------------------------------------------
// Handle one inbound frame
// ---------------------------------------------------------------------------

async fn on_text(ws: &mut WebSocket, ctx: &SessionCtx, state: &AppState, text: &str) {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        send_ws(ws, "input_rejected", json!({"error": "Invalid JSON"})).await;
        return;
    };
    let Some(type_) = parsed.get("type").and_then(|v| v.as_str()) else {
        send_ws(ws, "input_rejected", json!({"error": "Missing type field"})).await;
        return;
    };

    match type_ {
        "user_message" => match serde_json::from_value::<UserMessage>(parsed) {
            Ok(msg) => {
                on_user_message(
                    ctx.clone(),
                    state.clone(),
                    msg,
                )
                .await;
            }
            Err(_) => {
                send_ws(
                    ws,
                    "input_rejected",
                    json!({"error": "Invalid user_message format"}),
                )
                .await;
            }
        },
        "approval" => {
            if let Ok(msg) = serde_json::from_value::<ApprovalMsg>(parsed) {
                // Drive the Inbox: clients that received a `permission_required`
                // event with an `item_id` round-trip the answer back here so a
                // reconnect / Slack reply / second device sees the same single
                // resolved item. (First responder wins — `resolve` is no-op if
                // already resolved.)
                if let Some(item_id) = msg.item_id.clone().or_else(|| {
                    msg.tool_call_id.clone().and_then(|tcid| {
                        state
                            .inbox_store
                            .for_tool_call(&ctx.session_id, &tcid)
                            .map(|i| i.id)
                    })
                }) {
                    let resolution = msg
                        .resolution
                        .clone()
                        .unwrap_or_else(|| approval_decision_to_resolution(&msg.decision));
                    state.inbox_store.resolve(&item_id, &resolution);
                }
            }
        }
        "directory_response" => {
            if let Ok(msg) = serde_json::from_value::<DirectoryResponse>(parsed) {
                // Mirror of `app.py`'s `directory_requester`: a grant is applied
                // to this session's roots (file tools + permissions see it
                // immediately) before the result goes back to the engine.
                let result = if !msg.granted {
                    ocw_engine::DirectoryResult {
                        granted: false,
                        path: String::new(),
                        writable: false,
                        error: Some("the user declined the request".into()),
                    }
                } else {
                    let path = msg.path.unwrap_or_default();
                    if path.trim().is_empty() {
                        ocw_engine::DirectoryResult {
                            granted: false,
                            path: String::new(),
                            writable: false,
                            error: Some("no directory was provided".into()),
                        }
                    } else {
                        let writable = msg.writable.unwrap_or(false);
                        match state.add_root(&ctx.session_id, &path, writable) {
                            Ok(roots) => {
                                // Echo the canonical path of the matching root
                                // (Python looks up the primary by resolved path).
                                let resolved = std::path::Path::new(&path)
                                    .canonicalize()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| path.clone());
                                let primary = roots.iter().find(|r| r.path == resolved).cloned();
                                ocw_engine::DirectoryResult {
                                    granted: true,
                                    path: primary.map(|r| r.path).unwrap_or(path),
                                    writable,
                                    error: None,
                                }
                            }
                            Err(e) => ocw_engine::DirectoryResult {
                                granted: false,
                                path: String::new(),
                                writable: false,
                                error: Some(e),
                            },
                        }
                    }
                };
                // Resolve the pending Inbox item so the engine's callback unblocks.
                let resolution = serde_json::json!({
                    "granted": result.granted,
                    "path": result.path,
                    "writable": result.writable,
                    "error": result.error,
                })
                .to_string();
                let pend = state.inbox_store.pending(Some(&ctx.session_id));
                if let Some(item) = pend.into_iter().find(|i| i.kind == ocw_data::KIND_DIRECTORY) {
                    state.inbox_store.resolve(&item.id, &resolution);
                }
            }
        }
        "plan_response" => {
            if let Ok(msg) = serde_json::from_value::<PlanResponse>(parsed) {
                // Resolve the pending Inbox item so the engine's callback unblocks.
                let resolution = serde_json::json!({
                    "approved": msg.approved,
                    "mode": msg.mode.unwrap_or_else(|| "interactive".into()),
                    "feedback": msg.feedback.unwrap_or_default(),
                })
                .to_string();
                let pend = state.inbox_store.pending(Some(&ctx.session_id));
                if let Some(item) = pend.into_iter().find(|i| i.kind == ocw_data::KIND_PLAN) {
                    state.inbox_store.resolve(&item.id, &resolution);
                }
            }
        }
        "question_response" => {
            let answer = parsed
                .get("answer")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            // Resolve the pending Inbox item so the engine's callback unblocks.
            let pend = state.inbox_store.pending(Some(&ctx.session_id));
            if let Some(item) = pend.into_iter().find(|i| i.kind == ocw_data::KIND_QUESTION) {
                state.inbox_store.resolve(&item.id, &answer);
            } else {
                tracing::warn!(
                    session_id = %ctx.session_id,
                    "question_response with no pending question item"
                );
            }
        }
        "interrupt" => {
            // Align with Python app.py: only request_interrupt(); mark_idle stays in
            // the turn spawn's finally so a Stop mid-ask_user cannot open a parallel turn.
            if let Ok(mut g) = ctx.run.cancel.lock() {
                *g = true;
            }
            // Engine extracted for the turn still shares this Arc via run.cancel.
            if let Some(eng) = ctx.run.engine.write().as_ref() {
                eng.request_cancel();
            }
        }
        "retry" => {
            on_retry(
                ctx.clone(),
                state.clone(),
            )
            .await;
        }
        "set_mode" | "set_model" => {
            let meta_change: Option<(String, String, crate::state::SessionMeta, bool)> = {
                let sessions = state.sessions.read().unwrap();
                sessions.get(&ctx.session_id).cloned().map(|mut meta| {
                    let mut changed_model = false;
                    if type_ == "set_mode" {
                        if let Some(mode) = parsed.get("mode").and_then(|v| v.as_str()) {
                            meta.mode = mode.to_string();
                        }
                    } else if let Some(model) = parsed.get("model").and_then(|v| v.as_str()) {
                        meta.model = model.to_string();
                        changed_model = true;
                    }
                    (meta.model.clone(), meta.mode.clone(), meta, changed_model)
                })
            };
            if let Some((model, mode, updated_meta, changed_model)) = meta_change {
                // Persist
                let _ =
                    state
                        .conversation_store
                        .update_model_and_mode(&ctx.session_id, &model, &mode);
                // Update in-memory
                state
                    .sessions
                    .write()
                    .unwrap()
                    .insert(ctx.session_id.clone(), updated_meta);
                // Mid-turn model rebind — propagate to a live engine so subsequent
                // turns use the new model without reconnecting (mirrors Python's _apply_model).
                if changed_model {
                    if let Some(eng) = ctx.run.engine.write().as_mut() {
                        if let Some(notice_text) = eng.switch_model(model.clone()) {
                            // Broadcast the model-switch notice to the frontend
                            // so the user sees the transition marker (mirrors Python).
                            state.broadcast_sync(
                                &ctx.session_id,
                                json!({
                                    "type": "model_changed",
                                    "data": {
                                        "session_id": ctx.session_id,
                                        "model": model,
                                        "notice": notice_text,
                                    }
                                }),
                            );
                            return;
                        }
                    }
                }
                send_ws(
                    ws,
                    "model_changed",
                    json!({
                        "session_id": ctx.session_id,
                        "model": model,
                        "mode": mode,
                    }),
                )
                .await;
            }
        }
        _ => {
            send_ws(
                ws,
                "input_rejected",
                json!({"error": format!("unknown type: {type_}")}),
            )
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Run turn
// ---------------------------------------------------------------------------

/// Human-readable form of a caught panic payload (for logs + the GUI error event).
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "unknown panic".to_string()
    }
}

async fn on_user_message(ctx: SessionCtx, state: AppState, msg: UserMessage) {
    let session_id = ctx.session_id.clone();
    let agent = ctx.agent.clone();
    let run = StdArc::clone(&ctx.run);
    let todo_list = StdArc::clone(&ctx.todo_list);
    let text = msg.text.unwrap_or_default();

    if text.len() > MAX_MESSAGE_TEXT_CHARS {
        state.broadcast_sync(&session_id,
            serde_json::json!({"type": "input_rejected", "data": {"error": format!("Message too long ({} > {} chars)", text.len(), MAX_MESSAGE_TEXT_CHARS)}}));
        return;
    }

    // Validate attachments — mirrors Python's `_validate_attachments` guard in app.py.
    if let Some(attachments) = msg.attachments.as_ref() {
        if let Err(reason) = validate_attachments(attachments) {
            state.broadcast_sync(
                &session_id,
                serde_json::json!({"type": "input_rejected", "data": {"error": reason}}),
            );
            return;
        }
    }

    if !{
        let mut r = run.running.write();
        if *r {
            false
        } else {
            *r = true;
            true
        }
    } {
        state.broadcast_sync(&session_id,
            serde_json::json!({"type": "input_rejected", "data": {"error": "This session is already running a turn."}}));
        return;
    }

    if let Ok(mut g) = run.cancel.lock() {
        *g = false;
    }
    // Build OpenAI content-parts (text + image/pdf/text attachments) — mirror
    // of `app.py`'s `content = build_user_content(text, attachments)`.
    let content = crate::attachments::build_user_content(Some(&text), msg.attachments.as_deref());
    let input_preview = crate::attachments::content_to_text(&content, "[image]");
    state.broadcast_sync(
        &session_id,
        json!({"type": "turn_start", "data": {"input": input_preview}}),
    );

    // Initialize engine lazily
    if run.engine.read().is_none() {
        let model = msg
            .model
            .filter(|m| !m.is_empty())
            .or_else(|| {
                state
                    .get_session_sync(&session_id)
                    .map(|s| s.model)
                    .filter(|m| !m.is_empty())
            })
            .unwrap_or_else(|| state.default_model_or_configured());
        let provider = StdArc::clone(&state.provider);
        let workspace = state
            .get_session_sync(&session_id)
            .and_then(|s| s.workspace)
            .unwrap_or_else(|| ".".to_string());
        let team_role = state
            .persona_store
            .get(&agent)
            .and_then(|e| e.team.clone());
        let board_kick_state = state.clone();
        let board_args = crate::board_tools::BoardToolsArgs {
            store: StdArc::clone(&state.board.store),
            journal: StdArc::clone(&state.board.journal),
            team_registry: StdArc::clone(&state.board.registry),
            session_id: session_id.clone(),
            persona: agent.clone(),
            workspace: workspace.clone(),
            team_role,
            on_mutate: Some(StdArc::new(move || {
                crate::team_tick::kick_team_tick(board_kick_state.clone());
            })),
        };
        let registry = build_builtin_registry(
            &workspace,
            StdArc::clone(&todo_list),
            StdArc::clone(&provider),
            &model,
            &agent,
            &state.skill_store,
            Some((state.automations.read().await.clone(), session_id.clone())),
            Some(board_args),
        );
        let permissions = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            state.config.data_dir.join("permissions.json"),
        )));
        // Automation run sessions carry their task's standing allowances across
        // engine (re)builds — mirrors Python's `_seed_task_permissions` in the
        // general engine path. Target-bound rules feed task_rules; name-only
        // legacy entries go to the session allow-list.
        {
            let store = state.automations.read().await;
            if let Some(task) = store.task_for_run_session(&session_id) {
                let mut guard = permissions.lock().await;
                for entry in &task.always_allowed_tools {
                    let (tool, target) = crate::automations::parse_rule(entry);
                    if !tool.is_empty() {
                        if let Some(t) = target {
                            guard.add_task_rule(tool, t);
                        } else {
                            guard.allow_tool_for_session(tool);
                        }
                    }
                }
            }
        }
        let fresh_system = state.build_system_messages(&agent, &workspace, &model).await;
        let history_json = state.list_messages(&session_id).await;
        let has_system = history_json
            .first()
            .and_then(|v| v.get("role"))
            .and_then(|v| v.as_str())
            .map(|r| r == "system")
            .unwrap_or(false);

        let mut messages = Vec::new();
        if !has_system {
            messages.extend(fresh_system);
        }
        for v in history_json {
            if let Ok(msg) = serde_json::from_value::<ocw_engine::Message>(v) {
                messages.push(msg);
            }
        }
        let mut eng = ocw_engine::TurnEngine::new(
            provider,
            registry,
            permissions,
            model,
            12,
            serde_json::Map::new(),
            messages,
        )
        .with_audit_sink(make_audit_sink(state.clone()))
        .with_cancel(StdArc::clone(&run.cancel))
        .with_workspace_root(workspace.clone())
        .with_context_provider(|| {
            chrono::Local::now()
                .format("Current date: %Y-%m-%d")
                .to_string()
        });
        // Restore compaction state from the durable session record when present.
        if let Ok(Some(record)) = state.conversation_store.load(&session_id) {
            if let Some(raw) = record.compaction {
                if let Some(cs) = ocw_engine::CompactionState::from_value(&raw) {
                    eng.set_compaction_state(Some(cs));
                }
            }
        }
        {
            let mut ctx_map = serde_json::Map::new();
            ctx_map.insert("session_id".into(), serde_json::Value::String(session_id.clone()));
            ctx_map.insert("agent".into(), serde_json::Value::String(agent.clone()));
            ctx_map.insert("workspace".into(), serde_json::Value::String(workspace.clone()));
            eng.set_audit_context(ctx_map);
        }
        *run.engine.write() = Some(eng);
    }

    // Take engine from the shared run state and wire callbacks directly (no mpsc).
    // Mirrors Python: the engine's callbacks are Inbox-backed and persist across
    // WS reconnects — the WS handler only needs to resolve the Inbox item.
    let persona_id = state
        .get_session_sync(&session_id)
        .map(|s| s.agent)
        .unwrap_or_else(|| agent.clone());
    let mut eng = match run.engine.write().take() {
        Some(eng) => eng,
        None => {
            *run.running.write() = false;
            return;
        }
    };
    // Share the permission engine handle with the approver so an "Allow every
    // time" mint can hot-update task_rules mid-run (mirrors Python's direct
    // `engine.permissions.task_rules` mutation in `mint_task_rule`).
    let perms = eng.permissions_handle();
    let reviewer_model = state
        .get_session_sync(&session_id)
        .map(|s| s.model)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| state.default_model_or_configured());
    eng = eng
        .with_approver(make_inbox_approver(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
            perms,
        ))
        .with_question_asker(make_inbox_question_asker(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
        ))
        .with_directory_requester(make_inbox_directory_requester(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
        ))
        .with_plan_approver(make_inbox_plan_approver(
            state.clone(),
            session_id.clone(),
            Some(persona_id),
        ))
        .with_is_attended(|| true)
        .with_compaction_settings({
            let settings = state.settings.clone();
            move || settings.compaction_settings_sync()
        })
        .with_cancel(StdArc::clone(&run.cancel));

    // Auto-Approve: only attach the live reviewer when the setting is on
    // (mirrors Python's conditional reviewer wiring).
    if state.settings.auto_approve_sync() {
        eng = eng.with_reviewer(make_session_reviewer(
            StdArc::clone(&state.provider),
            reviewer_model,
        ));
    }

    // Seed the mirror with the full engine history so checkpoints match Python's
    // `manager.save(session_id, engine)` (full messages, not turn deltas only).
    let message_mirror: StdArc<std::sync::RwLock<Vec<ocw_engine::Message>>> =
        StdArc::new(std::sync::RwLock::new(eng.messages().to_vec()));
    let park_state = state.clone();
    let park_sid = session_id.clone();
    eng = eng
        .with_message_mirror(StdArc::clone(&message_mirror))
        .with_park_hook(StdArc::new(move |msgs: &[ocw_engine::Message]| {
            let values: Vec<Value> = msgs
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .collect();
            park_state.persist_engine_messages_inner(&park_sid, &values);
        }));

    let session_id2 = session_id.clone();
    let run2 = StdArc::clone(&run);

    tokio::spawn(async move {
        let (live_tx, live_rx) = std::sync::mpsc::channel::<ocw_engine::Event>();
        eng.set_live_events(Some(live_tx));

        let state_live = state.clone();
        let sid_live = session_id2.clone();
        let mirror = StdArc::clone(&message_mirror);
        let live_pump = tokio::task::spawn_blocking(move || {
            while let Ok(ev) = live_rx.recv() {
                // Skip the engine's own permission_required: the approver closure
                // broadcasts its authoritative copy (with item_id) for the same
                // tool call right after parking the Inbox item. Rebroadcasting the
                // engine's item_id-less copy would render a second approval card.
                if ev.event_type == ocw_engine::EventType::PermissionRequired {
                    continue;
                }
                let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
                state_live.broadcast_sync(&sid_live, wire);

                if matches!(
                    ev.event_type,
                    ocw_engine::EventType::TurnStart
                        | ocw_engine::EventType::DirectoryRequested
                        | ocw_engine::EventType::PlanProposed
                        | ocw_engine::EventType::IterationEnd
                ) {
                    state_live.checkpoint_engine_messages(&sid_live, &mirror);
                }
            }
        });

        let run_result = std::panic::AssertUnwindSafe(eng.run(content, None))
            .catch_unwind()
            .await;
        eng.set_live_events(None);
        let _ = live_pump.await;

        if let Err(payload) = &run_result {
            tracing::error!(
                session_id = %session_id2,
                "Turn panicked: {}",
                panic_message(payload)
            );
            state.broadcast_sync(
                &session_id2,
                json!({"type": "error", "data": {"error": "The turn crashed unexpectedly — the conversation up to this point has been saved."}}),
            );
        }

        let all_messages: Vec<Value> = eng
            .messages()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect();
        state.persist_engine_messages_inner(&session_id2, &all_messages);

        let grants = eng.grants().await;
        state.persist_turn(&session_id2);
        state.persist_grants(&session_id2, &grants);
        let compaction_val = eng.compaction_state().map(|s| s.as_value());
        state.persist_compaction(&session_id2, compaction_val.as_ref());
        state.maybe_autotitle(&session_id2);
        state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
        *run2.engine.write() = Some(eng);
        *run2.running.write() = false;
    });
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

async fn on_retry(ctx: SessionCtx, state: AppState) {
    let session_id = ctx.session_id.clone();
    let run = StdArc::clone(&ctx.run);

    if !{
        let mut r = run.running.write();
        if *r {
            false
        } else {
            *r = true;
            true
        }
    } {
        state.broadcast_sync(&session_id,
            serde_json::json!({"type": "input_rejected", "data": {"error": "Cannot retry while a turn is running."}}));
        return;
    }

    // Take engine from the shared run state and wire callbacks directly (no mpsc).
    let persona_id = state
        .get_session_sync(&session_id)
        .map(|s| s.agent)
        .unwrap_or_else(|| "code".to_string());
    let mut eng = match run.engine.write().take() {
        Some(eng) => eng,
        None => {
            *run.running.write() = false;
            return;
        }
    };
    let perms = eng.permissions_handle();
    eng = eng
        .with_approver(make_inbox_approver(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
            perms,
        ))
        .with_question_asker(make_inbox_question_asker(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
        ))
        .with_directory_requester(make_inbox_directory_requester(
            state.clone(),
            session_id.clone(),
            Some(persona_id.clone()),
        ))
        .with_plan_approver(make_inbox_plan_approver(
            state.clone(),
            session_id.clone(),
            Some(persona_id),
        ))
        .with_cancel(StdArc::clone(&run.cancel));

    let park_state = state.clone();
    let park_sid = session_id.clone();
    eng = eng.with_park_hook(StdArc::new(move |msgs: &[ocw_engine::Message]| {
        let values: Vec<Value> = msgs
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect();
        park_state.persist_engine_messages_inner(&park_sid, &values);
    }));

    let session_id2 = session_id.clone();
    let run2 = StdArc::clone(&run);

    tokio::spawn(async move {
        // Panic guard — same rationale as the normal turn path: cleanup below
        // (engine parking, `running` reset) must always run.
        let retry_result = std::panic::AssertUnwindSafe(eng.retry())
            .catch_unwind()
            .await;
        match retry_result {
            Err(payload) => {
                tracing::error!(
                    session_id = %session_id2,
                    "Retry panicked: {}",
                    panic_message(&payload)
                );
                let all_messages: Vec<Value> = eng
                    .messages()
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or_default())
                    .collect();
                state.persist_engine_messages_inner(&session_id2, &all_messages);
                state.broadcast_sync(
                    &session_id2,
                    json!({"type": "error", "data": {"error": "The retry crashed unexpectedly — the conversation up to this point has been saved."}}),
                );
                state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
            }
            Ok(Ok(events)) => {
                for ev in events {
                    // Same dedupe as the live pump: the approver already broadcast
                    // the authoritative permission_required (with item_id).
                    if ev.event_type == ocw_engine::EventType::PermissionRequired {
                        continue;
                    }
                    let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
                    state.broadcast_sync(&session_id2, wire);
                }
                let all_messages: Vec<Value> = eng
                    .messages()
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or_default())
                    .collect();
                state.persist_engine_messages_inner(&session_id2, &all_messages);
                state.persist_turn(&session_id2);
                state.persist_grants(&session_id2, &eng.grants().await);
                let compaction_val = eng.compaction_state().map(|s| s.as_value());
                state.persist_compaction(&session_id2, compaction_val.as_ref());
                state.maybe_autotitle(&session_id2);
                state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
            }
            Ok(Err(_)) => {
                state.broadcast_sync(
                    &session_id2,
                    json!({"type": "error", "data": {"error": "Cannot retry: no error at tail"}}),
                );
                state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
            }
        }
        *run2.engine.write() = Some(eng);
        *run2.running.write() = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Config;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ocw-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_state(dir: std::path::PathBuf) -> AppState {
        let config = Config {
            data_dir: dir,
            ..Config::default()
        };
        let provider: StdArc<dyn ocw_provider::Provider> =
            StdArc::new(ocw_provider::Router::new("anthropic"));
        AppState::new(config, provider)
    }

    fn sample_task(id: &str) -> crate::automations::ScheduledTask {
        crate::automations::ScheduledTask {
            id: id.to_string(),
            title: "T".to_string(),
            instructions: "do it".to_string(),
            schedule: crate::automations::Schedule::default(),
            workspace: String::new(),
            origin_surface: String::new(),
            origin_session_id: String::new(),
            agent: "cowork".to_string(),
            model: None,
            notify_on_completion: true,
            notify_target: None,
            always_allowed_tools: Vec::new(),
            always_allowed_commands: Vec::new(),
            enabled: true,
            created_at: 0.0,
            updated_at: 0.0,
            next_run: None,
            last_run: None,
            last_status: None,
            run_count: 0,
            max_runs: None,
            seen_runs_at: 0.0,
            task_session_id: format!("__task__{id}"),
        }
    }

    fn make_registry(
        workspace: &std::path::Path,
        agent: &str,
        automations: Option<(StdArc<crate::automations::AutomationStore>, String)>,
    ) -> StdArc<ocw_engine::ToolRegistry> {
        let provider: StdArc<dyn ocw_provider::Provider> =
            StdArc::new(ocw_provider::Router::new("anthropic"));
        build_builtin_registry(
            workspace.to_string_lossy().as_ref(),
            ocw_tools::TodoList::new(),
            provider,
            "test-model",
            agent,
            &ocw_skills::SkillStore::new(workspace.to_path_buf()),
            automations,
            None,
        )
    }

    #[test]
    fn ready_payload_reports_live_turn() {
        // A reconnect can land mid-turn (sidebar revisit, relaunch, dropped socket).
        // `ready` must carry server truth on the running turn or the GUI loses Stop +
        // the waiting row (owner catch 2026-08-24; Python: test_ws_ready_reports_live_turn).
        let meta = crate::state::SessionMeta::new(
            "live1".into(),
            Some("/tmp/ws".into()),
            "code",
            "test-model",
        );
        let idle = ready_payload(&meta, false);
        assert_eq!(idle["session_id"], "live1");
        assert_eq!(idle["running"], false);
        assert_eq!(idle["agent"], "code");
        assert_eq!(idle["workspace"], "/tmp/ws");

        let live = ready_payload(&meta, true);
        assert_eq!(live["running"], true);
        assert_eq!(live["model"], "test-model");
        assert_eq!(live["mode"], "interactive");
    }

    #[test]
    fn approval_decision_to_resolution_passes_always_task() {
        // The live card sends its decision over WS (no `resolution` field);
        // "always_task" must reach the inbox approver as-is, not degrade to
        // "deny" (the mint path depends on it).
        assert_eq!(approval_decision_to_resolution("once"), "once");
        assert_eq!(approval_decision_to_resolution("always_tool"), "always_tool");
        assert_eq!(
            approval_decision_to_resolution("always_command"),
            "always_command"
        );
        assert_eq!(
            approval_decision_to_resolution("always_task"),
            "always_task"
        );
        assert_eq!(approval_decision_to_resolution("deny"), "deny");
    }

    #[test]
    fn scheduling_tools_registered_only_for_knowledge_sessions_with_store() {
        let dir = temp_dir("reg");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let store = StdArc::new(crate::automations::AutomationStore::new(dir.clone()));

        // knowledge family + live store → registered
        let reg = make_registry(
            &ws,
            "cowork",
            Some((StdArc::clone(&store), "sess-1".to_string())),
        );
        for name in [
            "create_scheduled_task",
            "list_scheduled_tasks",
            "update_scheduled_task",
            "delete_scheduled_task",
        ] {
            assert!(reg.contains(name), "cowork must register {name}");
        }

        // knowledge family + no store (scheduler path) → not registered
        let reg = make_registry(&ws, "cowork", None);
        assert!(!reg.contains("create_scheduled_task"));

        // knowledge-family agent without a workspace (chat) + store → not registered
        let reg = make_registry(&ws, "chat", Some((store, "sess-1".to_string())));
        assert!(!reg.contains("create_scheduled_task"));
    }

    #[tokio::test]
    async fn always_task_mints_rule_and_hot_updates_engine() {
        let dir = temp_dir("mint");
        let state = make_state(dir.clone());
        let session_id = "__run__run-xyz".to_string();

        // Set up the owning task + run record.
        {
            let store = state.automations.read().await;
            store.save_task(sample_task("task-abc"));
            store.add_run(crate::automations::TaskRun {
                run_id: "run-xyz".to_string(),
                task_id: "task-abc".to_string(),
                started_at: 1.0,
                finished_at: None,
                status: "running".to_string(),
                result_text: None,
                artifacts: Vec::new(),
                error: None,
                trigger: "manual".to_string(),
                session_id: session_id.clone(),
                model: None,
            });
        }

        let perms = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            dir.join("permissions.json"),
        )));
        let approver = make_inbox_approver(
            state.clone(),
            session_id.clone(),
            None,
            StdArc::clone(&perms),
        );

        let mut args = serde_json::Map::new();
        args.insert(
            "target".into(),
            serde_json::Value::String("alice".into()),
        );
        let request = PermissionRequest {
            tool_name: "send_message".to_string(),
            arguments: args,
            reason: "test".to_string(),
            category: String::new(),
            tool_call_id: None,
        };
        let handle = tokio::spawn(approver(request.clone()));

        // Wait for the parked inbox item, then resolve with "always_task".
        let mut item = None;
        for _ in 0..200 {
            let pending = state.inbox_store.pending(Some(&session_id));
            if let Some(i) = pending.into_iter().next() {
                item = Some(i);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let item = item.expect("approver must park an inbox item");
        // The card carries the task binding + standing target (the GUI gates
        // its "Allow every time" button on exactly these fields).
        assert_eq!(item.data["task_id"], "task-abc");
        assert_eq!(item.data["task_title"], "T");
        assert_eq!(item.data["standing_target"], "alice");
        state.inbox_store.resolve(&item.id, "always_task");

        let outcome = handle.await.expect("approver join").expect("approver result");
        assert!(matches!(outcome, ApprovalOutcome::Once));

        // The task record gained the standing rule.
        let stored = {
            let store = state.automations.read().await;
            store.get("task-abc").unwrap()
        };
        assert_eq!(
            stored.always_allowed_tools,
            vec!["send_message alice".to_string()]
        );

        // The live engine hot-updated: the next call with the same target auto-allows.
        let guard = perms.lock().await;
        let meta = json!({"requires_approval": true, "category": "connector"});
        let d = guard.evaluate("send_message", &json!({"target": "alice"}), Some(&meta));
        assert!(d.allowed, "expected rule hit, got {d:?}");
        assert_eq!(d.rule, "send_message → alice");
    }

    /// Attended sessions: the engine's live permission_required is the ONLY
    /// card. The approver parks the Inbox item for durability but must NOT
    /// broadcast a second copy — a duplicate stacks a second card in the GUI
    /// when the engine's tool_call_id is empty (dedupe key), the double-popup
    /// bug this guards against. Mirrors Python's app.py approver.
    #[tokio::test]
    async fn attended_approver_broadcasts_nothing() {
        let dir = temp_dir("attended");
        let state = make_state(dir.clone());
        let session_id = "sess-attended".to_string();
        // Subscribe BEFORE the approver parks: a tokio broadcast sender drops
        // messages with no receivers, which would false-pass the assertion.
        let mut rx = state.register_ws_async(&session_id).await;
        let perms = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            dir.join("permissions.json"),
        )));
        let approver = make_inbox_approver(
            state.clone(),
            session_id.clone(),
            None,
            StdArc::clone(&perms),
        );

        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::Value::String("out.md".into()));
        let request = PermissionRequest {
            tool_name: "write_file".to_string(),
            arguments: args,
            reason: "test".to_string(),
            category: String::new(),
            tool_call_id: Some("call_1".to_string()),
        };
        let handle = tokio::spawn(approver(request));

        let mut item = None;
        for _ in 0..200 {
            let pending = state.inbox_store.pending(Some(&session_id));
            if let Some(i) = pending.into_iter().next() {
                item = Some(i);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let item = item.expect("approver must park an inbox item");
        assert_eq!(item.visibility, ocw_data::VIS_INLINE);

        let recv = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(
            recv.is_err(),
            "attended approver must not broadcast a second permission_required"
        );

        // Resolve so the spawned approver completes.
        state.inbox_store.resolve(&item.id, "once");
        let outcome = handle.await.expect("approver join").expect("approver result");
        assert!(matches!(outcome, ApprovalOutcome::Once));
    }

    /// Unattended sessions have no live card (the GUI suppresses
    /// permission_required there), so the broadcast stays: it carries the
    /// item_id that routes the approval reply back to this exact Inbox row.
    #[tokio::test]
    async fn unattended_approver_still_broadcasts() {
        let dir = temp_dir("unattended");
        let state = make_state(dir.clone());
        let session_id = "sess-unattended".to_string();
        state.unattended.set(&session_id, true);
        let mut rx = state.register_ws_async(&session_id).await;
        let perms = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            dir.join("permissions.json"),
        )));
        let approver = make_inbox_approver(
            state.clone(),
            session_id.clone(),
            None,
            StdArc::clone(&perms),
        );

        let request = PermissionRequest {
            tool_name: "write_file".to_string(),
            arguments: serde_json::Map::new(),
            reason: "test".to_string(),
            category: String::new(),
            tool_call_id: Some("call_2".to_string()),
        };
        let handle = tokio::spawn(approver(request));

        let mut item = None;
        for _ in 0..200 {
            let pending = state.inbox_store.pending(Some(&session_id));
            if let Some(i) = pending.into_iter().next() {
                item = Some(i);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let item = item.expect("approver must park an inbox item");
        assert_eq!(item.visibility, ocw_data::VIS_INBOX);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("unattended approver must broadcast")
            .expect("broadcast channel closed");
        assert_eq!(msg["type"], "permission_required");
        assert_eq!(msg["data"]["item_id"].as_str(), Some(item.id.as_str()));

        state.inbox_store.resolve(&item.id, "once");
        let outcome = handle.await.expect("approver join").expect("approver result");
        assert!(matches!(outcome, ApprovalOutcome::Once));
    }
}
