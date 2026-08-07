//! Background scheduler for automation tasks.
//!
//! Mirrors `coworker/automation/scheduler.py`:
//! - Every 30 seconds, queries due tasks and spawns background async tasks to run them.
//! - First tick on startup fires a "catchup" run for anything missed while the server was down.

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

use crate::automations::{compute_next_run, AutomationStore, ScheduledTask, TaskRun};
use crate::state::AppState;
use crate::ws::build_builtin_registry;

fn short_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &uuid::Uuid::new_v4().to_string()[..10])
}

fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

async fn run_task(state: &Arc<AppState>, task: ScheduledTask, trigger: &str) {
    let run_id = short_id("run");
    let session_id = format!("__run__{}", run_id);
    let started_at = epoch_now();

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
    };

    // Persist run as "running" so frontend can see it immediately
    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.add_run(run.clone());
    }

    // Create the __run__ session so GET /v1/sessions/{id}/messages works
    state.get_or_create_session(&session_id, &task.agent, Some(&task.workspace));

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
    );
    let permissions = Arc::new(tokio::sync::Mutex::new(ocw_engine::PermissionEngine::new(
        state.config.data_dir.join("permissions.json"),
    )));
    let mut eng = ocw_engine::TurnEngine::new(
        provider,
        registry,
        permissions,
        model,
        12,
        serde_json::Map::new(),
        vec![],
    );

    // Opening message — sent as the automation's first user turn
    let opening = format!(
        "⏰ Scheduled run — {}\n\n\
         This automation is due now: carry out the task below immediately.\n\n\
         Instructions:\n{}",
        task.title, task.instructions
    );

    // Persist the opening user message
    state.push_message_sync(
        &session_id,
        json!({
            "role": "user",
            "content": &opening,
        }),
    );

    // Run the engine and broadcast events
    let events = eng.run(json!(&opening), None).await;
    let mut assistant_text = String::new();
    for ev in events {
        let wire = serde_json::to_value(&ev).unwrap_or(json!({}));
        state.broadcast_sync(&session_id, wire.clone());

        // Persist assistant messages so re-opening shows history
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
                        &session_id,
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

    // Broadcast turn_done
    state.broadcast_sync(&session_id, json!({"type": "turn_done", "data": {}}));

    // Finalize: update run status and recompute next_run
    let status = if run.error.is_none() {
        "success"
    } else {
        "error"
    };
    let next_run = compute_next_run(&task);
    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.finalize(&run_id, &task.id, status, run.error.clone(), next_run);
    }
}

async fn run_tick(state: &Arc<AppState>, trigger: String) {
    let due: Vec<_> = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.due()
    };
    for task in due {
        let state = Arc::clone(state);
        let trigger = trigger.clone();
        tokio::spawn(async move {
            run_task(&state, task, &trigger).await;
        });
    }
}

pub async fn start_scheduler(state: Arc<AppState>) {
    // Catch-up tick: fire anything that was missed while the server was down
    run_tick(&state, "catchup".to_string()).await;

    // Regular 30-second tick loop
    let mut ticker = interval(Duration::from_secs(30));
    loop {
        ticker.tick().await;
        run_tick(&state, "schedule".to_string()).await;
    }
}
