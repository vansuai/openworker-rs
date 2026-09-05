//! PyO3 bindings — exposes the Rust data layer to Python.
//!
//! Compile with `cargo build -p ocw-data --features pyo3`.
//! Produces `target/release/libocw_data.{so,dylib}` importable as `import ocw_data`.

use crate::automation::{
    ScheduledTask as RustTask, TaskRun as RustTaskRun, TaskStore as RustTaskStore,
};
use crate::conversation::ConversationStore as RustConvStore;
use crate::memory::SQLiteMemoryStore as RustMemStore;
use crate::types::SessionSummary;
use crate::types::{
    MemoryItem as RustMemItem, Scope as RustScope, SessionRecord as RustSessionRecord,
};
use pyo3::prelude::*;
use pyo3::types::PyDict;

fn err_json(e: impl ToString) -> PyErr {
    pyo3::exceptions::PyValueError::new_err(e.to_string())
}
fn err_io(e: impl ToString) -> PyErr {
    pyo3::exceptions::PyOSError::new_err(e.to_string())
}

// ---------------------------------------------------------------------------
// SQLiteMemoryStore
// ---------------------------------------------------------------------------

#[pyclass(module = "ocw_data")]
pub struct PySQLiteMemoryStore {
    inner: RustMemStore,
}

fn mem_to_dict(item: &RustMemItem, py: Python<'_>) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("id", item.id)?;
    d.set_item("scope", item.scope.as_str())?;
    d.set_item("content", &item.content)?;
    d.set_item("key", &item.key)?;
    d.set_item("summary", &item.summary)?;
    d.set_item("workspace", &item.workspace)?;
    d.set_item("session_id", &item.session_id)?;
    d.set_item("created_at", &item.created_at)?;
    Ok(d.into())
}

