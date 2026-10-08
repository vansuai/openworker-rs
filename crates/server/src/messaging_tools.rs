//! Agent messaging tools — `send_message` / `send_file`.
//!
//! Mirrors `coworker/connectors/tools.py`: parses the reply handle, resolves bot
//! tokens from the SecretStore at call time, and dispatches via the stateless
//! senders in `ocw_connectors::senders`.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ocw_connectors::{
    list_channels, parse_target, send_slack, send_slack_file, send_telegram, split_slack_chat_id,
    FileSender, SecretResolver, Sender,
};
use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};

const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Context for outbound messaging tools.
pub struct MessagingToolsCtx {
    pub secrets: SecretResolver,
    /// Enumerate stored profile keys (for `slack:team:*` resolution).
    pub profile_keys: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pub workspace: PathBuf,
}

fn messaging_spec() -> ToolSpec {
    ToolSpec {
        risk_level: "medium",
        category: "messaging",
        parallel_safe: false,
    }
}

fn slack_channel_name_like(chat_id: &str) -> bool {
    if chat_id.starts_with('#') {
        return true;
    }
    let s = chat_id.trim();
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
    })
}

fn parse_or_coerce(target: &str) -> Result<(String, String, Option<String>), String> {
    match parse_target(target) {
        Ok(v) => Ok(v),
        Err(_) => {
            let raw = target.trim();
            if !raw.is_empty() && slack_channel_name_like(raw.trim_start_matches('#')) {
                Ok(("slack".into(), raw.to_string(), None))
            } else {
                Err(format!(
                    "invalid target {target:?} (expected 'platform:chat_id[:thread]')"
                ))
            }
        }
    }
}

