//! Approval provenance + agent-authored file provenance (OPE-114 / OPE-136).
//!
//! Two related concerns live here so the engine can attach fixed-vocabulary facts to
//! tool messages (`_display` sidecar) and audit rows without inventing free-form text:
//!
//! 1. **Approval origins** — how a consequential call was allowed or denied
//!    (`user_approved`, `auto_approved`, `trusted_rule`, `run_grant`, `denied`, …).
//! 2. **Session files** — whether the agent itself wrote/downloaded a path this session
//!    (port of `coworker/provenance.py`), rendered as one cautious line for the reviewer.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// Approval provenance (message sidecar / audit)
// ---------------------------------------------------------------------------

/// How a tool call was allowed or denied — fixed vocabulary for `_display.approval_origin`
/// and audit rows. Mirrors the engine's cardless / card origins without free-form prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOrigin {
    /// Human clicked allow on the approval card.
    UserApproved,
    /// Standing allow / reviewer allow / bypass — ran without a fresh human click.
    AutoApproved,
    /// Trusted-MCP user rule (OPE-136).
    TrustedRule,
    /// In-run "Allow for this request" grant (OPE-136).
    RunGrant,
    /// Explicit deny (user, reviewer, or hard floor).
    Denied,
    /// Full-access / bypass mode.
    Bypass,
    /// Live safety reviewer allowed the call.
    Reviewer,
    /// Live safety reviewer denied the call.
    ReviewerDenied,
    /// Trusted-MCP server flag (mcp.json), distinct from [`Self::TrustedRule`].
    TrustedServer,
}

impl ApprovalOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserApproved => "user_approved",
            Self::AutoApproved => "auto_approved",
            Self::TrustedRule => "trusted_rule",
            Self::RunGrant => "run_grant",
            Self::Denied => "denied",
            Self::Bypass => "bypass",
            Self::Reviewer => "reviewer",
            Self::ReviewerDenied => "reviewer_denied",
            Self::TrustedServer => "trusted_server",
        }
    }

    /// Parse engine / wire labels (including the Python engine's shorter `user` form).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user_approved" | "user" => Some(Self::UserApproved),
            "auto_approved" | "auto_allowed" => Some(Self::AutoApproved),
            "trusted_rule" => Some(Self::TrustedRule),
            "run_grant" => Some(Self::RunGrant),
            "denied" => Some(Self::Denied),
            "bypass" => Some(Self::Bypass),
            "reviewer" => Some(Self::Reviewer),
            "reviewer_denied" => Some(Self::ReviewerDenied),
            "trusted_server" => Some(Self::TrustedServer),
            _ => None,
        }
    }

    /// Map a permission-engine reason string onto an origin (cardless paths).
    pub fn from_decision_reason(reason: &str) -> Option<Self> {
        if reason == "full access" {
            return Some(Self::Bypass);
        }
        if reason == "tool allowed for this request" {
            return Some(Self::RunGrant);
        }
        if reason.starts_with("trusted MCP tool") {
            return if reason.contains("user trust rule") {
                Some(Self::TrustedRule)
            } else {
                Some(Self::TrustedServer)
            };
        }
        None
    }

    /// `_display` sidecar fragment for a tool message (approval chip).
    pub fn display_sidecar(self, note: Option<&str>, grant: Option<&str>) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "approval_origin".into(),
            Value::String(self.as_str().to_string()),
        );
        if let Some(n) = note.filter(|s| !s.is_empty()) {
            m.insert("approval_note".into(), Value::String(n.to_string()));
        }
        if let Some(g) = grant.filter(|s| !s.is_empty()) {
            m.insert("approval_grant".into(), Value::String(g.to_string()));
        }
        m
    }

    /// Audit-friendly flat fields.
    pub fn audit_fields(self, note: Option<&str>, grant: Option<&str>) -> Value {
        json!({
            "origin": self.as_str(),
            "note": note.unwrap_or(""),
            "grant": grant.unwrap_or(""),
        })
    }
}

