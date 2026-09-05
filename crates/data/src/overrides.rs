//! User-local risk overrides + OPE-136 per-tool MCP trust rules.
//! Mirrors `coworker/overrides.py` (RiskOverrideStore).

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    rules: Vec<serde_json::Value>,
    #[serde(default)]
    trust: Vec<TrustEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum TrustEntry {
    Pattern { pattern: String },
    Bare(String),
}

impl TrustEntry {
    fn pattern(&self) -> &str {
        match self {
            TrustEntry::Pattern { pattern } => pattern,
            TrustEntry::Bare(s) => s,
        }
    }
}

/// File-backed store for risk rules + durable MCP trust tool names.
#[derive(Debug)]
pub struct RiskOverrideStore {
    path: Option<PathBuf>,
    state: Mutex<Persisted>,
}

impl RiskOverrideStore {
    pub fn open(path: Option<impl AsRef<Path>>) -> Self {
        let path = path.map(|p| p.as_ref().to_path_buf());
        let state = match &path {
            Some(p) if p.is_file() => {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                serde_json::from_str(&text).unwrap_or_default()
            }
            _ => Persisted::default(),
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    pub fn trusted(&self, tool_name: &str) -> bool {
        let state = self.state.lock();
        state
            .trust
            .iter()
            .any(|t| t.pattern() == tool_name || fnmatch(t.pattern(), tool_name))
    }

    pub fn set_trust(&self, tool_name: &str) {
        {
            let mut state = self.state.lock();
            if state.trust.iter().any(|t| t.pattern() == tool_name) {
                return;
            }
            state.trust.push(TrustEntry::Pattern {
                pattern: tool_name.to_string(),
            });
        }
        self.persist();
    }

    pub fn revoke_trust(&self, tool_name: &str) -> bool {
        let removed = {
            let mut state = self.state.lock();
            let before = state.trust.len();
            state.trust.retain(|t| t.pattern() != tool_name);
            state.trust.len() != before
        };
        if removed {
            self.persist();
        }
        removed
    }

    /// Trust rules whose pattern starts with `mcp__{server}__`.
    pub fn trusted_for_server(&self, server: &str) -> Vec<String> {
        let prefix = format!("mcp__{server}__");
        let state = self.state.lock();
        state
            .trust
            .iter()
            .map(|t| t.pattern().to_string())
            .filter(|p| p.starts_with(&prefix) || p == &format!("mcp__{server}"))
            .collect()
    }

    pub fn all_trusted(&self) -> HashSet<String> {
        let state = self.state.lock();
        state
            .trust
            .iter()
            .map(|t| t.pattern().to_string())
            .collect()
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let state = self.state.lock();
        if let Ok(text) = serde_json::to_string_pretty(&*state) {
            let _ = std::fs::write(path, text + "\n");
        }
    }
}

fn fnmatch(pattern: &str, name: &str) -> bool {
    // Minimal glob: only trailing `*` (card writes exact names; globs are hand-edit).
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn trust_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("risk_overrides.json");
        let store = RiskOverrideStore::open(Some(&path));
        store.set_trust("mcp__jira__getIssue");
        assert!(store.trusted("mcp__jira__getIssue"));
        assert_eq!(
            store.trusted_for_server("jira"),
            vec!["mcp__jira__getIssue".to_string()]
        );
        assert!(store.revoke_trust("mcp__jira__getIssue"));
        let reloaded = RiskOverrideStore::open(Some(&path));
        assert!(!reloaded.trusted("mcp__jira__getIssue"));
    }
}
