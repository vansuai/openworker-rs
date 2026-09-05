//! Auto-compaction of long session histories (OPE-27).
//!
//! Port of `coworker/compaction.py`. When the outbound history approaches the model's
//! context limit, the older portion of the *outbound* view is replaced with an LLM-written
//! structured summary plus mechanically extracted state. The persisted JSONL transcript is
//! never modified — only what is sent to the provider.
//!
//! Pure functions + [`CompactionState`]; the engine owns *when* and *with what* provider.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trigger: min(threshold_pct × context_window, cap_tokens).
pub const DEFAULT_THRESHOLD_PCT: f64 = 0.8;
pub const DEFAULT_CAP_TOKENS: i64 = 250_000;
pub const DEFAULT_CONTEXT_WINDOW: i64 = 128_000;
pub const KEEP_RECENT_FRACTION: f64 = 0.25;
pub const SUMMARY_MAX_TOKENS: i64 = 3_000;

const SPAN_TOOL_RESULT_CLIP: usize = 400;
const SPAN_BUDGET_CHARS: usize = 400_000;
const USER_MESSAGE_CLIP: usize = 600;
const USER_MESSAGES_MAX: usize = 40;
const TRIM_FRACTION: f64 = 0.10;

const WRITE_HINTS: &[&str] = &["write", "edit", "append", "save", "create", "patch"];
const ARTIFACT_HINTS: &[&str] = &["artifact", "publish", "deploy"];

pub const SUMMARY_SYSTEM_PROMPT: &str = r#"You are compacting an AI coworker's session history so the coworker can continue working in a smaller context. Write a structured summary of the conversation below. It is the coworker's ONLY memory of these turns, so preserve everything load-bearing.

Produce ALL of the following sections, in this order, each as a markdown heading:

1. **Primary request and intent** — what the user is trying to get done, in their terms, including standing constraints stated at any point (e.g. "never send without my approval"). Constraints outlive the turns they were stated in.
2. **Key concepts and decisions** — domain facts, technical choices, and rationale established so far. Include the WHY, not just the what — a decision without its reason gets relitigated.
3. **Artifacts and files** — every file/deliverable created, modified, or read that still matters: path, its role, and a short excerpt of load-bearing content only.
4. **Errors and fixes** — problems hit and how they were resolved, including user corrections ("no, do it this way") — those are feedback with lasting force.
5. **All user messages** — a chronological list of every user message (trimmed of pasted bulk). This is the intent audit-trail.
6. **Pending tasks** — explicitly incomplete items, promised follow-ups, things the user said "later" about.
7. **Current work** — precisely what was in progress at this point: which step, which file, what state.
8. **Next step** — the immediate next action, justified by the user's request.

Rules:
- Do NOT carry full file contents as truth. Note THAT a file was read/edited; the coworker re-reads if it needs the content again. Stale memory of a file is worse than no memory.
- Be concrete: paths, names, commands, ids — not vague references.
- Output only the summary sections, no preamble."#;

pub const CONTINUATION_CONTRACT: &str = concat!(
    "Continue where you left off: pick up the current work and next step exactly as ",
    "described. Do not re-ask answered questions, do not recap, do not mention that the ",
    "context was compacted. If you need the contents of a file noted above, re-read it.",
);

const OVERFLOW_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "context window",
    "prompt is too long",
    "input is too long",
    "too many tokens",
    "input length and `max_tokens` exceed",
    "exceeds the maximum number of tokens",
];

// -- token math ---------------------------------------------------------------

/// chars/4 over the serialized messages — fallback signal when providers omit usage.
pub fn estimate_tokens(messages: &[Value]) -> i64 {
    let mut total = 0usize;
    for msg in messages {
        total += match serde_json::to_string(msg) {
            Ok(s) => s.len(),
            Err(_) => msg.to_string().len(),
        };
    }
    (total / 4) as i64
}

pub fn trigger_tokens(
    context_window: Option<i64>,
    threshold_pct: f64,
    cap_tokens: i64,
) -> i64 {
    let window = context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let pct = (threshold_pct * window as f64) as i64;
    pct.min(cap_tokens)
}

pub fn trigger_tokens_default(context_window: Option<i64>) -> i64 {
    trigger_tokens(
        context_window,
        DEFAULT_THRESHOLD_PCT,
        DEFAULT_CAP_TOKENS,
    )
}

