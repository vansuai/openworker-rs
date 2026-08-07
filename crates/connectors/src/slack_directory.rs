//! Slack workspace rosters for the pickers (people + channels).
//!
//! Mirrors `coworker/connectors/slack_directory.py`: cursor-paginated sweeps of
//! `users.list` / `conversations.list`, filtered locally, cached in-process for
//! 15 minutes. Pure reads on scopes every install already granted — names/ids
//! are routing metadata, never persisted.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

const TTL: std::time::Duration = std::time::Duration::from_secs(900);
const PAGE_LIMIT: u32 = 200;
const CHANNEL_PAGE_LIMIT: u32 = 999;
const MAX_PAGES: usize = 25;

/// (team_id, kind) -> (fetched_at, rows). Process-local on purpose: nothing
/// roster-shaped is persisted.
type RosterCache = HashMap<(String, String), (Instant, Vec<Value>)>;
static CACHE: Mutex<Option<RosterCache>> = Mutex::new(None);

fn api_base() -> String {
    std::env::var("SLACK_API_URL").unwrap_or_else(|_| "https://slack.com/api/".to_string())
}

/// Cursor-paginated GET of one Slack Web API method.
fn get_pages(
    token: &str,
    method: &str,
    params: &[(&str, &str)],
    key: &str,
    page_limit: u32,
) -> Result<Vec<Value>, String> {
    let client = reqwest::blocking::Client::new();
    let mut rows = Vec::new();
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let mut q: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        q.push(("limit".into(), page_limit.to_string()));
        if !cursor.is_empty() {
            q.push(("cursor".into(), cursor.clone()));
        }
        let resp = client
            .get(format!("{}{method}", api_base()))
            .query(&q)
            .header("Authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .map_err(|e| e.to_string())?;
        let data: Value = resp.json().map_err(|e| e.to_string())?;
        if data.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = data
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("request failed");
            return Err(format!("{method}: {err}"));
        }
        if let Some(page) = data.get(key).and_then(|v| v.as_array()) {
            rows.extend(page.clone());
        }
        cursor = data
            .get("response_metadata")
            .and_then(|m| m.get("next_cursor"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if cursor.is_empty() {
            break;
        }
    }
    Ok(rows)
}

fn cached(
    team_id: &str,
    kind: &str,
    fetch: impl FnOnce() -> Result<Vec<Value>, String>,
    refresh: bool,
) -> Result<Vec<Value>, String> {
    let now = Instant::now();
    {
        let cache = CACHE.lock().unwrap();
        if !refresh {
            if let Some(map) = cache.as_ref() {
                if let Some((fetched, rows)) = map.get(&(team_id.to_string(), kind.to_string()))
                {
                    if now.duration_since(*fetched) < TTL {
                        return Ok(rows.clone());
                    }
                }
            }
        }
    }
    let rows = fetch()?;
    let mut cache = CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    map.insert((team_id.to_string(), kind.to_string()), (now, rows.clone()));
    Ok(rows)
}

/// Case-insensitive substring filter; prefix matches first, then alpha.
fn rank(rows: Vec<Value>, query: &str, key: &str, limit: i64) -> Vec<Value> {
    let q = query.trim().to_lowercase();
    let mut filtered: Vec<Value> = if q.is_empty() {
        rows
    } else {
        rows.into_iter()
            .filter(|r| {
                let name = r.get(key).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let handle = r.get("handle").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                name.contains(&q) || handle.contains(&q)
            })
            .collect()
    };
    filtered.sort_by(|a, b| {
        let an = a.get(key).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let bn = b.get(key).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let ap = an.starts_with(&q);
        let bp = bn.starts_with(&q);
        bp.cmp(&ap).then_with(|| an.cmp(&bn))
    });
    let limit = limit.clamp(1, 100) as usize;
    filtered.truncate(limit);
    filtered
}

/// Human members of the workspace: id, display name, @handle, guest flag.
/// Bots, deleted users, and Slackbot are filtered.
pub fn list_members(token: &str, team_id: &str, query: &str, limit: i64, refresh: bool) -> Value {
    if token.is_empty() {
        return json!({"ok": false, "error": "workspace not connected"});
    }
    let rows = cached(team_id, "members", || {
        let members = get_pages(token, "users.list", &[], "members", PAGE_LIMIT)?;
        let mut out = Vec::new();
        for m in members {
            if m.get("deleted").and_then(|v| v.as_bool()) == Some(true)
                || m.get("is_bot").and_then(|v| v.as_bool()) == Some(true)
                || m.get("id").and_then(|v| v.as_str()) == Some("USLACKBOT")
            {
                continue;
            }
            let profile = m.get("profile").cloned().unwrap_or_default();
            let name = profile
                .get("display_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| profile.get("real_name").and_then(|v| v.as_str()))
                .or_else(|| m.get("name").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            out.push(json!({
                "id": m.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "name": name,
                "handle": m.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "guest": m.get("is_restricted").and_then(|v| v.as_bool()).unwrap_or(false)
                    || m.get("is_ultra_restricted").and_then(|v| v.as_bool()).unwrap_or(false),
            }));
        }
        Ok(out)
    }, refresh);
    match rows {
        Ok(rows) => json!({"ok": true, "members": rank(rows, query, "name", limit)}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// Channels the token can see: all public ones, private only where the bot
/// is a member. `is_member` lets the GUI hint "invite @OpenWorker" for the rest.
pub fn list_channels(token: &str, team_id: &str, query: &str, limit: i64, refresh: bool) -> Value {
    if token.is_empty() {
        return json!({"ok": false, "error": "workspace not connected"});
    }
    let rows = cached(team_id, "channels", || {
        let chans = get_pages(
            token,
            "conversations.list",
            &[
                ("types", "public_channel,private_channel"),
                ("exclude_archived", "true"),
            ],
            "channels",
            CHANNEL_PAGE_LIMIT,
        )?;
        let mut out = Vec::new();
        for c in chans {
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() || name.is_empty() {
                continue;
            }
            out.push(json!({
                "id": id,
                "name": name,
                "is_private": c.get("is_private").and_then(|v| v.as_bool()).unwrap_or(false),
                "is_member": c.get("is_member").and_then(|v| v.as_bool()).unwrap_or(false),
            }));
        }
        Ok(out)
    }, refresh);
    match rows {
        Ok(rows) => json!({"ok": true, "channels": rank(rows, query, "name", limit)}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// Drop cached rosters (all teams, or one) — disconnect/reconnect hygiene.
pub fn clear_cache(team_id: Option<&str>) {
    let mut cache = CACHE.lock().unwrap();
    let map = match cache.as_mut() {
        Some(m) => m,
        None => return,
    };
    if let Some(team_id) = team_id {
        let keys: Vec<_> = map
            .keys()
            .filter(|(t, _)| t == team_id)
            .cloned()
            .collect();
        for k in keys {
            map.remove(&k);
        }
    } else {
        map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_prefix_first_then_alpha() {
        let rows = vec![
            json!({"name": "bob", "handle": "bob"}),
            json!({"name": "alice", "handle": "alice"}),
            json!({"name": "alex", "handle": "alex"}),
        ];
        let ranked = rank(rows, "al", "name", 25);
        // both start with "al" -> alpha order wins; "bob" doesn't match
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["name"], "alex");
        assert_eq!(ranked[1]["name"], "alice");
    }

    #[test]
    fn rank_filters_and_limits() {
        let rows = vec![
            json!({"name": "xavier", "handle": "x"}),
            json!({"name": "yolanda", "handle": "y"}),
        ];
        let ranked = rank(rows, "x", "name", 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0]["name"], "xavier");
    }

    #[test]
    fn no_token_is_error() {
        let v = list_members("", "T1", "", 25, false);
        assert_eq!(v["ok"], false);
    }
}