fn secret_bot_token(secrets: &SecretResolver, key: &str) -> Option<String> {
    secrets(key).and_then(|v| {
        v.get("bot_token")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

fn resolve_token(secrets: &SecretResolver, platform: &str, chat_id: &str) -> Option<String> {
    if platform == "slack" {
        let (team, _) = split_slack_chat_id(chat_id);
        if let Some(team) = team {
            if let Some(token) = secret_bot_token(secrets, &format!("slack:team:{team}")) {
                return Some(token);
            }
        }
    }
    secret_bot_token(secrets, &format!("{platform}:default"))
}

fn slack_team_ids(secrets: &SecretResolver, profile_keys: &dyn Fn() -> Vec<String>) -> Vec<String> {
    let mut teams: Vec<String> = profile_keys()
        .into_iter()
        .filter_map(|k| {
            k.strip_prefix("slack:team:")
                .filter(|id| !id.is_empty())
                .map(String::from)
        })
        .collect();
    if teams.is_empty() && secret_bot_token(secrets, "slack:default").is_some() {
        teams.push("default".to_string());
    }
    teams
}

fn resolve_slack_channel(
    secrets: &SecretResolver,
    profile_keys: &dyn Fn() -> Vec<String>,
    name: &str,
) -> Result<String, String> {
    let query = name.trim_start_matches('#').trim();
    let teams = slack_team_ids(secrets, profile_keys);
    if teams.is_empty() {
        return Err("no bot token for slack — connect it first".into());
    }
    let mut hits: Vec<(String, Value)> = Vec::new();
    for team in &teams {
        let token = if team == "default" {
            secret_bot_token(secrets, "slack:default")
        } else {
            secret_bot_token(secrets, &format!("slack:team:{team}"))
        };
        let Some(token) = token else {
            continue;
        };
        let roster = list_channels(&token, team, query, 50, false);
        if roster.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if let Some(channels) = roster.get("channels").and_then(Value::as_array) {
            for c in channels {
                let cname = c.get("name").and_then(Value::as_str).unwrap_or("");
                if cname.eq_ignore_ascii_case(query) {
                    hits.push((team.clone(), c.clone()));
                }
            }
        }
    }
    if hits.is_empty() {
        let plural = if teams.len() > 1 { "s" } else { "" };
        return Err(format!(
            "no Slack channel named #{query} in the connected workspace{plural} — check the name, \
             or pass the full address (slack:C… / slack:T…/C…)"
        ));
    }
    if hits.len() > 1 {
        return Err(format!(
            "#{query} exists in more than one connected workspace — use the full address \
             (slack:TEAM_ID/CHANNEL_ID) to pick one"
        ));
    }
    let (team, channel) = &hits[0];
    let channel_id = channel
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "channel missing id".to_string())?;
    let is_member = channel
        .get("is_member")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_member {
        return Err(format!(
            "found #{query}, but the bot isn't a member — invite @OpenWorker to #{query} in Slack, \
             then retry"
        ));
    }
    let chat_id = if team == "default" {
        channel_id.to_string()
    } else {
        format!("{team}/{channel_id}")
    };
    Ok(chat_id)
}

fn resolve_within_simple(path: &str, bases: &[PathBuf]) -> Option<PathBuf> {
    let raw = Path::new(path);
    let mut candidates: Vec<PathBuf> = if raw.is_absolute() {
        vec![raw.to_path_buf()]
    } else {
        bases.iter().map(|b| b.join(raw)).collect()
    };
    for cand in candidates.drain(..) {
        let resolved = match cand.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        for base in bases {
            let base_resolved = match base.canonicalize() {
                Ok(p) => p,
                Err(_) => base.clone(),
            };
            if resolved.starts_with(&base_resolved) {
                return Some(resolved);
            }
        }
    }
    None
}

fn path_has_parent_traversal(path: &str) -> bool {
    Path::new(path).components().any(|c| matches!(c, Component::ParentDir))
}

/// Register `send_message` and `send_file` (always, when called — mirrors Python
/// agent.py for messaging personas).
pub fn register_messaging_tools(registry: &mut ToolRegistry, ctx: MessagingToolsCtx) {
    register_send_message(registry, ctx.clone());
    register_send_file(registry, ctx);
}

impl Clone for MessagingToolsCtx {
    fn clone(&self) -> Self {
        Self {
            secrets: Arc::clone(&self.secrets),
            profile_keys: Arc::clone(&self.profile_keys),
            workspace: self.workspace.clone(),
        }
    }
}

fn register_send_message(registry: &mut ToolRegistry, ctx: MessagingToolsCtx) {
    let send_message: ToolFn = Arc::new(move |args: Map<String, Value>| {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (platform, mut chat_id, thread_id) = match parse_or_coerce(&target) {
            Ok(v) => v,
            Err(e) => return ToolResult::ok(json!({ "error": e })),
        };
        let sender: Option<Sender> = match platform.as_str() {
            "telegram" => Some(send_telegram),
            "slack" => Some(send_slack),
            _ => None,
        };
        let Some(sender) = sender else {
            return ToolResult::ok(json!({ "error": format!("unknown platform: {platform}") }));
        };
        if platform == "slack" && slack_channel_name_like(&chat_id) {
            match resolve_slack_channel(&ctx.secrets, &*ctx.profile_keys, &chat_id) {
                Ok(id) => chat_id = id,
                Err(e) => return ToolResult::ok(json!({ "error": e })),
            }
        }
        let Some(token) = resolve_token(&ctx.secrets, &platform, &chat_id) else {
            return ToolResult::ok(json!({
                "error": format!("no bot token for {platform} — connect it first")
            }));
        };
        let thread = thread_id.as_deref();
        let result = sender(&token, &chat_id, &text, thread);
        if result.ok {
            ToolResult::ok(json!({
                "ok": true,
                "message_id": result.message_id,
                "target": target,
            }))
        } else {
            ToolResult::ok(json!({
                "error": result.error.unwrap_or_else(|| "send failed".into())
            }))
        }
    });
    registry.register(
        "send_message",
        send_message,
        messaging_spec(),
        Some(ToolSchema::new(
            "send_message",
            Some(
                "Send a message to a connected chat (Slack or Telegram). `target` is the reply \
                 handle from an inbound message (e.g. 'telegram:12345' or 'slack:C0123', \
                 optionally with a ':<thread>' suffix) — or, for Slack, just the channel NAME \
                 ('#general' or 'general'; resolved against the connected workspaces). Use this \
                 to actually reach a person — plain assistant text is not delivered anywhere.",
            ),
            Some(json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Destination handle 'platform:chat_id[:thread]', e.g. 'telegram:12345'."
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send."
                    }
                },
                "required": ["target", "text"]
            })),
        )),
    );
}

