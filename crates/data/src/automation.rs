//! Automation store — Rust reimplementation of `coworker/automation/store.py`.

use crate::error::Error;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn true_bool() -> bool {
    true
}
fn local_tz() -> String {
    "local".to_string()
}
fn running_status() -> String {
    "running".to_string()
}
fn schedule_trigger() -> String {
    "schedule".to_string()
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub kind: String,
    pub cron: Option<String>,
    pub fire_at: Option<String>,
    #[serde(default = "local_tz")]
    pub timezone: String,
}

impl Schedule {
    pub fn human(&self) -> String {
        if self.kind == "once" {
            return format!("Once at {}", self.fire_at.as_deref().unwrap_or("?"));
        }
        let cron = match &self.cron {
            Some(c) => c,
            None => return "?".to_string(),
        };
        let parts: Vec<&str> = cron.split_whitespace().collect();
        if parts.len() != 5 {
            return cron.to_string();
        }
        format!(
            "{} {} {} {} {}",
            parts[0], parts[1], parts[2], parts[3], parts[4]
        )
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            kind: "cron".to_string(),
            cron: Some("* * * * *".to_string()),
            fire_at: None,
            timezone: "local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub schedule: Schedule,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub origin_surface: String,
    #[serde(default)]
    pub origin_session_id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub task_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "true_bool")]
    pub notify_on_completion: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default)]
    pub run_count: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<i32>,
    #[serde(default)]
    pub seen_runs_at: f64,
}

impl ScheduledTask {
    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|e| Error::Json(e.to_string()))
    }
    pub fn from_json(s: &str) -> Result<Self, Error> {
        serde_json::from_str(s).map_err(|e| Error::Json(e.to_string()))
    }
    pub fn schedule_human(&self) -> String {
        self.schedule.human()
    }
}

impl Default for ScheduledTask {
    fn default() -> Self {
        let now = epoch_now();
        Self {
            id: String::new(),
            title: String::new(),
            instructions: String::new(),
            schedule: Schedule::default(),
            workspace: String::new(),
            origin_surface: "cowork".to_string(),
            origin_session_id: String::new(),
            agent: "cowork".to_string(),
            task_session_id: String::new(),
            model: None,
            notify_on_completion: true,
            notify_target: None,
            always_allowed_tools: Vec::new(),
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub run_id: String,
    pub task_id: String,
    pub started_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<f64>,
    #[serde(default = "running_status")]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_text: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default = "schedule_trigger")]
    pub trigger: String,
    #[serde(default)]
    pub session_id: String,
}

impl TaskRun {
    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|e| Error::Json(e.to_string()))
    }
    pub fn from_json(s: &str) -> Result<Self, Error> {
        serde_json::from_str(s).map_err(|e| Error::Json(e.to_string()))
    }
}

impl Default for TaskRun {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            task_id: String::new(),
            started_at: epoch_now(),
            finished_at: None,
            status: "running".to_string(),
            result_text: None,
            artifacts: Vec::new(),
            error: None,
            trigger: "schedule".to_string(),
            session_id: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

pub struct TaskStore {
    conn: Mutex<Connection>,
}

impl TaskStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scheduled_tasks (id TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 1, next_run REAL, data TEXT NOT NULL); \
             CREATE TABLE IF NOT EXISTS task_runs (run_id TEXT PRIMARY KEY, task_id TEXT NOT NULL, started_at REAL NOT NULL, data TEXT NOT NULL); \
             CREATE INDEX IF NOT EXISTS idx_runs_task ON task_runs(task_id, started_at DESC);"
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn save(&self, task: &ScheduledTask) -> Result<ScheduledTask, Error> {
        let mut task = task.clone();
        task.updated_at = epoch_now();
        task.next_run = if task.enabled {
            compute_next_run(&task, None)
        } else {
            None
        };
        let conn = self.conn.lock();
        conn.execute("INSERT OR REPLACE INTO scheduled_tasks (id, enabled, next_run, data) VALUES (?1, ?2, ?3, ?4)",
            params![task.id, if task.enabled { 1i64 } else { 0i64 }, task.next_run, task.to_json()?])?;
        Ok(task)
    }

