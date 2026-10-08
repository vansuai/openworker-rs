//! Process-in ConnectorPlugin registry — compile-time registration, no dynload.
//!
//! Mirrors the plan's plugin surface:
//! - [`InboundPlugin`]: build messaging adapters from a SecretStore profile
//! - [`NativeToolPack`]: register session tools for connected connectors
//! - Catalog descriptors drive AuthKind / GUI; plugins are optional per name

use crate::adapters::{SlackAdapter, TelegramAdapter};
use crate::base::BasePlatformAdapter;
use crate::catalog::{auth_kind, get_descriptor, AuthKind, ConnectorDescriptor};
use crate::integration::{register_all, IntegrationContext};
use ocw_engine::ToolRegistry;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Context needed to build managed-relay adapters (Slack / GitHub).
#[derive(Clone)]
pub struct RelayContext {
    pub relay_url: String,
    /// Cloud sign-in JWT provider (empty string when signed out).
    pub cloud_token: Arc<dyn Fn() -> String + Send + Sync>,
}

/// Build an inbound adapter for a connected platform, or `None` if creds/mode
/// don't support inbound listening.
pub type InboundBuilder =
    fn(profile: &Value, relay: Option<&RelayContext>) -> Option<Box<dyn BasePlatformAdapter>>;

/// Register native tools when the connector is connected.
pub type ToolPackRegister = fn(ctx: Arc<IntegrationContext>, registry: &mut ToolRegistry);

pub struct PluginEntry {
    pub name: &'static str,
    pub inbound: Option<InboundBuilder>,
    pub tools: Option<ToolPackRegister>,
}

fn telegram_inbound(profile: &Value, _relay: Option<&RelayContext>) -> Option<Box<dyn BasePlatformAdapter>> {
    let token = profile.get("bot_token")?.as_str()?;
    if token.is_empty() {
        return None;
    }
    Some(Box::new(TelegramAdapter::new(token.to_string())))
}

fn slack_inbound(profile: &Value, relay: Option<&RelayContext>) -> Option<Box<dyn BasePlatformAdapter>> {
    let mode = profile
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if mode == "relay" {
        // Relay adapter lives in `relay` module; Gateway wires it when hub exists.
        // Manual Socket Mode path below.
        let _ = relay;
        return None;
    }
    let bot = profile.get("bot_token")?.as_str()?;
    let app = profile.get("app_token")?.as_str()?;
    if bot.is_empty() || app.is_empty() {
        return None;
    }
    Some(Box::new(SlackAdapter::new(bot.to_string(), app.to_string())))
}

fn github_inbound(profile: &Value, relay: Option<&RelayContext>) -> Option<Box<dyn BasePlatformAdapter>> {
    let mode = profile
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if mode != "relay" {
        return None;
    }
    let _ = relay;
    // Built by Gateway via RelayHub — no standalone adapter here yet.
    None
}

fn register_github_gmail_hubspot(ctx: Arc<IntegrationContext>, registry: &mut ToolRegistry) {
    register_all(ctx, registry);
}

/// Compile-time plugin table. Add a row when implementing a new first-party connector.
pub static PLUGINS: &[PluginEntry] = &[
    PluginEntry {
        name: "telegram",
        inbound: Some(telegram_inbound),
        tools: None,
    },
    PluginEntry {
        name: "slack",
        inbound: Some(slack_inbound),
        tools: None,
    },
    PluginEntry {
        name: "github",
        inbound: Some(github_inbound),
        tools: Some(register_github_gmail_hubspot),
    },
    PluginEntry {
        name: "gmail",
        inbound: None,
        tools: Some(register_github_gmail_hubspot),
    },
    PluginEntry {
        name: "google_calendar",
        inbound: None,
        tools: Some(register_github_gmail_hubspot),
    },
    PluginEntry {
        name: "hubspot",
        inbound: None,
        tools: Some(register_github_gmail_hubspot),
    },
];

pub fn plugin_for(name: &str) -> Option<&'static PluginEntry> {
    PLUGINS.iter().find(|p| p.name == name)
}

pub fn inbound_platforms() -> Vec<&'static str> {
    PLUGINS
        .iter()
        .filter(|p| p.inbound.is_some())
        .map(|p| p.name)
        .collect()
}

/// Register native tool packs once for the session (github/gmail/gcal/hubspot share one pack).
pub fn register_connected_tools(
    connected: &[String],
    ctx: Arc<IntegrationContext>,
    registry: &mut ToolRegistry,
) {
    let mut registered_pack = false;
    for name in connected {
        if let Some(p) = plugin_for(name) {
            if p.tools.is_some() && !registered_pack {
                register_github_gmail_hubspot(ctx.clone(), registry);
                registered_pack = true;
            }
        }
    }
}

pub fn descriptor_auth(name: &str) -> Option<(ConnectorDescriptor, AuthKind)> {
    let d = get_descriptor(name)?;
    let kind = auth_kind(&d);
    Some((d, kind))
}

/// Index plugins by name for O(1) lookup in Gateway refresh.
pub fn plugin_index() -> HashMap<&'static str, &'static PluginEntry> {
    PLUGINS.iter().map(|p| (p.name, p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::descriptor_names;

    #[test]
    fn inbound_plugins_cover_messaging() {
        assert!(plugin_for("telegram").unwrap().inbound.is_some());
        assert!(plugin_for("slack").unwrap().inbound.is_some());
        assert!(plugin_for("github").unwrap().inbound.is_some());
    }

    #[test]
    fn catalog_has_google_calendar_not_hyphen() {
        let names = descriptor_names();
        assert!(names.contains(&"google_calendar"));
        assert!(!names.contains(&"google-calendar"));
    }

    #[test]
    fn auth_kind_managed_and_mcp() {
        let (_, kind) = descriptor_auth("slack").unwrap();
        assert!(matches!(kind, AuthKind::ManagedOAuth { provider: "slack" }));
        let (_, kind) = descriptor_auth("jira").unwrap();
        assert!(matches!(kind, AuthKind::McpOAuth { .. }));
        let (_, kind) = descriptor_auth("browser").unwrap();
        assert_eq!(kind, AuthKind::None);
    }
}