pub fn should_compact(
    signal: i64,
    context_window: Option<i64>,
    threshold_pct: f64,
    cap_tokens: i64,
) -> bool {
    signal >= trigger_tokens(context_window, threshold_pct, cap_tokens)
}

pub fn should_compact_default(signal: i64, context_window: Option<i64>) -> bool {
    should_compact(
        signal,
        context_window,
        DEFAULT_THRESHOLD_PCT,
        DEFAULT_CAP_TOKENS,
    )
}

pub fn keep_tokens_for_trigger(trigger: i64) -> i64 {
    ((trigger as f64) * KEEP_RECENT_FRACTION) as i64
}

// -- state --------------------------------------------------------------------

/// One compaction point. `boundary_index` indexes the CANONICAL message list:
/// messages before it are represented by the compacted block in the outbound view;
/// messages from it on are sent verbatim. Persisted with the session so reloads keep
/// the view. The JSONL transcript itself is never rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionState {
    pub boundary_index: usize,
    pub summary_text: String,
    pub working_state: String,
    #[serde(default)]
    pub user_messages: Vec<String>,
    #[serde(default)]
    pub user_messages_dropped: i64,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub model_used: String,
    /// True when this state came from the no-summary trim fallback.
    #[serde(default)]
    pub trimmed: bool,
}

impl CompactionState {
    pub fn as_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_value(raw: &Value) -> Option<Self> {
        let obj = raw.as_object()?;
        if !obj.contains_key("boundary_index") {
            return None;
        }
        serde_json::from_value(raw.clone()).ok()
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn role_of(msg: &Value) -> Option<&str> {
    msg.get("role").and_then(|v| v.as_str())
}

// -- boundary -----------------------------------------------------------------

fn turn_starts(messages: &[Value], start: usize) -> (Vec<usize>, Vec<usize>) {
    let mut users = Vec::new();
    let mut assistants = Vec::new();
    for (i, msg) in messages.iter().enumerate().skip(start) {
        match role_of(msg) {
            Some("user") => users.push(i),
            Some("assistant") => assistants.push(i),
            _ => {}
        }
    }
    (users, assistants)
}

fn fit_boundary(messages: &[Value], candidates: &[usize], keep_tokens: i64) -> Option<usize> {
    for &i in candidates {
        if estimate_tokens(&messages[i..]) <= keep_tokens {
            return Some(i);
        }
    }
    None
}

/// Canonical index where the verbatim tail begins. Prefers user-message boundaries;
/// falls back to assistant (iteration) boundaries. `None` when nothing meaningful to summarize.
pub fn pick_boundary(messages: &[Value], keep_tokens: i64) -> Option<usize> {
    let start = if messages
        .first()
        .and_then(role_of)
        .is_some_and(|r| r == "system")
    {
        1
    } else {
        0
    };
    let (users, assistants) = turn_starts(messages, start);

    let mut boundary = fit_boundary(messages, &users, keep_tokens);
    if boundary.is_none() && !users.is_empty() {
        let last_user = *users.last().unwrap();
        let inside: Vec<usize> = assistants.iter().copied().filter(|&i| i > last_user).collect();
        boundary = fit_boundary(messages, &inside, keep_tokens);
        if boundary.is_none() {
            boundary = inside.last().copied().or(Some(last_user));
        }
    }
    if boundary.is_none() {
        boundary = fit_boundary(messages, &assistants, keep_tokens)
            .or_else(|| assistants.last().copied());
    }
    match boundary {
        Some(b) if b > start => Some(b),
        _ => None,
    }
}

// -- mechanical extraction ----------------------------------------------------

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        out.push(p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string());
                    }
                    Some("image_url") => out.push("[image]".to_string()),
                    _ => {}
                }
            }
            out.join("\n")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn result_status(result: Option<&Value>) -> String {
    let Some(Value::String(s)) = result else {
        return String::new();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(s) else {
        return String::new();
    };
    let Some(obj) = parsed.as_object() else {
        return String::new();
    };
    if obj.contains_key("error") {
        return "error".to_string();
    }
    if let Some(code) = obj.get("exit_code") {
        let ok = matches!(code, Value::Number(n) if n.as_i64() == Some(0))
            || matches!(code, Value::String(s) if s == "0");
        return if ok {
            "ok".to_string()
        } else {
            format!("exit {code}")
        };
    }
    String::new()
}

fn iter_tool_calls(span: &[Value]) -> Vec<(String, Value, Option<Value>)> {
    let mut results = std::collections::HashMap::new();
    for m in span {
        if role_of(m) == Some("tool") {
            if let Some(id) = m.get("tool_call_id").and_then(|v| v.as_str()) {
                results.insert(id.to_string(), m.get("content").cloned());
            }
        }
    }
    let mut out = Vec::new();
    for msg in span {
        if role_of(msg) != Some("assistant") {
            continue;
        }
        let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) else {
            continue;
        };
        for tc in tcs {
            let fn_ = tc.get("function").cloned().unwrap_or(json!({}));
            let name = fn_
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = match fn_.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                Some(other) => other.clone(),
                None => json!({}),
            };
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            out.push((name, args, results.get(id).cloned().flatten()));
        }
    }
    out
}