/// Attach approval provenance onto an existing tool-message `_display` object (or create one).
pub fn attach_approval_display(
    message: &mut Value,
    origin: ApprovalOrigin,
    note: Option<&str>,
    grant: Option<&str>,
) {
    let sidecar = Value::Object(origin.display_sidecar(note, grant));
    match message.get_mut("_display") {
        Some(Value::Object(existing)) => {
            if let Value::Object(extra) = sidecar {
                for (k, v) in extra {
                    existing.insert(k, v);
                }
            }
        }
        _ => {
            if let Some(obj) = message.as_object_mut() {
                obj.insert("_display".into(), sidecar);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session file provenance (port of coworker/provenance.py)
// ---------------------------------------------------------------------------

pub const WRITTEN: &str = "written";
pub const DOWNLOADED: &str = "downloaded";

const DOWNLOAD_RESULT_TOOLS: &[&str] = &[
    "github_clone",
    "github_pull",
    "email_download_attachment",
];

const WRITE_TOOLS: &[&str] = &[
    "write_file",
    "replace_in_file",
    "apply_patch",
    "apply_unified_diff",
];

const SCRIPT_SUFFIXES: &[&str] = &[
    ".py", ".sh", ".bash", ".zsh", ".js", ".mjs", ".cjs", ".ts", ".rb", ".pl", ".php",
    ".ps1", ".bat", ".cmd", ".jar", ".exe", ".json", ".yml", ".yaml", ".ini", ".toml",
    ".cfg", ".mk",
];

fn implicit_targets(program: &str) -> &'static [&'static str] {
    match program {
        "make" => &["Makefile", "makefile", "GNUmakefile"],
        "npm" | "pnpm" | "yarn" | "bun" => &["package.json"],
        "pytest" => &["conftest.py"],
        "tox" => &["tox.ini"],
        "nox" => &["noxfile.py"],
        "docker-compose" => &[
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yaml",
            "compose.yml",
        ],
        _ => &[],
    }
}

fn fetcher_output_flags(program: &str) -> Option<&'static [&'static str]> {
    match program {
        "curl" => Some(&["-o", "--output"]),
        "wget" => Some(&["-O", "--output-document"]),
        "invoke-webrequest" | "iwr" => Some(&["-outfile"]),
        _ => None,
    }
}

fn case_folded_fetcher(program: &str) -> bool {
    matches!(program, "invoke-webrequest" | "iwr")
}

/// How a path came into being this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub step: i64,
    pub kind: String, // WRITTEN | DOWNLOADED
}

/// A proposed call naming a path this session created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: String,
    pub origin: Origin,
    pub steps_ago: i64,
}

impl Match {
    pub fn downloaded(&self) -> bool {
        self.origin.kind == DOWNLOADED
    }

    /// One line, fixed vocabulary — never file content, never outside-authored text.
    pub fn render(&self) -> String {
        let verb = if self.downloaded() {
            "downloaded"
        } else {
            "created"
        };
        let when = if self.steps_ago <= 0 {
            "just now".to_string()
        } else if self.steps_ago == 1 {
            "1 step ago".to_string()
        } else {
            format!("{} steps ago", self.steps_ago)
        };
        format!("{} was {} by the agent {}", self.path, verb, when)
    }
}

/// One canonical key per file so `./a.py`, `a.py`, and absolute forms collapse.
pub fn resolve(path: &str, root: &Path) -> String {
    let p = PathBuf::from(path);
    let expanded = if path.starts_with('~') {
        // Best-effort: leave ~ forms as PathBuf; canonicalize below.
        p
    } else {
        p
    };
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    };
    normalize_lexically(&candidate)
        .to_string_lossy()
        .into_owned()
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() || token.starts_with('-') || token.contains("://") {
        return false;
    }
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    let lower = token.to_lowercase();
    SCRIPT_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn program_name(argv0: &str) -> String {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0)
        .to_lowercase();
    if let Some(stripped) = name.strip_suffix(".exe") {
        stripped.to_string()
    } else {
        name
    }
}

