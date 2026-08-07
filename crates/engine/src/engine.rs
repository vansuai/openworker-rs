//! TurnEngine — the owned agent loop.

use crate::events::Event;
use crate::permissions::{Mode, PermissionEngine};
use crate::tool_registry::ToolRegistry;
use crate::tool_types::{Error as ToolError, ToolResult};
use crate::types::{Message, ToolCall};
use ocw_provider::{AssistantTurn, Error as ProviderError, Provider, StreamEvent};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum Error {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("cannot retry: no error at tail")]
    CannotRetry,
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
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ApprovalOutcome, tokio::task::JoinError>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

/// Channels for interactive tool callbacks: directory requests, plan approvals, and
/// questions. These are set on the engine when running in server mode so the
/// WS handler can drive them asynchronously.
/// Uses tokio::sync::Mutex (not parking_lot) so MutexGuard is Send across thread pools.
#[derive(Clone)]
pub struct EngineCallbacks {
    /// Receives directory grant/decline from the WS handler.
    pub directory_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<DirectoryResult>>>>,
    /// Receives plan approval result from the WS handler.
    pub plan_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<PlanResult>>>>,
    /// Receives user answer from the WS handler.
    pub question_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<String>>>>,
    /// Receives approval outcomes from the WS handler (for permission-required tools).
    pub approval_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<ApprovalOutcome>>>>,
}