fn dedupe_recent_first(items: &[String], limit: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for item in items.iter().rev() {
        if !seen.iter().any(|s: &String| s == item) {
            seen.push(item.clone());
        }
        if seen.len() >= limit {
            break;
        }
    }
    seen
}

/// Mechanical block from the span's tool-call records (no LLM).
pub fn extract_working_state(span: &[Value]) -> String {
    let mut files = Vec::new();
    let mut commands = Vec::new();
    let mut artifacts = Vec::new();
    let mut tools = Vec::new();

    for (name, args, result) in iter_tool_calls(span) {
        if !name.is_empty() && !tools.iter().any(|t: &String| t == &name) {
            tools.push(name.clone());
        }
        let lowered = name.to_lowercase();
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(|v| v.as_str());
        if let Some(path) = path {
            if WRITE_HINTS.iter().any(|h| lowered.contains(h)) {
                files.push(path.to_string());
            }
        }
        if lowered == "run_shell" {
            if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                let status = result_status(result.as_ref());
                let line: String = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
                let line: String = line.chars().take(160).collect();
                if status.is_empty() {
                    commands.push(line);
                } else {
                    commands.push(format!("{line}  [{status}]"));
                }
            }
        }
        if ARTIFACT_HINTS.iter().any(|h| lowered.contains(h)) {
            if let Some(loc) = args
                .get("url")
                .or_else(|| args.get("path"))
                .or_else(|| args.get("title"))
                .and_then(|v| v.as_str())
            {
                artifacts.push(loc.to_string());
            }
        }
    }

    let mut lines = vec!["## Working state (extracted mechanically from tool records)".to_string()];
    let written = dedupe_recent_first(&files, 20);
    if !written.is_empty() {
        lines.push("Files written/edited (most recent first):".to_string());
        for p in &written {
            lines.push(format!("- {p}"));
        }
    }
    let recent_cmds: Vec<_> = commands.iter().rev().take(10).cloned().collect::<Vec<_>>();
    let recent_cmds: Vec<_> = recent_cmds.into_iter().rev().collect();
    if !recent_cmds.is_empty() {
        lines.push("Recent shell commands:".to_string());
        for c in &recent_cmds {
            lines.push(format!("- {c}"));
        }
    }
    let made = dedupe_recent_first(&artifacts, 10);
    if !made.is_empty() {
        lines.push("Artifacts produced:".to_string());
        for a in &made {
            lines.push(format!("- {a}"));
        }
    }
    if !tools.is_empty() {
        let mut sorted = tools;
        sorted.sort();
        lines.push(format!(
            "Tools used in the summarized span: {}",
            sorted.join(", ")
        ));
    }
    if lines.len() > 1 {
        lines.join("\n")
    } else {
        String::new()
    }
}

/// Every user message in the span, chronological, trimmed of pasted bulk.
pub fn extract_user_messages(span: &[Value], clip: usize) -> Vec<String> {
    let mut out = Vec::new();
    for msg in span {
        if role_of(msg) != Some("user") {
            continue;
        }
        let text: String = text_of(msg.get("content").unwrap_or(&Value::Null))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        if text.len() > clip {
            let mut clipped: String = text.chars().take(clip.saturating_sub(1)).collect();
            clipped.push('…');
            out.push(clipped);
        } else {
            out.push(text);
        }
    }
    out
}

pub fn extract_user_messages_default(span: &[Value]) -> Vec<String> {
    extract_user_messages(span, USER_MESSAGE_CLIP)
}

fn cap_user_messages(
    messages: Vec<String>,
    prior_dropped: i64,
    limit: usize,
) -> (Vec<String>, i64) {
    if messages.len() <= limit {
        return (messages, prior_dropped);
    }
    let drop_n = (messages.len() - limit) as i64;
    (messages[messages.len() - limit..].to_vec(), prior_dropped + drop_n)
}

