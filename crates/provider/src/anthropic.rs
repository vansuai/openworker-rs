//! Anthropic Messages API client — native Claude support.

use crate::error::Error;
use crate::tool_args::{normalize_tool_input, parse_tool_arguments};
use crate::types::{AssistantTurn, ModelCapabilities, StreamEvent, TokenUsage, ToolCall};
use reqwest::blocking::Client;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_TOKENS: usize = 16_000;
#[allow(dead_code)]
const DEFAULT_THINKING_BUDGET: usize = 8_192;

const BUDGET_THINKING_PREFIXES: &[&str] = &[
    "claude-haiku-4-5",
    "claude-sonnet-4-5",
    "claude-opus-4-5",
    "claude-opus-4-1",
    "claude-opus-4-0",
    "claude-sonnet-4-0",
    "claude-3",
    "claude-2",
];

const FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
#[allow(dead_code)]
const FALLBACK_MODEL: &str = "claude-opus-4-8";

fn uses_budget_thinking(model: &str) -> bool {
    BUDGET_THINKING_PREFIXES
        .iter()
        .any(|p| model.starts_with(p))
}

fn needs_refusal_fallback(model: &str) -> bool {
    model.starts_with("claude-fable") || model.starts_with("claude-mythos")
}

fn stop_reason_map(reason: &str) -> &str {
    match reason {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "refusal" | "pause_turn" => "stop",
        _ => reason,
    }
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

fn convert_messages(msgs: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut converted: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        if m.get("role").and_then(|v| v.as_str()) == Some("system") {
            if let Some(content) = m.get("content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    system_parts.push(content.to_string());
                }
            }
            i += 1;
        } else {
            break;
        }
    }

    for m in &msgs[i..] {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = m.get("content");

        match role {
            "system" => {
                let text = content.and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    converted.push(serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": format!("<system>\n{text}\n</system>")}]
                    }));
                }
            }
            "user" => {
                let blocks = content_to_blocks(content);
                if !blocks.is_empty() {
                    converted.push(serde_json::json!({ "role": "user", "content": blocks }));
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();

                if let Some(anthropic) = m.get("_anthropic").and_then(|v| v.as_object()) {
                    if let Some(blks) = anthropic.get("blocks").and_then(|v| v.as_array()) {
                        for b in blks {
                            blocks.push(b.clone());
                        }
                    }
                }

                if let Some(text) = content.and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                }

                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let fn_ = tc.get("function").and_then(|v| v.as_object());
                        let args = fn_
                            .and_then(|f| f.get("arguments"))
                            .map(|a| {
                                if let Some(s) = a.as_str() {
                                    parse_tool_arguments(s)
                                } else {
                                    normalize_tool_input(a.clone())
                                }
                            })
                            .unwrap_or(serde_json::Value::Null);
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            "name": fn_.and_then(|f| f.get("name").and_then(|v| v.as_str())).unwrap_or(""),
                            "input": args,
                        }));
                    }
                }

                if !blocks.is_empty() {
                    converted.push(serde_json::json!({ "role": "assistant", "content": blocks }));
                }
            }
            "tool" => {
                converted.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                        "content": m.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    }]
                }));
            }
            _ => {}
        }
    }

    let mut folded: Vec<Value> = Vec::new();
    for msg in converted {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(last) = folded.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some(role) {
                if let (Some(arr1), Some(arr2)) = (last.as_array_mut(), msg.as_array()) {
                    for c in arr2 {
                        arr1.push(c.clone());
                    }
                    continue;
                }
            }
        }
        folded.push(msg);
    }

    if folded.is_empty() || folded[0].get("role").and_then(|v| v.as_str()) != Some("user") {
        folded.insert(0, serde_json::json!({ "role": "user", "content": [{"type": "text", "text": "(continued)"}] }));
    }

    (
        if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        },
        folded,
    )
}

