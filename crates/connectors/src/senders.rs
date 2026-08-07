//! Stateless outbound senders — one-shot HTTP POSTs, no SDK, no live connection.
//!
//! Mirrors `coworker/connectors/senders.py`. These power the `send_message` tool
//! (and the super-agent's replies). Both Telegram and Slack outbound are simple
//! HTTP calls, so we use a synchronous `reqwest::blocking` client (configured
//! behind the `sync` feature when invoked) and avoid the heavy SDKs (those are
//! only needed for the inbound listeners).
//!
//! A `Sender` is `(token, chat_id, text, thread_id) -> SendResult`. The registry
//! is swappable so tests inject fakes — no network.
//!
//! The pure senders here are deliberately simple — they don't do channel-name
//! resolution, secret lookup, or attribution. Tool wrappers (see
//! `ocw_tools::messaging`) layer those concerns on top, mirroring how
//! `coworker/connectors/tools.py` builds `make_send_message_tool`.

use crate::base::SendResult;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TIMEOUT: Duration = Duration::from_secs(120);

fn slack_api_base() -> String {
    std::env::var("SLACK_API_URL").unwrap_or_else(|_| "https://slack.com/api/".to_string())
}

/// Strip a managed-relay team prefix (`"T…/C…"` → `"C…"`). Slack's API wants the
/// bare channel id; the team prefix selects the bot token (in the tool wrapper).
pub fn split_slack_chat_id(chat_id: &str) -> (Option<&str>, &str) {
    if let Some((team, channel)) = chat_id.split_once('/') {
        if team.starts_with('T') && !team.is_empty() {
            return (Some(team), channel);
        }
    }
    (None, chat_id)
}

/// Post a Telegram text message via `sendMessage`. Skips
/// `message_thread_id="1"` (Telegram's General forum topic rejects it).
pub fn send_telegram(
    token: &str,
    chat_id: &str,
    text: &str,
    thread_id: Option<&str>,
) -> SendResult {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": text,
    });
    if let Some(tid) = thread_id {
        if tid != "1" {
            if let Ok(n) = tid.parse::<i64>() {
                payload["message_thread_id"] = json!(n);
            }
        }
    }
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    match post_json_blocking(&url, &payload, None) {
        Ok(data) => {
            if data.get("ok").and_then(Value::as_bool) == Some(true) {
                let mid = data
                    .get("result")
                    .and_then(|r| r.get("message_id"))
                    .and_then(|v| v.as_i64())
                    .map(|n| n.to_string());
                SendResult::ok(mid)
            } else {
                let desc = data
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("telegram send failed");
                SendResult::err(desc)
            }
        }
        Err(e) => SendResult::err(e),
    }
}

/// Post a Slack `chat.postMessage`. Bot token is a raw `xoxb-…` (Socket Mode /
/// legacy) or an `xoxe-…` managed-relay token. Errors include the actionable
/// `not_in_channel` hint.
pub fn send_slack(
    token: &str,
    chat_id: &str,
    text: &str,
    thread_id: Option<&str>,
) -> SendResult {
    let (_team, channel) = split_slack_chat_id(chat_id);
    let mut payload = json!({
        "channel": channel,
        "text": text,
    });
    if let Some(tid) = thread_id {
        payload["thread_ts"] = json!(tid);
    }
    let url = format!("{}chat.postMessage", slack_api_base());
    let auth = format!("Bearer {token}");
    match post_json_blocking(&url, &payload, Some(&auth)) {
        Ok(data) => {
            if data.get("ok").and_then(Value::as_bool) == Some(true) {
                SendResult::ok(data.get("ts").and_then(Value::as_str).map(String::from))
            } else {
                let err = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("slack send failed");
                let err = if err == "not_in_channel" {
                    "not_in_channel — invite @OpenWorker to the channel in Slack, then retry"
                } else {
                    err
                };
                SendResult::err(err)
            }
        }
        Err(e) => SendResult::err(e),
    }
}

/// Build a Slack Block Kit message with one section + a row of buttons.
pub fn slack_blocks(text: &str, buttons: &[(&str, &str)]) -> Value {
    let mut elements = Vec::with_capacity(buttons.len());
    for (i, (label, value)) in buttons.iter().enumerate() {
        elements.push(json!({
            "type": "button",
            "text": {"type": "plain_text", "text": label},
            "value": value,
            "action_id": format!("ocw_{i}"),
        }));
    }
    json!([
        {"type": "section", "text": {"type": "mrkdwn", "text": text}},
        {"type": "actions", "elements": elements},
    ])
}

/// Post a Slack interactive message (block with buttons).
pub fn send_slack_interactive(
    token: &str,
    chat_id: &str,
    text: &str,
    buttons: &[(&str, &str)],
    thread_id: Option<&str>,
) -> SendResult {
    let (_team, channel) = split_slack_chat_id(chat_id);
    let mut payload = json!({
        "channel": channel,
        "text": text,
        "blocks": slack_blocks(text, buttons),
    });
    if let Some(tid) = thread_id {
        payload["thread_ts"] = json!(tid);
    }
    let url = format!("{}chat.postMessage", slack_api_base());
    let auth = format!("Bearer {token}");
    match post_json_blocking(&url, &payload, Some(&auth)) {
        Ok(data) => {
            if data.get("ok").and_then(Value::as_bool) == Some(true) {
                SendResult::ok(data.get("ts").and_then(Value::as_str).map(String::from))
            } else {
                SendResult::err(
                    data.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("slack send failed"),
                )
            }
        }
        Err(e) => SendResult::err(e),
    }
}

