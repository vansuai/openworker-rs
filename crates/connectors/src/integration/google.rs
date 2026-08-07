//! Google integration tools — Gmail and Google Calendar.
//!
//! Mirrors `coworker/connectors/integration_tools.py` (gmail_* / gcal_*
//! sections). Uses the Google REST APIs with OAuth access tokens stored in the
//! connector profile (`gmail:default` / `gcal:default` → `token`).

use super::helpers::{
    arg_i64, arg_str_opt, arg_string, clamp, err, ok, request_json, schema, IntegrationContext,
};
use ocw_engine::{ToolFn, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const GOOGLE_BASE: &str = "https://gmail.googleapis.com";
const GCAL_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Profile key for a Gmail account (default or named).
fn gmail_profile_key(account: &str) -> String {
    if account.is_empty() {
        "gmail:default".to_string()
    } else {
        format!("gmail:{account}")
    }
}

fn gcal_profile_key(account: &str) -> String {
    if account.is_empty() {
        "gcal:default".to_string()
    } else {
        format!("gcal:{account}")
    }
}

fn profile_token(ctx: &IntegrationContext, key: &str) -> Option<String> {
    ctx.secret_str(key, "token").or_else(|| {
        ctx.secret_str(key, "access_token")
    })
}

fn headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Accept", "application/json".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Gmail schemas + factories
// ---------------------------------------------------------------------------

fn gmail_search_schema() -> ToolSchema {
    schema(
        "gmail_search_messages",
        "Search a Gmail inbox for messages (by query, label, from, subject…). Read-only.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Gmail search query (Gmail syntax)."},
                "max_results": {"type": "integer", "description": "Max messages (default 10, max 20)."},
                "account": {"type": "string", "description": "Gmail account id (default: primary)."}
            },
            "required": ["query"]
        }),
    )
}

fn gmail_get_schema() -> ToolSchema {
    schema(
        "gmail_get_message",
        "Fetch a single Gmail message by id, including the parsed body. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "message_id": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["message_id"]
        }),
    )
}

fn gmail_send_schema() -> ToolSchema {
    schema(
        "gmail_send_email",
        "Send an email from the connected Gmail account.",
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Recipient email address."},
                "subject": {"type": "string"},
                "body": {"type": "string", "description": "Plain-text body."},
                "account": {"type": "string"}
            },
            "required": ["to", "subject", "body"]
        }),
    )
}

