//! Google Gemini provider — native GenAI API (`generateContent`).

use crate::error::Error;
use crate::types::*;
use reqwest::blocking::Client;
use serde_json::{Map, Value};
use std::sync::mpsc;

const GEMINI_API_HOST: &str = "https://generativelanguage.googleapis.com";

fn usage_from(meta: &Value) -> Option<TokenUsage> {
    let prompt = meta.get("prompt_token_count")?.as_u64()? as usize;
    let cached = meta
        .get("cached_content_token_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let output = meta
        .get("candidates_token_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let thoughts = meta
        .get("thoughts_token_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    Some(TokenUsage {
        input: prompt.saturating_sub(cached),
        output: output + thoughts,
        cache_read: cached,
        cache_write: 0,
    })
}

fn finish_reason_map(reason: &str) -> &str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" => "stop",
        "MALFORMED_FUNCTION_CALL" => "stop",
        _ => reason,
    }
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Convert OpenAI-shaped messages to Gemini `contents` format.
/// Returns (contents, function_responses_str)
fn convert_messages(msgs: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut contents: Vec<Value> = Vec::new();
    let mut func_responses: Vec<String> = Vec::new();

    for msg in msgs {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

        if role == "system" {
            continue; // handled separately
        }

        if role == "tool" {
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let resp = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            func_responses.push(format!(
                r#"{{"functionResponse": {{"name": {}, "response": {{"result": {}}}}}}}"#,
                serde_json::json!(name),
                serde_json::json!(resp)
            ));
            continue;
        }

        let role_str = match role {
            "user" => "user",
            "assistant" => "model",
            _ => continue,
        };

        let parts = content_to_parts(msg.get("content"));

        if !parts.is_empty() {
            contents.push(serde_json::json!({
                "role": role_str,
                "parts": parts,
            }));
        }
    }

    // Append function responses as a single user message
    if !func_responses.is_empty() {
        let joined = format!("[{}]", func_responses.join(","));
        contents.push(serde_json::json!({
            "role": "user",
            "parts": [{ "text": joined }],
        }));
    }

    (contents, func_responses)
}

fn content_to_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![serde_json::json!({ "text": s })],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|part| {
                let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match kind {
                    "text" => {
                        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.is_empty() {
                            None
                        } else {
                            Some(serde_json::json!({ "text": text }))
                        }
                    }
                    "image_url" => {
                        let url = part
                            .get("image_url")
                            .and_then(|v| v.as_object())
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        image_part(url)
                    }
                    _ => None,
                }
            })
            .collect(),
        _ => vec![],
    }
}

fn image_part(url: &str) -> Option<Value> {
    let base64_start = url.find(";base64,")?;
    let media_type = &url[5..base64_start];
    let data = &url[base64_start + 8..];
    Some(serde_json::json!({
        "inlineData": { "mimeType": media_type, "data": data }
    }))
}

fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let function = t.get("function").and_then(|v| v.as_object())?;
            let name = function.get("name").and_then(|v| v.as_str())?;
            let description = function.get("description").and_then(|v| v.as_str());
            let params = function.get("parameters").and_then(|v| v.as_object());

            let mut decl: Map<String, Value> = serde_json::json!({ "name": name })
                .as_object()
                .cloned()
                .unwrap_or_default();
            if let Some(d) = description {
                decl.insert("description".to_string(), serde_json::json!(d));
            }
            if let Some(p) = params {
                let props = p
                    .get("properties")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("object");
                decl.insert(
                    "parameters".to_string(),
                    serde_json::json!({ "type": ptype, "properties": props }),
                );
            }
            Some(Value::Object(decl))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Gemini client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GeminiClient {
    http: Client,
    api_key: String,
    default_model: String,
}

