//! Core filesystem and search tools for the OpenWorker agent.
//!
//! Read-only tools are low-risk; write and delete are high-risk / require approval.
//! All filesystem tools are sandboxed to `workspace_root` (symlink-aware canonical
//! path check, matching the Python `file_tools` / `search_tools`).

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use ocw_provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Truncate to at most `max_chars` characters without splitting a multi-byte
/// UTF-8 sequence — file contents and grep hits routinely carry CJK text, and
/// a byte-offset slice into it panics (same class as the `preview()` crash).
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ---------------------------------------------------------------------------
// TodoList — shared per-session task list surfaced to the UI.
// ---------------------------------------------------------------------------

/// A single todo entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String, // "pending" | "in_progress" | "done"
}

/// A shared task list held by Arc<Mutex<...>> so the tool, the engine,
/// and any surface observing it see the same state.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoList {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_items(&self, items: Vec<TodoItem>) {
        let mut guard = self.items.lock().expect("TodoList mutex poisoned");
        *guard = items;
    }

    pub fn items(&self) -> Vec<TodoItem> {
        self.items.lock().expect("TodoList mutex poisoned").clone()
    }
}

/// Normalize a list of todos from the model into the canonical shape
/// (matches Python's _TODO_SCHEMA behavior).
fn normalize_todos(raw: &[Value]) -> Vec<TodoItem> {
    raw.iter()
        .map(|entry| {
            let obj = entry.as_object();
            let content = obj
                .and_then(|o| o.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_status = obj
                .and_then(|o| o.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            let status = match raw_status {
                "completed" => "done", // common model alias
                s if s == "pending" || s == "in_progress" || s == "done" => s,
                _ => "pending",
            };
            TodoItem {
                content,
                status: status.to_string(),
            }
        })
        .collect()
}

/// Extract todo entries from model args. Accepts a direct array, the legacy
/// `items` alias (Python parity), and MiniMax-style wrappers like
/// `{"todos": {"item": [{...}]}}`.
fn extract_todo_array(args: &Map<String, Value>) -> Vec<Value> {
    let normalized =
        ocw_provider::normalize_tool_input(serde_json::Value::Object(args.clone()));
    let map = normalized.as_object().unwrap_or(args);

    for key in ["todos", "items"] {
        if let Some(arr) = coerce_todo_value(map.get(key)) {
            return arr;
        }
    }
    vec![]
}

fn coerce_todo_value(value: Option<&Value>) -> Option<Vec<Value>> {
    let value = value?;
    if let Some(arr) = value.as_array() {
        return Some(arr.clone());
    }
    if let Some(obj) = value.as_object() {
        for key in ["item", "items", "todos"] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                return Some(arr.clone());
            }
        }
        // Last resort: any single array-valued field in the wrapper object.
        for val in obj.values() {
            if let Some(arr) = val.as_array() {
                return Some(arr.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod todo_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_todo_array_accepts_direct_array() {
        let args = json!({"todos": [{"content": "a", "status": "pending"}]})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(extract_todo_array(&args).len(), 1);
    }

    #[test]
    fn extract_todo_array_accepts_item_wrapper() {
        let args = json!({"todos": {"item": [{"content": "a", "status": "pending"}]}})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(extract_todo_array(&args).len(), 1);
    }

    #[test]
    fn extract_todo_array_accepts_items_alias() {
        let args = json!({"items": [{"content": "a", "status": "pending"}]})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(extract_todo_array(&args).len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 500;
const MAX_GREP_RESULTS: usize = 1000;

const IGNORE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".idea",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a path relative to `workspace_root`, canonicalise, and verify it does
/// not escape the workspace.
fn resolve_in_workspace(workspace: &Path, rel: &str) -> Result<PathBuf, String> {
    let candidate = workspace.join(rel);
    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("cannot resolve path: {e}"))?;
    let ws_canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !canonical.starts_with(&ws_canon) {
        return Err("path escapes the workspace".to_string());
    }
    Ok(canonical)
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Resolve write_file path/content, including aliases and salvaged `_raw` payloads.
fn resolve_write_file_args(args: &Map<String, Value>) -> (Option<String>, Option<String>) {
    let normalized =
        ocw_provider::normalize_tool_input(serde_json::Value::Object(args.clone()));
    let map = normalized.as_object().unwrap_or(args);

    let path = ["path", "file", "filepath", "filename"]
        .iter()
        .find_map(|k| arg_str(map, k))
        .map(str::to_string);

    let content = ["content", "body", "text", "data"]
        .iter()
        .find_map(|k| arg_str(map, k))
        .map(str::to_string);

    (path, content)
}

fn arg_i64(args: &Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn arg_bool(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn tool_schema(name: &str, desc: &str, params: Value) -> ToolSchema {
    ToolSchema::new(name, Some(desc), Some(params))
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

fn read_file_schema() -> ToolSchema {
    tool_schema(
        "read_file",
        "Read a text file, returning numbered lines so code can be referenced as path:line. Large files are windowed: pass start_line to continue where the previous read stopped. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the workspace." },
                "start_line": { "type": "integer", "description": "First line to read, 1-based (default 1)." },
                "max_lines": { "type": "integer", "description": "How many lines (default 2000)." }
            },
            "required": ["path"]
        }),
    )
}

fn make_read_file(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let path_str = match arg_str(&args, "path") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "path is required"})),
        };
        let start = arg_i64(&args, "start_line")
            .filter(|&n| n > 0)
            .map(|n| n as usize)
            .unwrap_or(1);
        let max_lines = arg_i64(&args, "max_lines")
            .filter(|&n| n > 0)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_LINES)
            .min(DEFAULT_MAX_LINES);

        let target = match resolve_in_workspace(&workspace, path_str) {
            Ok(t) => t,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };
        if !target.is_file() {
            return ToolResult::ok(json!({"error": format!("not a file: {path_str}")}));
        }

        let file = match fs::File::open(&target) {
            Ok(f) => f,
            Err(e) => return ToolResult::ok(json!({"error": format!("read failed: {e}")})),
        };

        let reader = BufReader::new(file);
        let mut selected: Vec<String> = Vec::new();
        let mut total = 0usize;

        for (i, line_result) in reader.lines().enumerate() {
            let i = i + 1; // 1-based
            total = i;
            if i < start || selected.len() >= max_lines {
                continue;
            }
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    selected.push(format!("{i:>6}\t<read error: {e}>"));
                    continue;
                }
            };
            let text = if line.len() > MAX_LINE_CHARS {
                format!("{}… (line truncated)", truncate_chars(&line, MAX_LINE_CHARS))
            } else {
                line
            };
            selected.push(format!("{i:>6}\t{text}"));
        }

        let end = if selected.is_empty() {
            start.saturating_sub(1)
        } else {
            start + selected.len() - 1
        };
        let rel = target
            .strip_prefix(&workspace)
            .unwrap_or(&target)
            .display()
            .to_string();
        let mut result = json!({
            "path": rel,
            "start_line": start,
            "end_line": end,
            "total_lines": total,
            "content": selected.join("\n"),
        });
        if end < total {
            result["note"] = json!(format!(
                "showing lines {start}-{end} of {total}; call again with start_line={} to continue",
                end + 1
            ));
        }
        ToolResult::ok(result)
    })
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

