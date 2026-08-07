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
use futures_util::StreamExt;
use parking_lot::RwLock as PlRwLock;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::state::AppState;
use ocw_data::VIS_INBOX;
use ocw_data::VIS_INLINE;
use ocw_engine::{ApprovalOutcome, Approver, PermissionRequest};
use ocw_tools;

const RATE_LIMIT_COUNT: usize = 30;
const RATE_LIMIT_WINDOW_SECS: f64 = 10.0;
const MAX_MESSAGE_TEXT_CHARS: usize = 200_000;
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENTS_BYTES: usize = 15_000_000;
const MAX_IMAGE_CHARS: usize = 12_000_000;
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

// ---------------------------------------------------------------------------
// Per-socket session context
// ---------------------------------------------------------------------------

struct SessionCtx {
    session_id: String,
    agent: String,
    workspace: Option<String>,
    engine: StdArc<PlRwLock<Option<ocw_engine::TurnEngine>>>,
    /// Shared with TurnEngine so interrupt stops an in-flight provider/tool loop.
    cancel: StdArc<std::sync::Mutex<bool>>,
    running: StdArc<PlRwLock<bool>>,
    approval_tx: StdArc<PlRwLock<Option<mpsc::Sender<ocw_engine::ApprovalOutcome>>>>,
    question_tx: StdArc<PlRwLock<Option<mpsc::Sender<String>>>>,
    directory_tx: StdArc<PlRwLock<Option<mpsc::Sender<ocw_engine::DirectoryResult>>>>,
    plan_tx: StdArc<PlRwLock<Option<mpsc::Sender<ocw_engine::PlanResult>>>>,
    /// Per-session shared task list; mutated by `todo_write` and read by the GUI.
    todo_list: StdArc<ocw_tools::TodoList>,
}

impl SessionCtx {
    fn new(session_id: String, agent: String, workspace: Option<String>) -> Self {
        Self {
            session_id,
            agent,
            workspace,
            engine: StdArc::new(PlRwLock::new(None)),
            cancel: StdArc::new(std::sync::Mutex::new(false)),
            running: StdArc::new(PlRwLock::new(false)),
            approval_tx: StdArc::new(PlRwLock::new(None)),
            question_tx: StdArc::new(PlRwLock::new(None)),
            directory_tx: StdArc::new(PlRwLock::new(None)),
            plan_tx: StdArc::new(PlRwLock::new(None)),
            todo_list: ocw_tools::TodoList::new(),
        }
    }

    fn mark_idle(&self) {
        *self.running.write() = false;
    }

