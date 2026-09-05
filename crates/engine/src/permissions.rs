//! Permission engine — decides allow / deny / ask-user for each proposed tool call.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Permission mode — how aggressively to gate tool calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// Read-only conversation: no edits, no planning workflow.
    Discuss,
    /// Read-only + the planning contract (explore → propose_plan → execute).
    Plan,
    /// Ask for approval on writes/shell (default).
    #[default]
    Interactive,
    /// Full access (bypass approvals; legacy spelling `"auto"`).
    Auto,
    /// Interactive, but an LLM reviewer may turn `needs_user` into allow first.
    AutoApprove,
    /// Interactive + auto-allow the configured auto_allow tools.
    Custom,
}

impl Mode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "discuss" => Mode::Discuss,
            "plan" => Mode::Plan,
            "interactive" => Mode::Interactive,
            "auto" | "bypass-approvals" => Mode::Auto,
            "auto-approve" => Mode::AutoApprove,
            "custom" => Mode::Custom,
            _ => Mode::Interactive,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Discuss => "discuss",
            Mode::Plan => "plan",
            Mode::Interactive => "interactive",
            Mode::Auto => "auto",
            Mode::AutoApprove => "auto-approve",
            Mode::Custom => "custom",
        }
    }
}

/// Risk classification for a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RiskClass {
    /// Reads — always allowed.
    Read,
    /// Network egress — request itself can carry data off-machine (OPE-111).
    Egress,
    /// Write to local filesystem.
    WriteLocal,
    /// External side effects (connectors, MCP, messaging).
    External,
    /// Shell / exec.
    Exec,
}

impl RiskClass {
    pub fn is_consequential(&self) -> bool {
        !matches!(self, RiskClass::Read)
    }

    /// Strictness rank for override-tightening (OPE-136): higher = stricter.
    /// Overrides may only tighten (or match) a floored base class.
    pub fn strictness(self) -> u8 {
        match self {
            RiskClass::Read => 0,
            RiskClass::Egress => 1,
            RiskClass::External => 2,
            RiskClass::WriteLocal | RiskClass::Exec => 3,
        }
    }
}

/// The result of evaluating a tool call against the permission engine.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    /// True if the call is allowed.
    pub allowed: bool,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// True → the surface should prompt the user for approval.
    pub needs_user: bool,
    /// Set when a standing rule allowed the call ("tool → target"), so the engine
    /// can audit the exact rule and the tool card can surface it.
    pub rule: String,
}

impl Decision {
    pub fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
            needs_user: false,
            rule: String::new(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            needs_user: false,
            rule: String::new(),
        }
    }

    pub fn ask(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            needs_user: true,
            rule: String::new(),
        }
    }
}

const WRITE_TOOLS: &[&str] = &[
    "write_file",
    "replace_in_file",
    "apply_patch",
    "apply_unified_diff",
    "edit_file",
    "create_directory",
    "move_file",
    "delete_file",
    "delete_directory",
];

const EGRESS_TOOLS: &[&str] = &[
    "web_fetch",
    "web_search",
    "browser_open_url",
    "apollo_enrich_person",
    "apollo_enrich_company",
    "apollo_search_people",
    "hunter_domain_search",
    "hunter_find_email",
    "hunter_verify_email",
];

/// Classify a tool call's risk by name + optional metadata (mirrors Python `risk.classify`).
///
/// OPE-136: third-party MCP tools (`category == "mcp"`, or bare `mcp__*` with no metadata)
/// are floored to EXTERNAL; overrides may only tighten, never loosen a floored base.
pub fn classify_risk(tool_name: &str) -> RiskClass {
    classify_risk_with(tool_name, None, None)
}

