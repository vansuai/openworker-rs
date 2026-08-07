//! GitHub integration tools — issues, PRs, commits, clone/pull.
//!
//! Mirrors `coworker/connectors/integration_tools.py` (github_* section). Uses
//! the REST API (`api.github.com`) with the stored personal-access token; git
//! operations (clone/pull) shell out to the system `git` binary so HTTPS auth
//! goes through git-credential helpers exactly like the Python side.

use super::helpers::{
    arg_i64, arg_str_opt, arg_string, clamp, err, ok, request_json, schema, IntegrationContext,
};
use ocw_engine::{ToolFn, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

fn github_base() -> String {
    std::env::var("GITHUB_API_URL")
        .unwrap_or_else(|_| "https://api.github.com".to_string())
}

fn github_token(ctx: &IntegrationContext) -> Option<String> {
    // GitHub profile can live under "github" (classic) or "github:default".
    ctx.secret_str("github", "token")
        .or_else(|| ctx.secret_str("github:default", "token"))
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
}

fn headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Accept", "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version", "2022-11-28".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Tool schemas
// ---------------------------------------------------------------------------

fn schema_search() -> ToolSchema {
    schema(
        "github_search",
        "Search GitHub repositories, issues, or PRs. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "GitHub search query (e.g. 'repo:user/repo is:issue bug')."},
                "max_results": {"type": "integer", "description": "Max results (default 10, max 20)."}
            },
            "required": ["query"]
        }),
    )
}

fn schema_get_issue() -> ToolSchema {
    schema(
        "github_get_issue",
        "Fetch an issue or PR by number from a repository. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "issue_number": {"type": "integer"}
            },
            "required": ["owner", "repo", "issue_number"]
        }),
    )
}

fn schema_create_issue() -> ToolSchema {
    schema(
        "github_create_issue",
        "Create an issue in a repository.",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "title": {"type": "string"},
                "body": {"type": "string", "description": "Markdown body."},
                "labels": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["owner", "repo", "title"]
        }),
    )
}

fn schema_reply() -> ToolSchema {
    schema(
        "github_reply",
        "Comment on an issue or PR.",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "number": {"type": "integer"},
                "body": {"type": "string", "description": "Markdown comment body."}
            },
            "required": ["owner", "repo", "number", "body"]
        }),
    )
}

fn schema_review() -> ToolSchema {
    schema(
        "github_review",
        "Submit a review on a pull request (approve / request changes / comment).",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "pull_number": {"type": "integer"},
                "event": {"type": "string", "enum": ["APPROVE", "REQUEST_CHANGES", "COMMENT"]},
                "body": {"type": "string"}
            },
            "required": ["owner", "repo", "pull_number", "event"]
        }),
    )
}

fn schema_list_commits() -> ToolSchema {
    schema(
        "github_list_commits",
        "List recent commits on a branch of a repository. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "branch": {"type": "string", "description": "Branch (default: default branch)."},
                "max_results": {"type": "integer"}
            },
            "required": ["owner", "repo"]
        }),
    )
}

fn schema_clone() -> ToolSchema {
    schema(
        "github_clone",
        "Clone a GitHub repository into the session workspace.",
        json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "directory": {"type": "string", "description": "Target dir under the workspace (default: repo name)."}
            },
            "required": ["owner", "repo"]
        }),
    )
}

fn schema_pull() -> ToolSchema {
    schema(
        "github_pull",
        "Pull the latest changes for a repo previously cloned into the workspace.",
        json!({
            "type": "object",
            "properties": {
                "directory": {"type": "string", "description": "Repo directory under the workspace."}
            },
            "required": ["directory"]
        }),
    )
}

// ---------------------------------------------------------------------------
// Tool factories
// ---------------------------------------------------------------------------

