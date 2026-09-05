//! OpenAI Responses API provider — `/v1/responses`.
//!
//! Upstream: `coworker/providers/openai_responses.py`.
//!
//! Native path for GPT-5.x reasoning + tools (Chat Completions rejects function tools
//! with non-`none` reasoning_effort). Converts canonical OpenAI-chat history to Responses
//! `input` items, streams reasoning summaries, and keeps CoT continuity via
//! `store: false` + `include: ["reasoning.encrypted_content"]`.

use crate::error::Error;
use crate::types::{AssistantTurn, StreamEvent, TokenUsage, ToolCall};
use reqwest::blocking::Client;
use serde_json::{json, Map, Value};
use std::time::Duration;

const DEFAULT_BASE: &str = "https://api.openai.com/v1";
const SETTINGS_WHITELIST: &[&str] = &[
    "temperature",
    "top_p",
    "max_output_tokens",
    "tool_choice",
    "parallel_tool_calls",
];

// ---------------------------------------------------------------------------
// Message / tool conversion
// ---------------------------------------------------------------------------

fn user_content(content: &Value) -> Value {
    match content {
        Value::String(_) => content.clone(),
        Value::Array(parts) => {
            let converted: Vec<Value> = parts
                .iter()
                .filter_map(|part| {
                    let kind = part.get("type")?.as_str()?;
                    match kind {
                        "text" => Some(json!({
                            "type": "input_text",
                            "text": part.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                        })),
                        "image_url" => {
                            let url = part
                                .get("image_url")
                                .and_then(|u| u.get("url"))
                                .and_then(|u| u.as_str())
                                .unwrap_or("");
                            Some(json!({
                                "type": "input_image",
                                "image_url": url,
                            }))
                        }
                        "file" => {
                            let file = part.get("file").cloned().unwrap_or(json!({}));
                            let mut entry = Map::new();
                            entry.insert("type".into(), json!("input_file"));
                            if let Some(name) = file.get("filename") {
                                entry.insert("filename".into(), name.clone());
                            }
                            if let Some(data) = file.get("file_data") {
                                entry.insert("file_data".into(), data.clone());
                            }
                            Some(Value::Object(entry))
                        }
                        _ => None,
                    }
                })
                .collect();
            Value::Array(converted)
        }
        _ => Value::Null,
    }
}

fn synthesized_items(message: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            items.push(json!({"role": "assistant", "content": text}));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for call in calls {
            let function = call.get("function").cloned().unwrap_or(json!({}));
            let arguments = match function.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            items.push(json!({
                "type": "function_call",
                "call_id": call.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                "name": function.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                "arguments": arguments,
            }));
        }
    }
    items
}

/// Canonical OpenAI-chat history → (`instructions`, Responses `input` items).
pub fn convert_messages(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < messages.len()
        && messages[index].get("role").and_then(|r| r.as_str()) == Some("system")
    {
        if let Some(content) = messages[index].get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                system_parts.push(content.to_string());
            }
        }
        index += 1;
    }

    let mut items: Vec<Value> = Vec::new();
    for message in &messages[index..] {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        items.push(json!({"role": "system", "content": text}));
                    }
                }
            }
            "user" => {
                let content = user_content(message.get("content").unwrap_or(&Value::Null));
                let nonempty = match &content {
                    Value::String(s) => !s.is_empty(),
                    Value::Array(a) => !a.is_empty(),
                    _ => false,
                };
                if nonempty {
                    items.push(json!({"role": "user", "content": content}));
                }
            }
            "assistant" => {
                let replay = message
                    .get("_openai")
                    .and_then(|s| s.get("items"))
                    .and_then(|i| i.as_array());
                if let Some(replay) = replay {
                    if !replay.is_empty() {
                        items.extend(replay.iter().cloned());
                        continue;
                    }
                }
                items.extend(synthesized_items(message));
            }
            "tool" => {
                let content = message.get("content").cloned().unwrap_or(Value::Null);
                let output = match content {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": message.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or(""),
                    "output": output,
                }));
            }
            _ => {}
        }
    }

    let instructions = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (instructions, items)
}

