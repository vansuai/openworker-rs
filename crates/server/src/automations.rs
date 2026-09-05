//! Automations (scheduled tasks) API handlers.
//!
//! Mirrors `coworker/server/app.py` endpoints + `coworker/server/manager.py` methods.
//! Storage is JSON-based (tasks + runs in data_dir/automations.json).

use axum::extract::State;
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use crate::error::{Error, Result};
use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};

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
    /// Model used for this run (resolved from task.model or server default).
    #[serde(default)]
    pub model: Option<String>,
}

fn default_status() -> String {
    "running".to_string()
}
fn default_trigger() -> String {
    "schedule".to_string()
}

impl ScheduledTask {
    /// Add a standing rule entry ("tool target"); returns false when the tool or
    /// target is empty or the entry already exists. Mirrors Python
    /// `ScheduledTask.add_rule` (`coworker/automation/models.py`).
    pub fn add_rule(&mut self, tool: &str, target: &str) -> bool {
        let entry = if target.is_empty() {
            tool.to_string()
        } else {
            format!("{} {}", tool, target)
        };
        if tool.is_empty() || target.is_empty() || self.always_allowed_tools.contains(&entry) {
            return false;
        }
        self.always_allowed_tools.push(entry);
        true
    }
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

    /// Reverse-lookup the owning task for a run session id (`__run__<run_id>`).
    /// Mirrors Python `TaskStore.task_for_run_session`
    /// (`coworker/automation/store.py`).
    pub fn task_for_run_session(&self, session_id: &str) -> Option<ScheduledTask> {
        let run_id = session_id.strip_prefix("__run__")?;
        let run = self.get_run(run_id)?;
        self.get(&run.task_id)
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
        result_text: Option<String>,
        artifacts: Vec<String>,
    ) {
        let now = now_epoch();
        {
            let mut runs = self.runs.write();
            for task_runs in runs.values_mut() {
                if let Some(r) = task_runs.iter_mut().find(|r| r.run_id == run_id) {
                    r.status = status.to_string();
                    r.error = error;
                    r.result_text = result_text;
                    r.artifacts = artifacts;
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

/// Resolve a schedule timezone for next-run math. Mirrors Python's `_tz`:
/// 'local'/empty → the machine's local zone; a valid IANA name → that zone;
/// anything else falls back to local. Returns None for "use chrono::Local".
fn resolve_schedule_tz(name: &str) -> Option<chrono_tz::Tz> {
    if name.is_empty() || name.eq_ignore_ascii_case("local") {
        return None;
    }
    name.parse::<chrono_tz::Tz>().ok()
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
    let named_tz = resolve_schedule_tz(&task.schedule.timezone);
    match task.schedule.kind.as_str() {
        "once" => {
            let fa = task.schedule.fire_at.as_ref()?;
            // RFC3339 (with an explicit offset) parses as-is; a naive datetime is
            // interpreted in the schedule's timezone (mirrors Python, which attaches
            // `_tz(sched.timezone)` to naive fire_at values).
            let ts = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(fa) {
                dt.timestamp()
            } else {
                let naive = chrono::NaiveDateTime::parse_from_str(fa, "%Y-%m-%dT%H:%M:%S")
                    .or_else(|_| chrono::NaiveDateTime::parse_from_str(fa, "%Y-%m-%d %H:%M:%S"))
                    .ok()?;
                let resolved = match named_tz {
                    Some(tz) => tz
                        .from_local_datetime(&naive)
                        .earliest()
                        .map(|dt| dt.timestamp()),
                    None => chrono::Local
                        .from_local_datetime(&naive)
                        .earliest()
                        .map(|dt| dt.timestamp()),
                };
                resolved?
            };
            if ts as f64 > now && task.run_count == 0 {
                Some(ts as f64)
            } else {
                None
            }
        }
        "cron" => {
            let cron_str = task.schedule.cron.as_deref()?;
            let with_seconds = format!("0 {}", cron_str);
            let schedule = cron::Schedule::from_str(&with_seconds).ok()?;
            // Evaluate the cron wall-clock in the schedule's timezone (not UTC):
            // '10:00 local' must fire at 10:00 on the machine's clock.
            match named_tz {
                Some(tz) => schedule
                    .upcoming(tz)
                    .next()
                    .map(|dt| dt.timestamp() as f64),
                None => schedule
                    .upcoming(chrono::Local)
                    .next()
                    .map(|dt: chrono::DateTime<chrono::Local>| dt.timestamp() as f64),
            }
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

pub(crate) fn parse_rule(entry: &str) -> (String, Option<String>) {
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

/// Allocate (idempotently) and return a task's scratch workspace directory —
/// `scratch_base/__task__{id}`. Mirror of `coworker/server/manager.py::_provision_scratch`.
pub(crate) async fn provision_scratch(state: &crate::state::AppState, session_id: &str) -> String {
    let scratch_base = {
        let prefs = state.settings.get_settings().await;
        prefs
            .get("scratch_base")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.starts_with('~') {
                    dirs::home_dir()
                        .map(|h| s.replacen('~', &h.to_string_lossy(), 1))
                        .unwrap_or_else(|| s.to_string())
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|p| p.join("OpenWorker").to_string_lossy().to_string())
                    .unwrap_or_default()
            })
    };
    let d = PathBuf::from(&scratch_base).join(session_id);
    let _ = std::fs::create_dir_all(&d);
    d.canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| d.to_string_lossy().to_string())
}

/// Ensure a task's workspace directory exists before a run. Legacy tasks created
/// before the scratch-provision fix carry an empty workspace — allocate one now
/// and persist it so the run (and its artifacts) land in a real directory.
/// Mirrors Python's `prepare_manual_run` mkdir plus the scheduler's legacy fix.
pub(crate) async fn ensure_task_workspace(
    state: &crate::state::AppState,
    task: ScheduledTask,
) -> ScheduledTask {
    let mut task = task;
    if task.task_session_id.trim().is_empty() {
        task.task_session_id = format!("__task__{}", task.id);
    }
    let task = if task.workspace.trim().is_empty() {
        let ws = provision_scratch(state, &task.task_session_id).await;
        let mut t = task;
        t.workspace = ws;
        {
            let store: &AutomationStore = &*state.automations.write().await;
            store.save_task(t.clone());
        }
        t
    } else {
        task
    };
    let _ = std::fs::create_dir_all(&task.workspace);
    task
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

    // Mirror Python's `create_automation`: the task gets its own scratch
    // workspace (scratch_base/__task__{id}) so scheduled runs write files into
    // a real directory and artifact previews resolve.
    let workspace = provision_scratch(&state, &task_session_id).await;

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
        workspace,
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
    // Mirror Python `prepare_manual_run`: the workspace must exist before the
    // run session starts; legacy empty-workspace tasks get the scratch fix too.
    let task = ensure_task_workspace(&state, task).await;

    let run_id = short_id("run");
    let session_id = format!("__run__{}", run_id);
    let now = now_epoch();

    let effective_model = task
        .model
        .clone()
        .unwrap_or_else(|| state.default_model_or_configured());

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
        model: Some(effective_model.clone()),
    };

    {
        let store: &AutomationStore = &*state.automations.read().await;
        store.add_run(run.clone());
    }

    // Create the __run__ session so GET /v1/sessions/{id}/messages returns messages
    // (this matches Python server's get_engine() lazily creating sessions at WS connect time).
    state.get_or_create_session(
        &session_id,
        &task.agent,
        Some(&task.workspace),
        Some(&effective_model),
    );

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
        "model": effective_model,
        "prompt": prompt,
    })))
}

/// POST /v1/automations/{task_id}/runs/{run_id}/finalize
pub async fn handler_finalize(
    State(state): State<AppStateRef>,
    axum::extract::Path((task_id, run_id)): axum::extract::Path<(String, String)>,
    body: Option<axum::Json<Value>>,
) -> Result<axum::Json<Value>> {
    // Body is optional: the GUI sends a bare POST (no Content-Type, no body),
    // matching the Python endpoint `automation_run_finalize(task_id, run_id)`
    // (no body parameter). axum's `Json` extractor rejects a missing JSON body
    // with 415, which silently left manual runs forever "running" — extract as
    // `Option` and default to an empty object instead.
    let body = body.map(|axum::Json(v)| v).unwrap_or(json!({}));
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("ok");
    let error = body.get("error").and_then(|v| v.as_str()).map(String::from);

    // Verify run exists and matches task_id.
    let run = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get_run(&run_id)
    };
    let run = match run {
        Some(r) if r.task_id == task_id => r,
        _ => return Err(Error::NotFound),
    };

