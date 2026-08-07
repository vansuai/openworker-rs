//! Automations (scheduled tasks) API handlers.
//!
//! Mirrors `coworker/server/app.py` endpoints + `coworker/server/manager.py` methods.
//! Storage is JSON-based (tasks + runs in data_dir/automations.json).

use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub kind: String, // "cron" | "once"
    pub cron: Option<String>,
    pub fire_at: Option<String>,
    #[serde(default = "default_tz")]
    pub timezone: String,
}

fn default_tz() -> String {
    "local".to_string()
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            kind: "cron".to_string(),
            cron: None,
            fire_at: None,
            timezone: "local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedEntry {
    pub entry: String,
    pub tool: String,
    pub target: Option<String>,
}

/// A scheduled automation task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub schedule: Schedule,
    pub workspace: String,
    #[serde(default)]
    pub origin_surface: String,
    #[serde(default)]
    pub origin_session_id: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "true_bool")]
    pub notify_on_completion: bool,
    #[serde(default)]
    pub notify_target: Option<String>,
    #[serde(default)]
    pub always_allowed_tools: Vec<String>,
    #[serde(default)]
    pub always_allowed_commands: Vec<String>,
    #[serde(default = "true_bool")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub updated_at: f64,
    #[serde(default, skip)]
    pub next_run: Option<f64>,
    #[serde(default)]
    pub last_run: Option<f64>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub run_count: i32,
    #[serde(default)]
    pub max_runs: Option<i32>,
    /// Runs started after this timestamp count as "unseen" (UX-023 sidebar badges).
    #[serde(default)]
    pub seen_runs_at: f64,
    /// The task's own conversation thread session ID.
    #[serde(default)]
    pub task_session_id: String,
}

fn default_agent() -> String {
    "cowork".to_string()
}
fn true_bool() -> bool {
    true
}

/// A single execution of a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub run_id: String,
    pub task_id: String,
    #[serde(default)]
    pub started_at: f64,
    #[serde(default)]
    pub finished_at: Option<f64>,
    #[serde(default = "default_status")]
    pub status: String, // "running" | "ok" | "error" | "skipped"
    #[serde(default)]
    pub result_text: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default = "default_trigger")]
    pub trigger: String, // "schedule" | "manual" | "catchup"
    #[serde(default)]
    pub session_id: String,
}

fn default_status() -> String {
    "running".to_string()
}
fn default_trigger() -> String {
    "schedule".to_string()
}

// ---------------------------------------------------------------------------
// AutomationStore
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AutomationStore {
    path: PathBuf,
    tasks: parking_lot::RwLock<HashMap<String, ScheduledTask>>,
    runs: parking_lot::RwLock<HashMap<String, Vec<TaskRun>>>,
}