fn gmail_search(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let query = arg_string(&args, "query");
        if query.is_empty() {
            return ToolResult::ok(err("query is required"));
        }
        let max = clamp(arg_i64(&args, "max_results"), 10, 20);
        let account = arg_string(&args, "account");
        let key = gmail_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Gmail token for {key} — connect Gmail first"))),
        };
        let url = format!(
            "{GOOGLE_BASE}/gmail/v1/users/me/messages?q={}&maxResults={}",
            urlencode(query),
            max
        );
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => ToolResult::ok(ok(v)),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn gmail_get(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let mid = arg_string(&args, "message_id");
        if mid.is_empty() {
            return ToolResult::ok(err("message_id is required"));
        }
        let account = arg_string(&args, "account");
        let key = gmail_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Gmail token for {key} — connect Gmail first"))),
        };
        let url = format!(
            "{GOOGLE_BASE}/gmail/v1/users/me/messages/{mid}?format=full"
        );
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => {
                // Decode base64url body parts into a readable text summary.
                let body_text = extract_body_text(&v);
                ToolResult::ok(ok(json!({
                    "id": v.get("id").and_then(|i| i.as_str()),
                    "thread_id": v.get("threadId").and_then(|i| i.as_str()),
                    "snippet": v.get("snippet").and_then(|i| i.as_str()),
                    "from": header_value(&v, "From"),
                    "to": header_value(&v, "To"),
                    "subject": header_value(&v, "Subject"),
                    "date": header_value(&v, "Date"),
                    "body": body_text,
                })))
            }
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn gmail_send(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let to = arg_string(&args, "to");
        let subject = arg_string(&args, "subject");
        let body = arg_string(&args, "body");
        if to.is_empty() || subject.is_empty() {
            return ToolResult::ok(err("to and subject are required"));
        }
        let account = arg_string(&args, "account");
        let key = gmail_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Gmail token for {key} — connect Gmail first"))),
        };
        // Build a minimal RFC 2822 message and base64url-encode it.
        let raw = format!(
            "To: {to}\r\nSubject: {subject}\r\n\r\n{body}"
        );
        let encoded = base64url(raw.as_bytes());
        let payload = json!({"raw": encoded});
        let url = format!("{GOOGLE_BASE}/gmail/v1/users/me/messages/send");
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "message_id": v.get("id").and_then(|i| i.as_str()),
                "thread_id": v.get("threadId").and_then(|i| i.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

/// Collect text parts, preferring `text/plain` over `text/html` (multipart
/// alternative bodies usually carry both; the plain part is the readable one).
/// Falls back to the HTML part when no plain part exists.
fn extract_body_text(msg: &Value) -> String {
    let payload = msg.get("payload");
    let mut plain = String::new();
    let mut html = String::new();
    collect_body(payload, &mut plain, &mut html, 0);
    if !plain.is_empty() {
        plain
    } else {
        html
    }
}

fn collect_body(payload: Option<&Value>, plain: &mut String, html: &mut String, depth: usize) {
    if depth > 4 {
        return;
    }
    let payload = match payload {
        Some(p) => p,
        None => return,
    };
    if let Some(parts) = payload.get("parts").and_then(|p| p.as_array()) {
        for part in parts {
            collect_body(Some(part), plain, html, depth + 1);
        }
        return;
    }
    let mime = payload
        .get("mimeType")
        .and_then(|m| m.as_str())
        .unwrap_or("text/plain");
    if !mime.starts_with("text/") {
        return;
    }
    if let Some(data) = payload
        .get("body")
        .and_then(|b| b.get("data"))
        .and_then(|d| d.as_str())
    {
        if let Ok(bytes) = base64_decode(data) {
            let s = String::from_utf8_lossy(&bytes).to_string();
            let target = if mime == "text/plain" { plain } else { html };
            if target.is_empty() {
                *target = s;
            } else {
                target.push_str("\n\n");
                target.push_str(&s);
            }
        }
    }
}

fn header_value(msg: &Value, name: &str) -> Option<String> {
    msg.get("payload")
        .and_then(|p| p.get("headers"))
        .and_then(|h| h.as_array())
        .and_then(|headers| {
            headers.iter().find_map(|h| {
                if h.get("name").and_then(|n| n.as_str()) == Some(name) {
                    h.get("value").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
        })
}

fn base64url(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(s))
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

// ---------------------------------------------------------------------------
// Google Calendar schemas + factories
// ---------------------------------------------------------------------------

fn gcal_list_schema() -> ToolSchema {
    schema(
        "gcal_list_events",
        "List upcoming events from the connected Google Calendar. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "calendar_id": {"type": "string", "description": "Calendar id (default: 'primary')."},
                "max_results": {"type": "integer", "description": "Max events (default 10, max 20)."},
                "account": {"type": "string"}
            },
            "required": []
        }),
    )
}

fn gcal_create_schema() -> ToolSchema {
    schema(
        "gcal_create_event",
        "Create an event on the connected Google Calendar.",
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "description": "Event title."},
                "description": {"type": "string"},
                "start": {"type": "string", "description": "ISO-8601 start, e.g. 2026-08-10T10:00:00."},
                "end": {"type": "string", "description": "ISO-8601 end."},
                "calendar_id": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["summary", "start", "end"]
        }),
    )
}

