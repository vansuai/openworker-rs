//! Configuration — layered TOML: built-in defaults < global < per-workspace.
//!
//! Mirrors `coworker/config.py`:
//! - Global:    `<state-dir>/config.toml` (platform-native data dir)
//! - Workspace: `<workspace>/.coworker/config.toml` (overrides global)
//!
//! Workspace command allowances apply only after the user trusts that exact
//! canonical workspace path; other permission grants remain global-only.

use crate::state::Config;
use std::path::{Path, PathBuf};

/// Fields a workspace `config.toml` may override (global-only fields excluded).
const WORKSPACE_FIELDS: &[&str] = &[
    "model",
    "mode",
    "max_iterations",
    "host",
    "port",
    "web_search_provider",
    "cloud_base_url",
    "cloud_auth_domain",
    "cloud_client_id",
    "cloud_audience",
    "cloud_relay_ws_url",
];

/// `<state-dir>/config.toml` — mirror of `config.py::global_config_path`.
pub fn global_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

/// Read a TOML file, returning an empty table on any error (missing file,
/// permission problem, or malformed TOML) — mirror of `config.py::_read`.
fn read_toml(path: &Path) -> toml::Table {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return toml::Table::new(),
    };
    match toml::from_str::<toml::Table>(&text) {
        Ok(table) => table,
        Err(_) => toml::Table::new(),
    }
}

