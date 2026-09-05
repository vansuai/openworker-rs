//! Persona manifest — parse + validate a persona definition (mirror of
//! `coworker/personas/manifest.py`).
//!
//! Format: YAML frontmatter (identity + capability declaration) followed by a
//! markdown body that is the system prompt. `persona ⊇ skill` — the same
//! frontmatter-markdown shape as SKILL.md, with more structured fields.
//! Parsing is strict: an invalid manifest returns an error rather than silently
//! producing a broken persona.

use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Constants (mirror of manifest.py)
// ---------------------------------------------------------------------------

pub const VALID_FAMILIES: [&str; 2] = ["code", "knowledge"];
pub const VALID_WORKSPACES: [&str; 4] = ["git", "project", "deliverable", "none"];
pub const VALID_MODES: [&str; 5] = ["discuss", "plan", "interactive", "custom", "auto"];
pub const VALID_REC_KINDS: [&str; 2] = ["connector", "mcp"];
pub const VALID_REC_TIERS: [&str; 2] = ["core", "optional"];
pub const VALID_TEAM: [&str; 2] = ["lead", "worker"];
/// Known tool capability ids (mirror of `coworker/catalog.py` `CATALOG`).
pub const KNOWN_TOOLS: [&str; 6] = ["code_files", "files", "git", "search", "shell", "todo"];

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Recommendation {
    /// "connector" | "mcp"
    pub kind: String,
    /// A connector id or an MCP server name.
    #[serde(rename = "ref")]
    pub r#ref: String,
    #[serde(default)]
    pub reason: String,
    /// "core" | "optional"
    #[serde(default = "rec_tier_default")]
    pub tier: String,
}

fn rec_tier_default() -> String {
    "optional".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonaManifest {
    pub id: String,
    pub name: String,
    pub system_prompt: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    /// "code" | "knowledge"
    #[serde(default = "family_default")]
    pub family: String,
    /// Derived from family (code → "git", knowledge → "deliverable").
    #[serde(default = "workspace_default")]
    pub workspace: String,
    #[serde(default)]
    pub messaging: bool,
    #[serde(default)]
    pub connectors: bool,
    #[serde(default = "mode_default")]
    pub default_permission_mode: String,
    #[serde(default)]
    pub recommended_models: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub recommends: Vec<Recommendation>,
    /// Team trait: `"lead"` | `"worker"` | None (solo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default)]
    pub builtin: bool,
    /// Where it was loaded from (path / url), for provenance.
    #[serde(default)]
    pub source: Option<String>,
}

fn family_default() -> String {
    "knowledge".into()
}

fn workspace_default() -> String {
    "deliverable".into()
}

fn mode_default() -> String {
    "interactive".into()
}

impl PersonaManifest {
    pub fn needs_workspace(&self) -> bool {
        self.workspace != "none"
    }
}

// ---------------------------------------------------------------------------
// Frontmatter splitting (mirror of `_split_frontmatter`)
// ---------------------------------------------------------------------------

fn split_frontmatter(text: &str) -> Result<(serde_yaml::Mapping, String), String> {
    if !text.starts_with("---") {
        return Err("manifest must start with a YAML frontmatter block (---)".into());
    }
    let rest = &text[3..];
    let end = rest
        .find("\n---")
        .ok_or("unterminated frontmatter block (missing closing ---)")?;
    let raw = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n').to_string();

    let value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
    let map = value
        .as_mapping()
        .cloned()
        .ok_or("frontmatter must be a mapping of key: value")?;
    Ok((map, body))
}

// ---------------------------------------------------------------------------
// Helpers (mirror of `_slugify`, `_strlist`, `_recommends`)
// ---------------------------------------------------------------------------

/// Persona ids become directory names, so they are restricted to a
/// filesystem-safe slug: no path separators or `..`, no `:*?"<>|`, bounded
/// length.
fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    let mut count = 1;
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return false;
        }
        count += 1;
        if count > 64 {
            return false;
        }
    }
    true
}

/// Normalize a filename stem into the persona-id charset (used only for ids
/// derived from filenames; explicit `id:` values must already be valid).
fn slugify(stem: &str) -> String {
    let lowered = stem.trim().to_lowercase();
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            slug.push(c);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches(|c| c == '-' || c == '_').to_string();
    let truncated: String = trimmed.chars().take(64).collect();
    if is_valid_id(&truncated) {
        truncated
    } else {
        String::new()
    }
}

