//! Connector session tools — integration packs + messaging registration.
//!
//! Wires `ocw_connectors::register_connected_tools` and messaging tools into the
//! session `ToolRegistry`, mirroring Python `agent.py` connector gating.

use std::path::PathBuf;
use std::sync::Arc;

use ocw_connectors::{register_connected_tools, IntegrationContext, SecretResolver, WorkspaceRoot};
use ocw_engine::ToolRegistry;
use serde_json::Value;

use crate::connectors::ConnectorStore;
use crate::messaging_tools::{register_messaging_tools, MessagingToolsCtx};
use crate::state::SettingsManager;

const INTEGRATION_CONNECTORS: &[&str] = &["github", "gmail", "google_calendar", "hubspot"];

/// Args for connector/messaging tool registration at engine build time.
pub struct ConnectorSessionArgs {
    pub settings: SettingsManager,
    pub connected: Vec<String>,
}

impl ConnectorSessionArgs {
    pub fn from_store(settings: SettingsManager, store: &ConnectorStore) -> Self {
        let mut connected: Vec<String> = INTEGRATION_CONNECTORS
            .iter()
            .filter(|name| store.is_connected(name))
            .map(|s| (*s).to_string())
            .collect();
        // Also treat a populated `{name}:default` profile as connected when the
        // in-memory flag hasn't synced yet (startup / tests).
        for name in INTEGRATION_CONNECTORS {
            if connected.iter().any(|c| c == *name) {
                continue;
            }
            let key = format!("{name}:default");
            if settings.secrets_get_sync(&key).is_some_and(|p| !p.is_empty()) {
                connected.push((*name).to_string());
            }
        }
        Self {
            settings,
            connected,
        }
    }
}

fn make_secret_resolver(settings: SettingsManager) -> SecretResolver {
    Arc::new(move |key: &str| {
        settings
            .secrets_get_sync(key)
            .map(Value::Object)
    })
}

fn make_integration_context(
    settings: SettingsManager,
    workspace_root: &str,
) -> Arc<IntegrationContext> {
    let secrets = make_secret_resolver(settings);
    let workspace: WorkspaceRoot = Arc::new({
        let root = PathBuf::from(workspace_root);
        move || Some(root.clone())
    });
    Arc::new(IntegrationContext::new(secrets, workspace))
}

/// Register connector session tools for a live or scheduled engine.
pub fn register_connector_session_tools(
    registry: &mut ToolRegistry,
    workspace_root: &str,
    messaging: bool,
    integrations: bool,
    args: &ConnectorSessionArgs,
) {
    if messaging {
        let settings = args.settings.clone();
        let profile_keys: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
            Arc::new(move || settings.secrets_profile_keys_sync());
        register_messaging_tools(
            registry,
            MessagingToolsCtx {
                secrets: make_secret_resolver(args.settings.clone()),
                profile_keys,
                workspace: PathBuf::from(workspace_root),
            },
        );
    }

    if integrations && !args.connected.is_empty() {
        let ctx = make_integration_context(args.settings.clone(), workspace_root);
        register_connected_tools(&args.connected, ctx, registry);
    }
}