fn split_commands(command: &str) -> Vec<String> {
    const SEPARATORS: &[&str] = &["&&", "||", ";", "|&", "|", "&", "\n", "\r"];
    let mut parts = vec![command.to_string()];
    for sep in SEPARATORS {
        parts = parts
            .into_iter()
            .flat_map(|part| {
                part.split(sep)
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Simple shell-ish tokenizer (whitespace + basic quotes). Unbalanced quotes fall back
/// to whitespace split — same conservative spirit as Python's shlex fallback.
fn tokenize(part: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in part.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if in_single || in_double {
        // Unbalanced: still worth scanning for paths.
        return part.split_whitespace().map(|s| s.to_string()).collect();
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn sub_commands(command: &str) -> Vec<Vec<String>> {
    split_commands(command)
        .into_iter()
        .map(|p| tokenize(&p))
        .filter(|argv| !argv.is_empty())
        .collect()
}

/// Every path a shell command names, plus implicit files it would read.
pub fn command_paths(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    for argv in sub_commands(command) {
        for t in argv.iter().skip(1) {
            if looks_like_path(t) {
                found.push(t.clone());
            }
        }
        let mut program = program_name(&argv[0]);
        if program == "docker" && argv.len() > 1 && argv[1].eq_ignore_ascii_case("compose") {
            program = "docker-compose".to_string();
        }
        for t in implicit_targets(&program) {
            found.push((*t).to_string());
        }
        if looks_like_path(&argv[0]) {
            found.push(argv[0].clone());
        }
    }
    found
}

fn shell_download_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for argv in sub_commands(command) {
        let program = program_name(&argv[0]);
        let Some(flags) = fetcher_output_flags(&program) else {
            continue;
        };
        let folded = case_folded_fetcher(&program);
        for i in 1..argv.len() {
            let probe = if folded {
                argv[i].to_lowercase()
            } else {
                argv[i].clone()
            };
            if flags.contains(&probe.as_str()) && i + 1 < argv.len() {
                out.push(argv[i + 1].clone());
            }
        }
        if program == "curl" && argv[1..].iter().any(|t| t == "-O") {
            for candidate in &argv[1..] {
                if candidate.contains("://") {
                    let name = candidate
                        .split('?')
                        .next()
                        .unwrap_or(candidate)
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("");
                    if !name.is_empty() {
                        out.push(name.to_string());
                    }
                    break;
                }
            }
        }
    }
    out
}

fn write_paths(tool_name: &str, arguments: &Value) -> (Vec<String>, bool) {
    let args = arguments.as_object();
    match tool_name {
        "write_file" | "replace_in_file" => {
            let path = args.and_then(|a| a.get("path")).and_then(|v| v.as_str());
            match path {
                Some(p) if !p.is_empty() => (vec![p.to_string()], true),
                _ => (vec![], false),
            }
        }
        "apply_patch" => {
            let blob = args
                .and_then(|a| a.get("patch"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut paths = Vec::new();
            for line in blob.lines() {
                if let Some(rest) = line.strip_prefix("*** Add File: ") {
                    paths.push(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("*** Update File: ") {
                    paths.push(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
                    paths.push(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("*** Move to: ") {
                    paths.push(rest.trim().to_string());
                }
            }
            let ok = !paths.is_empty();
            (paths, ok)
        }
        "apply_unified_diff" => {
            let blob = args
                .and_then(|a| a.get("diff"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut paths = Vec::new();
            for line in blob.lines() {
                if let Some(rest) = line.strip_prefix("+++ ") {
                    let p = rest.trim().strip_prefix("b/").unwrap_or(rest.trim());
                    if !p.is_empty() && p != "/dev/null" {
                        paths.push(p.to_string());
                    }
                }
            }
            let ok = !paths.is_empty();
            (paths, ok)
        }
        _ => (vec![], false),
    }
}

/// `(paths, origin)` for a call that just SUCCEEDED, or `([], "")` when it created nothing.
pub fn created_paths(tool_name: &str, arguments: &Value, result: Option<&Value>) -> (Vec<String>, String) {
    if WRITE_TOOLS.contains(&tool_name) {
        let (paths, located) = write_paths(tool_name, arguments);
        return if located && !paths.is_empty() {
            (paths, WRITTEN.to_string())
        } else {
            (vec![], String::new())
        };
    }
    if DOWNLOAD_RESULT_TOOLS.contains(&tool_name) {
        let path = result
            .and_then(|r| r.get("path"))
            .and_then(|v| v.as_str());
        return match path {
            Some(p) => (vec![p.to_string()], DOWNLOADED.to_string()),
            None => (vec![], String::new()),
        };
    }
    if tool_name == "run_shell" {
        let cmd = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let fetched = shell_download_paths(cmd);
        return if fetched.is_empty() {
            (vec![], String::new())
        } else {
            (fetched, DOWNLOADED.to_string())
        };
    }
    (vec![], String::new())
}

/// Paths a PROPOSED call would run or act on. Shell only in phase 1.
pub fn referenced_paths(tool_name: &str, arguments: &Value) -> Vec<String> {
    if tool_name == "run_shell" {
        let cmd = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return command_paths(cmd);
    }
    vec![]
}

/// Per-session record of what the agent created. Runtime-only.
#[derive(Debug, Clone)]
pub struct SessionFiles {
    root: PathBuf,
    files: HashMap<String, Origin>,
}

impl SessionFiles {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            root: workspace_root.into(),
            files: HashMap::new(),
        }
    }

    /// Update the workspace root used for path resolution (set after construction).
    pub fn set_workspace(&mut self, workspace_root: impl Into<PathBuf>) {
        self.root = workspace_root.into();
    }

    pub fn workspace_root(&self) -> &Path {
        &self.root
    }

    /// Note what a SUCCESSFUL call created. Callers must not record failed calls.
    pub fn record(&mut self, tool_name: &str, arguments: &Value, result: Option<&Value>, step: i64) {
        let (paths, origin_kind) = created_paths(tool_name, arguments, result);
        if origin_kind.is_empty() {
            return;
        }
        for path in paths {
            self.files.insert(
                resolve(&path, &self.root),
                Origin {
                    step,
                    kind: origin_kind.clone(),
                },
            );
        }
    }

    /// The most recently created path this call names, or `None`.
    pub fn match_call(&self, tool_name: &str, arguments: &Value, step: i64) -> Option<Match> {
        let mut best: Option<Match> = None;
        for path in referenced_paths(tool_name, arguments) {
            let key = resolve(&path, &self.root);
            let Some(origin) = self.files.get(&key) else {
                continue;
            };
            let candidate = Match {
                path: path.clone(),
                origin: origin.clone(),
                steps_ago: (step - origin.step).max(0),
            };
            if best
                .as_ref()
                .map(|b| candidate.origin.step > b.origin.step)
                .unwrap_or(true)
            {
                best = Some(candidate);
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_root() -> PathBuf {
        env::temp_dir().join(format!("ocw-prov-{}", std::process::id()))
    }

    #[test]
    fn approval_origin_labels_round_trip() {
        for o in [
            ApprovalOrigin::UserApproved,
            ApprovalOrigin::AutoApproved,
            ApprovalOrigin::TrustedRule,
            ApprovalOrigin::RunGrant,
            ApprovalOrigin::Denied,
        ] {
            assert_eq!(ApprovalOrigin::parse(o.as_str()), Some(o));
        }
        assert_eq!(
            ApprovalOrigin::parse("user"),
            Some(ApprovalOrigin::UserApproved)
        );
    }

    #[test]
    fn approval_from_decision_reason() {
        assert_eq!(
            ApprovalOrigin::from_decision_reason("full access"),
            Some(ApprovalOrigin::Bypass)
        );
        assert_eq!(
            ApprovalOrigin::from_decision_reason("tool allowed for this request"),
            Some(ApprovalOrigin::RunGrant)
        );
        assert_eq!(
            ApprovalOrigin::from_decision_reason(
                "trusted MCP tool (user trust rule for slack_post)"
            ),
            Some(ApprovalOrigin::TrustedRule)
        );
        assert_eq!(
            ApprovalOrigin::from_decision_reason("trusted MCP tool (server trust)"),
            Some(ApprovalOrigin::TrustedServer)
        );
    }

    #[test]
    fn attach_approval_display_sidecar() {
        let mut msg = json!({"role": "tool", "content": "{}"});
        attach_approval_display(
            &mut msg,
            ApprovalOrigin::RunGrant,
            Some("quiet"),
            None,
        );
        assert_eq!(msg["_display"]["approval_origin"], "run_grant");
        assert_eq!(msg["_display"]["approval_note"], "quiet");
    }

    #[test]
    fn agent_written_script_is_flagged_when_run() {
        let root = tmp_root();
        let _ = std::fs::create_dir_all(&root);
        let mut files = SessionFiles::new(&root);
        files.record(
            "write_file",
            &json!({"path": "scripts/setup.py", "content": "x"}),
            Some(&json!({"ok": true})),
            12,
        );
        let m = files
            .match_call(
                "run_shell",
                &json!({"command": "python scripts/setup.py"}),
                15,
            )
            .unwrap();
        assert_eq!(
            m.render(),
            "scripts/setup.py was created by the agent 3 steps ago"
        );
        assert!(!m.downloaded());
    }

    #[test]
    fn pre_existing_file_produces_no_fact() {
        let root = tmp_root();
        let mut files = SessionFiles::new(&root);
        files.record(
            "write_file",
            &json!({"path": "scripts/setup.py", "content": "x"}),
            Some(&json!({"ok": true})),
            12,
        );
        assert!(files
            .match_call(
                "run_shell",
                &json!({"command": "python tools/other.py"}),
                15
            )
            .is_none());
    }

    #[test]
    fn step_distance_reads_naturally() {
        let root = tmp_root();
        let mut files = SessionFiles::new(&root);
        files.record(
            "write_file",
            &json!({"path": "a.py", "content": "x"}),
            Some(&json!({"ok": true})),
            5,
        );
        assert!(files
            .match_call("run_shell", &json!({"command": "python a.py"}), 5)
            .unwrap()
            .render()
            .contains("just now"));
        assert!(files
            .match_call("run_shell", &json!({"command": "python a.py"}), 6)
            .unwrap()
            .render()
            .contains("1 step ago"));
    }

    #[test]
    fn shell_fetchers_record_output_path() {
        let cases = [
            ("curl -o tool.sh https://x.io/a", vec!["tool.sh"]),
            ("curl -O https://x.io/a/tool.sh", vec!["tool.sh"]),
            ("wget -O tool.sh https://x.io/a", vec!["tool.sh"]),
        ];
        for (cmd, expected) in cases {
            let (paths, origin) =
                created_paths("run_shell", &json!({"command": cmd}), None);
            assert_eq!(paths, expected);
            assert_eq!(origin, DOWNLOADED);
        }
    }

    #[test]
    fn reads_create_nothing() {
        assert_eq!(
            created_paths("read_file", &json!({"path": "a.py"}), Some(&json!({"ok": true}))),
            (vec![], String::new())
        );
    }
}