fn write_file_schema() -> ToolSchema {
    tool_schema(
        "write_file",
        "Write (or overwrite) a file in the workspace. Creates parent directories as needed.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the workspace." },
                "content": { "type": "string", "description": "The full content to write." }
            },
            "required": ["path", "content"]
        }),
    )
}

fn make_write_file(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let (path_opt, content_opt) = resolve_write_file_args(&args);
        let path_str = match path_opt {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "path is required"})),
        };
        let content = match content_opt {
            Some(c) => c,
            None => return ToolResult::ok(json!({"error": "content is required"})),
        };

        let target = workspace.join(path_str);
        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return ToolResult::ok(json!({"error": format!("cannot create parent dirs: {e}")}));
            }
        }

        // Still enforce sandboxing
        {
            let ws_canon = workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.clone());
            match target.canonicalize() {
                Ok(canonical) => {
                    if !canonical.starts_with(&ws_canon) {
                        return ToolResult::ok(json!({"error": "path escapes the workspace"}));
                    }
                }
                Err(_) => {
                    // File doesn't exist yet — check parent
                    match target.parent().and_then(|p| p.canonicalize().ok()) {
                        Some(pc) => {
                            if !pc.starts_with(&ws_canon) {
                                return ToolResult::ok(
                                    json!({"error": "path escapes the workspace"}),
                                );
                            }
                        }
                        None => return ToolResult::ok(json!({"error": "cannot resolve path"})),
                    }
                }
            }
        }

        let nbytes = content.len();
        match fs::write(&target, &content) {
            Ok(()) => {
                let rel = target
                    .strip_prefix(&workspace)
                    .unwrap_or(&target)
                    .display()
                    .to_string();
                ToolResult::ok(json!({"path": rel, "bytes_written": nbytes}))
            }
            Err(e) => ToolResult::ok(json!({"error": format!("write failed: {e}")})),
        }
    })
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

