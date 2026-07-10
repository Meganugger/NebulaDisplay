//! PowerShell tools: non-interactive command/script execution, elevated
//! execution, and remoting. Windows-only; on other hosts every tool returns a
//! structured [`nebula_mcp_core::ToolError::PlatformUnsupported`].

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::exec_result;
use crate::common::platform::{ensure_windows, run_powershell, POWERSHELL};
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "powershell";

/// Build PowerShell tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(PsRun), Arc::new(PsElevated), Arc::new(PsRemote)]
}

fn open_world() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        open_world_hint: Some(true),
        ..Default::default()
    })
}

/// Non-interactive PowerShell command execution.
struct PsRun;

#[async_trait]
impl Tool for PsRun {
    fn name(&self) -> &str {
        "powershell.run"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run a PowerShell command non-interactively (-NoProfile -NonInteractive). \
         Have the script emit JSON (e.g. '... | ConvertTo-Json') for structured output. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("script", "PowerShell command/script to run.", true)
            .enumerated("shell", "Shell to use.", &["powershell", "pwsh"], false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        open_world()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let script = a.str("script")?;
        let shell = a.str_or("shell", POWERSHELL)?;
        let result = run_powershell(ctx, shell, script, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result("powershell", &result))
    }
}

/// Elevated PowerShell execution via a self-elevating Start-Process.
struct PsElevated;

#[async_trait]
impl Tool for PsElevated {
    fn name(&self) -> &str {
        "powershell.elevated"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run a PowerShell command elevated (UAC). Output is redirected to a file and returned. \
         Requires allow_elevated. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("script", "PowerShell command to run elevated.", true)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        open_world()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        ctx.policy.ensure_elevation_allowed()?;
        let script = a.str("script")?;
        // Launch an elevated child that writes combined output to a temp file,
        // then read it back. Escaping single quotes for the inner script.
        let escaped = script.replace('\'', "''");
        let wrapper = format!(
            "$tmp = [System.IO.Path]::GetTempFileName(); \
             $p = Start-Process -FilePath powershell -Verb RunAs -Wait -PassThru \
                 -ArgumentList '-NoProfile','-Command',\"& {{ {escaped} }} *> $tmp\"; \
             Get-Content -Raw $tmp; Remove-Item $tmp -ErrorAction SilentlyContinue; \
             exit $p.ExitCode"
        );
        let result = run_powershell(ctx, POWERSHELL, &wrapper, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result("powershell (elevated)", &result))
    }
}

/// PowerShell remoting via Invoke-Command.
struct PsRemote;

#[async_trait]
impl Tool for PsRemote {
    fn name(&self) -> &str {
        "powershell.remote"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run a PowerShell command on a remote computer via Invoke-Command (WinRM must be configured). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("computer", "Remote computer name.", true)
            .string("script", "Command to run remotely.", true)
            .string(
                "credentialUser",
                "Optional user name for the remote session.",
                false,
            )
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        open_world()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let computer = a.str("computer")?;
        let script = a.str("script")?.replace('\'', "''");
        let cred = a.opt_str("credentialUser")?;
        let cred_clause = match cred {
            Some(u) => format!(
                "-Credential (Get-Credential -UserName '{}' -Message 'nebula-mcp remote')",
                u.replace('\'', "''")
            ),
            None => String::new(),
        };
        let wrapper = format!(
            "Invoke-Command -ComputerName '{}' {} -ScriptBlock {{ {} }} | ConvertTo-Json -Depth 6",
            computer.replace('\'', "''"),
            cred_clause,
            script
        );
        let spec = CommandSpec::new(POWERSHELL, ctx.working_dir.clone(), ctx).args(vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            wrapper,
        ]);
        let result = run_checked(ctx, spec, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result("powershell (remote)", &result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::{Metrics, ToolError};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        let base = SecurityConfig {
            allowed_commands: vec!["powershell".into(), "pwsh".into()],
            allow_elevated: true,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("powershell.run", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Default::default()),
            request_id: "r".into(),
            progress: None,
        }
    }

    #[tokio::test]
    async fn returns_platform_unsupported_off_windows() {
        if cfg!(windows) {
            return;
        }
        let err = PsRun
            .call(&ctx(), json!({"script": "Get-Process"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PlatformUnsupported(_)));
    }
}
