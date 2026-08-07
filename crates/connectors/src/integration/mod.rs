//! Third-party integration tools — GitHub, Gmail/GCal, HubSpot (and more to
//! come: Jira, Linear, Notion, …).
//!
//! Mirrors `coworker/connectors/integration_tools.py`. Each platform module
//! registers its tools into a shared `ToolRegistry` via the [`register_all`]
//! entry point. The server injects [`IntegrationContext`] (secret resolver +
//! workspace resolver) at startup, so the tools never see raw credentials —
//! they resolve tokens lazily per call.

pub mod github;
pub mod google;
pub mod helpers;
pub mod hubspot;

pub use helpers::{IntegrationContext, SecretResolver, WorkspaceRoot};

use ocw_engine::ToolRegistry;
use std::sync::Arc;

/// Register every integration tool into the given registry.
///
/// `ctx` carries the secret + workspace resolvers; typically built by the
/// server from its `SettingsManager` and the active session's workspace root.
pub fn register_all(ctx: Arc<IntegrationContext>, registry: &mut ToolRegistry) {
    github::register(ctx.clone(), registry);
    google::register(ctx.clone(), registry);
    hubspot::register(ctx.clone(), registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    fn test_ctx() -> Arc<IntegrationContext> {
        Arc::new(IntegrationContext::new(
            Arc::new(|key: &str| -> Option<Value> {
                match key {
                    "github" => Some(json!({"token": "gh-token"})),
                    _ => None,
                }
            }),
            Arc::new(|| None),
        ))
    }

    #[test]
    fn register_all_registers_github_tools() {
        let mut registry = ToolRegistry::new();
        register_all(test_ctx(), &mut registry);
        for name in [
            "github_search",
            "github_get_issue",
            "github_create_issue",
            "github_reply",
            "github_review",
            "github_list_commits",
            "github_clone",
            "github_pull",
            "gmail_search_messages",
            "gmail_get_message",
            "gmail_send_email",
            "gcal_list_events",
            "gcal_create_event",
            "gcal_delete_event",
            "hubspot_search",
            "hubspot_get_object",
            "hubspot_create_contact",
            "hubspot_log_note",
        ] {
            assert!(registry.contains(name), "missing tool {name}");
        }
    }

    #[test]
    fn github_tool_returns_error_without_token() {
        let ctx = Arc::new(IntegrationContext::new(
            Arc::new(|_: &str| -> Option<Value> { None }),
            Arc::new(|| None),
        ));
        let mut registry = ToolRegistry::new();
        register_all(ctx, &mut registry);
        let mut args = Map::new();
        args.insert("query".to_string(), Value::String("repo:test".to_string()));
        let res = registry.execute("github_search", args).unwrap();
        let v = res.value();
        assert!(v.get("error").is_some());
    }
}