fn edit_file_schema() -> ToolSchema {
    tool_schema(
        "edit_file",
        "Replace a string in a file with another string. When replace_all is false, old_string must uniquely identify the location.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the workspace." },
                "old_string": { "type": "string", "description": "The exact text to replace." },
                "new_string": { "type": "string", "description": "The replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)." }
            },
            "required": ["path", "old_string", "new_string"]
        }),
    )
}

fn make_edit_file(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let path_str = match arg_str(&args, "path") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "path is required"})),
        };
        let old_str = match arg_str(&args, "old_string") {
            Some(s) => s,
            None => return ToolResult::ok(json!({"error": "old_string is required"})),
        };
        let new_str = match arg_str(&args, "new_string") {
            Some(s) => s,
            None => return ToolResult::ok(json!({"error": "new_string is required"})),
        };
        let replace_all = arg_bool(&args, "replace_all");

        let target = match resolve_in_workspace(&workspace, path_str) {
            Ok(t) => t,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };
        if !target.is_file() {
            return ToolResult::ok(json!({"error": format!("not a file: {path_str}")}));
        }

        let content = match fs::read_to_string(&target) {
            Ok(c) => c,
            Err(e) => return ToolResult::ok(json!({"error": format!("read failed: {e}")})),
        };

        if replace_all {
            let new_content = content.replace(old_str, new_str);
            if new_content == content {
                return ToolResult::ok(json!({"error": "old_string not found in file"}));
            }
            match fs::write(&target, &new_content) {
                Ok(()) => {
                    let count = content.matches(old_str).count();
                    ToolResult::ok(json!({"path": path_str, "replacements": count}))
                }
                Err(e) => ToolResult::ok(json!({"error": format!("write failed: {e}")})),
            }
        } else {
            let count = content.matches(old_str).count();
            if count == 0 {
                return ToolResult::ok(json!({"error": "old_string not found in file"}));
            }
            if count > 1 {
                return ToolResult::ok(
                    json!({"error": format!("old_string found {count} times; set replace_all=true or make it unique")}),
                );
            }
            let new_content = content.replacen(old_str, new_str, 1);
            match fs::write(&target, &new_content) {
                Ok(()) => ToolResult::ok(json!({"path": path_str, "replacements": 1})),
                Err(e) => ToolResult::ok(json!({"error": format!("write failed: {e}")})),
            }
        }
    })
}

// ---------------------------------------------------------------------------
// create_directory
// ---------------------------------------------------------------------------

fn create_directory_schema() -> ToolSchema {
    tool_schema(
        "create_directory",
        "Create a directory (and any missing parents) in the workspace.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path, relative to the workspace." }
            },
            "required": ["path"]
        }),
    )
}

fn make_create_directory(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let path_str = match arg_str(&args, "path") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "path is required"})),
        };
        let target = workspace.join(path_str);
        // Check parent for sandboxing
        if let Some(parent) = target.parent() {
            if let Ok(canon) = parent.canonicalize() {
                let ws_canon = workspace
                    .canonicalize()
                    .unwrap_or_else(|_| workspace.clone());
                if !canon.starts_with(&ws_canon) {
                    return ToolResult::ok(json!({"error": "path escapes the workspace"}));
                }
            }
        }
        match fs::create_dir_all(&target) {
            Ok(()) => {
                let rel = target
                    .strip_prefix(&workspace)
                    .unwrap_or(&target)
                    .display()
                    .to_string();
                ToolResult::ok(json!({"path": rel, "created": true}))
            }
            Err(e) => ToolResult::ok(json!({"error": format!("mkdir failed: {e}")})),
        }
    })
}

// ---------------------------------------------------------------------------
// delete_file
// ---------------------------------------------------------------------------

