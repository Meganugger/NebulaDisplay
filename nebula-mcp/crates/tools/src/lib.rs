//! # nebula-mcp-tools
//!
//! Concrete MCP tool implementations for the NebulaDisplay MCP server, grouped
//! by category. Cross-platform categories (filesystem, terminal, process, git,
//! github, network) work everywhere; Windows categories (powershell, windows,
//! driver, display, benchmark, diagnostics) compile everywhere but return
//! [`nebula_mcp_core::ToolError::PlatformUnsupported`] off Windows.
//!
//! All tools are constructed once and share a small set of stateful services
//! via [`ToolServices`].

#![warn(missing_docs)]

pub mod benchmark;
pub mod browser;
pub mod common;
pub mod diagnostics;
pub mod display;
pub mod docker;
pub mod driver;
pub mod filesystem;
pub mod git;
pub mod github;
pub mod network;
pub mod powershell;
pub mod process;
pub mod terminal;
pub mod windows;

use std::sync::Arc;

use nebula_mcp_core::{Tool, ToolRegistry};

pub use common::session::SessionManager;

/// Stateful services shared across tools for the process lifetime.
#[derive(Clone)]
pub struct ToolServices {
    /// Interactive session manager (persistent shells/REPLs).
    pub sessions: Arc<SessionManager>,
    /// Shared HTTP client with connection reuse for network/github tools.
    pub http: reqwest::Client,
}

impl ToolServices {
    /// Build the default services set.
    #[must_use]
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("nebula-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            sessions: Arc::new(SessionManager::new()),
            http,
        }
    }
}

impl Default for ToolServices {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a registry containing every tool this crate provides.
#[must_use]
pub fn build_registry(services: &ToolServices) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for t in all_tools(services) {
        reg.register(t);
    }
    reg
}

/// Collect every tool instance.
#[must_use]
pub fn all_tools(services: &ToolServices) -> Vec<Arc<dyn Tool>> {
    let mut v: Vec<Arc<dyn Tool>> = Vec::new();
    v.extend(filesystem::tools());
    v.extend(terminal::tools(services));
    v.extend(process::tools());
    v.extend(git::tools());
    v.extend(github::tools(services));
    v.extend(network::tools(services));
    v.extend(powershell::tools());
    v.extend(windows::tools());
    v.extend(driver::tools());
    v.extend(display::tools());
    v.extend(benchmark::tools());
    v.extend(diagnostics::tools());
    v.extend(browser::tools());
    v.extend(docker::tools());
    v
}