fn github_search(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let query = arg_string(&args, "query");
        if query.is_empty() {
            return ToolResult::ok(err("query is required"));
        }
        let max = clamp(arg_i64(&args, "max_results"), 10, 20);
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let url = format!(
            "{}search/issues?q={}&per_page={}",
            github_base(),
            urlencode(query),
            max
        );
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => {
                let items = v.get("items").cloned().unwrap_or(Value::Array(vec![]));
                ToolResult::ok(ok(json!({
                    "query": query,
                    "count": items.as_array().map(|a| a.len()).unwrap_or(0),
                    "items": items,
                })))
            }
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn github_get_issue(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        let number = arg_i64(&args, "issue_number");
        if owner.is_empty() || repo.is_empty() || number.is_none() {
            return ToolResult::ok(err("owner, repo, and issue_number are required"));
        }
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let url = format!("{}/repos/{owner}/{repo}/issues/{}", github_base(), number.unwrap());
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => ToolResult::ok(ok(v)),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn github_create_issue(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        let title = arg_string(&args, "title");
        if owner.is_empty() || repo.is_empty() || title.is_empty() {
            return ToolResult::ok(err("owner, repo, and title are required"));
        }
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let mut body = json!({"title": title});
        if let Some(b) = arg_str_opt(&args, "body") {
            body["body"] = json!(b);
        }
        if let Some(labels) = args.get("labels").and_then(|v| v.as_array()) {
            body["labels"] = json!(labels);
        }
        let url = format!("{}/repos/{owner}/{repo}/issues", github_base());
        match request_json("POST", &url, &headers(&token), Some(&body)) {
            Ok(v) => {
                let number = v.get("number").and_then(|n| n.as_i64());
                ToolResult::ok(ok(json!({
                    "ok": true,
                    "number": number,
                    "url": v.get("html_url").and_then(|u| u.as_str()),
                    "state": v.get("state").and_then(|s| s.as_str()),
                })))
            }
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn github_reply(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        let number = arg_i64(&args, "number");
        let body = arg_string(&args, "body");
        if owner.is_empty() || repo.is_empty() || number.is_none() || body.is_empty() {
            return ToolResult::ok(err("owner, repo, number, and body are required"));
        }
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{}/comments",
            github_base(),
            number.unwrap()
        );
        let payload = json!({"body": body});
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "comment_id": v.get("id").and_then(|i| i.as_i64()),
                "html_url": v.get("html_url").and_then(|u| u.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn github_review(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        let number = arg_i64(&args, "pull_number");
        let event = arg_string(&args, "event");
        if owner.is_empty() || repo.is_empty() || number.is_none() || event.is_empty() {
            return ToolResult::ok(err("owner, repo, pull_number, and event are required"));
        }
        let event = match event {
            "APPROVE" | "REQUEST_CHANGES" | "COMMENT" => event.to_string(),
            other => {
                return ToolResult::ok(err(format!(
                    "invalid review event {other:?} (expected APPROVE / REQUEST_CHANGES / COMMENT)"
                )))
            }
        };
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let mut payload = json!({"event": event});
        if let Some(b) = arg_str_opt(&args, "body") {
            payload["body"] = json!(b);
        }
        let url = format!(
            "{}/repos/{owner}/{repo}/pulls/{}/reviews",
            github_base(),
            number.unwrap()
        );
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "review_id": v.get("id").and_then(|i| i.as_i64()),
                "state": v.get("state").and_then(|s| s.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn github_list_commits(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        if owner.is_empty() || repo.is_empty() {
            return ToolResult::ok(err("owner and repo are required"));
        }
        let max = clamp(arg_i64(&args, "max_results"), 10, 50);
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let mut url = format!(
            "{}/repos/{owner}/{repo}/commits?per_page={}",
            github_base(),
            max
        );
        if let Some(branch) = arg_str_opt(&args, "branch") {
            url.push_str(&format!("&sha={}", urlencode(branch)));
        }
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => ToolResult::ok(ok(v)),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

/// Resolve the target directory for a clone, enforcing workspace confinement.
fn resolve_clone_target(
    ctx: &IntegrationContext,
    repo: &str,
    directory: Option<&str>,
) -> Result<PathBuf, String> {
    let ws = (ctx.workspace)().ok_or_else(|| "no workspace available for github_clone".to_string())?;
    let ws_canon = ws
        .canonicalize()
        .unwrap_or_else(|_| ws.clone());
    let dir = directory.unwrap_or(repo);
    let target = ws.join(dir);
    if let Ok(canon) = target.canonicalize() {
        if !canon.starts_with(&ws_canon) {
            return Err("directory escapes the workspace".to_string());
        }
    } else if let Some(parent) = target.parent() {
        if let Ok(pc) = parent.canonicalize() {
            if !pc.starts_with(&ws_canon) {
                return Err("directory escapes the workspace".to_string());
            }
        }
    }
    Ok(target)
}

fn github_clone(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let owner = arg_string(&args, "owner");
        let repo = arg_string(&args, "repo");
        if owner.is_empty() || repo.is_empty() {
            return ToolResult::ok(err("owner and repo are required"));
        }
        let token = match github_token(&ctx) {
            Some(t) => t,
            None => return ToolResult::ok(err("no GitHub token — connect GitHub first")),
        };
        let target = match resolve_clone_target(&ctx, repo, arg_str_opt(&args, "directory")) {
            Ok(t) => t,
            Err(e) => return ToolResult::ok(err(e)),
        };
        if target.exists() {
            return ToolResult::ok(err(format!(
                "{} already exists — use github_pull to update it",
                target.display()
            )));
        }
        // HTTPS URL with the token in the URL lets plain `git clone` authenticate.
        let clone_url = format!("https://x-access-token:{token}@github.com/{owner}/{repo}.git");
        let out = Command::new("git")
            .arg("clone")
            .arg("--depth")
            .arg("1")
            .arg(&clone_url)
            .arg(&target)
            .output();
        match out {
            Ok(o) if o.status.success() => ToolResult::ok(ok(json!({
                "ok": true,
                "path": target.display().to_string(),
                "repo": format!("{owner}/{repo}"),
            }))),
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                ToolResult::ok(err(format!("git clone failed: {}", msg)))
            }
            Err(e) => ToolResult::ok(err(format!("git clone failed: {e}"))),
        }
    })
}

fn github_pull(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let directory = arg_string(&args, "directory");
        if directory.is_empty() {
            return ToolResult::ok(err("directory is required"));
        }
        let ws = match (ctx.workspace)() {
            Some(w) => w,
            None => return ToolResult::ok(err("no workspace available")),
        };
        let target = ws.join(directory);
        if !target.is_dir() {
            return ToolResult::ok(err(format!("{} is not a directory", target.display())));
        }
        // Token not strictly needed for public pull; add it when configured so
        // private repos work too.
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&target).args(["pull", "--ff-only"]);
        if let Some(token) = github_token(&ctx) {
            cmd.env(
                "GIT_ASKPASS",
                askpass_script(&token),
            );
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {
                let msg = String::from_utf8_lossy(&o.stdout).trim().to_string();
                ToolResult::ok(ok(json!({"ok": true, "result": msg})))
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                ToolResult::ok(err(format!("git pull failed: {}", msg)))
            }
            Err(e) => ToolResult::ok(err(format!("git pull failed: {e}"))),
        }
    })
}

/// GIT_ASKPASS helper: echo the token for username prompts. Written to a temp
/// file at call time (only when a token is configured).
fn askpass_script(token: &str) -> String {
    let script = "#!/bin/sh\nprintf '%s\\n' \"$GITHUB_TOKEN\"\n".to_string();
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ocw-askpass-{}.sh", std::process::id()));
    let _ = std::fs::write(&path, script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    let _ = token;
    path.display().to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Register all GitHub tools into the registry.
pub fn register(ctx: Arc<IntegrationContext>, registry: &mut ocw_engine::ToolRegistry) {
    let spec = |risk: &'static str| ToolSpec {
        risk_level: risk,
        category: "github",
        parallel_safe: false,
    };
    registry.register(
        "github_search",
        github_search(ctx.clone()),
        spec("low"),
        Some(schema_search()),
    );
    registry.register(
        "github_get_issue",
        github_get_issue(ctx.clone()),
        spec("low"),
        Some(schema_get_issue()),
    );
    registry.register(
        "github_create_issue",
        github_create_issue(ctx.clone()),
        spec("medium"),
        Some(schema_create_issue()),
    );
    registry.register(
        "github_reply",
        github_reply(ctx.clone()),
        spec("medium"),
        Some(schema_reply()),
    );
    registry.register(
        "github_review",
        github_review(ctx.clone()),
        spec("medium"),
        Some(schema_review()),
    );
    registry.register(
        "github_list_commits",
        github_list_commits(ctx.clone()),
        spec("low"),
        Some(schema_list_commits()),
    );
    registry.register(
        "github_clone",
        github_clone(ctx.clone()),
        spec("medium"),
        Some(schema_clone()),
    );
    registry.register(
        "github_pull",
        github_pull(ctx.clone()),
        spec("medium"),
        Some(schema_pull()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_basic() {
        assert_eq!(urlencode("repo:user/repo is:issue"), "repo%3Auser%2Frepo+is%3Aissue");
    }

    #[test]
    fn clamp_values() {
        assert_eq!(clamp(None, 10, 20), 10);
        assert_eq!(clamp(Some(3), 10, 20), 3);
        assert_eq!(clamp(Some(99), 10, 20), 20);
        assert_eq!(clamp(Some(-5), 10, 20), 10);
    }
}
