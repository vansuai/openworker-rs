//! Inbox routing — Rust reimplementation of `coworker/inbox_routing.py`.
//!
//! Manages named inboxes + delivery bindings. Sessions route to an inbox by a
//! per-session override, else the persona's default, else `"default"`. Bindings
//! mirror items to a Slack channel or Telegram chat; the embedding of the item
//! id in the message allows an inbound reply to be correlated to the original
//! item.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_INBOX: &str = "default";

/// A single named inbox and its optional delivery binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxBinding {
    pub name: String,
    /// `None` (in-app only) | `Some("slack")` | `Some("telegram")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default)]
    pub target: String,
}

/// Routing configuration: bindings, persona defaults, session overrides.
#[derive(Debug)]
pub struct InboxRouting {
    path: Option<PathBuf>,
    state: Mutex<RoutingState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RoutingState {
    #[serde(default)]
    bindings: HashMap<String, InboxBinding>,
    #[serde(default)]
    persona_default: HashMap<String, String>,
    #[serde(default)]
    session_override: HashMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFile {
    #[serde(default)]
    bindings: Vec<InboxBinding>,
    #[serde(default)]
    persona_default: HashMap<String, String>,
    #[serde(default)]
    session_override: HashMap<String, String>,
}

impl InboxRouting {
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let state = match &path {
            Some(p) if p.is_file() => {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                let parsed: PersistedFile = serde_json::from_str(&text).unwrap_or_default();
                let mut bindings = HashMap::new();
                for b in parsed.bindings {
                    bindings.insert(b.name.clone(), b);
                }
                RoutingState {
                    bindings,
                    persona_default: parsed.persona_default,
                    session_override: parsed.session_override,
                }
            }
            _ => {
                let mut bindings = HashMap::new();
                bindings.insert(
                    DEFAULT_INBOX.to_string(),
                    InboxBinding {
                        name: DEFAULT_INBOX.to_string(),
                        channel: None,
                        target: String::new(),
                    },
                );
                RoutingState {
                    bindings,
                    persona_default: HashMap::new(),
                    session_override: HashMap::new(),
                }
            }
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let state = self.state.lock();
        let payload = PersistedFile {
            bindings: state.bindings.values().cloned().collect(),
            persona_default: state.persona_default.clone(),
            session_override: state.session_override.clone(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn set_binding(&self, name: &str, channel: Option<&str>, target: &str) {
        let mut state = self.state.lock();
        state.bindings.insert(
            name.to_string(),
            InboxBinding {
                name: name.to_string(),
                channel: channel.map(|s| s.to_string()),
                target: target.to_string(),
            },
        );
        drop(state);
        self.save();
    }

    pub fn binding_for(&self, name: &str) -> InboxBinding {
        let state = self.state.lock();
        state
            .bindings
            .get(name)
            .cloned()
            .unwrap_or_else(|| InboxBinding {
                name: name.to_string(),
                channel: None,
                target: String::new(),
            })
    }

    pub fn set_persona_default(&self, persona_id: &str, inbox_name: &str) {
        let mut state = self.state.lock();
        state
            .persona_default
            .insert(persona_id.to_string(), inbox_name.to_string());
        drop(state);
        self.save();
    }

    pub fn set_session_override(&self, session_id: &str, inbox_name: &str) {
        let mut state = self.state.lock();
        state
            .session_override
            .insert(session_id.to_string(), inbox_name.to_string());
        drop(state);
        self.save();
    }

    /// Per-session override > persona default > the global default inbox.
    pub fn route_for(&self, session_id: &str, persona_id: Option<&str>) -> String {
        let state = self.state.lock();
        if let Some(name) = state.session_override.get(session_id) {
            return name.clone();
        }
        if let Some(pid) = persona_id {
            if let Some(name) = state.persona_default.get(pid) {
                return name.clone();
            }
        }
        DEFAULT_INBOX.to_string()
    }

    pub fn bindings(&self) -> Vec<InboxBinding> {
        let state = self.state.lock();
        let mut out: Vec<InboxBinding> = state.bindings.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn session_override(&self) -> HashMap<String, String> {
        self.state.lock().session_override.clone()
    }

    pub fn persona_default(&self) -> HashMap<String, String> {
        self.state.lock().persona_default.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_routing() {
        let r = InboxRouting::new(None::<&str>);
        assert_eq!(r.route_for("s1", None), DEFAULT_INBOX);
    }

    #[test]
    fn session_overrides_persona() {
        let r = InboxRouting::new(None::<&str>);
        r.set_session_override("s1", "urgent");
        r.set_persona_default("p1", "work");
        assert_eq!(r.route_for("s1", Some("p1")), "urgent");
        assert_eq!(r.route_for("s2", Some("p1")), "work");
        assert_eq!(r.route_for("s2", None), DEFAULT_INBOX);
    }

    #[test]
    fn binding_round_trip() {
        let r = InboxRouting::new(None::<&str>);
        r.set_binding("work", Some("slack"), "C123");
        let b = r.binding_for("work");
        assert_eq!(b.channel.as_deref(), Some("slack"));
        assert_eq!(b.target, "C123");
    }
}
