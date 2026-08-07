//! Project context — AGENTS.md ingestion (root + global) into the system prompt.
//!
//! Mirrors `coworker/project.py`.

use std::path::{Path, PathBuf};

/// `<state-dir>/AGENTS.md` — mirror of `project.py::default_global_agents_path`.
pub fn default_global_agents_path(data_dir: &Path) -> PathBuf {
    data_dir.join("AGENTS.md")
}

/// Return a system-prompt block from the global and project AGENTS.md files.
///
/// v1 loads global (`<state-dir>/AGENTS.md`) + project-root `AGENTS.md` only;
/// nested discovery is a fast-follow. Mirrors `project.py::load_agents_md`.
pub fn load_agents_md(workspace: &str, data_dir: &Path) -> String {
    let mut parts: Vec<(&str, String)> = Vec::new();

    let g = default_global_agents_path(data_dir);
    if g.is_file() {
        if let Ok(text) = std::fs::read_to_string(&g) {
            parts.push(("global", text));
        }
    }

    if !workspace.trim().is_empty() {
        // expanduser + resolve, mirroring Path(workspace).expanduser().resolve()
        let expanded = shellexpand::tilde(workspace);
        let root = Path::new(expanded.as_ref())
            .canonicalize()
            .unwrap_or_else(|_| Path::new(expanded.as_ref()).to_path_buf())
            .join("AGENTS.md");
        if root.is_file() {
            if let Ok(text) = std::fs::read_to_string(&root) {
                parts.push(("project", text));
            }
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    let blocks: Vec<String> = parts
        .iter()
        .map(|(label, text)| {
            format!("<{label} AGENTS.md>\n{}\n</{label} AGENTS.md>", text.trim())
        })
        .collect();
    format!("Project conventions:\n{}", blocks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ocw-project-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn no_files_returns_empty() {
        let dir = temp_dir("none");
        assert_eq!(load_agents_md("/nonexistent-ocw-workspace", &dir), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_only_block() {
        let dir = temp_dir("project");
        let ws = temp_dir("ws");
        std::fs::write(ws.join("AGENTS.md"), "  use Rust\n  use TDD  ").unwrap();
        let out = load_agents_md(ws.to_str().unwrap(), &dir);
        assert!(out.starts_with("Project conventions:\n"));
        assert!(out.contains("<project AGENTS.md>"));
        assert!(out.contains("use Rust"));
        assert!(out.contains("use TDD"));
        assert!(!out.contains("<global AGENTS.md>"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn global_and_project_merge() {
        let dir = temp_dir("both");
        let ws = temp_dir("ws2");
        std::fs::write(dir.join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(ws.join("AGENTS.md"), "project rules").unwrap();
        let out = load_agents_md(ws.to_str().unwrap(), &dir);
        assert!(out.contains("<global AGENTS.md>"));
        assert!(out.contains("<project AGENTS.md>"));
        assert!(out.find("<global").unwrap() < out.find("<project").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn empty_workspace_skips_project_read() {
        let dir = temp_dir("empty");
        assert!(load_agents_md("", &dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