    // Load the task for workspace + next_run (mirrors Python's
    // `finalize_manual_run`, which re-reads the task before finishing).
    let task = {
        let store: &AutomationStore = &*state.automations.read().await;
        store.get(&task_id)
    };
    let next_run = task.as_ref().and_then(compute_next_run);

    // Pull result text from the persisted transcript (mirrors
    // `_last_assistant_text`: last non-empty assistant content).
    let result_text = state.session_messages.read().ok().and_then(|guard| {
        guard.get(&run.session_id).and_then(|m| {
            m.messages.iter().rev().find_map(|msg| {
                if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                    msg.get("content")
                        .and_then(|c| c.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                } else {
                    None
                }
            })
        })
    });

    // Artifacts: files in the task workspace modified during the run.
    let artifacts = if status == "ok" {
        task.as_ref()
            .map(|t| crate::state::recent_files(&t.workspace, run.started_at, 20))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    {
        let s: &AutomationStore = &*state.automations.write().await;
        s.finalize(
            &run_id,
            &task_id,
            status,
            error.clone(),
            next_run,
            result_text,
            artifacts,
        );
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

// ---------------------------------------------------------------------------
// Agent-facing scheduling tools (mirror of coworker/automation/tools.py)
// ---------------------------------------------------------------------------

/// Origin context for agent-created scheduled tasks — the launching session's
/// surface/workspace, mirroring Python's `origin` dict in `agent.py`.
#[derive(Clone)]
pub(crate) struct SchedulingOrigin {
    pub workspace: String,
    pub session_id: String,
    pub surface: String, // agent name
    pub agent: String,
}

/// Validate a proposed `permissions` list down to the entries actually
/// grantable. Mirror of Python's `grant_entries` (`automation/models.py`):
/// only `access: "write"` items whose tool declares a target argument (which
/// excludes exec/destructive tools by construction) and whose target is
/// non-empty become grants; reads are disclosure-only and dropped. Fail-closed.
fn grant_entries(permissions: Option<&Value>) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let Some(arr) = permissions.and_then(|v| v.as_array()) else {
        return entries;
    };
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let access = obj.get("access").and_then(|v| v.as_str()).unwrap_or("");
        if !access.eq_ignore_ascii_case("write") {
            continue;
        }
        let tool = obj
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let target = obj
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if tool.is_empty() || target.is_empty() || ocw_engine::target_arg_for(&tool).is_none() {
            continue;
        }
        let entry = format!("{} {}", tool, target);
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries
}

const CREATE_TASK_DESC: &str = "Create a scheduled automation that re-runs `instructions` on a schedule. Convert the user's natural-language timing into a cron expression yourself (e.g. 'every day at 7:10pm' → '10 19 * * *'), or pass a one-time `fire_at` ISO datetime. The user confirms before it is created.";

fn create_task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string", "description": "Short label, e.g. 'Daily news briefing'."},
            "instructions": {"type": "string", "description": "What to do on each run, written as a direct command to execute immediately. Do NOT restate the schedule or timing here — timing belongs in cron/fire_at; this text is handed verbatim to the agent every run."},
            "cron": {"type": "string", "description": "5-field cron, e.g. '10 19 * * *'. Omit for one-time."},
            "fire_at": {"type": "string", "description": "ISO datetime for a one-time run. Omit for recurring."},
            "timezone": {"type": "string", "description": "IANA tz, e.g. 'America/New_York'. Defaults to the machine's local time — pass it only to override."},
            "permissions": {
                "type": "array",
                "description": "What this automation will touch, surfaced on the creation consent card. Reads (access:'read') are disclosure only. Writes (access:'write') become standing grants IF the user approves. Targets must be exact — no wildcards.",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string"},
                        "target": {"type": "string"},
                        "access": {"type": "string", "enum": ["read", "write"]}
                    },
                    "required": ["tool", "target", "access"]
                }
            }
        },
        "required": ["title", "instructions"]
    })
}

