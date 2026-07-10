//! Error taxonomy shared by the core runtime and every tool.
//!
//! Tools return [`ToolError`]. The server maps these onto JSON-RPC error codes
//! or, more commonly, onto MCP `CallToolResult` objects with `isError = true`
//! so the model can read and react to the failure text.

use nebula_mcp_protocol::error_codes;

/// The result type used throughout the runtime.
pub type Result<T> = std::result::Result<T, ToolError>;

/// A structured, categorised tool error.
///
/// Each variant carries a stable category so telemetry and clients can reason
/// about failures without string matching, plus a human-readable message.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller supplied invalid or missing arguments.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// A security policy denied the operation.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The requested path is outside every allowed root.
    #[error("path '{path}' is not within an allowed root")]
    PathNotAllowed {
        /// The offending path.
        path: String,
    },

    /// A command or executable is not on the allowlist.
    #[error("command '{0}' is not permitted by policy")]
    CommandNotAllowed(String),

    /// The operation exceeded its configured timeout.
    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The operation was cancelled by the client or a shutdown signal.
    #[error("operation cancelled")]
    Cancelled,

    /// Output exceeded the configured maximum size.
    #[error("output exceeded the maximum of {limit} bytes")]
    OutputTooLarge {
        /// Configured byte limit.
        limit: usize,
    },

    /// A required external tool (git, msbuild, PresentMon, ...) was not found.
    #[error("required external tool not found: {0}")]
    ToolNotFound(String),

    /// The feature requires a platform this build is not running on.
    #[error("operation is not supported on this platform: {0}")]
    PlatformUnsupported(String),

    /// An underlying I/O error.
    #[error("I/O error: {0}")]
    Io(String),

    /// The tool ran but reported a failure (e.g. non-zero exit, test failure).
    #[error("execution failed: {0}")]
    Execution(String),

    /// A dependency or subsystem returned an unexpected internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ToolError {
    /// Map the error onto the most appropriate JSON-RPC error code.
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self {
            ToolError::InvalidArguments(_) => error_codes::INVALID_PARAMS,
            ToolError::PermissionDenied(_)
            | ToolError::PathNotAllowed { .. }
            | ToolError::CommandNotAllowed(_) => error_codes::PERMISSION_DENIED,
            ToolError::Timeout(_) | ToolError::Cancelled => error_codes::REQUEST_CANCELLED,
            ToolError::OutputTooLarge { .. }
            | ToolError::ToolNotFound(_)
            | ToolError::PlatformUnsupported(_)
            | ToolError::Io(_)
            | ToolError::Execution(_) => error_codes::TOOL_EXECUTION_ERROR,
            ToolError::Internal(_) => error_codes::INTERNAL_ERROR,
        }
    }

    /// A short, stable category label for metrics/telemetry.
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            ToolError::InvalidArguments(_) => "invalid_arguments",
            ToolError::PermissionDenied(_) => "permission_denied",
            ToolError::PathNotAllowed { .. } => "path_not_allowed",
            ToolError::CommandNotAllowed(_) => "command_not_allowed",
            ToolError::Timeout(_) => "timeout",
            ToolError::Cancelled => "cancelled",
            ToolError::OutputTooLarge { .. } => "output_too_large",
            ToolError::ToolNotFound(_) => "tool_not_found",
            ToolError::PlatformUnsupported(_) => "platform_unsupported",
            ToolError::Io(_) => "io",
            ToolError::Execution(_) => "execution",
            ToolError::Internal(_) => "internal",
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::InvalidArguments(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_stable() {
        assert_eq!(
            ToolError::PathNotAllowed { path: "x".into() }.category(),
            "path_not_allowed"
        );
        assert_eq!(
            ToolError::Timeout(std::time::Duration::from_secs(1)).category(),
            "timeout"
        );
    }

    #[test]
    fn permission_errors_map_to_permission_code() {
        assert_eq!(
            ToolError::CommandNotAllowed("rm".into()).json_rpc_code(),
            error_codes::PERMISSION_DENIED
        );
    }
}
