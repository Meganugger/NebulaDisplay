//! Minimal helpers for building JSON Schema objects for tool inputs.
//!
//! These keep tool definitions terse and consistent without pulling in a full
//! schema-generation dependency. Only the subset of JSON Schema understood by
//! MCP clients is produced.

use serde_json::{json, Map, Value};

/// Builder for a JSON Schema `object` with typed properties.
#[derive(Default)]
pub struct ObjectSchema {
    properties: Map<String, Value>,
    required: Vec<String>,
    additional: bool,
}

impl ObjectSchema {
    /// Start a new object schema (additional properties disallowed by default).
    #[must_use]
    pub fn new() -> Self {
        Self {
            properties: Map::new(),
            required: Vec::new(),
            additional: false,
        }
    }

    /// Add a property with an explicit schema fragment.
    #[must_use]
    pub fn prop(mut self, name: &str, schema: Value, required: bool) -> Self {
        self.properties.insert(name.to_string(), schema);
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a string property.
    #[must_use]
    pub fn string(self, name: &str, desc: &str, required: bool) -> Self {
        self.prop(
            name,
            json!({"type": "string", "description": desc}),
            required,
        )
    }

    /// Add a boolean property with a default.
    #[must_use]
    pub fn boolean(self, name: &str, desc: &str, default: bool) -> Self {
        self.prop(
            name,
            json!({"type": "boolean", "description": desc, "default": default}),
            false,
        )
    }

    /// Add an integer property.
    #[must_use]
    pub fn integer(self, name: &str, desc: &str, required: bool) -> Self {
        self.prop(
            name,
            json!({"type": "integer", "description": desc}),
            required,
        )
    }

    /// Add a string-array property.
    #[must_use]
    pub fn string_array(self, name: &str, desc: &str, required: bool) -> Self {
        self.prop(
            name,
            json!({"type": "array", "items": {"type": "string"}, "description": desc}),
            required,
        )
    }

    /// Add an enum (string) property.
    #[must_use]
    pub fn enumerated(self, name: &str, desc: &str, values: &[&str], required: bool) -> Self {
        self.prop(
            name,
            json!({"type": "string", "description": desc, "enum": values}),
            required,
        )
    }

    /// Allow additional properties beyond those declared.
    #[must_use]
    pub fn allow_additional(mut self) -> Self {
        self.additional = true;
        self
    }

    /// Finalise into a JSON Schema value.
    #[must_use]
    pub fn build(self) -> Value {
        json!({
            "type": "object",
            "properties": Value::Object(self.properties),
            "required": self.required,
            "additionalProperties": self.additional,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_schema() {
        let s = ObjectSchema::new()
            .string("path", "the path", true)
            .boolean("recursive", "recurse", false)
            .build();
        assert_eq!(s["type"], "object");
        assert_eq!(s["required"][0], "path");
        assert_eq!(s["properties"]["recursive"]["default"], false);
    }
}