fn str_list(value: &toml::Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Apply a single known field onto `cfg`, skipping unknown keys and values of
/// the wrong TOML type — mirror of `config.py`'s `setattr(cfg, key, value)`
/// pass, but type-safe (bad values are ignored instead of crashing).
fn apply_field(cfg: &mut Config, key: &str, value: &toml::Value) {
    match key {
        "model" => {
            if let Some(v) = value.as_str() {
                cfg.default_model = v.to_string();
            }
        }
        "mode" => {
            if let Some(v) = value.as_str() {
                cfg.mode = v.to_string();
            }
        }
        "max_iterations" => {
            if let Some(v) = value.as_integer() {
                if (0..=u32::MAX as i64).contains(&v) {
                    cfg.max_iterations = v as u32;
                }
            }
        }
        "allowed_commands" => {
            cfg.allowed_commands = str_list(value);
        }
        "auto_allow" => {
            cfg.auto_allow = str_list(value);
        }
        "host" => {
            if let Some(v) = value.as_str() {
                cfg.host = v.to_string();
            }
        }
        "port" => {
            if let Some(v) = value.as_integer() {
                if (0..=u16::MAX as i64).contains(&v) {
                    cfg.port = v as u16;
                }
            }
        }
        "web_search_provider" => {
            if let Some(v) = value.as_str() {
                cfg.web_search_provider = v.to_string();
            }
        }
        "cloud_base_url" => {
            if let Some(v) = value.as_str() {
                cfg.cloud_url = Some(v.to_string());
            }
        }
        "cloud_auth_domain" => {
            if let Some(v) = value.as_str() {
                cfg.cloud_auth_domain = v.to_string();
            }
        }
        "cloud_client_id" => {
            if let Some(v) = value.as_str() {
                cfg.cloud_client_id = v.to_string();
            }
        }
        "cloud_audience" => {
            if let Some(v) = value.as_str() {
                cfg.cloud_audience = v.to_string();
            }
        }
        "cloud_relay_ws_url" => {
            if let Some(v) = value.as_str() {
                cfg.cloud_relay_ws_url = v.to_string();
            }
        }
        _ => {}
    }
}

/// Command prefixes requested by repository config; advisory until workspace
/// trust — mirror of `config.py::workspace_allowed_commands`.
pub fn workspace_allowed_commands(workspace: &str) -> Vec<String> {
    let expanded = shellexpand::tilde(workspace);
    let path = Path::new(expanded.as_ref()).join(".coworker").join("config.toml");
    let table = read_toml(&path);
    let mut cmds = str_list(table.get("allowed_commands").unwrap_or(&toml::Value::Array(vec![])));
    // `dict.fromkeys` preserves first-seen order.
    let mut seen = std::collections::HashSet::new();
    cmds.retain(|c| seen.insert(c.clone()));
    cmds
}

/// Layered load: built-in defaults < global config.toml < workspace
/// `.coworker/config.toml` (workspace fields only, plus command allowances
/// once the workspace is trusted) — mirror of `config.py::load_config`.
pub fn load_config(workspace: Option<&str>, workspace_trusted: bool) -> Config {
    load_config_with_path(workspace, workspace_trusted, None)
}

/// Same as `load_config`, with an explicit global path (test seam mirroring
/// `config.py`'s `global_path=` keyword).
fn load_config_with_path(
    workspace: Option<&str>,
    workspace_trusted: bool,
    global_path: Option<&Path>,
) -> Config {
    let mut cfg = Config::default();

    // Global layer: every known field is applicable.
    let g = match global_path {
        Some(p) => p.to_path_buf(),
        None => global_config_path(&cfg.data_dir),
    };
    if g.is_file() {
        let table = read_toml(&g);
        for (key, value) in &table {
            apply_field(&mut cfg, key, value);
        }
    }

    // Workspace layer: only non-privileged fields, and `allowed_commands`
    // merges behind the global list once the workspace is trusted.
    if let Some(ws) = workspace {
        if !ws.trim().is_empty() {
            let expanded = shellexpand::tilde(ws);
            let w = Path::new(expanded.as_ref()).join(".coworker").join("config.toml");
            if w.is_file() {
                let table = read_toml(&w);
                for (key, value) in &table {
                    if WORKSPACE_FIELDS.contains(&key.as_str()) {
                        apply_field(&mut cfg, key, value);
                    }
                }
            }
            if workspace_trusted {
                let requested = workspace_allowed_commands(ws);
                let mut merged = cfg.allowed_commands.clone();
                let mut seen: std::collections::HashSet<String> =
                    merged.iter().cloned().collect();
                for c in requested {
                    if seen.insert(c.clone()) {
                        merged.push(c);
                    }
                }
                cfg.allowed_commands = merged;
            }
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ocw-config-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_without_files() {
        let dir = temp_dir("empty");
        let cfg = load_config(None, false);
        assert_eq!(cfg.data_dir, Config::default().data_dir);
        assert_eq!(cfg.mode, "interactive");
        assert_eq!(cfg.max_iterations, 150);
        assert_eq!(cfg.web_search_provider, "duckduckgo");
        assert_eq!(cfg.allowed_commands, Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_layer_applies() {
        let dir = temp_dir("global");
        std::fs::write(
            dir.join("config.toml"),
            "mode = \"plan\"\nmax_iterations = 42\nweb_search_provider = \"tavily\"\nport = 9999\n",
        )
        .unwrap();
        let cfg = load_config_with_path(None, false, Some(&dir.join("config.toml")));
        assert_eq!(cfg.mode, "plan");
        assert_eq!(cfg.max_iterations, 42);
        assert_eq!(cfg.web_search_provider, "tavily");
        assert_eq!(cfg.port, 9999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_layer_overrides_and_global_only_ignored() {
        let dir = temp_dir("ws");
        let ws = temp_dir("wsroot");
        std::fs::write(
            dir.join("config.toml"),
            "mode = \"plan\"\nauto_allow = [\"edit\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.join(".coworker")).unwrap();
        std::fs::write(
            ws.join(".coworker/config.toml"),
            "mode = \"discuss\"\nauto_allow = [\"break\"]\nallowed_commands = [\"git log\"]\n",
        )
        .unwrap();
        let cfg = load_config_with_path(Some(ws.to_str().unwrap()), false, Some(&dir.join("config.toml")));
        // Workspace `mode` wins; global-only fields stay untouched.
        assert_eq!(cfg.mode, "discuss");
        assert_eq!(cfg.auto_allow, vec!["edit"]);
        // Untrusted: workspace allowed_commands are NOT merged.
        assert!(cfg.allowed_commands.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn trusted_workspace_merges_command_allowances() {
        let dir = temp_dir("trust");
        let ws = temp_dir("wsroot2");
        std::fs::write(
            dir.join("config.toml"),
            "allowed_commands = [\"git status\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.join(".coworker")).unwrap();
        std::fs::write(
            ws.join(".coworker/config.toml"),
            "allowed_commands = [\"  git log \", \"git log\", \"ls\"]\n",
        )
        .unwrap();
        let cfg = load_config_with_path(Some(ws.to_str().unwrap()), true, Some(&dir.join("config.toml")));
        // Global first, workspace appended, deduped (first-seen order).
        assert_eq!(cfg.allowed_commands, vec!["git status", "git log", "ls"]);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn malformed_toml_is_ignored() {
        let dir = temp_dir("bad");
        std::fs::write(dir.join("config.toml"), "mode = [unclosed").unwrap();
        let cfg = load_config_with_path(None, false, Some(&dir.join("config.toml")));
        assert_eq!(cfg.mode, "interactive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
