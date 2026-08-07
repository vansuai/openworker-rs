//! Skills — Anthropic SKILL.md format with progressive disclosure.
//!
//! A skill is a folder containing `SKILL.md` (YAML frontmatter: name, description,
//! optional allowed-tools) + a markdown body of instructions + optional resources/scripts.
//!
//! Progressive disclosure: at session start only the catalog (name + description) is injected
//! into the agent's context; the full body is loaded on demand via the `load_skill` tool.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, path::Path};

use ocw_engine::{ToolArg, ToolFn, ToolRegistry, ToolResult, ToolSpec};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub path: Option<String>,
    pub allowed_tools: Vec<String>,
}

// ---------------------------------------------------------------------------
// SkillLoader — discovery and parsing
// ---------------------------------------------------------------------------

pub struct SkillLoader {
    dirs: Vec<PathBuf>,
    skills: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, Skill>>>,
}

impl Clone for SkillLoader {
    fn clone(&self) -> Self {
        Self {
            dirs: self.dirs.clone(),
            skills: std::sync::Arc::clone(&self.skills),
        }
    }
}

impl SkillLoader {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        let skills = std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let dirs_clone = dirs.clone();
        let skills_clone = std::sync::Arc::clone(&skills);
        std::thread::spawn(move || {
            let mut map = std::collections::HashMap::new();
            for dir in &dirs_clone {
                Self::_discover(dir, &mut map);
            }
            *skills_clone.write().unwrap() = map;
        })
        .join()
        .ok();
        Self { dirs, skills }
    }

    pub fn rescan(&self) {
        let mut skills = std::collections::HashMap::new();
        for dir in &self.dirs {
            Self::_discover(dir, &mut skills);
        }
        *self.skills.write().unwrap() = skills;
    }

    fn _discover(dir: &Path, skills: &mut std::collections::HashMap<String, Skill>) {
        if !dir.is_dir() {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let sub = entry.path();
            if !sub.is_dir() {
                continue;
            }
            let md_path = sub.join("SKILL.md");
            if !md_path.is_file() {
                continue;
            }
            if let Some(skill) = Self::_parse_skill(&md_path) {
                skills.insert(skill.name.clone(), skill);
            }
        }
    }

    fn _parse_skill(md_path: &Path) -> Option<Skill> {
        let text = fs::read_to_string(md_path).ok()?;
        let mut name = md_path.parent()?.file_name()?.to_str()?.to_string();
        let mut description = String::new();
        let mut allowed = Vec::new();
        let mut body = text.as_str();

        if let Some(stripped) = text.strip_prefix("---") {
            if let Some(end) = stripped.find("\n---") {
                let frontmatter = &stripped[..end];
                body = stripped[end + 4..].trim_start_matches('\n');
                for line in frontmatter.lines() {
                    if let Some((key, val)) = line.split_once(':') {
                        let key = key.trim().to_lowercase();
                        let val = val.trim();
                        match key.as_str() {
                            "name" if !val.is_empty() => name = val.to_string(),
                            "description" => description = val.to_string(),
                            "allowed-tools" | "allowed_tools" => {
                                allowed = val
                                    .split(',')
                                    .map(|t| t.trim().to_string())
                                    .filter(|t| !t.is_empty())
                                    .collect();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Some(Skill {
            name,
            description,
            instructions: body.trim().to_string(),
            path: md_path.parent().map(|p| p.to_string_lossy().into_owned()),
            allowed_tools: allowed,
        })
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.skills.read().unwrap().get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.read().unwrap().keys().cloned().collect()
    }

    pub fn catalog(&self) -> Vec<SkillCatalogEntry> {
        self.skills
            .read()
            .unwrap()
            .values()
            .map(|s| SkillCatalogEntry {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// skill_catalog_text — generates the injected prompt text
// ---------------------------------------------------------------------------

pub fn skill_catalog_text(loader: &SkillLoader, allowed: Option<&HashSet<String>>) -> String {
    let catalog: Vec<_> = loader
        .catalog()
        .into_iter()
        .filter(|c| allowed.map(|a| a.contains(&c.name)).unwrap_or(true))
        .collect();
    if catalog.is_empty() {
        return String::new();
    }
    let lines: Vec<_> = catalog
        .iter()
        .map(|c| format!("- {}: {}", c.name, c.description))
        .collect();
    format!(
        "Available skills — call load_skill(name) to load one's full instructions when it's relevant to the task:\n{}",
        lines.join("\n")
    )
}

// ---------------------------------------------------------------------------
// load_skill tool
// ---------------------------------------------------------------------------

pub struct LoadSkillTool {
    loader: SkillLoader,
    allowed_fn: std::sync::Arc<dyn Fn() -> Option<HashSet<String>> + Send + Sync>,
}

impl LoadSkillTool {
    pub fn new(
        loader: SkillLoader,
        allowed_fn: std::sync::Arc<dyn Fn() -> Option<HashSet<String>> + Send + Sync>,
    ) -> Self {
        Self { loader, allowed_fn }
    }

    pub fn register(self, registry: &mut ToolRegistry) {
        let loader = self.loader.clone();
        let allowed_fn = self.allowed_fn.clone();

        let f: ToolFn = Arc::new(move |args| {
            let args = ToolArg::new(args);
            let name = args.get_str("name").unwrap_or("").to_string();
            let loader = loader.clone();
            loader.rescan();

            let allowed = (allowed_fn)();
            if let Some(ref a) = allowed {
                if !a.contains(&name) {
                    let available: Vec<_> = loader
                        .names()
                        .into_iter()
                        .filter(|n| allowed.as_ref().map(|s| s.contains(n)).unwrap_or(true))
                        .collect();
                    return ToolResult::ok(serde_json::json!({
                        "error": format!("unknown skill: {}", name),
                        "available": available,
                    }));
                }
            }

            match loader.get(&name) {
                Some(skill) => ToolResult::ok(serde_json::json!({
                    "name": skill.name,
                    "instructions": skill.instructions,
                    "resources_path": skill.path,
                })),
                None => ToolResult::ok(serde_json::json!({
                    "error": format!("unknown skill: {}", name),
                    "available": loader.names(),
                })),
            }
        });

        let schema = ocw_engine::ToolSchema::new(
            "load_skill",
            Some("Load a skill's full instructions and resources path by name. Call this when a skill from the catalog is relevant to the current task."),
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill name to load.",
                    },
                },
                "required": ["name"],
            })),
        );

        let spec = ToolSpec {
            risk_level: "low",
            category: "skills",
            parallel_safe: true,
        };

        registry.register("load_skill", f, spec, Some(schema));
    }
}

// ---------------------------------------------------------------------------
// _loaded_skill_names — scan conversation history for successfully loaded skills
// ---------------------------------------------------------------------------

pub fn loaded_skill_names(messages: &[ocw_engine::Message]) -> HashSet<String> {
    let mut tool_results: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in messages {
        let msg = m.to_wire();
        if msg.get("role").and_then(|v| v.as_str()) == Some("tool") {
            if let Some(id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
                let content = msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                tool_results.insert(id.to_string(), content);
            }
        }
    }

    let mut loaded = HashSet::new();
    for m in messages {
        let msg = m.to_wire();
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let tool_calls = msg.get("tool_calls").and_then(|v| v.as_array());
        let Some(calls) = tool_calls else { continue };
        for call in calls {
            let fn_obj = call.get("function");
            let name = fn_obj.and_then(|f| f.get("name")).and_then(|v| v.as_str());
            if name != Some("load_skill") {
                continue;
            }
            let args = fn_obj
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let skill_name: String = match serde_json::from_str::<serde_json::Value>(args) {
                Ok(v) => match &v {
                    serde_json::Value::Object(m) => match m.get("name") {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                },
                Err(_) => String::new(),
            };

            let tc_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let result = tool_results.get(tc_id).map(|s| s.as_str()).unwrap_or("");
            if !skill_name.is_empty() && result.contains("instructions") {
                loaded.insert(skill_name);
            }
        }
    }
    loaded
}

// ---------------------------------------------------------------------------
// effective_skills — any-off-wins: disabled in Settings = off everywhere
// ---------------------------------------------------------------------------

pub fn effective_skills(
    names: HashSet<String>,
    disabled: &HashSet<String>,
    session_overrides: &std::collections::HashMap<String, bool>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for name in names {
        if disabled.contains(&name) {
            continue; // Settings disable wins
        }
        let enabled = session_overrides.get(&name).copied().unwrap_or(true);
        if !enabled {
            continue; // session mute
        }
        out.insert(name);
    }
    out
}