    #[allow(dead_code)]
    fn try_claim(&self) -> bool {
        let mut r = self.running.write();
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
            engine: StdArc::clone(&self.engine),
            cancel: StdArc::clone(&self.cancel),
            running: StdArc::clone(&self.running),
            approval_tx: StdArc::clone(&self.approval_tx),
            question_tx: StdArc::clone(&self.question_tx),
            directory_tx: StdArc::clone(&self.directory_tx),
            plan_tx: StdArc::clone(&self.plan_tx),
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
    // Shell executor is managed separately (persistent per-workspace) and registered
    // in init_engine where we have access to the shell executor map.
    StdArc::new(reg)
}

#[allow(dead_code)]
fn init_engine(state: &AppState, ctx: &SessionCtx) {
    if ctx.engine.read().is_some() {
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
    // Shell: reuse or create per-workspace executor, register shell tools
    state.register_shell_tools_for_workspace(&mut registry, workspace);
    let registry = StdArc::new(registry);
    let permissions = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
        state.config.data_dir.join("permissions.json"),
    )));
    // Build system prompt messages
    let system_messages = state.build_system_messages(&ctx.agent, workspace, &session.model);
    let eng = ocw_engine::TurnEngine::new(
        provider,
        registry,
        permissions,
        session.model.clone(),
        12,
        serde_json::Map::new(),
        system_messages,
    );
    *ctx.engine.write() = Some(eng);
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
///    `allow`/`always` → success; everything else → `Deny`).
fn make_inbox_approver(
    state: AppState,
    session_id: String,
    persona_id: Option<String>,
) -> Approver {
    StdArc::new(move |request: PermissionRequest| {
        let state = state.clone();
        let session_id = session_id.clone();
        let persona_id = persona_id.clone();
        let req = request.clone();
        Box::pin(async move {
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
            // Mirror to live WS clients — the engine already emits
            // PERMISSION_REQUIRED too, but we attach item_id so the WS handler
            // can route the upcoming approval reply back into the inbox.
            let args_for_wire = permission_args_to_value(&req);
            state.broadcast_sync(
                &session_id,
                serde_json::json!({
                    "type": "permission_required",
                    "data": {
                        "tool": req.tool_name,
                        "arguments": args_for_wire,
                        "reason": req.reason,
                        "category": req.category,
                        "item_id": item.id,
                    },
                }),
            );
            // Suspend until a surface resolves the item.
            let resolution = state.inbox_store.wait(&item.id).await;
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
        _ => "deny".to_string(),
    }
}

/// Best-effort "is this session unattended?" check used to pick the inbox
/// item's visibility (mirror of `manager.unattended.is_unattended`).
fn state_unattended(state: &AppState, session_id: &str) -> bool {
    state.unattended.is_unattended(session_id)
}

/// Take engine from Arc, bind channels, return (engine, channels) tuple.
/// Must be called from a tokio runtime context.
async fn extract_engine_with_callbacks(
    engine_arc: &StdArc<PlRwLock<Option<ocw_engine::TurnEngine>>>,
    state: &AppState,
    session_id: &str,
    persona_id: &str,
) -> Option<(
    ocw_engine::TurnEngine,
    mpsc::Sender<ocw_engine::ApprovalOutcome>,
    mpsc::Sender<String>,
    mpsc::Sender<ocw_engine::DirectoryResult>,
    mpsc::Sender<ocw_engine::PlanResult>,
)> {
    let (approval_tx, approval_rx) = mpsc::channel(1);
    let (question_tx, question_rx) = mpsc::channel(1);
    let (directory_tx, directory_rx) = mpsc::channel(1);
    let (plan_tx, plan_rx) = mpsc::channel(1);

    let mut eng = engine_arc.write().take()?;

    let approver = make_inbox_approver(
        state.clone(),
        session_id.to_string(),
        Some(persona_id.to_string()),
    );
    let cbs = ocw_engine::EngineCallbacks::new();
    cbs.bind_approval(approval_rx).await;
    cbs.bind_question(question_rx).await;
    cbs.bind_directory(directory_rx).await;
    cbs.bind_plan(plan_rx).await;
    eng.reset_callbacks();
    let eng = eng.with_approver(approver).with_callbacks(cbs);

    Some((eng, approval_tx, question_tx, directory_tx, plan_tx))
}

/// Returns (TurnEngine, ApprovalTx, QuestionTx, DirectoryTx, PlanTx)
#[allow(dead_code)]
type EngineWithSenders = (
    ocw_engine::TurnEngine,
    mpsc::Sender<ocw_engine::ApprovalOutcome>,
    mpsc::Sender<String>,
    mpsc::Sender<ocw_engine::DirectoryResult>,
    mpsc::Sender<ocw_engine::PlanResult>,
);

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
    let ctx = SessionCtx::new(session_id, agent, query.workspace);
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
        state.get_or_create_session(&ctx.session_id, &ctx.agent, ctx.workspace.as_deref());
    send_ws(
        &mut ws,
        "ready",
        json!({
            "session_id": ctx.session_id,
            "agent": session.agent,
            "model": session.model,
            "mode": session.mode,
            "workspace": session.workspace,
        }),
    )
    .await;

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
                    ctx.session_id.clone(),
                    ctx.agent.clone(),
                    StdArc::clone(&ctx.engine),
                    StdArc::clone(&ctx.cancel),
                    StdArc::clone(&ctx.running),
                    StdArc::clone(&ctx.todo_list),
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
                let outcome = match msg.decision.as_str() {
                    "once" => ocw_engine::ApprovalOutcome::Once,
                    "always_tool" => ocw_engine::ApprovalOutcome::AlwaysTool,
                    "always_command" => ocw_engine::ApprovalOutcome::AlwaysCommand,
                    _ => ocw_engine::ApprovalOutcome::Deny,
                };
                if let Some(tx) = ctx.approval_tx.write().take() {
                    let _ = tx.blocking_send(outcome);
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
                if let Some(tx) = ctx.directory_tx.write().take() {
                    let _ = tx.blocking_send(result);
                }
            }
        }
        "plan_response" => {
            if let Ok(msg) = serde_json::from_value::<PlanResponse>(parsed) {
                if let Some(tx) = ctx.plan_tx.write().take() {
                    let _ = tx.blocking_send(ocw_engine::PlanResult {
                        approved: msg.approved,
                        mode: msg.mode.unwrap_or_else(|| "interactive".into()),
                        feedback: msg.feedback.unwrap_or_default(),
                    });
                }
            }
        }
        "question_response" => {
            let answer = parsed
                .get("answer")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(tx) = ctx.question_tx.write().take() {
                let _ = tx.blocking_send(answer);
            }
        }
        "interrupt" => {
            if let Ok(mut g) = ctx.cancel.lock() {
                *g = true;
            }
            // Engine extracted for the turn still shares this Arc.
            if let Some(eng) = ctx.engine.write().as_ref() {
                eng.request_cancel();
            }
            ctx.mark_idle();
        }
        "retry" => {
            on_retry(
                ctx.session_id.clone(),
                StdArc::clone(&ctx.engine),
                StdArc::clone(&ctx.running),
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
                    if let Some(eng) = ctx.engine.write().as_mut() {
                        eng.switch_model(model.clone());
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

#[allow(clippy::too_many_arguments)]
async fn on_user_message(
    session_id: String,
    agent: String,
    engine_arc: StdArc<PlRwLock<Option<ocw_engine::TurnEngine>>>,
    cancel_arc: StdArc<std::sync::Mutex<bool>>,
    running_arc: StdArc<PlRwLock<bool>>,
    todo_list: StdArc<ocw_tools::TodoList>,
    state: AppState,
    msg: UserMessage,
) {
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
        let mut r = running_arc.write();
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

    if let Ok(mut g) = cancel_arc.lock() {
        *g = false;
    }
    // Build OpenAI content-parts (text + image/pdf/text attachments) — mirror
    // of `app.py`'s `content = build_user_content(text, attachments)`.
    let content = crate::attachments::build_user_content(Some(&text), msg.attachments.as_deref());
    let input_preview = crate::attachments::content_to_text(&content, "[image]");
    state.push_message_sync(
        &session_id,
        json!({"role": "user", "content": content.clone()}),
    );
    state.broadcast_sync(
        &session_id,
        json!({"type": "turn_start", "data": {"input": input_preview}}),
    );

    // Initialize engine lazily
    if engine_arc.read().is_none() {
        let model = msg
            .model
            .unwrap_or_else(|| state.default_model_or_configured());
        let provider = StdArc::clone(&state.provider);
        let workspace = state
            .get_session_sync(&session_id)
            .and_then(|s| s.workspace)
            .unwrap_or_else(|| ".".to_string());
        let registry = build_builtin_registry(
            &workspace,
            StdArc::clone(&todo_list),
            StdArc::clone(&provider),
            &model,
            &agent,
        );
        let permissions = StdArc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            state.config.data_dir.join("permissions.json"),
        )));
        // Build system prompt messages (agent role + memory + skills + workspace + date)
        let system_messages = state.build_system_messages(&agent, &workspace, &model);
        let eng = ocw_engine::TurnEngine::new(
            provider,
            registry,
            permissions,
            model,
            12,
            serde_json::Map::new(),
            system_messages,
        )
        .with_cancel(StdArc::clone(&cancel_arc));
        *engine_arc.write() = Some(eng);
    }

    // Extract engine and set up channels synchronously
    let persona_id = state
        .get_session_sync(&session_id)
        .map(|s| s.agent)
        .unwrap_or_else(|| agent.clone());
    let Some((eng, _approval_tx, _question_tx, _directory_tx, _plan_tx)) =
        extract_engine_with_callbacks(&engine_arc, &state, &session_id, &persona_id).await
    else {
        return;
    };
    let mut eng = eng.with_cancel(StdArc::clone(&cancel_arc));

    let session_id2 = session_id.clone();
    let running_arc2 = StdArc::clone(&running_arc);
    let engine_arc2 = StdArc::clone(&engine_arc);

    tokio::spawn(async move {
        let mut assistant_text = String::new();
        let (live_tx, live_rx) = std::sync::mpsc::channel::<ocw_engine::Event>();
        eng.set_live_events(Some(live_tx));

        let state_live = state.clone();
        let sid_live = session_id2.clone();
        let live_pump = tokio::task::spawn_blocking(move || {
            while let Ok(ev) = live_rx.recv() {
                let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
                state_live.broadcast_sync(&sid_live, wire);
            }
        });

        let events = eng.run(content, None).await;
        eng.set_live_events(None);
        let _ = live_pump.await;

        for ev in events {
            let wire = serde_json::to_value(ev).unwrap_or(json!({}));
            // Persistence only — live pump already broadcast turn events.
            match wire.get("type").and_then(|v| v.as_str()) {
                Some("assistant_delta") => {
                    if let Some(s) = wire
                        .get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        assistant_text.push_str(s);
                    }
                }
                Some("assistant_message") => {
                    if let Some(s) = wire
                        .get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        assistant_text.push_str(s);
                    }
                    if !assistant_text.is_empty() {
                        state.push_message_sync(
                            &session_id2,
                            json!({
                                "role": "assistant",
                                "content": std::mem::take(&mut assistant_text),
                            }),
                        );
                    }
                }
                _ => {}
            }
        }
        state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
        *engine_arc2.write() = Some(eng);
        *running_arc2.write() = false;
    });
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

async fn on_retry(
    session_id: String,
    engine_arc: StdArc<PlRwLock<Option<ocw_engine::TurnEngine>>>,
    running_arc: StdArc<PlRwLock<bool>>,
    state: AppState,
) {
    if !{
        let mut r = running_arc.write();
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

    // Extract engine and set up channels synchronously
    let persona_id = state
        .get_session_sync(&session_id)
        .map(|s| s.agent)
        .unwrap_or_else(|| "code".to_string());
    let Some((eng, _approval_tx, _question_tx, _directory_tx, _plan_tx)) =
        extract_engine_with_callbacks(&engine_arc, &state, &session_id, &persona_id).await
    else {
        return;
    };

    let session_id2 = session_id.clone();
    let running_arc2 = StdArc::clone(&running_arc);
    let engine_arc2 = StdArc::clone(&engine_arc);

    tokio::spawn(async move {
        let mut eng = eng;
        let mut assistant_text = String::new();
        match eng.retry().await {
            Ok(events) => {
                for ev in events {
                    let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
                    state.broadcast_sync(&session_id2, wire.clone());
                    if let Some(data) = wire.get("data") {
                        match data.get("event_type").and_then(|v| v.as_str()) {
                            Some("AssistantDelta") => {
                                if let Some(s) = data.get("text").and_then(|v| v.as_str()) {
                                    assistant_text.push_str(s);
                                }
                            }
                            Some("AssistantMessage") => {
                                if let Some(s) = data.get("text").and_then(|v| v.as_str()) {
                                    assistant_text.push_str(s);
                                }
                                state.push_message_sync(
                                    &session_id2,
                                    json!({
                                        "role": "assistant",
                                        "content": std::mem::take(&mut assistant_text),
                                    }),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
            }
            Err(_) => {
                state.broadcast_sync(
                    &session_id2,
                    json!({"type": "error", "data": {"error": "Cannot retry: no error at tail"}}),
                );
                state.broadcast_sync(&session_id2, json!({"type": "turn_done", "data": {}}));
            }
        }
        *engine_arc2.write() = Some(eng);
        *running_arc2.write() = false;
    });
}