impl AutomationStore {
    pub fn new(data_dir: PathBuf) -> Self {
        let path = data_dir.join("automations.json");
        let mut tasks: HashMap<String, ScheduledTask>;
        let runs;
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(Value::Object(obj)) = serde_json::from_str(&text) {
                tasks = serde_json::from_value(obj.get("tasks").cloned().unwrap_or(json!({})))
                    .unwrap_or_default();
                runs = serde_json::from_value(obj.get("runs").cloned().unwrap_or(json!({})))
                    .unwrap_or_default();
            } else {
                tasks = HashMap::new();
                runs = HashMap::new();
            }
        } else {
            tasks = HashMap::new();
            runs = HashMap::new();
        }
        // Compute next_run for all loaded tasks (prevents stale values from disk).
        for task in tasks.values_mut() {
            task.next_run = compute_next_run(task);
        }
        Self {
            path,
            tasks: parking_lot::RwLock::new(tasks),
            runs: parking_lot::RwLock::new(runs),
        }
    }

    fn persist(&self) {
        let tasks = self.tasks.read().clone();
        let runs = self.runs.read().clone();
        let obj = json!({ "tasks": tasks, "runs": runs });
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = serde_json::to_string_pretty(&obj).unwrap_or_default();
        let _ = std::fs::write(&self.path, text);
    }

    // --- task operations ---

    pub fn list(&self) -> Vec<ScheduledTask> {
        let tasks = self.tasks.read();
        let mut vals: Vec<_> = tasks.values().cloned().collect();
        vals.sort_by(|a, b| {
            a.next_run
                .unwrap_or(f64::INFINITY)
                .partial_cmp(&b.next_run.unwrap_or(f64::INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        vals
    }

    pub fn get(&self, task_id: &str) -> Option<ScheduledTask> {
        self.tasks.read().get(task_id).cloned()
    }

    pub fn save_task(&self, task: ScheduledTask) {
        self.tasks.write().insert(task.id.clone(), task);
        self.persist();
    }

    pub fn delete(&self, task_id: &str) -> bool {
        let removed = self.tasks.write().remove(task_id).is_some();
        if removed {
            self.runs.write().remove(task_id);
            self.persist();
        }
        removed
    }

    // --- run operations ---

    pub fn add_run(&self, run: TaskRun) -> TaskRun {
        let mut guard = self.runs.write();
        guard
            .entry(run.task_id.clone())
            .or_default()
            .push(run.clone());
        drop(guard);
        self.persist();
        run
    }

    pub fn get_run(&self, run_id: &str) -> Option<TaskRun> {
        let runs = self.runs.read();
        for task_runs in runs.values() {
            if let Some(r) = task_runs.iter().find(|r| r.run_id == run_id) {
                return Some(r.clone());
            }
        }
        None
    }

    pub fn runs(&self, task_id: &str) -> Vec<TaskRun> {
        self.runs.read().get(task_id).cloned().unwrap_or_default()
    }

    /// Finalize a run and update task metadata atomically under a single lock scope.
    pub fn finalize(
        &self,
        run_id: &str,
        task_id: &str,
        status: &str,
        error: Option<String>,
        next_run: Option<f64>,
    ) {
        let now = now_epoch();
        {
            let mut runs = self.runs.write();
            for task_runs in runs.values_mut() {
                if let Some(r) = task_runs.iter_mut().find(|r| r.run_id == run_id) {
                    r.status = status.to_string();
                    r.error = error;
                    r.finished_at = Some(now);
                    break;
                }
            }
        }
        {
            let mut tasks = self.tasks.write();
            if let Some(t) = tasks.get_mut(task_id) {
                t.last_run = Some(now);
                t.last_status = Some(status.to_string());
                t.run_count += 1;
                t.updated_at = now;
                t.next_run = next_run;
            }
        }
        self.persist();
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Compute the next UNIX epoch time for a task's schedule, or None if not schedulable.
pub fn compute_next_run(task: &ScheduledTask) -> Option<f64> {
    if !task.enabled {
        return None;
    }
    if let Some(max) = task.max_runs {
        if task.run_count >= max {
            return None;
        }
    }
    let now = now_epoch();
    match task.schedule.kind.as_str() {
        "once" => {
            let fa = task.schedule.fire_at.as_ref()?;
            let dt = chrono::NaiveDateTime::parse_from_str(fa, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(fa, "%Y-%m-%d %H:%M:%S"))
                .ok()?;
            let ts = dt.and_utc().timestamp();
            if ts as f64 > now && task.run_count == 0 {
                Some(ts as f64)
            } else {
                None
            }
        }
        "cron" => {
            let cron_str = task.schedule.cron.as_deref()?;
            let with_seconds = format!("0 {}", cron_str);
            cron::Schedule::from_str(&with_seconds)
                .ok()?
                .upcoming(chrono::Utc)
                .next()
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.timestamp() as f64)
        }
        _other => None,
    }
}

/// Return all enabled tasks whose next_run <= now, ordered by next_run.
impl AutomationStore {
    pub fn due(&self) -> Vec<ScheduledTask> {
        let now = now_epoch();
        let tasks = self.tasks.read();
        let mut out: Vec<_> = tasks
            .values()
            .filter(|t| t.enabled && t.next_run.map(|n| n <= now).unwrap_or(false))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.next_run
                .unwrap_or(f64::INFINITY)
                .partial_cmp(&b.next_run.unwrap_or(f64::INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }
}

fn short_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &uuid::Uuid::new_v4().to_string()[..10])
}

// ---------------------------------------------------------------------------
// Human-readable schedule formatting
// ---------------------------------------------------------------------------

fn schedule_human(sched: &Schedule) -> String {
    if sched.kind == "once" {
        if let Some(fa) = &sched.fire_at {
            return format!("Once at {}", fa);
        }
        return "Once".to_string();
    }
    let cron = match &sched.cron {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return "?".to_string(),
    };
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() != 5 {
        return cron;
    }
    let minute = parts[0];
    let hour = parts[1];
    let dom = parts[2];
    let dow = parts[3];

    let fmt_time = || -> String {
        let h: std::result::Result<i32, _> = hour.parse();
        let m: std::result::Result<i32, _> = minute.parse();
        if let (Ok(hh), Ok(mm)) = (h, m) {
            let ampm = if hh < 12 { "AM" } else { "PM" };
            let h12 = if hh == 0 {
                12
            } else if hh > 12 {
                hh - 12
            } else {
                hh
            };
            format!("{:02}:{:02} {}", h12, mm, ampm)
        } else {
            format!("{}:{}", hour, minute)
        }
    };

    if dom == "*" && dow == "*" {
        return format!("Every day at ~{}", fmt_time());
    }
    if dom == "*"
        && dow
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        if let Ok(d) = dow.parse::<usize>() {
            let day = days.get(d % 7).unwrap_or(&dow);
            return format!("Every {} at ~{}", day, fmt_time());
        }
    }
    if !dom.is_empty() && dom.chars().all(|c| c.is_ascii_digit()) && dow == "*" {
        return format!("Monthly on day {} at ~{}", dom, fmt_time());
    }
    cron
}

// ---------------------------------------------------------------------------
// Allowed tool helpers
// ---------------------------------------------------------------------------

fn parse_rule(entry: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = entry.trim().splitn(2, ' ').collect();
    let tool = parts.first().unwrap_or(&"").to_string();
    let target = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (tool, target)
}

fn public_task(task: &ScheduledTask) -> Value {
    let always_allowed: Vec<AllowedEntry> = task
        .always_allowed_tools
        .iter()
        .map(|e| {
            let (tool, target) = parse_rule(e);
            AllowedEntry {
                entry: e.clone(),
                tool,
                target,
            }
        })
        .collect();

    json!({
        "id": task.id,
        "title": task.title,
        "instructions": task.instructions,
        "schedule": schedule_human(&task.schedule),
        "schedule_raw": task.schedule,
        "workspace": task.workspace,
        "agent": task.agent,
        "enabled": task.enabled,
        "next_run": task.next_run,
        "last_run": task.last_run,
        "last_status": task.last_status,
        "run_count": task.run_count,
        "notify_on_completion": task.notify_on_completion,
        "seen_runs_at": task.seen_runs_at,
        "always_allowed": always_allowed,
    })
}

// ---------------------------------------------------------------------------
// Route handlers (use State<AppState> from the parent app)
// ---------------------------------------------------------------------------

type AppStateRef = crate::state::AppState;

/// GET /v1/automations
pub async fn handler_list(State(state): State<AppStateRef>) -> axum::Json<Value> {
    let all_tasks = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.list()
    };
    let mut tasks: Vec<Value> = Vec::new();
    for task in all_tasks {
        let runs = {
            let store: &AutomationStore = &*state.automations.read().await;
            store.runs(&task.id)
        };
        let unseen: Vec<_> = runs
            .iter()
            .filter(|r| r.started_at > task.seen_runs_at)
            .collect();
        let unseen_failed = unseen.first().map(|r| r.status == "error").unwrap_or(false);
        let mut v = public_task(&task);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("unseen_runs".into(), serde_json::json!(unseen.len()));
            obj.insert("unseen_failed".into(), serde_json::json!(unseen_failed));
        }
        tasks.push(v);
    }
    axum::Json(json!({ "tasks": tasks }))
}

/// POST /v1/automations
pub async fn handler_create(
    State(state): State<AppStateRef>,
    axum::Json(body): axum::Json<Value>,
) -> Result<axum::Json<Value>> {
    let store: &AutomationStore = &*state.automations.read().await;
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let cron = body
        .get("cron")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let fire_at = body
        .get("fire_at")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let timezone = body
        .get("timezone")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "local".to_string());
    let permissions: Vec<String> = body
        .get("permissions")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if title.is_empty() {
        return Err(Error::BadRequest("title is required".into()));
    }
    if instructions.is_empty() {
        return Err(Error::BadRequest("instructions are required".into()));
    }
    if cron.is_none() && fire_at.is_none() {
        return Err(Error::BadRequest(
            "provide cron (recurring) or fire_at (one-time)".into(),
        ));
    }

    let kind = if fire_at.is_some() && cron.is_none() {
        "once"
    } else {
        "cron"
    };

    if let Some(ref c) = cron {
        if !is_valid_cron(c) {
            return Err(Error::BadRequest(format!("invalid cron expression: {}", c)));
        }
    }

    let now = now_epoch();
    let task_id = short_id("task");
    let task_session_id = format!("__task__{}", task_id);

    let task = ScheduledTask {
        id: task_id.clone(),
        title: title.to_string(),
        instructions: instructions.to_string(),
        schedule: Schedule {
            kind: kind.to_string(),
            cron,
            fire_at,
            timezone,
        },
        workspace: String::new(),
        origin_surface: "cowork".to_string(),
        origin_session_id: String::new(),
        agent: "cowork".to_string(),
        model: None,
        notify_on_completion: true,
        notify_target: None,
        always_allowed_tools: permissions,
        always_allowed_commands: Vec::new(),
        enabled: true,
        created_at: now,
        updated_at: now,
        next_run: None,
        last_run: None,
        last_status: None,
        run_count: 0,
        max_runs: None,
        seen_runs_at: 0.0,
        task_session_id: task_session_id.clone(),
    };

    // Compute next_run immediately on creation so the task is scheduled
    let task_next_run = compute_next_run(&task);
    let task = ScheduledTask {
        next_run: task_next_run,
        ..task
    };

    store.save_task(task.clone());

    Ok(axum::Json(json!({
        "ok": true,
        "task": public_task(&task),
    })))
}

