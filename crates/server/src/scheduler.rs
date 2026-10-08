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
use crate::state::{memory_turn_context, AppState, SessionRunState};
use crate::ws::build_builtin_registry;
use ocw_data::{args_preview, VIS_INBOX};
use ocw_engine::{
    classify_risk, name_only_grantable, standing_target_candidate_with, ApprovalOutcome, Approver,
    PermissionRequest, RiskClass,
};
use std::path::PathBuf;

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
        // Scheduled-task engines DO get memory tools — Python's
        // `_build_task_engine` passes `memory_store=self.memory_store`,
        // `memory_workspace=self._memory_key_for(None, task.workspace)` (which
        // with no binding resolves to `project_key(task.workspace)`), and
        // `memory_saving_enabled=lambda: self.memory_settings.enabled`.
        Some((
            Arc::clone(&state.memory_store),
            Some(crate::projects::project_key(&task.workspace)),
            {
                let settings = state.memory_settings.clone();
                Arc::new(move || {
                    settings
                        .snapshot()
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true)
                })
            },
        )),
        Some(crate::connector_tools::ConnectorSessionArgs::from_store(
            state.settings.clone(),
            &state.connector_store,
        )),
    );
    // Workspace root = the task scratch (NOT data_dir/permissions.json). Path
    // scoping for WriteLocal must match the files the run actually touches.
    let workspace_root = PathBuf::from(&task.workspace);
    let permissions = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
        workspace_root.clone(),
    )));

    let fresh_system = state
        .build_system_messages(&task.agent, &task.workspace, &model)
        .await;

    // Seed task standing rules into the permission engine (mirrors Python's
    // `_seed_task_permissions`). Target-bound entries feed task_rules; name-only
    // legacy entries go into session_allow_tools for the auto-allow check below.
    seed_task_permissions(&permissions, &task).await;

    let approver = make_scheduled_approver(
        state.as_ref().clone(),
        &task,
        session_id.clone(),
        Arc::clone(&permissions),
    );

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
    .with_workspace_root(task.workspace.clone())
    .with_park_hook(Arc::new(move |msgs: &[ocw_engine::Message]| {
        let values: Vec<Value> = msgs
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect();
        park_state.persist_engine_messages_inner(&park_sid, &values);
    }))
    .with_context_provider({
        let settings = state.memory_settings.clone();
        move || {
            let saving_enabled = settings
                .snapshot()
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let date = chrono::Local::now()
                .format("Current date: %Y-%m-%d")
                .to_string();
            match memory_turn_context(saving_enabled) {
                Some(notice) => format!("{date}\n\n{notice}"),
                None => date,
            }
        }
    });
    {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert("session_id".into(), serde_json::Value::String(session_id.clone()));
        ctx_map.insert("agent".into(), serde_json::Value::String(task.agent.clone()));
        ctx_map.insert("workspace".into(), serde_json::Value::String(task.workspace.clone()));
        eng.set_audit_context(ctx_map);
    }

    // Register into running_engines before the turn (mirrors Python's
    // `self._engines[run.session_id] = engine`) so View-run / follow-up WS
    // reconnects reuse this engine + scheduled approver instead of minting a
    // fresh attended live-approver session.
    let run_state = {
        let mut map = state.running_engines.write();
        map.entry(session_id.clone())
            .or_insert_with(|| Arc::new(SessionRunState::new()))
            .clone()
    };
    *run_state.running.write() = true;
    // Clear any stale engine handle while we own the turn locally.
    *run_state.engine.write() = None;
    struct RunningGuard(Arc<SessionRunState>);
    impl Drop for RunningGuard {
        fn drop(&mut self) {
            *self.0.running.write() = false;
        }
    }
    let _running_guard = RunningGuard(Arc::clone(&run_state));

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
        // Skip permission_required from the engine: headless scheduled runs never
        // push a live approval card (Python discards all events). Ungranted tools
        // park in the Inbox only — no session WS `permission_required` broadcast.
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

    // Keep the engine available for follow-up turns (standing rules already seeded).
    *run_state.engine.write() = Some(eng);

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

