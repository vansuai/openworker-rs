//! Google Vertex AI provider — three wire paths by model family.
//!
//! Model ids look like `vertex:<family>/<model id>`:
//! - `vertex:gemini/gemini-3.6-flash` → Gemini API on Vertex
//! - `vertex:claude/claude-sonnet-4-6` → Anthropic Messages API on Vertex
//! - `vertex:openweight/meta/llama-4-maverick` → OpenAI-compatible MaaS endpoint

use crate::error::Error;
use crate::registry::ProviderConfig;
use crate::types::{AssistantTurn, ModelCapabilities, TokenUsage, ToolCall};

use base64::Engine;
use once_cell::sync::Lazy;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::sync::Mutex;

static CLIENT: Lazy<Client> = Lazy::new(Client::new);
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

// ---------------------------------------------------------------------------
// JWT / OAuth2
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct SaKey {
    client_email: Option<String>,
    private_key: Option<String>,
    token_uri: Option<String>,
}

fn jwt_token(sa_json: &str) -> Result<String, String> {
    let sa: SaKey =
        serde_json::from_str(sa_json).map_err(|e| format!("bad service account JSON: {e}"))?;
    let email = sa.client_email.as_deref().unwrap_or("");
    let pk = sa.private_key.as_deref().unwrap_or("");
    let uri = sa.token_uri.as_deref().unwrap_or(TOKEN_URL);
    if email.is_empty() || pk.is_empty() {
        return Err("missing client_email or private_key".into());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let claims = serde_json::json!({"iss":email,"scope":SCOPES.join(" "),"aud":uri,"iat":now,"exp":now+3600});
    let header = serde_json::json!({"alg":"RS256","typ":"JWT"});
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let _hb = b64.encode(claims.to_string().as_bytes());
    let _cb = b64.encode(header.to_string().as_bytes()); // FIXED: header first, then claims
    let hb = b64.encode(header.to_string().as_bytes());
    let cb = b64.encode(claims.to_string().as_bytes());
    let si = format!("{}.{}", hb, cb);

    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pk.as_bytes())
        .map_err(|e| format!("bad private key: {e}"))?;
    let sig = jsonwebtoken::crypto::sign(si.as_bytes(), &key, jsonwebtoken::Algorithm::RS256)
        .map_err(|e| format!("sign failed: {e}"))?;
    let jwt = format!("{}.{}", si, b64.encode(&sig));

    let resp = CLIENT
        .post(uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .map_err(|e| format!("token req: {e}"))?;
    let body: Value = resp.json().map_err(|e| format!("parse: {e}"))?;
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("no access_token: {body}"))
}

// ---------------------------------------------------------------------------
// VertexClient
// ---------------------------------------------------------------------------

pub struct VertexClient {
    project: Option<String>,
    location: String,
    api_key: Option<String>,
    sa_json: Option<String>,
    token_cache: Mutex<Option<(String, u64)>>,
}

impl VertexClient {
    pub fn new(profile: &ProviderConfig) -> Self {
        Self {
            project: profile
                .get("project")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            location: profile
                .get("region")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("us-central1")
                .into(),
            api_key: profile
                .get("vertex_api_key")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            sa_json: profile
                .get("service_account_json")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            token_cache: Mutex::new(None),
        }
    }

    fn split_model(model: &str) -> (String, String) {
        const FAMS: &[&str] = &["gemini", "claude", "openweight"];
        if let Some(i) = model.find('/') {
            let (f, r) = model.split_at(i);
            if FAMS.contains(&f) {
                return (f.into(), r[1..].into());
            }
        }
        if model.starts_with("gemini") {
            ("gemini".into(), model.into())
        } else if model.starts_with("claude") {
            ("claude".into(), model.into())
        } else {
            ("openweight".into(), model.into())
        }
    }

