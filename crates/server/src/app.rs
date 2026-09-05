//! Axum router — REST API + WebSocket.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::automations::{
    handler_create, handler_delete, handler_finalize, handler_get, handler_list, handler_mark_seen,
    handler_run, handler_update,
};
use crate::error::Error;
use crate::settings;
use crate::state::AppState;
use crate::subsystems;
use crate::teams;
use crate::ws::ws_session_handler;

// ---------------------------------------------------------------------------
// App builder
// ---------------------------------------------------------------------------

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(handler_health))
        .route("/v1/sessions", get(handler_list_sessions))
        .route("/v1/sessions", post(handler_create_session))
        .route("/v1/sessions/{session_id}", get(handler_get_session))
        .route(
            "/v1/sessions/{session_id}/messages",
            get(handler_session_messages),
        )
        .route("/v1/sessions/{session_id}", patch(handler_patch_session))
        .route("/v1/sessions/{session_id}", delete(handler_delete_session))
        .route(
            "/v1/sessions/{session_id}/skills",
            get(handler_session_skills),
        )
        .route(
            "/v1/sessions/{session_id}/skills",
            post(handler_set_session_skill),
        )
        .route("/v1/skills", get(handler_list_skills))
        .route("/v1/skills", post(handler_create_skill))
        .route("/v1/skills/{name}", get(handler_reveal_skill))
        .route("/v1/skills/{name}", patch(handler_update_skill))
        .route("/v1/skills/{name}", delete(handler_delete_skill))
        .route("/v1/skills/{name}/move", post(handler_move_skill))
        .route("/v1/skills/{name}/reveal", post(handler_reveal_skill_folder))
        .route("/v1/skills/upload", post(handler_stage_upload))
        .route("/v1/skills/upload/confirm", post(handler_confirm_upload))
        .route("/v1/memory", get(handler_list_memory))
        .route("/v1/memory", post(handler_add_memory))
        .route("/v1/memory", delete(handler_delete_all_memory))
        .route("/v1/memory/settings", get(handler_memory_settings_get))
        .route("/v1/memory/settings", axum::routing::put(handler_memory_settings_put))
        .route(
            "/v1/memory/{item_id}",
            patch(handler_patch_memory),
        )
        .route(
            "/v1/memory/{item_id}",
            delete(handler_delete_memory),
        )
        .route("/v1/chat/completions", post(handler_chat_completions))
        // Settings
        .route("/v1/settings", get(settings::handler_get_settings))
        .route(
            "/v1/settings/default-model",
            post(settings::handler_set_default_model),
        )
        .route(
            "/v1/settings/model-key",
            post(settings::handler_set_model_key),
        )
        .route("/v1/settings/models/add", post(settings::handler_add_model))
        .route(
            "/v1/settings/models/remove",
            post(settings::handler_remove_model),
        )
        .route(
            "/v1/settings/onboarded",
            post(settings::handler_set_onboarded),
        )
        .route(
            "/v1/settings/scratch-base",
            post(settings::handler_set_scratch_base),
        )
        .route(
            "/v1/settings/sessions-peek",
            post(settings::handler_set_sessions_peek),
        )
        .route("/v1/settings/pdf", post(settings::handler_set_pdf))
        .route(
            "/v1/settings/surfaces",
            post(settings::handler_set_surfaces),
        )
        .route(
            "/v1/settings/nav-layout",
            post(settings::handler_set_nav_layout),
        )
        .route(
            "/v1/settings/experimental-connectors",
            post(settings::handler_set_experimental_connectors),
        )
        .route(
            "/v1/settings/context-bar",
            post(settings::handler_set_context_bar),
        )
        .route(
            "/v1/settings/auto-approve",
            post(settings::handler_set_auto_approve),
        )
        .route(
            "/v1/settings/auto-approve-shadow",
            post(settings::handler_set_auto_approve_shadow),
        )
        .route(
            "/v1/settings/compaction",
            post(settings::handler_set_compaction),
        )
        // Providers
        .route("/v1/providers", get(settings::handler_get_providers))
        .route("/v1/providers", post(settings::handler_connect_provider))
        .route(
            "/v1/providers/{name}",
            delete(settings::handler_remove_provider),
        )
        .route(
            "/v1/providers/verify",
            post(settings::handler_verify_provider),
        )
        .route(
            "/v1/providers/openai-codex/status",
            get(settings::handler_codex_status),
        )
        .route(
            "/v1/providers/openai-codex/signin",
            post(settings::handler_codex_signin),
        )
        .route(
            "/v1/providers/openai-codex/signout",
            post(settings::handler_codex_signout),
        )
        // Automations
        .route("/v1/automations", get(handler_list))
        .route("/v1/automations", post(handler_create))
        .route("/v1/automations/{task_id}", get(handler_get))
        .route("/v1/automations/{task_id}", patch(handler_update))
        .route("/v1/automations/{task_id}", delete(handler_delete))
        .route("/v1/automations/{task_id}/seen", post(handler_mark_seen))
        .route("/v1/automations/{task_id}/run", post(handler_run))
        .route(
            "/v1/automations/{task_id}/runs/{run_id}/finalize",
            post(handler_finalize),
        )
        // Session roots
        .route(
            "/v1/sessions/{session_id}/roots",
            get(handler_session_roots),
        )
        .route("/v1/sessions/{session_id}/roots", post(handler_add_root))
        .route(
            "/v1/sessions/{session_id}/roots",
            delete(handler_remove_root),
        )
        // Session artifacts
        .route(
            "/v1/sessions/{session_id}/artifacts",
            get(handler_list_artifacts),
        )
        .route(
            "/v1/sessions/{session_id}/artifacts/read",
            get(handler_read_artifact),
        )
        .route(
            "/v1/sessions/{session_id}/artifacts/reveal",
            post(handler_reveal_artifact),
        )
        // Session connections
        .route(
            "/v1/sessions/{session_id}/connections",
            get(handler_get_connections),
        )
        .route(
            "/v1/sessions/{session_id}/connections",
            post(handler_set_connections),
        )
        // Session unattended
        .route(
            "/v1/sessions/{session_id}/unattended",
            get(handler_get_unattended),
        )
        .route(
            "/v1/sessions/{session_id}/unattended",
            post(handler_set_unattended),
        )
        // Workspaces
        .route(
            "/v1/workspaces/recent",
            get(subsystems::handler_workspaces_recent),
        )
        .route(
            "/v1/workspaces/trusted",
            get(subsystems::handler_workspaces_trusted),
        )
        .route(
            "/v1/workspaces/open",
            post(subsystems::handler_workspaces_open),
        )
        .route(
            "/v1/workspaces/pick",
            post(subsystems::handler_workspaces_pick),
        )
        .route(
            "/v1/workspaces/trust",
            post(subsystems::handler_workspaces_trust),
        )
        .route("/v1/workspaces/temp", post(handler_workspaces_temp))
        // Personas
        .route("/v1/personas", get(subsystems::handler_personas_list))
        .route(
            "/v1/personas/{persona_id}",
            get(subsystems::handler_persona_get),
        )
        .route(
            "/v1/personas/{persona_id}",
            post(subsystems::handler_persona_update),
        )
        .route(
            "/v1/personas/{persona_id}",
            delete(subsystems::handler_persona_delete),
        )
        .route(
            "/v1/personas/{persona_id}/enable",
            post(subsystems::handler_persona_enable),
        )
        .route(
            "/v1/personas/{persona_id}/connections",
            post(subsystems::handler_persona_connections),
        )
        .route(
            "/v1/personas/{persona_id}/media/{name}",
            get(handler_persona_media),
        )
        .route(
            "/v1/personas/{persona_id}/export",
            post(handler_persona_export),
        )
        .route(
            "/v1/personas/install",
            post(subsystems::handler_persona_install),
        )
        // Cloud
        .route("/v1/cloud/gallery", get(subsystems::handler_cloud_gallery))
        .route(
            "/v1/cloud/gallery/{slug}",
            get(subsystems::handler_cloud_gallery_item),
        )
        .route("/v1/cloud/status", get(subsystems::handler_cloud_status))
        .route("/v1/cloud/login", post(subsystems::handler_cloud_login))
        .route("/v1/cloud/logout", post(subsystems::handler_cloud_logout))
        .route(
            "/v1/cloud/telemetry",
            post(subsystems::handler_cloud_telemetry),
        )
        // Inbox
        .route("/v1/inbox", get(subsystems::handler_inbox_list))
        .route(
            "/v1/inbox/{item_id}/resolve",
            post(subsystems::handler_inbox_resolve),
        )
        .route(
            "/v1/inbox/reconcile",
            get(subsystems::handler_inbox_reconcile),
        )
        .route("/v1/inbox/routing", get(subsystems::handler_inbox_routing))
        .route(
            "/v1/inbox/routing/binding",
            post(subsystems::handler_inbox_routing_binding),
        )
        // Subscriptions
        .route(
            "/v1/subscriptions",
            get(subsystems::handler_subscriptions_list),
        )
        .route(
            "/v1/subscriptions",
            post(subsystems::handler_subscriptions_add),
        )
        .route(
            "/v1/subscriptions/remove",
            post(subsystems::handler_subscriptions_remove),
        )
        // Agents
        .route("/v1/agents", get(subsystems::handler_agents))
        // Audit
        .route("/v1/audit", get(subsystems::handler_audit))
        // Browser
        .route("/v1/browser/state", get(subsystems::handler_browser_state))
        .route("/v1/browser/close", post(subsystems::handler_browser_close))
        .route(
            "/v1/browser/screenshot",
            post(subsystems::handler_browser_screenshot),
        )
        // Channels
        .route(
            "/v1/channels/recent",
            get(subsystems::handler_channels_recent),
        )
        // Web search
        .route("/v1/web-search", get(subsystems::handler_web_search_get))
        .route("/v1/web-search", post(subsystems::handler_web_search_set))
        // Messaging
        .route(
            "/v1/messaging/dm-route",
            get(subsystems::handler_messaging_dm_route_get),
        )
        .route(
            "/v1/messaging/dm-route",
            post(subsystems::handler_messaging_dm_route_set),
        )
        // Unrouted
        .route("/v1/unrouted", get(subsystems::handler_unrouted))
        // Attachments
        .route(
            "/v1/attachments/inspect-pdf",
            post(subsystems::handler_attachments_inspect_pdf),
        )
        // Agent teams board (token-authenticated `/v1/board` — matches Python paths)
        .route("/v1/board/whoami", get(teams::handler_whoami))
        .route("/v1/board/spaces", get(teams::handler_board_spaces))
        .route("/v1/board/items", get(teams::handler_list_items))
        .route("/v1/board/items", post(teams::handler_create_item))
        .route("/v1/board/item", get(teams::handler_get_item))
        .route(
            "/v1/board/items/transition",
            post(teams::handler_board_transition),
        )
        .route(
            "/v1/board/items/comment",
            post(teams::handler_board_comment),
        )
        .route("/v1/board/items/assign", post(teams::handler_board_assign))
        .route("/v1/board/items/claim", post(teams::handler_board_claim))
        .route("/v1/board/items/attach", post(teams::handler_board_attach))
        .route("/v1/board/link", post(teams::handler_board_link))
        .route("/v1/board/attachment", get(teams::handler_attachment))
        .route("/v1/board/policy", get(teams::handler_board_policy_get))
        .route("/v1/board/policy", post(teams::handler_board_policy_set))
        .route("/v1/board/pending", get(teams::handler_board_pending))
        .route("/v1/board/consume", post(teams::handler_board_consume))
        .route(
            "/v1/board/journal/cases",
            get(teams::handler_board_journal_cases),
        )
        .route("/v1/board/journal", get(teams::handler_board_journal_get))
        .route("/v1/board/journal", post(teams::handler_board_journal_post))
        // Session-scoped board (sidecar session auth — no board bearer)
        .route(
            "/v1/sessions/{session_id}/board",
            get(teams::handler_session_board),
        )
        .route(
            "/v1/sessions/{session_id}/board/item",
            get(teams::handler_session_board_item),
        )
        .route(
            "/v1/sessions/{session_id}/board/attachment",
            get(teams::handler_session_board_attachment),
        )
        .route(
            "/v1/sessions/{session_id}/board/comment",
            post(teams::handler_session_board_comment),
        )
        .route(
            "/v1/sessions/{session_id}/board/transition",
            post(teams::handler_session_board_transition),
        )
        .route(
            "/v1/sessions/{session_id}/project-menu",
            get(handler_project_menu),
        )
        .route(
            "/v1/sessions/{session_id}/project-name",
            post(handler_project_name),
        )
        .route(
            "/v1/sessions/{session_id}/save-as-project",
            post(handler_save_as_project),
        )
        .route(
            "/v1/sessions/{session_id}/bindings",
            axum::routing::put(handler_session_bindings),
        )
        .route(
            "/v1/sessions/{session_id}/reviewer-stats",
            get(handler_reviewer_stats),
        )
        // Team registry + chat / journal
        .route("/v1/teams", get(teams::handler_list_teams))
        .route("/v1/teams", post(teams::handler_create_team))
        .route("/v1/teams/journal", get(teams::handler_teams_journal))
        .route("/v1/teams/{team_id}/chat", get(teams::handler_team_chat_get))
        .route(
            "/v1/teams/{team_id}/chat",
            post(teams::handler_team_chat_post),
        )
        // Auth / OAuth
        .route("/auth/callback", get(subsystems::handler_auth_callback))
        .route("/oauth/callback", post(subsystems::handler_oauth_callback))
        .route(
            "/mcp/oauth/callback",
            get(subsystems::handler_mcp_oauth_callback),
        )
        // MCP
        .route("/v1/mcp", get(subsystems::handler_mcp_list))
        .route("/v1/mcp", post(subsystems::handler_mcp_create))
        .route("/v1/mcp/{name}", patch(subsystems::handler_mcp_update))
        .route("/v1/mcp/{name}", delete(subsystems::handler_mcp_delete))
        .route("/v1/mcp/{name}/tools", get(subsystems::handler_mcp_tools))
        .route(
            "/v1/mcp/{name}/trust",
            get(subsystems::handler_mcp_trust_list),
        )
        .route(
            "/v1/mcp/{name}/trust/{tool}",
            delete(subsystems::handler_mcp_trust_revoke),
        )
        .route(
            "/v1/mcp/{name}/trust/convert",
            post(subsystems::handler_mcp_trust_convert),
        )
        .route(
            "/v1/mcp/config/reveal",
            post(subsystems::handler_mcp_config_reveal),
        )
        .route(
            "/v1/mcp/{name}/connect",
            post(subsystems::handler_mcp_connect),
        )
        .route(
            "/v1/mcp/{name}/signout",
            post(subsystems::handler_mcp_signout),
        )
        .route("/v1/mcp/reload", post(subsystems::handler_mcp_reload))
        // Connectors
        .route("/v1/connectors", get(subsystems::handler_connectors_list))
        .route(
            "/v1/connectors/{name}/status",
            get(subsystems::handler_connector_status),
        )
        .route(
            "/v1/connectors/slack/status",
            get(subsystems::handler_slack_status),
        )
        .route(
            "/v1/connectors/github/status",
            get(subsystems::handler_github_status),
        )
        .route(
            "/v1/connectors/github/installations/{installation_id}/disconnect",
            post(subsystems::handler_github_installation_disconnect),
        )
        .route(
            "/v1/connectors/{name}/connect",
            post(subsystems::handler_connector_connect),
        )
        .route(
            "/v1/connectors/{name}/disconnect",
            post(subsystems::handler_connector_disconnect),
        )
        .route(
            "/v1/connectors/{name}/connect-managed",
            post(subsystems::handler_connector_connect_managed),
        )
        .route(
            "/v1/connectors/{name}/mcp-connect",
            post(subsystems::handler_connector_mcp_connect),
        )
        .route(
            "/v1/connectors/{name}/accounts/{account_id}/disconnect",
            post(subsystems::handler_connector_account_disconnect),
        )
        .route(
            "/v1/connectors/{name}/accounts/{account_id}/default",
            post(subsystems::handler_connector_account_default),
        )
        .route(
            "/v1/connectors/{name}/allow",
            post(subsystems::handler_connector_allow),
        )
        .route(
            "/v1/connectors/{name}/disallow",
            post(subsystems::handler_connector_disallow),
        )
        .route(
            "/v1/connectors/{name}/tools",
            patch(subsystems::handler_connector_tools_patch),
        )
        .route(
            "/v1/connectors/{name}/unauthorized/{item_id}",
            post(subsystems::handler_connector_unauthorized),
        )
        .route(
            "/v1/connectors/slack/workspaces/{team_id}/disconnect",
            post(subsystems::handler_slack_workspace_disconnect),
        )
        .route(
            "/v1/connectors/slack/workspaces/{team_id}/directory",
            get(subsystems::handler_slack_directory),
        )
        .route(
            "/v1/connectors/slack/workspaces/{team_id}/channels",
            get(subsystems::handler_slack_channels),
        )
        .route(
            "/v1/connectors/slack/approval-owners/add",
            post(subsystems::handler_slack_approval_owners_add),
        )
        .route(
            "/v1/connectors/slack/approval-owners/remove",
            post(subsystems::handler_slack_approval_owners_remove),
        )
        .route(
            "/v1/connectors/gmail/accounts/{email}/disconnect",
            post(subsystems::handler_gmail_account_disconnect),
        )
        .route(
            "/v1/connectors/gmail/accounts/{email}/default",
            post(subsystems::handler_gmail_account_default),
        )
        .route(
            "/v1/connectors/gmail/filters",
            patch(subsystems::handler_gmail_filters),
        )
        .route(
            "/v1/connectors/google_calendar/accounts/{email}/disconnect",
            post(subsystems::handler_gcal_account_disconnect),
        )
        .route(
            "/v1/connectors/google_calendar/accounts/{email}/default",
            post(subsystems::handler_gcal_account_default),
        )
        .route(
            "/v1/connectors/hubspot/portals/{hub_id}/disconnect",
            post(subsystems::handler_hubspot_portal_disconnect),
        )
        .route(
            "/v1/connectors/hubspot/portals/{hub_id}/default",
            post(subsystems::handler_hubspot_portal_default),
        )
        .route(
            "/v1/connectors/hubspot/hidden-fields",
            patch(subsystems::handler_hubspot_hidden_fields),
        )
        // WebSocket
        .route(
            "/ws/session/{session_id}",
            axum::routing::get(ws_session_handler),
        )
        .route(
            "/ws/events",
            axum::routing::get(crate::events_ws::ws_events_handler),
        )
        .with_state(state)
        .layer(CorsLayer::permissive())
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn handler_health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "model": state.default_model_or_configured(),
    }))
}

