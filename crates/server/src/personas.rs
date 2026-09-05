//! Persona store — mirrors `coworker/personas/registry.py`.
//!
//! Manages installed personas (builtin + third-party), their lifecycle state
//! (enabled/surfaced/default), and persistence to a JSON file. Third-party
//! personas are snapshotted into a managed install directory at install time.

use crate::persona_manifest::{PersonaManifest, Recommendation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

const DEFAULT_PERSONA_ID: &str = "cowork";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub description: String,
    /// The markdown body (system prompt) from the manifest.
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub needs_workspace: bool,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default = "default_true")]
    pub default_surfaced: bool,
    #[serde(default)]
    pub messaging: bool,
    #[serde(default)]
    pub connectors: bool,
    #[serde(default)]
    pub default_permission_mode: String,
    #[serde(default)]
    pub recommended_models: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Connector/MCP recommendations surfaced in the connections drawer.
    #[serde(default)]
    pub recommends: Vec<Recommendation>,
}

fn default_true() -> bool {
    true
}

impl PersonaEntry {
    /// Convert a parsed manifest into a store entry (mirror of
    /// `registry.py::_register_manifest`).
    fn from_manifest(m: PersonaManifest) -> PersonaEntry {
        let needs_workspace = m.needs_workspace();
        PersonaEntry {
            id: m.id,
            name: m.name,
            icon: m.icon,
            tagline: m.tagline,
            description: m.description,
            system_prompt: m.system_prompt,
            tools: m.tools,
            family: m.family,
            workspace: m.workspace,
            needs_workspace,
            builtin: m.builtin,
            default_surfaced: true,
            messaging: m.messaging,
            connectors: m.connectors,
            default_permission_mode: m.default_permission_mode,
            recommended_models: m.recommended_models,
            skills: m.skills,
            mcp: m.mcp,
            recommends: m.recommends,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersonaState {
    #[serde(default)]
    enabled: HashMap<String, bool>,
    #[serde(default)]
    surfaced: HashMap<String, bool>,
    #[serde(default)]
    default: Option<String>,
}

// ---------------------------------------------------------------------------
// PersonaStore
// ---------------------------------------------------------------------------

pub struct PersonaStore {
    state_path: PathBuf,
    installed_dir: PathBuf,
    state: RwLock<InnerState>,
}

struct InnerState {
    entries: HashMap<String, PersonaEntry>,
    enabled: HashMap<String, bool>,
    surfaced: HashMap<String, bool>,
    default_id: String,
}

impl PersonaStore {
    /// Create a new persona store. `data_dir` is the coworker data directory.
    pub fn new(data_dir: &Path) -> Self {
        let state_path = data_dir.join("personas.json");
        let installed_dir = data_dir.join("personas-installed");

        let store = Self {
            state_path,
            installed_dir,
            state: RwLock::new(InnerState {
                entries: HashMap::new(),
                enabled: HashMap::new(),
                surfaced: HashMap::new(),
                default_id: DEFAULT_PERSONA_ID.to_string(),
            }),
        };

        store.init_builtins();
        store.load_installed();
        store.load_state();
        store
    }

    // -- builtins ----------------------------------------------------------------

    fn init_builtins(&self) {
        let mut s = self.state.write().unwrap();
        // Core builtins matching Python PersonaRegistry._load_builtin
        let builtins = vec![
            PersonaEntry {
                id: "cowork".into(),
                name: "OpenWorker".into(),
                icon: "cowork".into(),
                tagline: "Produce a deliverable — research, analysis, scripts".into(),
                description: String::new(),
                system_prompt: String::new(),
                tools: vec![
                    "shell".into(),
                    "files".into(),
                    "web_search".into(),
                    "git".into(),
                ],
                family: "knowledge".into(),
                workspace: "deliverable".into(),
                needs_workspace: true,
                builtin: true,
                default_surfaced: true,
                messaging: false,
                connectors: false,
                default_permission_mode: "interactive".into(),
                recommended_models: vec![],
                skills: vec![],
                mcp: vec![],
                recommends: vec![],
            },
            PersonaEntry {
                id: "code".into(),
                name: "Code".into(),
                icon: "code".into(),
                tagline: "Work in a codebase — files, git, shell".into(),
                description: String::new(),
                system_prompt: String::new(),
                tools: vec![
                    "shell".into(),
                    "files".into(),
                    "git".into(),
                    "code_search".into(),
                ],
                family: "code".into(),
                workspace: "git".into(),
                needs_workspace: true,
                builtin: true,
                default_surfaced: true,
                messaging: false,
                connectors: false,
                default_permission_mode: "interactive".into(),
                recommended_models: vec![],
                skills: vec![],
                mcp: vec![],
                recommends: vec![],
            },
            PersonaEntry {
                id: "chat".into(),
                name: "Chat".into(),
                icon: "chat".into(),
                tagline: "Quick questions — no workspace".into(),
                description: String::new(),
                system_prompt: String::new(),
                tools: vec![],
                family: "knowledge".into(),
                workspace: "none".into(),
                needs_workspace: false,
                builtin: true,
                default_surfaced: false,
                messaging: false,
                connectors: false,
                default_permission_mode: "interactive".into(),
                recommended_models: vec![],
                skills: vec![],
                mcp: vec![],
                recommends: vec![],
            },
        ];

        for b in builtins {
            s.entries.insert(b.id.clone(), b);
        }
    }

    // -- persistence ------------------------------------------------------------

    fn load_state(&self) {
        let mut s = self.state.write().unwrap();
        if let Ok(data) = std::fs::read_to_string(&self.state_path) {
            if let Ok(ps) = serde_json::from_str::<PersonaState>(&data) {
                for (id, en) in &ps.enabled {
                    s.enabled.insert(id.clone(), *en);
                }
                for (id, sf) in &ps.surfaced {
                    s.surfaced.insert(id.clone(), *sf);
                }
                if let Some(ref d) = ps.default {
                    s.default_id = d.clone();
                }
            }
        }
    }

    fn save_state(&self) {
        let s = self.state.read().unwrap();
        let ps = PersonaState {
            enabled: s.enabled.clone(),
            surfaced: s.surfaced.clone(),
            default: Some(s.default_id.clone()),
        };
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&ps) {
            let _ = std::fs::write(&self.state_path, json);
        }
    }

    fn load_installed(&self) {
        let dir = &self.installed_dir;
        if !dir.is_dir() {
            return;
        }
        let mut s = self.state.write().unwrap();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    if let Some(id) = sub.file_name().and_then(|n| n.to_str()) {
                        // Load manifest.md from the snapshot dir
                        let manifest = sub.join("manifest.md");
                        if manifest.is_file() {
                            if let Ok(pe) = self.parse_manifest_file(&manifest, false) {
                                s.entries.insert(id.to_string(), pe);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Parse a manifest file with the strict frontmatter parser
    /// (mirror of `manifest.py::load_manifest_file`), converting to an entry.
    fn parse_manifest_file(&self, path: &Path, builtin: bool) -> Result<PersonaEntry, String> {
        let m = crate::persona_manifest::load_manifest_file(path, builtin)?;
        Ok(PersonaEntry::from_manifest(m))
    }

    // -- queries ----------------------------------------------------------------

    pub fn list_all(&self) -> Vec<Value> {
        let s = self.state.read().unwrap();
        s.entries
            .values()
            .map(|e| {
                json!({
                    "id": e.id,
                    "name": e.name,
                    "icon": e.icon,
                    "tagline": e.tagline,
                    "description": e.description,
                    "needs_workspace": e.needs_workspace,
                    "builtin": e.builtin,
                    "family": e.family,
                    "workspace": e.workspace,
                    "tools": e.tools,
                    "enabled": self.is_enabled_inner(&s, &e.id),
                    "surfaced": self.is_surfaced_inner(&s, &e.id),
                    "default": e.id == s.default_id,
                })
            })
            .collect()
    }

    pub fn get(&self, persona_id: &str) -> Option<PersonaEntry> {
        self.state.read().unwrap().entries.get(persona_id).cloned()
    }

    /// Media folder for an installed persona (`personas-installed/{id}/media`), if present.
    pub fn media_dir(&self, persona_id: &str) -> Option<PathBuf> {
        if persona_id.is_empty() || persona_id.contains('/') || persona_id.contains('\\') {
            return None;
        }
        let dir = self.installed_dir.join(persona_id).join("media");
        if dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }

    /// Sharing v1 export: zip manifest (+ skills/) into dest_dir.
    pub fn export_persona(&self, persona_id: &str, dest_dir: &str) -> Value {
        let snap = self.installed_dir.join(persona_id);
        let md = snap.join("manifest.md");
        if !md.is_file() {
            return json!({ "ok": false, "error": "this coworker has no shareable bundle" });
        }
        let dest = PathBuf::from(shellexpand::tilde(dest_dir).as_ref());
        if !dest.is_dir() {
            return json!({ "ok": false, "error": "destination folder does not exist" });
        }
        let zip_path = dest.join(format!("{persona_id}-coworker.zip"));
        match write_persona_zip(&md, &snap.join("skills"), &zip_path) {
            Ok(()) => json!({ "ok": true, "path": zip_path.to_string_lossy() }),
            Err(e) => json!({ "ok": false, "error": format!("could not write the archive: {e}") }),
        }
    }

    /// The current default persona id.
    pub fn default_persona(&self) -> String {
        self.state.read().unwrap().default_id.clone()
    }

    pub fn get_detail(&self, persona_id: &str) -> Option<Value> {
        let s = self.state.read().unwrap();
        let e = s.entries.get(persona_id)?;
        Some(json!({
            "id": e.id,
            "name": e.name,
            "icon": e.icon,
            "tagline": e.tagline,
            "description": e.description,
            "enabled": self.is_enabled_inner(&s, &e.id),
            "tools": e.tools,
            "recommended_models": e.recommended_models,
            "default_permission_mode": e.default_permission_mode,
            "workspace": e.workspace,
            "recommends": e.recommends,
            "default_connections": [],
        }))
    }

    pub fn is_enabled(&self, persona_id: &str) -> bool {
        let s = self.state.read().unwrap();
        self.is_enabled_inner(&s, persona_id)
    }

    fn is_enabled_inner(&self, s: &InnerState, persona_id: &str) -> bool {
        if let Some(&en) = s.enabled.get(persona_id) {
            return en;
        }
        persona_id == s.default_id || persona_id == DEFAULT_PERSONA_ID
    }

    fn is_surfaced_inner(&self, s: &InnerState, persona_id: &str) -> bool {
        if let Some(&sf) = s.surfaced.get(persona_id) {
            return sf;
        }
        s.entries
            .get(persona_id)
            .map(|e| e.default_surfaced)
            .unwrap_or(true)
    }

    // -- mutations --------------------------------------------------------------

    pub fn set_enabled(&self, persona_id: &str, enabled: bool) -> Result<(), String> {
        let mut s = self.state.write().unwrap();
        if !s.entries.contains_key(persona_id) {
            return Err(format!("unknown persona: {persona_id}"));
        }
        s.enabled.insert(persona_id.to_string(), enabled);
        if enabled {
            s.surfaced.insert(persona_id.to_string(), true);
        }
        drop(s);
        self.save_state();
        Ok(())
    }

    pub fn set_surfaced(&self, persona_id: &str, surfaced: bool) -> Result<(), String> {
        let mut s = self.state.write().unwrap();
        if !s.entries.contains_key(persona_id) {
            return Err(format!("unknown persona: {persona_id}"));
        }
        s.surfaced.insert(persona_id.to_string(), surfaced);
        drop(s);
        self.save_state();
        Ok(())
    }

    pub fn set_default(&self, persona_id: &str) -> Result<(), String> {
        let mut s = self.state.write().unwrap();
        if !s.entries.contains_key(persona_id) {
            return Err(format!("unknown persona: {persona_id}"));
        }
        s.default_id = persona_id.to_string();
        s.enabled.insert(persona_id.to_string(), true);
        drop(s);
        self.save_state();
        Ok(())
    }

    pub fn uninstall(&self, persona_id: &str) -> Result<(), String> {
        let mut s = self.state.write().unwrap();
        let entry = s
            .entries
            .get(persona_id)
            .ok_or(format!("unknown persona: {persona_id}"))?;
        if entry.builtin {
            return Err(format!("{persona_id} is built-in and cannot be deleted"));
        }
        s.entries.remove(persona_id);
        s.enabled.remove(persona_id);
        s.surfaced.remove(persona_id);
        if s.default_id == persona_id {
            s.default_id = DEFAULT_PERSONA_ID.to_string();
        }
        // Remove snapshot dir
        let snap = self.installed_dir.join(persona_id);
        if snap.is_dir() {
            let _ = std::fs::remove_dir_all(&snap);
        }
        drop(s);
        self.save_state();
        Ok(())
    }

    /// Clone a persona repo and install its personas (disabled pending consent)
    /// — mirror of `registry.py::install_from_git` + `loading.py`.
    pub fn install_from_git(&self, url: &str) -> Result<Vec<Value>, String> {
        let base = self
            .state_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("persona-cache");
        let dest = clone_persona_repo(url, &base)?;
        self.install_from_dir(&dest)
    }
}

/// A stable cache directory for a git URL (sanitized last path segment + short
/// hash) — mirror of `loading.py::cache_dir_for`.
fn cache_dir_for(url: &str, base: &Path) -> PathBuf {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or("persona");
    let last = last.strip_suffix(".git").unwrap_or(last);
    let slug: String = last
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let slug = if slug.is_empty() { "persona".to_string() } else { slug };
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    base.join(format!("{slug}-{}", &digest[..8]))
}

/// Clone (or reuse) a persona repo under `base` and return its directory —
/// mirror of `loading.py::clone_persona_repo` (shallow, one level).
fn clone_persona_repo(url: &str, base: &Path) -> Result<PathBuf, String> {
    let dest = cache_dir_for(url, base);
    if dest.is_dir() {
        return Ok(dest);
    }
    let _ = std::fs::create_dir_all(base);
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(&dest)
        .output()
        .map_err(|e| format!("git clone failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git clone failed: {}", stderr.trim()));
    }
    Ok(dest)
}

impl PersonaStore {
    pub fn install_from_dir(&self, dir: &Path) -> Result<Vec<Value>, String> {
        if !dir.is_dir() {
            return Err("not a directory".into());
        }

        let mut summaries = Vec::new();
        let md_files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                .collect(),
            Err(_) => return Err("cannot read directory".into()),
        };

        if md_files.is_empty() {
            return Err("no persona manifests (*.md) found".into());
        }

        for md in &md_files {
            let entry = self
                .parse_manifest_file(md, false)
                .map_err(|e| format!("{}: {e}", md.display()))?;

            // Snapshot manifest to installed dir
            let snap_dir = self.installed_dir.join(&entry.id);
            let _ = std::fs::create_dir_all(&snap_dir);
            let _ = std::fs::copy(md, snap_dir.join("manifest.md"));

            let mut s = self.state.write().unwrap();
            // Update manifest source from the snapshot (so it's self-contained)
            let snap_manifest = snap_dir.join("manifest.md");
            match self.parse_manifest_file(&snap_manifest, false) {
                Ok(installed) => {
                    s.entries.insert(entry.id.clone(), installed);
                    s.enabled.insert(entry.id.clone(), false); // pending consent
                    s.surfaced.insert(entry.id.clone(), false);
                }
                Err(e) => {
                    drop(s);
                    return Err(format!("installed manifest parse error: {e}"));
                }
            }
            drop(s);

            summaries.push(json!({
                "id": entry.id,
                "name": entry.name,
                "icon": entry.icon,
                "tagline": entry.tagline,
                "tools": entry.tools,
                "recommended_models": entry.recommended_models,
            }));
        }
        self.save_state();
        Ok(summaries)
    }
}

fn write_persona_zip(manifest: &Path, skills_dir: &Path, zip_path: &Path) -> Result<(), String> {
    use std::io::Write;
    let file = std::fs::File::create(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.md", opts)
        .map_err(|e| e.to_string())?;
    let bytes = std::fs::read(manifest).map_err(|e| e.to_string())?;
    zip.write_all(&bytes).map_err(|e| e.to_string())?;
    if skills_dir.is_dir() {
        add_dir_to_zip(&mut zip, skills_dir, Path::new("skills"), opts)?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn add_dir_to_zip(
    zip: &mut zip::ZipWriter<std::fs::File>,
    dir: &Path,
    prefix: &Path,
    opts: zip::write::FileOptions<'_, ()>,
) -> Result<(), String> {
    use std::io::Write;
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = prefix.join(entry.file_name());
        if path.is_dir() {
            add_dir_to_zip(zip, &path, &name, opts)?;
        } else if path.is_file() {
            zip.start_file(name.to_string_lossy(), opts)
                .map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            zip.write_all(&bytes).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
