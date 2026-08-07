//! Stub handlers for subsystems not yet fully ported from Python.
//!
//! Each handler returns a valid JSON response so the GUI doesn't get 404.
//! Real logic will be wired in as each subsystem is implemented.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::cloud;
use crate::connector_accounts;
use crate::state::AppState;
use crate::stores::{inspect_pdf, resolve_channel};

// ---------------------------------------------------------------------------
// Workspaces (5 routes)
// ---------------------------------------------------------------------------

pub async fn handler_workspaces_recent(State(state): State<AppState>) -> Json<Value> {
    let scratch_base = {
        let prefs = state.settings.get_settings().await;
        prefs.get("scratch_base").and_then(|v| v.as_str()).map(|s| {
            let expanded = if s.starts_with('~') {
                dirs::home_dir()
                    .map(|h| s.replacen('~', &h.to_string_lossy(), 1))
                    .unwrap_or_else(|| s.to_string())
            } else {
                s.to_string()
            };
            std::path::PathBuf::from(expanded)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(s))
        })
    };

    let recent = state
        .conversation_store
        .recent_workspaces(20)
        .unwrap_or_default();
    let workspaces: Vec<Value> = recent
        .into_iter()
        .filter_map(|path| {
            let p = std::path::Path::new(&path);
            // Exclude scratch directories (per-conversation dirs shouldn't appear as projects)
            if let Some(ref scratch) = scratch_base {
                if let Ok(resolved) = p.canonicalize() {
                    if resolved.starts_with(scratch) {
                        return None;
                    }
                }
            }
            Some(json!({
                "path": path,
                "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                "exists": p.is_dir(),
            }))
        })
        .collect();
    Json(json!({"workspaces": workspaces}))
}

pub async fn handler_workspaces_trusted(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"workspaces": state.trust_store.list_detailed()}))
}

pub async fn handler_workspaces_open(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let create = body
        .get("create")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if path.is_empty() {
        return Json(json!({"ok": false, "error": "path required"}));
    }

    let expanded = if path.starts_with('~') {
        dirs::home_dir()
            .map(|h| path.replacen('~', &h.to_string_lossy(), 1))
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };
    let p = std::path::Path::new(&expanded);

    if !p.is_dir() {
        if create {
            if let Err(e) = std::fs::create_dir_all(p) {
                return Json(json!({"ok": false, "error": format!("cannot create: {e}")}));
            }
        } else {
            return Json(json!({"ok": false, "error": "directory does not exist"}));
        }
    }

    let resolved = p
        .canonicalize()
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_else(|_| expanded);

    // Touch workspace in the store for recents
    let _ = state.conversation_store.touch_workspace(&resolved);

    Json(json!({
        "ok": true,
        "path": resolved,
        "exists": true,
    }))
}

pub async fn handler_workspaces_pick(State(_state): State<AppState>) -> Json<Value> {
    // Native folder picker not available from server (requires GUI frontend).
    // The Python version opens a native dialog via the local sidecar.
    Json(json!({"ok": false, "error": "folder picker not available — use the GUI"}))
}

pub async fn handler_workspaces_trust(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let trusted = body
        .get("trusted")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if path.is_empty() {
        return Json(json!({"ok": false, "error": "path required"}));
    }
    let canonical = state.trust_store.set_trusted(path, trusted);
    Json(json!({"ok": true, "path": canonical, "trusted": trusted}))
}

// ---------------------------------------------------------------------------
// Personas (7 routes)
// ---------------------------------------------------------------------------

pub async fn handler_personas_list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"personas": state.persona_store.list_all()}))
}

pub async fn handler_persona_get(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
) -> Json<Value> {
    match state.persona_store.get_detail(&persona_id) {
        Some(detail) => Json(detail),
        None => Json(json!({"ok": false, "error": format!("unknown persona: {persona_id}")})),
    }
}

pub async fn handler_persona_update(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let store = &state.persona_store;

    if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
        if let Err(e) = store.set_enabled(&persona_id, enabled) {
            return Json(json!({"ok": false, "error": e}));
        }
        // When disabling, we would archive sessions here (Python does).
        // For now, let the persona enable/disable be a simple flag toggle.
        // The full archive behaviour will be added when conversation store has archive support.
    }
    if let Some(surfaced) = body.get("surfaced").and_then(|v| v.as_bool()) {
        if let Err(e) = store.set_surfaced(&persona_id, surfaced) {
            return Json(json!({"ok": false, "error": e}));
        }
    }
    if body
        .get("default")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        if let Err(e) = store.set_default(&persona_id) {
            return Json(json!({"ok": false, "error": e}));
        }
    }
    Json(json!({"ok": true, "personas": store.list_all(), "archived_sessions": 0}))
}

