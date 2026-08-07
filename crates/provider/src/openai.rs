//! OpenAI-compatible provider — /v1/chat/completions.
//!
//! Covers: OpenAI, Ollama, OpenRouter, Azure OpenAI, and any OpenAI-compliant gateway.

use crate::error::Error;
use crate::types::{AssistantTurn, StreamEvent, TokenUsage, ToolCall};
use reqwest::blocking::Client;
use serde_json::{Map, Value};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_tool_calls(raw: &Value) -> Vec<ToolCall> {
    match raw {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|tc| {
                let id = tc.get("id")?.as_str()?.to_string();
                let fn_ = tc.get("function")?.as_object()?;
                let name = fn_.get("name")?.as_str().unwrap_or("").to_string();
                let args_str = fn_
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let arguments: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
                Some(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect(),
        _ => vec![],
    }
}

fn parse_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(String::from))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(""))
            }
        }
        _ => None,
    }
}

fn parse_reasoning(msg: &Value) -> Option<String> {
    let s = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .or_else(|| msg.get("reasoning").and_then(|v| v.as_str()))?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn extract_usage(response: &Value) -> Option<TokenUsage> {
    let usage = response.get("usage")?;
    let prompt = usage.get("prompt_tokens")?.as_u64()? as usize;
    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    Some(TokenUsage {
        input: prompt.saturating_sub(cached),
        output: completion,
        cache_read: cached,
        cache_write: 0,
    })
}

fn build_body(model: &str, messages: &[Value], tools: Option<&[Value]>, settings: &Value) -> Value {
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut obj = m.clone();
            if let Some(map) = obj.as_object_mut() {
                map.retain(|k, _| !k.starts_with('_'));
            }
            obj
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".to_string(), serde_json::json!(model));
    body.insert("messages".to_string(), serde_json::json!(messages));

    if let Some(tools) = tools {
        let defs: Vec<Value> = tools.to_vec();
        body.insert("tools".to_string(), serde_json::json!(defs));
    }

    if let Some(settings_map) = settings.as_object() {
        for (k, v) in settings_map {
            body.insert(k.clone(), v.clone());
        }
    }

    Value::Object(body)
}

// ---------------------------------------------------------------------------
// OpenAI client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenAiClient {
    http: Client,
    base_url: String,
    api_key: String,
    default_model: String,
}

impl OpenAiClient {
    pub fn new(base_url: String, api_key: String, default_model: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            http,
            base_url,
            api_key,
            default_model,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    pub fn complete(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<AssistantTurn, Error> {
        let tools_ref = tools.as_deref();
        let body = build_body(model, &messages, tools_ref, &settings);

        let resp = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()?;

        let status = resp.status().as_u16();
        let text = resp.text()?;
        if status != 200 {
            return Err(Error::from_response(status, &text, "openai"));
        }

        let json: Value = serde_json::from_str(&text)?;

        let choices = json
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| Error::Other("No choices in response".into()))?;
        let msg = choices
            .get("message")
            .ok_or_else(|| Error::Other("No message in choice".into()))?;

        let text = parse_text(msg);
        let tool_calls = match msg.get("tool_calls") {
            Some(Value::Array(arr)) => parse_tool_calls(&Value::Array(arr.clone())),
            _ => vec![],
        };
        let reasoning = parse_reasoning(msg);
        let finish_reason = choices
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(String::from);
        let usage = extract_usage(&json);

        Ok(AssistantTurn {
            text,
            tool_calls,
            finish_reason,
            reasoning,
            usage,
        })
    }

    pub fn stream(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<impl Iterator<Item = StreamEvent>, Error> {
        let tools_ref = tools.as_deref();
        let body = build_body(model, &messages, tools_ref, &settings);

        let mut obj = serde_json::from_value::<Map<String, Value>>(body)?;
        obj.insert("stream".to_string(), serde_json::json!(true));
        obj.insert(
            "stream_options".to_string(),
            serde_json::json!({"include_usage": true}),
        );

        let resp = self
            .http
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&Value::Object(obj))
            .send()?;

        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.text()?;
            return Err(Error::from_response(status, &body, "openai"));
        }

        let body = resp.text()?;
        Ok(SSEIterator::new(&body))
    }
}

