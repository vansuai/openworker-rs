//! Axum router — REST API + WebSocket.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
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
        .route("/v1/skills/upload", post(handler_stage_upload))
        .route("/v1/skills/upload/confirm", post(handler_confirm_upload))
        .route("/v1/memory", get(handler_list_memory))
        .route("/v1/memory", post(handler_add_memory))
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
    let entries = state.memory_store.list(None, None, None);
    let items: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
        .collect();
    Json(json!({ "memory": items }))
}

async fn handler_add_memory(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let scope = match body.get("scope").and_then(|v| v.as_str()) {
        Some("global") => ocw_data::Scope::Global,
        Some("session") => ocw_data::Scope::Session,
        _ => ocw_data::Scope::Workspace,
    };
    let entry = state.memory_store.add(content, scope, None, None, None);
    Json(serde_json::to_value(&entry).unwrap_or(json!({})))
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