fn gcal_delete_schema() -> ToolSchema {
    schema(
        "gcal_delete_event",
        "Delete an event from the connected Google Calendar.",
        json!({
            "type": "object",
            "properties": {
                "event_id": {"type": "string"},
                "calendar_id": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["event_id"]
        }),
    )
}

fn gcal_list(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let max = clamp(arg_i64(&args, "max_results"), 10, 20);
        let account = arg_string(&args, "account");
        let key = gcal_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Google Calendar token for {key} — connect Calendar first"))),
        };
        let calendar = if arg_string(&args, "calendar_id").is_empty() {
            "primary"
        } else {
            arg_string(&args, "calendar_id")
        };
        let url = format!(
            "{GCAL_BASE}/calendars/{calendar}/events?maxResults={max}&orderBy=startTime&singleEvents=true"
        );
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => ToolResult::ok(ok(json!({
                "count": v.get("items").and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0),
                "events": v.get("items").cloned().unwrap_or(Value::Array(vec![])),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn gcal_create(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let summary = arg_string(&args, "summary");
        let start = arg_string(&args, "start");
        let end = arg_string(&args, "end");
        if summary.is_empty() || start.is_empty() || end.is_empty() {
            return ToolResult::ok(err("summary, start, and end are required"));
        }
        let account = arg_string(&args, "account");
        let key = gcal_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Google Calendar token for {key} — connect Calendar first"))),
        };
        let calendar = if arg_string(&args, "calendar_id").is_empty() {
            "primary"
        } else {
            arg_string(&args, "calendar_id")
        };
        let mut payload = json!({
            "summary": summary,
            "start": {"dateTime": start},
            "end": {"dateTime": end},
        });
        if let Some(d) = arg_str_opt(&args, "description") {
            payload["description"] = json!(d);
        }
        let url = format!("{GCAL_BASE}/calendars/{calendar}/events");
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "event_id": v.get("id").and_then(|i| i.as_str()),
                "html_link": v.get("htmlLink").and_then(|i| i.as_str()),
                "status": v.get("status").and_then(|i| i.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn gcal_delete(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let event_id = arg_string(&args, "event_id");
        if event_id.is_empty() {
            return ToolResult::ok(err("event_id is required"));
        }
        let account = arg_string(&args, "account");
        let key = gcal_profile_key(account);
        let token = match profile_token(&ctx, &key) {
            Some(t) => t,
            None => return ToolResult::ok(err(format!("no Google Calendar token for {key} — connect Calendar first"))),
        };
        let calendar = if arg_string(&args, "calendar_id").is_empty() {
            "primary"
        } else {
            arg_string(&args, "calendar_id")
        };
        let url = format!("{GCAL_BASE}/calendars/{calendar}/events/{event_id}");
        match request_json("DELETE", &url, &headers(&token), None) {
            Ok(_) => ToolResult::ok(ok(json!({"ok": true, "deleted": event_id}))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

/// Register all Gmail + GCal tools.
pub fn register(ctx: Arc<IntegrationContext>, registry: &mut ocw_engine::ToolRegistry) {
    let low = ToolSpec {
        risk_level: "low",
        category: "google",
        parallel_safe: false,
    };
    let medium = ToolSpec {
        risk_level: "medium",
        category: "google",
        parallel_safe: false,
    };
    registry.register("gmail_search_messages", gmail_search(ctx.clone()), low.clone(), Some(gmail_search_schema()));
    registry.register("gmail_get_message", gmail_get(ctx.clone()), low.clone(), Some(gmail_get_schema()));
    registry.register("gmail_send_email", gmail_send(ctx.clone()), medium.clone(), Some(gmail_send_schema()));
    registry.register("gcal_list_events", gcal_list(ctx.clone()), low.clone(), Some(gcal_list_schema()));
    registry.register("gcal_create_event", gcal_create(ctx.clone()), medium.clone(), Some(gcal_create_schema()));
    registry.register("gcal_delete_event", gcal_delete(ctx.clone()), medium.clone(), Some(gcal_delete_schema()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trip() {
        let raw = b"hello world";
        let enc = base64url(raw);
        assert_eq!(base64_decode(&enc).unwrap(), raw);
    }

    #[test]
    fn urlencode_google() {
        assert_eq!(urlencode("from:alice subject:hello"), "from%3Aalice+subject%3Ahello");
    }

    #[test]
    fn profile_key_shape() {
        assert_eq!(gmail_profile_key(""), "gmail:default");
        assert_eq!(gmail_profile_key("work"), "gmail:work");
        assert_eq!(gcal_profile_key(""), "gcal:default");
    }

    #[test]
    fn extract_body_plain() {
        let msg = json!({
            "payload": {
                "mimeType": "text/plain",
                "body": {"data": base64url(b"hi there")}
            }
        });
        assert_eq!(extract_body_text(&msg), "hi there");
    }

    #[test]
    fn extract_body_multipart() {
        let msg = json!({
            "payload": {
                "mimeType": "multipart/alternative",
                "parts": [
                    {"mimeType": "text/plain", "body": {"data": base64url(b"plain part")}},
                    {"mimeType": "text/html", "body": {"data": base64url(b"<b>html</b>")}}
                ]
            }
        });
        assert_eq!(extract_body_text(&msg), "plain part");
    }

    #[test]
    fn header_lookup() {
        let msg = json!({
            "payload": {
                "headers": [
                    {"name": "From", "value": "a@b.c"},
                    {"name": "Subject", "value": "Hi"}
                ]
            }
        });
        assert_eq!(header_value(&msg, "Subject").as_deref(), Some("Hi"));
        assert_eq!(header_value(&msg, "Missing"), None);
    }
}