    pub fn get(&self, task_id: &str) -> Result<Option<ScheduledTask>, Error> {
        let row: Option<String> = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT data FROM scheduled_tasks WHERE id = ?",
                [task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::Sqlite(e.to_string()))?
        };
        row.map(|d| ScheduledTask::from_json(&d)).transpose()
    }

    pub fn list(&self) -> Result<Vec<ScheduledTask>, Error> {
        let data: Vec<String> = {
            let conn = self.conn.lock();
            let mut stmt = conn
                .prepare("SELECT data FROM scheduled_tasks ORDER BY next_run IS NULL, next_run")?;
            let mut out = Vec::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let s: String = row.get(0)?;
                out.push(s);
            }
            out
        };
        data.into_iter()
            .map(|d| ScheduledTask::from_json(&d))
            .collect()
    }

    pub fn delete(&self, task_id: &str) -> Result<bool, Error> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM scheduled_tasks WHERE id = ?", [task_id])?;
        conn.execute("DELETE FROM task_runs WHERE task_id = ?", [task_id])?;
        Ok(n > 0)
    }

    pub fn due(&self, now: Option<f64>) -> Result<Vec<ScheduledTask>, Error> {
        let now = now.unwrap_or_else(epoch_now);
        let data: Vec<String> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare("SELECT data FROM scheduled_tasks WHERE enabled = 1 AND next_run IS NOT NULL AND next_run <= ? ORDER BY next_run")?;
            let mut out = Vec::new();
            let mut rows = stmt.query([now])?;
            while let Some(row) = rows.next()? {
                let s: String = row.get(0)?;
                out.push(s);
            }
            out
        };
        data.into_iter()
            .map(|d| ScheduledTask::from_json(&d))
            .collect()
    }

    pub fn add_run(&self, run: &TaskRun) -> Result<TaskRun, Error> {
        let conn = self.conn.lock();
        conn.execute("INSERT OR REPLACE INTO task_runs (run_id, task_id, started_at, data) VALUES (?1, ?2, ?3, ?4)",
            params![run.run_id, run.task_id, run.started_at, run.to_json()?])?;
        Ok(run.clone())
    }

    pub fn find_run(&self, run_id: &str) -> Result<Option<TaskRun>, Error> {
        let row: Option<String> = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT data FROM task_runs WHERE run_id = ?",
                [run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::Sqlite(e.to_string()))?
        };
        row.map(|d| TaskRun::from_json(&d)).transpose()
    }

    pub fn task_for_run_session(&self, session_id: &str) -> Result<Option<ScheduledTask>, Error> {
        if !session_id.starts_with("__run__") {
            return Ok(None);
        }
        let run_id = &session_id[7..];
        let run = self.find_run(run_id)?;
        match run {
            Some(r) => self.get(&r.task_id),
            None => Ok(None),
        }
    }

    pub fn runs(&self, task_id: &str, limit: usize) -> Result<Vec<TaskRun>, Error> {
        let data: Vec<String> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT data FROM task_runs WHERE task_id = ? ORDER BY started_at DESC LIMIT ?",
            )?;
            let mut out = Vec::new();
            let mut rows = stmt.query(params![task_id, limit as i64])?;
            while let Some(row) = rows.next()? {
                let s: String = row.get(0)?;
                out.push(s);
            }
            out
        };
        data.into_iter().map(|d| TaskRun::from_json(&d)).collect()
    }
}

// ---------------------------------------------------------------------------
// Scheduling logic
// ---------------------------------------------------------------------------

/// Normalize a cron expression for the `cron` crate (expects 6 fields including seconds).
/// Python `croniter` accepts classic 5-field exprs (`min hour dom mon dow`); prepend `0` seconds.
pub fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