fn delete_file_schema() -> ToolSchema {
    tool_schema(
        "delete_file",
        "Delete a file in the workspace. High-risk; requires approval.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the workspace." }
            },
            "required": ["path"]
        }),
    )
}

fn make_delete_file(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let path_str = match arg_str(&args, "path") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "path is required"})),
        };
        let target = match resolve_in_workspace(&workspace, path_str) {
            Ok(t) => t,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };
        if !target.exists() {
            return ToolResult::ok(json!({"error": format!("file not found: {path_str}")}));
        }
        match fs::remove_file(&target) {
            Ok(()) => ToolResult::ok(json!({"path": path_str, "deleted": true})),
            Err(e) => ToolResult::ok(json!({"error": format!("delete failed: {e}")})),
        }
    })
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

fn grep_schema() -> ToolSchema {
    tool_schema(
        "grep",
        "Search the workspace for a regular-expression pattern and return matching lines as file:line:text. Fast and .gitignore-aware (skips node_modules, build dirs, etc.). Read-only.",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "Subdirectory to search (default: whole workspace)." },
                "glob": { "type": "string", "description": "Optional filename glob filter, e.g. *.py." },
                "max_results": { "type": "integer", "description": "Max matches (default 100, max 1000)." }
            },
            "required": ["pattern"]
        }),
    )
}

fn make_grep(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "pattern is required"})),
        };
        let sub = arg_str(&args, "path").unwrap_or(".");
        let glob_filter = arg_str(&args, "glob");
        let n = arg_i64(&args, "max_results")
            .filter(|&v| v > 0)
            .map(|v| v as usize)
            .unwrap_or(100)
            .min(MAX_GREP_RESULTS);

        let base = match resolve_in_workspace(&workspace, sub) {
            Ok(b) => b,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };

        // Prefer ripgrep
        if let Ok(rg) = which::which("rg") {
            let mut cmd = Command::new(rg);
            cmd.args([
                "--line-number",
                "--no-heading",
                "--color=never",
                "--max-count",
                &n.to_string(),
            ])
            .arg("-e")
            .arg(pattern);
            if let Some(g) = glob_filter {
                cmd.arg("--glob").arg(g);
            }
            for ignored in IGNORE_DIRS {
                cmd.arg("--glob").arg(format!("!**/{ignored}/**"));
            }
            cmd.arg(&base);
            match cmd.output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let matches: Vec<Value> = stdout
                        .lines()
                        .take(n)
                        .filter_map(|line| {
                            let mut parts = line.splitn(3, ':');
                            let file = parts.next()?;
                            let ln: i64 = parts.next()?.parse().ok()?;
                            let txt = parts.next().unwrap_or("")
                    .to_string();
                            let rel = Path::new(file)
                                .strip_prefix(&workspace)
                                .unwrap_or_else(|_| Path::new(file))
                                .display()
                                .to_string();
                            Some(json!({"file": rel, "line": ln, "text": truncate_chars(&txt, 300)}))
                        })
                        .collect();
                    return ToolResult::ok(
                        json!({"engine": "ripgrep", "count": matches.len(), "matches": matches}),
                    );
                }
                Err(e) => {
                    return ToolResult::ok(json!({"error": format!("grep failed: {e}")}));
                }
            }
        }

        // Python-like fallback
        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ToolResult::ok(json!({"error": format!("invalid regex: {e}")})),
        };

        let mut matches: Vec<Value> = Vec::new();
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_entry(|e| {
                !e.file_name()
                    .to_str()
                    .map(|s| IGNORE_DIRS.contains(&s))
                    .unwrap_or(false)
            })
            .filter_map(|e| e.ok())
        {
            if matches.len() >= n {
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let fname = entry.file_name().to_string_lossy();
            if let Some(g) = glob_filter {
                if !glob_match::glob_match(g, &fname) {
                    continue;
                }
            }
            let file = match fs::File::open(entry.path()) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = BufReader::new(file);
            for (i, line_result) in reader.lines().enumerate() {
                if matches.len() >= n {
                    break;
                }
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                if re.is_match(&line) {
                    let rel = entry
                        .path()
                        .strip_prefix(&workspace)
                        .unwrap_or(entry.path())
                        .display()
                        .to_string();
                    matches.push(json!({
                        "file": rel,
                        "line": i + 1,
                        "text": truncate_chars(&line, 300)
                    }));
                }
            }
        }
        ToolResult::ok(json!({"engine": "rust", "count": matches.len(), "matches": matches}))
    })
}

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

