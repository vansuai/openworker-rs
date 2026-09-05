//! Skill management — CRUD over skill folders + per-session mutes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

pub const GLOBAL_SCOPE: &str = "global";
pub const PROJECT_SCOPE: &str = "project";

pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Skill name is required.".to_string());
    }
    if name.len() > 64 {
        return Err("Skill name too long (limit 64 characters).".to_string());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(
            "Skill name may only contain letters, digits, dots, dashes, and underscores."
                .to_string(),
        );
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_alphanumeric() {
        return Err(
            "Skill name may only contain letters, digits, dots, dashes, and underscores."
                .to_string(),
        );
    }
    for c in chars {
        if !c.is_alphanumeric() && c != '.' && c != '-' && c != '_' {
            return Err(
                "Skill name may only contain letters, digits, dots, dashes, and underscores."
                    .to_string(),
            );
        }
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// SkillStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SkillStore {
    global_dir: PathBuf,
    settings_path: PathBuf,
    staging_dir: PathBuf,
    disabled_cache: Arc<RwLock<HashMap<String, bool>>>,
}

impl SkillStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            global_dir: data_dir.join("skills"),
            settings_path: data_dir.join("skills-settings.json"),
            staging_dir: data_dir.join("skills-staged"),
            disabled_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    pub fn project_dir_for(&self, workspace: &Path) -> PathBuf {
        workspace.join(".coworker").join("skills")
    }

    fn project_dir(&self, workspace: &Path) -> PathBuf {
        self.project_dir_for(workspace)
    }

    fn _base(&self, scope: &str, workspace: Option<&Path>) -> Result<PathBuf, String> {
        match scope {
            GLOBAL_SCOPE => Ok(self.global_dir.clone()),
            PROJECT_SCOPE => {
                let ws = workspace.ok_or("A workspace is required for a project-scoped skill.")?;
                if !ws.is_dir() {
                    return Err(format!("Unknown workspace: {}", ws.display()));
                }
                Ok(self.project_dir(ws))
            }
            _ => Err(format!("Unknown scope: {}", scope)),
        }
    }

    fn _folder_of(&self, base: &Path, name: &str) -> Result<PathBuf, String> {
        let folder = base.join(name);
        let resolved = folder.canonicalize().unwrap_or_else(|_| folder.clone());
        let base_resolved = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
        if !resolved.starts_with(&base_resolved) {
            return Err(format!("Skill folder escapes its scope: {}", name));
        }
        Ok(folder)
    }

    pub fn find(&self, name: &str, workspace: Option<&Path>) -> Result<(PathBuf, String), String> {
        let name = validate_name(name)?;
        if let Some(ws) = workspace {
            let project = self.project_dir(ws);
            if project.join(&name).join("SKILL.md").is_file() {
                let folder = self._folder_of(&project, &name)?;
                return Ok((folder, PROJECT_SCOPE.to_string()));
            }
        }
        if self.global_dir.join(&name).join("SKILL.md").is_file() {
            let folder = self._folder_of(&self.global_dir, &name)?;
            return Ok((folder, GLOBAL_SCOPE.to_string()));
        }
        Err(format!("Unknown skill: {}", name))
    }

    pub fn rows(&self, workspace: Option<&Path>) -> Vec<SkillRow> {
        let disabled = self.disabled_names();
        let mut out = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        let mut scopes: Vec<(PathBuf, &str)> = vec![(self.global_dir.clone(), GLOBAL_SCOPE)];
        if let Some(ws) = workspace {
            scopes.push((self.project_dir(ws), PROJECT_SCOPE));
        }

        for (base, scope) in scopes {
            if !base.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&base) {
                for entry in entries.flatten() {
                    let sub = entry.path();
                    if !sub.is_dir() {
                        continue;
                    }
                    let md_path = sub.join("SKILL.md");
                    if !md_path.is_file() {
                        continue;
                    }
                    if let Some(skill) = parse_skill(&md_path) {
                        let source =
                            frontmatter_source(&md_path).unwrap_or_else(|| "local".to_string());
                        let files_count: i32 = std::fs::read_dir(&sub)
                            .ok()
                            .map(|e| {
                                e.flatten()
                                    .filter(|e| {
                                        e.file_type().ok().map(|ft| ft.is_file()).unwrap_or(false)
                                    })
                                    .count()
                            })
                            .unwrap_or(0)
                            .saturating_sub(1)
                            as i32;

                        let row = SkillRow {
                            name: skill.name.clone(),
                            description: skill.description,
                            instructions: skill.instructions,
                            scope: scope.to_string(),
                            source,
                            enabled: !disabled.contains_key(&skill.name),
                            path: sub.to_string_lossy().into_owned(),
                            files: files_count,
                        };

                        if let Some(idx) = seen.get(&skill.name) {
                            out[*idx] = row;
                        } else {
                            seen.insert(skill.name.clone(), out.len());
                            out.push(row);
                        }
                    }
                }
            }
        }
        out
    }

    pub fn create(
        &self,
        name: &str,
        description: &str,
        instructions: &str,
        scope: &str,
        workspace: Option<&Path>,
        source: &str,
    ) -> Result<SkillCreated, String> {
        let name = validate_name(name)?;
        let description = description.trim();
        let instructions = instructions.trim();
        if instructions.is_empty() {
            return Err("Skill instructions are required.".to_string());
        }
        let base = self._base(scope, workspace)?;
        let folder = self._folder_of(&base, &name)?;
        if folder.join("SKILL.md").is_file() {
            return Err(format!(
                "A skill named '{}' already exists in that scope.",
                name
            ));
        }
        write_skill_md(&folder, &name, description, instructions, source)?;
        Ok(SkillCreated {
            name,
            scope: scope.to_string(),
            path: folder.to_string_lossy().into_owned(),
        })
    }

    pub fn update(
        &self,
        name: &str,
        description: Option<&str>,
        instructions: Option<&str>,
        workspace: Option<&Path>,
    ) -> Result<SkillUpdated, String> {
        let (folder, scope) = self.find(name, workspace)?;
        let current = parse_skill(&folder.join("SKILL.md"))
            .ok_or_else(|| "Could not read current skill".to_string())?;
        let description = description.unwrap_or(&current.description);
        let instructions = instructions.unwrap_or(&current.instructions);
        if instructions.is_empty() {
            return Err("Skill instructions are required.".to_string());
        }
        let source = frontmatter_source(&folder.join("SKILL.md")).unwrap_or_default();
        write_skill_md(&folder, &current.name, description, instructions, &source)?;
        self._invalidate_cache();
        Ok(SkillUpdated {
            name: current.name,
            scope,
        })
    }

    pub fn delete(&self, name: &str, workspace: Option<&Path>) -> Result<(), String> {
        let (folder, _scope) = self.find(name, workspace)?;
        if folder.is_symlink() {
            std::fs::remove_file(&folder).map_err(|e| e.to_string())?;
        } else {
            std::fs::remove_dir_all(&folder).map_err(|e| e.to_string())?;
        }
        self._invalidate_cache();
        Ok(())
    }

    pub fn move_skill(
        &self,
        name: &str,
        to_scope: &str,
        workspace: Option<&Path>,
    ) -> Result<SkillMoved, String> {
        let (folder, from_scope) = self.find(name, workspace)?;
        if from_scope == to_scope {
            return Ok(SkillMoved {
                name: name.to_string(),
                scope: to_scope.to_string(),
            });
        }
        let target_base = self._base(to_scope, workspace)?;
        let target = self._folder_of(&target_base, name)?;
        if target.join("SKILL.md").is_file() {
            return Err(format!(
                "A skill named '{}' already exists in the target scope.",
                name
            ));
        }
        std::fs::create_dir_all(&target_base).map_err(|e| e.to_string())?;
        std::fs::rename(&folder, &target).map_err(|e| e.to_string())?;
        self._invalidate_cache();
        Ok(SkillMoved {
            name: name.to_string(),
            scope: to_scope.to_string(),
        })
    }

    pub fn disabled_names(&self) -> HashMap<String, bool> {
        let cache = self.disabled_cache.read();
        if !cache.is_empty() || self.settings_path.is_file() {
            return cache.clone();
        }
        drop(cache);
        let disabled = self._load_disabled();
        *self.disabled_cache.write() = disabled.clone();
        disabled
    }

    fn _load_disabled(&self) -> HashMap<String, bool> {
        let text = match std::fs::read_to_string(&self.settings_path) {
            Ok(t) => t,
            Err(_) => return HashMap::new(),
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(_) => return HashMap::new(),
        };
        let arr = match value.get("disabled").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return HashMap::new(),
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| (s.to_string(), true)))
            .collect()
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let name = validate_name(name)?;
        let mut disabled = self._load_disabled();
        if enabled {
            disabled.remove(&name);
        } else {
            disabled.insert(name, true);
        }
        if let Some(parent) = self.settings_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut keys: Vec<_> = disabled.keys().cloned().collect();
        keys.sort();
        let text = serde_json::to_string_pretty(&serde_json::json!({ "disabled": keys }))
            .map_err(|e| e.to_string())?;
        std::fs::write(&self.settings_path, text).map_err(|e| e.to_string())?;
        *self.disabled_cache.write() = disabled;
        Ok(())
    }

    fn _invalidate_cache(&self) {
        *self.disabled_cache.write() = HashMap::new();
    }

    pub fn stage_upload(&self, data: &[u8], filename: &str) -> Result<SkillUploadPreview, String> {
        if let Ok(preview) = self._stage_zip(data, filename) {
            return Ok(preview);
        }
        self._stage_single_md(data, filename)
    }

    fn _stage_zip(&self, data: &[u8], _filename: &str) -> Result<SkillUploadPreview, String> {
        use std::io::Cursor;
        let mut archive = zip::ZipArchive::new(Cursor::new(data))
            .map_err(|_| "Not a valid zip archive".to_string())?;

        let names: Vec<PathBuf> = archive
            .file_names()
            .filter(|n| {
                !n.ends_with('/')
                    && !n.contains("__MACOSX")
                    && !Path::new(n)
                        .file_name()
                        .map(|f| f.to_str() == Some(".DS_Store"))
                        .unwrap_or(false)
                    && !Path::new(n)
                        .file_name()
                        .map(|f| f.to_str().map(|s| s.starts_with("._")).unwrap_or(false))
                        .unwrap_or(false)
            })
            .map(PathBuf::from)
            .collect();

        let md_entries: Vec<_> = names
            .iter()
            .filter(|n| n.file_name().map(|f| f == "SKILL.md").unwrap_or(false))
            .collect();
        let roots: std::collections::HashSet<String> = md_entries
            .iter()
            .filter_map(|n| {
                n.components()
                    .next()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
            })
            .collect();

        if md_entries.is_empty() || roots.len() != 1 {
            return Err("Archive must contain exactly one skill (one SKILL.md).".to_string());
        }

        let root = roots.into_iter().next().unwrap();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let staged = self.staged_path(&token)?;
        std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;

        for name in &names {
            let parts: Vec<_> = name.components().collect();
            let rel = if !root.is_empty()
                && !parts.is_empty()
                && parts[0].as_os_str().to_str() == Some(&root)
            {
                PathBuf::from_iter(parts[1..].iter())
            } else {
                PathBuf::from(name.as_os_str())
            };

            if rel
                .components()
                .any(|c| c == std::path::Component::ParentDir)
                || rel.is_absolute()
            {
                return Err("Archive contains unsafe paths.".to_string());
            }

            let target = staged.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut file = std::fs::File::create(&target).map_err(|e| e.to_string())?;
            let mut zip_file = archive
                .by_name(name.to_str().unwrap())
                .map_err(|e| e.to_string())?;
            std::io::copy(&mut zip_file, &mut file).map_err(|e| e.to_string())?;
        }

        let md_path = staged.join("SKILL.md");
        let skill = parse_skill(&md_path).ok_or_else(|| "Failed to parse skill".to_string())?;
        let _ = validate_name(&skill.name)?;

        let extras: Vec<String> = std::fs::read_dir(&staged)
            .ok()
            .map(|e| {
                e.flatten()
                    .filter(|e| e.file_type().ok().map(|ft| ft.is_file()).unwrap_or(false))
                    .filter(|e| e.file_name() != "SKILL.md")
                    .map(|e| e.path().file_name().unwrap().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();

        Ok(SkillUploadPreview {
            ok: true,
            token: Some(token),
            name: Some(skill.name),
            description: Some(skill.description),
            instructions: Some(skill.instructions),
            files: Some(extras),
            error: None,
        })
    }

    fn _stage_single_md(&self, data: &[u8], filename: &str) -> Result<SkillUploadPreview, String> {
        if filename.to_lowercase().ends_with(".zip") || filename.to_lowercase().ends_with(".skill")
        {
            return Err("Not a valid .zip archive.".to_string());
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| "Not a valid skill file — upload a .zip or a .md.".to_string())?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let staged = self.staged_path(&token)?;
        std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
        let md_path = staged.join("SKILL.md");
        std::fs::write(&md_path, text).map_err(|e| e.to_string())?;

        let skill = parse_skill(&md_path).ok_or_else(|| {
            "The .md file needs YAML frontmatter with at least a skill name.".to_string()
        })?;
        if let Err(e) = validate_name(&skill.name) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }

        Ok(SkillUploadPreview {
            ok: true,
            token: Some(token),
            name: Some(skill.name),
            description: Some(skill.description),
            instructions: Some(skill.instructions),
            files: Some(vec![]),
            error: None,
        })
    }

    pub fn confirm_upload(
        &self,
        token: &str,
        scope: &str,
        workspace: Option<&Path>,
    ) -> Result<SkillCreated, String> {
        let staged = self.staged_path(token)?;
        if !staged.join("SKILL.md").is_file() {
            return Err("Unknown or expired upload.".to_string());
        }
        let skill = parse_skill(&staged.join("SKILL.md"))
            .ok_or_else(|| "Could not parse skill".to_string())?;
        let name = validate_name(&skill.name)?;
        let base = self._base(scope, workspace)?;
        let folder = self._folder_of(&base, &name)?;
        if folder.join("SKILL.md").is_file() {
            return Err(format!(
                "A skill named '{}' already exists in that scope.",
                name
            ));
        }
        std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        std::fs::rename(&staged, &folder).map_err(|e| e.to_string())?;

        if frontmatter_source(&folder.join("SKILL.md")).is_none() {
            let skill2 = parse_skill(&folder.join("SKILL.md")).unwrap();
            write_skill_md(
                &folder,
                &skill2.name,
                &skill2.description,
                &skill2.instructions,
                "uploaded",
            )?;
        }

        self._invalidate_cache();
        Ok(SkillCreated {
            name,
            scope: scope.to_string(),
            path: folder.to_string_lossy().into_owned(),
        })
    }

    pub fn discard_upload(&self, token: &str) {
        if let Ok(staged) = self.staged_path(token) {
            let _ = std::fs::remove_dir_all(staged);
        }
    }

    /// Confine staged upload tokens under `staging_dir` (mirrors session-id chokepoint).
    fn staged_path(&self, token: &str) -> Result<PathBuf, String> {
        if !is_safe_upload_token(token) {
            return Err("Unknown or expired upload.".to_string());
        }
        let staged = self.staging_dir.join(token);
        let staging_canon = self
            .staging_dir
            .canonicalize()
            .unwrap_or_else(|_| self.staging_dir.clone());
        // Parent of the staged dir is staging_dir (even before create).
        let parent = staged
            .parent()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
            .unwrap_or_else(|| staging_canon.clone());
        if parent != staging_canon && !parent.starts_with(&staging_canon) {
            return Err("Unknown or expired upload.".to_string());
        }
        Ok(staged)
    }
}

fn is_safe_upload_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && !token.contains("..")
        && !token.contains('/')
        && !token.contains('\\')
        && token
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
}