fn content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![serde_json::json!({ "type": "text", "text": s })],
        Some(Value::Array(arr)) => arr.iter().filter_map(|part| {
            let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            match kind {
                "text" => {
                    let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if text.is_empty() { None } else { Some(serde_json::json!({ "type": "text", "text": text })) }
                }
                "image_url" => {
                    let url = part.get("image_url")
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() && (url.starts_with("data:image/") || url.starts_with("http")) {
                        Some(image_block(url))
                    } else {
                        Some(serde_json::json!({ "type": "text", "text": "[unsupported image]" }))
                    }
                }
                "file" => {
                    let file = part.get("file").and_then(|v| v.as_object());
                    let file_data = file.and_then(|f| f.get("file_data")).and_then(|v| v.as_str()).unwrap_or("");
                    let filename = file.and_then(|f| f.get("filename")).and_then(|v| v.as_str()).unwrap_or("");
                    if file_data.starts_with("data:application/pdf;base64,") {
                        let b64 = &file_data[37..];
                        let mut block = serde_json::json!({
                            "type": "document",
                            "source": { "type": "base64", "media_type": "application/pdf", "data": b64 }
                        });
                        if !filename.is_empty() {
                            if let Some(obj) = block.as_object_mut() {
                                obj.insert("title".to_string(), serde_json::json!(filename));
                            }
                        }
                        Some(block)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }).collect(),
        _ => vec![],
    }
}

fn image_block(url: &str) -> Value {
    if let Some(pos) = url.find(";base64,") {
        let media_type = &url[5..pos];
        let data = &url[pos + 8..];
        serde_json::json!({
            "type": "image",
            "source": { "type": "base64", "mimeType": media_type, "data": data }
        })
    } else {
        serde_json::json!({ "type": "image", "source": { "type": "url", "url": url } })
    }
}

fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools.iter().map(|t| {
        let empty = serde_json::Map::new();
        let function = t.get("function").and_then(|v| v.as_object()).unwrap_or(&empty);
        let name = function.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mut entry = serde_json::json!({ "name": name });
        if let Some(desc) = function.get("description").and_then(|v| v.as_str()) {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("description".to_string(), serde_json::json!(desc));
            }
        }
        let params = function.get("parameters")
            .and_then(|v| v.as_object())
            .map(strip_schema);
        let schema = match params {
            Some(p) => {
                let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("object");
                let mut schema_obj = serde_json::Map::new();
                schema_obj.insert("type".to_string(), serde_json::json!(t));
                schema_obj.insert(
                    "properties".to_string(),
                    p.get("properties")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                );
                if let Some(required) = p.get("required") {
                    schema_obj.insert("required".to_string(), required.clone());
                }
                serde_json::Value::Object(schema_obj)
            }
            None => serde_json::json!({ "type": "object", "properties": {} }),
        };
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("input_schema".to_string(), schema);
        }
        entry
    }).collect()
}