async fn handler_create_session(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let workspace = body.get("workspace").and_then(|v| v.as_str());
    let agent = body.get("agent").and_then(|v| v.as_str()).unwrap_or("code");
    let meta = state.create_session(workspace, agent).await;
    Json(serde_json::to_value(&meta).unwrap_or(json!({})))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

async fn handler_list_sessions(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Json<Value> {
    let workspace = params.get("workspace").and_then(|v| v.as_str());
    let sessions = state.list_sessions(workspace).await;
    Json(json!({ "sessions": sessions }))
}

async fn handler_get_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, Error> {
    let meta = state
        .get_session(&session_id)
        .await
        .ok_or(Error::SessionNotFound(session_id.clone()))?;
    Ok(Json(serde_json::to_value(&meta).unwrap_or(json!({}))))
}

async fn handler_session_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, Error> {
    if !state.session_exists(&session_id).await {
        return Err(Error::SessionNotFound(session_id));
    }
    let messages = state.list_messages(&session_id).await;
    Ok(Json(json!({ "messages": messages })))
}

async fn handler_patch_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let title = body.get("title").and_then(|v| v.as_str());
    let pinned = body.get("pinned").and_then(|v| v.as_bool());
    let archived = body.get("archived").and_then(|v| v.as_bool());
    let updated = state
        .patch_session(&session_id, title, pinned, archived)
        .await
        .ok_or(Error::SessionNotFound(session_id))?;
    Ok(Json(serde_json::to_value(&updated).unwrap_or(json!({}))))
}

async fn handler_delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, Error> {
    if !state.session_exists(&session_id).await {
        return Err(Error::SessionNotFound(session_id));
    }
    state.delete_session(&session_id).await;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

async fn handler_list_memory(State(state): State<AppState>) -> Json<Value> {
    let entries = state.memory_store.list(None, None, None).unwrap_or_default();
    let items: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
        .collect();
    Json(json!({ "memory": items }))
}

fn rest_add_memory(store: &dyn ocw_data::MemoryBackend, body: &Value) -> Value {
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        return json!({ "ok": false, "error": "content required" });
    }
    let scope = match body.get("scope").and_then(|v| v.as_str()) {
        Some("global") => ocw_data::Scope::Global,
        Some("session") => ocw_data::Scope::Workspace,
        _ => ocw_data::Scope::Workspace,
    };
    let summary = body
        .get("summary")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let workspace = body
        .get("workspace")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let ws_key = workspace.map(crate::projects::project_key);
    let ws_arg = if matches!(scope, ocw_data::Scope::Workspace) {
        ws_key.as_deref()
    } else {
        None
    };
    match store.add(content, scope, None, summary, ws_arg, None) {
        Ok(entry) => serde_json::to_value(&entry).unwrap_or(json!({})),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

async fn handler_add_memory(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    Json(rest_add_memory(state.memory_store.as_ref(), &body))
}

async fn handler_delete_all_memory(State(state): State<AppState>) -> Json<Value> {
    let deleted = state.memory_store.delete_all().unwrap_or(0);
    Json(json!({ "ok": true, "deleted": deleted }))
}

async fn handler_memory_settings_get(State(state): State<AppState>) -> Json<Value> {
    Json(state.memory_settings.snapshot())
}

async fn handler_memory_settings_put(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let user_rules = body.get("user_rules").and_then(|v| v.as_str());
    Json(state.memory_settings.set(enabled, user_rules))
}

async fn handler_patch_memory(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        return Json(json!({ "ok": false, "error": "content required" }));
    }
    let summary: Option<&str> = if body.get("summary").is_some() {
        Some(
            body.get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )
    } else {
        None
    };
    match state.memory_store.update(item_id, content, summary) {
        Ok(Some(item)) => Json(json!({ "ok": true, "id": item.id, "content": item.content })),
        Ok(None) => Json(json!({ "ok": false, "error": format!("no memory with id {item_id}") })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn handler_delete_memory(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> Json<Value> {
    match state.memory_store.delete(item_id) {
        Ok(true) => Json(json!({ "ok": true, "id": item_id })),
        Ok(false) => Json(json!({ "ok": false, "error": format!("no memory with id {item_id}") })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions
// ---------------------------------------------------------------------------

async fn handler_chat_completions(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(&state.config.default_model);
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tools: Option<Vec<Value>> = body
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .map(|a| a.into_iter().collect());

    let settings = json!({
        "temperature": body.get("temperature").cloned().unwrap_or(json!(0.7)),
        "max_tokens": body.get("max_tokens").cloned(),
        "stream": false,
    });

    let turn = state
        .provider
        .complete(model, messages, tools, settings)
        .map_err(Error::Provider)?;

    let assistant_msg = if let Some(text) = &turn.text {
        let mut m = serde_json::Map::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!(text));
        Value::Object(m)
    } else {
        Value::Null
    };

    let finish_reason = turn.finish_reason.as_deref().unwrap_or("stop");

    let response = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": assistant_msg,
            "finish_reason": finish_reason,
        }],
    });

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

async fn handler_list_skills(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Json<Value> {
    let workspace = params.get("workspace").and_then(|v| v.as_str());
    let rows = state.list_skills(workspace);
    Json(serde_json::to_value(&rows).unwrap_or(json!({ "skills": [] })))
}

async fn handler_create_skill(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("global");
    let workspace = body.get("workspace").and_then(|v| v.as_str());

    let ws = workspace.map(std::path::PathBuf::from);
    let created = state
        .skill_store
        .create(name, description, instructions, scope, ws.as_deref(), "api")
        .map_err(Error::BadRequest)?;

    Ok(Json(serde_json::to_value(&created).unwrap_or(json!({}))))
}

async fn handler_reveal_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Result<Json<Value>, Error> {
    let workspace = params.get("workspace").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);
    let (folder, scope) = state
        .skill_store
        .find(&name, ws.as_deref())
        .map_err(Error::BadRequest)?;

    let md_path = folder.join("SKILL.md");
    let content =
        std::fs::read_to_string(&md_path).map_err(|e| Error::BadRequest(e.to_string()))?;

    Ok(Json(json!({
        "name": name,
        "scope": scope,
        "content": content,
    })))
}

async fn handler_update_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(params): axum::extract::Query<Value>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let workspace = params.get("workspace").and_then(|v| v.as_str());
    let description = body.get("description").and_then(|v| v.as_str());
    let instructions = body.get("instructions").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);

    let updated = state
        .skill_store
        .update(&name, description, instructions, ws.as_deref())
        .map_err(Error::BadRequest)?;

    Ok(Json(serde_json::to_value(&updated).unwrap_or(json!({}))))
}

async fn handler_delete_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Result<Json<Value>, Error> {
    let workspace = params.get("workspace").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);
    state
        .skill_store
        .delete(&name, ws.as_deref())
        .map_err(Error::BadRequest)?;
    Ok(Json(json!({ "ok": true })))
}

async fn handler_move_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let to_scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("global");
    let workspace = body.get("workspace").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);
    let moved = state
        .skill_store
        .move_skill(&name, to_scope, ws.as_deref())
        .map_err(Error::BadRequest)?;
    Ok(Json(serde_json::to_value(&moved).unwrap_or(json!({}))))
}

