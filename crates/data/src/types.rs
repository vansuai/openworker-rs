//! Shared data types used across modules.

use serde::{Deserialize, Serialize};

/// A single memory entry in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: i64,
    pub scope: Scope,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Memory visibility scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Workspace,
    Session,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Workspace => "workspace",
            Scope::Session => "session",
        }
    }
}

impl TryFrom<&str> for Scope {
    type Error = &'static str;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "global" => Ok(Scope::Global),
            "workspace" => Ok(Scope::Workspace),
            "session" => Ok(Scope::Session),
            _ => Err("unknown scope"),
        }
    }
}

impl TryFrom<String> for Scope {
    type Error = &'static str;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_from(s.as_str())
    }
}

/// A session record — metadata + messages for one conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionRecord {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub mode: String,
    #[serde(default)]
    pub messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub message_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub extra_roots: Vec<serde_json::Value>,
    #[serde(default)]
    pub grants: serde_json::Value,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_label: Option<String>,
    /// Not persisted — set by list() queries
    #[serde(default)]
    pub auto_title: Option<String>,
    /// Not persisted — set by list() queries
    #[serde(default)]
    pub renamed: bool,
}

fn default_agent() -> String {
    "code".to_string()
}

impl Default for SessionRecord {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            workspace: String::new(),
            model: String::new(),
            mode: String::new(),
            messages: Vec::new(),
            title: None,
            agent: "code".to_string(),
            message_count: 0,
            updated_at: None,
            extra_roots: Vec::new(),
            grants: serde_json::json!({}),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            auto_title: None,
            renamed: false,
        }
    }
}

/// Lightweight session summary (no messages) returned by list().
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionSummary {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub message_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_label: Option<String>,
    #[serde(default)]
    pub auto_title: Option<String>,
    #[serde(default)]
    pub renamed: bool,
}

impl From<SessionRecord> for SessionSummary {
    fn from(r: SessionRecord) -> Self {
        Self {
            session_id: r.session_id,
            workspace: r.workspace,
            model: r.model,
            mode: r.mode,
            title: r.title,
            agent: r.agent,
            message_count: r.message_count,
            updated_at: r.updated_at,
            pinned: r.pinned,
            archived: r.archived,
            origin: r.origin,
            origin_label: r.origin_label,
            auto_title: r.auto_title,
            renamed: r.renamed,
        }
    }
}