impl GeminiClient {
    pub fn new(api_key: String, default_model: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            default_model,
        }
    }

    fn api_url(&self, model: &str, stream: bool) -> String {
        let sp = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        format!(
            "{}/v1beta/models/{}:{}?key={}",
            GEMINI_API_HOST, model, sp, self.api_key
        )
    }

    fn build_request(
        &self,
        _model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Value {
        let (contents, _) = convert_messages(messages);

        let system_instruction = {
            let sys_msgs: Vec<&Value> = messages
                .iter()
                .take_while(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
                .collect();
            if sys_msgs.is_empty() {
                None
            } else {
                let text: String = sys_msgs
                    .iter()
                    .filter_map(|m| m.get("content").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
        };

        let mut body = Map::new();
        body.insert("contents".to_string(), serde_json::json!(contents));

        if let Some(si) = system_instruction {
            body.insert(
                "systemInstruction".to_string(),
                serde_json::json!({
                    "parts": [{ "text": si }]
                }),
            );
        }

        if let Some(tools) = tools {
            let decls = convert_tools(tools);
            if !decls.is_empty() {
                body.insert(
                    "tools".to_string(),
                    serde_json::json!([{ "functionDeclarations": decls }]),
                );
            }
        }

        let whitelist = [
            "temperature",
            "topP",
            "topK",
            "maxOutputTokens",
            "stopSequences",
        ];
        for k in &whitelist {
            if let Some(v) = settings.get(*k) {
                body.insert(k.to_string(), v.clone());
            }
        }

        Value::Object(body)
    }

    pub fn complete(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<AssistantTurn, Error> {
        let tools_ref = tools.as_deref();
        let body = self.build_request(model, &messages, tools_ref, &settings);

        let resp = self
            .http
            .post(self.api_url(model, false))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()?;

        let status = resp.status().as_u16();
        let text = resp.text()?;
        if status != 200 {
            return Err(Error::from_response(status, &text, "gemini"));
        }

        let json: Value = serde_json::from_str(&text)?;
        self.parse_response(&json)
    }

    pub fn stream(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<impl Iterator<Item = StreamEvent>, Error> {
        let tools_ref = tools.as_deref();
        let body = self.build_request(model, &messages, tools_ref, &settings);

        let resp = self
            .http
            .post(self.api_url(model, true))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()?;

        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.text()?;
            return Err(Error::from_response(status, &body, "gemini"));
        }

        let body = resp.text()?;
        Ok(GeminiStreamIter::new(&body))
    }

    fn parse_response(&self, json: &Value) -> Result<AssistantTurn, Error> {
        let candidates = json
            .get("candidates")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| Error::Other("No candidates in Gemini response".into()))?;

        let content = candidates
            .get("content")
            .and_then(|v| v.as_object())
            .ok_or_else(|| Error::Other("No content in candidate".into()))?;

        let parts_arr = content.get("parts");
        let parts = parts_arr.and_then(|v| v.as_array());

        let finish_reason = candidates
            .get("finishReason")
            .and_then(|v| v.as_str())
            .map(finish_reason_map)
            .map(String::from);

        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for part in parts.into_iter().flatten() {
            // Thought parts (Gemini 3 reasoning summaries) are skipped for text
            let is_thought = part
                .get("thought")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !is_thought {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
            if let Some(fn_) = part.get("functionCall").and_then(|v| v.as_object()) {
                let name = fn_
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fn_.get("args").cloned().unwrap_or(serde_json::Value::Null);
                let id = format!("call_{}", tool_calls.len());
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: args,
                });
            }
        }

        let usage = candidates.get("usageMetadata").and_then(usage_from);

        Ok(AssistantTurn {
            text: if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join(""))
            },
            tool_calls,
            finish_reason,
            reasoning: None,
            usage,
        })
    }
}

// ---------------------------------------------------------------------------
// Gemini SSE stream iterator
// ---------------------------------------------------------------------------

pub struct GeminiStreamIter {
    lines: mpsc::IntoIter<String>,
    text_parts: Vec<String>,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    done: bool,
}

impl GeminiStreamIter {
    fn new(body: &str) -> Self {
        let lines: Vec<String> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
        let (tx, rx) = mpsc::channel();
        for line in lines {
            let _ = tx.send(line);
        }
        drop(tx);
        Self {
            lines: rx.into_iter(),
            text_parts: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            done: false,
        }
    }
}

impl Iterator for GeminiStreamIter {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        while let Some(line) = self.lines.next() {
            // Gemini SSE: each line is a JSON array: [{...}, {...}]
            let Ok(entries) = serde_json::from_str::<Vec<Value>>(&line) else {
                continue;
            };

            for entry in &entries {
                if let Some(candidates) = entry.get("candidates").and_then(|v| v.as_array()) {
                    for candidate in candidates {
                        // Check finish reason
                        if let Some(reason) = candidate.get("finishReason").and_then(|v| v.as_str())
                        {
                            self.finish_reason = Some(finish_reason_map(reason).to_string());
                        }

                        // Extract usage
                        if let Some(meta) = candidate.get("usageMetadata") {
                            if let Some(u) = usage_from(meta) {
                                self.usage = Some(u);
                            }
                        }

                        // Extract content parts
                        if let Some(content) = candidate.get("content") {
                            if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
                                for part in parts {
                                    // Text delta
                                    let is_thought = part
                                        .get("thought")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    if !is_thought {
                                        if let Some(text) =
                                            part.get("text").and_then(|v| v.as_str())
                                        {
                                            self.text_parts.push(text.to_string());
                                            return Some(StreamEvent::TextDelta {
                                                text: text.to_string(),
                                            });
                                        }
                                    } else if let Some(text) =
                                        part.get("text").and_then(|v| v.as_str())
                                    {
                                        // Reasoning/thought content
                                        return Some(StreamEvent::ReasoningDelta {
                                            reasoning: text.to_string(),
                                        });
                                    }

                                    // Function call
                                    if let Some(fn_) =
                                        part.get("functionCall").and_then(|v| v.as_object())
                                    {
                                        let name = fn_
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let args = fn_
                                            .get("args")
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Null);
                                        let id = format!("call_{}", self.tool_calls.len());
                                        self.tool_calls.push(ToolCall {
                                            id,
                                            name,
                                            arguments: args,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // If there are no more lines, emit final turn
            if self.lines.size_hint().0 == 0 {
                break;
            }
        }

        // Emit final turn with accumulated content
        self.done = true;
        Some(StreamEvent::Turn {
            turn: AssistantTurn {
                text: if self.text_parts.is_empty() {
                    None
                } else {
                    Some(self.text_parts.join(""))
                },
                tool_calls: std::mem::take(&mut self.tool_calls),
                finish_reason: self.finish_reason.take(),
                reasoning: None,
                usage: self.usage.take(),
            },
        })
    }
}

pub fn capabilities_for(_model: &str) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        vision: true,
        pdf: true,
        parallel_tool_calls: true,
        streaming: true,
    }
}