/// OpenAI chat function schemas → Responses FLAT tool entries (no nested `function`).
pub fn convert_tools(tools: Option<&[Value]>) -> Vec<Value> {
    let mut converted = Vec::new();
    for tool in tools.unwrap_or(&[]) {
        let function = tool.get("function").cloned().unwrap_or(json!({}));
        let Some(name) = function.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut entry = Map::new();
        entry.insert("type".into(), json!("function"));
        entry.insert("name".into(), json!(name));
        if let Some(desc) = function.get("description") {
            if !desc.is_null() {
                entry.insert("description".into(), desc.clone());
            }
        }
        if let Some(params) = function.get("parameters") {
            if !params.is_null() {
                entry.insert("parameters".into(), params.clone());
            }
        }
        converted.push(Value::Object(entry));
    }
    converted
}

fn parse_arguments(raw: &Value) -> Value {
    match raw {
        Value::Object(_) => raw.clone(),
        Value::String(s) if s.is_empty() => json!({}),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => json!({ "_raw": s }),
            Err(_) => json!({ "_raw": s }),
        },
        Value::Null => json!({}),
        other => json!({ "_raw": other }),
    }
}

fn usage_from(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let prompt = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    Some(TokenUsage {
        input: prompt.saturating_sub(cached),
        output,
        cache_read: cached,
        cache_write: 0,
    })
}

