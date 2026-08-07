//! Read-only Explorer subagent support.
//!
//! The Explorer runs a child [`TurnEngine`] with a fresh context and a registry supplied
//! by the tools crate. Keeping the orchestration here avoids a dependency cycle between
//! the engine and concrete filesystem/git tools.

use crate::{EventData, Message, Mode, PermissionEngine, ToolRegistry, TurnEngine};
use ocw_provider::Provider;
use serde_json::Map;
use std::path::PathBuf;
use std::sync::Arc;

/// System instructions for the read-only research child.
pub const EXPLORER_INSTRUCTIONS: &str = "You are a read-only code explorer working inside the user's workspace. Answer the research task by searching and reading the code with grep, read_file, list_files, glob_search, git_log, git_status, and git_diff. You cannot write files or run shell commands.\n\nYour final message is a self-contained report for the agent that spawned you, not for the user. Answer the task directly, reference code as path:line, quote key snippets when useful, and mention anything surprising. If you could not find something, say what you searched so the caller does not repeat the same work.";

/// Maximum model/tool rounds used by an Explorer child.
pub const EXPLORER_MAX_ITERATIONS: usize = 10;

/// The final output of an Explorer child run.
#[derive(Debug, Clone)]
pub struct ExplorerReport {
    pub report: String,
    pub status: String,
    pub error: Option<String>,
}

/// Run a fresh, read-only child engine and retain only its final report.
///
/// The caller owns the child registry. It must contain only read-only tools; the Plan
/// permission mode is an additional guard that rejects consequential calls even if a
/// future registry accidentally includes one.
pub async fn run_explorer(
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    workspace: PathBuf,
    model: String,
    task: String,
) -> ExplorerReport {
    let permissions = Arc::new(tokio::sync::Mutex::new(PermissionEngine::new(workspace)));
    permissions.lock().await.set_mode(Mode::Plan);

    let mut engine = TurnEngine::new(
        provider,
        registry,
        permissions,
        model,
        EXPLORER_MAX_ITERATIONS,
        Map::new(),
        vec![Message::system(EXPLORER_INSTRUCTIONS)],
    );

    let mut report = String::new();
    let mut streamed = String::new();
    let mut status = "unknown".to_string();
    let mut error = None;

    for event in engine.run(serde_json::Value::String(task), None).await {
        match event.data {
            EventData::AssistantDelta { text } => streamed.push_str(&text),
            EventData::AssistantMessage {
                text: Some(text), ..
            } => {
                report = text;
            }
            EventData::TurnEnd { status: value, .. } => status = value,
            EventData::Error { error: value, .. } => {
                status = "error".to_string();
                error = Some(value);
            }
            _ => {}
        }
    }

    if report.is_empty() {
        report = streamed;
    }
    if report.is_empty() && error.is_none() {
        error = Some("explorer produced no report".to_string());
        status = "error".to_string();
    }

    ExplorerReport {
        report,
        status,
        error,
    }
}