/// Seed a task's standing allowances into a permission engine.
/// Target-bound entries → `task_rules`; name-only legacy entries → session allow-list.
pub(crate) async fn seed_task_permissions(
    permissions: &Arc<tokio::sync::Mutex<ocw_engine::PermissionEngine>>,
    task: &ScheduledTask,
) {
    let mut perms = permissions.lock().await;
    let mut rules: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in &task.always_allowed_tools {
        let (tool, target) = crate::automations::parse_rule(entry);
        if !tool.is_empty() {
            if let Some(t) = target {
                rules.entry(tool).or_default().insert(t);
            } else {
                perms.allow_tool_for_session(tool);
            }
        }
    }
    perms.set_task_rules(rules);
}

/// Headless scheduled-run approver — mirrors Python's `_scheduled_approver`:
///   - Auto-allow WriteLocal tools (path-scoped by the permission engine).
///   - Auto-allow name-only legacy tools from the task config.
///   - Everything else parks in the Inbox (§25). No live `permission_required`
///     broadcast (Python never pushes a session card for scheduled runs).
pub(crate) fn make_scheduled_approver(
    state: AppState,
    task: &ScheduledTask,
    session_id: String,
    permissions: Arc<tokio::sync::Mutex<ocw_engine::PermissionEngine>>,
) -> Approver {
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
    let task_id = task.id.clone();
    let task_title = task.title.clone();
    let agent = task.agent.clone();
    Arc::new(move |request: PermissionRequest| {
        let state = state.clone();
        let session_id = session_id.clone();
        let agent = agent.clone();
        let name_allowed = name_allowed.clone();
        let task_id = task_id.clone();
        let task_title = task_title.clone();
        let permissions = Arc::clone(&permissions);
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
            // Do NOT broadcast permission_required — that would pop a live
            // ApprovalCard for anyone watching the __run__ session (View run).
            let args_value = {
                let mut obj = serde_json::Map::new();
                for (k, v) in req.arguments.iter() {
                    obj.insert(k.clone(), v.clone());
                }
                serde_json::Value::Object(obj)
            };
            let standing_target = {
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
                standing_target_candidate_with(&req.tool_name, &req.arguments, meta.as_ref())
            };
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
            let name_allow = standing_target.is_none() && name_only_grantable(&req.tool_name);
            let data = serde_json::json!({
                "tool": req.tool_name,
                "arguments": args_value,
                "task_id": task_id,
                "task_title": task_title,
                "standing_target": standing_target,
                "name_allow": name_allow,
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
            // Suspend until a surface resolves the item (Inbox / Slack / etc.).
            let resolution = state.inbox_store.wait(&item.id).await;
            if resolution == "always_task" {
                // Mint a standing rule on the owning task ("Allow every time",
                // §25). Target-bound tools use add_rule; name-only grantable
                // tools (web_search) use add_name_allow + session allow.
                let store = state.automations.read().await;
                if let Some(mut task) = store.task_for_run_session(&session_id) {
                    if let Some(target) = standing_target {
                        if task.add_rule(&req.tool_name, &target) {
                            store.save_task(task.clone());
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
                    } else if name_only_grantable(&req.tool_name) {
                        if task.add_name_allow(&req.tool_name) {
                            store.save_task(task.clone());
                            let mut guard = permissions.lock().await;
                            guard.allow_tool_for_session(req.tool_name.clone());
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
                                    "allow every time: {} (name-only, task {})",
                                    req.tool_name, task.id
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
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Config;
    use ocw_engine::PermissionRequest;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ocw-sched-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_state(dir: PathBuf) -> AppState {
        let config = Config {
            data_dir: dir,
            ..Config::default()
        };
        let provider: Arc<dyn ocw_provider::Provider> =
            Arc::new(ocw_provider::Router::new("anthropic"));
        AppState::new(config, provider)
    }

    fn sample_task(id: &str, workspace: &str, always: Vec<String>) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            title: "T".to_string(),
            instructions: "do it".to_string(),
            schedule: crate::automations::Schedule::default(),
            workspace: workspace.to_string(),
            origin_surface: String::new(),
            origin_session_id: String::new(),
            agent: "cowork".to_string(),
            model: None,
            notify_on_completion: true,
            notify_target: None,
            always_allowed_tools: always,
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

    fn req(tool: &str, args: serde_json::Map<String, Value>) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool.to_string(),
            arguments: args,
            reason: "requires approval".to_string(),
            category: String::new(),
            tool_call_id: Some("tc1".to_string()),
        }
    }

    /// WriteLocal tools auto-allow without parking — mirrors
    /// `test_scheduled_approver_name_allows_and_denies` WriteLocal path.
    #[tokio::test]
    async fn scheduled_approver_write_local_auto_allows() {
        let dir = temp_dir("write");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let state = make_state(dir.clone());
        let task = sample_task("t1", &ws.to_string_lossy(), vec![]);
        let session_id = "__run__run-w1".to_string();
        let perms = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            ws.clone(),
        )));
        let approver = make_scheduled_approver(state.clone(), &task, session_id.clone(), perms);

        let mut args = serde_json::Map::new();
        args.insert("path".into(), json!("out.md"));
        let outcome = approver(req("write_file", args)).await.expect("ok");
        assert!(matches!(outcome, ApprovalOutcome::Once));
        assert!(
            state.inbox_store.pending(Some(&session_id)).is_empty(),
            "WriteLocal must not park in Inbox"
        );
    }

    /// Name-only legacy grant auto-allows without parking.
    #[tokio::test]
    async fn scheduled_approver_name_allowed_auto_allows() {
        let dir = temp_dir("name");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let state = make_state(dir.clone());
        let task = sample_task(
            "t2",
            &ws.to_string_lossy(),
            vec!["web_search".to_string()],
        );
        let session_id = "__run__run-n1".to_string();
        let perms = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            ws.clone(),
        )));
        let approver = make_scheduled_approver(state.clone(), &task, session_id.clone(), perms);

        let mut args = serde_json::Map::new();
        args.insert("query".into(), json!("x"));
        let outcome = approver(req("web_search", args)).await.expect("ok");
        assert!(matches!(outcome, ApprovalOutcome::Once));
        assert!(state.inbox_store.pending(Some(&session_id)).is_empty());
    }

    /// Ungranted external tool parks in Inbox and must NOT broadcast
    /// permission_required (the bug that popped a live ApprovalCard).
    #[tokio::test]
    async fn scheduled_approver_parks_without_broadcast() {
        let dir = temp_dir("park");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let state = make_state(dir.clone());
        let task = sample_task("t3", &ws.to_string_lossy(), vec![]);
        {
            let store = state.automations.read().await;
            store.save_task(task.clone());
            store.add_run(TaskRun {
                run_id: "run-p1".to_string(),
                task_id: "t3".to_string(),
                started_at: 1.0,
                finished_at: None,
                status: "running".to_string(),
                result_text: None,
                artifacts: Vec::new(),
                error: None,
                trigger: "schedule".to_string(),
                session_id: "__run__run-p1".to_string(),
                model: None,
            });
        }
        let session_id = "__run__run-p1".to_string();
        let mut rx = state.register_ws_async(&session_id).await;
        let perms = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            ws.clone(),
        )));
        let approver = make_scheduled_approver(state.clone(), &task, session_id.clone(), perms);

        let mut args = serde_json::Map::new();
        args.insert("target".into(), json!("slack:T1/C1"));
        args.insert("text".into(), json!("digest"));
        let handle = tokio::spawn(approver(req("send_message", args)));

        let mut item = None;
        for _ in 0..200 {
            let pending = state.inbox_store.pending(Some(&session_id));
            if let Some(i) = pending.into_iter().next() {
                item = Some(i);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let item = item.expect("ungranted tool must park in Inbox");
        assert_eq!(item.visibility, VIS_INBOX);
        assert_eq!(item.data["task_id"], "t3");
        assert_eq!(item.data["standing_target"], "slack:T1/C1");

        let recv = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(
            recv.is_err(),
            "scheduled approver must not broadcast permission_required"
        );

        state.inbox_store.resolve(&item.id, "once");
        let outcome = handle.await.expect("join").expect("result");
        assert!(matches!(outcome, ApprovalOutcome::Once));
    }

    /// always_task on web_search mints a name-only grant on the task.
    #[tokio::test]
    async fn scheduled_approver_always_task_mints_web_search() {
        let dir = temp_dir("mint-ws");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let state = make_state(dir.clone());
        let task = sample_task("t-ws", &ws.to_string_lossy(), vec![]);
        {
            let store = state.automations.read().await;
            store.save_task(task.clone());
            store.add_run(TaskRun {
                run_id: "run-ws1".to_string(),
                task_id: "t-ws".to_string(),
                started_at: 1.0,
                finished_at: None,
                status: "running".to_string(),
                result_text: None,
                artifacts: Vec::new(),
                error: None,
                trigger: "schedule".to_string(),
                session_id: "__run__run-ws1".to_string(),
                model: None,
            });
        }
        let session_id = "__run__run-ws1".to_string();
        let perms = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            ws.clone(),
        )));
        let approver =
            make_scheduled_approver(state.clone(), &task, session_id.clone(), Arc::clone(&perms));

        let mut args = serde_json::Map::new();
        args.insert("query".into(), json!("tech news"));
        let handle = tokio::spawn(approver(req("web_search", args)));

        let mut item = None;
        for _ in 0..200 {
            let pending = state.inbox_store.pending(Some(&session_id));
            if let Some(i) = pending.into_iter().next() {
                item = Some(i);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let item = item.expect("web_search must park without name-only grant");
        assert_eq!(item.data["name_allow"], true);
        assert!(item.data["standing_target"].is_null());

        state.inbox_store.resolve(&item.id, "always_task");
        let outcome = handle.await.expect("join").expect("result");
        assert!(matches!(outcome, ApprovalOutcome::Once));

        let saved = state
            .automations
            .read()
            .await
            .get("t-ws")
            .expect("task");
        assert_eq!(saved.always_allowed_tools, vec!["web_search".to_string()]);
        // Hot-updated session allow so the same run's next search skips the card.
        let guard = perms.lock().await;
        let hit = guard.evaluate("web_search", &json!({"query": "more"}), None);
        assert!(hit.allowed, "session allow after mint: {hit:?}");
    }

    /// Standing tool→target rules allow at evaluate time (never reach approver).
    #[tokio::test]
    async fn standing_rule_allows_without_approver() {
        let dir = temp_dir("stand");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let task = sample_task(
            "t4",
            &ws.to_string_lossy(),
            vec!["send_message slack:T1/C1".to_string()],
        );
        let perms = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
            ws.clone(),
        )));
        seed_task_permissions(&perms, &task).await;
        let guard = perms.lock().await;
        let meta = json!({"requires_approval": true, "category": "connector"});
        let hit = guard.evaluate(
            "send_message",
            &json!({"target": "slack:T1/C1"}),
            Some(&meta),
        );
        assert!(hit.allowed, "standing rule must allow: {hit:?}");
        assert!(!hit.rule.is_empty());
        let miss = guard.evaluate(
            "send_message",
            &json!({"target": "slack:T1/C2"}),
            Some(&meta),
        );
        assert!(!miss.allowed && miss.needs_user);
    }
}
