//! Core data types shared across the engine.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single tool call requested by the model.
///
/// Serialized as OpenAI nested format: `{"id": ..., "type": "function",
/// "function": {"name": ..., "arguments": ...}}` — matching the Python
/// persistence format.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
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

// -- Custom serde for ToolCall: OpenAI nested format -----------------------

impl Serialize for ToolCall {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let args_str = match &self.arguments {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut s = serializer.serialize_struct("ToolCall", 3)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("type", "function")?;
        s.serialize_field(
            "function",
            &serde_json::json!({"name": self.name, "arguments": args_str}),
        )?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(deserializer)?;
        let id = v["id"].as_str().unwrap_or("").to_string();
        let func = &v["function"];
        let name = func["name"].as_str().unwrap_or("").to_string();
        let arguments = func.get("arguments").cloned().unwrap_or(Value::Null);
        Ok(ToolCall { id, name, arguments })
    }
}

/// One message in the conversation history.
///
/// Canonical OpenAI shape with display-only sidecars:
/// - `ts` — unix timestamp (not sent to provider)
/// - `source` — connector card metadata (not sent to provider)
/// - `reasoning` — display-only thinking text. On the wire it's emitted as
///   `reasoning_content` (DeepSeek/GLM/Kimi/MiniMax/Qwen … and any other
///   OpenAI-compat vendor that promises a thinking-mode reply) so the next
///   request can replay it — without it, those vendors 400 with
///   "The reasoning_content in the thinking mode must be passed back to the API".
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

    pub fn assistant(
        content: String,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
        usage: Option<Value>,
        ts: f64,
    ) -> Self {
        Self::Assistant {
            content,
            ts: Some(ts),
            reasoning,
            usage,
            tool_calls,
            extras: Default::default(),
        }
    }

    pub fn tool_result(tool_call_id: String, content: String, ts: f64) -> Self {
        Self::Tool {
            tool_call_id,
            content,
            ts: Some(ts),
        }
    }

    pub fn tool_error(tool_call_id: String, reason: &str, ts: f64) -> Self {
        let body = serde_json::json!({
            "error": "tool call not executed",
            "reason": reason
        });
        Self::Tool {
            tool_call_id,
            content: serde_json::to_string(&body).unwrap_or_default(),
            ts: Some(ts),
        }
    }

    pub fn notice(kind: &str, text: Option<String>, ts: f64) -> Self {
        Self::Notice {
            kind: kind.to_string(),
            text,
            ts: Some(ts),
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
                reasoning,
                tool_calls,
                extras,
                ..
            } => {
                let mut obj = serde_json::json!({ "role": "assistant", "content": content });
                if !tool_calls.is_empty() {
                    let tcs: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| serde_json::to_value(tc).unwrap_or(Value::Null))
                        .collect();
                    obj["tool_calls"] = serde_json::json!(tcs);
                }
                // Replay the model's thinking text as `reasoning_content` on the wire.
                // DeepSeek/GLM/Kimi/MiniMax/Qwen and most other OpenAI-compat thinking
                // models require this exact field name on every assistant turn that
                // produced one — empty strings are dropped so we don't pollute a
                // non-thinking replay path.
                if let Some(r) = reasoning {
                    if !r.is_empty() {
                        obj["reasoning_content"] = Value::String(r.clone());
                    }
                }
                // Fields that `to_wire` already sets explicitly. If they
                // reappear here it means `extras` captured them during a
                // serde round-trip (extras is `#[serde(flatten)]`), and we
                // must NOT overwrite the canonical values — DeepSeek rejects
                // a message that has two `tool_calls` keys with
                // "Duplicate value for 'tool_call_id' of in message[N]".
                const MANAGED: &[&str] = &[
                    "role",
                    "content",
                    "tool_calls",
                    "reasoning_content",
                    "ts",
                    "usage",
                ];
                for (k, v) in extras {
                    if MANAGED.contains(&k.as_str()) {
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_wire_emits_reasoning_content_when_present() {
        // DeepSeek/GLM/Kimi/MiniMax/Qwen and any other OpenAI-compat thinking model
        // requires `reasoning_content` on every assistant turn that produced one.
        // Without it, the next request 400s with "The reasoning_content in the
        // thinking mode must be passed back to the API."
        let msg = Message::assistant("ok".into(), vec![], Some("think".into()), None, 0.0);
        let wire = msg.to_wire();
        assert_eq!(
            wire,
            json!({
                "role": "assistant",
                "content": "ok",
                "reasoning_content": "think",
            })
        );
    }

    #[test]
    fn to_wire_omits_reasoning_content_when_absent() {
        // Non-thinking turns (or models that don't surface thinking) must NOT grow a
        // `reasoning_content` field — the wire stays exactly what OpenAI expects.
        let msg = Message::assistant("ok".into(), vec![], None, None, 0.0);
        let wire = msg.to_wire();
        assert_eq!(wire, json!({ "role": "assistant", "content": "ok" }));
        assert!(wire.get("reasoning_content").is_none());
    }

    #[test]
    fn to_wire_drops_empty_reasoning() {
        // An empty string is meaningless to the vendor and would just be dead weight on
        // the wire — drop it so every reasoning-carrying path keeps the contract tight.
        let msg = Message::assistant("ok".into(), vec![], Some(String::new()), None, 0.0);
        let wire = msg.to_wire();
        assert_eq!(wire, json!({ "role": "assistant", "content": "ok" }));
        assert!(wire.get("reasoning_content").is_none());
    }

    #[test]
    fn to_wire_keeps_tool_calls_and_extras_alongside_reasoning() {
        // Make sure the new field doesn't shadow or collide with the existing
        // tool_calls / extras path that Anthropic, Gemini, and OpenAI rely on.
        let mut extras = serde_json::Map::new();
        extras.insert("_gemini".into(), json!({"call_sigs": ["x"]}));
        let msg = Message::Assistant {
            content: "calling".into(),
            ts: None,
            reasoning: Some("think".into()),
            usage: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.py"}),
            }],
            extras,
        };
        let wire = msg.to_wire();
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["content"], "calling");
        assert_eq!(wire["reasoning_content"], "think");
        assert_eq!(wire["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(wire["_gemini"]["call_sigs"][0], "x");
    }

    #[test]
    fn to_wire_drops_duplicates_of_managed_fields_when_extras_contain_them() {
        // Regression: `Message::Assistant::extras` is `#[serde(flatten)]`, so a
        // round-trip through the session store (or any JSON blob) captures the
        // canonical fields (`tool_calls`, `reasoning_content`, `role`,
        // `content`, `ts`, `usage`) inside `extras` too. `to_wire()` then
        // re-emits BOTH the explicit field and the extras version, which makes
        // DeepSeek's API reject the request with:
        //   "Duplicate value for 'tool_call_id' of in message[N]".
        // The fix: skip managed field names when spreading `extras`.
        let mut extras = serde_json::Map::new();
        // Simulate what serde-flatten picks up from a stored assistant message.
        extras.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_dupe",
                "type": "function",
                "function": {"name": "stale", "arguments": "{}"}
            }]),
        );
        extras.insert("reasoning_content".into(), json!("duplicated thought"));
        extras.insert("content".into(), json!("stale content"));
        extras.insert("role".into(), json!("assistant"));
        // Keep a real extra so we still verify it spreads.
        extras.insert("_gemini".into(), json!({"call_sigs": ["x"]}));

        let msg = Message::Assistant {
            content: "fresh".into(),
            ts: None,
            reasoning: Some("think".into()),
            usage: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.py"}),
            }],
            extras,
        };
        let wire = msg.to_wire();

        // Canonical values win; the extras copies are dropped.
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["content"], "fresh");
        assert_eq!(wire["reasoning_content"], "think");
        let tcs = wire["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(tcs.len(), 1, "must not duplicate tool_calls entry");
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["function"]["name"], "read_file");
        // Real extras still spread through.
        assert_eq!(wire["_gemini"]["call_sigs"][0], "x");
    }
}
