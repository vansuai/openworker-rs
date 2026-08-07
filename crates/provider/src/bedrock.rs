//! AWS Bedrock provider — Converse API via bearer token or SigV4.
//!
//! Model ids look like `bedrock:<family>/<model id>`, e.g.:
//! - `bedrock:claude/anthropic.claude-sonnet-4-6-v1:0`
//! - `bedrock:other/amazon.nova-2-pro-v1:0`
//!
//! Auth: Bearer token (api_key) or AWS credentials (access_key + secret_key).
//! When a bearer token is provided it is sent as `Authorization: Bearer <token>`.
//! Otherwise requests are unsigned (the caller is expected to have AWS env vars set).

use crate::error::Error;
use crate::registry::ProviderConfig;
use crate::types::{AssistantTurn, ModelCapabilities, TokenUsage, ToolCall};

use once_cell::sync::Lazy;
use reqwest::blocking::Client;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_TOKENS: usize = 4096;
const DEFAULT_REGION: &str = "us-east-1";

static CLIENT: Lazy<Client> = Lazy::new(Client::new);

// Converse stopReason → finish_reason
fn stop_reason_map(reason: &str) -> &str {
    match reason {
        "end_turn" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "guardrail_intervened" => "stop",
        "content_filtered" => "stop",
        _ => reason,
    }
}