// ---------------------------------------------------------------------------
// SessionSkillStore
// ---------------------------------------------------------------------------

pub struct SessionSkillStore {
    path: Option<PathBuf>,
    /// session_id → {skill_name → enabled}
    rows: RwLock<HashMap<String, HashMap<String, bool>>>,
}

impl Default for SessionSkillStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SessionSkillStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let store = Self {
            path,
            rows: RwLock::new(HashMap::new()),
        };
        store._load();
        store
    }

    fn _load(&self) {
        let Some(path) = &self.path else { return };
        if !path.is_file() {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(sessions) = data.get("sessions").and_then(|v| v.as_object()) {
                    let mut rows = self.rows.write();
                    for (sid, row) in sessions {
                        if let Some(obj) = row.as_object() {
                            let map: HashMap<String, bool> = obj
                                .iter()
                                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                                .collect();
                            rows.insert(sid.clone(), map);
                        }
                    }
                }
            }
        }
    }

    fn _save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let rows = self.rows.read();
        let mut sessions_map = serde_json::Map::new();
        for (k, v) in rows.iter() {
            let mut inner = serde_json::Map::new();
            for (sk, sv) in v {
                inner.insert(sk.clone(), serde_json::json!(sv));
            }
            sessions_map.insert(k.clone(), serde_json::Value::Object(inner));
        }
        let data = serde_json::json!({ "sessions": sessions_map });
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
    }

    pub fn get(&self, session_id: &str) -> HashMap<String, bool> {
        self.rows
            .read()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set(&self, session_id: &str, skill: &str, enabled: bool) {
        self.rows
            .write()
            .entry(session_id.to_string())
            .or_default()
            .insert(skill.to_string(), enabled);
        self._save();
    }

    pub fn clear_skill(&self, session_id: &str, skill: &str) {
        let mut rows = self.rows.write();
        if let Some(row) = rows.get_mut(session_id) {
            row.remove(skill);
            if row.is_empty() {
                rows.remove(session_id);
            }
        }
        drop(rows);
        self._save();
    }

    pub fn remove_session(&self, session_id: &str) {
        self.rows.write().remove(session_id);
        self._save();
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct SkillRow {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing)]
    pub instructions: String,
    pub scope: String,
    pub source: String,
    pub enabled: bool,
    #[serde(skip_serializing)]
    pub path: String,
    #[serde(default)]
    pub files: i32,
}

