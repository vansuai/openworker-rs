//! Core data types shared across the engine.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl ToolCall {
    pub fn arguments_dict(&self) -> Option<&serde_json::Map<String, Value>> {
        self.arguments.as_object()
    }

    pub fn arguments_str(&self) -> String {
        self.arguments.to_string()
    }
}

/// One message in the conversation history.
///
/// Canonical OpenAI shape with display-only sidecars:
/// - `ts` — unix timestamp (not sent to provider)
/// - `source` — connector card metadata (not sent to provider)
/// - `reasoning` — display-only thinking text (not sent to provider)
/// - `usage` — token counts (not sent to provider)
/// - `role: "notice"` — error/interrupted/model_switch markers (dropped from provider feed)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User {
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Value>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(flatten)]
        extras: serde_json::Map<String, Value>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<f64>,
    },
    /// Display-only marker (never sent to a provider).
    #[serde(rename = "notice")]
    Notice {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<f64>,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<Value>) -> Self {
        Self::User {
            content: content.into(),
            ts: None,
            source: None,
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            content,
            ts: None,
            reasoning: None,
            usage: None,
            tool_calls,
            extras: Default::default(),
        }
    }

    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self::Tool {
            tool_call_id,
            content,
            ts: None,
        }
    }

    pub fn tool_error(tool_call_id: String, reason: &str) -> Self {
        let body = serde_json::json!({
            "error": "tool call not executed",
            "reason": reason
        });
        Self::Tool {
            tool_call_id,
            content: serde_json::to_string(&body).unwrap_or_default(),
            ts: None,
        }
    }

    pub fn notice(kind: &str, text: Option<String>) -> Self {
        Self::Notice {
            kind: kind.to_string(),
            text,
            ts: None,
        }
    }

    /// True for messages that should never reach the provider.
    pub fn is_display_only(&self) -> bool {
        matches!(self, Self::Notice { .. })
    }

    /// Convert to the wire format (OpenAI shape) for sending to providers.
    pub fn to_wire(&self) -> Value {
        match self {
            Self::System { content } => serde_json::json!({ "role": "system", "content": content }),
            Self::User { content, .. } => serde_json::json!({ "role": "user", "content": content }),
            Self::Assistant {
                content,
                tool_calls,
                extras,
                ..
            } => {
                let mut obj = serde_json::json!({ "role": "assistant", "content": content });
                if !tool_calls.is_empty() {
                    let tcs: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string(),
                                }
                            })
                        })
                        .collect();
                    obj["tool_calls"] = serde_json::json!(tcs);
                }
                for (k, v) in extras {
                    obj[k] = v.clone();
                }
                obj
            }
            Self::Tool {
                tool_call_id,
                content,
                ..
            } => {
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                })
            }
            Self::Notice { .. } => serde_json::json!({ "role": "notice" }),
        }
    }

    /// Extract the `ts` timestamp.
    pub fn ts(&self) -> Option<f64> {
        match self {
            Self::User { ts, .. }
            | Self::Assistant { ts, .. }
            | Self::Tool { ts, .. }
            | Self::Notice { ts, .. } => *ts,
            Self::System { .. } => None,
        }
    }
}