fn strip_schema(obj: &serde_json::Map<String, Value>) -> Value {
    const KEEP: &[&str] = &[
        "type",
        "format",
        "description",
        "nullable",
        "enum",
        "items",
        "properties",
        "required",
    ];
    let filtered: serde_json::Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| KEEP.contains(&k.as_str()))
        .map(|(k, v)| {
            let v = match v {
                Value::Object(o) => strip_schema(o),
                Value::Array(a) => Value::Array(
                    a.iter()
                        .map(|i| {
                            if let Value::Object(o) = i {
                                strip_schema(o)
                            } else {
                                i.clone()
                            }
                        })
                        .collect(),
                ),
                _ => v.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    Value::Object(filtered)
}

// ---------------------------------------------------------------------------
// Anthropic client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AnthropicClient {
    http: Client,
    api_key: String,
    #[allow(dead_code)]
    default_model: String,
    thinking_budget: usize,
    base_url: String,
    vendor: String,
}

impl AnthropicClient {
    pub fn new(api_key: String, default_model: String, thinking_budget: usize) -> Self {
        Self::with_base_url(
            api_key,
            default_model,
            thinking_budget,
            "https://api.anthropic.com".into(),
            "anthropic".into(),
        )
    }

    pub fn with_base_url(
        api_key: String,
        default_model: String,
        thinking_budget: usize,
        base_url: String,
        vendor: String,
    ) -> Self {
        Self {
            http: Client::new(),
            api_key,
            default_model,
            thinking_budget,
            base_url,
            vendor,
        }
    }

    fn messages_endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn is_native_anthropic(&self) -> bool {
        self.vendor == "anthropic"
    }

    fn build_request(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Value {
        let (system, msgs) = convert_messages(messages);
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), serde_json::json!(model));
        body.insert("messages".to_string(), serde_json::json!(msgs));

        if let Some(s) = system {
            if self.is_native_anthropic() {
                // Anthropic prompt caching — array of blocks with cache_control.
                body.insert(
                    "system".to_string(),
                    serde_json::json!([{
                        "type": "text",
                        "text": s,
                        "cache_control": { "type": "ephemeral" }
                    }]),
                );
            } else {
                // MiniMax and other Anthropic-compatible vendors expect a plain string
                // (see platform.minimax.cn Anthropic SDK docs) — NOT {role, content}.
                body.insert("system".to_string(), serde_json::json!(s));
            }
        }

        if let Some(tools) = tools {
            let converted = convert_tools(tools);
            body.insert("tools".to_string(), serde_json::json!(converted));
        }

        // Prompt-cache breakpoint on the last message block — Anthropic-only.
        if self.is_native_anthropic() {
            if let Some(msgs_arr) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
                if let Some(last_msg) = msgs_arr.last_mut() {
                    if let Some(content) =
                        last_msg.get_mut("content").and_then(|v| v.as_array_mut())
                    {
                        if let Some(last_block) = content.last_mut() {
                            let mut merged = last_block.clone();
                            if let Some(obj) = merged.as_object_mut() {
                                obj.insert(
                                    "cache_control".to_string(),
                                    serde_json::json!({ "type": "ephemeral" }),
                                );
                            } else {
                                merged = serde_json::json!({
                                    "type": "text",
                                    "text": "",
                                    "cache_control": { "type": "ephemeral" }
                                });
                            }
                            *last_block = merged;
                        }
                    }
                }
            }
        }

        let whitelist = [
            "temperature",
            "top_p",
            "top_k",
            "max_tokens",
            "stop_sequences",
        ];
        for k in &whitelist {
            if let Some(v) = settings.get(*k) {
                body.insert(k.to_string(), v.clone());
            }
        }

        // Thinking config
        if self.thinking_budget > 0 && !body.contains_key("thinking") {
            if uses_budget_thinking(model) {
                body.insert(
                    "thinking".to_string(),
                    serde_json::json!({
                        "type": "enabled", "budget_tokens": self.thinking_budget
                    }),
                );
            } else {
                body.insert(
                    "thinking".to_string(),
                    serde_json::json!({
                        "type": "adaptive", "display": "summarized"
                    }),
                );
            }
        }

        body.entry("max_tokens".to_string())
            .or_insert(serde_json::json!(DEFAULT_MAX_TOKENS));

        Value::Object(body)
    }

    /// Perform one blocking completion.
    ///
    /// Internally uses the streaming Messages API and accumulates to a final
    /// `AssistantTurn` — Anthropic refuses non-streaming requests whose
    /// `max_tokens` could exceed ~10 minutes (mirrors Python OPE fix).
    pub fn complete(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<AssistantTurn, Error> {
        let tools_ref = tools.as_deref();
        let body = self.build_request(model, &messages, tools_ref, &settings);
        let mut obj = body.as_object().cloned().unwrap_or_default();
        obj.insert("stream".to_string(), serde_json::json!(true));
        let beta = needs_refusal_fallback(model);

        let mut req = self
            .http
            .post(self.messages_endpoint())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        if beta {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }

        let resp = req.json(&Value::Object(obj)).send()?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.text()?;
            return Err(Error::from_response(status, &body, "anthropic"));
        }

        let body = resp.text()?;
        let mut iter = AnthropicStreamIter::new(&body);
        let mut final_turn: Option<AssistantTurn> = None;
        while let Some(ev) = iter.next() {
            if let StreamEvent::Turn { turn } = ev {
                final_turn = Some(turn);
            }
        }
        final_turn.ok_or_else(|| {
            Error::Other(
                "anthropic stream completed without a final turn (empty or truncated SSE)"
                    .to_string(),
            )
        })
    }

    /// Stream completions.
    #[allow(dead_code)]
    pub fn stream(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<impl Iterator<Item = StreamEvent>, Error> {
        let tools_ref = tools.as_deref();
        let body = self.build_request(model, &messages, tools_ref, &settings);
        let mut obj = body.as_object().cloned().unwrap_or_default();
        obj.insert("stream".to_string(), serde_json::json!(true));
        let beta = needs_refusal_fallback(model);

        let mut req = self
            .http
            .post(self.messages_endpoint())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        if beta {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }

        let resp = req.json(&Value::Object(obj)).send()?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.text()?;
            return Err(Error::from_response(status, &body, "anthropic"));
        }

        let body = resp.text()?;
        Ok(AnthropicStreamIter::new(&body))
    }
}

// ---------------------------------------------------------------------------
// Anthropic SSE streaming iterator
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct AnthropicStreamIter {
    lines: std::sync::mpsc::IntoIter<String>,
    text_parts: Vec<String>,
    reasoning_parts: Vec<String>,
    tool_accum: std::collections::HashMap<usize, (String, String, String)>,
    thinking_accum: std::collections::HashMap<usize, Value>,
    stop_reason: Option<String>,
    usage: Option<TokenUsage>,
    done: bool,
}

impl AnthropicStreamIter {
    fn new(body: &str) -> Self {
        let lines: Vec<String> = body
            .lines()
            .filter(|l| l.starts_with("data: ") && l.trim() != "data: [DONE]")
            .map(|l| l[6..].to_string())
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        for line in lines {
            let _ = tx.send(line);
        }
        drop(tx);
        Self {
            lines: rx.into_iter(),
            text_parts: Vec::new(),
            reasoning_parts: Vec::new(),
            tool_accum: Default::default(),
            thinking_accum: Default::default(),
            stop_reason: None,
            usage: None,
            done: false,
        }
    }
}

