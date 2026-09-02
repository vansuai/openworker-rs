//! TurnEngine — the owned agent loop.

use crate::events::Event;
use crate::permissions::{Mode, PermissionEngine};
use crate::tool_registry::ToolRegistry;
use crate::tool_types::{Error as ToolError, ToolResult};
use crate::types::{Message, ToolCall};
use ocw_provider::{friendly_model_error, AssistantTurn, Error as ProviderError, Provider, StreamEvent};
use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("cannot retry: no error at tail")]
    CannotRetry,
}

fn provider_error_message(model: &str, e: &ProviderError) -> String {
    let raw = e.to_string();
    friendly_model_error(model, &raw).unwrap_or(raw)
}

/// What an approval callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Once,
    AlwaysTool,
    AlwaysCommand,
    Deny,
}

/// A permission request delivered to the approver callback.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub arguments: Map<String, Value>,
    pub reason: String,
    pub category: String,
    pub tool_call_id: Option<String>,
}

/// Result of a directory request.
#[derive(Debug, Clone)]
pub struct DirectoryResult {
    pub granted: bool,
    pub path: String,
    pub writable: bool,
    /// Human-readable reason for a denial (declined / invalid path / not a directory).
    pub error: Option<String>,
}

/// Result of a plan approval.
#[derive(Debug, Clone)]
pub struct PlanResult {
    pub approved: bool,
    pub mode: String,
    pub feedback: String,
}

/// The approver callback type — async, called when a tool needs user approval.
#[allow(clippy::type_complexity)]
pub type Approver = Arc<
    dyn Fn(
            PermissionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ApprovalOutcome, tokio::task::JoinError>> + Send>>
        + Send
        + Sync,
>;

/// ask_user callback — mirrors Python's `question_asker`.
/// Called when the model invokes `ask_user`; args are the tool arguments,
/// tool_call_id is the provider-assigned call id.
#[allow(clippy::type_complexity)]
pub type QuestionAsker = Arc<
    dyn Fn(Map<String, Value>, Option<String>) -> Pin<Box<dyn Future<Output = String> + Send>>
        + Send
        + Sync,
>;

/// Directory request callback — mirrors Python's `directory_requester`.
#[allow(clippy::type_complexity)]
pub type DirectoryRequester = Arc<
    dyn Fn(
            Map<String, Value>,
            Option<String>,
        ) -> Pin<Box<dyn Future<Output = DirectoryResult> + Send>>
        + Send
        + Sync,
>;

/// Plan approval callback — mirrors Python's `plan_approver`.
#[allow(clippy::type_complexity)]
pub type PlanApprover = Arc<
    dyn Fn(Map<String, Value>, Option<String>) -> Pin<Box<dyn Future<Output = PlanResult> + Send>>
        + Send
        + Sync,
>;

/// Audit sink — called for every tool lifecycle event (proposed / started /
/// finished / interrupted / filtered). Mirrors Python's `audit_sink` callback.
pub type AuditSink = Arc<dyn Fn(serde_json::Map<String, Value>) + Send + Sync>;

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Monotonic sequence for fallback tool-call ids (see `fallback_tool_call_id`).
static TOOL_CALL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique id for a tool call that arrived without one. Some provider parsers
/// (Bedrock / Anthropic conversion / Vertex) fall back to an empty id; the GUI
/// dedupes approval events by tool_call_id and the server routes the approval
/// reply back to the Inbox item by tool_call_id, so an empty id breaks both.
fn fallback_tool_call_id() -> String {
    let seq = TOOL_CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("tc_{}_{}", (now_ts() * 1e6) as u64, seq)
}

/// Python `_handle_ask_user` status: ok iff `answer` is non-empty.
fn ask_user_status(result: &str) -> &'static str {
    if let Ok(v) = serde_json::from_str::<Value>(result) {
        if let Some(answer) = v.get("answer").and_then(|a| a.as_str()) {
            if !answer.is_empty() {
                return "ok";
            }
        }
        return "denied";
    }
    if result.trim().is_empty() {
        "denied"
    } else {
        "ok"
    }
}

/// Compact preview for TOOL_FINISHED — mirrors Python `_preview`.
fn tool_result_preview(result: &str) -> String {
    let text = result.replace('\n', "\\n");
    if text.len() <= 300 {
        text
    } else {
        format!("{}...", &text[..297])
    }
}

/// Truncate to at most `max_chars` characters without splitting a multi-byte
/// UTF-8 sequence. Tool output regularly carries CJK text; a byte-offset slice
/// into it panics and would kill the whole turn task (owner-hit 2026-08-08: a
/// weather report's box-drawing chars crashed `preview()`).
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn preview(value: &Value, max_chars: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    };
    let text = text.replace('\n', "\\n");
    if text.chars().count() <= max_chars {
        text
    } else {
        format!("{}...", truncate_chars(&text, max_chars.saturating_sub(3)))
    }
}

// ---------------------------------------------------------------------------
// TurnEngine
// ---------------------------------------------------------------------------

pub struct TurnEngine {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    /// tokio::sync::Mutex so the guard is Send (needed for tokio::spawn).
    permissions: Arc<tokio::sync::Mutex<PermissionEngine>>,
    model: String,
    max_iterations: usize,
    model_settings: Map<String, Value>,
    messages: Vec<Message>,
    approver: Approver,
    question_asker: QuestionAsker,
    directory_requester: DirectoryRequester,
    plan_approver: PlanApprover,
    /// tokio::sync::Mutex so the guard is Send.
    standing_notes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Shared with the WS layer so `interrupt` can stop an in-flight turn.
    cancel: Arc<std::sync::Mutex<bool>>,
    /// Optional live sink — events are forwarded as they are produced (for WS streaming).
    live_tx: Option<std::sync::mpsc::Sender<Event>>,
    /// Per-turn context provider: called before each provider request; result is
    /// injected as a `<system-context>` block into the last user message (ephemeral,
    /// never persisted). Mirrors Python's `TurnEngine.context_provider`.
    context_provider: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// Audit sink — called for every tool lifecycle event. Mirrors Python's
    /// `TurnEngine.audit_sink`.
    audit_sink: Option<AuditSink>,
    /// Per-session audit context (session_id, agent, workspace). Merged into every
    /// audit event. Mirrors Python's `TurnEngine.audit_context`.
    audit_context: Map<String, Value>,
    /// Shared message mirror — when set, every `push_msg` also writes to this
    /// `Arc<RwLock<Vec<Message>>>` so an external observer (e.g. the live event
    /// pump) can read the turn's current messages for mid-turn checkpoint
    /// persistence. Mirrors Python's on-demand `manager.save(session_id, engine)`.
    pub message_mirror: Option<Arc<RwLock<Vec<Message>>>>,
    /// Called before parking on an Inbox wait (ask_user / etc.) so the pending
    /// tool call survives a crash — mirrors Python's `manager.persist_session`.
    park_hook: Option<Arc<dyn Fn(&[Message]) + Send + Sync>>,
}

