//! Background scheduler for automation tasks.
//!
//! Mirrors `coworker/automation/scheduler.py`:
//! - Every 30 seconds, queries due tasks and spawns background async tasks to run them.
//! - First tick on startup fires a "catchup" run for anything missed while the server was down.

use serde_json::{json, Value};
use futures_util::FutureExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::automations::{compute_next_run, AutomationStore, ScheduledTask, TaskRun};
use crate::state::AppState;
use crate::ws::build_builtin_registry;
use ocw_data::{args_preview, VIS_INBOX};
use ocw_engine::{
    classify_risk, standing_target_candidate, ApprovalOutcome, Approver, PermissionRequest, RiskClass,
};

fn short_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &uuid::Uuid::new_v4().to_string()[..10])
}

fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Overlap guard — mirrors Python's `Scheduler._running_ids` (skip-on-overlap):
/// never stack a run while the previous one for the same task is still going.
/// The task's `next_run` only advances at finalize time, so without this every
/// 30s tick would spawn duplicate engine runs while one is in flight.
static RUNNING_IDS: LazyLock<parking_lot::Mutex<HashSet<String>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashSet::new()));

async fn run_task(state: &Arc<AppState>, task: ScheduledTask, trigger: &str) {
    let run_id = short_id("run");
    let session_id = format!("__run__{}", run_id);
    let started_at = epoch_now();

    // Legacy tasks created before the scratch-provision fix carry an empty
    // workspace — allocate one now and persist it so the run (and its
    // artifacts) land in a real directory (shared with the manual-run path).
    let task = crate::automations::ensure_task_workspace(state, task).await;

    let effective_model = task
        .model
        .clone()
        .unwrap_or_else(|| state.default_model_or_configured());

    let run = TaskRun {
        run_id: run_id.clone(),
        task_id: task.id.clone(),
        started_at,
        finished_at: None,
        status: "running".to_string(),
        result_text: None,
        artifacts: Vec::new(),
        error: None,
        trigger: trigger.to_string(),
        session_id: session_id.clone(),
        model: Some(effective_model.clone()),
    };

    // Persist run as "running" so frontend can see it immediately
    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.add_run(run.clone());
    }

    // Create the __run__ session so GET /v1/sessions/{id}/messages works
    state.get_or_create_session(
        &session_id,
        &task.agent,
        Some(&task.workspace),
        Some(&effective_model),
    );

    // Broadcast automation_run_started (matches Python SessionManager._run_scheduled_task)
    let event = json!({
        "type": "automation_run_started",
        "data": {
            "task_id": task.id,
            "task_title": task.title,
            "session_id": &session_id,
            "workspace": task.workspace,
            "agent": task.agent,
            "trigger": trigger,
        }
    });
    state.broadcast_sync(&session_id, event.clone());
    let _ = state.event_broadcast.send(event);
    let task_id = task.id.clone();

    // Execute + finalize the run. Python's `Scheduler.run_task` catches every
    // exception from the runner and still records an error run; here the panic
    // net is `catch_unwind` so a crash inside the engine finalizes the run as
    // "error" instead of leaving it forever "running".
    let inner = run_task_inner(
        Arc::clone(state),
        task,
        run_id.clone(),
        task_id.clone(),
        session_id,
        started_at,
    );
    if std::panic::AssertUnwindSafe(inner).catch_unwind().await.is_err() {
        let store: &AutomationStore = &*state.automations.read().await;
        store.finalize(
            &run_id,
            &task_id,
            "error",
            Some("run crashed unexpectedly".to_string()),
            None,
            None,
            Vec::new(),
        );
    }
}