/// GET /v1/automations/{task_id}
pub async fn handler_get(
    State(state): State<AppStateRef>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>> {
    let task = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get(&task_id)
    };
    match task {
        Some(task) => {
            let runs: Vec<Value> = {
                let store: &AutomationStore = &*state.automations.read().await;
                store
                    .runs(&task_id)
                    .into_iter()
                    .map(|r| serde_json::to_value(&r).unwrap_or(json!({})))
                    .collect()
            };
            Ok(axum::Json(json!({
                "task": public_task(&task),
                "runs": runs,
            })))
        }
        None => Err(Error::NotFound),
    }
}

/// PATCH /v1/automations/{task_id}
pub async fn handler_update(
    State(state): State<AppStateRef>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Result<axum::Json<Value>> {
    let task = {
        let s: &AutomationStore = &*state.automations.write().await;
        s.get(&task_id)
    };
    let mut task = match task {
        Some(t) => t,
        None => return Err(Error::NotFound),
    };

    if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
        task.enabled = enabled;
    }
    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        task.instructions = instructions.to_string();
    }
    if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
        task.title = title.to_string();
    }
    if let Some(cron) = body.get("cron").and_then(|v| v.as_str()) {
        if !is_valid_cron(cron) {
            return Err(Error::BadRequest(format!("invalid cron: {}", cron)));
        }
        task.schedule.kind = "cron".to_string();
        task.schedule.cron = Some(cron.to_string());
    }

    task.updated_at = now_epoch();
    {
        let s: &AutomationStore = &*state.automations.write().await;
        s.save_task(task.clone());
    }

    Ok(axum::Json(json!({
        "ok": true,
        "task": public_task(&task),
    })))
}

