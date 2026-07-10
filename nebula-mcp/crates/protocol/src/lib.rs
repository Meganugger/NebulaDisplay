//! # nebula-mcp-protocol
//!
//! Transport-agnostic types for the Model Context Protocol (MCP) plus a
//! newline-delimited JSON stdio transport.
//!
//! The crate is intentionally free of business logic: it only knows how to
//! parse, represent and serialise MCP/JSON-RPC messages. The server crate
//! layers dispatch, permissions and tool execution on top.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod jsonrpc;
pub mod mcp;
pub mod transport;

pub use jsonrpc::{error_codes, ErrorObject, Request, RequestId, Response, JSONRPC_VERSION};
pub use mcp::{
    CallToolParams, CallToolResult, ClientCapabilities, Content, Implementation, InitializeParams,
    InitializeResult, ListToolsResult, ProgressToken, ResourceContents, ServerCapabilities, Tool,
    ToolAnnotations, ToolsCapability, PROTOCOL_VERSION,
};
pub use transport::{FrameReader, FrameWriter, TransportError};