    fn bearer(&self) -> Result<String, Error> {
        if let Some(ref k) = self.api_key {
            return Ok(k.clone());
        }
        {
            let c = self.token_cache.lock().unwrap();
            if let Some((ref t, exp)) = *c {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if now + 300 < exp {
                    return Ok(t.clone());
                }
            }
        }
        let sa = self.sa_json.as_deref().unwrap_or("");
        if sa.is_empty() {
            if let Ok(p) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    let tok = jwt_token(&content).map_err(Error::Other)?;
                    let exp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        + 3500;
                    self.token_cache.lock().unwrap().replace((tok.clone(), exp));
                    return Ok(tok);
                }
            }
            return Err(Error::Other("no Google credentials — set GOOGLE_APPLICATION_CREDENTIALS or provide service_account_json".into()));
        }
        let tok = jwt_token(sa).map_err(Error::Other)?;
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3500;
        self.token_cache.lock().unwrap().replace((tok.clone(), exp));
        Ok(tok)
    }

    // -- Gemini family ---------------------------------------------------------

    fn call_gemini(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Result<AssistantTurn, Error> {
        let token = self.bearer()?;
        let loc = &self.location;
        let proj = self.project.as_deref().unwrap_or("");

        let url = if self.api_key.is_some() {
            format!("https://aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}/publishers/google/models/{model}:generateContent")
        } else {
            format!("https://{loc}-aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}/publishers/google/models/{model}:generateContent")
        };

        let (contents, sys) = self.to_gemini(messages);
        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": settings.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(8192),
                "temperature": settings.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7),
            },
        });
        if let Some(s) = sys {
            body["systemInstruction"] = serde_json::json!({"parts":[{"text":s}]});
        }
        if let Some(t) = tools {
            if !t.is_empty() {
                body["tools"] = serde_json::Value::Array(self.gemini_tools(t));
            }
        }

        let resp = CLIENT
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .map_err(Error::Http)?;
        let s = resp.status();
        let b: Value = resp.json().map_err(Error::Http)?;
        if !s.is_success() {
            let msg = b
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(Error::Other(format!("Vertex Gemini {s}: {msg}")));
        }
        self.from_gemini(&b)
    }

    fn to_gemini(&self, msgs: &[Value]) -> (Vec<Value>, Option<String>) {
        let mut contents: Vec<Value> = Vec::new();
        let mut sys: Option<String> = None;
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            if role == "system" {
                let text = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                sys = Some(if let Some(ref s) = sys {
                    format!("{s}\n{text}")
                } else {
                    text.into()
                });
                continue;
            }
            if role == "tool" {
                continue;
            } // simplify: skip tool responses for now
            let role = if role == "assistant" { "model" } else { "user" };
            let parts = self.content_to_gemini_parts(m.get("content"));
            if !parts.is_empty() {
                contents.push(serde_json::json!({"role":role,"parts":parts}));
            }
        }
        (contents, sys)
    }

    fn content_to_gemini_parts(&self, content: Option<&Value>) -> Vec<Value> {
        match content {
            Some(Value::String(s)) if !s.is_empty() => vec![serde_json::json!({"text":s})],
            Some(Value::Array(arr)) => arr.iter().filter_map(|p| {
                match p.get("type").and_then(|v| v.as_str()).unwrap_or("text") {
                    "text" => Some(serde_json::json!({"text":p.get("text").and_then(|v| v.as_str()).unwrap_or("")})),
                    "image_url" => {
                        let url = p.get("image_url").and_then(|v| v.get("url")).and_then(|v| v.as_str()).unwrap_or("");
                        url.find(";base64,").map(|idx| serde_json::json!({"inlineData":{"mimeType":&url[5..idx],"data":&url[idx+8..]}}))
                    }
                    _ => None,
                }
            }).collect(),
            _ => vec![],
        }
    }

    fn gemini_tools(&self, tools: &[Value]) -> Vec<Value> {
        tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name")?.as_str()?;
                let desc = f.get("description").and_then(|v| v.as_str());
                let params = f.get("parameters");
                let mut d = serde_json::json!({"name":name});
                if let Some(desc) = desc {
                    d["description"] = serde_json::json!(desc);
                }
                if let Some(p) = params {
                    d["parameters"] = p.clone();
                }
                Some(d)
            })
            .collect()
    }

    #[allow(clippy::wrong_self_convention)]
    fn from_gemini(&self, body: &Value) -> Result<AssistantTurn, Error> {
        let cands = body
            .get("candidates")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Other("no candidates".into()))?;
        let cand = cands
            .first()
            .ok_or_else(|| Error::Other("empty candidates".into()))?;
        let finish = cand
            .get("finishReason")
            .and_then(|v| v.as_str())
            .unwrap_or("STOP");
        let fr = match finish {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            _ => "stop",
        };
        let parts = cand
            .get("content")
            .and_then(|v| v.get("parts"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut text = String::new();
        let mut tcs: Vec<ToolCall> = Vec::new();
        for p in &parts {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
            }
            if let Some(fc) = p.get("functionCall") {
                tcs.push(ToolCall {
                    id: fc.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    name: fc.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    arguments: fc.get("args").cloned().unwrap_or(Value::Null),
                });
            }
        }
        let um = body.get("usageMetadata");
        Ok(AssistantTurn {
            text: if text.is_empty() { None } else { Some(text) },
            tool_calls: tcs,
            finish_reason: Some(fr.into()),
            reasoning: None,
            usage: um.map(|u| TokenUsage {
                input: u
                    .get("promptTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                output: u
                    .get("candidatesTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                cache_read: u
                    .get("cachedContentTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                cache_write: 0,
            }),
        })
    }

    // -- Claude family ---------------------------------------------------------

    fn call_claude(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Result<AssistantTurn, Error> {
        let token = self.bearer()?;
        let loc = &self.location;
        let proj = self.project.as_deref().unwrap_or("");

        let url = format!("https://{loc}-aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}/publishers/anthropic/models/{model}:streamRawPredict");

        let (sys, msgs) = self.to_anthropic(messages);
        let mut body = serde_json::json!({
            "anthropic_version": "vertex-2025-03-26",
            "messages": msgs,
            "max_tokens": settings.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(16000),
        });
        if let Some(s) = sys {
            body["system"] = serde_json::json!(s);
        }
        if let Some(t) = settings.get("temperature").and_then(|v| v.as_f64()) {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(t) = tools.and_then(|t| if t.is_empty() { None } else { Some(t) }) {
            let at: Vec<Value> = t.iter().map(|t| {
                let f = t.get("function");
                serde_json::json!({
                    "name": f.and_then(|v| v.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
                    "description": f.and_then(|v| v.get("description")).and_then(|v| v.as_str()).unwrap_or(""),
                    "input_schema": f.and_then(|v| v.get("parameters")).cloned().unwrap_or(serde_json::json!({"type":"object","properties":{}})),
                })
            }).collect();
            body["tools"] = serde_json::json!(at);
        }

        let resp = CLIENT
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .map_err(Error::Http)?;
        let st = resp.status();
        let txt = resp.text().map_err(Error::Http)?;
        if !st.is_success() {
            return Err(Error::Other(format!("Vertex Claude {st}: {txt}")));
        }
        self.from_anthropic(&txt)
    }

    fn to_anthropic(&self, msgs: &[Value]) -> (Option<String>, Vec<Value>) {
        let mut sys: Option<String> = None;
        let mut out: Vec<Value> = Vec::new();
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            if role == "system" {
                let t = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                sys = Some(if let Some(ref s) = sys {
                    format!("{s}\n{t}")
                } else {
                    t.into()
                });
                continue;
            }
            if role == "tool" {
                continue;
            }
            let role = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            let content = m.get("content");
            let blocks: Vec<Value> = match content.and_then(|v| v.as_str()) {
                Some(t) if !t.is_empty() => vec![serde_json::json!({"type":"text","text":t})],
                _ => vec![],
            };
            if blocks.is_empty() {
                continue;
            }
            out.push(serde_json::json!({"role":role,"content":blocks}));
        }
        (sys, out)
    }

    #[allow(clippy::wrong_self_convention)]
    fn from_anthropic(&self, text: &str) -> Result<AssistantTurn, Error> {
        // Parse JSON or SSE response
        let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let content = body
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut txt = String::new();
        let mut tcs: Vec<ToolCall> = Vec::new();
        for b in &content {
            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                txt.push_str(t);
            }
            if let Some(tu) = b.get("tool_use").or(b.get("toolUse")) {
                tcs.push(ToolCall {
                    id: tu.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                    name: tu.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    arguments: tu.get("input").cloned().unwrap_or(Value::Null),
                });
            }
        }
        let stop = body
            .get("stop_reason")
            .or(body.get("stopReason"))
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");
        let fr = match stop {
            "end_turn" | "stop_sequence" => "stop",
            "tool_use" => "tool_calls",
            "max_tokens" => "length",
            _ => "stop",
        };
        Ok(AssistantTurn {
            text: if txt.is_empty() { None } else { Some(txt) },
            tool_calls: tcs,
            finish_reason: Some(fr.into()),
            reasoning: None,
            usage: body.get("usage").map(|u| TokenUsage {
                input: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                output: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                cache_read: u
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                cache_write: u
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
            }),
        })
    }

    // -- Openweight / MaaS -----------------------------------------------------

    fn call_openweight(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        settings: &Value,
    ) -> Result<AssistantTurn, Error> {
        let token = self.bearer()?;
        let loc = &self.location;
        let proj = self.project.as_deref().unwrap_or("");

        let url = format!("https://{loc}-aiplatform.googleapis.com/v1beta1/projects/{proj}/locations/{loc}/endpoints/openapi/chat/completions");
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "max_tokens": settings.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096),
            "temperature": settings.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7),
        });
        if let Some(t) = tools {
            if !t.is_empty() {
                body["tools"] = serde_json::json!(t);
            }
        }

        let resp = CLIENT
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .map_err(Error::Http)?;
        let s = resp.status();
        let b: Value = resp.json().map_err(Error::Http)?;
        if !s.is_success() {
            let msg = b
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(Error::Other(format!("Vertex MaaS {s}: {msg}")));
        }
        let choice = b
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| Error::Other("no choices".into()))?;
        let msg = choice.get("message").unwrap_or(&Value::Null);
        Ok(AssistantTurn {
            text: msg
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from),
            tool_calls: msg
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|tc| {
                            let f = tc.get("function");
                            ToolCall {
                                id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                                name: f
                                    .and_then(|v| v.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .into(),
                                arguments: f
                                    .and_then(|v| v.get("arguments"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| {
                                        serde_json::from_str(s).unwrap_or(Value::String(s.into()))
                                    })
                                    .unwrap_or(Value::Null),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            finish_reason: choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(String::from),
            reasoning: None,
            usage: b.get("usage").map(|u| TokenUsage {
                input: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                output: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                cache_read: 0,
                cache_write: 0,
            }),
        })
    }

    // -- Public API ------------------------------------------------------------

    pub fn invoke(
        &self,
        model: &str,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        settings: Value,
    ) -> Result<AssistantTurn, Error> {
        let (family, bare) = Self::split_model(model);
        match family.as_str() {
            "gemini" => self.call_gemini(&bare, &messages, tools.as_deref(), &settings),
            "claude" => self.call_claude(&bare, &messages, tools.as_deref(), &settings),
            "openweight" => self.call_openweight(&bare, &messages, tools.as_deref(), &settings),
            _ => Err(Error::Other(format!("unknown vertex family: {family}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

pub fn capabilities_for(model: &str) -> ModelCapabilities {
    let family = if let Some(i) = model.find('/') {
        &model[..i]
    } else if model.starts_with("gemini") {
        "gemini"
    } else if model.starts_with("claude") {
        "claude"
    } else {
        "openweight"
    };
    match family {
        "gemini" => ModelCapabilities {
            tools: true,
            streaming: true,
            vision: true,
            pdf: false,
            parallel_tool_calls: true,
        },
        "claude" => ModelCapabilities {
            tools: true,
            streaming: true,
            vision: true,
            pdf: true,
            parallel_tool_calls: true,
        },
        _ => ModelCapabilities {
            tools: true,
            streaming: true,
            vision: false,
            pdf: false,
            parallel_tool_calls: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Provider trait impl
// ---------------------------------------------------------------------------

impl crate::router::Provider for VertexClient {
    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<AssistantTurn, Error> {
        self.invoke(model, messages, tools, settings)
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        capabilities_for(model)
    }

    fn name(&self) -> &str {
        "vertex"
    }
}
