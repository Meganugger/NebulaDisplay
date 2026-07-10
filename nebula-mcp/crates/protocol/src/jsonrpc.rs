//! JSON-RPC 2.0 message types as used by the Model Context Protocol.
//!
//! MCP frames every message as a JSON-RPC 2.0 object. This module models
//! requests, responses, notifications and the standard error object with
//! enough fidelity to round-trip any well-formed MCP message.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only supported JSON-RPC protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request or notification identifier.
///
/// The specification allows string, number, or null identifiers. Notifications
/// omit the identifier entirely (represented by `Option::None` at the message
/// level).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric identifier.
    Number(i64),
    /// String identifier.
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s}"),
        }
    }
}

/// An incoming JSON-RPC message. MCP peers may batch, but the reference
/// implementation transmits one object per line, which is what we model here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Protocol marker, always `"2.0"`.
    pub jsonrpc: String,
    /// Absent for notifications, present for requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// The RPC method name (e.g. `tools/call`).
    pub method: String,
    /// Method parameters. MCP methods use a structured object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Returns `true` when the message is a notification (no `id`).
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A successful or failed JSON-RPC response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Protocol marker, always `"2.0"`.
    pub jsonrpc: String,
    /// Echoes the request identifier (null for parse errors without an id).
    pub id: Option<RequestId>,
    /// Present on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// Build a success response for the given id and payload.
    #[must_use]
    pub fn success(id: Option<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response for the given id.
    #[must_use]
    pub fn error(id: Option<RequestId>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Numeric error code (see [`error_codes`]).
    pub code: i64,
    /// Short human-readable description.
    pub message: String,
    /// Optional structured error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Construct an error object with an optional data payload.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

/// Standard and MCP-specific JSON-RPC error codes.
pub mod error_codes {
    /// Invalid JSON was received by the server.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Server rejected the call for policy/permission reasons (application code).
    pub const PERMISSION_DENIED: i64 = -32000;
    /// The tool execution failed (application code).
    pub const TOOL_EXECUTION_ERROR: i64 = -32001;
    /// The request was cancelled or timed out (application code).
    pub const REQUEST_CANCELLED: i64 = -32002;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_with_numeric_id() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(req.id, Some(RequestId::Number(7)));
        assert_eq!(req.method, "tools/list");
        assert!(!req.is_notification());
    }

    #[test]
    fn parses_notification_without_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn serializes_success_response_without_error_field() {
        let resp = Response::success(Some(RequestId::Number(1)), serde_json::json!({"ok": true}));
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("\"result\""));
        assert!(!text.contains("\"error\""));
    }

    #[test]
    fn string_and_number_ids_roundtrip() {
        let s = RequestId::String("abc".into());
        let n = RequestId::Number(42);
        assert_eq!(s.to_string(), "abc");
        assert_eq!(n.to_string(), "42");
    }
}
