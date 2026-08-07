//! HubSpot integration tools — CRM search / objects / contacts.
//!
//! Mirrors `coworker/connectors/integration_tools.py` (hubspot_* section).
//! Uses the CRM v3 REST API with a private-app access token stored in the
//! `hubspot` / `hubspot:<portal>` profile.

use super::helpers::{
    arg_i64, arg_string, clamp, err, ok, request_json, schema, IntegrationContext,
};
use ocw_engine::{ToolFn, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const HUBSPOT_BASE: &str = "https://api.hubapi.com/crm/v3";

fn hubspot_token(ctx: &IntegrationContext, account: &str) -> Option<String> {
    if account.is_empty() {
        ctx.secret_str("hubspot", "token")
            .or_else(|| ctx.secret_str("hubspot:default", "token"))
    } else {
        ctx.secret_str(&format!("hubspot:{account}"), "token")
    }
}

fn headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Accept", "application/json".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

fn hubspot_search_schema() -> ToolSchema {
    schema(
        "hubspot_search",
        "Search HubSpot CRM objects (contacts, companies, deals, tickets…). Read-only.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Free-text search."},
                "object_type": {"type": "string", "description": "contacts|companies|deals|tickets (default contacts)."},
                "max_results": {"type": "integer"},
                "account": {"type": "string"}
            },
            "required": ["query"]
        }),
    )
}