pub async fn handler_persona_delete(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
) -> Json<Value> {
    match state.persona_store.uninstall(&persona_id) {
        Ok(()) => Json(json!({"ok": true, "personas": state.persona_store.list_all()})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn handler_persona_enable(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match state.persona_store.set_enabled(&persona_id, enabled) {
        Ok(()) => Json(json!({"ok": true, "personas": state.persona_store.list_all()})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn handler_persona_connections(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // §5: set a persona-default connector on/off (mirror of
    // `manager.set_persona_connection`). Seeds the manifest defaults first so
    // the stored row stays complete, then returns the refreshed list.
    let connector = body
        .get("connector")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if connector.is_empty() {
        return Json(json!({"ok": false, "error": "connector required"}));
    }
    if state.persona_store.get(&persona_id).is_none() {
        return Json(json!({"ok": false, "error": format!("unknown persona: {persona_id}")}));
    }
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Ensure the seeded row exists before overlaying the edit.
    let _ = state.persona_defaults(&persona_id);
    state
        .persona_connections
        .set(&persona_id, connector, enabled);
    Json(json!({
        "ok": true,
        "default_connections": state.persona_default_connections(&persona_id),
    }))
}

pub async fn handler_persona_install(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Mirrors `app.py::install_persona`: returns a consent summary per persona;
    // they land disabled pending the user's approval (then POST
    // /v1/personas/{id} {enabled:true, surfaced:true}).
    let result = if let Some(dir) = body.get("dir").and_then(|v| v.as_str()) {
        state
            .persona_store
            .install_from_dir(std::path::Path::new(dir))
    } else if let Some(url) = body.get("git_url").and_then(|v| v.as_str()) {
        // Clone (or reuse the cache) then reuse the dir install path.
        state.persona_store.install_from_git(url)
    } else if let Some(slug) = body.get("gallery_slug").and_then(|v| v.as_str()) {
        install_from_gallery(&state, slug.trim()).await
    } else {
        return Json(json!({"ok": false, "error": "provide a `dir`, `git_url`, or `gallery_slug`"}));
    };
    match result {
        Ok(summaries) => Json(
            json!({"ok": true, "consent": summaries, "personas": state.persona_store.list_all()}),
        ),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

/// Gallery install = fetch the manifest markdown from the cloud (sign-in
/// required), verify its hash, then reuse the exact same parser + consent path
/// as a local/Git install — mirror of `app.py`'s gallery branch. The gallery
/// never changes the trust model: no executable code, lands disabled pending
/// consent.
async fn install_from_gallery(state: &AppState, slug: &str) -> Result<Vec<Value>, String> {
    if slug.is_empty() {
        return Err("gallery_slug required".into());
    }
    let Some(manifest) =
        cloud::gallery_manifest(&state.settings, &state.config, slug).await
    else {
        return Err("gallery requires cloud sign-in (or the cloud is unreachable)".into());
    };
    let markdown = manifest
        .get("manifest_markdown")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if markdown.trim().is_empty() {
        return Err("gallery manifest is empty".into());
    }
    // Verify `manifest_hash` (sha256:<hex>) when the cloud supplies one.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(markdown.as_bytes());
    let digest = format!("sha256:{:x}", hasher.finalize());
    if let Some(expected) = manifest
        .get("manifest_hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        if expected != digest {
            return Err("manifest hash mismatch".into());
        }
    }
    // Write the markdown to a scratch dir and reuse the dir install path.
    let td = std::env::temp_dir().join(format!(
        "ocw-gallery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&td);
    let md_path = td.join(format!("{slug}.md"));
    std::fs::write(&md_path, &markdown).map_err(|e| format!("write manifest: {e}"))?;
    let summaries = state.persona_store.install_from_dir(&td)?;
    let _ = std::fs::remove_dir_all(&td);
    cloud::gallery_install_event(&state.settings, &state.config, slug).await;
    Ok(summaries)
}

// ---------------------------------------------------------------------------
// Cloud (6 routes)
// ---------------------------------------------------------------------------

/// The page shown in the user's browser at the end of a loopback flow (sign-in
/// or connector callback) — one branded card. Inline CSS, light/dark via
/// prefers-color-scheme, no external assets — it must render offline.
fn browser_page(
    title: &str,
    detail: &str,
    ok: bool,
    error: &str,
    connector: &str,
) -> axum::response::Html<String> {
    let _ = connector; // badge omitted for now; the branded card stays
    let icon = if ok { "✓" } else { "✕" };
    let err_html = if error.is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"err\">{}</div>",
            escape_html(error)
        )
    };
    let html = format!(
        "<!doctype html><html><head><meta charset='utf-8'>\
<meta name='viewport' content='width=device-width, initial-scale=1'>\
<title>{title} — OpenWorker</title><style>\
:root{{--paper:#f6f5f2;--panel:#fff;--line:#e4e2dc;--ink:#2c2c2a;--muted:#6f6e68;--ok:#2e7d4f;--ok-soft:#e3f2e9;--bad:#b3423a;--bad-soft:#f8e7e5}}\
@media(prefers-color-scheme:dark){{:root{{--paper:#191918;--panel:#232322;--line:#373633;--ink:#e8e6e1;--ok:#5cb884;--ok-soft:#20362a;--bad:#d97b74;--bad-soft:#3a2422}}}}\
body{{margin:0;min-height:100vh;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:18px;background:var(--paper);color:var(--ink);font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;padding:24px}}\
.card{{background:var(--panel);border:1px solid var(--line);border-radius:16px;padding:34px 32px 28px;max-width:320px;width:100%;text-align:center;box-shadow:0 10px 30px rgba(0,0,0,.06);box-sizing:border-box}}\
.ico{{width:52px;height:52px;border-radius:50%;display:flex;align-items:center;justify-content:center;font-size:24px;font-weight:700;margin:0 auto 16px}}\
.ico.ok{{background:var(--ok-soft);color:var(--ok)}}\
.ico.bad{{background:var(--bad-soft);color:var(--bad)}}\
h1{{font-size:18px;margin:0 0 8px}}\
p{{margin:0 0 6px;color:var(--muted)}}\
.err{{margin-top:12px;padding:8px 10px;border-radius:8px;background:var(--bad-soft);color:var(--bad);font-size:12px;word-break:break-word}}\
</style></head><body><div class=\"card\"><div class=\"ico {}\">{}</div><h1>{}</h1><p>{}</p>{}</div></body></html>",
        if ok { "ok" } else { "bad" },
        icon,
        escape_html(title),
        escape_html(detail),
        err_html,
    );
    axum::response::Html(html)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub async fn handler_cloud_gallery(State(state): State<AppState>) -> Json<Value> {
    match cloud::gallery_list(&state.settings, &state.config).await {
        Some(body) => Json(json!({
            "ok": true,
            "personas": body.get("personas").cloned().unwrap_or(json!([])),
        })),
        None => Json(json!({
            "ok": false,
            "error": "gallery requires cloud sign-in",
            "personas": [],
        })),
    }
}

pub async fn handler_cloud_gallery_item(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Json<Value> {
    match cloud::gallery_detail(&state.settings, &state.config, &slug).await {
        Some(body) => Json(body),
        None => Json(json!({"ok": false, "error": "gallery requires cloud sign-in"})),
    }
}

pub async fn handler_cloud_status(State(state): State<AppState>) -> Json<Value> {
    let st = cloud::status(&state.settings).await;
    let mut out = st.as_object().cloned().unwrap_or_default();
    out.insert(
        "telemetry_enabled".into(),
        json!(cloud::telemetry_enabled(&state.settings).await),
    );
    Json(json!(out))
}

pub async fn handler_cloud_login(State(state): State<AppState>) -> Json<Value> {
    let out = cloud::begin_login(&state.config);
    if let Some(url) = out.get("authorize_url").and_then(|v| v.as_str()) {
        open_browser(url);
    }
    Json(json!({"ok": true, "authorize_url": out.get("authorize_url").cloned().unwrap_or(json!(""))}))
}

pub async fn handler_cloud_logout(State(state): State<AppState>) -> Json<Value> {
    Json(cloud::logout(&state.settings).await)
}

pub async fn handler_cloud_telemetry(
    State(state): State<AppState>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    let enabled = body
        .and_then(|b| {
            b.get("enabled")
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(true);
    Json(cloud::set_telemetry_enabled(&state.settings, enabled).await)
}

// ---------------------------------------------------------------------------
// Inbox (5 routes)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct InboxListQuery {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub inbox: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
}

pub async fn handler_inbox_list(
    State(state): State<AppState>,
    Query(q): Query<InboxListQuery>,
) -> Json<Value> {
    let items = state.inbox_store.list(
        q.session_id.as_deref(),
        q.state.as_deref(),
        q.inbox.as_deref(),
        q.visibility.as_deref(),
    );
    let pending = state.inbox_store.pending(q.session_id.as_deref()).len();
    Json(json!({"items": items, "unread": pending}))
}

pub async fn handler_inbox_resolve(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    // The body shape varies by client ({"resolution":"allow"} | {"value":"..."}).
    // Try to pull a "resolution" string first, fall back to "value"/"answer".
    let resolution = {
        let v = &_body;
        v.get("resolution")
            .or_else(|| v.get("value"))
            .or_else(|| v.get("answer"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let item = state.inbox_store.get(&item_id);
    let ok = state.inbox_store.resolve(&item_id, &resolution);
    // Durable wake: notify the session (and global events bus) so a restart-orphaned
    // approval can continue. Full engine.resume() lands with MCP/tool-call rebuild;
    // broadcasting matches the Python surface contract that something happened.
    if ok {
        if let Some(ref item) = item {
            if item.tool_call_id.is_some() {
                let payload = json!({
                    "type": "inbox_resolved",
                    "data": {
                        "item_id": item_id,
                        "session_id": item.session_id,
                        "resolution": resolution,
                        "tool_call_id": item.tool_call_id,
                        "kind": item.kind,
                    }
                });
                state.broadcast_sync(&item.session_id, payload.clone());
                let _ = state.event_broadcast.send(payload);
            }
        }
    }
    Json(json!({"ok": ok, "id": item_id, "resolution": resolution}))
}

pub async fn handler_inbox_reconcile(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let session_id = q.get("session_id").cloned().unwrap_or_default();
    if session_id.is_empty() {
        return Json(json!({"pending": [], "recap": []}));
    }
    let report = state.inbox_store.reconcile_on_resume(&session_id);
    Json(report)
}

pub async fn handler_inbox_routing(State(state): State<AppState>) -> Json<Value> {
    let bindings = state.inbox_routing.bindings();
    let persona_default = state.inbox_routing.persona_default();
    let session_override = state.inbox_routing.session_override();
    Json(json!({
        "bindings": bindings,
        "persona_default": persona_default,
        "session_override": session_override,
    }))
}

pub async fn handler_inbox_routing_binding(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Accepts either:
    //   {name, channel, target} — set a binding
    //   {session_id, inbox} — set a session override
    //   {persona_id, inbox} — set a persona default
    if let Some(session_id) = body.get("session_id").and_then(|v| v.as_str()) {
        let inbox = body
            .get("inbox")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        state.inbox_routing.set_session_override(session_id, inbox);
        return Json(json!({"ok": true, "kind": "session_override"}));
    }
    if let Some(persona_id) = body.get("persona_id").and_then(|v| v.as_str()) {
        let inbox = body
            .get("inbox")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        state.inbox_routing.set_persona_default(persona_id, inbox);
        return Json(json!({"ok": true, "kind": "persona_default"}));
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let channel = body.get("channel").and_then(|v| v.as_str());
    let target = body.get("target").and_then(|v| v.as_str()).unwrap_or("");
    state.inbox_routing.set_binding(name, channel, target);
    Json(json!({"ok": true, "kind": "binding"}))
}

// ---------------------------------------------------------------------------
// Subscriptions (3 routes)
// ---------------------------------------------------------------------------

pub async fn handler_subscriptions_list(State(state): State<AppState>) -> Json<Value> {
    // Global view-only list: each (session → channel) subscription, enriched
    // with the channel's display name and the channel its Inbox routes OUT to
    // (so an inbound/outbound collision on the same channel is visible).
    let mut out: Vec<Value> = Vec::new();
    for sub in state.subscriptions.all() {
        let channel_name = state.channel_buffer.name_for(&sub.channel);
        let routing_target: Option<String> = state
            .inbox_routing
            .binding_for(&state.inbox_routing.route_for(&sub.session_id, None))
            .channel;
        let collision = routing_target.as_deref() == Some(sub.channel.as_str());
        out.push(json!({
            "session_id": sub.session_id,
            "channel": sub.channel,
            "filter": sub.filter,
            "channel_name": channel_name,
            "routing_target": routing_target,
            "collision": collision,
        }));
    }
    Json(json!({"subscriptions": out}))
}

pub async fn handler_subscriptions_add(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let raw = body
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let addr = resolve_channel(&raw, "slack");
    if session_id.is_empty() || addr.is_empty() || !addr.contains(':') {
        if raw.trim().starts_with('#') {
            return Json(json!({"ok": false, "error": "Channel names can't be looked up — paste the channel ID (channel name ▸ About) or the channel's Copy-link URL."}));
        }
        return Json(json!({"ok": false, "error": "need a session_id and a channel"}));
    }
    state.subscriptions.subscribe(&session_id, &addr, "all");
    Json(json!({"ok": true, "channel": addr}))
}

pub async fn handler_subscriptions_remove(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let addr = resolve_channel(
        body.get("channel").and_then(|v| v.as_str()).unwrap_or(""),
        "slack",
    );
    let removed = state.subscriptions.unsubscribe(&session_id, &addr);
    Json(json!({"ok": true, "removed": removed}))
}

// ---------------------------------------------------------------------------
// Agents (1 route)
// ---------------------------------------------------------------------------

pub async fn handler_agents(State(state): State<AppState>) -> Json<Value> {
    // Mirror Python PersonaRegistry.sidebar(): enabled AND surfaced only,
    // fields name/title (not id/name/description).
    let agents: Vec<Value> = state
        .persona_store
        .list_all()
        .into_iter()
        .filter(|p| {
            p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
                && p.get("surfaced").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .map(|p| {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
            json!({
                "name": id,
                "title": p.get("name").cloned().unwrap_or(json!(id)),
                "needs_workspace": p.get("needs_workspace").cloned().unwrap_or(json!(false)),
                "icon": p.get("icon").cloned().unwrap_or(json!("")),
                "tagline": p.get("tagline").cloned().unwrap_or(json!("")),
                "default": p.get("default").cloned().unwrap_or(json!(false)),
            })
        })
        .collect();
    Json(json!({"agents": agents}))
}

// ---------------------------------------------------------------------------
// Audit (1 route)
// ---------------------------------------------------------------------------

pub async fn handler_audit(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let session_id = params.get("session_id").map(String::as_str);
    let connector = params.get("connector").map(String::as_str);
    let tool = params.get("tool").map(String::as_str);
    let events = state.audit.list(limit, session_id, connector, tool);
    Json(json!({"events": events}))
}

// ---------------------------------------------------------------------------
// Browser (3 routes)
// ---------------------------------------------------------------------------

pub async fn handler_browser_state(State(state): State<AppState>) -> Json<Value> {
    Json(state.browser.state())
}

pub async fn handler_browser_close(State(state): State<AppState>) -> Json<Value> {
    Json(state.browser.close())
}

pub async fn handler_browser_screenshot(State(state): State<AppState>) -> Json<Value> {
    Json(state.browser.screenshot())
}

// ---------------------------------------------------------------------------
// Channels (1 route)
// ---------------------------------------------------------------------------

pub async fn handler_channels_recent(State(state): State<AppState>) -> Json<Value> {
    // The picker's "recently-seen" source: channels the bot has received
    // messages from.
    Json(json!({"channels": state.channel_buffer.channels()}))
}

// ---------------------------------------------------------------------------
// Web search (2 routes)
// ---------------------------------------------------------------------------

const WEB_SEARCH_PROVIDERS: &[&str] = &["duckduckgo", "tavily", "brave"];

pub async fn handler_web_search_get(State(state): State<AppState>) -> Json<Value> {
    let profile = state.settings.secrets_get("web_search:default").await;
    let provider = profile
        .as_ref()
        .and_then(|m| m.get("provider"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&state.config.web_search_provider)
        .to_string();
    let has_key = profile
        .as_ref()
        .and_then(|m| m.get("api_key"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Json(json!({
        "provider": provider,
        "has_key": has_key,
        "providers": WEB_SEARCH_PROVIDERS,
    }))
}

pub async fn handler_web_search_set(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if provider.is_empty() {
        return Json(json!({"ok": false, "error": "provider required"}));
    }
    if !WEB_SEARCH_PROVIDERS.contains(&provider.as_str()) {
        return Json(json!({"ok": false, "error": format!("unknown provider: {provider}")}));
    }
    let mut profile = Map::new();
    profile.insert("provider".into(), json!(provider.clone()));
    if let Some(key) = body.get("api_key").and_then(|v| v.as_str()) {
        if !key.is_empty() {
            profile.insert("api_key".into(), json!(key));
        }
    }
    state.settings.secrets_put("web_search:default", profile).await;
    Json(json!({"ok": true, "provider": provider}))
}

// ---------------------------------------------------------------------------
// Messaging (2 routes)
// ---------------------------------------------------------------------------

pub async fn handler_messaging_dm_route_get(State(state): State<AppState>) -> Json<Value> {
    // The session a DM to the bot is routed to; null → DMs park as unrouted.
    let dm_session = state.settings.get_dm_session().await;
    Json(json!({"dm_session": dm_session}))
}

pub async fn handler_messaging_dm_route_set(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // A falsy session_id clears the designation (DMs then park as unrouted).
    let sid = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    state.settings.set_dm_session(sid.as_deref()).await;
    let dm_session = state.settings.get_dm_session().await;
    Json(json!({"ok": true, "dm_session": dm_session}))
}

// ---------------------------------------------------------------------------
// Unrouted (1 route)
// ---------------------------------------------------------------------------

pub async fn handler_unrouted(State(state): State<AppState>) -> Json<Value> {
    // Dead-letter view: inbound messages with no destination + background-turn
    // failures. Most-recent-first.
    Json(json!({"items": state.unrouted.list(100)}))
}

// ---------------------------------------------------------------------------
// Attachments (1 route)
// ---------------------------------------------------------------------------

pub async fn handler_attachments_inspect_pdf(
    State(_state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Attach-time page/size probe for the composer's threshold check. Local only.
    let data_url = body
        .get("data_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Json(inspect_pdf(data_url))
}

// ---------------------------------------------------------------------------
// Auth / OAuth callbacks (3 routes)
// ---------------------------------------------------------------------------

/// Loopback landing for the Auth0 sign-in flow. Completes the PKCE exchange,
/// then kicks off connection restore in the background so the browser's
/// "Signed in" page isn't held hostage to an extra broker round trip.
pub async fn handler_auth_callback(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Html<String> {
    const DETAIL: &str = "Close this tab and try signing in again from OpenWorker.";
    let code = params.get("code").cloned().unwrap_or_default();
    let state_param = params.get("state").cloned().unwrap_or_default();
    let error = params.get("error").cloned().unwrap_or_default();
    if !error.is_empty() {
        return browser_page("Sign-in failed", DETAIL, false, &error, "");
    }
    let result =
        cloud::complete_login(&state.settings, &state.config, &code, &state_param).await;
    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return browser_page("Sign-in failed", DETAIL, false, &err, "");
    }
    // Restore managed connections in the background (best-effort metadata work).
    let settings = state.settings.clone();
    let config = state.config.clone();
    tokio::spawn(async move {
        let _ = cloud::sync_connections(&settings, &config).await;
    });
    browser_page(
        "Signed in",
        "You're signed in to OpenWorker Cloud. You can close this tab and return to OpenWorker.",
        true,
        "",
        "",
    )
}

fn connector_title(state: &AppState, name: &str) -> String {
    state
        .connector_store
        .get(name)
        .map(|d| d.title.clone())
        .unwrap_or_else(|| {
            let mut chars = name.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
}

/// Loopback landing for the broker's managed-OAuth form-POST. Validates the
/// one-shot app state, then stores the profile in the connector's own layout
/// (multi-account keyed profiles, Slack/GitHub relay installs, or the flat
/// `{connector}:default` for single-token connectors).
pub async fn handler_oauth_callback(
    State(state): State<AppState>,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> axum::response::Html<String> {
    const FAIL_DETAIL: &str =
        "Something went wrong finishing this connection. Close this tab and try again from OpenWorker.";
    let connector = form.get("connector").cloned().unwrap_or_default();
    let app_state = form.get("app_state").cloned().unwrap_or_default();
    if !cloud::consume_managed_state(&app_state) {
        return browser_page(
            "Connection failed",
            FAIL_DETAIL,
            false,
            "unknown or expired connection attempt",
            "",
        );
    }
    if let Some(err) = form.get("error") {
        if !err.is_empty() {
            return browser_page("Connection failed", FAIL_DETAIL, false, err, "");
        }
    }
    let form_map: Map<String, Value> =
        form.into_iter().map(|(k, v)| (k, json!(v))).collect();

    // Managed GitHub deliberately carries NO token fields — the loopback POST is
    // routing metadata only (installation tokens mint on demand, relay spec §4).
    if connector == "github" {
        let result = cloud::managed_connect_install(&state.settings, &form_map).await;
        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return browser_page("Connection failed", FAIL_DETAIL, false, &err, "");
        }
        return browser_page(
            "GitHub connected",
            "You can close this tab and return to OpenWorker.",
            true,
            "",
            "github",
        );
    }
    if connector.is_empty() || form_map.get("access_token").is_none() {
        return browser_page("Connection failed", FAIL_DETAIL, false, "missing fields", "");
    }

    let result: Value = if connector == "slack" && form_map.get("team_id").is_some() {
        // Managed Slack is multi-workspace + relay: per-team bot token, relay mode.
        cloud::managed_connect_slack_install(&state.settings, &form_map).await
    } else if connector == "gmail" {
        let profile = cloud::managed_profile_from_callback(&form_map);
        cloud::managed_connect_account(
            &state.settings,
            connector_accounts::GMAIL_PREFIX,
            connector_accounts::GMAIL_DEFAULT,
            "default_account",
            "account",
            profile,
        )
        .await
    } else if connector == "google_calendar" {
        let profile = cloud::managed_profile_from_callback(&form_map);
        cloud::managed_connect_account(
            &state.settings,
            connector_accounts::GCAL_PREFIX,
            connector_accounts::GCAL_DEFAULT,
            "default_account",
            "account",
            profile,
        )
        .await
    } else if connector == "hubspot" {
        let mut profile = cloud::managed_profile_from_callback(&form_map);
        if let Some(hub_id) = form_map.get("hub_id") {
            profile.insert("hub_id".into(), hub_id.clone());
        }
        if form_map.get("sandbox").is_some() {
            profile.insert("sandbox".into(), json!(true));
        }
        cloud::managed_connect_account(
            &state.settings,
            connector_accounts::HUBSPOT_PREFIX,
            connector_accounts::HUBSPOT_DEFAULT,
            "default_portal",
            "hub_id",
            profile,
        )
        .await
    } else {
        // Generic single-token connector: flat `{connector}:default`, preserving
        // an existing allow-list on reconnect just like the manual path does.
        let mut profile = cloud::managed_profile_from_callback(&form_map);
        let existing = state
            .settings
            .secrets_get(&format!("{connector}:default"))
            .await
            .unwrap_or_default();
        if let Some(allowed) = existing.get("allowed_users").cloned() {
            profile.insert("allowed_users".into(), allowed);
        }
        state
            .settings
            .secrets_put(&format!("{connector}:default"), profile)
            .await;
        json!({"ok": true})
    };

    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return browser_page("Connection failed", FAIL_DETAIL, false, &err, "");
    }
    let title = format!("{} connected", connector_title(&state, &connector));
    browser_page(
        &title,
        "You can close this tab and return to OpenWorker.",
        true,
        "",
        &connector,
    )
}

/// Loopback landing for the MCP OAuth browser flow. The single-slot pending
/// flow arrives with the MCP client implementation (阶段 L3); until then no
/// flow is ever waiting, so stray callbacks get the standard failure page.
pub async fn handler_mcp_oauth_callback(
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Html<String> {
    if let Some(error) = params.get("error") {
        if !error.is_empty() {
            return browser_page(
                "Sign-in failed",
                "The service reported an error. Return to OpenWorker and try again.",
                false,
                error,
                "",
            );
        }
    }
    browser_page(
        "Nothing waiting for this sign-in",
        "The sign-in may have timed out. Return to OpenWorker and start it again.",
        false,
        "",
        "",
    )
}

// ---------------------------------------------------------------------------
// MCP (8 routes)
// ---------------------------------------------------------------------------

pub async fn handler_mcp_list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"servers": state.mcp_store.list()}))
}

pub async fn handler_mcp_create(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Json(json!({"ok": false, "error": "name is required"}));
    }
    let config = body.get("config").cloned().unwrap_or(body.clone());
    match state.mcp_store.create(name, config) {
        Ok(()) => Json(json!({"ok": true, "servers": state.mcp_store.list()})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn handler_mcp_update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    match state.mcp_store.update(&name, body) {
        Ok(()) => Json(json!({"ok": true, "servers": state.mcp_store.list()})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn handler_mcp_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    match state.mcp_store.delete(&name) {
        Ok(()) => {
            state.mcp_runtime.invalidate(&name).await;
            Json(json!({"ok": true, "servers": state.mcp_store.list()}))
        }
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn handler_mcp_tools(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    let Some(server) = state.mcp_store.get(&name) else {
        return Json(json!({"ok": false, "error": format!("server '{name}' not found")}));
    };
    match state.mcp_runtime.tools_for(&server).await {
        Ok(tools) => Json(json!({
            "name": server.name,
            "transport": server.transport,
            "ok": true,
            "tools": tools.iter().map(|t| t.to_json()).collect::<Vec<_>>(),
        })),
        Err(e) => Json(json!({
            "name": server.name,
            "transport": server.transport,
            "ok": false,
            "error": e,
            "tools": [],
        })),
    }
}

pub async fn handler_mcp_connect(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    let Some(server) = state.mcp_store.get(&name) else {
        return Json(json!({"ok": false, "error": format!("server '{name}' not found")}));
    };
    match state.mcp_runtime.connect_and_list(&server).await {
        Ok(tools) => Json(json!({
            "ok": true,
            "name": server.name,
            "connected": true,
            "tools": tools.len(),
        })),
        Err(e) => Json(json!({
            "ok": false,
            "name": server.name,
            "connected": false,
            "error": e,
        })),
    }
}

pub async fn handler_mcp_signout(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    // Mirrors `manager.py::signout_mcp`: forget the stored OAuth tokens (and
    // the DCR-issued client registration) under the `mcp-oauth:{name}` secrets
    // profile so the next connect runs a fresh browser flow.
    if state.mcp_store.get(&name).is_none() {
        return Json(json!({ "ok": false, "error": format!("server '{name}' not found") }));
    }
    let had_tokens = state
        .settings
        .secrets_delete(&format!("mcp-oauth:{name}"))
        .await;
    Json(json!({ "ok": true, "had_tokens": had_tokens }))
}

pub async fn handler_mcp_reload(State(state): State<AppState>) -> Json<Value> {
    state.mcp_store.reload();
    Json(json!({"ok": true, "servers": state.mcp_store.list()}))
}

// ---------------------------------------------------------------------------
// Connectors
// ---------------------------------------------------------------------------

pub async fn handler_connectors_list(State(state): State<AppState>) -> Json<Value> {
    let connectors = state.connector_store.list();
    Json(json!({ "connectors": connectors }))
}

pub async fn handler_connector_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    let descriptor = state.connector_store.get(&name);
    match descriptor {
        Some(d) => Json(json!({
            "name": d.name,
            "title": d.title,
            "auth": d.auth,
            "connected": state.connector_store.is_connected(&d.name),
            "available": d.available,
        })),
        None => Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    }
}

/// GUI-hardcoded path — three honest layers (relay / cloud sign-in / teams).
pub async fn handler_slack_status(State(state): State<AppState>) -> Json<Value> {
    let default = state
        .settings
        .secrets_get("slack:default")
        .await
        .unwrap_or_default();
    let mode = default
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let signin = cloud::status(&state.settings).await;
    let signed_in = signin
        .get("signed_in")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Gateway/relay adapter not yet ported — report offline honestly.
    Json(json!({
        "ok": true,
        "mode": mode,
        "relay": {
            "state": "offline",
            "reconnects": 0,
            "last_event_at": Value::Null,
            "last_error": "",
        },
        "signed_in": signed_in,
        "teams": {},
    }))
}

/// GUI-hardcoded path — relay / cloud / installs.
pub async fn handler_github_status(State(state): State<AppState>) -> Json<Value> {
    let default = state
        .settings
        .secrets_get("github:default")
        .await
        .unwrap_or_default();
    let mode = default
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let signin = cloud::status(&state.settings).await;
    let signed_in = signin
        .get("signed_in")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Enumerate local install profiles for the GUI list.
    let mut installs = Map::new();
    let all = state.settings.secrets_all().await;
    for (key, profile) in all {
        if let Some(id) = key.strip_prefix("github:install:") {
            installs.insert(
                id.to_string(),
                json!({
                    "installation_id": id,
                    "account": profile.get("account").cloned().unwrap_or(Value::Null),
                    "account_type": profile.get("account_type").cloned().unwrap_or(Value::Null),
                }),
            );
        }
    }
    Json(json!({
        "ok": true,
        "mode": mode,
        "relay": {
            "state": "offline",
            "reconnects": 0,
            "last_event_at": Value::Null,
            "last_error": "",
        },
        "signed_in": signed_in,
        "installs": installs,
        "missed": {},
    }))
}

pub async fn handler_github_installation_disconnect(
    State(state): State<AppState>,
    Path(installation_id): Path<String>,
) -> Json<Value> {
    let installation_id = installation_id.trim().to_string();
    let key = format!("github:install:{installation_id}");
    if installation_id.is_empty() || state.settings.secrets_get(&key).await.is_none() {
        return Json(json!({"ok": false, "error": "installation not connected"}));
    }
    cloud::github_disconnect_installation(&state.settings, &state.config, &installation_id).await;
    state.settings.secrets_delete(&key).await;
    // Recount remaining installs.
    let remaining = state
        .settings
        .secrets_all()
        .await
        .keys()
        .filter(|k| k.starts_with("github:install:"))
        .count();
    if remaining == 0 {
        if let Some(mut default) = state.settings.secrets_get("github:default").await {
            if default.get("mode").and_then(|v| v.as_str()) == Some("relay") {
                default.remove("mode");
                if default.is_empty() {
                    state.settings.secrets_delete("github:default").await;
                } else {
                    state.settings.secrets_put("github:default", default).await;
                }
            }
        }
    }
    Json(json!({"ok": true, "remaining_installs": remaining}))
}

/// Validate stored creds with a live API call; returns the account identity
/// (e.g. "Acme Corp / openworker-bot") or an error string. Mirrors the
/// per-connector `_validate_*` helpers in `coworker/connectors/descriptors.py`.
fn validate_connector(name: &str, creds: &Map<String, Value>) -> Result<String, String> {
    let get = |k: &str| {
        creds
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let client = || {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())
    };
    match name {
        "slack" => {
            let token = get("bot_token");
            let data: Value = client()?
                .post("https://slack.com/api/auth.test")
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .map_err(|e| e.to_string())?
                .json()
                .map_err(|e| e.to_string())?;
            if data.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                let team = data.get("team").and_then(|v| v.as_str()).unwrap_or("?");
                let user = data.get("user").and_then(|v| v.as_str()).unwrap_or("bot");
                Ok(format!("{team} / {user}"))
            } else {
                Err(data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("invalid bot token")
                    .to_string())
            }
        }
        "telegram" => {
            let token = get("bot_token");
            let data: Value = client()?
                .get(format!("https://api.telegram.org/bot{token}/getMe"))
                .send()
                .map_err(|e| e.to_string())?
                .json()
                .map_err(|e| e.to_string())?;
            if data.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                let username = data
                    .get("result")
                    .and_then(|r| r.get("username"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("bot");
                Ok(format!("@{username}"))
            } else {
                Err(data
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("invalid bot token")
                    .to_string())
            }
        }
        "github" => {
            let token = get("token");
            let data: Value = client()?
                .get(format!(
                    "{}/user",
                    std::env::var("GITHUB_API_URL")
                        .unwrap_or_else(|_| "https://api.github.com".to_string())
                ))
                .header("Authorization", format!("Bearer {token}"))
                .header("User-Agent", "openworker")
                .send()
                .map_err(|e| e.to_string())?
                .json()
                .map_err(|e| e.to_string())?;
            data.get("login")
                .and_then(|v| v.as_str())
                .map(|l| format!("@{l}"))
                .ok_or_else(|| {
                    data.get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("invalid token")
                        .to_string()
                })
        }
        "hubspot" => {
            let token = if !get("token").is_empty() {
                get("token")
            } else {
                get("access_token")
            };
            let data: Value = client()?
                .get("https://api.hubapi.com/account-info/v3/details")
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .map_err(|e| e.to_string())?
                .json()
                .map_err(|e| e.to_string())?;
            data.get("portalId")
                .and_then(|v| v.as_i64())
                .map(|id| format!("portal {id}"))
                .ok_or_else(|| {
                    data.get("message")
                        .or_else(|| data.get("error"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("invalid token")
                        .to_string()
                })
        }
        _ => Ok(String::new()),
    }
}

fn profile_type_for(auth: &str) -> &'static str {
    match auth {
        "oauth" => "oauth",
        "none" => "none",
        _ => "token",
    }
}

pub async fn handler_connector_connect(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let descriptor = match state.connector_store.get(&name) {
        Some(d) => d.clone(),
        None => return Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    };
    if !descriptor.available {
        return Json(json!({"ok": false, "error": "unknown or unavailable connector"}));
    }
    if descriptor.experimental {
        if !state.settings.experimental_connectors_enabled().await {
            return Json(json!({"ok": false, "error": "experimental connectors are disabled"}));
        }
        let acknowledged = body
            .get("acknowledge_risk")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !acknowledged {
            return Json(json!({
                "ok": false,
                "error": "risk acknowledgment required",
                "risk_notice": descriptor.risk_notice,
            }));
        }
    }

    let fields = body
        .get("fields")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    // Reconnect-safe: a blank — or mask-equal — secret submission means "keep
    // what's stored", never "overwrite with the mask".
    let existing = state
        .settings
        .secrets_get(&format!("{name}:default"))
        .await
        .unwrap_or_default();
    let mut resolved: Map<String, Value> = Map::new();
    for f in &descriptor.fields {
        let v = fields
            .get(&f.key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if f.key == "allowed_users" {
            resolved.insert(f.key.clone(), Value::String(v));
            continue;
        }
        if v.is_empty() || (f.secret && v == f.placeholder.trim()) {
            if let Some(ev) = existing.get(&f.key) {
                resolved.insert(f.key.clone(), ev.clone());
            }
        } else {
            resolved.insert(f.key.clone(), Value::String(v));
        }
    }

    let missing: Vec<String> = descriptor
        .fields
        .iter()
        .filter(|f| {
            f.required
                && !resolved
                    .get(&f.key)
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
        })
        .map(|f| f.label.clone())
        .collect();
    if !missing.is_empty() {
        return Json(json!({"ok": false, "error": format!("missing: {}", missing.join(", "))}));
    }

    let mut allowed: Vec<String> = resolved
        .get("allowed_users")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default();
    allowed.sort();
    allowed.dedup();
    if allowed.is_empty() {
        if let Some(au) = existing.get("allowed_users").and_then(|v| v.as_array()) {
            allowed = au
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }

    let mut creds: Map<String, Value> = resolved
        .into_iter()
        .filter(|(k, _)| k != "allowed_users")
        .collect();
    // Live validation (blocking HTTP — handlers run on the tokio worker pool).
    let mut identity: Option<String> = None;
    if !creds.is_empty() {
        match validate_connector(&name, &creds) {
            Ok(id) => {
                if !id.is_empty() {
                    identity = Some(id);
                }
            }
            Err(e) => return Json(json!({"ok": false, "error": e})),
        }
    }

    let mut profile: Map<String, Value> = Map::new();
    profile.insert("type".into(), json!(profile_type_for(&descriptor.auth)));
    profile.insert("enabled".into(), Value::Bool(true));
    profile.append(&mut creds);
    if descriptor.fields.iter().any(|f| f.key == "allowed_users") {
        profile.insert("allowed_users".into(), json!(allowed));
    }
    if name == "slack" {
        // Re-pasting manual Socket Mode tokens must not erase locally selected
        // approval owners.
        if let Some(ao) = existing.get("approval_owner_ids") {
            profile.insert("approval_owner_ids".into(), ao.clone());
        }
    }
    if let Some(id) = &identity {
        profile.insert("account".into(), json!(id));
    }
    state
        .settings
        .secrets_put(&format!("{name}:default"), profile)
        .await;
    state.connector_store.set_connected(&name, true);
    Json(json!({"ok": true, "account": identity}))
}

pub async fn handler_connector_disconnect(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    match state.connector_store.get(&name) {
        Some(_) => {
            // Whole-connector disconnect drops every per-account profile too.
            match name.as_str() {
                "gmail" => {
                    let accounts = connector_accounts::list_accounts(&state.settings, "gmail").await;
                    for (id, _) in accounts {
                        state
                            .settings
                            .secrets_delete(&format!("gmail:account:{id}"))
                            .await;
                    }
                }
                "google-calendar" => {
                    let accounts =
                        connector_accounts::list_accounts(&state.settings, "google-calendar").await;
                    for (id, _) in accounts {
                        state
                            .settings
                            .secrets_delete(&format!("google-calendar:account:{id}"))
                            .await;
                    }
                }
                "hubspot" => {
                    let accounts = connector_accounts::list_accounts(&state.settings, "hubspot").await;
                    for (id, _) in accounts {
                        state
                            .settings
                            .secrets_delete(&format!("hubspot:account:{id}"))
                            .await;
                    }
                }
                "github" => {
                    let accounts = connector_accounts::list_accounts(&state.settings, "github").await;
                    for (id, _) in accounts {
                        state
                            .settings
                            .secrets_delete(&format!("github:account:{id}"))
                            .await;
                    }
                }
                _ => {}
            }
            ocw_connectors::clear_slack_directory_cache(Some(&name));
            state.connector_store.set_connected(&name, false);
            state.settings.secrets_delete(&format!("{name}:default")).await;
            Json(json!({"ok": true}))
        }
        None => Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Whether the local store holds a cloud sign-in session.
async fn cloud_signed_in(settings: &crate::state::SettingsManager) -> bool {
    settings
        .secrets_get(crate::cloud::CLOUD_AUTH_PROFILE)
        .await
        .map(|m| {
            m.get("access_token")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

pub async fn handler_connector_connect_managed(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let descriptor = match state.connector_store.get(&name) {
        Some(d) => d.clone(),
        None => return Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    };
    if !cloud_signed_in(&state.settings).await {
        return Json(json!({
            "ok": false,
            "error": "Managed connect requires signing in to OpenWorker Cloud first.",
        }));
    }
    let access = body
        .get("access")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let flow = body
        .get("flow")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let out = cloud::begin_managed_connect(&state.settings, &state.config, &name, &access, &flow)
        .await;
    if out.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(url) = out.get("authorize_url").and_then(|v| v.as_str()) {
            open_browser(url);
        }
    }
    let mut resp = out;
    if let Some(o) = resp.as_object_mut() {
        o.insert("connector_title".into(), json!(descriptor.title));
    }
    Json(resp)
}

pub async fn handler_connector_mcp_connect(
    State(_state): State<AppState>,
    Path(name): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    // OAuth browser flow for MCP-backed connectors is not ported yet.
    // Never return started:true — that was a fake-success stub.
    let known_mcp = matches!(name.as_str(), "jira" | "monday" | "asana" | "linear" | "notion");
    if !known_mcp {
        return Json(json!({"ok": false, "error": format!("{name} has no MCP connect path")}));
    }
    Json(json!({
        "ok": false,
        "error": "OAuth MCP connect is not yet available in the Rust server",
    }))
}

pub async fn handler_connector_tools_patch(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Mirrors `tool_defs.patch_tool_settings`: persists per-connector tool
    // enable/disable overrides under `{connector}:tools`.
    let enabled = body.get("enabled").and_then(|v| v.as_object());
    let enabled = match enabled {
        Some(e) => e.clone(),
        None => return Json(json!({"ok": false, "error": "enabled map required"})),
    };
    let mut current = state
        .settings
        .secrets_get(&format!("{name}:tools"))
        .await
        .and_then(|m| m.get("enabled").cloned())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in enabled {
        current.insert(k, json!(v.as_bool().unwrap_or(false)));
    }
    let mut wrapper = Map::new();
    wrapper.insert("enabled".into(), json!(current));
    state.settings.secrets_put(&format!("{name}:tools"), wrapper).await;
    Json(json!({"ok": true, "tools": current}))
}

pub async fn handler_connector_account_disconnect(
    State(state): State<AppState>,
    Path((name, account_id)): Path<(String, String)>,
) -> Json<Value> {
    if !connector_accounts::is_account_connector(&name) {
        return Json(json!({"ok": false, "error": "not a multi-account connector"}));
    }
    let r = connector_accounts::disconnect_account(
        &state.settings,
        &name,
        &account_id,
        "default_account",
        connector_accounts::keep_none,
    )
    .await;
    Json(r)
}

pub async fn handler_connector_account_default(
    State(state): State<AppState>,
    Path((name, account_id)): Path<(String, String)>,
) -> Json<Value> {
    if !connector_accounts::is_account_connector(&name) {
        return Json(json!({"ok": false, "error": "not a multi-account connector"}));
    }
    let r = connector_accounts::set_default(&state.settings, &name, &account_id, "default_account").await;
    Json(r)
}

pub async fn handler_connector_allow(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    match state.connector_store.get(&name) {
        Some(_) => {
            let user_id = body.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            if user_id.is_empty() {
                return Json(json!({"ok": false, "error": "user_id required"}));
            }
            state.connector_store.set_disallowed(&name, false);
            allow_user_in_profile(&state, &name, user_id).await;
            Json(json!({"ok": true}))
        }
        None => Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    }
}

pub async fn handler_connector_disallow(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    match state.connector_store.get(&name) {
        Some(_) => {
            state.connector_store.set_disallowed(&name, true);
            Json(json!({"ok": true}))
        }
        None => Json(json!({"ok": false, "error": format!("unknown connector: {name}")})),
    }
}

/// Add `user_id` to the connector's stored allow-list (flat `allowed_users`).
async fn allow_user_in_profile(state: &AppState, name: &str, user_id: &str) {
    let key = format!("{name}:default");
    let mut profile = state.settings.secrets_get(&key).await.unwrap_or_default();
    let mut allowed: Vec<String> = profile
        .get("allowed_users")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if !allowed.iter().any(|u| u == user_id) {
        allowed.push(user_id.to_string());
        allowed.sort();
    }
    profile.insert("allowed_users".into(), json!(allowed));
    state.settings.secrets_put(&key, profile).await;
}

pub async fn handler_connector_unauthorized(
    State(state): State<AppState>,
    Path((name, item_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Resolve a parked unauthorized message: dismiss / allow / allow_deliver.
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let item = state.parked_messages.write().unwrap().remove(&item_id);
    let item = match item {
        Some(i) => i,
        None => return Json(json!({"ok": false, "error": "unknown item"})),
    };
    if item.get("platform").and_then(|v| v.as_str()) != Some(name.as_str()) {
        return Json(json!({"ok": false, "error": "unknown item"}));
    }
    if action == "dismiss" {
        return Json(json!({"ok": true}));
    }
    if action != "allow" && action != "allow_deliver" {
        return Json(json!({"ok": false, "error": format!("unknown action: {action}")}));
    }
    let user_id = item.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    if user_id.is_empty() {
        return Json(json!({"ok": false, "error": "item has no user_id"}));
    }
    allow_user_in_profile(&state, &name, user_id).await;
    // allow_deliver would re-inject through the inbound path once the gateway
    // is wired; for now the allow-list update is the durable effect.
    Json(json!({"ok": true}))
}

pub async fn handler_slack_workspace_disconnect(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
) -> Json<Value> {
    // Stop relaying ONE workspace: drop the local per-team token; removing the
    // last workspace clears relay mode on slack:default (manual Socket Mode
    // fields, if any, are left untouched but disabled).
    let team_id = team_id.trim().to_string();
    let profile_key = format!("slack:team:{team_id}");
    if team_id.is_empty() || state.settings.secrets_get(&profile_key).await.is_none() {
        return Json(json!({"ok": false, "error": "workspace not connected"}));
    }
    state.settings.secrets_delete(&profile_key).await;
    ocw_connectors::clear_slack_directory_cache(Some(&team_id));
    let remaining: Vec<String> = state
        .settings
        .secrets_all()
        .await
        .keys()
        .filter(|k| k.starts_with("slack:team:"))
        .cloned()
        .collect();
    if remaining.is_empty() {
        let mut default = state
            .settings
            .secrets_get("slack:default")
            .await
            .unwrap_or_default();
        let mode = default.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        if mode == "relay" {
            default.remove("mode");
            default.remove("managed");
            if default.get("bot_token").is_some() {
                // Manual Socket Mode creds predating the relay switch: keep them
                // stored but DISABLED — never silently start listening.
                default.insert("type".into(), json!("token"));
                default.insert("enabled".into(), Value::Bool(false));
                state.settings.secrets_put("slack:default", default).await;
            } else {
                default.remove("type");
                default.remove("enabled");
                if default.is_empty() {
                    state.settings.secrets_delete("slack:default").await;
                } else {
                    state.settings.secrets_put("slack:default", default).await;
                }
            }
        }
    }
    state.connector_store.set_connected("slack", !remaining.is_empty());
    Json(json!({"ok": true, "remaining_workspaces": remaining.len()}))
}

/// The workspace's bot token: per-team profile (managed relay) or the flat
/// default profile (manual Socket Mode — team_id "default").
async fn slack_bot_token(state: &AppState, team_id: &str) -> String {
    if !team_id.is_empty() && team_id != "default" {
        if let Some(p) = state.settings.secrets_get(&format!("slack:team:{team_id}")).await {
            if let Some(t) = p.get("bot_token").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
    }
    match state.settings.secrets_get("slack:default").await {
        Some(p) => p
            .get("bot_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

pub async fn handler_slack_directory(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let q = query.get("q").cloned().unwrap_or_default();
    let limit = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(25);
    let refresh = query.get("refresh").map(|v| v == "true" || v == "1").unwrap_or(false);
    let token = slack_bot_token(&state, &team_id).await;
    Json(ocw_connectors::list_members(&token, &team_id, &q, limit, refresh))
}

pub async fn handler_slack_channels(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let q = query.get("q").cloned().unwrap_or_default();
    let limit = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(25);
    let refresh = query.get("refresh").map(|v| v == "true" || v == "1").unwrap_or(false);
    let token = slack_bot_token(&state, &team_id).await;
    Json(ocw_connectors::list_channels(&token, &team_id, &q, limit, refresh))
}

async fn set_slack_approval_owner(
    state: &AppState,
    user_id: &str,
    add: bool,
    display_name: &str,
) -> Json<Value> {
    let user_id = user_id.trim().to_string();
    if user_id.is_empty() {
        return Json(json!({"ok": false, "error": "user_id required"}));
    }
    let mut profile = match state.settings.secrets_get("slack:default").await {
        Some(p) => p,
        None => {
            return Json(json!({
                "ok": false,
                "error": "Slack is not connected in Manual mode."
            }))
        }
    };
    if profile.get("mode").and_then(|v| v.as_str()) == Some("relay") || profile.get("managed").is_some() {
        return Json(json!({
            "ok": false,
            "error": "Relay approval ownership is set by the Slack installer."
        }));
    }
    let mut owners: Vec<String> = profile
        .get("approval_owner_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if add {
        if !owners.iter().any(|o| o == &user_id) {
            owners.push(user_id.clone());
        }
        let mut allowed: Vec<String> = profile
            .get("allowed_users")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !allowed.iter().any(|u| u == &user_id) {
            allowed.push(user_id.clone());
            allowed.sort();
            profile.insert("allowed_users".into(), json!(allowed));
        }
    } else {
        owners.retain(|o| o != &user_id);
    }
    owners.sort();
    owners.dedup();
    profile.insert("approval_owner_ids".into(), json!(owners));
    state.settings.secrets_put("slack:default", profile).await;
    let _ = display_name;
    Json(json!({"ok": true, "approval_owner_ids": owners}))
}

pub async fn handler_slack_approval_owners_add(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let user_id = body.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    set_slack_approval_owner(&state, user_id, true, name).await
}

pub async fn handler_slack_approval_owners_remove(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let user_id = body.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    set_slack_approval_owner(&state, user_id, false, "").await
}

pub async fn handler_gmail_account_disconnect(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::gmail_disconnect(&state.settings, &email).await;
    Json(r)
}

pub async fn handler_gmail_account_default(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::gmail_set_default(&state.settings, &email).await;
    Json(r)
}

pub async fn handler_gmail_filters(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Replace the "Never show agents" lists.
    let senders = body.get("senders").cloned();
    let labels = body.get("labels").cloned();
    if let Some(s) = &senders {
        if !s.is_array() {
            return Json(json!({"ok": false, "error": "senders must be a list"}));
        }
    }
    if let Some(l) = &labels {
        if !l.is_array() {
            return Json(json!({"ok": false, "error": "labels must be a list"}));
        }
    }
    let senders = senders.map(|s| s.as_array().cloned().unwrap_or_default());
    let labels = labels.map(|l| l.as_array().cloned().unwrap_or_default());
    let r = connector_accounts::gmail_set_filters(&state.settings, senders, labels).await;
    Json(r)
}

pub async fn handler_gcal_account_disconnect(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::gcal_disconnect(&state.settings, &email).await;
    Json(r)
}

pub async fn handler_gcal_account_default(
    State(state): State<AppState>,
    Path(email): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::gcal_set_default(&state.settings, &email).await;
    Json(r)
}

pub async fn handler_hubspot_portal_disconnect(
    State(state): State<AppState>,
    Path(hub_id): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::hubspot_disconnect(&state.settings, &hub_id).await;
    Json(r)
}

pub async fn handler_hubspot_portal_default(
    State(state): State<AppState>,
    Path(hub_id): Path<String>,
) -> Json<Value> {
    let r = connector_accounts::hubspot_set_default(&state.settings, &hub_id).await;
    Json(r)
}

pub async fn handler_hubspot_hidden_fields(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let fields = body.get("hidden_fields").cloned();
    let fields = match fields {
        Some(f) if f.is_array() => f.as_array().cloned().unwrap_or_default(),
        _ => return Json(json!({"ok": false, "error": "hidden_fields must be a list"})),
    };
    let r = connector_accounts::hubspot_set_hidden_fields(&state.settings, fields).await;
    Json(r)
}