fn list_files_schema() -> ToolSchema {
    tool_schema(
        "list_files",
        "List files and directories in a workspace directory. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path, relative to the workspace (default: root)." }
            }
        }),
    )
}

fn make_list_files(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let sub = arg_str(&args, "path").unwrap_or(".");
        let base = match resolve_in_workspace(&workspace, sub) {
            Ok(b) => b,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };
        if !base.is_dir() {
            return ToolResult::ok(json!({"error": format!("not a directory: {sub}")}));
        }

        let mut entries: Vec<Value> = Vec::new();
        match fs::read_dir(&base) {
            Ok(dir) => {
                for entry in dir.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    let ftype = entry
                        .file_type()
                        .map(|ft| if ft.is_dir() { "directory" } else { "file" })
                        .unwrap_or("unknown");
                    let path = entry.path();
                    let rel = path
                        .strip_prefix(&workspace)
                        .unwrap_or(path.as_path())
                        .display()
                        .to_string();
                    entries.push(json!({"name": fname, "type": ftype, "path": rel}));
                }
                entries.sort_by(|a, b| {
                    let ta = a["type"].as_str().unwrap_or("").to_string();
                    let tb = b["type"].as_str().unwrap_or("").to_string();
                    if ta == tb {
                        a["name"].as_str().cmp(&b["name"].as_str())
                    } else if ta == "directory" {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                });
                ToolResult::ok(json!({"path": sub, "entries": entries}))
            }
            Err(e) => ToolResult::ok(json!({"error": format!("readdir failed: {e}")})),
        }
    })
}

// ---------------------------------------------------------------------------
// glob_search
// ---------------------------------------------------------------------------

fn glob_search_schema() -> ToolSchema {
    tool_schema(
        "glob_search",
        "Search for files matching a glob pattern in the workspace. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. **/*.rs or src/**" },
                "path": { "type": "string", "description": "Subdirectory to search (default: root)." }
            },
            "required": ["pattern"]
        }),
    )
}

fn make_glob_search(workspace: PathBuf) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Some(p) => p,
            None => return ToolResult::ok(json!({"error": "pattern is required"})),
        };
        let sub = arg_str(&args, "path").unwrap_or(".");
        let base = match resolve_in_workspace(&workspace, sub) {
            Ok(b) => b,
            Err(e) => return ToolResult::ok(json!({"error": e})),
        };

        let mut results: Vec<Value> = Vec::new();
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_entry(|e| {
                !e.file_name()
                    .to_str()
                    .map(|s| IGNORE_DIRS.contains(&s))
                    .unwrap_or(false)
            })
            .filter_map(|e| e.ok())
        {
            let rel = entry
                .path()
                .strip_prefix(&workspace)
                .unwrap_or(entry.path())
                .display()
                .to_string();
            let fname = entry.file_name().to_string_lossy();
            if glob_match::glob_match(pattern, &rel) || glob_match::glob_match(pattern, &fname) {
                let ftype = if entry.file_type().is_dir() {
                    "directory"
                } else {
                    "file"
                };
                results.push(json!({"path": rel, "type": ftype}));
            }
            if results.len() >= 200 {
                break;
            }
        }
        results.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        ToolResult::ok(json!({"pattern": pattern, "count": results.len(), "matches": results}))
    })
}

// ---------------------------------------------------------------------------
// Web tools
// ---------------------------------------------------------------------------

fn web_search_schema() -> ToolSchema {
    tool_schema(
        "web_search",
        "Search the web for information using a search API. Returns title, URL, and snippet for each result. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." }
            },
            "required": ["query"]
        }),
    )
}

fn make_web_search() -> ToolFn {
    Arc::new(|args: Map<String, Value>| -> ToolResult {
        let query = match arg_str(&args, "query") {
            Some(q) => q,
            None => return ToolResult::ok(json!({"error": "query is required"})),
        };

        // Use DuckDuckGo HTML search (no API key needed)
        let client = reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; OpenWorker/1.0)")
            .build()
            .map_err(|e| format!("client error: {e}"))
            .unwrap();

        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
        match client.get(&url).send() {
            Ok(resp) => {
                let html = resp.text().unwrap_or_default();
                let results = parse_duckduckgo_html(&html);
                ToolResult::ok(json!({"query": query, "count": results.len(), "results": results}))
            }
            Err(e) => ToolResult::ok(json!({"error": format!("search failed: {e}")})),
        }
    })
}

fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => result.push_str(&format!("%{:02X}", byte)),
        }
    }
    result
}

fn parse_duckduckgo_html(html: &str) -> Vec<Value> {
    let mut results = Vec::new();
    // Simple regex-based extraction of DuckDuckGo HTML results
    let re_link =
        regex::Regex::new(r#"class="result__a"[^>]*href="([^"]*)"[^>]*>([^<]*)</a>"#).unwrap();
    let re_snippet = regex::Regex::new(r#"class="result__snippet"[^>]*>([^<]*)</a>"#).unwrap();

    for cap in re_link.captures_iter(html) {
        let url = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let title = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        results.push(json!({"title": title, "url": url, "snippet": ""}));
        if results.len() >= 10 {
            break;
        }
    }

    // Try to extract snippets
    for (i, cap) in re_snippet.captures_iter(html).enumerate() {
        if i >= results.len() {
            break;
        }
        let snippet = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if !snippet.is_empty() {
            if let Some(obj) = results[i].as_object_mut() {
                obj.insert("snippet".to_string(), json!(snippet));
            }
        }
    }

    results
}

fn web_fetch_schema() -> ToolSchema {
    tool_schema(
        "web_fetch",
        "Fetch the content of a web page and extract readable text. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch." }
            },
            "required": ["url"]
        }),
    )
}

fn make_web_fetch() -> ToolFn {
    Arc::new(|args: Map<String, Value>| -> ToolResult {
        let url_str = match arg_str(&args, "url") {
            Some(u) => u,
            None => return ToolResult::ok(json!({"error": "url is required"})),
        };

        let client = reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; OpenWorker/1.0)")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| format!("client error: {e}"))
            .unwrap();

        match client.get(url_str).send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status != 200 {
                    return ToolResult::ok(
                        json!({"error": format!("HTTP {status}"), "url": url_str}),
                    );
                }

                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                if !content_type.contains("text/html") && !content_type.contains("text/plain") {
                    // For non-HTML, return truncated text
                    let body = resp.text().unwrap_or_default();
                    let truncated: String = body.chars().take(20000).collect();
                    return ToolResult::ok(
                        json!({"url": url_str, "content_type": content_type, "text": truncated}),
                    );
                }

                let html = resp.text().unwrap_or_default();
                let text = strip_html(&html);
                let truncated: String = text.chars().take(20000).collect();
                ToolResult::ok(json!({"url": url_str, "text": truncated, "length": text.len()}))
            }
            Err(e) => {
                ToolResult::ok(json!({"error": format!("fetch failed: {e}"), "url": url_str}))
            }
        }
    })
}

fn strip_html(html: &str) -> String {
    // Remove script and style tags with content
    let no_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .unwrap()
        .replace_all(html, "");
    let no_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .unwrap()
        .replace_all(&no_script, "");
    // Remove all HTML tags
    let no_tags = regex::Regex::new(r"<[^>]*>")
        .unwrap()
        .replace_all(&no_style, " ");
    // Decode common HTML entities
    let text = no_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse whitespace
    let text = regex::Regex::new(r"\s+").unwrap().replace_all(&text, " ");
    text.trim().to_string()
}

// ---------------------------------------------------------------------------
// Interactive tools (schema carriers — engine intercepts these)
// ---------------------------------------------------------------------------

fn ask_user_schema() -> ToolSchema {
    tool_schema(
        "ask_user",
        "Ask the user a question and wait for their answer.",
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The full question." },
                "options": { "type": "array", "items": { "type": "string" }, "description": "Quick-reply choices." },
                "allow_text": { "type": "boolean", "description": "Keep free-text answer available (default true)." },
                "multi": { "type": "boolean", "description": "Allow multiple options to be selected." },
                "header": { "type": "string", "description": "Short label (~12 chars) for the Inbox chip." }
            },
            "required": ["question"]
        }),
    )
}

fn make_ask_user() -> ToolFn {
    Arc::new(|_args: Map<String, Value>| -> ToolResult {
        ToolResult::ok(
            json!({"answer": "", "error": "asking the user is not available in this surface"}),
        )
    })
}

