//! TOML-backed configuration with hot reload.
//!
//! The configuration is split into a small number of sections:
//!
//! * `[server]`   – identity and MCP handshake metadata.
//! * `[logging]`  – tracing/telemetry sinks and verbosity.
//! * `[security]` – the global permission baseline every tool inherits.
//! * `[tools.<name>]` – optional per-tool overrides layered on the baseline.
//!
//! A [`ConfigStore`] owns the parsed config behind an atomic swap so hot
//! reloads are lock-free for readers. See [`crate::hotreload`] for the file
//! watcher that drives reloads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;

/// Root configuration document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Server identity and handshake metadata.
    pub server: ServerConfig,
    /// Logging and telemetry configuration.
    pub logging: LoggingConfig,
    /// Global permission baseline.
    pub security: SecurityConfig,
    /// Per-tool overrides keyed by fully-qualified tool name.
    pub tools: BTreeMap<String, ToolOverride>,
    /// Per-category enable switches keyed by category name (e.g. `driver`).
    pub categories: BTreeMap<String, CategoryConfig>,
}

/// Server identity section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Advertised server name.
    pub name: String,
    /// Advertised server version.
    pub version: String,
    /// Optional instructions surfaced to the model during initialize.
    pub instructions: Option<String>,
    /// Maximum number of tool calls executed concurrently.
    pub max_concurrent_calls: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "nebula-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            instructions: Some(
                "NebulaDisplay Windows autonomous-engineering MCP server. \
                 Filesystem, git, process, network tools work on all platforms; \
                 Windows-specific tools require a Windows host."
                    .to_string(),
            ),
            max_concurrent_calls: 16,
        }
    }
}

/// Logging / telemetry section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Verbosity filter (`error`, `warn`, `info`, `debug`, `trace`, or an
    /// `env_logger`-style directive such as `nebula=debug,info`).
    pub level: String,
    /// Output format for the file sink.
    pub format: LogFormat,
    /// Directory to write rotating log files into. `None` disables file logs.
    pub directory: Option<PathBuf>,
    /// Log file name prefix.
    pub file_prefix: String,
    /// Rotation cadence for file logs.
    pub rotation: LogRotation,
    /// When set, an OTLP endpoint URL to export traces to. Presence toggles
    /// OpenTelemetry export in the server binary.
    pub otel_endpoint: Option<String>,
    /// Emit per-tool metrics summaries to the logs.
    pub emit_metrics: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Json,
            directory: None,
            file_prefix: "nebula-mcp".to_string(),
            rotation: LogRotation::Daily,
            otel_endpoint: None,
            emit_metrics: true,
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Structured JSON, one object per line.
    Json,
    /// Human-readable, coloured when a TTY.
    Pretty,
}

/// Rotation cadence for the rolling file appender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    /// Rotate every minute (useful for stress tests).
    Minutely,
    /// Rotate hourly.
    Hourly,
    /// Rotate daily.
    Daily,
    /// Never rotate (single growing file).
    Never,
}

/// Global permission baseline. Every tool inherits these unless overridden.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Glob patterns of paths tools may access. Empty means "deny all file
    /// access" — a safe default that forces explicit configuration.
    pub allowed_paths: Vec<String>,
    /// Glob patterns explicitly denied even if matched by `allowed_paths`.
    pub denied_paths: Vec<String>,
    /// Executable names permitted for process/terminal tools (basename match,
    /// case-insensitive on Windows). Empty means "deny all execution".
    pub allowed_commands: Vec<String>,
    /// Default per-call timeout in seconds.
    pub default_timeout_secs: u64,
    /// Absolute maximum runtime a single call may request, in seconds.
    pub max_runtime_secs: u64,
    /// Maximum captured output per stream, in bytes.
    pub max_output_bytes: usize,
    /// Whether tools may request elevated (admin/root) execution.
    pub allow_elevated: bool,
    /// Whether tools that reach external networks are permitted.
    pub allow_network: bool,
    /// Whether destructive operations (delete, reset --hard, driver uninstall)
    /// are permitted at all.
    pub allow_destructive: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allowed_paths: Vec::new(),
            denied_paths: default_denied_paths(),
            allowed_commands: Vec::new(),
            default_timeout_secs: 120,
            max_runtime_secs: 3600,
            max_output_bytes: 8 * 1024 * 1024,
            allow_elevated: false,
            allow_network: true,
            allow_destructive: false,
        }
    }
}

/// Sensitive paths denied by default on every platform.
fn default_denied_paths() -> Vec<String> {
    vec![
        "**/.ssh/**".to_string(),
        "**/.aws/**".to_string(),
        "**/.gnupg/**".to_string(),
        "**/*.pem".to_string(),
        "**/*.key".to_string(),
        "**/id_rsa*".to_string(),
        "**/.env".to_string(),
    ]
}

