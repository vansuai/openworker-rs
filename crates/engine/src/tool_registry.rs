//! Tool registry — schema storage and execution dispatch.

use serde_json::Map;
use std::collections::HashMap;

pub use crate::tool_types::{
    Error as ToolError, ToolArg, ToolFn, ToolResult, ToolSchema, ToolSpec,
};

/// The tool registry — holds schemas and execution functions.
pub struct ToolRegistry {
    tools: HashMap<String, (ToolFn, ToolSpec, ToolSchema)>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool with its execution function, spec, and optional schema.
    pub fn register(&mut self, name: &str, f: ToolFn, spec: ToolSpec, schema: Option<ToolSchema>) {
        let schema = schema.unwrap_or_else(|| ToolSchema::new(name, None, None));
        self.tools.insert(name.to_string(), (f, spec, schema));
    }

    /// Register multiple tools from an iterator.
    pub fn register_all(
        &mut self,
        tools: impl IntoIterator<Item = (String, ToolFn, ToolSpec, Option<ToolSchema>)>,
    ) {
        for (name, f, spec, schema) in tools {
            self.register(&name, f, spec, schema);
        }
    }

    /// Get the schema for all registered tools.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|(_, _, s)| s.clone()).collect()
    }

    /// Get the schema for a specific tool.
    pub fn get_schema(&self, name: &str) -> Option<&ToolSchema> {
        self.tools.get(name).map(|(_, _, s)| s)
    }

    /// Get the spec (risk level, etc.) for a specific tool.
    pub fn get_spec(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name).map(|(_, s, _)| s)
    }

    /// Execute a registered tool by name.
    pub fn execute(
        &self,
        name: &str,
        arguments: Map<String, serde_json::Value>,
    ) -> Result<ToolResult, ToolError> {
        let (f, _, _) = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        Ok(f(arguments))
    }

    /// True if a tool is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}