fn request_directory_schema() -> ToolSchema {
    tool_schema(
        "request_directory",
        "Ask the user to grant access to a directory.",
        json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "Why you need access to this directory." },
                "path": { "type": "string", "description": "Suggested path." },
                "writable": { "type": "boolean", "description": "Whether write access is needed." }
            },
            "required": ["reason"]
        }),
    )
}

fn make_request_directory() -> ToolFn {
    Arc::new(|_args: Map<String, Value>| -> ToolResult {
        ToolResult::ok(
            json!({"granted": false, "error": "directory requests are not available in this surface"}),
        )
    })
}

fn propose_plan_schema() -> ToolSchema {
    tool_schema(
        "propose_plan",
        "Present an implementation plan for user approval.",
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "The implementation plan." }
            },
            "required": ["plan"]
        }),
    )
}

fn make_propose_plan() -> ToolFn {
    Arc::new(|_args: Map<String, Value>| -> ToolResult {
        ToolResult::ok(
            json!({"approved": false, "error": "plan approval is not available in this surface"}),
        )
    })
}

fn todo_write_schema() -> ToolSchema {
    tool_schema(
        "todo_write",
        "Replace the task list. Provide the full list of todos each call.",
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
    )
}

fn make_todo_write(todo: Arc<TodoList>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let raw = extract_todo_array(&args);
        let normalized = normalize_todos(&raw);
        let count = normalized.len();
        let display_items: Vec<Value> = normalized
            .iter()
            .map(|t| json!({"content": t.content, "status": t.status}))
            .collect();
        todo.set_items(normalized);
        ToolResult::ok(json!({"count": count, "todos": display_items}))
    })
}

// ---------------------------------------------------------------------------
// Explorer subagent
// ---------------------------------------------------------------------------

fn explore_schema() -> ToolSchema {
    tool_schema(
        "explore",
        "Delegate a broad, read-only codebase research task to a fresh Explorer context. Use it for multi-file questions; for one known file, use read_file directly.",
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The precise research question and the expected report contents." }
            },
            "required": ["task"]
        }),
    )
}

fn register_explorer_child_tools(registry: &mut ToolRegistry, workspace_root: &str) {
    let workspace = PathBuf::from(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace_root));

    registry.register(
        "read_file",
        make_read_file(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "filesystem",
            parallel_safe: true,
        },
        Some(read_file_schema()),
    );
    registry.register(
        "grep",
        make_grep(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "search",
            parallel_safe: true,
        },
        Some(grep_schema()),
    );
    registry.register(
        "list_files",
        make_list_files(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "filesystem",
            parallel_safe: true,
        },
        Some(list_files_schema()),
    );
    registry.register(
        "glob_search",
        make_glob_search(workspace),
        ToolSpec {
            risk_level: "low",
            category: "search",
            parallel_safe: true,
        },
        Some(glob_search_schema()),
    );
    ocw_git::register_all(registry, workspace_root);
}

fn make_explore(workspace: PathBuf, provider: Arc<dyn Provider>, model: String) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let task = match arg_str(&args, "task") {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => return ToolResult::ok(json!({"error": "task is required"})),
        };
        let child_workspace = workspace.clone();
        let child_provider = Arc::clone(&provider);
        let child_model = model.clone();

        let joined = std::thread::Builder::new()
            .name("openworker-explorer".to_string())
            .spawn(move || {
                let mut registry = ToolRegistry::new();
                let workspace_text = child_workspace.to_string_lossy().to_string();
                register_explorer_child_tools(&mut registry, &workspace_text);
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        return ocw_engine::ExplorerReport {
                            report: String::new(),
                            status: "error".to_string(),
                            error: Some(format!("failed to create explorer runtime: {error}")),
                        };
                    }
                };
                runtime.block_on(ocw_engine::run_explorer(
                    child_provider,
                    Arc::new(registry),
                    child_workspace,
                    child_model,
                    task,
                ))
            });

        let report = match joined {
            Ok(handle) => match handle.join() {
                Ok(report) => report,
                Err(_) => ocw_engine::ExplorerReport {
                    report: String::new(),
                    status: "error".to_string(),
                    error: Some("explorer thread panicked".to_string()),
                },
            },
            Err(error) => ocw_engine::ExplorerReport {
                report: String::new(),
                status: "error".to_string(),
                error: Some(format!("failed to start explorer: {error}")),
            },
        };

        let mut result = json!({
            "report": report.report,
            "status": report.status,
        });
        if let Some(error) = report.error {
            result["error"] = json!(error);
        }
        ToolResult::ok(result)
    })
}