impl EngineCallbacks {
    pub fn new() -> Self {
        Self {
            directory_rx: Arc::new(tokio::sync::Mutex::new(None)),
            plan_rx: Arc::new(tokio::sync::Mutex::new(None)),
            question_rx: Arc::new(tokio::sync::Mutex::new(None)),
            approval_rx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Bind a channel for a pending permission approval request.
    pub async fn bind_approval(&self, rx: mpsc::Receiver<ApprovalOutcome>) {
        *self.approval_rx.lock().await = Some(rx);
    }

    /// Bind a channel for a pending directory request.
    pub async fn bind_directory(&self, rx: mpsc::Receiver<DirectoryResult>) {
        *self.directory_rx.lock().await = Some(rx);
    }

    /// Bind a channel for a pending plan approval.
    pub async fn bind_plan(&self, rx: mpsc::Receiver<PlanResult>) {
        *self.plan_rx.lock().await = Some(rx);
    }

    /// Bind a channel for a pending question.
    pub async fn bind_question(&self, rx: mpsc::Receiver<String>) {
        *self.question_rx.lock().await = Some(rx);
    }
}

impl Default for EngineCallbacks {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn preview(value: &Value, max_chars: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    };
    let text = text.replace('\n', "\\n");
    if text.len() <= max_chars {
        text
    } else {
        format!("{}...", &text[..max_chars.saturating_sub(3)])
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
    callbacks: EngineCallbacks,
    /// tokio::sync::Mutex so the guard is Send.
    standing_notes: Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Shared with the WS layer so `interrupt` can stop an in-flight turn.
    cancel: Arc<std::sync::Mutex<bool>>,
    /// Optional live sink — events are forwarded as they are produced (for WS streaming).
    live_tx: Option<std::sync::mpsc::Sender<Event>>,
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
            callbacks: EngineCallbacks::new(),
            standing_notes: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            cancel: Arc::new(std::sync::Mutex::new(false)),
            live_tx: None,
        }
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

    /// Set the interactive callbacks (directory/plan/question channels).
    pub fn with_callbacks(mut self, callbacks: EngineCallbacks) -> Self {
        self.callbacks = callbacks;
        self
    }

    /// Reset all pending callback channels (call between turns).
    pub fn reset_callbacks(&mut self) {
        self.callbacks = EngineCallbacks::new();
    }

    /// Set the model mid-session (model switch).
    pub fn switch_model(&mut self, model: String) {
        self.model = model;
    }

    /// The current conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
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
        self.messages.push(msg);
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
                    Err(e) => return StreamResult::Err(e),
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
                            push(
                                &mut events_out,
                                &live_tx2,
                                Event::error(e.to_string(), "ProviderError".into()),
                            );
                            return StreamResult::Err(e);
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
                                Some(streamed_text),
                                vec![],
                                if streamed_reasoning.is_empty() {
                                    None
                                } else {
                                    Some(streamed_reasoning)
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
                    return StreamResult::Interrupted(events_out);
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
            .unwrap_or_else(|e| StreamResult::Err(ProviderError::Other(e.to_string())));

            match result {
                StreamResult::Err(e) => {
                    self.emit(
                        &mut events,
                        Event::error(e.to_string(), "ProviderError".into()),
                    );
                    break;
                }
                StreamResult::Interrupted(evs) => {
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
                    if tool_calls_vec.is_empty() {
                        self.emit(
                            &mut events,
                            Event::turn_end("completed".into(), iterations),
                        );
                        break;
                    }

                    let turn_text = turn.text.clone();
                    let _turn_reasoning = turn.reasoning.clone();
                    let _turn_usage = turn.usage.as_ref().map(|u| serde_json::json!(u));
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

                    self.messages.push(Message::assistant(
                        turn_text.unwrap_or_default(),
                        tool_calls_for_message,
                    ));

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

        // Permission required — emit event and await user response
        let reason = decision.reason.clone();
        let category = spec.map(|s| s.category).unwrap_or("");
        events.push(Event::permission_required(
            tool_call.name.clone(),
            tool_call.arguments.clone(),
            reason,
            category.to_string(),
        ));

        // If an approval channel is bound, wait for the user's decision
        if let Some(mut rx) = self.callbacks.approval_rx.lock().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv()).await {
                Ok(Some(outcome)) => {
                    match outcome {
                        ApprovalOutcome::Once
                        | ApprovalOutcome::AlwaysTool
                        | ApprovalOutcome::AlwaysCommand => {
                            // Apply session allow
                            if matches!(outcome, ApprovalOutcome::AlwaysTool) {
                                let mut perms = self.permissions.lock().await;
                                perms.allow_tool_for_session(tool_call.name.clone());
                            }
                            if matches!(outcome, ApprovalOutcome::AlwaysCommand) {
                                // For AlwaysCommand we'd need to extract the command from args
                                // and allow it — simplified: allow the tool
                                let mut perms = self.permissions.lock().await;
                                perms.allow_tool_for_session(tool_call.name.clone());
                            }
                            return true;
                        }
                        ApprovalOutcome::Deny => {
                            return false;
                        }
                    }
                }
                Ok(None) | Err(_) => {
                    // Timeout or channel closed
                    return false;
                }
            }
        }

        false
    }

    async fn handle_tool_calls(
        &mut self,
        tool_calls: Vec<ocw_provider::ToolCall>,
        cancel: Arc<std::sync::Mutex<bool>>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let mut cleared: Vec<ocw_provider::ToolCall> = Vec::new();

        for tc in tool_calls {
            if *cancel.lock().unwrap() {
                events.push(self.interrupted_tool(&tc));
                continue;
            }

            let tc_name = tc.name.clone();
            events.push(Event::tool_proposed(tc_name.clone(), tc.arguments.clone()));

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
                self.messages
                    .push(Message::tool_error(tc.id.clone(), "requires approval"));
                events.push(Event::tool_finished(
                    tc.name.clone(),
                    "denied".into(),
                    Some("requires approval".into()),
                    None,
                    None,
                    None,
                ));
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
        self.messages.push(Message::tool_error(
            tool_call.id.clone(),
            "interrupted by user",
        ));
        Event::tool_finished(
            tool_call.name.clone(),
            "interrupted".into(),
            Some("interrupted by user".into()),
            Some("stopped".into()),
            None,
            None,
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

        self.messages.push(Message::tool_result(
            tool_call.id.clone(),
            match &value {
                Value::String(s) => s.clone(),
                _ => serde_json::to_string(&value).unwrap_or_default(),
            },
        ));

        let rule = self.standing_notes.lock().await.remove(&tool_call.id);

        Event::tool_finished(
            tool_call.name.clone(),
            status,
            Some(preview(&value, 300)),
            None,
            display_val,
            rule,
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

        // Try to receive the user's response from the WS handler
        let result = match self.callbacks.directory_rx.lock().await.take() {
            Some(mut rx) => {
                match tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv()).await {
                    Ok(Some(result)) => {
                        let mut result_json = serde_json::json!({
                            "granted": result.granted,
                            "path": result.path,
                            "writable": result.writable,
                        });
                        if let Some(err) = &result.error {
                            result_json["error"] = serde_json::json!(err);
                        }
                        serde_json::to_string(&result_json).unwrap_or_default()
                    }
                    Ok(None) | Err(_) => serde_json::json!({
                        "granted": false,
                        "error": "directory request timed out or was cancelled",
                    })
                    .to_string(),
                }
            }
            None => serde_json::json!({
                "granted": false,
                "error": "directory request not available (no callback channel set)",
            })
            .to_string(),
        };

        self.messages
            .push(Message::tool_result(tool_call.id.clone(), result));
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

        let result = match self.callbacks.plan_rx.lock().await.take() {
            Some(mut rx) => {
                match tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv()).await {
                    Ok(Some(result)) => serde_json::json!({
                        "approved": result.approved,
                        "mode": result.mode,
                        "feedback": result.feedback,
                    })
                    .to_string(),
                    Ok(None) | Err(_) => serde_json::json!({
                        "approved": false,
                        "feedback": "plan approval timed out or was cancelled",
                    })
                    .to_string(),
                }
            }
            None => {
                if mode == Mode::Plan {
                    serde_json::json!({
                        "approved": false,
                        "feedback": "plan approval not available (no callback channel set)",
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "approved": false,
                        "feedback": "not in plan mode",
                    })
                    .to_string()
                }
            }
        };

        self.messages
            .push(Message::tool_result(tool_call.id.clone(), result));
        Event::plan_proposed(plan)
    }

    async fn handle_ask_user(&mut self, tool_call: &ocw_provider::ToolCall) -> Event {
        let args = tool_call.arguments.as_object().cloned().unwrap_or_default();
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let result = match self.callbacks.question_rx.lock().await.take() {
            Some(mut rx) => {
                match tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv()).await {
                    Ok(Some(answer)) => serde_json::json!({ "answer": answer }).to_string(),
                    Ok(None) | Err(_) => serde_json::json!({
                        "answer": "",
                        "error": "question timed out or was cancelled",
                    })
                    .to_string(),
                }
            }
            None => {
                if question.is_empty() {
                    serde_json::json!({ "answer": "" }).to_string()
                } else {
                    serde_json::json!({
                        "answer": "",
                        "error": "ask_user not available (no callback channel set)",
                    })
                    .to_string()
                }
            }
        };

        self.messages
            .push(Message::tool_result(tool_call.id.clone(), result));
        Event::question_requested(question, tool_call.id.clone())
    }

    // ---------------------------------------------------------------------------
    // Message preparation
    // ---------------------------------------------------------------------------

    fn outbound_messages(&self) -> Vec<Value> {
        self.messages
            .iter()
            .filter(|m| !m.is_display_only())
            .map(|m| m.to_wire())
            .collect()
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
    Err(ProviderError),
    Interrupted(Vec<Event>),
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
