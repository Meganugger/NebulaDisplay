//! Windows subsystem tools: services, registry, event log, performance
//! counters, scheduled tasks, environment variables, firewall and network
//! adapters. Implemented as typed wrappers over `sc.exe`, `reg.exe`,
//! `schtasks.exe` and PowerShell/CIM. Windows-only.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::exec_result;
use crate::common::platform::{ensure_windows, run_powershell, POWERSHELL};
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "windows";

/// Build Windows subsystem tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WinService),
        Arc::new(WinRegistry),
        Arc::new(WinEventLog),
        Arc::new(WinPerfCounters),
        Arc::new(WinScheduledTasks),
        Arc::new(WinEnv),
        Arc::new(WinFirewall),
        Arc::new(WinNetworkAdapters),
    ]
}

fn ro() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    })
}

async fn run_cmd(
    ctx: &ToolContext,
    program: &str,
    args: Vec<String>,
    label: &str,
    timeout: Option<u64>,
) -> Result<CallToolResult> {
    let spec = CommandSpec::new(program, ctx.working_dir.clone(), ctx).args(args);
    let result = run_checked(ctx, spec, timeout).await?;
    Ok(exec_result(label, &result))
}

/// Windows service control via sc.exe.
struct WinService;

#[async_trait]
impl Tool for WinService {
    fn name(&self) -> &str {
        "windows.service"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Query or control a Windows service (query/start/stop/restart) via sc.exe. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("name", "Service name.", true)
            .enumerated(
                "action",
                "Action.",
                &["query", "start", "stop", "restart"],
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let name = a.str("name")?.to_string();
        let action = a.str_or("action", "query")?;
        match action {
            "query" => {
                run_cmd(
                    ctx,
                    "sc",
                    vec!["query".into(), name.clone()],
                    "sc query",
                    None,
                )
                .await
            }
            "start" => {
                run_cmd(
                    ctx,
                    "sc",
                    vec!["start".into(), name.clone()],
                    "sc start",
                    None,
                )
                .await
            }
            "stop" => {
                ctx.policy
                    .ensure_destructive_allowed("windows.service stop")?;
                run_cmd(
                    ctx,
                    "sc",
                    vec!["stop".into(), name.clone()],
                    "sc stop",
                    None,
                )
                .await
            }
            "restart" => {
                ctx.policy
                    .ensure_destructive_allowed("windows.service restart")?;
                // sc has no restart; use PowerShell Restart-Service.
                let script = format!(
                    "Restart-Service -Name '{}' -Force -PassThru | ConvertTo-Json",
                    name.replace('\'', "''")
                );
                let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
                Ok(exec_result("Restart-Service", &r))
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// Windows registry access via reg.exe.
struct WinRegistry;

#[async_trait]
impl Tool for WinRegistry {
    fn name(&self) -> &str {
        "windows.registry"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Query, set or delete registry keys/values via reg.exe. Delete is destructive. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated("action", "Action.", &["query", "add", "delete"], true)
            .string("key", "Registry key path, e.g. HKLM\\SOFTWARE\\Foo.", true)
            .string("value", "Value name (optional).", false)
            .string(
                "type",
                "Value type for add (REG_SZ, REG_DWORD, ...).",
                false,
            )
            .string("data", "Value data for add.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let action = a.str("action")?;
        let key = a.str("key")?.to_string();
        match action {
            "query" => {
                let mut args = vec!["query".to_string(), key];
                if let Some(v) = a.opt_str("value")? {
                    args.push("/v".into());
                    args.push(v.into());
                }
                run_cmd(ctx, "reg", args, "reg query", None).await
            }
            "add" => {
                let mut args = vec!["add".to_string(), key];
                if let Some(v) = a.opt_str("value")? {
                    args.push("/v".into());
                    args.push(v.into());
                }
                args.push("/t".into());
                args.push(a.str_or("type", "REG_SZ")?.into());
                args.push("/d".into());
                args.push(a.str_or("data", "")?.into());
                args.push("/f".into());
                run_cmd(ctx, "reg", args, "reg add", None).await
            }
            "delete" => {
                ctx.policy
                    .ensure_destructive_allowed("windows.registry delete")?;
                let mut args = vec!["delete".to_string(), key];
                if let Some(v) = a.opt_str("value")? {
                    args.push("/v".into());
                    args.push(v.into());
                }
                args.push("/f".into());
                run_cmd(ctx, "reg", args, "reg delete", None).await
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// Windows event log query via Get-WinEvent.
struct WinEventLog;

#[async_trait]
impl Tool for WinEventLog {
    fn name(&self) -> &str {
        "windows.event_log"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Query a Windows event log (e.g. System, Application) and return recent events as JSON. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("logName", "Log name, e.g. System or Application.", true)
            .integer("maxEvents", "Maximum events (default 50).", false)
            .string(
                "level",
                "Optional level filter: Critical, Error, Warning, Information.",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let log = a.str("logName")?.replace('\'', "''");
        let max = a.u64_or("maxEvents", 50)?;
        let level_map = match a.opt_str("level")? {
            Some("Critical") => ";Level=1",
            Some("Error") => ";Level=2",
            Some("Warning") => ";Level=3",
            Some("Information") => ";Level=4",
            _ => "",
        };
        let script = format!(
            "Get-WinEvent -FilterHashtable @{{LogName='{log}'{level_map}}} -MaxEvents {max} \
             | Select-Object TimeCreated,Id,LevelDisplayName,ProviderName,Message \
             | ConvertTo-Json -Depth 4"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
        Ok(exec_result("Get-WinEvent", &r))
    }
}

/// Performance counters via Get-Counter.
struct WinPerfCounters;

#[async_trait]
impl Tool for WinPerfCounters {
    fn name(&self) -> &str {
        "windows.perf_counters"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Sample Windows performance counters (e.g. '\\Processor(_Total)\\% Processor Time') via Get-Counter. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string_array("counters", "Counter paths to sample.", true)
            .integer("samples", "Number of samples (default 1).", false)
            .integer(
                "intervalSecs",
                "Seconds between samples (default 1).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let counters = a.str_array("counters")?;
        let list = counters
            .iter()
            .map(|c| format!("'{}'", c.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let samples = a.u64_or("samples", 1)?;
        let interval = a.u64_or("intervalSecs", 1)?;
        let script = format!(
            "Get-Counter -Counter {list} -MaxSamples {samples} -SampleInterval {interval} \
             | ForEach-Object {{ $_.CounterSamples }} \
             | Select-Object Path,CookedValue,Timestamp | ConvertTo-Json -Depth 4"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, Some(samples * interval + 30)).await?;
        Ok(exec_result("Get-Counter", &r))
    }
}

/// Scheduled tasks via schtasks.exe.
struct WinScheduledTasks;

#[async_trait]
impl Tool for WinScheduledTasks {
    fn name(&self) -> &str {
        "windows.scheduled_tasks"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List or control scheduled tasks (query/run/end) via schtasks.exe. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated("action", "Action.", &["query", "run", "end"], false)
            .string("taskName", "Task name (required for run/end).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        match a.str_or("action", "query")? {
            "query" => {
                run_cmd(
                    ctx,
                    "schtasks",
                    vec!["/query".into(), "/fo".into(), "CSV".into()],
                    "schtasks /query",
                    None,
                )
                .await
            }
            "run" => {
                let name = a.str("taskName")?;
                run_cmd(
                    ctx,
                    "schtasks",
                    vec!["/run".into(), "/tn".into(), name.into()],
                    "schtasks /run",
                    None,
                )
                .await
            }
            "end" => {
                let name = a.str("taskName")?;
                run_cmd(
                    ctx,
                    "schtasks",
                    vec!["/end".into(), "/tn".into(), name.into()],
                    "schtasks /end",
                    None,
                )
                .await
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// Environment variable inspection/mutation.
struct WinEnv;

#[async_trait]
impl Tool for WinEnv {
    fn name(&self) -> &str {
        "windows.env"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Get or set a Windows environment variable at Process/User/Machine scope. Machine scope requires elevation. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated("action", "Action.", &["get", "set"], true)
            .string("name", "Variable name.", true)
            .string("value", "Value to set (for set).", false)
            .enumerated("scope", "Scope.", &["Process", "User", "Machine"], false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let name = a.str("name")?.replace('\'', "''");
        let scope = a.str_or("scope", "Process")?;
        match a.str("action")? {
            "get" => {
                let script = format!("[Environment]::GetEnvironmentVariable('{name}','{scope}')");
                let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
                Ok(exec_result("GetEnvironmentVariable", &r))
            }
            "set" => {
                if scope == "Machine" {
                    ctx.policy.ensure_elevation_allowed()?;
                }
                let value = a.str_or("value", "")?.replace('\'', "''");
                let script =
                    format!("[Environment]::SetEnvironmentVariable('{name}','{value}','{scope}')");
                let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
                Ok(exec_result("SetEnvironmentVariable", &r))
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// Firewall rule inspection via Get-NetFirewallRule.
struct WinFirewall;

#[async_trait]
impl Tool for WinFirewall {
    fn name(&self) -> &str {
        "windows.firewall"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List Windows firewall rules (optionally filtered by display-name substring) as JSON. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "nameContains",
                "Optional display-name substring filter.",
                false,
            )
            .integer("limit", "Maximum rules (default 100).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let limit = a.u64_or("limit", 100)?;
        let filter = match a.opt_str("nameContains")? {
            Some(f) => format!(
                "| Where-Object {{ $_.DisplayName -like '*{}*' }} ",
                f.replace('\'', "''")
            ),
            None => String::new(),
        };
        let script = format!(
            "Get-NetFirewallRule {filter}| Select-Object -First {limit} \
             DisplayName,Direction,Action,Enabled,Profile | ConvertTo-Json -Depth 4"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
        Ok(exec_result("Get-NetFirewallRule", &r))
    }
}

/// Network adapter inventory via Get-NetAdapter.
struct WinNetworkAdapters;

#[async_trait]
impl Tool for WinNetworkAdapters {
    fn name(&self) -> &str {
        "windows.network_adapters"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List network adapters (name, status, MAC, link speed) as JSON. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let _ = Args::new(&args)?;
        let script = "Get-NetAdapter | Select-Object Name,InterfaceDescription,Status,MacAddress,LinkSpeed | ConvertTo-Json -Depth 4";
        let r = run_powershell(ctx, POWERSHELL, script, None).await?;
        Ok(exec_result("Get-NetAdapter", &r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::Metrics;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        let base = SecurityConfig {
            allowed_commands: vec![
                "sc".into(),
                "reg".into(),
                "schtasks".into(),
                "powershell".into(),
            ],
            ..Default::default()
        };
        let policy = EffectivePolicy::build("windows.service", &base, None).unwrap();
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
    async fn windows_tools_gate_off_windows() {
        if cfg!(windows) {
            return;
        }
        for res in [
            WinService.call(&ctx(), json!({"name": "spooler"})).await,
            WinRegistry
                .call(&ctx(), json!({"action": "query", "key": "HKLM\\SOFTWARE"}))
                .await,
            WinNetworkAdapters.call(&ctx(), json!({})).await,
        ] {
            assert!(matches!(
                res.unwrap_err(),
                ToolError::PlatformUnsupported(_)
            ));
        }
    }
}
