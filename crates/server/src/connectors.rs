//! Connector store — mirrors `coworker/connectors/descriptors.py`.
//!
//! Provides connector descriptors (setup fields, auth methods, instructions) and
//! tracks basic connection/disallow state. Full OAuth flows and third-party API
//! interactions are deferred to the `crates/connectors/` crate.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Descriptor types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorDescriptor {
    pub name: String,
    pub title: String,
    pub icon: String,
    pub blurb: String,
    pub auth: String,
    pub two_way: bool,
    pub fields: Vec<FieldDef>,
    pub instructions: Vec<String>,
    #[serde(default = "default_true")]
    pub available: bool,
    #[serde(default)]
    pub channels: bool,
    #[serde(default = "default_gray")]
    pub brand_color: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub experimental: bool,
    #[serde(default)]
    pub risk_notice: String,
}

fn default_gray() -> String {
    "#6b7280".to_string()
}

// ---------------------------------------------------------------------------
// Builtin connector descriptors
// ---------------------------------------------------------------------------

fn slack_descriptor() -> ConnectorDescriptor {
    ConnectorDescriptor {
        name: "slack".into(),
        title: "Slack".into(),
        icon: "slack".into(),
        blurb: "Send and read messages in Slack workspaces".into(),
        auth: "bot_token".into(),
        two_way: true,
        channels: true,
        brand_color: "#4A154B".into(),
        logo: "slack".into(),
        fields: vec![
            FieldDef {
                key: "app_token".into(),
                label: "App-Level Token".into(),
                secret: true,
                required: true,
                help: "Starts with xapp-".into(),
                placeholder: "xapp-...".into(),
            },
            FieldDef {
                key: "bot_token".into(),
                label: "Bot Token".into(),
                secret: true,
                required: true,
                help: "Starts with xoxb-".into(),
                placeholder: "xoxb-...".into(),
            },
        ],
        instructions: vec![
            "Go to https://api.slack.com/apps".into(),
            "Create a new app from manifest".into(),
            "Install to workspace".into(),
            "Copy the App-Level Token and Bot Token".into(),
        ],
        available: true,
        experimental: false,
        risk_notice: "".into(),
    }
}

fn gmail_descriptor() -> ConnectorDescriptor {
    ConnectorDescriptor {
        name: "gmail".into(),
        title: "Gmail".into(),
        icon: "gmail".into(),
        blurb: "Read, search, and compose emails".into(),
        auth: "oauth".into(),
        two_way: true,
        channels: false,
        brand_color: "#EA4335".into(),
        logo: "gmail".into(),
        fields: vec![],
        instructions: vec![
            "Sign in with your Google account".into(),
            "Grant access to Gmail".into(),
        ],
        available: true,
        experimental: false,
        risk_notice: "".into(),
    }
}

fn github_descriptor() -> ConnectorDescriptor {
    ConnectorDescriptor {
        name: "github".into(),
        title: "GitHub".into(),
        icon: "github".into(),
        blurb: "Manage issues, PRs, and repositories".into(),
        auth: "token".into(),
        two_way: true,
        channels: false,
        brand_color: "#24292e".into(),
        logo: "github".into(),
        fields: vec![FieldDef {
            key: "token".into(),
            label: "Personal Access Token".into(),
            secret: true,
            required: true,
            help: "From Settings ▸ Developer settings ▸ Personal access tokens".into(),
            placeholder: "ghp_...".into(),
        }],
        instructions: vec![
            "Go to https://github.com/settings/tokens".into(),
            "Create a fine-grained token with repo scope".into(),
            "Paste the token here".into(),
        ],
        available: true,
        experimental: false,
        risk_notice: "".into(),
    }
}

fn gcal_descriptor() -> ConnectorDescriptor {
    ConnectorDescriptor {
        name: "google-calendar".into(),
        title: "Google Calendar".into(),
        icon: "calendar".into(),
        blurb: "Read and create calendar events".into(),
        auth: "oauth".into(),
        two_way: true,
        channels: false,
        brand_color: "#4285F4".into(),
        logo: "gcal".into(),
        fields: vec![],
        instructions: vec![
            "Sign in with your Google account".into(),
            "Grant access to Google Calendar".into(),
        ],
        available: true,
        experimental: false,
        risk_notice: "".into(),
    }
}

fn hubspot_descriptor() -> ConnectorDescriptor {
    ConnectorDescriptor {
        name: "hubspot".into(),
        title: "HubSpot".into(),
        icon: "hubspot".into(),
        blurb: "Manage contacts, deals, and companies".into(),
        auth: "api_token".into(),
        two_way: true,
        channels: false,
        brand_color: "#FF7A59".into(),
        logo: "hubspot".into(),
        fields: vec![FieldDef {
            key: "access_token".into(),
            label: "Access Token".into(),
            secret: true,
            required: true,
            help: "From Settings ▸ Integrations ▸ Private Apps".into(),
            placeholder: "pat-...".into(),
        }],
        instructions: vec![
            "Go to https://app.hubspot.com/settings".into(),
            "Create a private app with required scopes".into(),
            "Copy the access token".into(),
        ],
        available: true,
        experimental: true,
        risk_notice: "Experimental connector — API behaviour may change".into(),
    }
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
    /// Create with builtin connector descriptors.
    pub fn new() -> Self {
        Self {
            descriptors: vec![
                slack_descriptor(),
                gmail_descriptor(),
                github_descriptor(),
                gcal_descriptor(),
                hubspot_descriptor(),
            ],
            connected: RwLock::new(HashSet::new()),
            disallowed: RwLock::new(HashSet::new()),
        }
    }

    /// List all connector descriptors.
    pub fn list_descriptors(&self) -> &[ConnectorDescriptor] {
        &self.descriptors
    }

    /// Build the full connector list response.
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
                    "auth": d.auth,
                    "two_way": d.two_way,
                    "fields": d.fields.iter().map(|f| json!({
                        "key": f.key,
                        "label": f.label,
                        "secret": f.secret,
                        "required": f.required,
                        "help": f.help,
                        "placeholder": f.placeholder,
                    })).collect::<Vec<_>>(),
                    "instructions": d.instructions,
                    "available": d.available,
                    "channels": d.channels,
                    "brand_color": d.brand_color,
                    "logo": d.logo,
                    "experimental": d.experimental,
                    "risk_notice": d.risk_notice,
                    "connected": connected.contains(&d.name),
                    "disallowed": disallowed.contains(&d.name),
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