async fn handler_stage_upload(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, Error> {
    let mut data: Vec<u8> = Vec::new();
    let mut filename = "skill.zip".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e: axum::extract::multipart::MultipartError| Error::BadRequest(e.to_string()))?
    {
        if let Some(name) = field.name() {
            if name == "file" {
                filename = field.file_name().unwrap_or("skill.zip").to_string();
                let bytes = field.bytes().await.map_err(
                    |e: axum::extract::multipart::MultipartError| Error::BadRequest(e.to_string()),
                )?;
                data.extend_from_slice(&bytes);
            }
        }
    }

    let preview = state
        .skill_store
        .stage_upload(&data, &filename)
        .map_err(Error::BadRequest)?;

    Ok(Json(serde_json::to_value(&preview).unwrap_or(json!({}))))
}

async fn handler_confirm_upload(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("global");
    let workspace = body.get("workspace").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);

    let created = state
        .skill_store
        .confirm_upload(token, scope, ws.as_deref())
        .map_err(Error::BadRequest)?;

    Ok(Json(serde_json::to_value(&created).unwrap_or(json!({}))))
}

async fn handler_session_skills(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    let skills = state.get_session_skills(&session_id);
    let map: std::collections::HashMap<String, bool> = skills;
    Json(serde_json::to_value(&map).unwrap_or(json!({})))
}