/// Register the read-only Explorer delegation tool. The child registry is built
/// inside the tool invocation and deliberately does not register `explore` again.
pub fn register_explorer(
    registry: &mut ToolRegistry,
    workspace_root: &str,
    provider: Arc<dyn Provider>,
    model: impl Into<String>,
) {
    let workspace = PathBuf::from(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace_root));
    registry.register(
        "explore",
        make_explore(workspace, provider, model.into()),
        ToolSpec {
            risk_level: "low",
            category: "search",
            parallel_safe: true,
        },
        Some(explore_schema()),
    );
}

// ---------------------------------------------------------------------------
// Register all tools
// ---------------------------------------------------------------------------

/// Register all workspace tools (filesystem, search, interactive, planning) into
/// the given `ToolRegistry`. The `workspace_root` is the canonical root directory
/// the tools are confined to. `todo_list` is the shared per-session task list
/// (required so the `todo_write` tool can mutate state visible to the surface).
pub fn register_all(registry: &mut ToolRegistry, workspace_root: &str, todo_list: Arc<TodoList>) {
    let workspace = PathBuf::from(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace_root));

    // Filesystem
    registry.register(
        "read_file",
        make_read_file(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "filesystem",
            parallel_safe: true,
        },
        Some(read_file_schema()),
    );
    registry.register(
        "write_file",
        make_write_file(workspace.clone()),
        ToolSpec {
            risk_level: "high",
            category: "filesystem",
            parallel_safe: false,
        },
        Some(write_file_schema()),
    );
    registry.register(
        "edit_file",
        make_edit_file(workspace.clone()),
        ToolSpec {
            risk_level: "high",
            category: "filesystem",
            parallel_safe: false,
        },
        Some(edit_file_schema()),
    );
    registry.register(
        "create_directory",
        make_create_directory(workspace.clone()),
        ToolSpec {
            risk_level: "medium",
            category: "filesystem",
            parallel_safe: true,
        },
        Some(create_directory_schema()),
    );
    registry.register(
        "delete_file",
        make_delete_file(workspace.clone()),
        ToolSpec {
            risk_level: "high",
            category: "filesystem",
            parallel_safe: false,
        },
        Some(delete_file_schema()),
    );

    // Search
    registry.register(
        "grep",
        make_grep(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "search",
            parallel_safe: true,
        },
        Some(grep_schema()),
    );
    registry.register(
        "list_files",
        make_list_files(workspace.clone()),
        ToolSpec {
            risk_level: "low",
            category: "filesystem",
            parallel_safe: true,
        },
        Some(list_files_schema()),
    );
    registry.register(
        "glob_search",
        make_glob_search(workspace),
        ToolSpec {
            risk_level: "low",
            category: "search",
            parallel_safe: true,
        },
        Some(glob_search_schema()),
    );

    // Interactive (schema carriers — engine intercepts)
    registry.register(
        "ask_user",
        make_ask_user(),
        ToolSpec {
            risk_level: "low",
            category: "interaction",
            parallel_safe: false,
        },
        Some(ask_user_schema()),
    );
    registry.register(
        "request_directory",
        make_request_directory(),
        ToolSpec {
            risk_level: "low",
            category: "filesystem",
            parallel_safe: false,
        },
        Some(request_directory_schema()),
    );

    // Planning
    registry.register(
        "propose_plan",
        make_propose_plan(),
        ToolSpec {
            risk_level: "low",
            category: "planning",
            parallel_safe: false,
        },
        Some(propose_plan_schema()),
    );
    registry.register(
        "todo_write",
        make_todo_write(todo_list),
        ToolSpec {
            risk_level: "low",
            category: "planning",
            parallel_safe: true,
        },
        Some(todo_write_schema()),
    );

    // Web
    registry.register(
        "web_search",
        make_web_search(),
        ToolSpec {
            risk_level: "low",
            category: "web",
            parallel_safe: true,
        },
        Some(web_search_schema()),
    );
    registry.register(
        "web_fetch",
        make_web_fetch(),
        ToolSpec {
            risk_level: "low",
            category: "web",
            parallel_safe: true,
        },
        Some(web_fetch_schema()),
    );
}