#[pymethods]
impl PySQLiteMemoryStore {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        Ok(Self {
            inner: RustMemStore::open(path).map_err(err_io)?,
        })
    }

    fn add(
        &self,
        py: Python<'_>,
        content: &str,
        scope: &str,
        key: Option<&str>,
        summary: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> PyResult<Py<PyDict>> {
        let scope =
            RustScope::try_from(scope).map_err(|e| pyo3::exceptions::PyValueError::new_err(e))?;
        let item = self
            .inner
            .add(content, scope, key, summary, workspace, session_id)
            .map_err(err_io)?;
        mem_to_dict(&item, py)
    }

    fn get(&self, py: Python<'_>, item_id: i64) -> PyResult<Option<Py<PyDict>>> {
        match self.inner.get(item_id).map_err(err_io)? {
            Some(i) => Ok(Some(mem_to_dict(&i, py)?)),
            None => Ok(None),
        }
    }

    fn list(
        &self,
        py: Python<'_>,
        scope: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> PyResult<Vec<Py<PyDict>>> {
        let scope = scope
            .map(|s| RustScope::try_from(s).map_err(|e| pyo3::exceptions::PyValueError::new_err(e)))
            .transpose()?;
        let items = self
            .inner
            .list(scope, workspace, session_id)
            .map_err(err_io)?;
        items.iter().map(|i| mem_to_dict(i, py)).collect()
    }

    fn update(
        &self,
        py: Python<'_>,
        item_id: i64,
        content: &str,
        summary: Option<&str>,
    ) -> PyResult<Option<Py<PyDict>>> {
        match self
            .inner
            .update(item_id, content, summary)
            .map_err(err_io)?
        {
            Some(i) => Ok(Some(mem_to_dict(&i, py)?)),
            None => Ok(None),
        }
    }

    fn delete(&self, item_id: i64) -> PyResult<bool> {
        self.inner.delete(item_id).map_err(err_io)
    }
}

// ---------------------------------------------------------------------------
// ConversationStore
// ---------------------------------------------------------------------------

#[pyclass(module = "ocw_data")]
pub struct PyConversationStore {
    inner: RustConvStore,
}

fn record_to_dict(record: &RustSessionRecord, py: Python<'_>) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("session_id", &record.session_id)?;
    d.set_item("workspace", &record.workspace)?;
    d.set_item("model", &record.model)?;
    d.set_item("mode", &record.mode)?;
    let msgs: Vec<String> = record
        .messages
        .iter()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
        .collect();
    d.set_item("messages", msgs)?;
    d.set_item("title", &record.title)?;
    d.set_item("agent", &record.agent)?;
    d.set_item("message_count", record.message_count)?;
    d.set_item("updated_at", &record.updated_at)?;
    d.set_item(
        "extra_roots",
        serde_json::to_string(&record.extra_roots).unwrap_or_default(),
    )?;
    d.set_item(
        "grants",
        serde_json::to_string(&record.grants).unwrap_or_default(),
    )?;
    d.set_item(
        "compaction",
        record
            .compaction
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_else(|| "{}".into()),
    )?;
    d.set_item("pinned", record.pinned)?;
    d.set_item("archived", record.archived)?;
    d.set_item("origin", &record.origin)?;
    d.set_item("origin_label", &record.origin_label)?;
    Ok(d.into())
}

fn summary_to_dict(s: &SessionSummary, py: Python<'_>) -> PyResult<Py<PyDict>> {
    let d = PyDict::new(py);
    d.set_item("session_id", &s.session_id)?;
    d.set_item("workspace", &s.workspace)?;
    d.set_item("model", &s.model)?;
    d.set_item("mode", &s.mode)?;
    d.set_item("title", &s.title)?;
    d.set_item("agent", &s.agent)?;
    d.set_item("message_count", s.message_count)?;
    d.set_item("updated_at", &s.updated_at)?;
    d.set_item("pinned", s.pinned)?;
    d.set_item("archived", s.archived)?;
    d.set_item("origin", &s.origin)?;
    d.set_item("origin_label", &s.origin_label)?;
    Ok(d.into())
}

#[pymethods]
impl PyConversationStore {
    #[new]
    fn new(base_dir: &str) -> PyResult<Self> {
        Ok(Self {
            inner: RustConvStore::open(base_dir).map_err(err_io)?,
        })
    }

    /// Load a session by id. Returns None if not found.
    fn load(&self, py: Python<'_>, session_id: &str) -> PyResult<Option<Py<PyDict>>> {
        match self.inner.load(session_id).map_err(err_io)? {
            Some(r) => Ok(Some(record_to_dict(&r, py)?)),
            None => Ok(None),
        }
    }

    /// Save a session from a JSON string representation of a SessionRecord.
    /// This avoids complex Python dict extraction in Rust.
    fn save_json(&self, record_json: &str) -> PyResult<()> {
        let record: RustSessionRecord = serde_json::from_str(record_json).map_err(err_json)?;
        self.inner.save(&record).map_err(err_io)
    }

    /// List sessions (summaries only). Pass workspace filter or None.
    fn list(&self, py: Python<'_>, workspace: Option<String>) -> PyResult<Vec<Py<PyDict>>> {
        let summaries = self.inner.list(workspace.as_deref()).map_err(err_io)?;
        summaries.iter().map(|s| summary_to_dict(s, py)).collect()
    }

    fn delete(&self, session_id: &str) -> PyResult<bool> {
        self.inner.delete(session_id).map_err(err_io)
    }

    fn rename(&self, session_id: &str, title: &str) -> PyResult<bool> {
        self.inner.rename(session_id, title).map_err(err_io)
    }

    fn set_flags(
        &self,
        session_id: &str,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> PyResult<bool> {
        self.inner
            .set_flags(session_id, pinned, archived)
            .map_err(err_io)
    }

    fn set_extra_roots_json(&self, session_id: &str, extra_roots_json: &str) -> PyResult<()> {
        let roots: Vec<serde_json::Value> =
            serde_json::from_str(extra_roots_json).map_err(err_json)?;
        self.inner
            .set_extra_roots(session_id, roots)
            .map_err(err_io)
    }

    fn touch_workspace(&self, path: &str) -> PyResult<()> {
        self.inner.touch_workspace(path).map_err(err_io)
    }

    fn recent_workspaces(&self, limit: usize) -> PyResult<Vec<String>> {
        self.inner.recent_workspaces(limit).map_err(err_io)
    }
}

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

#[pyclass(module = "ocw_data")]
pub struct PyTaskStore {
    inner: RustTaskStore,
}

#[pymethods]
impl PyTaskStore {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        Ok(Self {
            inner: RustTaskStore::open(path).map_err(err_io)?,
        })
    }

    /// Get a task as JSON string, or None.
    fn get(&self, task_id: &str) -> PyResult<Option<String>> {
        match self.inner.get(task_id).map_err(err_io)? {
            Some(t) => Ok(Some(t.to_json().map_err(err_json)?)),
            None => Ok(None),
        }
    }

    /// List all tasks as JSON strings.
    fn list(&self) -> PyResult<Vec<String>> {
        let tasks = self.inner.list().map_err(err_io)?;
        tasks
            .iter()
            .map(|t| t.to_json().map_err(err_json))
            .collect()
    }

    /// Get due tasks as JSON strings.
    fn due(&self, now: Option<f64>) -> PyResult<Vec<String>> {
        let tasks = self.inner.due(now).map_err(err_io)?;
        tasks
            .iter()
            .map(|t| t.to_json().map_err(err_json))
            .collect()
    }

    /// Save a task from JSON string. Returns updated task as JSON string.
    fn save(&self, task_json: &str) -> PyResult<String> {
        let task = RustTask::from_json(task_json).map_err(err_json)?;
        let saved = self.inner.save(&task).map_err(err_io)?;
        saved.to_json().map_err(err_json)
    }

    fn delete(&self, task_id: &str) -> PyResult<bool> {
        self.inner.delete(task_id).map_err(err_io)
    }

    fn add_run(&self, run_json: &str) -> PyResult<String> {
        let run = RustTaskRun::from_json(run_json).map_err(err_json)?;
        let saved = self.inner.add_run(&run).map_err(err_io)?;
        saved.to_json().map_err(err_json)
    }

    fn find_run(&self, run_id: &str) -> PyResult<Option<String>> {
        match self.inner.find_run(run_id).map_err(err_io)? {
            Some(r) => Ok(Some(r.to_json().map_err(err_json)?)),
            None => Ok(None),
        }
    }

    fn task_for_run_session(&self, session_id: &str) -> PyResult<Option<String>> {
        match self
            .inner
            .task_for_run_session(session_id)
            .map_err(err_io)?
        {
            Some(t) => Ok(Some(t.to_json().map_err(err_json)?)),
            None => Ok(None),
        }
    }

    fn runs(&self, task_id: &str, limit: usize) -> PyResult<Vec<String>> {
        let task_runs = self.inner.runs(task_id, limit).map_err(err_io)?;
        task_runs
            .iter()
            .map(|r| r.to_json().map_err(err_json))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Module (must match crate name)
// ---------------------------------------------------------------------------

#[pymodule]
fn ocw_data(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<PySQLiteMemoryStore>()?;
    m.add_class::<PyConversationStore>()?;
    m.add_class::<PyTaskStore>()?;
    Ok(())
}