impl Iterator for AnthropicStreamIter {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        for line in self.lines.by_ref() {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match kind {
                "message_start" => {
                    if let Some(msg) = event.get("message") {
                        if let Some(u) = msg.get("usage") {
                            self.usage = Some(TokenUsage {
                                input: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                                    as usize,
                                output: 0,
                                cache_read: u
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize,
                                cache_write: 0,
                            });
                        }
                    }
                }
                "content_block_start" => {
                    let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let block = event.get("content_block").and_then(|v| v.as_object())?;
                    let bkind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if bkind == "tool_use" {
                        self.tool_accum.insert(
                            idx,
                            (
                                block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                String::new(),
                            ),
                        );
                    } else if bkind == "thinking" {
                        self.thinking_accum.insert(idx, serde_json::json!({
                            "type": "thinking",
                            "thinking": block.get("thinking").and_then(|v| v.as_str()).unwrap_or(""),
                            "signature": block.get("signature").and_then(|v| v.as_str()).unwrap_or(""),
                        }));
                    }
                }
                "content_block_delta" => {
                    let delta = event.get("delta").and_then(|v| v.as_object())?;
                    let dkind = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                    match dkind {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                self.text_parts.push(text.to_string());
                                return Some(StreamEvent::TextDelta {
                                    text: text.to_string(),
                                });
                            }
                        }
                        "input_json_delta" => {
                            if let Some(acc) = self.tool_accum.get_mut(&idx) {
                                let partial = delta
                                    .get("partial_json")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                acc.2.push_str(partial);
                            }
                        }
                        "thinking_delta" => {
                            if let Some(acc) = self.thinking_accum.get_mut(&idx) {
                                let t =
                                    delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                                if !t.is_empty() {
                                    if let Some(obj) = acc.as_object_mut() {
                                        let cur = obj
                                            .get("thinking")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        obj.insert(
                                            "thinking".to_string(),
                                            serde_json::json!(cur.to_string() + t),
                                        );
                                    }
                                    self.reasoning_parts.push(t.to_string());
                                    return Some(StreamEvent::ReasoningDelta {
                                        reasoning: t.to_string(),
                                    });
                                }
                            }
                        }
                        "signature_delta" => {
                            if let Some(acc) = self.thinking_accum.get_mut(&idx) {
                                let sig = delta
                                    .get("signature")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if let Some(obj) = acc.as_object_mut() {
                                    let cur =
                                        obj.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                                    obj.insert(
                                        "signature".to_string(),
                                        serde_json::json!(cur.to_string() + sig),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_object()) {
                        if let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                            self.stop_reason = Some(stop_reason_map(reason).to_string());
                        }
                    }
                    if let Some(u) = event.get("usage").and_then(|v| v.as_object()) {
                        let out =
                            u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        if out > 0 {
                            self.usage = Some(TokenUsage {
                                input: self.usage.as_ref().map(|t| t.input).unwrap_or(0),
                                output: out,
                                cache_read: self.usage.as_ref().map(|t| t.cache_read).unwrap_or(0),
                                cache_write: 0,
                            });
                        }
                    }
                    self.done = true;
                    let tcs = self
                        .tool_accum
                        .drain()
                        .map(|(_, (id, name, args))| ToolCall {
                            id,
                            name,
                            arguments: parse_tool_arguments(&args),
                        })
                        .collect();
                    return Some(StreamEvent::Turn {
                        turn: AssistantTurn {
                            text: if self.text_parts.is_empty() {
                                None
                            } else {
                                Some(self.text_parts.join(""))
                            },
                            tool_calls: tcs,
                            finish_reason: self.stop_reason.clone(),
                            reasoning: if self.reasoning_parts.is_empty() {
                                None
                            } else {
                                Some(self.reasoning_parts.join(""))
                            },
                            usage: self.usage.clone(),
                        },
                    });
                }
                _ => {}
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

pub fn capabilities_for(_model: &str) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        vision: true,
        pdf: true,
        parallel_tool_calls: true,
        streaming: true,
    }
}

impl crate::router::Provider for AnthropicClient {
    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<AssistantTurn, crate::Error> {
        self.complete(model, messages, tools, settings)
    }

    fn stream(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<
        Box<dyn Iterator<Item = Result<crate::types::StreamEvent, crate::Error>> + Send>,
        crate::Error,
    > {
        let iter = self.stream(model, messages, tools, settings)?;
        Ok(Box::new(iter.map(Ok)))
    }

    fn capabilities(&self, model: &str) -> crate::types::ModelCapabilities {
        capabilities_for(model)
    }

    fn name(&self) -> &str {
        &self.vendor
    }
}