#[derive(Debug, serde::Serialize)]
pub struct SkillCreated {
    pub name: String,
    pub scope: String,
    pub path: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SkillUpdated {
    pub name: String,
    pub scope: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SkillMoved {
    pub name: String,
    pub scope: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SkillUploadPreview {
    pub ok: bool,
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_skill(md_path: &Path) -> Option<crate::skill::Skill> {
    use crate::skill::Skill;
    let text = std::fs::read_to_string(md_path).ok()?;
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

fn frontmatter_source(md_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(md_path).ok()?;
    if !text.starts_with("---") {
        return None;
    }
    let end = text[3..].find("\n---")?;
    let frontmatter = &text[3..3 + end];
    for line in frontmatter.lines() {
        if let Some((key, val)) = line.split_once(':') {
            if key.trim().to_lowercase() == "source" {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

fn write_skill_md(
    folder: &Path,
    name: &str,
    description: &str,
    instructions: &str,
    source: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let mut lines = vec![
        "---".to_string(),
        format!("name: {}", name),
        format!("description: {}", description),
    ];
    if !source.is_empty() {
        lines.push(format!("source: {}", source));
    }
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(instructions.to_string());
    lines.push(String::new());
    std::fs::write(folder.join("SKILL.md"), lines.join("\n")).map_err(|e| e.to_string())?;
    Ok(())
}