/// DELETE /v1/automations/{task_id}
pub async fn handler_delete(
    State(state): State<AppStateRef>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> axum::Json<Value> {
    let ok = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.delete(&task_id)
    };
    axum::Json(json!({ "ok": ok, "id": task_id }))
}

/// POST /v1/automations/{task_id}/seen
pub async fn handler_mark_seen(
    State(state): State<AppStateRef>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>> {
    let task = {
        let s: &AutomationStore = &*state.automations.write().await;
        s.get(&task_id)
    };
    let mut task = match task {
        Some(t) => t,
        None => return Err(Error::NotFound),
    };

    task.seen_runs_at = now_epoch();
    task.updated_at = now_epoch();
    {
        let s: &AutomationStore = &*state.automations.write().await;
        s.save_task(task.clone());
    }

    Ok(axum::Json(json!({ "ok": true })))
}

/// POST /v1/automations/{task_id}/run
pub async fn handler_run(
    State(state): State<AppStateRef>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>> {
    let task = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get(&task_id)
    };
    let task = match task {
        Some(t) => t,
        None => return Err(Error::NotFound),
    };

    let run_id = short_id("run");
    let session_id = format!("__run__{}", run_id);
    let now = now_epoch();

    let run = TaskRun {
        run_id: run_id.clone(),
        task_id: task.id.clone(),
        started_at: now,
        finished_at: None,
        status: "running".to_string(),
        result_text: None,
        artifacts: Vec::new(),
        error: None,
        trigger: "manual".to_string(),
        session_id: session_id.clone(),
    };

    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.add_run(run.clone());
    }

    // Create the __run__ session so GET /v1/sessions/{id}/messages returns messages
    // (this matches Python server's get_engine() lazily creating sessions at WS connect time).
    state.get_or_create_session(&session_id, &task.agent, Some(&task.workspace));

    let prompt = format!(
        "⏰ Running automation '{}' now. Carry out these instructions immediately and produce the result.\n\n{}",
        task.title,
        task.instructions
    );

    Ok(axum::Json(json!({
        "ok": true,
        "run_id": run_id,
        "session_id": session_id,
        "workspace": task.workspace,
        "agent": task.agent,
        "prompt": prompt,
    })))
}