pub fn compute_next_run(task: &ScheduledTask, after: Option<f64>) -> Option<f64> {
    let now = after.unwrap_or_else(epoch_now);
    let sched = &task.schedule;
    if sched.kind == "once" {
        let fire_at = sched.fire_at.as_ref()?;
        let dt = chrono::DateTime::parse_from_rfc3339(fire_at)
            .ok()?
            .with_timezone(&chrono::Utc);
        let ts = dt.timestamp() as f64;
        return if task.run_count == 0 && ts > now {
            Some(ts)
        } else {
            None
        };
    }
    let cron_expr = sched.cron.as_ref()?;
    if !cron_expr.is_empty() {
        if task.max_runs.is_some_and(|m| task.run_count >= m) {
            return None;
        }
        let normalized = normalize_cron_expr(cron_expr);
        if let Ok(schedule) = normalized.parse::<cron::Schedule>() {
            // `upcoming` yields times strictly after Utc::now(); when `after` is supplied
            // (e.g. tests / catch-up), walk until we pass that watermark.
            let after_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(now as i64, 0)
                .unwrap_or_else(chrono::Utc::now);
            for next in schedule.upcoming(chrono::Utc).take(64) {
                let ts = next.timestamp() as f64;
                if next > after_dt && ts > now {
                    return Some(ts);
                }
            }
        }
        // Do not silently fire every 60s — invalid cron means "no next run".
        return None;
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_store_basic() {
        let temp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(temp.path().join("tasks.db")).unwrap();
        let task = ScheduledTask {
            id: "test-task".to_string(),
            title: "Test Task".to_string(),
            instructions: "Do something".to_string(),
            schedule: Schedule {
                kind: "once".to_string(),
                cron: None,
                fire_at: Some("2099-01-01T00:00:00Z".to_string()),
                timezone: "UTC".to_string(),
            },
            workspace: "/tmp".to_string(),
            ..Default::default()
        };
        let saved = store.save(&task).unwrap();
        assert_eq!(saved.id, "test-task");
        assert!(saved.next_run.is_some());
        let loaded = store.get("test-task").unwrap().unwrap();
        assert_eq!(loaded.title, "Test Task");
        assert!(store.delete("test-task").unwrap());
        assert!(store.get("test-task").unwrap().is_none());
    }

    #[test]
    fn five_field_cron_is_not_now_plus_60() {
        let task = ScheduledTask {
            id: "cron-test".into(),
            schedule: Schedule {
                kind: "cron".into(),
                cron: Some("0 9 * * 1".into()), // every Monday 09:00 — Python croniter shape
                fire_at: None,
                timezone: "UTC".into(),
            },
            enabled: true,
            ..Default::default()
        };
        // Tuesday 2026-08-04 12:00 UTC
        let after = 1785854400.0_f64;
        let next = compute_next_run(&task, Some(after)).expect("next run");
        let delta = next - after;
        assert!(
            delta > 60.0 * 60.0,
            "expected next Monday (~days later), got delta={delta}s (likely now+60 bug)"
        );
        assert!(
            delta < 8.0 * 24.0 * 3600.0,
            "next run unreasonably far: {delta}s"
        );
    }

    #[test]
    fn normalize_cron_prepends_seconds() {
        assert_eq!(normalize_cron_expr("0 9 * * 1"), "0 0 9 * * 1");
        assert_eq!(normalize_cron_expr("0 0 9 * * 1"), "0 0 9 * * 1");
    }

    #[test]
    fn task_run_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(temp.path().join("tasks.db")).unwrap();
        let run = TaskRun {
            run_id: "run-1".to_string(),
            task_id: "task-1".to_string(),
            started_at: epoch_now(),
            status: "ok".to_string(),
            session_id: "__run__run-1".to_string(),
            ..Default::default()
        };
        store.add_run(&run).unwrap();
        let found = store.find_run("run-1").unwrap().unwrap();
        assert_eq!(found.status, "ok");
    }
}
