//! Result-formatting helpers shared across tools.

use nebula_mcp_protocol::{CallToolResult, Content};
use serde::Serialize;
use serde_json::Value;

use crate::common::exec::ExecResult;

/// Build a success result whose single text block is pretty-printed JSON.
pub fn json_result<T: Serialize>(value: &T) -> CallToolResult {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialise result: {e}\"}}"));
    CallToolResult {
        content: vec![Content::text(text)],
        is_error: Some(false),
    }
}

/// Build a success result from a raw JSON value.
pub fn json_value_result(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|e| format!("{{\"error\":\"failed to serialise result: {e}\"}}"));
    CallToolResult {
        content: vec![Content::text(text)],
        is_error: Some(false),
    }
}

/// Render an [`ExecResult`] as a structured JSON tool result, marking it as an
/// error result when the process exited non-zero.
pub fn exec_result(command: &str, r: &ExecResult) -> CallToolResult {
    let value = serde_json::json!({
        "command": command,
        "exitCode": r.code,
        "success": r.success(),
        "durationMs": r.duration.as_millis(),
        "stdout": r.stdout,
        "stderr": r.stderr,
        "stdoutTruncated": r.stdout_truncated,
        "stderrTruncated": r.stderr_truncated,
    });
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    CallToolResult {
        content: vec![Content::text(text)],
        is_error: Some(!r.success()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn exec_result_marks_failure() {
        let r = ExecResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "boom".into(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration: Duration::from_millis(5),
        };
        let res = exec_result("git status", &r);
        assert_eq!(res.is_error, Some(true));
    }
}
