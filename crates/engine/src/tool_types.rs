//! Shared tool types used by both the engine and external crates (e.g. skills).
//! Kept in a separate module to avoid cyclic dependencies.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
}

// ---------------------------------------------------------------------------
// ToolResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub value: Value,
    pub display: Option<Map<String, Value>>,
}

impl ToolResult {
    pub fn ok(value: impl Into<Value>) -> Self {
        Self {
            value: value.into(),
            display: None,
        }
    }

    pub fn with_display(mut self, display: Map<String, Value>) -> Self {
        self.display = Some(display);
        self
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

impl From<Value> for ToolResult {
    fn from(value: Value) -> Self {
        Self::ok(value)
    }
}

impl From<String> for ToolResult {
    fn from(value: String) -> Self {
        Self::ok(value)
    }
}

impl From<&str> for ToolResult {
    fn from(value: &str) -> Self {
        Self::ok(value)
    }
}

// ---------------------------------------------------------------------------
// ToolFn
// ---------------------------------------------------------------------------

pub type ToolFn = Arc<dyn Fn(Map<String, Value>) -> ToolResult + Send + Sync>;

// ---------------------------------------------------------------------------
// ToolSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ToolSpec {
    pub risk_level: &'static str,
    pub category: &'static str,
    pub parallel_safe: bool,
}

// ---------------------------------------------------------------------------
// ToolSchema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionSchema {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

impl ToolSchema {
    pub fn new(name: &str, description: Option<&str>, parameters: Option<Value>) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolFunctionSchema {
                name: name.to_string(),
                description: description.map(String::from),
                parameters,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// ToolArg
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ToolArg {
    args: Map<String, Value>,
}

impl ToolArg {
    pub fn new(args: Map<String, Value>) -> Self {
        Self { args }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.args.get(key)?.as_str()
    }

    pub fn get_str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get_str(key).unwrap_or(default)
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.args
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.args.get(key)?.as_i64()
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.args.get(key)?.as_f64()
    }

    pub fn get_object(&self, key: &str) -> Option<&Map<String, Value>> {
        self.args.get(key)?.as_object()
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.args.clone())
    }
}