fn hubspot_get_object_schema() -> ToolSchema {
    schema(
        "hubspot_get_object",
        "Fetch a HubSpot object by id. Read-only.",
        json!({
            "type": "object",
            "properties": {
                "object_type": {"type": "string"},
                "object_id": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["object_type", "object_id"]
        }),
    )
}

fn hubspot_create_contact_schema() -> ToolSchema {
    schema(
        "hubspot_create_contact",
        "Create a HubSpot contact.",
        json!({
            "type": "object",
            "properties": {
                "email": {"type": "string"},
                "first_name": {"type": "string"},
                "last_name": {"type": "string"},
                "phone": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["email"]
        }),
    )
}

fn hubspot_log_note_schema() -> ToolSchema {
    schema(
        "hubspot_log_note",
        "Log a note (engagement) on a HubSpot object.",
        json!({
            "type": "object",
            "properties": {
                "object_type": {"type": "string"},
                "object_id": {"type": "string"},
                "note": {"type": "string"},
                "account": {"type": "string"}
            },
            "required": ["object_type", "object_id", "note"]
        }),
    )
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

fn hubspot_search(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let query = arg_string(&args, "query");
        if query.is_empty() {
            return ToolResult::ok(err("query is required"));
        }
        let object_type = if arg_string(&args, "object_type").is_empty() {
            "contacts"
        } else {
            arg_string(&args, "object_type")
        };
        if !matches!(object_type, "contacts" | "companies" | "deals" | "tickets") {
            return ToolResult::ok(err(format!("unsupported object_type: {object_type}")));
        }
        let max = clamp(arg_i64(&args, "max_results"), 10, 20);
        let account = arg_string(&args, "account");
        let token = match hubspot_token(&ctx, account) {
            Some(t) => t,
            None => return ToolResult::ok(err("no HubSpot token — connect HubSpot first")),
        };
        let payload = json!({
            "query": query,
            "limit": max,
        });
        let url = format!("{HUBSPOT_BASE}/objects/{object_type}/search");
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "count": v.get("total").and_then(|t| t.as_i64()).unwrap_or(0),
                "results": v.get("results").cloned().unwrap_or(Value::Array(vec![])),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn hubspot_get_object(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let object_type = arg_string(&args, "object_type");
        let object_id = arg_string(&args, "object_id");
        if object_type.is_empty() || object_id.is_empty() {
            return ToolResult::ok(err("object_type and object_id are required"));
        }
        let account = arg_string(&args, "account");
        let token = match hubspot_token(&ctx, account) {
            Some(t) => t,
            None => return ToolResult::ok(err("no HubSpot token — connect HubSpot first")),
        };
        let url = format!(
            "{HUBSPOT_BASE}/objects/{object_type}/{object_id}?propertiesWithHistory=false"
        );
        match request_json("GET", &url, &headers(&token), None) {
            Ok(v) => ToolResult::ok(ok(v)),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn hubspot_create_contact(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let email = arg_string(&args, "email");
        if email.is_empty() {
            return ToolResult::ok(err("email is required"));
        }
        let account = arg_string(&args, "account");
        let token = match hubspot_token(&ctx, account) {
            Some(t) => t,
            None => return ToolResult::ok(err("no HubSpot token — connect HubSpot first")),
        };
        let mut properties = json!({"email": email});
        if !arg_string(&args, "first_name").is_empty() {
            properties["firstname"] = json!(arg_string(&args, "first_name"));
        }
        if !arg_string(&args, "last_name").is_empty() {
            properties["lastname"] = json!(arg_string(&args, "last_name"));
        }
        if !arg_string(&args, "phone").is_empty() {
            properties["phone"] = json!(arg_string(&args, "phone"));
        }
        let payload = json!({"properties": properties});
        let url = format!("{HUBSPOT_BASE}/objects/contacts");
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "contact_id": v.get("id").and_then(|i| i.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn hubspot_log_note(ctx: Arc<IntegrationContext>) -> ToolFn {
    Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let object_type = arg_string(&args, "object_type");
        let object_id = arg_string(&args, "object_id");
        let note = arg_string(&args, "note");
        if object_type.is_empty() || object_id.is_empty() || note.is_empty() {
            return ToolResult::ok(err("object_type, object_id, and note are required"));
        }
        let account = arg_string(&args, "account");
        let token = match hubspot_token(&ctx, account) {
            Some(t) => t,
            None => return ToolResult::ok(err("no HubSpot token — connect HubSpot first")),
        };
        let payload = json!({
            "properties": {"hs_timestamp": now_ms()},
            "associations": [{
                "types": [{
                    "associationCategory": "HUBSPOT_DEFINED",
                    "associationTypeId": 202
                }],
                "to": {"id": object_id}
            }]
        });
        let url = format!("{HUBSPOT_BASE}/objects/notes");
        match request_json("POST", &url, &headers(&token), Some(&payload)) {
            Ok(v) => ToolResult::ok(ok(json!({
                "ok": true,
                "note_id": v.get("id").and_then(|i| i.as_str()),
            }))),
            Err(e) => ToolResult::ok(err(e)),
        }
    })
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Register all HubSpot tools.
pub fn register(ctx: Arc<IntegrationContext>, registry: &mut ocw_engine::ToolRegistry) {
    let low = ToolSpec {
        risk_level: "low",
        category: "hubspot",
        parallel_safe: false,
    };
    let medium = ToolSpec {
        risk_level: "medium",
        category: "hubspot",
        parallel_safe: false,
    };
    registry.register("hubspot_search", hubspot_search(ctx.clone()), low.clone(), Some(hubspot_search_schema()));
    registry.register("hubspot_get_object", hubspot_get_object(ctx.clone()), low.clone(), Some(hubspot_get_object_schema()));
    registry.register("hubspot_create_contact", hubspot_create_contact(ctx.clone()), medium.clone(), Some(hubspot_create_contact_schema()));
    registry.register("hubspot_log_note", hubspot_log_note(ctx.clone()), medium.clone(), Some(hubspot_log_note_schema()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_key_default() {
        let ctx = IntegrationContext::new(
            std::sync::Arc::new(|key: &str| -> Option<Value> {
                if key == "hubspot" {
                    Some(json!({"token": "abc"}))
                } else {
                    None
                }
            }),
            std::sync::Arc::new(|| None),
        );
        assert_eq!(hubspot_token(&ctx, "").as_deref(), Some("abc"));
        assert_eq!(hubspot_token(&ctx, "missing"), None);
    }
}