/// Optional per-tool override. All fields are optional; unset fields fall back
/// to the [`SecurityConfig`] baseline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOverride {
    /// Explicitly enable/disable this tool.
    pub enabled: Option<bool>,
    /// Override timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Override maximum output bytes.
    pub max_output_bytes: Option<usize>,
    /// Additional allowed paths (merged with the baseline).
    pub allowed_paths: Option<Vec<String>>,
    /// Additional allowed commands (merged with the baseline).
    pub allowed_commands: Option<Vec<String>>,
    /// Override the destructive flag for this tool.
    pub allow_destructive: Option<bool>,
}

/// Per-category enable switch. A disabled category disables all of its tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CategoryConfig {
    /// Whether the category is enabled.
    pub enabled: bool,
}

impl Default for CategoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Config {
    /// Parse a config document from a TOML string.
    pub fn from_toml_str(text: &str) -> Result<Self, ToolError> {
        toml::from_str(text).map_err(|e| ToolError::Internal(format!("invalid config: {e}")))
    }

    /// Load a config document from disk.
    pub fn load(path: &Path) -> Result<Self, ToolError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ToolError::Io(format!("reading config {}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// Serialise this config back to a TOML string.
    pub fn to_toml_string(&self) -> Result<String, ToolError> {
        toml::to_string_pretty(self)
            .map_err(|e| ToolError::Internal(format!("serialising config: {e}")))
    }

    /// Return `true` if the given tool (in the given category) is enabled.
    ///
    /// A tool is enabled unless its category is disabled or its own
    /// `enabled = false` override is set.
    #[must_use]
    pub fn is_tool_enabled(&self, category: &str, tool: &str) -> bool {
        if let Some(cat) = self.categories.get(category) {
            if !cat.enabled {
                return false;
            }
        }
        match self.tools.get(tool) {
            Some(o) => o.enabled.unwrap_or(true),
            None => true,
        }
    }
}

/// A concurrency-friendly holder for the active [`Config`].
///
/// Readers take a cheap `Arc` snapshot; writers (hot reload) swap the pointer
/// atomically. This keeps the hot path lock-free for tool execution.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<RwLock<Arc<Config>>>,
    source_path: Option<PathBuf>,
}

impl ConfigStore {
    /// Create a store from an in-memory config.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
            source_path: None,
        }
    }

    /// Create a store backed by a file, remembering the path for reloads.
    pub fn from_path(path: &Path) -> Result<Self, ToolError> {
        let config = Config::load(path)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
            source_path: Some(path.to_path_buf()),
        })
    }

    /// Take a cheap snapshot of the current configuration.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Config> {
        self.inner.read().clone()
    }

    /// The file this store was loaded from, if any.
    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// Replace the active configuration.
    pub fn replace(&self, config: Config) {
        *self.inner.write() = Arc::new(config);
    }

    /// Re-read the source file and swap in the new config.
    ///
    /// Returns an error (without changing the active config) if the file is
    /// missing or invalid, so a bad edit never takes down a running server.
    pub fn reload_from_disk(&self) -> Result<(), ToolError> {
        let Some(path) = &self.source_path else {
            return Err(ToolError::Internal(
                "config store has no backing file to reload".to_string(),
            ));
        };
        let config = Config::load(path)?;
        self.replace(config);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_deny_file_and_command_access() {
        let c = Config::default();
        assert!(c.security.allowed_paths.is_empty());
        assert!(c.security.allowed_commands.is_empty());
        assert!(!c.security.allow_elevated);
        assert!(!c.security.allow_destructive);
    }

    #[test]
    fn parses_minimal_config() {
        let text = r#"
            [server]
            name = "custom"

            [security]
            allowed_paths = ["/work/**"]
            allowed_commands = ["git", "cargo"]
            default_timeout_secs = 30

            [tools."fs.delete"]
            enabled = false

            [categories.driver]
            enabled = false
        "#;
        let c = Config::from_toml_str(text).unwrap();
        assert_eq!(c.server.name, "custom");
        assert_eq!(c.security.default_timeout_secs, 30);
        assert!(!c.is_tool_enabled("filesystem", "fs.delete"));
        assert!(!c.is_tool_enabled("driver", "driver.install"));
        assert!(c.is_tool_enabled("filesystem", "fs.read"));
    }

    #[test]
    fn roundtrips_through_toml() {
        let c = Config::default();
        let text = c.to_toml_string().unwrap();
        let back = Config::from_toml_str(&text).unwrap();
        assert_eq!(back.server.name, c.server.name);
    }

    #[test]
    fn store_replace_is_visible_to_new_snapshots() {
        let store = ConfigStore::new(Config::default());
        assert_eq!(store.snapshot().server.name, "nebula-mcp");
        let mut c = Config::default();
        c.server.name = "renamed".into();
        store.replace(c);
        assert_eq!(store.snapshot().server.name, "renamed");
    }
}
