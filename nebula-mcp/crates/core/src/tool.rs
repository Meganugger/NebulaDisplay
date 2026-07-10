//! The [`Tool`] trait and registry.
//!
//! Every capability the server exposes implements [`Tool`]. A [`ToolRegistry`]
//! owns the set of tools and produces the MCP `tools/list` payload, honouring
//! the enable/disable configuration.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_protocol::mcp::{Tool as ToolDef, ToolAnnotations};
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::config::Config;
use crate::context::ToolContext;
use crate::error::Result;

/// A single MCP tool.
///
/// Implementations must be cheap to clone-by-`Arc` and fully thread-safe; the
/// server may invoke many tools concurrently.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Fully-qualified, unique tool name (e.g. `fs.read`).
    fn name(&self) -> &str;

    /// Category this tool belongs to (e.g. `filesystem`). Used for the
    /// per-category enable switch.
    fn category(&self) -> &str;

    /// One-line description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's argument object.
    fn input_schema(&self) -> Value;

    /// Optional behavioural annotations.
    fn annotations(&self) -> Option<ToolAnnotations> {
        None
    }

    /// Execute the tool.
    ///
    /// `args` is the raw arguments object from the client, already validated to
    /// be JSON but not yet against the tool's schema; implementations parse and
    /// validate it. Permission checks are the implementation's responsibility
    /// via [`ToolContext`].
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult>;

    /// Build the MCP tool definition for `tools/list`.
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_string(),
            description: Some(self.description().to_string()),
            input_schema: self.input_schema(),
            annotations: self.annotations(),
        }
    }
}

/// A thread-safe, immutable-after-build registry of tools.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool, panicking on duplicate names (a programming error).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if self.tools.insert(name.clone(), tool).is_some() {
            panic!("duplicate tool registration: {name}");
        }
    }

    /// Number of registered tools (regardless of enabled state).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Look up a tool by name, returning it only if enabled by `config`.
    #[must_use]
    pub fn get_enabled(&self, name: &str, config: &Config) -> Option<Arc<dyn Tool>> {
        let tool = self.tools.get(name)?;
        if config.is_tool_enabled(tool.category(), name) {
            Some(tool.clone())
        } else {
            None
        }
    }

    /// Look up a tool by name regardless of enabled state.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Whether a tool exists but is disabled by config.
    #[must_use]
    pub fn is_known_but_disabled(&self, name: &str, config: &Config) -> bool {
        match self.tools.get(name) {
            Some(t) => !config.is_tool_enabled(t.category(), name),
            None => false,
        }
    }

    /// Produce the `tools/list` definitions for all enabled tools.
    #[must_use]
    pub fn definitions(&self, config: &Config) -> Vec<ToolDef> {
        self.tools
            .values()
            .filter(|t| config.is_tool_enabled(t.category(), t.name()))
            .map(|t| t.definition())
            .collect()
    }

    /// All registered tool names (including disabled), sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Dummy {
        name: &'static str,
        category: &'static str,
    }

    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &str {
            self.name
        }
        fn category(&self) -> &str {
            self.category
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _ctx: &ToolContext, _args: Value) -> Result<CallToolResult> {
            Ok(CallToolResult::text("ok"))
        }
    }

    #[test]
    fn registers_and_filters_by_enabled() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy {
            name: "fs.read",
            category: "filesystem",
        }));
        reg.register(Arc::new(Dummy {
            name: "driver.install",
            category: "driver",
        }));

        let mut config = Config::default();
        config.categories.insert(
            "driver".into(),
            crate::config::CategoryConfig { enabled: false },
        );

        let defs = reg.definitions(&config);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "fs.read");
        assert!(reg.get_enabled("driver.install", &config).is_none());
        assert!(reg.is_known_but_disabled("driver.install", &config));
    }

    #[test]
    #[should_panic(expected = "duplicate tool registration")]
    fn duplicate_registration_panics() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy {
            name: "x",
            category: "c",
        }));
        reg.register(Arc::new(Dummy {
            name: "x",
            category: "c",
        }));
    }
}
