//! Git tools — read-only wrappers around `git` CLI.
//!
//! `git_status`, `git_diff`, `git_log`. All run in the workspace; read-only.
//! No commit/push here (those go through `run_shell` with approval).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn git_cmd(workspace: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .map_err(|e| format!("git failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git error: {}",
            stderr.trim().chars().take(300).collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn arg_i64(args: &Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn arg_bool(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// git_status
// ---------------------------------------------------------------------------

fn git_status_schema() -> ToolSchema {
    ToolSchema::new(
        "git_status",
        Some("Show working-tree status (porcelain format). Read-only."),
        Some(json!({
            "type": "object",
            "properties": {},
        })),
    )
}

fn make_git_status(workspace: String) -> ToolFn {
    Arc::new(move |_args: Map<String, Value>| -> ToolResult {
        match git_cmd(&workspace, &["status", "--porcelain"]) {
            Ok(stdout) => {
                let files: Vec<Value> = stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let status = &l[..2.min(l.len())];
                        let file = l[3..].trim();
                        json!({"status": status.trim(), "file": file})
                    })
                    .collect();
                ToolResult::ok(json!({"files": files}))
            }
            Err(e) => ToolResult::ok(json!({"error": e})),
        }
    })
}

// ---------------------------------------------------------------------------
// git_diff
// ---------------------------------------------------------------------------

fn git_diff_schema() -> ToolSchema {
    ToolSchema::new(
        "git_diff",
        Some("Show unstaged (default) or staged (--staged) diff. Optional path. Read-only."),
        Some(json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "description": "Show staged changes (--staged)." },
                "path": { "type": "string", "description": "Limit diff to this file/directory." }
            },
        })),
    )
}

fn make_git_diff(workspace: String) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let staged = arg_bool(&args, "staged");
        let path = arg_str(&args, "path");
        let mut cmd_args: Vec<&str> = vec!["diff"];
        if staged {
            cmd_args.push("--staged");
        }
        if let Some(p) = path {
            cmd_args.push("--");
            cmd_args.push(p);
        }
        // leak string for args lifetime — acceptable since this is a static tool
        let args_slice: Vec<String> = cmd_args.iter().map(|s| s.to_string()).collect();
        let args_refs: Vec<&str> = args_slice.iter().map(|s| s.as_str()).collect();
        match git_cmd(&workspace, &args_refs) {
            Ok(diff) => ToolResult::ok(json!({"diff": diff})),
            Err(e) => ToolResult::ok(json!({"error": e})),
        }
    })
}

// ---------------------------------------------------------------------------
// git_log
// ---------------------------------------------------------------------------

fn git_log_schema() -> ToolSchema {
    ToolSchema::new("git_log", Some(
        "Recent git commit history (hash, author, date, subject). Optionally scope to a path. Read-only."
    ), Some(json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Optional file/dir to scope history to." },
            "max_count": { "type": "integer", "description": "How many commits (default 20, max 200)." }
        },
    })))
}

fn make_git_log(workspace: String) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let max_count = arg_i64(&args, "max_count")
            .filter(|&n| n > 0)
            .map(|n| n as usize)
            .unwrap_or(20)
            .min(200);
        let path = arg_str(&args, "path");

        let sep = "\x1f";
        let format = format!("--pretty=format:%h{sep}%an{sep}%ad{sep}%s");
        let mut cmd_args: Vec<String> = vec![
            "log".to_string(),
            format!("-n{max_count}"),
            format,
            "--date=short".to_string(),
        ];
        if let Some(p) = path {
            cmd_args.push("--".to_string());
            cmd_args.push(p.to_string());
        }
        let args_refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
        match git_cmd(&workspace, &args_refs) {
            Ok(stdout) => {
                let commits: Vec<Value> = stdout
                    .lines()
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.split(sep).collect();
                        if parts.len() == 4 {
                            Some(json!({
                                "hash": parts[0],
                                "author": parts[1],
                                "date": parts[2],
                                "subject": parts[3],
                            }))
                        } else {
                            None
                        }
                    })
                    .collect();
                ToolResult::ok(json!({"count": commits.len(), "commits": commits}))
            }
            Err(e) => ToolResult::ok(json!({"error": e})),
        }
    })
}

// ---------------------------------------------------------------------------
// Register
// ---------------------------------------------------------------------------

/// Register git tools into `registry`. All tools are scoped to `workspace_root`.
pub fn register_all(registry: &mut ToolRegistry, workspace_root: &str) {
    let ws: String = PathBuf::from(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace_root))
        .display()
        .to_string();

    registry.register(
        "git_status",
        make_git_status(ws.clone()),
        ToolSpec {
            risk_level: "low",
            category: "git",
            parallel_safe: true,
        },
        Some(git_status_schema()),
    );
    registry.register(
        "git_diff",
        make_git_diff(ws.clone()),
        ToolSpec {
            risk_level: "low",
            category: "git",
            parallel_safe: true,
        },
        Some(git_diff_schema()),
    );
    registry.register(
        "git_log",
        make_git_log(ws),
        ToolSpec {
            risk_level: "low",
            category: "git",
            parallel_safe: true,
        },
        Some(git_log_schema()),
    );
}
