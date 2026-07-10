//! Model Context Protocol domain types layered on top of JSON-RPC.
//!
//! Only the subset required to operate a fully featured *tools* server is
//! modelled: initialization handshake, capability advertisement, tool
//! discovery (`tools/list`) and tool invocation (`tools/call`). The shapes
//! follow the MCP 2024-11-05 revision, which is what current agent clients
//! (Claude Code, Cursor, Codex) speak.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol revision this server implements and advertises.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Parameters for the `initialize` request sent by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Protocol version requested by the client.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Capabilities advertised by the client.
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    /// Identifying information about the client implementation.
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

/// Capabilities a client may advertise. We accept and ignore unknown fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Whether the client supports roots list-change notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Value>,
    /// Whether the client supports sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
    /// Experimental, client-specific capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
}

/// Name/version pair describing an MCP implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    /// Implementation name.
    pub name: String,
    /// Implementation semantic version.
    pub version: String,
}

/// Result of a successful `initialize` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Protocol version the server will speak.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Capabilities the server offers.
    pub capabilities: ServerCapabilities,
    /// Identifying information about this server.
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
    /// Optional free-form instructions surfaced to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Capabilities advertised by the server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Tool capability. Presence signals that `tools/*` methods are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Resource capability. Presence signals that `resources/*` are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    /// Prompt capability. Presence signals that `prompts/*` are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    /// Logging capability (server can emit `notifications/message` and accept
    /// `logging/setLevel`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
}

/// Tool capability descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsCapability {
    /// Whether the server emits `tools/list_changed` notifications.
    #[serde(rename = "listChanged", skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Resource capability descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourcesCapability {
    /// Whether the server supports resource subscriptions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    /// Whether the server emits `resources/list_changed` notifications.
    #[serde(rename = "listChanged", skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Prompt capability descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptsCapability {
    /// Whether the server emits `prompts/list_changed` notifications.
    #[serde(rename = "listChanged", skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// A single tool definition returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Unique tool name (namespaced, e.g. `fs.read`).
    pub name: String,
    /// Human-readable description shown to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input object.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    /// Optional annotations (read-only hints, destructive hints, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Behavioural hints attached to a tool. These are advisory metadata for the
/// client UI and planners; the server enforces the real policy itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// Human friendly title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Tool does not mutate its environment.
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Tool may perform irreversible/destructive operations.
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeated calls with the same args have no additional effect.
    #[serde(rename = "idempotentHint", skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Tool interacts with entities outside the local machine.
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Result of `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    /// The available tools.
    pub tools: Vec<Tool>,
    /// Opaque pagination cursor (unused; all tools returned at once).
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Parameters for `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    /// Name of the tool to invoke.
    pub name: String,
    /// Arguments object matching the tool's input schema.
    #[serde(default)]
    pub arguments: Value,
    /// Optional request metadata. MCP places the `progressToken` here when the
    /// client wants progress notifications for this call.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl CallToolParams {
    /// Extract the client's progress token, if any.
    #[must_use]
    pub fn progress_token(&self) -> Option<ProgressToken> {
        let tok = self.meta.as_ref()?.get("progressToken")?;
        match tok {
            Value::Number(n) => n.as_i64().map(ProgressToken::Number),
            Value::String(s) => Some(ProgressToken::String(s.clone())),
            _ => None,
        }
    }
}

/// An opaque progress token supplied by the client and echoed in
/// `notifications/progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgressToken {
    /// Numeric token.
    Number(i64),
    /// String token.
    String(String),
}

impl ProgressToken {
    /// Build a `notifications/progress` JSON-RPC message carrying this token.
    #[must_use]
    pub fn notification(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> Value {
        let mut params = serde_json::Map::new();
        params.insert(
            "progressToken".to_string(),
            match self {
                ProgressToken::Number(n) => Value::from(*n),
                ProgressToken::String(s) => Value::from(s.clone()),
            },
        );
        params.insert("progress".to_string(), Value::from(progress));
        if let Some(t) = total {
            params.insert("total".to_string(), Value::from(t));
        }
        if let Some(m) = message {
            params.insert("message".to_string(), Value::from(m));
        }
        serde_json::json!({
            "jsonrpc": crate::jsonrpc::JSONRPC_VERSION,
            "method": "notifications/progress",
            "params": Value::Object(params),
        })
    }
}

/// Result of `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    /// Ordered content blocks produced by the tool.
    pub content: Vec<Content>,
    /// When `true`, the content describes an error rather than success.
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl CallToolResult {
    /// Convenience constructor for a single text content success result.
    #[must_use]
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(body)],
            is_error: Some(false),
        }
    }

    /// Convenience constructor for a single text content error result.
    #[must_use]
    pub fn error_text(body: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(body)],
            is_error: Some(true),
        }
    }
}

/// A content block within a tool result. MCP supports text, image, audio and
/// embedded resource blocks; text and resource cover everything this server
/// produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    /// UTF-8 text content.
    Text {
        /// The text payload.
        text: String,
    },
    /// Base64 encoded image content.
    Image {
        /// Base64 image bytes.
        data: String,
        /// MIME type (e.g. `image/png`).
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Structured resource content.
    Resource {
        /// The embedded resource.
        resource: ResourceContents,
    },
}

impl Content {
    /// Build a text content block.
    #[must_use]
    pub fn text(body: impl Into<String>) -> Self {
        Content::Text { text: body.into() }
    }
}

/// Contents of an embedded resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContents {
    /// Resource URI.
    pub uri: String,
    /// MIME type of the resource.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text body, when the resource is textual.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 body, when the resource is binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A resource descriptor returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    /// Resource URI (e.g. `file:///abs/path`).
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional MIME type.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Result of `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResourcesResult {
    /// Available resources.
    pub resources: Vec<Resource>,
    /// Opaque pagination cursor.
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Parameters for `resources/read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    /// URI to read.
    pub uri: String,
}

/// Result of `resources/read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    /// The resource contents (usually a single entry).
    pub contents: Vec<ResourceContents>,
}

/// A prompt descriptor returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    /// Unique prompt name.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

/// A declared prompt argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the argument is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Result of `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPromptsResult {
    /// Available prompts.
    pub prompts: Vec<Prompt>,
}

/// Parameters for `prompts/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    /// Prompt name.
    pub name: String,
    /// Argument values.
    #[serde(default)]
    pub arguments: Value,
}

/// A single message in a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    /// Message role (`user` or `assistant`).
    pub role: String,
    /// Message content.
    pub content: Content,
}

/// Result of `prompts/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptResult {
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The prompt messages.
    pub messages: Vec<PromptMessage>,
}

/// Parameters for `logging/setLevel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetLevelParams {
    /// Requested level (`debug`, `info`, `notice`, `warning`, `error`, ...).
    pub level: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_result_uses_camel_case() {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                logging: None,
                ..Default::default()
            },
            server_info: Implementation {
                name: "nebula".into(),
                version: "0.1.0".into(),
            },
            instructions: None,
        };
        let text = serde_json::to_string(&result).unwrap();
        assert!(text.contains("protocolVersion"));
        assert!(text.contains("serverInfo"));
        assert!(text.contains("listChanged"));
    }

    #[test]
    fn call_tool_result_text_is_not_error() {
        let r = CallToolResult::text("hello");
        assert_eq!(r.is_error, Some(false));
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains("\"type\":\"text\""));
    }

    #[test]
    fn call_tool_params_defaults_arguments() {
        let p: CallToolParams = serde_json::from_str(r#"{"name":"fs.read"}"#).unwrap();
        assert_eq!(p.name, "fs.read");
        assert!(p.arguments.is_null());
    }
}
