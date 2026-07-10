//! # nebula-mcp-core
//!
//! The runtime foundation shared by every tool and the server binary:
//!
//! * [`config`] – TOML configuration with an atomically-swappable store.
//! * [`hotreload`] – debounced config file watcher.
//! * [`security`] – the permission policy engine (paths, commands, limits).
//! * [`telemetry`] – tracing/JSON logging and optional OpenTelemetry export.
//! * [`metrics`] – lock-free per-tool metrics.
//! * [`tool`] – the [`tool::Tool`] trait and [`tool::ToolRegistry`].
//! * [`context`] – the per-call [`context::ToolContext`].
//! * [`error`] – the shared [`error::ToolError`] taxonomy.
//!
//! The crate contains no I/O beyond config/telemetry setup; concrete tools live
//! in `nebula-mcp-tools` and dispatch lives in `nebula-mcp-server`.

#![warn(missing_docs)]

pub mod config;
pub mod context;
pub mod error;
pub mod hotreload;
pub mod metrics;
pub mod security;
pub mod telemetry;
pub mod tool;

pub use context::ToolContext;
pub use context::{ProgressSink, ProgressUpdate};
pub use error::{Result, ToolError};
pub use metrics::{Metrics, Outcome};
pub use telemetry::LogControl;
pub use tool::{Tool, ToolRegistry};