// -- summarizer ---------------------------------------------------------------

fn render_span(span: &[Value], budget_chars: usize) -> String {
    let mut lines = Vec::new();
    for msg in span {
        match role_of(msg) {
            Some("system") | Some("notice") => continue,
            Some("tool") => {
                let mut text: String = text_of(msg.get("content").unwrap_or(&Value::Null))
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.len() > SPAN_TOOL_RESULT_CLIP {
                    text = text
                        .chars()
                        .take(SPAN_TOOL_RESULT_CLIP.saturating_sub(1))
                        .collect::<String>()
                        + "…";
                }
                lines.push(format!("[tool result] {text}"));
            }
            Some("assistant") => {
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let fn_ = tc.get("function").cloned().unwrap_or(json!({}));
                        let name = fn_.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let mut args: String = fn_
                            .get("arguments")
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if args.len() > 200 {
                            args = args.chars().take(199).collect::<String>() + "…";
                        }
                        lines.push(format!("[assistant → {name}] {args}"));
                    }
                }
                let text = text_of(msg.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    lines.push(format!("[assistant] {text}"));
                }
            }
            Some("user") => {
                lines.push(format!(
                    "[user] {}",
                    text_of(msg.get("content").unwrap_or(&Value::Null))
                ));
            }
            _ => {}
        }
    }
    let mut rendered = lines.join("\n");
    if rendered.len() > budget_chars {
        let tail: String = rendered
            .chars()
            .skip(rendered.len().saturating_sub(budget_chars))
            .collect();
        rendered = format!("(…oldest turns elided…)\n{tail}");
    }
    rendered
}

/// Provider-ready messages for the summarizer call.
pub fn summarizer_messages(span: &[Value], prior_summary: &str) -> Vec<Value> {
    let mut body = render_span(span, SPAN_BUDGET_CHARS);
    if !prior_summary.is_empty() {
        body = format!(
            "[previous compaction summary — fold its still-relevant content into the new \
             summary]\n{prior_summary}\n\n[conversation since]\n{body}"
        );
    }
    vec![
        json!({"role": "system", "content": SUMMARY_SYSTEM_PROMPT}),
        json!({"role": "user", "content": body}),
    ]
}

/// No-LLM fallback: advance the boundary past ~`fraction` of the outbound messages.
pub fn trim_state(
    messages: &[Value],
    prior: Option<&CompactionState>,
    fraction: f64,
) -> Option<CompactionState> {
    let start = prior.map(|p| p.boundary_index).unwrap_or(0);
    let remaining = messages.len().saturating_sub(start);
    if remaining <= 2 {
        return None;
    }
    let step = ((remaining as f64) * fraction).max(1.0) as usize;
    let target = start + step;
    let mut boundary = None;
    for (i, msg) in messages.iter().enumerate().skip(target) {
        if matches!(role_of(msg), Some("user") | Some("assistant")) {
            boundary = Some(i);
            break;
        }
    }
    let boundary = boundary?;
    if boundary <= start || boundary >= messages.len() {
        return None;
    }
    let span = &messages[start..boundary];
    let mut prior_users = prior.map(|p| p.user_messages.clone()).unwrap_or_default();
    prior_users.extend(extract_user_messages_default(span));
    let prior_dropped = prior.map(|p| p.user_messages_dropped).unwrap_or(0);
    let (users, dropped) = cap_user_messages(prior_users, prior_dropped, USER_MESSAGES_MAX);

    let mut summary = String::new();
    if let Some(p) = prior {
        if !p.summary_text.is_empty() {
            summary.push_str(&p.summary_text);
            summary.push_str("\n\n");
        }
    }
    summary.push_str(
        "(Older turns were trimmed to fit the context window; no summary is available \
         for them. Re-read files and re-run commands if earlier results are needed.)",
    );

    Some(CompactionState {
        boundary_index: boundary,
        summary_text: summary,
        working_state: extract_working_state(span),
        user_messages: users,
        user_messages_dropped: dropped,
        created_at: now_secs(),
        model_used: String::new(),
        trimmed: true,
    })
}

pub fn trim_state_default(
    messages: &[Value],
    prior: Option<&CompactionState>,
) -> Option<CompactionState> {
    trim_state(messages, prior, TRIM_FRACTION)
}