/// Upload a file to Slack via the `files_upload_v2` flow (reserve URL → PUT
/// bytes → complete into channel/thread). Slack renders its own previews for
/// pdf/csv/images.
pub fn send_slack_file(
    token: &str,
    chat_id: &str,
    thread_id: Option<&str>,
    filename: &str,
    data: &[u8],
    title: Option<&str>,
    comment: Option<&str>,
) -> SendResult {
    let (_team, channel) = split_slack_chat_id(chat_id);
    let auth = format!("Bearer {token}");
    // 1) reserve upload URL
    let reserve_url = format!("{}files.getUploadURLExternal", slack_api_base());
    let reserve_payload = json!({
        "filename": filename,
        "length": data.len(),
    });
    let reserved = match post_json_blocking(&reserve_url, &reserve_payload, Some(&auth)) {
        Ok(v) => v,
        Err(e) => return SendResult::err(e),
    };
    if reserved.get("ok").and_then(Value::as_bool) != Some(true) {
        return SendResult::err(
            reserved
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("slack upload-url failed"),
        );
    }
    let upload_url = match reserved.get("upload_url").and_then(Value::as_str) {
        Some(u) => u.to_string(),
        None => return SendResult::err("slack upload-url missing upload_url"),
    };
    let file_id = match reserved.get("file_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return SendResult::err("slack upload-url missing file_id"),
    };
    // 2) PUT the bytes
    let client = match blocking_client() {
        Ok(c) => c,
        Err(e) => return SendResult::err(e),
    };
    let put_resp = match client
        .post(&upload_url)
        .header("Authorization", &auth)
        .body(data.to_vec())
        .send()
    {
        Ok(r) => r,
        Err(e) => return SendResult::err(e.to_string()),
    };
    if !put_resp.status().is_success() {
        return SendResult::err(format!(
            "slack upload failed ({})",
            put_resp.status()
        ));
    }
    // 3) complete the upload
    let mut complete = json!({
        "files": [{"id": file_id.clone(), "title": title.unwrap_or(filename)}],
        "channel_id": channel,
    });
    if let Some(tid) = thread_id {
        complete["thread_ts"] = json!(tid);
    }
    if let Some(c) = comment {
        complete["initial_comment"] = json!(c);
    }
    let complete_url = format!("{}files.completeUploadExternal", slack_api_base());
    match post_json_blocking(&complete_url, &complete, Some(&auth)) {
        Ok(v) => {
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                SendResult::ok(Some(file_id))
            } else {
                SendResult::err(
                    v.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("slack file send failed"),
                )
            }
        }
        Err(e) => SendResult::err(e),
    }
}

// ---------------------------------------------------------------------------
// Registry types + default senders
// ---------------------------------------------------------------------------

/// One-shot outbound message sender: `(token, chat_id, text, thread_id) -> SendResult`.
pub type Sender = fn(&str, &str, &str, Option<&str>) -> SendResult;

/// One-shot file uploader: `(token, chat_id, thread_id, filename, data, title, comment) -> SendResult`.
pub type FileSender =
    fn(&str, &str, Option<&str>, &str, &[u8], Option<&str>, Option<&str>) -> SendResult;

/// Default sender registry — keyed by platform id.
pub fn default_senders() -> HashMap<&'static str, Sender> {
    let mut map = HashMap::new();
    map.insert("telegram", send_telegram as Sender);
    map.insert("slack", send_slack as Sender);
    map
}

/// Default file-sender registry.
pub fn default_file_senders() -> HashMap<&'static str, FileSender> {
    let mut map = HashMap::new();
    map.insert("slack", send_slack_file as FileSender);
    map
}

// ---------------------------------------------------------------------------
// Tiny blocking HTTP helpers
// ---------------------------------------------------------------------------

fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(FILE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn post_json_blocking(url: &str, body: &Value, auth: Option<&str>) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.post(url).json(body);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    resp.json::<Value>().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_slack_chat_id_team_qualified() {
        let (team, channel) = split_slack_chat_id("T123/C456");
        assert_eq!(team, Some("T123"));
        assert_eq!(channel, "C456");
    }

    #[test]
    fn split_slack_chat_id_bare() {
        let (team, channel) = split_slack_chat_id("C456");
        assert_eq!(team, None);
        assert_eq!(channel, "C456");
    }

    #[test]
    fn split_slack_chat_id_no_team_prefix() {
        // random `/` not at start of a team id
        let (team, channel) = split_slack_chat_id("abc/def");
        assert_eq!(team, None);
        assert_eq!(channel, "abc/def");
    }

    #[test]
    fn slack_blocks_layout() {
        let blocks = slack_blocks("hello", &[("Allow", "allow"), ("Deny", "deny")]);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let elements = arr[1]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["action_id"], "ocw_0");
        assert_eq!(elements[1]["action_id"], "ocw_1");
        assert_eq!(elements[1]["value"], "deny");
    }

    #[test]
    fn default_senders_keys() {
        let s = default_senders();
        assert!(s.contains_key("slack"));
        assert!(s.contains_key("telegram"));
        let fs = default_file_senders();
        assert!(fs.contains_key("slack"));
    }
}
