//! Project identity + session bindings — minimal port of `coworker/projects.py`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::{json, Value};

const NAME_KINDS: &[&str] = &["memory", "board"];
const MAX_NAME_CHARS: usize = 60;

/// Derive the project key for a directory (git common-dir parent, else resolved path).
pub fn project_key(workspace: &str) -> String {
    let expanded = shellexpand::tilde(workspace);
    let ws = Path::new(expanded.as_ref());
    let ws = ws
        .canonicalize()
        .unwrap_or_else(|_| ws.to_path_buf());
    if ws.is_dir() {
        if let Some(common) = git_common_dir(&ws) {
            if common.file_name().and_then(|n| n.to_str()) == Some(".git") {
                if let Some(parent) = common.parent() {
                    return parent.to_string_lossy().into_owned();
                }
            }
            return common.to_string_lossy().into_owned();
        }
    }
    ws.to_string_lossy().into_owned()
}

fn git_common_dir(workspace: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["-C", &workspace.to_string_lossy(), "rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(&raw);
    let p = if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    };
    Some(p.canonicalize().unwrap_or(p))
}

pub fn project_label(key: &str) -> Value {
    let p = Path::new(key);
    let is_git = p.join(".git").exists();
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut shown = key.to_string();
    if !home.is_empty() {
        if shown == home {
            shown = "~".into();
        } else if let Some(rest) = shown.strip_prefix(&(home.clone() + "/")) {
            shown = format!("~/{rest}");
        }
    }
    let label = if is_git {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key)
            .to_string()
    } else {
        shown
    };
    json!({
        "kind": if is_git { "git" } else { "folder" },
        "label": label,
        "full": key,
    })
}

/// File-backed named project aliases + per-session bindings.
pub struct ProjectStore {
    names_path: PathBuf,
    bindings_path: PathBuf,
    lock: Mutex<()>,
}

impl ProjectStore {
    pub fn open(data_dir: &Path) -> Self {
        Self {
            names_path: data_dir.join("project-names.json"),
            bindings_path: data_dir.join("session-bindings.json"),
            lock: Mutex::new(()),
        }
    }

    fn load_map(path: &Path) -> HashMap<String, Value> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_map(path: &Path, map: &HashMap<String, Value>) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn list_names(&self, kind: &str) -> Vec<Value> {
        let _g = self.lock.lock().unwrap();
        let map = Self::load_map(&self.names_path);
        let mut rows: Vec<(String, Value)> = map
            .into_iter()
            .filter(|(k, _)| k.starts_with(&format!("{kind}:")))
            .collect();
        rows.sort_by(|a, b| {
            let ta = a.1.get("last_used_at").and_then(|v| v.as_str()).unwrap_or("");
            let tb = b.1.get("last_used_at").and_then(|v| v.as_str()).unwrap_or("");
            tb.cmp(ta).then_with(|| a.0.cmp(&b.0))
        });
        rows.into_iter()
            .map(|(_, v)| {
                json!({
                    "name": v.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    "key": v.get("key").and_then(|n| n.as_str()).unwrap_or(""),
                })
            })
            .collect()
    }

    pub fn resolve(&self, kind: &str, name: &str) -> Option<String> {
        let _g = self.lock.lock().unwrap();
        let map = Self::load_map(&self.names_path);
        map.get(&format!("{kind}:{name}"))
            .and_then(|v| v.get("key"))
            .and_then(|k| k.as_str())
            .map(String::from)
    }

    pub fn name_current(&self, kind: &str, name: &str, key: &str) -> Result<Value, String> {
        if !NAME_KINDS.contains(&kind) {
            return Err(format!("unknown kind {kind:?}"));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err("empty name".into());
        }
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let _g = self.lock.lock().unwrap();
        let mut map = Self::load_map(&self.names_path);
        map.insert(
            format!("{kind}:{name}"),
            json!({
                "kind": kind,
                "name": name,
                "key": key,
                "last_used_at": chrono::Utc::now().to_rfc3339(),
            }),
        );
        Self::save_map(&self.names_path, &map);
        Ok(json!({ "kind": kind, "name": name, "key": key }))
    }

    pub fn get_bindings(&self, session_id: &str) -> HashMap<String, String> {
        let _g = self.lock.lock().unwrap();
        let map = Self::load_map(&self.bindings_path);
        map.get(session_id)
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_binding(
        &self,
        session_id: &str,
        kind: &str,
        name: Option<&str>,
    ) -> Result<HashMap<String, String>, String> {
        if !NAME_KINDS.contains(&kind) {
            return Err(format!("unknown kind {kind:?}"));
        }
        if let Some(n) = name {
            if self.resolve(kind, n).is_none() {
                return Err(format!("no {kind} named {n:?}"));
            }
        }
        let _g = self.lock.lock().unwrap();
        let mut map = Self::load_map(&self.bindings_path);
        let mut bindings: HashMap<String, String> = map
            .get(session_id)
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(n) = name {
            bindings.insert(kind.to_string(), n.to_string());
        } else {
            bindings.remove(kind);
        }
        map.insert(session_id.to_string(), json!(bindings));
        Self::save_map(&self.bindings_path, &map);
        Ok(bindings)
    }
}

/// Memory settings (enabled + user_rules) — mirrors `coworker/memory/settings.py`.
pub struct MemorySettingsStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl MemorySettingsStore {
    pub fn open(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    fn load(&self) -> Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(json!({}))
    }

    fn save(&self, data: &Value) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(data) {
            let _ = std::fs::write(&self.path, text);
        }
    }

    pub fn snapshot(&self) -> Value {
        let _g = self.lock.lock().unwrap();
        let data = self.load();
        json!({
            "enabled": data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            "user_rules": data.get("user_rules").and_then(|v| v.as_str()).unwrap_or(""),
        })
    }

    pub fn set(&self, enabled: Option<bool>, user_rules: Option<&str>) -> Value {
        let _g = self.lock.lock().unwrap();
        let mut data = self.load();
        let obj = data.as_object_mut().cloned().unwrap_or_default();
        let mut obj = obj;
        if let Some(e) = enabled {
            obj.insert("enabled".into(), json!(e));
        }
        if let Some(rules) = user_rules {
            let clipped: String = rules.chars().take(20_000).collect();
            obj.insert("user_rules".into(), json!(clipped));
        }
        let data = Value::Object(obj);
        self.save(&data);
        json!({
            "enabled": data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            "user_rules": data.get("user_rules").and_then(|v| v.as_str()).unwrap_or(""),
        })
    }
}
