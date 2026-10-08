//! Connector store — mirrors `coworker/connectors/setup.py` list surface.
//!
//! Loads the full descriptor catalog from `ocw_connectors::catalog` and tracks
//! basic connection/disallow state in memory. Handlers can later sync `connected`
//! from secrets using [`profile_connected`].

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::RwLock;

pub use ocw_connectors::{
    all_descriptors, auth_kind, get_descriptor, AuthKind, ConnectorDescriptor, FieldDef,
};

// ---------------------------------------------------------------------------
// profile_connected
// ---------------------------------------------------------------------------

/// Whether a stored profile has the credentials needed to count as connected.
///
/// Mirrors Python `_profile_connected` in `coworker/connectors/setup.py`.
pub fn profile_connected(d: &ConnectorDescriptor, profile: &Map<String, Value>) -> bool {
    if !d.available {
        return false;
    }
    if d.auth == "none" {
        return true;
    }
    if profile
        .get("mode")
        .and_then(|v| v.as_str())
        == Some("relay")
    {
        return true;
    }
    for f in &d.fields {
        if !f.required || f.key == "allowed_users" {
            continue;
        }
        let present = profile
            .get(&f.key)
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !present {
            return false;
        }
    }
    !profile.is_empty()
}

// ---------------------------------------------------------------------------
// ConnectorStore
// ---------------------------------------------------------------------------

pub struct ConnectorStore {
    descriptors: Vec<ConnectorDescriptor>,
    connected: RwLock<HashSet<String>>,
    disallowed: RwLock<HashSet<String>>,
}

impl Default for ConnectorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectorStore {
    /// Create with the full connector catalog from `ocw_connectors`.
    pub fn new() -> Self {
        Self {
            descriptors: ocw_connectors::all_descriptors(),
            connected: RwLock::new(HashSet::new()),
            disallowed: RwLock::new(HashSet::new()),
        }
    }

    /// List all connector descriptors.
    pub fn list_descriptors(&self) -> &[ConnectorDescriptor] {
        &self.descriptors
    }

    /// Build the connector list response (GUI `/v1/connectors` shape).
    pub fn list(&self) -> Vec<Value> {
        let connected = self.connected.read().unwrap();
        let disallowed = self.disallowed.read().unwrap();
        self.descriptors
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "title": d.title,
                    "icon": d.icon,
                    "blurb": d.blurb,
                    "about": d.about,
                    "access": d.access,
                    "auth": d.auth,
                    "two_way": d.two_way,
                    "channels": d.channels,
                    "available": d.available,
                    "brand_color": d.brand_color,
                    "logo": d.logo,
                    "aliases": d.aliases,
                    "mcp": !d.mcp_url.is_empty(),
                    "fields": d.fields.iter().map(|f| json!({
                        "key": f.key,
                        "label": f.label,
                        "secret": f.secret,
                        "required": f.required,
                        "help": f.help,
                        "placeholder": f.placeholder,
                    })).collect::<Vec<_>>(),
                    "instructions": d.instructions,
                    "connected": connected.contains(&d.name),
                    "disallowed": disallowed.contains(&d.name),
                    "experimental": d.experimental,
                    "risk_notice": d.risk_notice,
                    "managed": d.managed,
                    "managed_paused": d.managed_paused,
                    "account_field": d.account_field,
                })
            })
            .collect()
    }

    /// Get a single connector descriptor.
    pub fn get(&self, name: &str) -> Option<&ConnectorDescriptor> {
        self.descriptors.iter().find(|d| d.name == name)
    }

    pub fn is_connected(&self, name: &str) -> bool {
        self.connected.read().unwrap().contains(name)
    }

    pub fn set_connected(&self, name: &str, val: bool) {
        let mut c = self.connected.write().unwrap();
        if val {
            c.insert(name.to_string());
        } else {
            c.remove(name);
        }
    }

    pub fn set_disallowed(&self, name: &str, val: bool) {
        let mut d = self.disallowed.write().unwrap();
        if val {
            d.insert(name.to_string());
        } else {
            d.remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn store_lists_full_catalog() {
        let store = ConnectorStore::new();
        let list = store.list();
        assert!(list.len() >= 40);
        let names: Vec<&str> = list
            .iter()
            .filter_map(|c| c.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"telegram"));
        assert!(names.contains(&"notion"));
        assert!(names.contains(&"jira"));
        assert!(names.contains(&"google_calendar"));
        assert!(!names.contains(&"google-calendar"));
    }

    #[test]
    fn list_json_has_gui_fields() {
        let store = ConnectorStore::new();
        let slack = store
            .list()
            .into_iter()
            .find(|c| c["name"] == "slack")
            .unwrap();
        assert!(slack["managed"].as_bool().unwrap());
        assert!(slack.get("about").is_some());
        assert!(slack["access"].as_array().unwrap().len() > 0);
        assert_eq!(slack["mcp"], false);
        let jira = store
            .list()
            .into_iter()
            .find(|c| c["name"] == "jira")
            .unwrap();
        assert_eq!(jira["mcp"], true);
    }

    #[test]
    fn profile_connected_rules() {
        let d = get_descriptor("telegram").unwrap();
        let empty = Map::new();
        assert!(!profile_connected(&d, &empty));

        let mut ok = Map::new();
        ok.insert("bot_token".into(), json!("123:ABC"));
        assert!(profile_connected(&d, &ok));

        let browser = get_descriptor("browser").unwrap();
        assert!(profile_connected(&browser, &empty));

        let slack = get_descriptor("slack").unwrap();
        let mut relay = Map::new();
        relay.insert("mode".into(), json!("relay"));
        assert!(profile_connected(&slack, &relay));
    }

    #[test]
    fn gmail_managed_paused() {
        let d = store_get("gmail");
        assert!(d.managed_paused);
    }

    fn store_get(name: &str) -> ConnectorDescriptor {
        ConnectorStore::new().get(name).unwrap().clone()
    }
}