fn register_send_file(registry: &mut ToolRegistry, ctx: MessagingToolsCtx) {
    let send_file: ToolFn = Arc::new(move |args: Map<String, Value>| {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(String::from);
        let comment = args
            .get("comment")
            .and_then(Value::as_str)
            .map(String::from);
        let as_screenshot = args
            .get("as_screenshot")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let (platform, mut chat_id, thread_id) = match parse_or_coerce(&target) {
            Ok(v) => v,
            Err(e) => return ToolResult::ok(json!({ "error": e })),
        };
        if platform != "slack" {
            return ToolResult::ok(json!({
                "error": format!("file sending is not supported on {platform} yet")
            }));
        }
        if platform == "slack" && slack_channel_name_like(&chat_id) {
            match resolve_slack_channel(&ctx.secrets, &*ctx.profile_keys, &chat_id) {
                Ok(id) => chat_id = id,
                Err(e) => return ToolResult::ok(json!({ "error": e })),
            }
        }
        let bases = vec![ctx.workspace.clone()];
        if bases.is_empty() {
            return ToolResult::ok(json!({ "error": "no workspace folders available to read from" }));
        }
        if path_has_parent_traversal(&path) {
            return ToolResult::ok(json!({
                "error": "path is outside the folders this session can access (or missing)"
            }));
        }
        let Some(resolved) = resolve_within_simple(&path, &bases) else {
            return ToolResult::ok(json!({
                "error": "path is outside the folders this session can access (or missing)"
            }));
        };
        if !resolved.is_file() {
            return ToolResult::ok(json!({
                "error": "path is outside the folders this session can access (or missing)"
            }));
        }
        let Some(token) = resolve_token(&ctx.secrets, &platform, &chat_id) else {
            return ToolResult::ok(json!({
                "error": format!("no bot token for {platform} — connect it first")
            }));
        };
        if as_screenshot {
            let ext = resolved
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "html" && ext != "htm" {
                return ToolResult::ok(json!({ "error": "as_screenshot only applies to .html files" }));
            }
            return ToolResult::ok(json!({
                "error": "as_screenshot is not available in the Rust server yet"
            }));
        }
        let size = match std::fs::metadata(&resolved) {
            Ok(m) => m.len(),
            Err(_) => {
                return ToolResult::ok(json!({
                    "error": "path is outside the folders this session can access (or missing)"
                }))
            }
        };
        if size > MAX_FILE_BYTES {
            return ToolResult::ok(json!({ "error": "file is larger than 50 MB" }));
        }
        let data = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(e) => return ToolResult::ok(json!({ "error": e.to_string() })),
        };
        let filename = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let sender: FileSender = send_slack_file;
        let result = sender(
            &token,
            &chat_id,
            thread_id.as_deref(),
            &filename,
            &data,
            title.as_deref(),
            comment.as_deref(),
        );
        if result.ok {
            ToolResult::ok(json!({
                "ok": true,
                "file_id": result.message_id,
                "target": target,
                "filename": filename,
            }))
        } else {
            ToolResult::ok(json!({
                "error": result.error.unwrap_or_else(|| "file send failed".into())
            }))
        }
    });
    registry.register(
        "send_file",
        send_file,
        messaging_spec(),
        Some(ToolSchema::new(
            "send_file",
            Some(
                "Upload a file from the session's workspace into a connected chat (Slack). \
                 `target` is the same handle send_message uses. Slack shows its own previews for \
                 pdf/csv/images — send the actual file, not a screenshot of it. For .html artifacts \
                 (which Slack can't preview) set as_screenshot=true to send a rendered PNG instead. \
                 This is a DISTINCT permission from send_message: it asks for approval even in \
                 threads where text replies are pre-approved.",
            ),
            Some(json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Destination handle 'platform:chat_id[:thread]', e.g. 'slack:C0123:171234.5678'."
                    },
                    "path": {
                        "type": "string",
                        "description": "The file to send — workspace-relative, or absolute within an allowed folder."
                    },
                    "title": {
                        "type": "string",
                        "description": "Display title (defaults to the filename)."
                    },
                    "comment": {
                        "type": "string",
                        "description": "Short message posted with the file."
                    },
                    "as_screenshot": {
                        "type": "boolean",
                        "description": "HTML only: render the page headless and send a PNG preview instead of the raw file."
                    }
                },
                "required": ["target", "path"]
            })),
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_name_like_matches_python_rules() {
        assert!(slack_channel_name_like("#general"));
        assert!(slack_channel_name_like("all-openworker"));
        assert!(!slack_channel_name_like("C0123ABC"));
        assert!(!slack_channel_name_like("General"));
    }

    #[test]
    fn parse_or_coerce_slack_channel_name() {
        let (p, c, th) = parse_or_coerce("all-openworker").unwrap();
        assert_eq!(p, "slack");
        assert_eq!(c, "all-openworker");
        assert!(th.is_none());
    }
}
