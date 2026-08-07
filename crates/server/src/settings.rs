//! Settings and providers API handlers.

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{Map, Value};

use crate::error::Error;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub async fn handler_get_settings(State(state): State<AppState>) -> Json<Value> {
    Json(state.settings.get_settings().await)
}

pub async fn handler_set_default_model(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if model.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty model" }));
    }
    state.settings.set_default_model(model.to_string()).await;
    let settings: Value = state.settings.get_settings().await;
    let mut out = serde_json::json!({ "ok": true });
    if let Some(obj) = out.as_object_mut() {
        if let Some(s) = settings.as_object() {
            for (k, v) in s.iter() {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Json(out)
}

pub async fn handler_set_model_key(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let api_key = body
        .get("api_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if api_key.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty api key" }));
    }
    let mut fields = Map::new();
    fields.insert("api_key".into(), serde_json::json!(api_key));
    fields.insert("type".into(), serde_json::json!("api_key"));
    state.settings.set_provider("openai", fields).await;
    // Sync secrets into the Router so the next API call uses the new key.
    state
        .provider
        .update_secrets(state.settings.secrets_providers_async().await);
    let settings: Value = state.settings.get_settings().await;
    let mut out = serde_json::json!({ "ok": true });
    if let Some(obj) = out.as_object_mut() {
        if let Some(s) = settings.as_object() {
            for (k, v) in s.iter() {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Json(out)
}

pub async fn handler_add_model(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if model.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty model" }));
    }
    state.settings.add_model(model.to_string()).await;
    let settings: Value = state.settings.get_settings().await;
    let mut out = serde_json::json!({ "ok": true });
    if let Some(obj) = out.as_object_mut() {
        if let Some(s) = settings.as_object() {
            for (k, v) in s.iter() {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Json(out)
}

pub async fn handler_remove_model(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    state.settings.remove_model(model).await;
    let settings: Value = state.settings.get_settings().await;
    let mut out = serde_json::json!({ "ok": true });
    if let Some(obj) = out.as_object_mut() {
        if let Some(settings_obj) = settings.as_object() {
            for (k, v) in settings_obj.iter() {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Json(out)
}

pub async fn handler_set_onboarded(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let value = body.get("value").and_then(|v| v.as_bool()).unwrap_or(true);
    state.settings.set_onboarded(value).await;
    Json(serde_json::json!({ "ok": true, "onboarded": value }))
}

pub async fn handler_set_scratch_base(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return Ok(Json(
            serde_json::json!({ "ok": false, "error": "empty path" }),
        ));
    }
    let expanded = shellexpand::full(path)
        .map_err(|e| Error::BadRequest(e.to_string()))?
        .into_owned();
    if let Err(e) = std::fs::create_dir_all(&expanded) {
        return Ok(Json(
            serde_json::json!({ "ok": false, "error": e.to_string() }),
        ));
    }
    state.settings.set_scratch_base(expanded).await;
    Ok(Json(state.settings.get_settings().await))
}

pub async fn handler_set_sessions_peek(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let n = body
        .get("sessions_peek")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as u32;
    state.settings.set_sessions_peek(n).await;
    Json(serde_json::json!({ "ok": true, "sessions_peek": n }))
}

pub async fn handler_set_pdf(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let fallback_str = body
        .get("pdf_fallback")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let fallback_owned = fallback_str.to_string();
    let max_pages = body
        .get("pdf_max_pages")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(20);
    let max_mb = body
        .get("pdf_max_mb")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(10);
    state
        .settings
        .set_pdf_settings(fallback_owned, max_pages, max_mb)
        .await;
    Json(serde_json::json!({
        "ok": true,
        "pdf_fallback": fallback_str,
        "pdf_max_pages": max_pages,
        "pdf_max_mb": max_mb,
    }))
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

pub async fn handler_get_providers(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::json!(state.settings.get_providers().await))
}

pub async fn handler_connect_provider(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Error> {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Ok(Json(
            serde_json::json!({ "ok": false, "error": "name required" }),
        ));
    }
    let fields: Map<String, Value> = body
        .get("fields")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    state.settings.set_provider(name, fields).await;
    // Sync secrets into the Router so the next API call uses the new key.
    state
        .provider
        .update_secrets(state.settings.secrets_providers_async().await);
    let recommended = ocw_provider::get_descriptor(name).and_then(|d| d.recommended_model.clone());
    Ok(Json(
        serde_json::json!({ "ok": true, "provider": name, "recommended_model": recommended }),
    ))
}

pub async fn handler_remove_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    if ocw_provider::get_descriptor(&name).is_none() {
        return Json(
            serde_json::json!({ "ok": false, "error": format!("unknown provider: {name}") }),
        );
    }
    state.settings.delete_provider(&name).await;
    // Sync secrets into the Router so removed keys are dropped immediately.
    state
        .provider
        .update_secrets(state.settings.secrets_providers_async().await);
    Json(serde_json::json!({ "ok": true, "provider": name }))
}

pub async fn handler_set_surfaces(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let chat = body.get("chat").and_then(|v| v.as_bool()).unwrap_or(true);
    let code = body.get("code").and_then(|v| v.as_bool()).unwrap_or(true);
    state.settings.set_surfaces(chat, code).await;
    Json(serde_json::json!({ "ok": true, "surfaces": { "chat": chat, "code": code } }))
}

pub async fn handler_set_nav_layout(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let layout = body
        .get("layout")
        .and_then(|v| v.as_str())
        .unwrap_or("flat")
        .trim()
        .to_string();
    if layout != "flat" && layout != "grouped" {
        return Json(
            serde_json::json!({ "ok": false, "error": "layout must be 'flat' or 'grouped'" }),
        );
    }
    state.settings.set_nav_layout(layout.clone()).await;
    Json(serde_json::json!({ "ok": true, "nav_layout": layout }))
}

pub async fn handler_set_experimental_connectors(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let value = body.get("value").and_then(|v| v.as_bool()).unwrap_or(false);
    state.settings.set_experimental_connectors(value).await;
    Json(serde_json::json!({ "ok": true, "enabled": value }))
}

pub async fn handler_verify_provider(
    State(_state): State<AppState>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    Json(serde_json::json!({ "ok": true }))
}