async fn handler_set_session_skill(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let skill = body.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    state.set_session_skill(&session_id, skill, enabled);
    Json(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// Session roots
// ---------------------------------------------------------------------------

async fn handler_session_roots(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    let roots = state.get_roots(&session_id);
    Json(json!({"roots": roots}))
}

async fn handler_add_root(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let writable = body
        .get("writable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match state.add_root(&session_id, path, writable) {
        Ok(roots) => Json(json!({"ok": true, "roots": roots})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn handler_remove_root(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    match state.remove_root(&session_id, path) {
        Ok(roots) => Json(json!({"ok": true, "roots": roots})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

// ---------------------------------------------------------------------------
// Session artifacts
// ---------------------------------------------------------------------------

async fn handler_list_artifacts(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    let artifacts = state.list_artifacts(&session_id);
    Json(json!({"artifacts": artifacts}))
}

async fn handler_read_artifact(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    Json(state.read_artifact(&session_id, path))
}

async fn handler_reveal_artifact(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("reveal");
    Json(state.reveal_artifact(&session_id, path, mode))
}

// ---------------------------------------------------------------------------
// Session connections (personas) — mirrors `app.py::session_connections`
// ---------------------------------------------------------------------------

async fn handler_get_connections(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    let dm = state.settings.get_dm_session().await;
    Json(state.get_connections(&session_id, dm.as_deref()))
}

async fn handler_set_connections(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // A session override: `clear` drops the override (inherit the persona
    // default again); otherwise set an explicit on/off. Return the refreshed
    // view so the drawer can re-render without a second GET.
    let connector = body
        .get("connector")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if connector.is_empty() {
        return Json(json!({"ok": false, "error": "connector required"}));
    }
    if body.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
        state.session_connections.clear(&session_id, connector);
    } else {
        let enabled = body
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        state.session_connections.set(&session_id, connector, enabled);
    }
    let dm = state.settings.get_dm_session().await;
    Json(json!({
        "ok": true,
        "connections": state.get_connections(&session_id, dm.as_deref()),
    }))
}

// ---------------------------------------------------------------------------
// Session unattended mode — mirrors `app.py` get/set unattended
// ---------------------------------------------------------------------------

async fn handler_get_unattended(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    Json(state.get_unattended(&session_id))
}

async fn handler_set_unattended(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // The GUI gates the on-transition behind a one-tap confirm.
    let on = body
        .get("unattended")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.set_unattended(&session_id, on);
    Json(json!({"ok": true, "session_id": session_id, "unattended": on}))
}

// ---------------------------------------------------------------------------
// Skills reveal (OS folder), personas media/export, projects, temp workspace
// ---------------------------------------------------------------------------

async fn handler_reveal_skill_folder(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let workspace = body.get("workspace").and_then(|v| v.as_str());
    let ws = workspace.map(std::path::PathBuf::from);
    let (folder, _scope) = match state.skill_store.find(&name, ws.as_deref()) {
        Ok(v) => v,
        Err(e) => return Json(json!({ "ok": false, "error": e })),
    };
    let path_str = folder.to_string_lossy().to_string();
    let opened = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&path_str).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(&path_str).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&path_str).spawn()
    };
    match opened {
        Ok(_) => Json(json!({ "ok": true, "path": path_str })),
        Err(_) => Json(json!({ "ok": true, "path": path_str })),
    }
}

async fn handler_persona_media(
    State(state): State<AppState>,
    Path((persona_id, name)): Path<(String, String)>,
) -> Response {
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(media_dir) = state.persona_store.media_dir(&persona_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let file = media_dir.join(&name);
    let Ok(canon) = file.canonicalize() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(media_canon) = media_dir.canonicalize() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !canon.starts_with(&media_canon) || !canon.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match std::fs::read(&canon) {
        Ok(bytes) => {
            let mime = match canon.extension().and_then(|e| e.to_str()) {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                Some("webp") => "image/webp",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn handler_persona_export(
    State(state): State<AppState>,
    Path(persona_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let dest = body.get("dest").and_then(|v| v.as_str()).unwrap_or("");
    if dest.is_empty() {
        return Json(json!({ "ok": false, "error": "dest required" }));
    }
    Json(state.persona_store.export_persona(&persona_id, dest))
}

async fn handler_workspaces_temp(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\\')
    {
        return Json(json!({ "ok": false, "error": "invalid session id" }));
    }
    let git = body.get("git").and_then(|v| v.as_bool()).unwrap_or(true);
    let path = crate::automations::provision_scratch(&state, session_id).await;
    if git {
        let git_dir = std::path::Path::new(&path).join(".git");
        if !git_dir.is_dir() {
            let _ = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .output();
        }
    }
    let has_git = std::path::Path::new(&path).join(".git").is_dir();
    Json(json!({ "ok": true, "path": path, "git": has_git }))
}

fn empty_reviewer_bucket() -> Value {
    json!({
        "checks": 0,
        "allow": 0,
        "deny": 0,
        "unsure": 0,
        "tokens_in": 0,
        "tokens_out": 0,
        "cache_read": 0,
        "cache_write": 0,
    })
}

async fn handler_reviewer_stats(
    State(_state): State<AppState>,
    Path(_session_id): Path<String>,
) -> Json<Value> {
    // Audit-backed aggregation not yet ported — honest zeros matching Python shape.
    Json(json!({
        "live": empty_reviewer_bucket(),
        "shadow": empty_reviewer_bucket(),
    }))
}

async fn handler_project_menu(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let kind = params
        .get("kind")
        .map(|s| s.as_str())
        .unwrap_or("memory");
    let workspace = state
        .get_session_sync(&session_id)
        .and_then(|s| s.workspace)
        .filter(|w| !w.is_empty());
    let derived = workspace.as_ref().map(|ws| {
        let key = crate::projects::project_key(ws);
        let mut label = crate::projects::project_label(&key);
        if let Some(obj) = label.as_object_mut() {
            obj.insert("key".into(), json!(key));
        }
        label
    });
    let bindings = state.project_store.get_bindings(&session_id);
    let bound = bindings.get(kind).cloned();
    let named = state.project_store.list_names(kind);
    Json(json!({
        "kind": kind,
        "bound": bound,
        "derived": derived,
        "named": named,
    }))
}

async fn handler_project_name(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let workspace = state
        .get_session_sync(&session_id)
        .and_then(|s| s.workspace)
        .filter(|w| !w.is_empty());
    let Some(ws) = workspace else {
        return Json(json!({ "ok": false, "error": "session has no workspace" }));
    };
    let key = crate::projects::project_key(&ws);
    match state.project_store.name_current(kind, name, &key) {
        Ok(entry) => {
            let mut out = entry;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("ok".into(), json!(true));
            }
            Json(out)
        }
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

async fn handler_session_bindings(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    if state.running_engines.read().contains_key(&session_id) {
        return Json(json!({ "ok": false, "error": "wait for the current task to finish first" }));
    }
    match state.project_store.set_binding(&session_id, kind, name) {
        Ok(bindings) => Json(json!({ "ok": true, "bindings": bindings })),
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

async fn handler_save_as_project(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let dest = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if dest.is_empty() {
        return Json(json!({ "ok": false, "error": "no destination folder" }));
    }
    if state.running_engines.read().contains_key(&session_id) {
        return Json(json!({ "ok": false, "error": "wait for the current task to finish first" }));
    }
    let src = state
        .get_session_sync(&session_id)
        .and_then(|s| s.workspace)
        .filter(|w| !w.is_empty());
    let Some(src) = src else {
        return Json(json!({ "ok": false, "error": "this session is not in a temporary folder" }));
    };
    let scratch = {
        let settings = state.settings.get_settings().await;
        settings
            .get("scratch_base")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let src_path = std::path::PathBuf::from(&src);
    let scratch_path = std::path::PathBuf::from(shellexpand::tilde(&scratch).as_ref());
    let under_scratch = src_path
        .canonicalize()
        .ok()
        .zip(scratch_path.canonicalize().ok())
        .map(|(s, sc)| s.starts_with(&sc))
        .unwrap_or(false);
    if !under_scratch || !src_path.is_dir() {
        return Json(json!({ "ok": false, "error": "this session is not in a temporary folder" }));
    }
    let dest_path = std::path::PathBuf::from(shellexpand::tilde(dest).as_ref());
    if dest_path.exists() {
        if !dest_path.is_dir() || std::fs::read_dir(&dest_path).map(|mut d| d.next().is_some()).unwrap_or(true)
        {
            return Json(json!({ "ok": false, "error": "destination must be a new or empty folder" }));
        }
        let _ = std::fs::remove_dir(&dest_path);
    }
    if let Some(parent) = dest_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(&src_path, &dest_path) {
        Ok(()) => {
            let new_path = dest_path
                .canonicalize()
                .unwrap_or(dest_path)
                .to_string_lossy()
                .to_string();
            // Rebind session workspace if present in-memory.
            {
                let mut sessions = state.sessions.write().unwrap();
                if let Some(meta) = sessions.get_mut(&session_id) {
                    meta.workspace = Some(new_path.clone());
                }
            }
            state.running_engines.write().remove(&session_id);
            Json(json!({ "ok": true, "path": new_path }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

pub async fn run(state: AppState) -> std::io::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("ocw_server=info".parse().unwrap()))
        .init();

    let addr: SocketAddr = format!("{}:{}", state.config.host, state.config.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 8765)));

    tracing::info!("OpenWorker server listening on {}", addr);

    // Start the background scheduler.
    let sched_state = state.clone();
    tokio::spawn(async move {
        let sched_state: Arc<AppState> = Arc::new(sched_state);
        crate::scheduler::start_scheduler(sched_state).await;
    });

    let app = build_app(state);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await
}

#[cfg(test)]
mod memory_handler_tests {
    use super::*;
    use ocw_data::{MemoryBackend, MemoryStore, Scope};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn rest_add_memory_stores_workspace_key() {
        let store = Arc::new(MemoryStore::new()) as Arc<dyn MemoryBackend>;
        let ws_dir = std::env::temp_dir().join(format!("ocw-mem-test-{}", std::process::id()));
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_path = ws_dir.to_string_lossy().to_string();
        let key = crate::projects::project_key(&ws_path);

        let body = json!({
            "content": "uses cargo",
            "scope": "workspace",
            "summary": "build tool",
            "workspace": ws_path,
        });
        let result = rest_add_memory(store.as_ref(), &body);
        assert!(
            result.get("id").is_some(),
            "expected success entry: {result}"
        );

        let listed = store
            .list(Some(Scope::Workspace), Some(&key), None)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].content, "uses cargo");
        assert_eq!(listed[0].summary.as_deref(), Some("build tool"));
        assert_eq!(listed[0].workspace.as_deref(), Some(key.as_str()));

        let _ = std::fs::remove_dir_all(&ws_dir);
    }

    #[test]
    fn rest_add_memory_session_scope_maps_to_workspace() {
        let store = Arc::new(MemoryStore::new()) as Arc<dyn MemoryBackend>;
        let body = json!({
            "content": "fact",
            "scope": "session",
        });
        let result = rest_add_memory(store.as_ref(), &body);
        assert_eq!(
            result.get("scope").and_then(|v| v.as_str()),
            Some("workspace")
        );
    }
}