/// POST /v1/automations/{task_id}/runs/{run_id}/finalize
pub async fn handler_finalize(
    State(state): State<AppStateRef>,
    axum::extract::Path((task_id, run_id)): axum::extract::Path<(String, String)>,
    axum::Json(body): axum::Json<Value>,
) -> Result<axum::Json<Value>> {
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("ok");
    let error = body.get("error").and_then(|v| v.as_str()).map(String::from);

    // Verify run exists and matches task_id.
    let run = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get_run(&run_id)
    };
    let _run = match run {
        Some(r) if r.task_id == task_id => r,
        _ => return Err(Error::NotFound),
    };

    // Compute next_run so the task's schedule stays current after finalize.
    let next_run = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get(&task_id).as_ref().and_then(compute_next_run)
    };

    {
        let s: &AutomationStore = &*state.automations.write().await;
        s.finalize(&run_id, &task_id, status, error.clone(), next_run);
    }

    let updated_run = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get_run(&run_id)
    };

    Ok(axum::Json(json!({
        "ok": true,
        "run": updated_run,
    })))
}

// ---------------------------------------------------------------------------
// Cron validation (simplified)
// ---------------------------------------------------------------------------

fn is_valid_cron(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    parts.len() == 5 && parts.iter().all(|p| !p.is_empty())
}
