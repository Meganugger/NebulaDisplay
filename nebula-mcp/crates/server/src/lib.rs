//! # nebula-mcp-server
//!
//! Library surface of the NebulaDisplay MCP server: the [`server::Server`]
//! dispatch engine. The `nebula-mcp` binary is a thin CLI wrapper around this.

#![warn(missing_docs)]

pub mod server;

pub use server::Server;