// ---------------------------------------------------------------------------
// SSE iterator
// ---------------------------------------------------------------------------

pub struct SSEIterator {
    lines: std::sync::mpsc::IntoIter<String>,
    text_parts: Vec<String>,
    reasoning_parts: Vec<String>,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    done: bool,
}

impl SSEIterator {
    fn new(body: &str) -> Self {
        let lines: Vec<String> = body
            .lines()
            .filter(|l| l.starts_with("data: "))
            .filter(|l| l.trim() != "data: [DONE]")
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
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            done: false,
        }
    }
}

impl Iterator for SSEIterator {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        while let Some(line) = self.lines.next() {
            let Ok(json) = serde_json::from_str::<Value>(&line) else {
                continue;
            };

            // Usage-only chunk
            if json.get("choices").is_none() {
                if let Some(u) = extract_usage(&json) {
                    self.usage = Some(u);
                }
                continue;
            }

            let choices = json.get("choices")?.as_array()?;
            let delta = choices.first()?.get("delta")?;

            // Text delta
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                self.text_parts.push(text.to_string());
                return Some(StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            }

            // Reasoning delta
            if let Some(reasoning) = parse_reasoning(delta) {
                self.reasoning_parts.push(reasoning.clone());
                return Some(StreamEvent::ReasoningDelta { reasoning });
            }

            // Tool call delta
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let id = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let parsed: Value = serde_json::from_str(args).unwrap_or(Value::Null);
                    self.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: parsed,
                    });
                }
            }

            // Final chunk
            if let Some(fr) = choices
                .first()?
                .get("finish_reason")
                .and_then(|v| v.as_str())
            {
                if let Some(u) = extract_usage(&json) {
                    self.usage = Some(u);
                }
                self.done = true;
                let reasoning = if self.reasoning_parts.is_empty() {
                    None
                } else {
                    Some(self.reasoning_parts.join(""))
                };
                let text = if self.text_parts.is_empty() {
                    None
                } else {
                    Some(self.text_parts.join(""))
                };
                let tcs = std::mem::take(&mut self.tool_calls);
                return Some(StreamEvent::Turn {
                    turn: AssistantTurn {
                        text,
                        tool_calls: tcs,
                        finish_reason: Some(fr.to_string()),
                        reasoning,
                        usage: self.usage.clone(),
                    },
                });
            }
        }

        // Stream ended without finish_reason
        if !self.text_parts.is_empty()
            || !self.tool_calls.is_empty()
            || !self.reasoning_parts.is_empty()
        {
            self.done = true;
            let reasoning = if self.reasoning_parts.is_empty() {
                None
            } else {
                Some(self.reasoning_parts.join(""))
            };
            let text = if self.text_parts.is_empty() {
                None
            } else {
                Some(self.text_parts.join(""))
            };
            let tcs = std::mem::take(&mut self.tool_calls);
            return Some(StreamEvent::Turn {
                turn: AssistantTurn {
                    text,
                    tool_calls: tcs,
                    finish_reason: self.finish_reason.clone(),
                    reasoning,
                    usage: self.usage.clone(),
                },
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

pub fn capabilities_for(model: &str) -> crate::types::ModelCapabilities {
    let name = model.split(':').last().unwrap_or(model).to_lowercase();
    if name.starts_with("o1") || name.starts_with("o3") || name.starts_with("o4") {
        crate::types::ModelCapabilities {
            tools: true,
            vision: false,
            pdf: false,
            parallel_tool_calls: false,
            streaming: true,
        }
    } else if name.starts_with("gpt-5") || name.starts_with("gpt-4") {
        crate::types::ModelCapabilities::default_agentic_vision()
    } else {
        crate::types::ModelCapabilities::default_agentic()
    }
}

impl crate::router::Provider for OpenAiClient {
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
        "openai"
    }
}