/// Full classification with metadata + optional override resolver.
pub fn classify_risk_with(
    tool_name: &str,
    metadata: Option<&serde_json::Value>,
    overrides: Option<&dyn Fn(&str) -> Option<RiskClass>>,
) -> RiskClass {
    let base = base_risk(tool_name).or_else(|| mcp_floor(tool_name, metadata));

    if let Some(resolver) = overrides {
        if let Some(ov) = resolver(tool_name) {
            match base {
                None => return ov,
                Some(b) if ov.strictness() >= b.strictness() => return ov,
                Some(_) => {} // Loosening override on a floored tool is ignored.
            }
        }
    }
    if let Some(b) = base {
        return b;
    }
    let requires_approval = metadata
        .and_then(|m| m.get("requires_approval"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if requires_approval {
        return RiskClass::External;
    }
    RiskClass::Read
}

fn base_risk(tool_name: &str) -> Option<RiskClass> {
    if matches!(tool_name, "run_shell" | "shell" | "bash" | "zsh") {
        return Some(RiskClass::Exec);
    }
    if WRITE_TOOLS.contains(&tool_name) {
        return Some(RiskClass::WriteLocal);
    }
    if EGRESS_TOOLS.contains(&tool_name) {
        return Some(RiskClass::Egress);
    }
    None
}

/// OPE-136 MCP floor: third-party MCP tools are always EXTERNAL.
fn mcp_floor(tool_name: &str, metadata: Option<&serde_json::Value>) -> Option<RiskClass> {
    let category = metadata
        .and_then(|m| m.get("category"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if category == "mcp" {
        return Some(RiskClass::External);
    }
    // Bare name without registration sticker fails closed.
    if metadata.is_none() && tool_name.starts_with("mcp__") {
        return Some(RiskClass::External);
    }
    None
}

const READ_ONLY_MODES: [Mode; 2] = [Mode::Discuss, Mode::Plan];

const SHELL_OPERATORS: &[&str] = &[";", "&", "|", ">", "<", "`", "$(", "(", "\n", "\r"];

fn has_shell_operators(command: &str) -> bool {
    SHELL_OPERATORS.iter().any(|op| command.contains(op))
}

/// Task-scoped standing rule: `{ tool_name → { allowed_targets } }`.
pub type TaskRules = HashMap<String, HashSet<String>>;

/// One resolved workspace root.
#[derive(Clone)]
pub struct ResolvedRoot {
    pub path: std::path::PathBuf,
    pub writable: bool,
}

/// The permission engine.
/// Wrapped in a std::sync::Mutex; the mutex guard is held briefly and released before
/// any async operations. PermissionEngine is Send+Sync because all its fields are.
#[derive(Clone)]
pub struct PermissionEngine {
    workspace_root: std::path::PathBuf,
    mode: Mode,
    allowed_commands: Vec<String>,
    auto_allow_tools: HashSet<String>,
    session_allow_tools: HashSet<String>,
    session_allow_commands: HashSet<String>,
    /// OPE-136 run grants ("Allow for this request"): in-memory, cleared at run boundary.
    run_allow_tools: HashSet<String>,
    /// Durable MCP trust rules (tool name → trusted). Waives card only, not the class.
    trust_tools: HashSet<String>,
    task_rules: TaskRules,
    roots: Vec<ResolvedRoot>,
}

// SAFETY: PermissionEngine only contains Send+Sync fields and no interior mutability
// beyond the std::sync::Mutex that wraps it. The HashMap/HashSet use standard
// hasher which is Send+Sync. PathBuf is Send+Sync. This allows it to be used
// inside Arc<std::sync::Mutex<PermissionEngine>> which is what the engine uses.
unsafe impl Send for PermissionEngine {}
unsafe impl Sync for PermissionEngine {}

impl PermissionEngine {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        let roots = vec![ResolvedRoot {
            path: workspace_root.clone(),
            writable: true,
        }];
        Self {
            workspace_root,
            mode: Mode::Interactive,
            allowed_commands: Vec::new(),
            auto_allow_tools: HashSet::new(),
            session_allow_tools: HashSet::new(),
            session_allow_commands: HashSet::new(),
            run_allow_tools: HashSet::new(),
            trust_tools: HashSet::new(),
            task_rules: HashMap::new(),
            roots,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_allowed_commands(&mut self, commands: Vec<String>) {
        self.allowed_commands = commands;
    }

    pub fn set_auto_allow_tools(&mut self, tools: HashSet<String>) {
        self.auto_allow_tools = tools;
    }

    pub fn set_roots(&mut self, roots: Vec<ResolvedRoot>) {
        self.roots = roots;
    }

    pub fn set_task_rules(&mut self, rules: TaskRules) {
        self.task_rules = rules;
    }

    /// Mint a single standing rule into the live engine's task rules (used by
    /// the "Allow every time" flow, §25). Existing `set_task_rules` remains for
    /// full-task seeding.
    pub fn add_task_rule(&mut self, tool: String, target: String) {
        self.task_rules.entry(tool).or_default().insert(target);
    }

    /// Allow a tool for this session.
    pub fn allow_tool_for_session(&mut self, tool_name: String) {
        self.session_allow_tools.insert(tool_name);
    }

    /// OPE-136: allow a tool for the remainder of the current run only.
    pub fn allow_tool_for_run(&mut self, tool_name: String) {
        self.run_allow_tools.insert(tool_name);
    }

    /// Clear run-scoped grants (call at run start/end).
    pub fn clear_run_grants(&mut self) {
        self.run_allow_tools.clear();
    }

    /// OPE-136 durable trust: mark an MCP tool as "don't ask" (card waiver only).
    pub fn trust_tool(&mut self, tool_name: String) {
        self.trust_tools.insert(tool_name);
    }

    pub fn set_trust_tools(&mut self, tools: HashSet<String>) {
        self.trust_tools = tools;
    }

    /// Allow a command for this session.
    pub fn allow_command_for_session(&mut self, command: String) {
        if !command.is_empty() {
            self.session_allow_commands.insert(command);
        }
    }

    /// Return current session grants as a JSON object — tools + commands.
    pub fn grants(&self) -> serde_json::Value {
        serde_json::json!({
            "tools": self.session_allow_tools.iter().collect::<Vec<_>>(),
            "commands": self.session_allow_commands.iter().collect::<Vec<_>>(),
        })
    }

    /// Evaluate whether a tool call may proceed.
    ///
    /// `metadata` is an optional dict-like object from the tool registry; the Rust
    /// fallback only reads the `risk_level` and `category` fields.
    pub fn evaluate(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        metadata: Option<&serde_json::Value>,
    ) -> Decision {
        let empty = serde_json::Map::new();
        let args_map: &serde_json::Map<String, serde_json::Value> =
            arguments.as_object().unwrap_or(&empty);

        let category = metadata
            .and_then(|m| m.get("category"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_connector = category == "connector";
        let is_mcp = category == "mcp";

        let risk = classify_risk_with(tool_name, metadata, None);

        let is_write = risk == RiskClass::WriteLocal;
        let is_shell = risk == RiskClass::Exec;
        let consequential = risk.is_consequential();

        if READ_ONLY_MODES.contains(&self.mode) && consequential {
            return Decision::deny(format!("{} mode is read-only", self.mode.as_str()));
        }

        if is_write {
            if let Some(path_val) = args_map.get("path").or_else(|| args_map.get("file_path")) {
                let path_str = path_val.as_str().unwrap_or("");
                if !path_str.is_empty() && !self.is_under_writable_root(path_str) {
                    return Decision::deny(format!(
                        "path is not in a writable directory: {path_str}"
                    ));
                }
            }
        }

        if !consequential {
            return Decision::allow("low risk");
        }

        if self.mode == Mode::Auto {
            return Decision::allow("full access");
        }

        if is_shell {
            if let Some(cmd_val) = args_map.get("command") {
                let cmd = cmd_val.as_str().unwrap_or("");
                if !cmd.is_empty() {
                    if self.command_allowed(cmd) {
                        return Decision::allow("command on allowlist");
                    }
                    if self.session_allow_commands.contains(cmd) {
                        return Decision::allow("command allowed for session");
                    }
                }
            }
        }

        if !is_connector && self.session_allow_tools.contains(tool_name) {
            return Decision::allow("tool allowed for session");
        }

        // OPE-136 run grant — covers EXTERNAL/MCP retries within this run.
        if self.run_allow_tools.contains(tool_name) {
            return Decision::allow("tool allowed for this request");
        }

        // OPE-136: MCP trust waives the card only (class stays EXTERNAL).
        if is_mcp {
            if self.trust_tools.contains(tool_name) {
                return Decision::allow("trusted MCP tool (user trust rule)");
            }
            let requires_approval = metadata
                .and_then(|m| m.get("requires_approval"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !requires_approval {
                return Decision::allow("trusted MCP tool (server marked don't-ask)");
            }
        }

        if let Some(targets) = self.task_rules.get(tool_name) {
            let target = extract_target(tool_name, args_map);
            if let Some(t) = target {
                if targets.contains(&t) {
                    let rule = format!("{tool_name} → {t}");
                    return Decision {
                        allowed: true,
                        reason: format!("allowed by standing rule: {rule}"),
                        needs_user: false,
                        rule,
                    };
                }
            }
        }

        if self.mode == Mode::Custom && self.auto_allow_tools.contains(tool_name) {
            return Decision::allow("auto-allowed by config");
        }

        Decision::ask("requires approval")
    }

    fn candidate_path(&self, path: &str) -> std::path::PathBuf {
        let p = if path.starts_with('~') {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home)
                .join(path.trim_start_matches("~").trim_start_matches('/'))
        } else {
            std::path::PathBuf::from(path)
        };
        if p.is_absolute() {
            p
        } else {
            self.workspace_root.join(p)
        }
    }

    fn is_under_root(&self, path: &str, writable: bool) -> bool {
        let candidate = self.candidate_path(path);
        for rp in &self.roots {
            if writable && !rp.writable {
                continue;
            }
            if candidate.starts_with(&rp.path) {
                return true;
            }
        }
        false
    }

    fn is_under_writable_root(&self, path: &str) -> bool {
        self.is_under_root(path, true)
    }

    fn command_allowed(&self, command: &str) -> bool {
        if has_shell_operators(command) {
            return false;
        }
        // Parse into argv for prefix matching
        if let Ok(argv) = shlex::split(command) {
            if argv.is_empty() {
                return false;
            }
            for allowed in &self.allowed_commands {
                if let Ok(prefix) = shlex::split(allowed) {
                    if !prefix.is_empty()
                        && argv.len() >= prefix.len()
                        && argv[..prefix.len()] == *prefix
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Extract the standing-rule-eligible target from a tool call's arguments, if any.
fn extract_target(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let key = if tool_name == "send_message"
        || tool_name.starts_with("mcp__")
        || tool_name.starts_with("connector__")
    {
        "target"
    } else if tool_name == "web_search" || tool_name == "web_fetch" {
        "url"
    } else {
        return None;
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The declared standing-rule target argument name for a tool, mirroring Python's
/// `TARGET_ARGS` (`coworker/connectors/tool_defs.py`): only tools that declare a
/// single "target" binding are eligible for task-scoped standing rules. Web tools
/// do not declare one (Python's candidate table excludes them).
pub fn target_arg_for(tool_name: &str) -> Option<&'static str> {
    if tool_name == "send_message"
        || tool_name.starts_with("mcp__")
        || tool_name.starts_with("connector__")
    {
        Some("target")
    } else {
        None
    }
}

/// The target value iff this call is eligible for a task-scoped standing rule
/// (UX-DECISIONS §25): external-risk only (never exec/write-local — shell asks
/// forever), the tool must declare a target argument, and the call must actually
/// name a target. Returns None otherwise — ineligible calls keep parking
/// approvals as today. Mirrors Python's `standing_rule_candidate`
/// (`coworker/permissions.py`).
pub fn standing_target_candidate(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    standing_target_candidate_with(tool_name, args, None)
}

/// Like [`standing_target_candidate`], but with tool metadata so `send_message`
/// (requires_approval) and MCP tools classify as EXTERNAL correctly.
pub fn standing_target_candidate_with(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    if classify_risk_with(tool_name, metadata, None) != RiskClass::External {
        return None;
    }
    let arg = target_arg_for(tool_name)?;
    let value = args
        .get(arg)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

// shlex for Rust
mod shlex {
    pub fn split(s: &str) -> Result<Vec<String>, std::io::Error> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        let mut quote_char = ' ';
        let mut escaped = false;

        for ch in s.chars() {
            if escaped {
                current.push(ch);
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if in_quote {
                if ch == quote_char {
                    in_quote = false;
                } else {
                    current.push(ch);
                }
            } else if ch == '"' || ch == '\'' {
                in_quote = true;
                quote_char = ch;
            } else if ch.is_whitespace() {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            } else {
                current.push(ch);
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn target_arg_for_matches_python_target_args() {
        assert_eq!(target_arg_for("send_message"), Some("target"));
        assert_eq!(target_arg_for("connector__slack"), Some("target"));
        assert_eq!(target_arg_for("mcp__filesystem"), Some("target"));
        // web tools are not in TARGET_ARGS.
        assert_eq!(target_arg_for("web_search"), None);
        assert_eq!(target_arg_for("read_file"), None);
    }

    #[test]
    fn standing_target_candidate_rules() {
        // MCP tools are floored EXTERNAL → eligible when they name a target.
        assert_eq!(
            standing_target_candidate("mcp__x", &args(&[("target", "t")])),
            Some("t".to_string())
        );
        assert_eq!(
            standing_target_candidate("connector__slack", &args(&[("target", "#general")])),
            None // connector__* without metadata is READ, not EXTERNAL
        );
        // Write-local / exec / egress never mint standing rules.
        assert_eq!(
            standing_target_candidate("write_file", &args(&[("path", "/tmp/a")])),
            None
        );
        assert_eq!(
            standing_target_candidate("shell", &args(&[("command", "ls")])),
            None
        );
        assert_eq!(
            standing_target_candidate("web_search", &args(&[("query", "x")])),
            None
        );
        // Bare send_message without EXTERNAL metadata → not eligible.
        assert_eq!(
            standing_target_candidate("send_message", &args(&[("target", "alice")])),
            None
        );
    }

    #[test]
    fn mcp_floor_and_egress_classification() {
        assert_eq!(classify_risk("web_fetch"), RiskClass::Egress);
        assert_eq!(classify_risk("web_search"), RiskClass::Egress);
        assert_eq!(classify_risk("run_shell"), RiskClass::Exec);
        assert_eq!(classify_risk("write_file"), RiskClass::WriteLocal);
        // Bare mcp__* fails closed to EXTERNAL.
        assert_eq!(classify_risk("mcp__notion__get_page"), RiskClass::External);
        // category=mcp floors even with requires_approval:false.
        let meta = json!({"category": "mcp", "requires_approval": false});
        assert_eq!(
            classify_risk_with("mcp__custom__read", Some(&meta), None),
            RiskClass::External
        );
        // category=connector is NOT floored by MCP rule (catalog knowledge).
        let connector = json!({"category": "connector", "requires_approval": false});
        assert_eq!(
            classify_risk_with("mcp__jira__getJiraIssue", Some(&connector), None),
            RiskClass::Read
        );
    }

    #[test]
    fn override_cannot_loosen_mcp_floor() {
        let meta = json!({"category": "mcp"});
        let loosen = |_name: &str| Some(RiskClass::Read);
        assert_eq!(
            classify_risk_with("mcp__x__y", Some(&meta), Some(&loosen)),
            RiskClass::External
        );
        let tighten = |_name: &str| Some(RiskClass::Exec);
        assert_eq!(
            classify_risk_with("mcp__x__y", Some(&meta), Some(&tighten)),
            RiskClass::Exec
        );
    }

    #[test]
    fn run_grant_and_trust_auto_allow() {
        let mut engine = PermissionEngine::new(std::path::PathBuf::from("/tmp"));
        let meta = json!({"category": "mcp", "requires_approval": true});
        let d = engine.evaluate("mcp__x__y", &json!({"a": 1}), Some(&meta));
        assert!(!d.allowed && d.needs_user);

        engine.allow_tool_for_run("mcp__x__y".to_string());
        let d = engine.evaluate("mcp__x__y", &json!({"a": 1}), Some(&meta));
        assert!(d.allowed, "run grant should allow: {d:?}");

        engine.clear_run_grants();
        engine.trust_tool("mcp__x__y".to_string());
        let d = engine.evaluate("mcp__x__y", &json!({"a": 1}), Some(&meta));
        assert!(d.allowed, "trust should waive card: {d:?}");
    }

    #[test]
    fn add_task_rule_auto_allows_next_call() {
        let mut engine = PermissionEngine::new(std::path::PathBuf::from("/tmp"));
        engine.add_task_rule("mcp__svc__act".to_string(), "alice".to_string());
        let meta = json!({"category": "mcp"});
        let d = engine.evaluate(
            "mcp__svc__act",
            &json!({"target": "alice"}),
            Some(&meta),
        );
        assert!(d.allowed, "expected rule hit, got {d:?}");
        assert_eq!(d.rule, "mcp__svc__act → alice");
        let d = engine.evaluate("mcp__svc__act", &json!({"target": "bob"}), Some(&meta));
        assert!(!d.allowed && d.needs_user, "expected ask, got {d:?}");
    }
}