/// The single outbound message standing in for everything before the boundary.
pub fn compacted_block(state: &CompactionState) -> String {
    let mut parts = vec![
        "<compacted-history>".to_string(),
        "Earlier turns of this session were compacted. The summary below is your memory \
         of them."
            .to_string(),
        String::new(),
        state.summary_text.clone(),
    ];
    if !state.working_state.is_empty() {
        parts.push(String::new());
        parts.push(state.working_state.clone());
    }
    if !state.user_messages.is_empty() {
        parts.push(String::new());
        parts.push("## User messages in the compacted span (verbatim, chronological)".to_string());
        if state.user_messages_dropped > 0 {
            parts.push(format!(
                "({} earlier user messages omitted — their intent is covered by the summary above)",
                state.user_messages_dropped
            ));
        }
        for u in &state.user_messages {
            parts.push(format!("- {u}"));
        }
    }
    parts.push(String::new());
    parts.push(CONTINUATION_CONTRACT.to_string());
    parts.push("</compacted-history>".to_string());
    parts.join("\n")
}

/// Outbound view for the provider: `[system?] + compacted block + verbatim tail`.
/// Canonical history is untouched; returns a new Vec. No-op when state is absent/stale.
pub fn apply_to_outbound(messages: &[Value], state: Option<&CompactionState>) -> Vec<Value> {
    let Some(state) = state else {
        return messages.to_vec();
    };
    let boundary = state.boundary_index;
    if boundary == 0 || boundary >= messages.len() {
        return messages.to_vec();
    }
    let mut head = Vec::new();
    if messages
        .first()
        .and_then(role_of)
        .is_some_and(|r| r == "system")
    {
        head.push(messages[0].clone());
    }
    head.push(json!({
        "role": "user",
        "content": compacted_block(state),
    }));
    head.extend_from_slice(&messages[boundary..]);
    head
}

/// Build a [`CompactionState`] without calling a provider — uses a pre-written summary.
/// Engine wires `summarize_span` via its provider; this covers the apply path + tests.
pub fn build_state_with_summary(
    messages: &[Value],
    summary: String,
    model: &str,
    keep_tokens: i64,
    prior: Option<&CompactionState>,
) -> Option<CompactionState> {
    let boundary = pick_boundary(messages, keep_tokens)?;
    if let Some(p) = prior {
        if boundary <= p.boundary_index {
            return None;
        }
    }
    let span_start = prior.map(|p| p.boundary_index).unwrap_or(0);
    let span = &messages[span_start..boundary];
    let mut prior_users = prior.map(|p| p.user_messages.clone()).unwrap_or_default();
    prior_users.extend(extract_user_messages_default(span));
    let prior_dropped = prior.map(|p| p.user_messages_dropped).unwrap_or(0);
    let (users, dropped) = cap_user_messages(prior_users, prior_dropped, USER_MESSAGES_MAX);

    Some(CompactionState {
        boundary_index: boundary,
        summary_text: summary,
        working_state: extract_working_state(span),
        user_messages: users,
        user_messages_dropped: dropped,
        created_at: now_secs(),
        model_used: model.to_string(),
        trimmed: false,
    })
}