/// One Responses result → an [`AssistantTurn`].
pub fn parse_response(response: &Value) -> AssistantTurn {
    let items = response
        .get("output")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();

    let mut texts: Vec<String> = Vec::new();
    let mut summaries: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for item in &items {
        let kind = item.get("type").and_then(|t| t.as_str());
        if kind == Some("message") || (kind.is_none() && item.get("content").is_some()) {
            match item.get("content") {
                Some(Value::String(s)) => texts.push(s.clone()),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    texts.push(text.to_string());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        } else if kind == Some("reasoning") {
            if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                for part in summary {
                    let text = part
                        .get("text")
                        .and_then(|t| t.as_str())
                        .or_else(|| part.as_str());
                    if let Some(text) = text {
                        if !text.is_empty() {
                            summaries.push(text.to_string());
                        }
                    }
                }
            }
        } else if kind == Some("function_call") {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = parse_arguments(item.get("arguments").unwrap_or(&Value::Null));
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    let incomplete_reason = response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str());
    let finish = if !tool_calls.is_empty() {
        "tool_calls"
    } else if incomplete_reason == Some("max_output_tokens") {
        "length"
    } else {
        "stop"
    };

    AssistantTurn {
        text: if texts.is_empty() {
            None
        } else {
            Some(texts.join(""))
        },
        tool_calls,
        finish_reason: Some(finish.to_string()),
        reasoning: if summaries.is_empty() {
            None
        } else {
            Some(summaries.join(""))
        },
        usage: usage_from(response.get("usage")),
    }
}

/// Drop the top-level request param named in an unsupported-parameter error.
fn param_fix_retry(body: &mut Map<String, Value>, err_text: &str) -> bool {
    let lower = err_text.to_lowercase();
    let idx = lower
        .find("unsupported parameter")
        .or_else(|| lower.find("unsupported value"))
        .or_else(|| lower.find("unsupported parameters"))
        .or_else(|| lower.find("unsupported values"));
    let Some(idx) = idx else {
        return false;
    };
    let after = &err_text[idx..];
    let Some(q1) = after.find('\'') else {
        return false;
    };
    let rest = &after[q1 + 1..];
    let Some(q2) = rest.find('\'') else {
        return false;
    };
    let param = &rest[..q2];
    let top = param.split(['.', '[']).next().unwrap_or(param);
    if top == "model" || top == "input" {
        return false;
    }
    body.remove(top).is_some()
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenAiResponsesClient {
    http: Client,
    base_url: String,
    api_key: String,
    #[allow(dead_code)]
    default_model: String,
    name: String,
    reasoning_summary: bool,
}

impl OpenAiResponsesClient {
    pub fn new(base_url: String, api_key: String, default_model: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .unwrap_or_else(|_| Client::new());
        let base = base_url.trim().trim_end_matches('/').to_string();
        Self {
            http,
            base_url: if base.is_empty() {
                DEFAULT_BASE.to_string()
            } else {
                base
            },
            api_key,
            default_model,
            name: "openai".into(),
            reasoning_summary: true,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_reasoning_summary(mut self, enabled: bool) -> Self {
        self.reasoning_summary = enabled;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn build_body(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Map<String, Value> {
        let (instructions, items) = convert_messages(messages);
        let mut settings_map = settings.as_object().cloned().unwrap_or_default();
        if settings_map.contains_key("max_tokens") && !settings_map.contains_key("max_output_tokens")
        {
            if let Some(v) = settings_map.remove("max_tokens") {
                settings_map.insert("max_output_tokens".into(), v);
            }
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert("input".into(), json!(items));
        body.insert("store".into(), json!(false));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if self.reasoning_summary {
            body.insert("reasoning".into(), json!({"summary": "auto"}));
        }
        if let Some(instructions) = instructions {
            body.insert("instructions".into(), json!(instructions));
        }
        if let Some(tools) = tools {
            let converted = convert_tools(Some(tools));
            if !converted.is_empty() {
                body.insert("tools".into(), json!(converted));
            }
        }
        for key in SETTINGS_WHITELIST {
            if let Some(v) = settings_map.get(*key) {
                body.insert((*key).to_string(), v.clone());
            }
        }
        body
    }

    fn post_json(&self, body: &Value, stream: bool) -> Result<(u16, String), Error> {
        let mut req = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");
        if stream {
            req = req.header("Accept", "text/event-stream");
        }
        let resp = req.json(body).send()?;
        let status = resp.status().as_u16();
        let text = resp.text()?;
        Ok((status, text))
    }

    fn create_with_retries(&self, mut body: Map<String, Value>) -> Result<Value, Error> {
        for _ in 0..3 {
            let (status, text) = self.post_json(&Value::Object(body.clone()), false)?;
            if status == 200 {
                return Ok(serde_json::from_str(&text)?);
            }
            if param_fix_retry(&mut body, &text) {
                continue;
            }
            return Err(Error::from_response(status, &text, &self.name));
        }
        let (status, text) = self.post_json(&Value::Object(body), false)?;
        if status != 200 {
            return Err(Error::from_response(status, &text, &self.name));
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub fn complete(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<AssistantTurn, Error> {
        let body = self.build_body(model, &messages, tools.as_deref(), &settings);
        let json = self.create_with_retries(body)?;
        Ok(parse_response(&json))
    }

    pub fn stream(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<impl Iterator<Item = StreamEvent>, Error> {
        let mut body = self.build_body(model, &messages, tools.as_deref(), &settings);
        body.insert("stream".into(), json!(true));

        // Param-fix retries on the non-stream path first when the server rejects knobs.
        let (status, text) = self.post_json(&Value::Object(body.clone()), true)?;
        if status != 200 {
            let mut retry_body = body.clone();
            if param_fix_retry(&mut retry_body, &text) {
                // Fall back to blocking complete + single Turn when streaming is awkward.
                let turn = {
                    retry_body.remove("stream");
                    let json = self.create_with_retries(retry_body)?;
                    parse_response(&json)
                };
                return Ok(ResponsesSSEIterator::from_turn(turn));
            }
            // Prefer complete()+single Turn when SSE endpoint rejects stream.
            let mut fallback = body;
            fallback.remove("stream");
            let json = self.create_with_retries(fallback)?;
            return Ok(ResponsesSSEIterator::from_turn(parse_response(&json)));
        }

        Ok(ResponsesSSEIterator::from_sse(&text))
    }
}

// ---------------------------------------------------------------------------
// SSE iterator (Responses wire)
// ---------------------------------------------------------------------------

pub struct ResponsesSSEIterator {
    events: std::vec::IntoIter<StreamEvent>,
}

impl ResponsesSSEIterator {
    fn from_turn(turn: AssistantTurn) -> Self {
        Self {
            events: vec![StreamEvent::Turn { turn }].into_iter(),
        }
    }

    fn from_sse(body: &str) -> Self {
        let mut text_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        let mut done_items: Vec<Value> = Vec::new();
        let mut final_response: Option<Value> = None;
        let mut out: Vec<StreamEvent> = Vec::new();

        for line in body.lines() {
            let data = line.strip_prefix("data: ").unwrap_or("");
            if data.is_empty() || data.trim() == "[DONE]" {
                continue;
            }
            let Ok(json) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let kind = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match kind {
                "response.output_text.delta" => {
                    if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                        if !delta.is_empty() {
                            text_parts.push(delta.to_string());
                            out.push(StreamEvent::TextDelta {
                                text: delta.to_string(),
                            });
                        }
                    }
                }
                "response.reasoning_summary_text.delta" => {
                    if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                        if !delta.is_empty() {
                            reasoning_parts.push(delta.to_string());
                            out.push(StreamEvent::ReasoningDelta {
                                reasoning: delta.to_string(),
                            });
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = json.get("item") {
                        done_items.push(item.clone());
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    final_response = json.get("response").cloned();
                }
                _ => {}
            }
        }

        let mut turn = if let Some(mut final_resp) = final_response {
            let empty_output = final_resp
                .get("output")
                .and_then(|o| o.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if empty_output && !done_items.is_empty() {
                if let Some(obj) = final_resp.as_object_mut() {
                    obj.insert("output".into(), Value::Array(done_items));
                }
            }
            parse_response(&final_resp)
        } else {
            AssistantTurn {
                text: if text_parts.is_empty() {
                    None
                } else {
                    Some(text_parts.join(""))
                },
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                reasoning: if reasoning_parts.is_empty() {
                    None
                } else {
                    Some(reasoning_parts.join(""))
                },
                usage: None,
            }
        };

        if turn.text.is_none() && turn.tool_calls.is_empty() && !text_parts.is_empty() {
            turn.text = Some(text_parts.join(""));
        }
        out.push(StreamEvent::Turn { turn });
        Self {
            events: out.into_iter(),
        }
    }
}

impl Iterator for ResponsesSSEIterator {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.events.next()
    }
}

// ---------------------------------------------------------------------------
// Capabilities + Provider trait
// ---------------------------------------------------------------------------

pub fn capabilities_for(model: &str) -> crate::types::ModelCapabilities {
    crate::openai::capabilities_for(model)
}

impl crate::router::Provider for OpenAiResponsesClient {
    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<crate::types::AssistantTurn, crate::Error> {
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
        &self.name
    }
}

/// Whether stock OpenAI (Responses API) should be used for this profile.
pub fn is_stock_openai_base(base_url: Option<&str>) -> bool {
    match base_url.map(str::trim).filter(|s| !s.is_empty()) {
        None => true,
        Some(u) => {
            let u = u.trim_end_matches('/');
            u == "https://api.openai.com/v1" || u == "https://api.openai.com"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn convert_extracts_leading_system_as_instructions() {
        let (instructions, items) = convert_messages(&[
            json!({"role": "system", "content": "be helpful"}),
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hi"}),
        ]);
        assert_eq!(instructions.as_deref(), Some("be helpful\n\nbe brief"));
        assert_eq!(items, vec![json!({"role": "user", "content": "hi"})]);
    }

    #[test]
    fn convert_mid_thread_system_stays_a_message() {
        let (_, items) = convert_messages(&[
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "steering"}),
        ]);
        assert_eq!(items[1], json!({"role": "system", "content": "steering"}));
    }

    #[test]
    fn convert_user_parts_to_input_parts() {
        let (_, items) = convert_messages(&[json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                {"type": "file", "file": {
                    "filename": "report.pdf",
                    "file_data": "data:application/pdf;base64,JVBERi0="
                }},
            ],
        })]);
        assert_eq!(
            items[0]["content"],
            json!([
                {"type": "input_text", "text": "what is this"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="},
                {
                    "type": "input_file",
                    "filename": "report.pdf",
                    "file_data": "data:application/pdf;base64,JVBERi0=",
                },
            ])
        );
    }

    #[test]
    fn convert_synthesizes_assistant_and_tool_items() {
        let (_, items) = convert_messages(&[
            json!({"role": "user", "content": "go"}),
            json!({
                "role": "assistant",
                "content": "on it",
                "tool_calls": [{
                    "id": "toolu_abc",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{\"x\": 1}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "toolu_abc", "content": "{\"ok\": true}"}),
        ]);
        assert_eq!(items[1], json!({"role": "assistant", "content": "on it"}));
        assert_eq!(
            items[2],
            json!({
                "type": "function_call",
                "call_id": "toolu_abc",
                "name": "f",
                "arguments": "{\"x\": 1}",
            })
        );
        assert_eq!(
            items[3],
            json!({
                "type": "function_call_output",
                "call_id": "toolu_abc",
                "output": "{\"ok\": true}",
            })
        );
    }

    #[test]
    fn convert_replays_openai_sidecar_verbatim() {
        let sidecar = vec![
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "blob",
            }),
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "on it"}],
            }),
            json!({
                "type": "function_call",
                "id": "fc_call_1",
                "call_id": "call_1",
                "name": "f",
                "arguments": "{}",
            }),
        ];
        let (_, items) = convert_messages(&[
            json!({"role": "user", "content": "go"}),
            json!({
                "role": "assistant",
                "content": "on it",
                "tool_calls": [{"id": "call_1", "function": {"name": "f", "arguments": "{}"}}],
                "_openai": {"items": sidecar.clone()},
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "done"}),
        ]);
        assert_eq!(&items[1..4], &sidecar[..]);
        assert_eq!(items[4]["type"], "function_call_output");
    }

    #[test]
    fn convert_tools_flattens_function_schemas() {
        let tools = convert_tools(Some(&[
            json!({"type": "function", "function": {"name": "bare"}}),
            json!({
                "type": "function",
                "function": {
                    "name": "full",
                    "description": "does things",
                    "parameters": {
                        "type": "object",
                        "properties": {"x": {"type": "integer"}},
                    },
                },
            }),
        ]));
        assert_eq!(tools[0], json!({"type": "function", "name": "bare"}));
        assert_eq!(tools[1]["name"], "full");
        assert!(tools[1].get("function").is_none());
        assert_eq!(
            tools[1]["parameters"]["properties"],
            json!({"x": {"type": "integer"}})
        );
        assert!(convert_tools(None).is_empty());
    }

    #[test]
    fn parse_response_text_tools_reasoning_and_usage() {
        let response = json!({
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "plan"}],
                    "encrypted_content": "enc",
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello"}],
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "f",
                    "arguments": "{\"x\": 1}",
                },
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "input_tokens_details": {"cached_tokens": 40},
            },
        });
        let turn = parse_response(&response);
        assert_eq!(turn.text.as_deref(), Some("hello"));
        assert_eq!(turn.reasoning.as_deref(), Some("plan"));
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "f");
        assert_eq!(turn.tool_calls[0].arguments["x"], 1);
        let usage = turn.usage.expect("usage");
        assert_eq!(usage.input, 60);
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.output, 20);
    }

    #[test]
    fn parse_response_length_finish_reason() {
        let response = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "cut"}],
            }],
            "incomplete_details": {"reason": "max_output_tokens"},
        });
        let turn = parse_response(&response);
        assert_eq!(turn.finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn is_stock_openai_base_detection() {
        assert!(is_stock_openai_base(None));
        assert!(is_stock_openai_base(Some("")));
        assert!(is_stock_openai_base(Some("https://api.openai.com/v1")));
        assert!(is_stock_openai_base(Some("https://api.openai.com/v1/")));
        assert!(!is_stock_openai_base(Some("https://azure.example/openai/v1")));
    }

    #[test]
    fn sse_emits_text_delta_then_turn() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\"}]}]}}\n",
        );
        let events: Vec<_> = ResponsesSSEIterator::from_sse(body).collect();
        assert!(matches!(events[0], StreamEvent::TextDelta { .. }));
        assert!(matches!(events.last(), Some(StreamEvent::Turn { .. })));
    }
}