/// Register the four agent-facing scheduling tools into a live-session registry.
/// Mirrors Python's `scheduling_tools()` (`automation/tools.py`): gated with
/// approval for create/update/delete (approve-at-creation), origin-bound
/// workspace (the launching session's folder, never scratch). Scheduled-run
/// engines do not call this (Python's `task_store=None`).
pub(crate) fn register_scheduling_tools(
    registry: &mut ToolRegistry,
    store: Arc<AutomationStore>,
    origin: SchedulingOrigin,
) {
    // -- create_scheduled_task ------------------------------------------------
    let create_store = Arc::clone(&store);
    let create_origin = origin.clone();
    let create: ToolFn = Arc::new(move |args: serde_json::Map<String, Value>| {
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let instructions = args
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let cron = args
            .get("cron")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let fire_at = args
            .get("fire_at")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let timezone = args
            .get("timezone")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".to_string());
        if cron.is_none() && fire_at.is_none() {
            return ToolResult::ok(json!({
                "error": "provide a cron (recurring) or a fire_at ISO datetime (one-time)"
            }));
        }
        if let Some(ref c) = cron {
            if !is_valid_cron(c) {
                return ToolResult::ok(json!({"error": format!("invalid cron expression: {c}")}));
            }
        }
        let kind = if fire_at.is_some() && cron.is_none() {
            "once"
        } else {
            "cron"
        };
        // Origin-bound: the launching session's workspace (never scratch — the
        // scratch allocation belongs to the GUI create path only).
        let workspace = create_origin.workspace.clone();
        let grants = grant_entries(args.get("permissions"));
        let now = now_epoch();
        let task_id = short_id("task");
        let task_session_id = format!("__task__{}", task_id);
        let task = ScheduledTask {
            id: task_id,
            title: title.clone(),
            instructions,
            schedule: Schedule {
                kind: kind.to_string(),
                cron,
                fire_at,
                timezone,
            },
            workspace: workspace.clone(),
            origin_surface: create_origin.surface.clone(),
            origin_session_id: create_origin.session_id.clone(),
            agent: create_origin.agent.clone(),
            model: None,
            notify_on_completion: true,
            notify_target: None,
            always_allowed_tools: grants.clone(),
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
            task_session_id,
        };
        let next_run = compute_next_run(&task);
        let task = ScheduledTask { next_run, ..task };
        create_store.save_task(task.clone());
        ToolResult::ok(json!({
            "ok": true,
            "id": task.id,
            "title": title,
            "schedule": schedule_human(&task.schedule),
            "next_run": task.next_run,
            "workspace": workspace,
            "always_allowed": grants,
        }))
    });
    registry.register(
        "create_scheduled_task",
        create,
        ToolSpec {
            risk_level: "write",
            category: "automation",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "create_scheduled_task",
            Some(CREATE_TASK_DESC),
            Some(create_task_schema()),
        )),
    );

    // -- list_scheduled_tasks ------------------------------------------------
    let list_store = Arc::clone(&store);
    let list: ToolFn = Arc::new(move |_args: serde_json::Map<String, Value>| {
        let tasks: Vec<Value> = list_store.list().iter().map(public_task).collect();
        ToolResult::ok(json!({ "tasks": tasks }))
    });
    registry.register(
        "list_scheduled_tasks",
        list,
        ToolSpec {
            risk_level: "low",
            category: "automation",
            parallel_safe: true,
        },
        Some(ToolSchema::new(
            "list_scheduled_tasks",
            Some("List the user's scheduled tasks (title, schedule, next run, status)."),
            Some(json!({"type": "object", "properties": {}})),
        )),
    );

    // -- update_scheduled_task -----------------------------------------------
    let update_store = Arc::clone(&store);
    let update: ToolFn = Arc::new(move |args: serde_json::Map<String, Value>| {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut task = match update_store.get(&id) {
            Some(t) => t,
            None => return ToolResult::ok(json!({ "error": format!("no such task: {id}") })),
        };
        if let Some(cron_val) = args.get("cron") {
            if let Some(cron) = cron_val.as_str() {
                let cron = cron.trim().to_string();
                if !is_valid_cron(&cron) {
                    return ToolResult::ok(json!({
                        "error": format!("invalid cron expression: {cron}")
                    }));
                }
                task.schedule.cron = Some(cron);
                task.schedule.kind = "cron".to_string();
            }
        }
        if let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) {
            task.enabled = enabled;
        }
        if let Some(instructions) = args.get("instructions").and_then(|v| v.as_str()) {
            task.instructions = instructions.to_string();
        }
        if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
            task.title = title.to_string();
        }
        // Mirror Python's `TaskStore.save`: refresh updated_at and next_run.
        task.updated_at = now_epoch();
        task.next_run = compute_next_run(&task);
        update_store.save_task(task.clone());
        ToolResult::ok(json!({ "ok": true, "task": public_task(&task) }))
    });
    registry.register(
        "update_scheduled_task",
        update,
        ToolSpec {
            risk_level: "write",
            category: "automation",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "update_scheduled_task",
            Some("Enable/disable or edit a scheduled task (its instructions, cron, or title)."),
            Some(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "enabled": {"type": "boolean"},
                    "instructions": {"type": "string"},
                    "cron": {"type": "string"},
                    "title": {"type": "string"}
                },
                "required": ["id"]
            })),
        )),
    );

    // -- delete_scheduled_task -----------------------------------------------
    let delete_store = Arc::clone(&store);
    let delete: ToolFn = Arc::new(move |args: serde_json::Map<String, Value>| {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let ok = delete_store.delete(&id);
        ToolResult::ok(json!({ "ok": ok, "id": id }))
    });
    registry.register(
        "delete_scheduled_task",
        delete,
        ToolSpec {
            risk_level: "write",
            category: "automation",
            parallel_safe: false,
        },
        Some(ToolSchema::new(
            "delete_scheduled_task",
            Some("Delete a scheduled task and its run history."),
            Some(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            })),
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use crate::state::AppState;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ocw-auto-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_task(id: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            title: "T".to_string(),
            instructions: "do it".to_string(),
            schedule: Schedule::default(),
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

    fn sample_run(run_id: &str, task_id: &str, session_id: &str) -> TaskRun {
        TaskRun {
            run_id: run_id.to_string(),
            task_id: task_id.to_string(),
            started_at: 1.0,
            finished_at: None,
            status: "running".to_string(),
            result_text: None,
            artifacts: Vec::new(),
            error: None,
            trigger: "manual".to_string(),
            session_id: session_id.to_string(),
            model: None,
        }
    }

    #[test]
    fn add_rule_dedups_and_rejects_empty() {
        let mut t = sample_task("task-1");
        assert!(t.add_rule("send_message", "alice"));
        assert_eq!(
            t.always_allowed_tools,
            vec!["send_message alice".to_string()]
        );
        assert!(!t.add_rule("send_message", "alice"), "duplicate must be rejected");
        assert!(!t.add_rule("", "alice"), "empty tool must be rejected");
        assert!(!t.add_rule("send_message", ""), "empty target must be rejected");
        assert_eq!(t.always_allowed_tools.len(), 1);
    }

    #[test]
    fn task_for_run_session_resolves_owner() {
        let store = AutomationStore::new(temp_dir("tfs"));
        let mut task = sample_task("task-abc");
        task.always_allowed_tools = vec!["send_message alice".to_string()];
        store.save_task(task);
        store.add_run(sample_run("run-xyz", "task-abc", "__run__run-xyz"));

        let found = store
            .task_for_run_session("__run__run-xyz")
            .expect("run session must resolve its owning task");
        assert_eq!(found.id, "task-abc");
        assert_eq!(found.always_allowed_tools, vec!["send_message alice".to_string()]);

        assert!(store.task_for_run_session("plain-session").is_none());
        assert!(store.task_for_run_session("__run__nope").is_none());
    }

    #[test]
    fn grant_entries_filters_and_dedups() {
        let permissions = json!({
            "permissions": [
                {"tool": "connector__slack", "target": "#gen", "access": "write"},
                {"tool": "send_message", "target": "alice", "access": "WRITE"},
                {"tool": "send_message", "target": "alice", "access": "write"},
                {"tool": "connector__slack", "target": "#gen", "access": "read"},
                {"tool": "web_search", "target": "q", "access": "write"},
                {"tool": "write_file", "target": "p", "access": "write"},
                {"tool": "send_message", "target": "", "access": "write"}
            ]
        });
        assert_eq!(
            grant_entries(permissions.get("permissions")),
            vec![
                "connector__slack #gen".to_string(),
                "send_message alice".to_string(),
            ]
        );
        assert_eq!(grant_entries(None), Vec::<String>::new());
    }

    // -- schedule timezone handling (parity with Python's _tz + croniter) -----

    #[test]
    fn cron_utc_timezone_fires_at_utc_wall_clock() {
        let mut t = sample_task("tz-utc");
        t.schedule = Schedule {
            kind: "cron".into(),
            cron: Some("0 10 * * *".into()),
            fire_at: None,
            timezone: "UTC".into(),
        };
        let next = compute_next_run(&t).expect("next run");
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(next as i64, 0)
            .unwrap();
        assert_eq!((dt.hour(), dt.minute()), (10, 0), "cron hour must be UTC wall clock");
    }

    #[test]
    fn cron_local_timezone_fires_at_local_wall_clock() {
        let mut t = sample_task("tz-local");
        t.schedule = Schedule {
            kind: "cron".into(),
            cron: Some("30 10 * * *".into()),
            fire_at: None,
            timezone: "local".into(),
        };
        let next = compute_next_run(&t).expect("next run");
        let dt = chrono::Local
            .timestamp_opt(next as i64, 0)
            .single()
            .expect("valid local time");
        assert_eq!(
            (dt.hour(), dt.minute()),
            (10, 30),
            "cron hour must be the machine's local wall clock, not UTC"
        );
    }

    #[test]
    fn cron_iana_timezone_fires_at_named_wall_clock() {
        let mut t = sample_task("tz-shanghai");
        t.schedule = Schedule {
            kind: "cron".into(),
            cron: Some("0 10 * * *".into()),
            fire_at: None,
            timezone: "Asia/Shanghai".into(),
        };
        let next = compute_next_run(&t).expect("next run");
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(next as i64, 0)
            .unwrap();
        // 10:00 Asia/Shanghai == 02:00 UTC (UTC+8, no DST)
        assert_eq!((dt.hour(), dt.minute()), (2, 0));
    }

    #[test]
    fn once_naive_fire_at_interpreted_in_schedule_timezone() {
        let mut t = sample_task("tz-once");
        t.schedule = Schedule {
            kind: "once".into(),
            cron: None,
            fire_at: Some("2099-01-01T10:00:00".into()),
            timezone: "Asia/Shanghai".into(),
        };
        let next = compute_next_run(&t).expect("next run");
        let expected = chrono::Utc
            .with_ymd_and_hms(2099, 1, 1, 2, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(next as i64, expected);
    }

    #[test]
    fn once_rfc3339_offset_is_respected() {
        let mut t = sample_task("tz-once-rfc");
        t.schedule = Schedule {
            kind: "once".into(),
            cron: None,
            fire_at: Some("2099-01-01T10:00:00+02:00".into()),
            timezone: "Asia/Shanghai".into(), // ignored — explicit offset wins
        };
        let next = compute_next_run(&t).expect("next run");
        let expected = chrono::Utc
            .with_ymd_and_hms(2099, 1, 1, 8, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(next as i64, expected);
    }

    #[test]
    fn scheduling_tools_create_list_update_delete() {
        let dir = temp_dir("tools");
        let ws = dir.join("origin-ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws_str = ws.to_string_lossy().to_string();
        let store = AutomationStore::new(dir.clone());
        let origin = SchedulingOrigin {
            workspace: ws_str.clone(),
            session_id: "sess-1".to_string(),
            surface: "cowork".to_string(),
            agent: "cowork".to_string(),
        };
        let mut registry = ocw_engine::ToolRegistry::new();
        register_scheduling_tools(&mut registry, Arc::new(store), origin);

        // create — valid cron, write grant + read disclosure
        let res = registry
            .execute(
                "create_scheduled_task",
                serde_json::from_value(json!({
                    "title": "Daily",
                    "instructions": "do",
                    "cron": "10 19 * * *",
                    "permissions": [
                        {"tool": "connector__slack", "target": "#gen", "access": "write"},
                        {"tool": "connector__slack", "target": "#gen", "access": "read"}
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        let v = res.value;
        assert_eq!(v["ok"], true);
        assert_eq!(v["title"], "Daily");
        // Workspace is the origin session's — never scratch.
        assert_eq!(v["workspace"], ws_str.as_str());
        assert_eq!(v["always_allowed"], json!(["connector__slack #gen"]));
        let id = v["id"].as_str().unwrap().to_string();

        // Persisted with origin workspace + task session id.
        let reloaded = AutomationStore::new(dir.clone());
        let task = reloaded.get(&id).unwrap();
        assert_eq!(task.workspace, ws_str);
        assert_eq!(task.task_session_id, format!("__task__{id}"));
        assert_eq!(task.origin_session_id, "sess-1");

        // create — invalid cron
        let res = registry
            .execute(
                "create_scheduled_task",
                serde_json::from_value(json!({"title": "x", "instructions": "y", "cron": "bad"}))
                    .unwrap(),
            )
            .unwrap();
        assert!(res.value["error"].as_str().unwrap().contains("invalid cron"));
        // create — neither cron nor fire_at
        let res = registry
            .execute(
                "create_scheduled_task",
                serde_json::from_value(json!({"title": "x", "instructions": "y"})).unwrap(),
            )
            .unwrap();
        assert!(res.value["error"].as_str().unwrap().contains("provide a cron"));

        // list
        let res = registry
            .execute("list_scheduled_tasks", serde_json::Map::new())
            .unwrap();
        assert_eq!(res.value["tasks"].as_array().unwrap().len(), 1);

        // update — unknown id
        let res = registry
            .execute(
                "update_scheduled_task",
                serde_json::from_value(json!({"id": "nope"})).unwrap(),
            )
            .unwrap();
        assert_eq!(res.value["error"], "no such task: nope");
        // update — invalid cron
        let res = registry
            .execute(
                "update_scheduled_task",
                serde_json::from_value(json!({"id": id.clone(), "cron": "bad"})).unwrap(),
            )
            .unwrap();
        assert!(res.value["error"].as_str().unwrap().contains("invalid cron"));
        // update — valid
        let res = registry
            .execute(
                "update_scheduled_task",
                serde_json::from_value(json!({"id": id.clone(), "enabled": false, "title": "Daily2"}))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(res.value["ok"], true);
        assert_eq!(res.value["task"]["title"], "Daily2");
        assert_eq!(res.value["task"]["enabled"], false);
        assert!(res.value["task"]["next_run"].is_null(), "disabled task has no next run");

        // delete
        let res = registry
            .execute(
                "delete_scheduled_task",
                serde_json::from_value(json!({"id": id})).unwrap(),
            )
            .unwrap();
        assert_eq!(res.value["ok"], true);
        let res = registry
            .execute("list_scheduled_tasks", serde_json::Map::new())
            .unwrap();
        assert_eq!(res.value["tasks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn ensure_task_workspace_fixes_legacy_and_mkdirs() {
        let dir = temp_dir("ew");
        let scratch = dir.join("scratch");
        let config = crate::state::Config {
            data_dir: dir.clone(),
            ..crate::state::Config::default()
        };
        let provider: Arc<dyn ocw_provider::Provider> =
            Arc::new(ocw_provider::Router::new("anthropic"));
        let state = AppState::new(config, provider);
        state
            .settings
            .set_scratch_base(scratch.to_string_lossy().to_string())
            .await;

        // Legacy: empty workspace → provision scratch + persist back.
        let legacy = sample_task("task-legacy");
        let fixed = ensure_task_workspace(&state, legacy).await;
        // provision_scratch canonicalizes (macOS temp dirs live under a symlinked
        // /var/folders → /private/var/folders), so assert on the tail instead of
        // a raw prefix.
        assert!(
            fixed.workspace.contains("scratch")
                && fixed.workspace.ends_with("__task__task-legacy"),
            "workspace must be scratch_base/task_session_id: {}",
            fixed.workspace
        );
        assert!(PathBuf::from(&fixed.workspace).is_dir());
        let stored = {
            let store = state.automations.read().await;
            store.get("task-legacy").unwrap()
        };
        assert_eq!(stored.workspace, fixed.workspace);

        // Non-empty workspace is untouched (mkdir only).
        let ws2 = dir.join("ws2");
        std::fs::create_dir_all(&ws2).unwrap();
        let ws2 = ws2.canonicalize().unwrap();
        let mut ok_task = sample_task("task-ok");
        ok_task.workspace = ws2.to_string_lossy().to_string();
        let fixed2 = ensure_task_workspace(&state, ok_task).await;
        assert_eq!(fixed2.workspace, ws2.to_string_lossy().to_string());
        assert!(ws2.is_dir());
    }
}