impl TurnEngine {
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        permissions: Arc<tokio::sync::Mutex<PermissionEngine>>,
        model: String,
        max_iterations: usize,
        model_settings: Map<String, Value>,
        messages: Vec<Message>,
    ) -> Self {
        Self {
            provider,
            registry,
            permissions,
            model,
            max_iterations,
            model_settings,
            messages,
            approver: Arc::new(|_| Box::pin(async { Ok(ApprovalOutcome::Deny) })),
            question_asker: Arc::new(|_args, _tool_call_id| {
                Box::pin(async {
                    serde_json::json!({
                        "answer": "",
                        "error": "ask_user not available (no callback set)",
                    })
                    .to_string()
                })
            }),
            directory_requester: Arc::new(|_args, _tool_call_id| {
                Box::pin(async {
                    DirectoryResult {
                        granted: false,
                        path: String::new(),
                        writable: false,
                        error: Some("directory request not available (no callback set)".into()),
                    }
                })
            }),
            plan_approver: Arc::new(|_args, _tool_call_id| {
                Box::pin(async {
                    PlanResult {
                        approved: false,
                        mode: String::new(),
                        feedback: "plan approval not available (no callback set)".into(),
                    }
                })
            }),
            standing_notes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            cancel: Arc::new(std::sync::Mutex::new(false)),
            live_tx: None,
            context_provider: None,
            audit_sink: None,
            audit_context: Map::new(),
            message_mirror: None,
            park_hook: None,
        }
    }

    /// Set a per-turn context provider. Its return value is injected as a
    /// `<system-context>` block into the last user message before every provider
    /// request (ephemeral, never persisted).
    pub fn with_context_provider(
        mut self,
        f: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.context_provider = Some(Arc::new(f));
        self
    }

    /// Set the audit sink. Called for every tool lifecycle event.
    pub fn with_audit_sink(mut self, sink: AuditSink) -> Self {
        self.audit_sink = Some(sink);
        self
    }

    /// Set per-session audit context (session_id, agent, workspace).
    /// Merged into every audit event.
    pub fn set_audit_context(&mut self, ctx: Map<String, Value>) {
        self.audit_context = ctx;
    }

    /// Attach a shared message mirror so an external observer (the live event
    /// pump) can read messages mid-turn for checkpoint persistence. Every
    /// `push_msg` call clones the message into the mirror.
    pub fn with_message_mirror(mut self, mirror: Arc<RwLock<Vec<Message>>>) -> Self {
        self.message_mirror = Some(mirror);
        self
    }

    /// Persist hook invoked before parking on Inbox waits (ask_user). Mirrors
    /// Python's `manager.persist_session(session_id)` inside the question asker.
    pub fn with_park_hook(mut self, hook: Arc<dyn Fn(&[Message]) + Send + Sync>) -> Self {
        self.park_hook = Some(hook);
        self
    }

    /// Replace the cancel flag with an externally-owned handle (WS session cancel).
    pub fn with_cancel(mut self, cancel: Arc<std::sync::Mutex<bool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Share the cancel flag so callers can interrupt without holding the engine.
    pub fn cancel_handle(&self) -> Arc<std::sync::Mutex<bool>> {
        Arc::clone(&self.cancel)
    }

    /// Request cooperative cancellation of the current turn.
    pub fn request_cancel(&self) {
        if let Ok(mut g) = self.cancel.lock() {
            *g = true;
        }
    }

    /// Forward events to a live consumer as they are produced (in addition to the return vec).
    pub fn with_live_events(mut self, tx: std::sync::mpsc::Sender<Event>) -> Self {
        self.live_tx = Some(tx);
        self
    }

    pub fn set_live_events(&mut self, tx: Option<std::sync::mpsc::Sender<Event>>) {
        self.live_tx = tx;
    }

    pub fn with_approver(mut self, approver: Approver) -> Self {
        self.approver = approver;
        self
    }

    /// Share the permission engine handle (the same Arc the engine evaluates
    /// against) so the service layer can hot-mint task rules mid-run
    /// ("Allow every time", §25) and have them take effect on the next call.
    pub fn permissions_handle(&self) -> Arc<tokio::sync::Mutex<PermissionEngine>> {
        Arc::clone(&self.permissions)
    }

    /// Set the question asker callback (Inbox-backed in server mode).
    pub fn with_question_asker(mut self, f: QuestionAsker) -> Self {
        self.question_asker = f;
        self
    }

    /// Set the directory requester callback (Inbox-backed in server mode).
    pub fn with_directory_requester(mut self, f: DirectoryRequester) -> Self {
        self.directory_requester = f;
        self
    }

    /// Set the plan approver callback (Inbox-backed in server mode).
    pub fn with_plan_approver(mut self, f: PlanApprover) -> Self {
        self.plan_approver = f;
        self
    }

    /// Set the model mid-session (model switch). Returns the notice text if a
    /// model-switch notice was appended to history, or None if nothing changed.
    pub fn switch_model(&mut self, model: String) -> Option<String> {
        if model == self.model {
            return None;
        }
        let had_history = self
            .messages
            .iter()
            .any(|m| !matches!(m, Message::System { .. }));
        self.model = model.clone();
        if had_history {
            let text = format!("Model switched to {model}");
            self.push_msg(Message::notice(
                "model_switch",
                Some(text.clone()),
                now_ts(),
            ));
            Some(text)
        } else {
            None
        }
    }

    /// The current conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Push a message into the engine's history, also cloning into the shared
    /// `message_mirror` (when set) so an external observer can read mid-turn.
    fn push_msg(&mut self, msg: Message) {
        if let Some(mirror) = &self.message_mirror {
            if let Ok(mut guard) = mirror.write() {
                guard.push(msg.clone());
            }
        }
        self.messages.push(msg);
    }

    /// Snapshot of the current session grants (tools + commands).
    pub async fn grants(&self) -> Value {
        self.permissions.lock().await.grants()
    }

    fn emit(&self, events: &mut Vec<Event>, ev: Event) {
        if let Some(tx) = &self.live_tx {
            let _ = tx.send(ev.clone());
        }
        events.push(ev);
    }

    /// Push a new user message and run the turn.
    pub async fn run(&mut self, user_input: Value, source: Option<Value>) -> EngineEvents {
        let msg = Message::User {
            content: user_input,
            ts: Some(now_ts()),
            source,
        };
        self.push_msg(msg);
        EngineEvents::new(self.run_loop().await)
    }

    /// Re-run after a provider error (only valid if the last message is an error notice).
    pub async fn retry(&mut self) -> Result<EngineEvents, Error> {
        let is_error = self
            .messages
            .last()
            .map(|m| matches!(m, Message::Notice { kind, .. } if kind == "error"))
            .unwrap_or(false);
        if !is_error {
            return Err(Error::CannotRetry);
        }
        Ok(EngineEvents::new(self.run_loop().await))
    }

    // ---------------------------------------------------------------------------
    // Main loop
    // ---------------------------------------------------------------------------

    async fn run_loop(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut iterations: usize = 0;
        // Reset cancel at the start of each turn; interrupt sets this mid-turn.
        if let Ok(mut g) = self.cancel.lock() {
            *g = false;
        }
        let cancel = Arc::clone(&self.cancel);
        let live_tx = self.live_tx.clone();

        self.emit(&mut events, Event::turn_start(serde_json::json!(())));

        loop {
            if iterations >= self.max_iterations {
                self.emit(
                    &mut events,
                    Event::assistant_message(
                        Some("I wasn't able to complete this reply — the agent kept calling tools without converging. Please try a more specific prompt, or break this into smaller pieces.".to_string()),
                        vec![],
                        None,
                        None,
                    ),
                );
                self.emit(
                    &mut events,
                    Event::turn_end("max_iterations_exceeded".into(), iterations),
                );
                break;
            }
            iterations += 1;

            let model = self.model.clone();
            let messages = self.outbound_messages();
            let settings: Value = Value::Object(self.model_settings.clone());
            let schemas = self.registry.schemas();
            let tools: Option<Vec<Value>> = if schemas.is_empty() {
                None
            } else {
                Some(
                    schemas
                        .into_iter()
                        .map(|s| serde_json::to_value(&s).unwrap_or(Value::Null))
                        .collect(),
                )
            };

            let cancel2 = Arc::clone(&cancel);
            let provider = Arc::clone(&self.provider);
            let live_tx2 = live_tx.clone();

            let result = tokio::task::spawn_blocking(move || {
                let mut stream = match provider.stream(&model, messages, tools, settings) {
                    Ok(s) => s,
                    Err(e) => return StreamResult::Err {
                        error: e,
                        streamed_text: String::new(),
                        streamed_reasoning: String::new(),
                    },
                };
                let mut events_out = Vec::new();
                let mut streamed_text = String::new();
                let mut streamed_reasoning = String::new();
                let mut turn: Option<AssistantTurn> = None;

                let push = |events_out: &mut Vec<Event>,
                            live: &Option<std::sync::mpsc::Sender<Event>>,
                            ev: Event| {
                    if let Some(tx) = live {
                        let _ = tx.send(ev.clone());
                    }
                    events_out.push(ev);
                };

                loop {
                    if *cancel2.lock().unwrap() {
                        break;
                    }
                    match stream.next() {
                        Some(Ok(StreamEvent::TextDelta { text })) => {
                            streamed_text.push_str(&text);
                            push(
                                &mut events_out,
                                &live_tx2,
                                Event::assistant_delta(text),
                            );
                        }
                        Some(Ok(StreamEvent::ReasoningDelta { reasoning })) => {
                            streamed_reasoning.push_str(&reasoning);
                            push(
                                &mut events_out,
                                &live_tx2,
                                Event::reasoning_delta(reasoning),
                            );
                        }
                        Some(Ok(StreamEvent::Turn { turn: t })) => {
                            turn = Some(t);
                            break;
                        }
                        Some(Err(e)) => {
                            let msg = provider_error_message(&model, &e);
                            push(
                                &mut events_out,
                                &live_tx2,
                                Event::error(msg, "ProviderError".into()),
                            );
                            return StreamResult::Err {
                                error: e,
                                streamed_text,
                                streamed_reasoning,
                            };
                        }
                        None => break,
                    }
                }

                if *cancel2.lock().unwrap() {
                    if !streamed_text.is_empty() || !streamed_reasoning.is_empty() {
                        push(
                            &mut events_out,
                            &live_tx2,
                            Event::assistant_message(
                                Some(streamed_text.clone()),
                                vec![],
                                if streamed_reasoning.is_empty() {
                                    None
                                } else {
                                    Some(streamed_reasoning.clone())
                                },
                                None,
                            ),
                        );
                    }
                    push(
                        &mut events_out,
                        &live_tx2,
                        Event::interrupted(iterations),
                    );
                    return StreamResult::Interrupted {
                        events: events_out,
                        streamed_text,
                        streamed_reasoning,
                    };
                }

                let turn = turn.unwrap_or_else(|| AssistantTurn {
                    text: None,
                    tool_calls: vec![],
                    finish_reason: None,
                    reasoning: None,
                    usage: None,
                });

                push(
                    &mut events_out,
                    &live_tx2,
                    Event::assistant_message(
                        turn.text.clone(),
                        turn.tool_calls.iter().map(|tc| tc.name.clone()).collect(),
                        turn.reasoning.clone(),
                        turn.usage.as_ref().map(|u| serde_json::json!(u)),
                    ),
                );

                StreamResult::Ok {
                    events: events_out,
                    turn,
                }
            })
            .await
            .unwrap_or_else(|e| StreamResult::Err {
                error: ProviderError::Other(e.to_string()),
                streamed_text: String::new(),
                streamed_reasoning: String::new(),
            });

            match result {
                StreamResult::Err {
                    error: e,
                    streamed_text,
                    streamed_reasoning,
                } => {
                    // Persist the partial assistant message the user watched arrive
                    // (mirrors Python engine.py:334-335), then append an error notice
                    // so retry() can find it (mirrors Python engine.py:343).
                    if !streamed_text.is_empty() || !streamed_reasoning.is_empty() {
                        self.push_msg(Message::assistant(
                            streamed_text,
                            vec![],
                            if streamed_reasoning.is_empty() {
                                None
                            } else {
                                Some(streamed_reasoning)
                            },
                            None,
                            now_ts(),
                        ));
                    }
                    self.push_msg(Message::notice(
                        "error",
                        Some(provider_error_message(&self.model, &e)),
                        now_ts(),
                    ));
                    self.emit(
                        &mut events,
                        Event::error(
                            provider_error_message(&self.model, &e),
                            "ProviderError".into(),
                        ),
                    );
                    break;
                }
                StreamResult::Interrupted {
                    events: evs,
                    streamed_text,
                    streamed_reasoning,
                } => {
                    // Persist the partial assistant message the user watched arrive
                    // and an interrupted notice (mirrors Python engine.py:348-350).
                    if !streamed_text.is_empty() || !streamed_reasoning.is_empty() {
                        self.push_msg(Message::assistant(
                            streamed_text,
                            vec![],
                            if streamed_reasoning.is_empty() {
                                None
                            } else {
                                Some(streamed_reasoning)
                            },
                            None,
                            now_ts(),
                        ));
                    }
                    self.push_msg(Message::notice(
                        "interrupted",
                        None,
                        now_ts(),
                    ));
                    // Already live-emitted inside spawn_blocking; only collect.
                    for e in evs {
                        events.push(e);
                    }
                    break;
                }
                StreamResult::Ok {
                    events: stream_events,
                    turn,
                } => {
                    // Already live-emitted inside spawn_blocking; only collect.
                    for e in stream_events {
                        events.push(e);
                    }

                    let tool_calls_vec = turn.tool_calls;
                    let turn_text = turn.text.unwrap_or_default();
                    let turn_reasoning = turn.reasoning;
                    let turn_usage = turn.usage.as_ref().map(|u| serde_json::json!(u));
                    let tool_calls_for_message: Vec<_> = tool_calls_vec
                        .iter()
                        .map(|tc| ToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        })
                        .collect();
                    let tool_calls_for_handler: Vec<_> = tool_calls_vec
                        .into_iter()
                        .map(|tc| ocw_provider::ToolCall {
                            id: tc.id,
                            name: tc.name,
                            arguments: tc.arguments,
                        })
                        .collect();

                    // Persist every successful assistant turn before deciding whether
                    // to continue into tools. The final no-tool answer is just as
                    // authoritative as an assistant message that proposes tools.
                    self.push_msg(Message::assistant(
                        turn_text,
                        tool_calls_for_message,
                        turn_reasoning,
                        turn_usage,
                        now_ts(),
                    ));

                    if tool_calls_for_handler.is_empty() {
                        self.emit(
                            &mut events,
                            Event::turn_end("completed".into(), iterations),
                        );
                        break;
                    }

                    let tool_events = self
                        .handle_tool_calls(tool_calls_for_handler, Arc::clone(&cancel))
                        .await;
                    for e in tool_events {
                        self.emit(&mut events, e);
                    }

                    self.emit(&mut events, Event::iteration_end(iterations));

                    if *cancel.lock().unwrap() {
                        self.emit(&mut events, Event::interrupted(iterations));
                        break;
                    }
                }
            }
        }

        events
    }

    // ---------------------------------------------------------------------------
    // Audit
    // ---------------------------------------------------------------------------

    /// Record a tool lifecycle event (proposed / started / finished /
    /// interrupted / filtered). Best-effort: errors are silently swallowed.
    /// Mirrors Python's `TurnEngine._audit`.
    fn audit(&self, tool_call: &ocw_provider::ToolCall, extra: Map<String, Value>) {
        let sink = match &self.audit_sink {
            Some(s) => s,
            None => return,
        };

        let mut payload = self.audit_context.clone();
        payload.insert("tool".into(), serde_json::Value::String(tool_call.name.clone()));
        payload.insert(
            "arguments".into(),
            tool_call.arguments.clone(),
        );
        for (k, v) in extra {
            payload.insert(k, v);
        }

        // Best-effort — never crash the turn for an audit write.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink(payload);
        }));
    }

    // ---------------------------------------------------------------------------
    // Tool call handling
    // ---------------------------------------------------------------------------

    /// Returns true if the tool call should proceed (was allowed after any prompt).
    /// If the tool needs user approval and a channel is set, this awaits the response.
    /// Emits PermissionRequired events when awaiting.
    async fn authorize_tool(
        &mut self,
        tool_call: &ocw_provider::ToolCall,
        events: &mut Vec<Event>,
    ) -> bool {
        let spec = self.registry.get_spec(&tool_call.name);
        let metadata_val = spec.as_ref().map(|s| {
            serde_json::json!({
                "risk_level": s.risk_level,
                "category": s.category,
            })
        });

        let permissions = self.permissions.lock().await;
        let decision =
            permissions.evaluate(&tool_call.name, &tool_call.arguments, metadata_val.as_ref());
        drop(permissions);

        if decision.allowed {
            return true;
        }

        if !decision.needs_user {
            // Blocked outright (read-only mode, unknown tool, etc.)
            return false;
        }

        // Permission required — emit event and await user response. The event
        // goes through `self.emit` so the live pump fans it out immediately
        // (previously it was only pushed onto the local vec, meaning the
        // `permission_required` arrived at the GUI AFTER the matching
        // `tool_finished: denied` — the GUI then never cleared the card).
        let reason = decision.reason.clone();
        let category = spec.map(|s| s.category).unwrap_or("");
        let tool_call_id = if tool_call.id.is_empty() {
            None
        } else {
            Some(tool_call.id.clone())
        };
        self.emit(
            events,
            Event::permission_required_for(
                tool_call.name.clone(),
                tool_call.arguments.clone(),
                reason.clone(),
                category.to_string(),
                tool_call_id.clone(),
            ),
        );

        // Call the approver callback directly (Inbox-backed in server mode).
        // Mirrors Python: the callback creates an Inbox item, broadcasts it to
        // WS clients, and suspends on inbox.wait() until a surface resolves it.
        let request = PermissionRequest {
            tool_name: tool_call.name.clone(),
            arguments: tool_call.arguments.as_object().cloned().unwrap_or_default(),
            reason: reason.clone(),
            category: category.to_string(),
            tool_call_id,
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            (self.approver)(request),
        )
        .await
        {
            Ok(Ok(outcome)) => match outcome {
                ApprovalOutcome::Once
                | ApprovalOutcome::AlwaysTool
                | ApprovalOutcome::AlwaysCommand => {
                    if matches!(outcome, ApprovalOutcome::AlwaysTool) {
                        let mut perms = self.permissions.lock().await;
                        perms.allow_tool_for_session(tool_call.name.clone());
                    }
                    if matches!(outcome, ApprovalOutcome::AlwaysCommand) {
                        let mut perms = self.permissions.lock().await;
                        perms.allow_tool_for_session(tool_call.name.clone());
                    }
                    return true;
                }
                ApprovalOutcome::Deny => return false,
            },
            Ok(Err(_)) | Err(_) => {
                // Timeout or approver panicked — deny the tool.
                return false;
            }
        }
    }

    async fn handle_tool_calls(
        &mut self,
        tool_calls: Vec<ocw_provider::ToolCall>,
        cancel: Arc<std::sync::Mutex<bool>>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let mut cleared: Vec<ocw_provider::ToolCall> = Vec::new();

        for mut tc in tool_calls {
            // Providers may parse a tool call without an id (Bedrock, the
            // Anthropic conversion path, Vertex). Everything downstream — the
            // live permission_required event, the Inbox item key, the GUI's
            // approval-card dedupe, and the approval reply's tool_call_id
            // lookup — needs a stable non-empty id, so mint one here.
            if tc.id.is_empty() {
                tc.id = fallback_tool_call_id();
            }
            if *cancel.lock().unwrap() {
                events.push(self.interrupted_tool(&tc));
                continue;
            }

            // Reject phantom tool calls that arrived without a name. Streaming
            // parsers (notably the OpenAI-compatible path before the index
            // aggregation fix) can produce calls with an empty `name` and a
            // partial JSON `arguments` payload when a single logical call was
            // split across many deltas. Surfacing them as "Use " steps in the
            // transcript and asking the user to approve them is pure noise; we
            // record a single `tool_finished: error` so the transcript stays
            // honest, and skip authorization entirely.
            if tc.name.trim().is_empty() {
                self.push_msg(Message::tool_error(
                    tc.id.clone(),
                    "tool call missing name (incomplete or malformed stream delta)",
                    now_ts(),
                ));
                self.emit(
                    &mut events,
                    Event::tool_finished_for(
                        tc.name.clone(),
                        "error".into(),
                        Some("missing tool name".into()),
                        Some("missing tool name".into()),
                        None,
                        None,
                        if tc.id.is_empty() { None } else { Some(tc.id.clone()) },
                    ),
                );
                continue;
            }

            let tc_name = tc.name.clone();
            events.push(Event::tool_proposed(tc_name.clone(), tc.arguments.clone()));

            self.audit(&tc, {
                let mut m = Map::new();
                m.insert("stage".into(), serde_json::Value::String("proposed".into()));
                m
            });

            match tc_name.as_str() {
                "request_directory" => {
                    let ev = self.handle_directory_request(&tc).await;
                    events.push(ev);
                    continue;
                }
                "propose_plan" => {
                    let ev = self.handle_plan_proposal(&tc).await;
                    events.push(ev);
                    continue;
                }
                "ask_user" => {
                    let ev = self.handle_ask_user(&tc).await;
                    events.push(ev);
                    continue;
                }
                _ => {}
            }

            let allowed = self.authorize_tool(&tc, &mut events).await;
            if allowed {
                cleared.push(tc);
            } else {
                self.push_msg(Message::tool_error(tc.id.clone(), "requires approval", now_ts()));
                self.emit(
                    &mut events,
                    Event::tool_finished_for(
                        tc.name.clone(),
                        "denied".into(),
                        Some("requires approval".into()),
                        None,
                        None,
                        None,
                        if tc.id.is_empty() { None } else { Some(tc.id.clone()) },
                    ),
                );
            }
        }

        // Partition cleared tools into serial vs concurrent
        let mut serial_list = Vec::new();
        let mut concurrent_list = Vec::new();
        for tc in cleared {
            let spec = self.registry.get_spec(&tc.name);
            let is_parallel = spec.map(|s| s.parallel_safe).unwrap_or(false);
            if is_parallel {
                concurrent_list.push(tc);
            } else {
                serial_list.push(tc);
            }
        }

        // Run concurrent tools
        if !concurrent_list.is_empty() {
            let handles: Vec<_> = concurrent_list
                .iter()
                .map(|tc| {
                    let started_name = tc.name.clone();
                    events.push(Event::tool_started(started_name));
                    self.audit(&tc, {
                        let mut m = Map::new();
                        m.insert("stage".into(), serde_json::Value::String("started".into()));
                        m
                    });
                    let registry = Arc::clone(&self.registry);
                    let name = tc.name.clone();
                    let id = tc.id.clone();
                    let args = tc
                        .arguments
                        .clone()
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                    tokio::task::spawn_blocking(move || {
                        let result = registry.execute(&name, args);
                        (name, id, result)
                    })
                })
                .collect();

            for handle in handles {
                if *cancel.lock().unwrap() {
                    break;
                }
                if let Ok((name, id, result)) = handle.await {
                    let tc = ocw_provider::ToolCall {
                        id,
                        name,
                        arguments: serde_json::Value::Object(Default::default()),
                    };
                    events.push(self.record_result(&tc, result).await);
                }
            }
        }

        // Run serial tools
        for tc in serial_list {
            if *cancel.lock().unwrap() {
                events.push(self.interrupted_tool(&tc));
                continue;
            }
            let started_name = tc.name.clone();
            events.push(Event::tool_started(started_name));
            self.audit(&tc, {
                let mut m = Map::new();
                m.insert("stage".into(), serde_json::Value::String("started".into()));
                m
            });
            let registry = Arc::clone(&self.registry);
            let tc_name = tc.name.clone();
            let tc_id = tc.id.clone();
            let args_raw = tc.arguments.clone();
            let args_map = args_raw.as_object().cloned().unwrap_or_default();
            let name_for_blocking = tc_name.clone();
            let handle =
                tokio::task::spawn_blocking(move || registry.execute(&name_for_blocking, args_map));
            let result = handle
                .await
                .unwrap_or_else(|_| Err(ToolError::ExecutionFailed("task cancelled".into())));
            let tc_result = ocw_provider::ToolCall {
                id: tc_id,
                name: tc_name,
                arguments: args_raw,
            };
            events.push(self.record_result(&tc_result, result).await);
        }

        events
    }

    fn interrupted_tool(&mut self, tool_call: &ocw_provider::ToolCall) -> Event {
        self.push_msg(Message::tool_error(
            tool_call.id.clone(),
            "interrupted by user",
            now_ts(),
        ));
        self.audit(tool_call, {
            let mut m = Map::new();
            m.insert("stage".into(), serde_json::Value::String("finished".into()));
            m.insert("status".into(), serde_json::Value::String("interrupted".into()));
            m.insert("reason".into(), serde_json::Value::String("user stop".into()));
            m
        });
        Event::tool_finished_for(
            tool_call.name.clone(),
            "interrupted".into(),
            Some("interrupted by user".into()),
            Some("stopped".into()),
            None,
            None,
            if tool_call.id.is_empty() {
                None
            } else {
                Some(tool_call.id.clone())
            },
        )
    }

    async fn record_result(
        &mut self,
        tool_call: &ocw_provider::ToolCall,
        result: Result<ToolResult, ToolError>,
    ) -> Event {
        let display_val = result
            .as_ref()
            .ok()
            .and_then(|r| r.display.clone())
            .map(|d| serde_json::json!(d));
        let (value, status): (Value, String) = match result {
            Ok(r) => (r.value, "ok".to_string()),
            Err(e) => (
                serde_json::json!({ "error": e.to_string() }),
                "error".to_string(),
            ),
        };

        self.push_msg(Message::tool_result(
            tool_call.id.clone(),
            match &value {
                Value::String(s) => s.clone(),
                _ => serde_json::to_string(&value).unwrap_or_default(),
            },
            now_ts(),
        ));

        let rule = self.standing_notes.lock().await.remove(&tool_call.id);

        // Audit: finished (mirrors Python engine.py:673-679)
        {
            let mut extra = Map::new();
            extra.insert("stage".into(), serde_json::Value::String("finished".into()));
            extra.insert("status".into(), serde_json::Value::String(status.clone()));
            extra.insert("result".into(), value.clone());
            extra.insert(
                "result_preview".into(),
                serde_json::Value::String(preview(&value, 500)),
            );
            self.audit(tool_call, extra);
        }

        // Audit: filtered (mirrors Python engine.py:667-672)
        if let Some(ref disp) = display_val {
            let hidden = disp.get("hidden_by_filters").and_then(|v| v.as_u64()).unwrap_or(0);
            let stripped = disp.get("hidden_fields").and_then(|v| v.as_u64()).unwrap_or(0);
            if hidden > 0 || stripped > 0 {
                let mut parts = Vec::new();
                if hidden > 0 {
                    parts.push(format!("{} result(s) hidden", hidden));
                }
                if stripped > 0 {
                    parts.push(format!("{} field value(s) stripped", stripped));
                }
                parts.push("by privacy filters".into());
                let reason = parts.join(" · ");
                let mut extra = Map::new();
                extra.insert("stage".into(), serde_json::Value::String("filtered".into()));
                extra.insert("status".into(), serde_json::Value::String("hidden".into()));
                extra.insert("reason".into(), serde_json::Value::String(reason));
                self.audit(tool_call, extra);
            }
        }

        Event::tool_finished_for(
            tool_call.name.clone(),
            status,
            Some(preview(&value, 300)),
            None,
            display_val,
            rule,
            if tool_call.id.is_empty() {
                None
            } else {
                Some(tool_call.id.clone())
            },
        )
    }

    async fn handle_directory_request(&mut self, tool_call: &ocw_provider::ToolCall) -> Event {
        let args = tool_call.arguments.as_object().cloned().unwrap_or_default();
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let writable = args
            .get("writable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let ev = Event::directory_requested(reason.clone(), path.clone(), writable);
        let tool_call_id = if tool_call.id.is_empty() {
            None
        } else {
            Some(tool_call.id.clone())
        };

        // Call the directory requester callback directly (Inbox-backed in server mode).
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            (self.directory_requester)(args, tool_call_id),
        )
        .await
        {
            Ok(dir_result) => {
                let mut result_json = serde_json::json!({
                    "granted": dir_result.granted,
                    "path": dir_result.path,
                    "writable": dir_result.writable,
                });
                if let Some(err) = &dir_result.error {
                    result_json["error"] = serde_json::json!(err);
                }
                serde_json::to_string(&result_json).unwrap_or_default()
            }
            Err(_) => serde_json::json!({
                "granted": false,
                "error": "directory request timed out or was cancelled",
            })
            .to_string(),
        };

        self.push_msg(Message::tool_result(tool_call.id.clone(), result, now_ts()));
        ev
    }

    async fn handle_plan_proposal(&mut self, tool_call: &ocw_provider::ToolCall) -> Event {
        let args = tool_call.arguments.as_object().cloned().unwrap_or_default();
        let plan = args
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let permissions = self.permissions.lock().await;
        let mode = permissions.mode();
        drop(permissions);

        let tool_call_id = if tool_call.id.is_empty() {
            None
        } else {
            Some(tool_call.id.clone())
        };

        // Call the plan approver callback directly (Inbox-backed in server mode).
        let result = if mode == Mode::Plan {
            match tokio::time::timeout(
                std::time::Duration::from_secs(3600),
                (self.plan_approver)(args, tool_call_id),
            )
            .await
            {
                Ok(plan_result) => serde_json::json!({
                    "approved": plan_result.approved,
                    "mode": plan_result.mode,
                    "feedback": plan_result.feedback,
                })
                .to_string(),
                Err(_) => serde_json::json!({
                    "approved": false,
                    "feedback": "plan approval timed out or was cancelled",
                })
                .to_string(),
            }
        } else {
            serde_json::json!({
                "approved": false,
                "feedback": "not in plan mode",
            })
            .to_string()
        };

        self.push_msg(Message::tool_result(tool_call.id.clone(), result, now_ts()));
        Event::plan_proposed(plan)
    }

    async fn handle_ask_user(&mut self, tool_call: &ocw_provider::ToolCall) -> Event {
        // Mirrors Python `TurnEngine._handle_ask_user`: the asker owns surfacing the
        // question over WS; the engine awaits (interruptibly), appends a tool result,
        // and emits TOOL_FINISHED — never a second question_requested.
        let args = tool_call.arguments.as_object().cloned().unwrap_or_default();
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let tool_call_id = if tool_call.id.is_empty() {
            None
        } else {
            Some(tool_call.id.clone())
        };

        let result = if question.is_empty() {
            serde_json::json!({
                "answer": "",
                "error": "no question was asked",
            })
            .to_string()
        } else {
            // Persist before parking — mirrors Python `persist_session` in the asker.
            if let Some(hook) = &self.park_hook {
                hook(&self.messages);
            }
            self.audit(tool_call, {
                let mut m = Map::new();
                m.insert(
                    "stage".into(),
                    serde_json::Value::String("question_requested".into()),
                );
                m.insert("reason".into(), serde_json::Value::String(question.clone()));
                m
            });
            self.interruptible_question(args, tool_call_id.clone()).await
        };

        let status = ask_user_status(&result);
        let preview = tool_result_preview(&result);
        self.push_msg(Message::tool_result(
            tool_call.id.clone(),
            result,
            now_ts(),
        ));
        self.audit(tool_call, {
            let mut m = Map::new();
            m.insert("stage".into(), serde_json::Value::String("finished".into()));
            m.insert(
                "status".into(),
                serde_json::Value::String(status.to_string()),
            );
            m
        });
        Event::tool_finished_for(
            "ask_user".into(),
            status.into(),
            Some(preview),
            None,
            None,
            None,
            tool_call_id,
        )
    }

    /// Await `question_asker`, but resolve early with an interrupted payload if
    /// the user stops the turn — mirrors Python `_interruptible`.
    async fn interruptible_question(
        &self,
        args: Map<String, Value>,
        tool_call_id: Option<String>,
    ) -> String {
        let interrupted = serde_json::json!({
            "answer": "",
            "error": "interrupted by user",
        })
        .to_string();
        let timed_out = serde_json::json!({
            "answer": "",
            "error": "question timed out or was cancelled",
        })
        .to_string();

        let ask = (self.question_asker)(args, tool_call_id);
        tokio::pin!(ask);
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3600));
        tokio::pin!(timeout);
        let cancel = Arc::clone(&self.cancel);

        loop {
            if cancel.lock().map(|g| *g).unwrap_or(false) {
                return interrupted;
            }
            tokio::select! {
                biased;
                result = &mut ask => return result,
                _ = &mut timeout => return timed_out,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Message preparation
    // ---------------------------------------------------------------------------

    fn outbound_messages(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .messages
            .iter()
            .filter(|m| !m.is_display_only())
            .map(|m| m.to_wire())
            .collect();

        // Per-turn context injection — mirrors Python's `_outbound_messages`.
        // The `<system-context>` block is appended to the last user message so
        // the model sees fresh per-turn data (date, plan reminders, etc.) without
        // polluting the persisted history.
        if let Some(ctx_fn) = &self.context_provider {
            let ctx = ctx_fn();
            if !ctx.is_empty() {
                let block = format!("\n\n<system-context>\n{ctx}\n</system-context>");
                // Append to the LAST user message
                if let Some(last_user) = out.iter_mut().rev().find(|v| {
                    v.get("role").and_then(|r| r.as_str()) == Some("user")
                }) {
                    match last_user.get_mut("content") {
                        Some(Value::String(s)) => s.push_str(&block),
                        Some(Value::Array(arr)) => {
                            arr.push(serde_json::json!({"type": "text", "text": block}));
                        }
                        Some(v) => *v = Value::String(block),
                        None => {
                            last_user["content"] = Value::String(block);
                        }
                    }
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum StreamResult {
    Ok {
        events: Vec<Event>,
        turn: AssistantTurn,
    },
    Err {
        error: ProviderError,
        streamed_text: String,
        streamed_reasoning: String,
    },
    Interrupted {
        events: Vec<Event>,
        streamed_text: String,
        streamed_reasoning: String,
    },
}

/// A consuming iterator over all events from a run.
pub struct EngineEvents {
    events: std::vec::IntoIter<Event>,
    current: Option<Event>,
}

impl EngineEvents {
    fn new(events: Vec<Event>) -> Self {
        let mut events = events;
        let current = events.pop();
        Self {
            events: events.into_iter(),
            current,
        }
    }
}

impl Iterator for EngineEvents {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.current.take().or_else(|| self.events.next())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventData, EventType};
    use crate::permissions::{Mode, PermissionEngine};
    use crate::tool_registry::ToolRegistry;
    use ocw_provider::{AssistantTurn, Provider, TokenUsage};
    use std::sync::Arc as StdArc;

    /// A provider that returns a single canned `AssistantTurn` (text + tool calls)
    /// on the first `stream()` call, then errors on every subsequent one. Used to
    /// drive the engine through a single round-trip in tests.
    struct CannedProvider {
        text: String,
        tool_calls: Vec<ocw_provider::ToolCall>,
        consumed: std::sync::Mutex<bool>,
    }

    impl CannedProvider {
        fn new(text: &str, tool_calls: Vec<ocw_provider::ToolCall>) -> Self {
            Self {
                text: text.to_string(),
                tool_calls,
                consumed: std::sync::Mutex::new(false),
            }
        }
    }

    impl Provider for CannedProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: Vec<Value>,
            _tools: Option<Vec<Value>>,
            _settings: Value,
        ) -> Result<AssistantTurn, ocw_provider::Error> {
            Ok(AssistantTurn {
                text: Some(self.text.clone()),
                tool_calls: self.tool_calls.clone(),
                finish_reason: Some("tool_calls".into()),
                reasoning: None,
                usage: Some(TokenUsage {
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                }),
            })
        }

        fn capabilities(&self, _model: &str) -> ocw_provider::ModelCapabilities {
            ocw_provider::ModelCapabilities::default()
        }

        fn name(&self) -> &str {
            "canned"
        }
    }

    fn make_engine(
        tool_calls: Vec<ocw_provider::ToolCall>,
    ) -> (TurnEngine, std::sync::mpsc::Receiver<Event>) {
        let provider: Arc<dyn Provider> = Arc::new(CannedProvider::new("ok", tool_calls));
        let registry = Arc::new(ToolRegistry::new());
        // Use a throwaway path for the permission engine so tests are hermetic.
        let tmp = std::env::temp_dir().join(format!(
            "ocw-engine-test-{}.json",
            std::process::id()
        ));
        let permissions = Arc::new(tokio::sync::Mutex::new(PermissionEngine::new(tmp)));
        let mut eng = TurnEngine::new(
            provider,
            registry,
            permissions,
            "test-model".into(),
            4,
            Map::new(),
            Vec::new(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        eng.set_live_events(Some(tx));
        (eng, rx)
    }

    #[tokio::test]
    async fn successful_final_answers_are_kept_in_message_history() {
        let (mut eng, _live_rx) = make_engine(vec![]);

        let _ = eng.run(serde_json::json!("first question"), None).await;
        let _ = eng.run(serde_json::json!("second question"), None).await;

        let serialized: Vec<Value> = eng
            .messages()
            .iter()
            .map(|message| serde_json::to_value(message).unwrap())
            .collect();
        let roles: Vec<&str> = serialized
            .iter()
            .filter_map(|message| message.get("role").and_then(Value::as_str))
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);

        for index in [1, 3] {
            assert_eq!(serialized[index]["content"], "ok");
            assert!(serialized[index].get("ts").and_then(Value::as_f64).is_some());
            assert!(serialized[index].get("usage").is_some());
            assert!(serialized[index].get("tool_calls").is_none());
        }
    }

    /// Mirrors §1 of the plan: an empty-named tool call (the symptom DeepSeek exposed
    /// before SSE aggregation) must surface as `tool_finished: error`, NOT as a
    /// permission_required that the user is asked to approve. Without this, the
    /// screenshot would render dozens of "Use " cards.
    #[tokio::test]
    async fn empty_tool_name_short_circuits_with_error() {
        let phantom = ocw_provider::ToolCall {
            id: "call_phantom_1".into(),
            name: "".into(), // <— the bug: name was empty in raw delta
            arguments: serde_json::json!({"_raw": "{\"q\":\"\"}"}),
        };
        let (mut eng, live_rx) = make_engine(vec![phantom]);
        // Drive the loop directly — we just need the tool-handling path to run.
        let events = eng.run_loop().await;

        // The phantom call must NOT reach the user as a permission prompt.
        let has_permission = events.iter().any(|e| {
            matches!(
                e.event_type,
                EventType::PermissionRequired | EventType::ToolProposed
            )
        });
        assert!(
            !has_permission,
            "phantom tool call should not produce permission_required or tool_proposed events"
        );
        // It MUST produce a tool_finished: error with the offending tool_call_id so
        // any already-rendered card can be cleared.
        let error_event = events.iter().find(|e| {
            matches!(e.event_type, EventType::ToolFinished)
                && matches!(
                    &e.data,
                    EventData::ToolFinished { status, tool_call_id, .. }
                    if status == "error" && tool_call_id.as_deref() == Some("call_phantom_1")
                )
        });
        assert!(
            error_event.is_some(),
            "expected a tool_finished: error event with tool_call_id=call_phantom_1"
        );
        // The live pump must also have received it BEFORE the run returns.
        let mut live_seen_error = false;
        while let Ok(ev) = live_rx.try_recv() {
            if matches!(&ev.event_type, EventType::ToolFinished) {
                if let EventData::ToolFinished { status, .. } = &ev.data {
                    if status == "error" {
                        live_seen_error = true;
                    }
                }
            }
        }
        assert!(live_seen_error, "live pump should have observed the error event");
    }

    /// Mirrors §2 of the plan: a permission_required event MUST be sent to the live
    /// pump before `authorize_tool` blocks on `rx.recv()`. Previously the event was
    /// only pushed onto the local `events` vec, so the GUI's approval card appeared
    /// AFTER the engine had already given up waiting and emitted `tool_finished:
    /// denied` — the card then never cleared.
    #[tokio::test]
    async fn permission_required_is_live_emitted_before_blocking() {
        // A tool call that the engine will need to authorize. The registry is empty
        // (so the spec lookup returns None), but we wire up a permissions engine in
        // a mode that requires user approval, and bind NO approval channel — so
        // authorize_tool will emit the event AND fall through to `false` (no
        // execution). That sequence exercises the live-pump path.
        // Note: the tool must classify as consequential — `shell` is Exec, while an
        // unknown name now defaults to Read (mirrors Python's `classify`) and is
        // auto-allowed without a permission_required event.
        let write_tool = ocw_provider::ToolCall {
            id: "call_shell_1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        };
        let (mut eng, live_rx) = make_engine(vec![write_tool]);

        // Force a permissions engine in Interactive mode (default requires approval).
        let tmp = std::env::temp_dir().join(format!(
            "ocw-engine-test-perms-{}.json",
            std::process::id()
        ));
        let perms = Arc::new(tokio::sync::Mutex::new(PermissionEngine::new(tmp)));
        {
            let mut g = perms.lock().await;
            g.set_mode(Mode::Interactive);
        }
        // Replace the engine's permissions handle.
        eng.permissions = perms;
        // Default approver already returns Deny — no channel setup needed.

        // The run loop is blocking; the live pump is sync mpsc. We poll briefly
        // from a background task to capture the ordering of the live events.
        let live_rx_arc = StdArc::new(std::sync::Mutex::new(live_rx));
        let probe_rx = StdArc::clone(&live_rx_arc);
        let probe = tokio::task::spawn_blocking(move || {
            let mut order: Vec<String> = Vec::new();
            for _ in 0..500 {
                if let Ok(ev) = probe_rx.lock().unwrap().try_recv() {
                    let is_terminator = matches!(ev.event_type, EventType::TurnEnd);
                    order.push(format!("{:?}", ev.event_type));
                    if is_terminator {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            order
        });

        // Run the engine — this will emit permission_required THEN tool_finished.
        let _events = eng.run_loop().await;
        // After run_loop returns, the live pump has drained. The probe task
        // captured the ordering — assert permission_required precedes tool_finished.
        let order = probe.await.unwrap();
        let pr_idx = order
            .iter()
            .position(|s| s == "PermissionRequired")
            .expect("permission_required should be live-emitted");
        let tf_idx = order
            .iter()
            .position(|s| s == "ToolFinished")
            .expect("tool_finished should be live-emitted (denial)");
        assert!(
            pr_idx < tf_idx,
            "permission_required must arrive BEFORE tool_finished in the live pump; got order={:?}",
            order
        );
    }

    /// A tool call parsed without an id (Bedrock / Anthropic conversion /
    /// Vertex fall back to empty) must still produce a permission_required
    /// event with a non-empty tool_call_id — the GUI dedupes approval cards
    /// by it and the server routes the approval reply back to the Inbox item
    /// by it. `handle_tool_calls` mints a fallback id before authorization.
    #[tokio::test]
    async fn empty_tool_call_id_gets_fallback_in_permission_required() {
        let write_tool = ocw_provider::ToolCall {
            id: String::new(), // <— the bug: some provider parsers yield an empty id
            name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        };
        let (mut eng, live_rx) = make_engine(vec![write_tool]);

        // Force a permissions engine in Interactive mode (default requires approval).
        let tmp = std::env::temp_dir().join(format!(
            "ocw-engine-test-perms-{}.json",
            std::process::id()
        ));
        let perms = Arc::new(tokio::sync::Mutex::new(PermissionEngine::new(tmp)));
        {
            let mut g = perms.lock().await;
            g.set_mode(Mode::Interactive);
        }
        eng.permissions = perms;
        // Default approver already returns Deny — no channel setup needed.

        // The run loop is blocking; poll the live pump from a background task
        // to capture the PermissionRequired event.
        let live_rx_arc = StdArc::new(std::sync::Mutex::new(live_rx));
        let probe_rx = StdArc::clone(&live_rx_arc);
        let probe = tokio::task::spawn_blocking(move || {
            let mut seen: Option<Event> = None;
            for _ in 0..500 {
                if let Ok(ev) = probe_rx.lock().unwrap().try_recv() {
                    if matches!(ev.event_type, EventType::PermissionRequired) {
                        seen = Some(ev);
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            seen
        });

        let _events = eng.run_loop().await;
        let seen = probe
            .await
            .unwrap()
            .expect("permission_required should be live-emitted");
        match &seen.data {
            EventData::PermissionRequired { tool_call_id, .. } => {
                let id = tool_call_id
                    .as_deref()
                    .expect("empty provider id must be replaced with a fallback");
                assert!(
                    id.starts_with("tc_"),
                    "fallback id expected, got {id:?}"
                );
            }
            other => panic!("expected PermissionRequired, got {other:?}"),
        }
    }

    #[test]
    fn fallback_tool_call_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(fallback_tool_call_id()),
                "fallback ids must be unique"
            );
        }
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        // Regression (owner-hit 2026-08-08): a tool result of ASCII followed by
        // box-drawing chars (a wttr.in weather report) made the byte slice
        // `&text[..297]` land inside '─' and panic, killing the turn task.
        let text = format!("{}{}", "a".repeat(296), "─".repeat(50));
        let out = preview(&serde_json::json!(text), 300);
        assert!(out.ends_with("..."));
        assert!(out.starts_with("aaa"));
        assert!(out.chars().count() <= 300);

        // Pure CJK, any length — never a char-boundary hazard.
        let cjk = "测".repeat(1000);
        let out = preview(&serde_json::json!(cjk), 300);
        assert!(out.ends_with("..."));

        // Under the limit: returned untouched.
        assert_eq!(preview(&serde_json::json!("短文本"), 300), "短文本");
    }
}
