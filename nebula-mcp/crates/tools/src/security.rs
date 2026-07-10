//! Security introspection tools: dry-run the permission engine so an agent can
//! check whether a path or command would be allowed — and inspect a tool's
//! effective policy — *before* attempting an operation. Cross-platform and
//! read-only.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::security::EffectivePolicy;
use nebula_mcp_core::{Result, Tool, ToolContext};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::output::json_value_result;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "security";

/// Build security tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CheckPath),
        Arc::new(CheckCommand),
        Arc::new(PolicyInspect),
    ]
}

fn ro() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    })
}

/// Resolve the policy to evaluate against: the named tool's, or the baseline.
fn policy_for(ctx: &ToolContext, tool: Option<&str>) -> Result<EffectivePolicy> {
    let name = tool.unwrap_or("security.baseline");
    EffectivePolicy::resolve(&ctx.config, name)
}

/// Would a path be permitted?
struct CheckPath;

#[async_trait]
impl Tool for CheckPath {
    fn name(&self) -> &str {
        "security.check_path"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Dry-run the path policy: report whether a path would be allowed (and its normalised form) \
         or why it is denied. Optionally evaluate against a specific tool's policy."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "path",
                "Path to evaluate (relative to the workspace root or absolute).",
                true,
            )
            .string(
                "tool",
                "Optional tool name whose policy to evaluate against.",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = a.str("path")?;
        let policy = policy_for(ctx, a.opt_str("tool")?)?;
        let result = match policy.check_path(Path::new(path), &ctx.working_dir) {
            Ok(normalized) => json!({
                "path": path,
                "allowed": true,
                "normalizedPath": normalized.display().to_string(),
            }),
            Err(e) => json!({
                "path": path,
                "allowed": false,
                "reason": e.to_string(),
                "category": e.category(),
            }),
        };
        Ok(json_value_result(result))
    }
}

/// Would a command be permitted?
struct CheckCommand;

#[async_trait]
impl Tool for CheckCommand {
    fn name(&self) -> &str {
        "security.check_command"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Dry-run the command allowlist: report whether a program would be permitted for execution. \
         Optionally evaluate against a specific tool's policy."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("program", "Executable name or path to evaluate.", true)
            .string(
                "tool",
                "Optional tool name whose policy to evaluate against.",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let program = a.str("program")?;
        let policy = policy_for(ctx, a.opt_str("tool")?)?;
        let result = match policy.check_command(program) {
            Ok(()) => json!({ "program": program, "allowed": true }),
            Err(e) => json!({
                "program": program,
                "allowed": false,
                "reason": e.to_string(),
                "category": e.category(),
            }),
        };
        Ok(json_value_result(result))
    }
}

/// Inspect the effective policy for a tool.
struct PolicyInspect;

#[async_trait]
impl Tool for PolicyInspect {
    fn name(&self) -> &str {
        "security.effective_policy"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Report the effective permission policy for a tool: timeouts, output cap, gates and the command allowlist."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "tool",
                "Tool name to resolve the policy for (default: baseline).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let tool = a.opt_str("tool")?;
        let policy = policy_for(ctx, tool)?;
        Ok(json_value_result(json!({
            "tool": tool.unwrap_or("security.baseline"),
            "timeoutSecs": policy.timeout().as_secs(),
            "maxRuntimeSecs": policy.max_runtime().as_secs(),
            "maxOutputBytes": policy.max_output_bytes(),
            "allowElevated": policy.allow_elevated(),
            "allowNetwork": policy.allow_network(),
            "allowDestructive": policy.allow_destructive(),
            "allowedPathPatternCount": policy.allowed_pattern_count(),
            "allowedCommands": policy.allowed_commands(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::{Config, SecurityConfig};
    use nebula_mcp_core::security::EffectivePolicy as Pol;
    use nebula_mcp_core::Metrics;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        let config = Config {
            security: SecurityConfig {
                allowed_paths: vec!["/work/**".into()],
                allowed_commands: vec!["git".into(), "cargo".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = Pol::build("security", &config.security, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: std::path::PathBuf::from("/work"),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(config),
            request_id: "r".into(),
            progress: None,
        }
    }

    #[tokio::test]
    async fn check_path_reports_allow_and_deny() {
        let allowed = CheckPath
            .call(&ctx(), json!({"path": "src/main.rs"}))
            .await
            .unwrap();
        let text = match &allowed.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"allowed\": true"));

        let denied = CheckPath
            .call(&ctx(), json!({"path": "/etc/passwd"}))
            .await
            .unwrap();
        let text = match &denied.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"allowed\": false"));
    }

    #[tokio::test]
    async fn check_command_matches_allowlist() {
        let ok = CheckCommand
            .call(&ctx(), json!({"program": "git"}))
            .await
            .unwrap();
        let text = match &ok.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"allowed\": true"));

        let no = CheckCommand
            .call(&ctx(), json!({"program": "rm"}))
            .await
            .unwrap();
        let text = match &no.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"allowed\": false"));
    }

    #[tokio::test]
    async fn effective_policy_lists_commands() {
        let res = PolicyInspect.call(&ctx(), json!({})).await.unwrap();
        let text = match &res.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("git"));
        assert!(text.contains("allowedPathPatternCount"));
    }
}