/// Parse a value as a string list: a comma-separated string or a YAML list.
fn strlist(meta: &serde_yaml::Mapping, key: &str) -> Result<Vec<String>, String> {
    let Some(val) = meta.get(serde_yaml::Value::String(key.to_string())) else {
        return Ok(vec![]);
    };
    if val.is_null() {
        return Ok(vec![]);
    }
    if let Some(s) = val.as_str() {
        return Ok(s
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect());
    }
    if let Some(arr) = val.as_sequence() {
        let mut out = Vec::new();
        for v in arr {
            if let Some(s) = v.as_str() {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            } else {
                return Err(format!("`{key}` must be a list or comma-separated string"));
            }
        }
        return Ok(out);
    }
    Err(format!("`{key}` must be a list or comma-separated string"))
}

fn meta_str(meta: &serde_yaml::Mapping, key: &str) -> String {
    meta.get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn meta_bool(meta: &serde_yaml::Mapping, key: &str) -> bool {
    match meta.get(serde_yaml::Value::String(key.to_string())) {
        Some(serde_yaml::Value::Bool(b)) => *b,
        // Python `bool(meta.get(...))` treats any non-empty string as truthy.
        Some(serde_yaml::Value::String(s)) => !s.trim().is_empty(),
        Some(serde_yaml::Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or(false),
        _ => false,
    }
}

fn parse_recommends(
    persona_id: &str,
    meta: &serde_yaml::Mapping,
) -> Result<Vec<Recommendation>, String> {
    let Some(raw) = meta.get(serde_yaml::Value::String("recommends".into())) else {
        return Ok(vec![]);
    };
    let Some(items) = raw.as_sequence() else {
        return Err(format!(
            "persona {persona_id:?}: `recommends` must be a list"
        ));
    };
    let mut out = Vec::new();
    for item in items {
        let Some(m) = item.as_mapping() else {
            return Err(format!(
                "persona {persona_id:?}: each `recommends` item must be a mapping"
            ));
        };
        let (kind, ref_val) = if m.contains_key(serde_yaml::Value::String("connector".into())) {
            (
                "connector",
                meta_str(m, "connector"),
            )
        } else if m.contains_key(serde_yaml::Value::String("mcp".into())) {
            (
                "mcp",
                meta_str(m, "mcp"),
            )
        } else {
            return Err(format!(
                "persona {persona_id:?}: each `recommends` item needs a `connector:` or `mcp:` key"
            ));
        };
        if ref_val.is_empty() {
            return Err(format!(
                "persona {persona_id:?}: a `recommends` item has an empty {kind}"
            ));
        }
        let tier = meta_str(m, "tier").to_lowercase();
        if !VALID_REC_TIERS.contains(&tier.as_str()) {
            return Err(format!(
                "persona {persona_id:?}: recommend tier must be one of [core, optional]"
            ));
        }
        out.push(Recommendation {
            kind: kind.to_string(),
            r#ref: ref_val,
            reason: meta_str(m, "reason"),
            tier,
        });
    }
    Ok(out)
}

fn validate_tools(persona_id: &str, tools: &[String]) -> Result<(), String> {
    let unknown: Vec<&String> = tools
        .iter()
        .filter(|t| !KNOWN_TOOLS.contains(&t.as_str()))
        .collect();
    if !unknown.is_empty() {
        let list = unknown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "persona {persona_id:?} references unknown tool capabilities: {list}. \
             Known: code_files, files, git, search, shell, todo"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API (mirror of `parse_manifest` / `load_manifest_file`)
// ---------------------------------------------------------------------------

pub fn parse_manifest(
    text: &str,
    fallback_id: Option<&str>,
    builtin: bool,
    source: Option<&str>,
) -> Result<PersonaManifest, String> {
    let (meta, body) = split_frontmatter(text)?;

    let explicit_id = meta_str(&meta, "id");
    let persona_id = if !explicit_id.is_empty() {
        if !is_valid_id(&explicit_id) {
            return Err(format!(
                "persona {explicit_id:?} is invalid: lowercase letters, digits, '-' or '_' \
                 only, starting with a letter/digit, max 64 chars (ids become directory names)"
            ));
        }
        explicit_id
    } else {
        // Derived from the filename: normalize it into the id charset instead
        // of erroring, so `My Persona.md` without an explicit id still installs
        // (as `my-persona`).
        let derived = slugify(fallback_id.unwrap_or(""));
        if derived.is_empty() {
            return Err("manifest needs an `id` (or a filename to derive one from)".into());
        }
        derived
    };

    if body.trim().is_empty() {
        return Err(format!(
            "persona {persona_id:?} has no body (the system prompt)"
        ));
    }

    let family_raw = meta_str(&meta, "family");
    let family = if family_raw.is_empty() {
        "knowledge".to_string()
    } else {
        family_raw.to_lowercase()
    };
    if !VALID_FAMILIES.contains(&family.as_str()) {
        return Err(format!(
            "persona {persona_id:?}: family must be one of [code, knowledge]"
        ));
    }

    // The workspace enum collapsed into family (owner decision 2026-07-03,
    // UX-DECISIONS §16): knowledge → transparent scratch + user-added roots;
    // code → an explicit directory picked by the user. The manifest key is
    // still accepted — and typo-checked — so older manifests parse, but it no
    // longer drives behavior.
    let declared = meta_str(&meta, "workspace").to_lowercase();
    if !declared.is_empty() && !VALID_WORKSPACES.contains(&declared.as_str()) {
        return Err(format!(
            "persona {persona_id:?}: workspace must be one of [git, project, deliverable, none]"
        ));
    }
    let workspace = if family == "code" { "git" } else { "deliverable" };

    let mode_raw = meta_str(&meta, "default_permission_mode");
    let mode = if mode_raw.is_empty() {
        "interactive".to_string()
    } else {
        mode_raw.to_lowercase()
    };
    if !VALID_MODES.contains(&mode.as_str()) {
        return Err(format!(
            "persona {persona_id:?}: default_permission_mode must be one of \
             [auto, custom, discuss, interactive, plan]"
        ));
    }

    let tools = strlist(&meta, "tools")?;
    validate_tools(&persona_id, &tools)?;

    let team_raw = meta_str(&meta, "team").to_lowercase();
    let team = if team_raw.is_empty() {
        None
    } else if VALID_TEAM.contains(&team_raw.as_str()) {
        Some(team_raw)
    } else {
        return Err(format!(
            "persona {persona_id:?}: team must be one of [lead, worker] (omit for a solo coworker)"
        ));
    };

    let name = meta_str(&meta, "name");
    let name = if name.is_empty() { persona_id.clone() } else { name };

    let recommends = parse_recommends(&persona_id, &meta)?;
    Ok(PersonaManifest {
        id: persona_id,
        name,
        system_prompt: body.trim().to_string(),
        icon: meta_str(&meta, "icon"),
        tagline: meta_str(&meta, "tagline"),
        description: meta_str(&meta, "description"),
        tools,
        family,
        workspace: workspace.to_string(),
        messaging: meta_bool(&meta, "messaging"),
        connectors: meta_bool(&meta, "connectors"),
        default_permission_mode: mode,
        recommended_models: strlist(&meta, "recommended_models")?,
        skills: strlist(&meta, "skills")?,
        mcp: strlist(&meta, "mcp")?,
        recommends,
        team,
        builtin,
        source: source.map(|s| s.to_string()),
    })
}

pub fn load_manifest_file(path: &Path, builtin: bool) -> Result<PersonaManifest, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read error: {e}"))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    parse_manifest(&text, Some(&stem), builtin, Some(&path.display().to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const OPS_MD: &str = r#"---
id: ops
name: Ops Coworker
icon: wrench
tagline: Operate and investigate — runbooks, logs, infrastructure
family: knowledge
tools: [files, search, shell, todo]
messaging: true
connectors: true
recommended_models: [anthropic:claude-opus-4-8, openai:gpt-5.5]
default_permission_mode: interactive
description: An operations-focused coworker.
recommends:
  - connector: github
    reason: confirm deploys
    tier: core
  - connector: slack
    reason: receive alerts
    tier: core
  - mcp: filesystem
    reason: read runbooks
    tier: optional
---
You are the Ops Coworker.
"#;

    #[test]
    fn parses_full_manifest() {
        let m = parse_manifest(OPS_MD, None, false, Some("ops.md")).unwrap();
        assert_eq!(m.id, "ops");
        assert_eq!(m.name, "Ops Coworker");
        assert_eq!(m.icon, "wrench");
        assert_eq!(m.system_prompt, "You are the Ops Coworker.");
        assert_eq!(m.family, "knowledge");
        assert_eq!(m.workspace, "deliverable");
        assert!(m.messaging);
        assert!(m.connectors);
        assert_eq!(m.tools, vec!["files", "search", "shell", "todo"]);
        assert_eq!(
            m.recommended_models,
            vec!["anthropic:claude-opus-4-8", "openai:gpt-5.5"]
        );
        assert_eq!(m.default_permission_mode, "interactive");
        assert_eq!(m.recommends.len(), 3);
        assert_eq!(m.recommends[0].kind, "connector");
        assert_eq!(m.recommends[0].r#ref, "github");
        assert_eq!(m.recommends[0].tier, "core");
        assert_eq!(m.recommends[2].kind, "mcp");
        assert_eq!(m.recommends[2].r#ref, "filesystem");
        assert_eq!(m.recommends[2].tier, "optional");
        assert!(!m.builtin);
        assert_eq!(m.source.as_deref(), Some("ops.md"));
        assert!(m.needs_workspace());
    }

    #[test]
    fn code_family_derives_git_workspace() {
        let md = "---\nid: dev\nfamily: code\ntools: [files]\n---\nWork in code.\n";
        let m = parse_manifest(md, None, false, None).unwrap();
        assert_eq!(m.workspace, "git");
    }

    #[test]
    fn derives_id_from_filename_stem() {
        let md = "---\nname: My Persona\ntools: [search]\n---\nBody here.\n";
        let m = parse_manifest(md, Some("My Persona"), false, None).unwrap();
        assert_eq!(m.id, "my-persona");
        assert_eq!(m.name, "My Persona");
    }

    #[test]
    fn rejects_invalid_id() {
        let md = "---\nid: Bad/ID!\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: ../../etc\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
    }

    #[test]
    fn rejects_missing_frontmatter_or_body() {
        assert!(parse_manifest("no frontmatter", None, false, None).is_err());
        let md = "---\nid: x\n---\n   \n";
        assert!(parse_manifest(md, None, false, None).is_err());
    }

    #[test]
    fn rejects_bad_family_workspace_mode() {
        let md = "---\nid: x\nfamily: magic\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: x\nworkspace: nowhere\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: x\ndefault_permission_mode: bossy\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
    }

    #[test]
    fn rejects_unknown_tools() {
        let md = "---\nid: x\ntools: [files, time_machine]\n---\nBody.\n";
        let err = parse_manifest(md, None, false, None).unwrap_err();
        assert!(err.contains("time_machine"), "err: {err}");
    }

    #[test]
    fn rejects_bad_recommends() {
        let md = "---\nid: x\nrecommends:\n  - tier: core\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: x\nrecommends:\n  - connector: \"\"\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: x\nrecommends:\n  - connector: github\n    tier: gold\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
        let md = "---\nid: x\nrecommends: nope\n---\nBody.\n";
        assert!(parse_manifest(md, None, false, None).is_err());
    }

    #[test]
    fn accepts_comma_string_lists_and_quotes() {
        let md = "---\nid: x\ntools: files, search\nrecommended_models: \"a:1, b:2\"\n---\nBody.\n";
        let m = parse_manifest(md, None, false, None).unwrap();
        assert_eq!(m.tools, vec!["files", "search"]);
        assert_eq!(m.recommended_models, vec!["a:1", "b:2"]);
    }

    #[test]
    fn slugify_edge_cases() {
        assert_eq!(slugify("My Persona"), "my-persona");
        assert_eq!(slugify("---Hello World---"), "hello-world");
        assert_eq!(slugify("..."), "");
        assert_eq!(slugify("COW"), "cow");
    }

    #[test]
    fn yaml_bool_strings_treated_truthy() {
        let md = "---\nid: x\nmessaging: \"yes\"\n---\nBody.\n";
        let m = parse_manifest(md, None, false, None).unwrap();
        assert!(m.messaging);
        let md = "---\nid: x\nmessaging: false\n---\nBody.\n";
        let m = parse_manifest(md, None, false, None).unwrap();
        assert!(!m.messaging);
    }
}