pub fn is_context_overflow(err_text: &str) -> bool {
    let text = err_text.to_lowercase();
    OVERFLOW_MARKERS.iter().any(|m| text.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text, "ts": 1.0})
    }

    fn assistant(text: &str) -> Value {
        json!({"role": "assistant", "content": text, "ts": 1.0})
    }

    fn assistant_tools(text: &str, name: &str, args: Value, id: &str) -> Value {
        json!({
            "role": "assistant",
            "content": text,
            "ts": 1.0,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            }]
        })
    }

    fn tool(call_id: &str, content: Value) -> Value {
        let content = match content {
            Value::String(s) => s,
            other => other.to_string(),
        };
        json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
            "ts": 1.0
        })
    }

    fn convo(turns: usize, bulk: usize) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "system", "content": "You are a coworker."})];
        let pad = "x".repeat(bulk);
        for i in 0..turns {
            msgs.push(user(&format!("request {i}")));
            msgs.push(assistant(&format!("answer {i} {pad}")));
        }
        msgs
    }

    #[test]
    fn trigger_is_min_of_pct_and_cap() {
        assert_eq!(trigger_tokens_default(Some(100_000)), 80_000);
        assert_eq!(trigger_tokens_default(Some(1_000_000)), DEFAULT_CAP_TOKENS);
        assert_eq!(
            trigger_tokens_default(None),
            (0.8 * DEFAULT_CONTEXT_WINDOW as f64) as i64
        );
        assert_eq!(trigger_tokens(Some(100_000), 0.5, 40_000), 40_000);
        assert_eq!(trigger_tokens(Some(100_000), 0.5, 999_999), 50_000);
    }

    #[test]
    fn should_compact_crosses_threshold() {
        assert!(!should_compact_default(79_999, Some(100_000)));
        assert!(should_compact_default(80_000, Some(100_000)));
    }

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        let msgs = vec![user(&"a".repeat(400))];
        let est = estimate_tokens(&msgs);
        assert!((100..=120).contains(&est));
    }

    #[test]
    fn boundary_prefers_earliest_user_turn_that_fits() {
        let msgs = convo(6, 2000);
        let per_turn = estimate_tokens(&msgs[1..3]);
        let boundary = pick_boundary(&msgs, per_turn * 2 + 10).unwrap();
        assert_eq!(role_of(&msgs[boundary]), Some("user"));
        assert_eq!(msgs[boundary]["content"], "request 4");
    }

    #[test]
    fn boundary_none_when_nothing_to_summarize() {
        let msgs = vec![
            json!({"role": "system", "content": "s"}),
            user("hi"),
            assistant("hello"),
        ];
        assert!(pick_boundary(&msgs, 10_000_000).is_none());
    }

    #[test]
    fn apply_to_outbound_leaves_canonical_untouched() {
        let msgs = convo(6, 2000);
        let keep = estimate_tokens(&msgs[msgs.len().saturating_sub(4)..]) + 10;
        let state = build_state_with_summary(
            &msgs,
            "## Summary\nthe gist".into(),
            "m",
            keep,
            None,
        )
        .unwrap();
        let original_len = msgs.len();
        let out = apply_to_outbound(&msgs, Some(&state));
        assert_eq!(out[0]["role"], "system");
        let block = out[1]["content"].as_str().unwrap();
        assert!(block.contains("<compacted-history>"));
        assert!(block.contains("the gist"));
        assert!(block.contains("request 0"));
        assert_eq!(out[2], msgs[state.boundary_index]);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn working_state_files_commands_tools() {
        let span = vec![
            user("write it"),
            assistant_tools("", "write_file", json!({"path": "a.py", "content": "x"}), "c0"),
            tool("c0", json!({"ok": true})),
            assistant_tools("", "run_shell", json!({"command": "pytest -q"}), "c1"),
            tool("c1", json!({"exit_code": 1})),
            assistant_tools("", "write_file", json!({"path": "b.py", "content": "y"}), "c2"),
            tool("c2", json!({"ok": true})),
            assistant_tools("", "write_file", json!({"path": "a.py", "content": "x2"}), "c3"),
            tool("c3", json!({"ok": true})),
        ];
        let block = extract_working_state(&span);
        assert!(block.find("- a.py").unwrap() < block.find("- b.py").unwrap());
        assert_eq!(block.matches("a.py").count(), 1);
        assert!(block.contains("pytest -q") && block.contains("[exit 1]"));
        assert!(block.contains("run_shell") && block.contains("write_file"));
    }

    #[test]
    fn summarizer_messages_fold_prior() {
        let span = vec![
            user("go"),
            assistant_tools("", "read_file", json!({"path": "big.txt"}), "c0"),
            tool("c0", Value::String("huge ".repeat(500))),
        ];
        let msgs = summarizer_messages(&span, "OLD SUMMARY");
        let body = msgs[1]["content"].as_str().unwrap();
        assert!(body.contains("OLD SUMMARY"));
        assert!(body.len() < 3000);
        assert!(msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("Primary request and intent"));
    }

    #[test]
    fn state_round_trips_via_value() {
        let state = CompactionState {
            boundary_index: 4,
            summary_text: "s".into(),
            working_state: "w".into(),
            user_messages: vec!["u".into()],
            user_messages_dropped: 2,
            created_at: 1.5,
            model_used: "m".into(),
            trimmed: false,
        };
        let restored = CompactionState::from_value(&state.as_value()).unwrap();
        assert_eq!(restored, state);
        assert!(CompactionState::from_value(&Value::Null).is_none());
        assert!(CompactionState::from_value(&json!({})).is_none());
    }

    #[test]
    fn is_context_overflow_detects_markers() {
        assert!(is_context_overflow("Error: context_length_exceeded"));
        assert!(is_context_overflow("maximum context length exceeded"));
        assert!(!is_context_overflow("rate limit exceeded"));
    }
}