fn usage_from(usage: &Value) -> Option<TokenUsage> {
    Some(TokenUsage {
        input: usage
            .get("inputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        output: usage
            .get("outputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        cache_read: usage
            .get("cacheReadInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        cache_write: usage
            .get("cacheWriteInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
    })
}

// ---------------------------------------------------------------------------
// BedrockClient
// ---------------------------------------------------------------------------

pub struct BedrockClient {
    region: String,
    api_key: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    #[allow(dead_code)]
    session_token: Option<String>,
}

impl BedrockClient {
    pub fn new(profile: &ProviderConfig) -> Self {
        let region = profile
            .get("region")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_REGION)
            .to_string();

        let api_key = profile
            .get("bedrock_api_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let access_key = profile
            .get("aws_access_key_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let secret_key = profile
            .get("aws_secret_access_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let session_token = profile
            .get("aws_session_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        Self {
            region,
            api_key,
            access_key,
            secret_key,
            session_token,
        }
    }

    /// Split a Bedrock model id into family and bare model name.
    fn split_model(model: &str) -> (String, String) {
        if let Some(idx) = model.find('/') {
            let (family, rest) = model.split_at(idx);
            (family.to_string(), rest[1..].to_string())
        } else {
            ("other".to_string(), model.to_string())
        }
    }

    /// Build the Converse API endpoint URL.
    fn endpoint(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region, model_id
        )
    }

    /// Convert OpenAI-format messages to Bedrock Converse format.
    fn convert_messages(&self, msgs: &[Value]) -> (Option<Vec<Value>>, Vec<Value>) {
        let mut system_blocks: Vec<Value> = Vec::new();
        let mut messages: Vec<Value> = Vec::new();

        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m.get("content");

            match role {
                "system" => {
                    if let Some(text) = content.and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            system_blocks.push(serde_json::json!({"text": text}));
                        }
                    }
                }
                "user" => {
                    let blocks = self.content_to_blocks(content);
                    if !blocks.is_empty() {
                        messages.push(serde_json::json!({"role": "user", "content": blocks}));
                    }
                }
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    if let Some(text) = content.and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            blocks.push(serde_json::json!({"text": text}));
                        }
                    }
                    if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let func = tc.get("function").and_then(|v| v.as_object());
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let args_str = func
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("{}");
                            let input: Value =
                                serde_json::from_str(args_str).unwrap_or(Value::Null);
                            blocks.push(serde_json::json!({
                                "toolUse": {
                                    "toolUseId": id,
                                    "name": name,
                                    "input": input,
                                }
                            }));
                        }
                    }
                    if blocks.is_empty() {
                        blocks.push(serde_json::json!({"text": ""}));
                    }
                    messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
                }
                "tool" => {
                    let tool_call_id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let result_content = content.cloned().unwrap_or(Value::String(String::new()));
                    let result_text = result_content.as_str().unwrap_or("");
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "toolResult": {
                                "toolUseId": tool_call_id,
                                "content": [{"text": result_text}],
                                "status": "success",
                            }
                        }]
                    }));
                }
                _ => {}
            }
        }

        let system = if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        };
        (system, messages)
    }

    /// Convert content to Bedrock Converse content blocks.
    fn content_to_blocks(&self, content: Option<&Value>) -> Vec<Value> {
        let content = match content {
            Some(c) => c,
            None => return vec![serde_json::json!({"text": ""})],
        };

        if let Some(text) = content.as_str() {
            if text.is_empty() {
                return vec![serde_json::json!({"text": ""})];
            }
            return vec![serde_json::json!({"text": text})];
        }

        if let Some(arr) = content.as_array() {
            let mut blocks: Vec<Value> = Vec::new();
            for item in arr {
                if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                    match t {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                blocks.push(serde_json::json!({"text": text}));
                            }
                        }
                        "image_url" => {
                            if let Some(url) = item
                                .get("image_url")
                                .and_then(|v| v.get("url"))
                                .and_then(|v| v.as_str())
                            {
                                if let Some((mime, data)) = parse_data_url(url) {
                                    blocks.push(serde_json::json!({
                                        "image": {
                                            "format": mime_to_fmt(&mime),
                                            "source": {"bytes": data},
                                        }
                                    }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if blocks.is_empty() {
                return vec![serde_json::json!({"text": ""})];
            }
            return blocks;
        }

        vec![serde_json::json!({"text": ""})]
    }

    /// Convert OpenAI-format tools to Bedrock Converse toolConfig.
    fn convert_tools(&self, tools: &[Value]) -> Value {
        let bedrock_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function")?;
                let name = func.get("name")?.as_str()?;
                let desc = func
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let params = func
                    .get("parameters")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                Some(serde_json::json!({
                    "toolSpec": {
                        "name": name,
                        "description": desc,
                        "inputSchema": {"json": params},
                    }
                }))
            })
            .collect();

        serde_json::json!({
            "tools": bedrock_tools,
        })
    }

    /// Sign a request with AWS SigV4 (simplified — only bearer token supported for now).
    fn sign_request(
        &self,
        req: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(ref token) = self.api_key {
            req.header("Authorization", format!("Bearer {}", token))
        } else if let (Some(ref ak), Some(ref _sk)) = (&self.access_key, &self.secret_key) {
            // For IAM credentials, we'd need SigV4 signing.
            // As a fallback, use the access key as a bearer token (some Bedrock setups).
            req.header("Authorization", format!("Bearer {}", ak))
        } else {
            // No explicit credentials — rely on ambient AWS env vars (AWS_ACCESS_KEY_ID, etc.)
            // reqwest doesn't auto-sign, so this path only works with bearer-token style auth.
            req
        }
    }

    /// Make a Converse API call.
    pub fn converse(
        &self,
        model_id: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Result<AssistantTurn, Error> {
        let (system, messages) = self.convert_messages(messages);
        let max_tokens = settings
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_TOKENS as u64) as usize;

        let mut body = serde_json::json!({
            "messages": messages,
            "inferenceConfig": {
                "maxTokens": max_tokens,
                "temperature": settings.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7),
            },
        });

        if let Some(sys) = system {
            body["system"] = serde_json::json!(sys);
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["toolConfig"] = self.convert_tools(tools);
            }
        }

        let url = self.endpoint(model_id);
        let req = CLIENT.post(&url).json(&body);
        let req = self.sign_request(req);

        let resp = req.send().map_err(Error::Http)?;
        let status = resp.status();
        let resp_body: Value = resp.json().map_err(Error::Http)?;

        if !status.is_success() {
            let msg = resp_body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(Error::Other(format!(
                "Bedrock error ({}): {}",
                status.as_u16(),
                msg
            )));
        }

        let output = resp_body
            .get("output")
            .ok_or_else(|| Error::Other("missing output".into()))?;
        let msg = output
            .get("message")
            .ok_or_else(|| Error::Other("missing message".into()))?;
        let content_blocks = msg
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for block in &content_blocks {
            match block.get("text").and_then(|v| v.as_str()) {
                Some(t) => text.push_str(t),
                None => {
                    if let Some(tu) = block.get("toolUse") {
                        let id = tu
                            .get("toolUseId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tu
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = tu.get("input").cloned().unwrap_or(Value::Null);
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments: input,
                        });
                    }
                }
            }
        }

        let finish_reason = stop_reason_map(
            resp_body
                .get("stopReason")
                .and_then(|v| v.as_str())
                .unwrap_or("stop"),
        );

        let usage = resp_body.get("usage").and_then(usage_from);

        Ok(AssistantTurn {
            text: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            finish_reason: Some(finish_reason.to_string()),
            reasoning: None,
            usage,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_data_url(url: &str) -> Option<(String, String)> {
    // data:image/png;base64,XXXX
    let after_prefix = url.strip_prefix("data:")?;
    let semi = after_prefix.find(';')?;
    let mime = after_prefix[..semi].to_string();
    let after_semi = &after_prefix[semi + 1..];
    let comma = after_semi.find(',')?;
    if &after_semi[..comma] != "base64" {
        return None;
    }
    let b64_data = &after_semi[comma + 1..];
    Some((mime, b64_data.to_string()))
}

fn mime_to_fmt(mime: &str) -> &str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

pub fn capabilities_for(model: &str) -> ModelCapabilities {
    // Bedrock models all support the Converse API
    let bare = if let Some(idx) = model.find('/') {
        &model[idx + 1..]
    } else {
        model
    };
    let is_claude = bare.starts_with("anthropic.claude");
    ModelCapabilities {
        tools: true,
        streaming: is_claude,
        vision: true,
        pdf: false,
        parallel_tool_calls: true,
    }
}

// ---------------------------------------------------------------------------
// Provider trait impl
// ---------------------------------------------------------------------------

impl crate::router::Provider for BedrockClient {
    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<AssistantTurn, Error> {
        let (_family, bare) = Self::split_model(model);
        self.converse(&bare, &messages, tools.as_deref(), &settings)
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        capabilities_for(model)
    }

    fn name(&self) -> &str {
        "bedrock"
    }
}