/// Engine execution + finalize for one scheduled run. Split out of `run_task`
/// so the outer wrapper can catch panics and still record an error run.
#[allow(clippy::too_many_arguments)]
async fn run_task_inner(
    state: Arc<AppState>,
    task: ScheduledTask,
    run_id: String,
    task_id: String,
    session_id: String,
    started_at: f64,
) {
    // Build TurnEngine (mirrors ws.rs engine initialization)
    let model = task
        .model
        .clone()
        .unwrap_or_else(|| state.default_model_or_configured());
    let provider = Arc::clone(&state.provider);
    let registry = build_builtin_registry(
        &task.workspace,
        ocw_tools::TodoList::new(),
        Arc::clone(&provider),
        &model,
        &task.agent,
        &state.skill_store,
        None, // scheduled-run engines get no scheduling tools (Python `task_store=None`)
        None, // no board tools on scheduled runs
    );
    let permissions = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
        state.config.data_dir.join("permissions.json"),
    )));

    let fresh_system = state
        .build_system_messages(&task.agent, &task.workspace, &model)
        .await;

    // Seed task standing rules into the permission engine (mirrors Python's
    // `_seed_task_permissions`). Target-bound entries feed task_rules; name-only
    // legacy entries go into session_allow_tools for the auto-allow check below.
    {
        let mut perms = permissions.lock().await;
        // Standing rules: tool -> {targets} (connector tools with declared target arg)
        let mut rules: HashMap<String, HashSet<String>> = HashMap::new();
        for entry in &task.always_allowed_tools {
            let (tool, target) = crate::automations::parse_rule(entry);
            if !tool.is_empty() {
                if let Some(t) = target {
                    rules.entry(tool).or_default().insert(t);
                } else {
                    // Name-only legacy entry — auto-allowed by the approver below
                    perms.allow_tool_for_session(tool);
                }
            }
        }
        perms.set_task_rules(rules);
    }

    // Collect name-allowed tools (legacy entries without target binding) for the
    // scheduled approver's fast-path auto-allow. These were already seeded into
    // session_allow_tools above, but we also check them in the approver as a
    // defense-in-depth measure matching Python's `_scheduled_approver`.
    let name_allowed: HashSet<String> = task
        .always_allowed_tools
        .iter()
        .filter_map(|entry| {
            let (tool, target) = crate::automations::parse_rule(entry);
            if target.is_none() && !tool.is_empty() {
                Some(tool)
            } else {
                None
            }
        })
        .collect();

    // Build the scheduled approver — mirrors Python's `_scheduled_approver`:
    //   - Auto-allow WriteLocal tools (write_file, edit_file, …) — path-scoped
    //     by the permission engine, same safety guarantee as Python's WRITE_TOOLS.
    //   - Auto-allow name-allowed legacy tools from the task config.
    //   - Everything else parks in the Inbox (§25 graceful degradation).
    let scheduled_state = Arc::clone(&state);
    let scheduled_sid = session_id.clone();
    let scheduled_agent = task.agent.clone();
    let approver_task_id = task.id.clone();
    let approver_task_title = task.title.clone();
    let mint_perms = Arc::clone(&permissions);
    let approver: Approver = Arc::new(move |request: PermissionRequest| {
        let state = Arc::clone(&scheduled_state);
        let session_id = scheduled_sid.clone();
        let agent = scheduled_agent.clone();
        let name_allowed = name_allowed.clone();
        let task_id = approver_task_id.clone();
        let task_title = approver_task_title.clone();
        let permissions = Arc::clone(&mint_perms);
        let req = request.clone();
        Box::pin(async move {
            // Fast-path: auto-allow WriteLocal tools (write_file, etc.) — the
            // permission engine has already path-scoped the call at this point.
            if classify_risk(&req.tool_name) == RiskClass::WriteLocal {
                return Ok(ApprovalOutcome::Once);
            }
            // Fast-path: auto-allow name-only tools the task explicitly allows.
            if name_allowed.contains(&req.tool_name) {
                return Ok(ApprovalOutcome::Once);
            }
            // Anything else parks in the Inbox and suspends the run.
            let args_value = {
                let mut obj = serde_json::Map::new();
                for (k, v) in req.arguments.iter() {
                    obj.insert(k.clone(), v.clone());
                }
                serde_json::Value::Object(obj)
            };
            // Standing-rule eligibility for the card's "Allow every time" offer.
            let standing_target =
                standing_target_candidate(&req.tool_name, &req.arguments);
            let title = format!("Run `{}`?", req.tool_name);
            let mut parts: Vec<String> = Vec::new();
            if !req.reason.trim().is_empty() {
                parts.push(req.reason.trim().to_string());
            }
            let preview = args_preview(Some(&args_value), 240);
            if !preview.is_empty() {
                parts.push(preview);
            }
            let body = parts.join("\n");
            let inbox_name = state.inbox_routing.route_for(&session_id, Some(&agent));
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
                VIS_INBOX,
                data,
                req.tool_call_id.as_deref(),
            );
            // Mirror to live WS clients so the GUI renders the approval card.
            state.broadcast_sync(
                &session_id,
                serde_json::json!({
                    "type": "permission_required",
                    "data": {
                        "name": req.tool_name,
                        "arguments": args_value,
                        "reason": req.reason,
                        "category": req.category,
                        "item_id": item.id,
                        "tool_call_id": req.tool_call_id,
                        "standing_target": standing_target,
                    },
                }),
            );
            // Suspend until a surface resolves the item.
            let resolution = state.inbox_store.wait(&item.id).await;
            if resolution == "always_task" {
                // Mint a standing rule on the owning task ("Allow every time",
                // §25). Mirrors Python's `mint_task_rule`: fresh task lookup +
                // rule eligibility + dedupe, all re-checked server-side; the
                // mint result never changes this call's outcome (Once).
                let store = state.automations.read().await;
                if let Some(mut task) = store.task_for_run_session(&session_id) {
                    if let Some(target) = standing_target {
                        if task.add_rule(&req.tool_name, &target) {
                            store.save_task(task.clone());
                            // Hot-update the live engine so the run's next call
                            // to this target auto-allows.
                            let mut guard = permissions.lock().await;
                            guard.add_task_rule(req.tool_name.clone(), target.clone());
                            drop(guard);
                            let mut event = serde_json::Map::new();
                            event.insert("session_id".into(), session_id.clone().into());
                            event.insert("tool".into(), req.tool_name.clone().into());
                            event.insert("arguments".into(), req.arguments.clone().into());
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
            Ok(match resolution.as_str() {
                "once" | "allow" => ApprovalOutcome::Once,
                "always_tool" | "always" => ApprovalOutcome::AlwaysTool,
                "always_command" => ApprovalOutcome::AlwaysCommand,
                _ => ApprovalOutcome::Deny,
            })
        })
    });

    let audit_state = Arc::clone(&state);
    let audit_sink: ocw_engine::AuditSink = Arc::new(move |event| {
        audit_state.audit.append(&event);
    });

    // Persist a durable suspend: when an ungranted tool parks in the Inbox the
    // engine must checkpoint its messages so the run resumes on restart.
    // Mirrors Python's `_scheduled_approver` → `persist_session` (§25).
    let park_state = Arc::clone(&state);
    let park_sid = session_id.clone();
    let mut eng = ocw_engine::TurnEngine::new(
        provider,
        registry,
        permissions,
        model,
        12,
        serde_json::Map::new(),
        fresh_system,
    )
    .with_approver(approver)
    .with_audit_sink(audit_sink)
    .with_park_hook(Arc::new(move |msgs: &[ocw_engine::Message]| {
        let values: Vec<Value> = msgs
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect();
        park_state.persist_engine_messages_inner(&park_sid, &values);
    }))
    .with_context_provider(|| {
        chrono::Local::now()
            .format("Current date: %Y-%m-%d")
            .to_string()
    });
    {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert("session_id".into(), serde_json::Value::String(session_id.clone()));
        ctx_map.insert("agent".into(), serde_json::Value::String(task.agent.clone()));
        ctx_map.insert("workspace".into(), serde_json::Value::String(task.workspace.clone()));
        eng.set_audit_context(ctx_map);
    }

    // Opening message — sent as the automation's first user turn
    let opening = format!(
        "⏰ Scheduled run — {}\n\n\
         This automation is due now: carry out the task below immediately.\n\n\
         Instructions:\n{}",
        task.title, task.instructions
    );

    // Run the engine — it adds the user message internally with ts + source.
    // No separate push_message_sync needed; engine.messages() is authoritative.
    let events = eng.run(json!(&opening), None).await;
    let mut run_error: Option<String> = None;
    for ev in events {
        // Skip permission_required from the engine: auto-approved tools
        // (WriteLocal, name_allowed) don't need a dialog, and Inbox-parked
        // tools already get their own broadcast (with item_id) from within
        // the approver closure. Mirrors Python's _run_scheduled_task which
        // consumes events without broadcasting (async for _event in ...: pass).
        if ev.event_type == ocw_engine::EventType::PermissionRequired {
            continue;
        }
        // Capture the first engine error (mirrors Python's except branch,
        // which sets run.error and marks the run failed).
        if run_error.is_none() {
            if let ocw_engine::EventData::Error { error, .. } = &ev.data {
                run_error = Some(error.clone());
            }
        }
        let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
        state.broadcast_sync(&session_id, wire);
    }

    // Persist ALL messages from the engine (user with ts, assistant with
    // tool_calls/reasoning/ts, tool results, notices).
    let all_messages: Vec<Value> = eng
        .messages()
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect();

    // Final assistant text (mirrors Python's `_last_assistant_text`): the
    // last non-empty assistant content in the transcript. Computed before
    // `all_messages` is moved into the session cache below.
    let result_text = all_messages.iter().rev().find_map(|m| {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            m.get("content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        } else {
            None
        }
    });
    let existing = state.conversation_store.count_jsonl(&session_id);
    if all_messages.len() > existing {
        // If the JSONL is missing the system message that the engine has
        // (legacy files from before the format fix), do a full rewrite.
        let disk_missing_system = if existing > 0 {
            let disk = state.conversation_store.read_jsonl(&session_id);
            !disk
                .first()
                .and_then(|v| v.get("role"))
                .and_then(|v| v.as_str())
                .map(|r| r == "system")
                .unwrap_or(false)
        } else {
            false // new file: system will be written in first append
        };

        if disk_missing_system {
            let _ = state
                .conversation_store
                .rewrite_jsonl(&session_id, &all_messages);
        } else {
            let _ = state
                .conversation_store
                .append_jsonl(&session_id, &all_messages[existing..]);
        }
    }
    if let Ok(mut guard) = state.session_messages.write() {
        guard
            .entry(session_id.clone())
            .or_default()
            .messages = all_messages;
    }

    // Update SQLite metadata (message count, title, updated_at) and
    // kick off auto-titling — mirror of ws.rs post-turn housekeeping.
    let grants = eng.grants().await;
    state.persist_turn(&session_id);
    state.persist_grants(&session_id, &grants);
    state.maybe_autotitle(&session_id);

    // Broadcast turn_done
    state.broadcast_sync(&session_id, json!({"type": "turn_done", "data": {}}));

    // Finalize: update run status, result, artifacts and recompute next_run.
    // Mirrors Python: status is "ok"/"error", artifacts come from files in the
    // task workspace modified during the run (empty on failure).
    let status = if run_error.is_none() { "ok" } else { "error" };
    let artifacts = if run_error.is_none() {
        crate::state::recent_files(&task.workspace, started_at, 20)
    } else {
        Vec::new()
    };
    let next_run = compute_next_run(&task);
    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.finalize(
            &run_id,
            &task_id,
            status,
            run_error,
            next_run,
            result_text,
            artifacts,
        );
    }
}

async fn run_tick(state: &Arc<AppState>, trigger: String) {
    let due: Vec<_> = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.due()
    };
    for task in due {
        // skip-on-overlap (mirrors Python's `_running_ids`): don't stack a run
        // while the previous one for this task is still going.
        {
            let mut running = RUNNING_IDS.lock();
            if !running.insert(task.id.clone()) {
                continue;
            }
        }
        let state = Arc::clone(state);
        let trigger = trigger.clone();
        let task_id = task.id.clone();
        tokio::spawn(async move {
            let _ = std::panic::AssertUnwindSafe(run_task(&state, task, &trigger))
                .catch_unwind()
                .await;
            RUNNING_IDS.lock().remove(&task_id);
        });
    }
    // Board wake drain — mirrors Python's scheduler calling team_tick each tick.
    let _ = crate::team_tick::team_tick(state.as_ref()).await;
}

pub async fn start_scheduler(state: Arc<AppState>) {
    // Catch-up tick: fire anything that was missed while the server was down.
    // Mirrors Python's `_loop`: sleep *before* the first regular tick — tokio's
    // `interval` fires immediately on the first `.tick().await`, which would
    // re-dispatch the same due set right after catchup (and race with
    // still-parked approval runs once RUNNING_IDS clears).
    run_tick(&state, "catchup".to_string()).await;

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        run_tick(&state, "schedule".to_string()).await;
    }
}
