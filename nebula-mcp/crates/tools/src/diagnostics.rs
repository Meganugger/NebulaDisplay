//! Diagnostics tools.
//!
//! A cross-platform capability probe (which external toolchains are available)
//! plus Windows crash-analysis wrappers: WER report enumeration, crash-dump
//! discovery, process dump creation (`procdump`), minidump analysis and live
//! stack capture (`cdb`), and ETW session control (`logman`).

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::exec::{program_available, run_checked, CommandSpec};
use crate::common::output::{exec_result, json_value_result};
use crate::common::platform::{ensure_windows, run_powershell, POWERSHELL};
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "diagnostics";

/// Build diagnostics tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Capabilities),
        Arc::new(ServerMetrics),
        Arc::new(EffectiveConfig),
        Arc::new(WerReports),
        Arc::new(CrashDumps),
        Arc::new(CreateDump),
        Arc::new(AnalyzeDump),
        Arc::new(StackTrace),
        Arc::new(EtwTrace),
    ]
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

/// Report the host OS and which external toolchains are installed. This lets an
/// agent plan work against the tools actually available.
struct Capabilities;

#[async_trait]
impl Tool for Capabilities {
    fn name(&self) -> &str {
        "diagnostics.capabilities"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Report the host platform and availability of external toolchains (git, msbuild, signtool, pnputil, PresentMon, ffmpeg, cdb, ...). Cross-platform."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, _ctx: &ToolContext, _args: Value) -> Result<CallToolResult> {
        const PROBES: &[&str] = &[
            "git",
            "gh",
            "cargo",
            "rustc",
            "node",
            "npx",
            "python",
            "docker",
            "curl",
            "ffmpeg",
            "iperf3",
            "ping",
            "powershell",
            "pwsh",
            "msbuild",
            "signtool",
            "inf2cat",
            "pnputil",
            "devcon",
            "verifier",
            "PresentMon",
            "wpr",
            "wpaexporter",
            "cdb",
            "procdump",
            "logman",
            "dumpcap",
            "tcpdump",
        ];
        let available: Vec<Value> = PROBES
            .iter()
            .map(|p| json!({ "tool": p, "available": program_available(p) }))
            .collect();
        Ok(json_value_result(json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
            "isWindows": cfg!(windows),
            "tools": available,
        })))
    }
}

/// Live per-tool metrics snapshot.
struct ServerMetrics;

#[async_trait]
impl Tool for ServerMetrics {
    fn name(&self) -> &str {
        "diagnostics.metrics"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Return live per-tool metrics (call counts, successes, failures, cancellations, mean/max durations, output bytes). Cross-platform."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<CallToolResult> {
        let snapshot = ctx.metrics.snapshot();
        Ok(json_value_result(json!({
            "toolCount": snapshot.len(),
            "tools": snapshot,
        })))
    }
}

/// Effective configuration / policy summary (no secrets).
struct EffectiveConfig;

#[async_trait]
impl Tool for EffectiveConfig {
    fn name(&self) -> &str {
        "diagnostics.config"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Summarise the server's effective configuration and permission policy (path/command counts, gates, disabled categories/tools). Cross-platform."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<CallToolResult> {
        let c = &ctx.config;
        let disabled_categories: Vec<&String> = c
            .categories
            .iter()
            .filter(|(_, v)| !v.enabled)
            .map(|(k, _)| k)
            .collect();
        let disabled_tools: Vec<&String> = c
            .tools
            .iter()
            .filter(|(_, v)| v.enabled == Some(false))
            .map(|(k, _)| k)
            .collect();
        Ok(json_value_result(json!({
            "server": {
                "name": c.server.name,
                "version": c.server.version,
                "maxConcurrentCalls": c.server.max_concurrent_calls,
            },
            "security": {
                "allowedPathCount": c.security.allowed_paths.len(),
                "deniedPathCount": c.security.denied_paths.len(),
                "allowedCommandCount": c.security.allowed_commands.len(),
                "defaultTimeoutSecs": c.security.default_timeout_secs,
                "maxRuntimeSecs": c.security.max_runtime_secs,
                "maxOutputBytes": c.security.max_output_bytes,
                "allowElevated": c.security.allow_elevated,
                "allowNetwork": c.security.allow_network,
                "allowDestructive": c.security.allow_destructive,
            },
            "workingDir": ctx.working_dir.display().to_string(),
            "disabledCategories": disabled_categories,
            "disabledTools": disabled_tools,
        })))
    }
}

/// Windows Error Reporting event enumeration.
struct WerReports;

#[async_trait]
impl Tool for WerReports {
    fn name(&self) -> &str {
        "diagnostics.wer_reports"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List recent Windows Error Reporting (application crash) events as JSON. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("maxEvents", "Maximum events (default 30).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let max = a.u64_or("maxEvents", 30)?;
        let script = format!(
            "Get-WinEvent -FilterHashtable @{{LogName='Application';ProviderName='Windows Error Reporting'}} -MaxEvents {max} \
             | Select-Object TimeCreated,Id,LevelDisplayName,Message | ConvertTo-Json -Depth 4"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
        Ok(exec_result("WER reports", &r))
    }
}

/// Enumerate crash-dump files in the standard WER locations (or a custom dir).
struct CrashDumps;

#[async_trait]
impl Tool for CrashDumps {
    fn name(&self) -> &str {
        "diagnostics.crash_dumps"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List crash dump (.dmp) files under the local WER CrashDumps directory or a provided directory. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "directory",
                "Directory to scan (default %LOCALAPPDATA%\\CrashDumps).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let dir = match a.opt_str("directory")? {
            Some(d) => d.to_string(),
            None => {
                let local = std::env::var("LOCALAPPDATA")
                    .map_err(|_| ToolError::Execution("LOCALAPPDATA not set".into()))?;
                format!("{local}\\CrashDumps")
            }
        };
        let script = format!(
            "Get-ChildItem -Path '{}' -Filter *.dmp -ErrorAction SilentlyContinue \
             | Select-Object FullName,Length,LastWriteTime | ConvertTo-Json -Depth 3",
            dir.replace('\'', "''")
        );
        let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
        Ok(exec_result("crash dumps", &r))
    }
}

/// Create a process dump with procdump.
struct CreateDump;

#[async_trait]
impl Tool for CreateDump {
    fn name(&self) -> &str {
        "diagnostics.create_dump"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Create a full memory dump of a process with procdump (Sysinternals). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("pid", "Process id.", true)
            .string(
                "outputPath",
                "Dump output path (within an allowed root).",
                true,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let pid = a
            .opt_u64("pid")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'pid'".into()))?;
        let out = ctx.resolve_path(a.str("outputPath")?)?;
        run_cmd(
            ctx,
            "procdump",
            vec![
                "-accepteula".into(),
                "-ma".into(),
                pid.to_string(),
                out.display().to_string(),
            ],
            "procdump",
            Some(120),
        )
        .await
    }
}

/// Analyse a minidump with cdb.
struct AnalyzeDump;

#[async_trait]
impl Tool for AnalyzeDump {
    fn name(&self) -> &str {
        "diagnostics.analyze_dump"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Analyse a crash dump with cdb (!analyze -v) and return the analysis text. cdb must be allowlisted. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("dump", "Path to the .dmp file.", true)
            .string(
                "commands",
                "Debugger commands (default '!analyze -v').",
                false,
            )
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let dump = ctx.resolve_path(a.str("dump")?)?;
        let commands = a.str_or("commands", "!analyze -v")?;
        let script = format!("{commands}; q");
        run_cmd(
            ctx,
            "cdb",
            vec!["-z".into(), dump.display().to_string(), "-c".into(), script],
            "cdb !analyze",
            a.opt_u64("timeoutSecs")?,
        )
        .await
    }
}

/// Capture live thread stacks of a process with cdb (non-invasive attach).
struct StackTrace;

#[async_trait]
impl Tool for StackTrace {
    fn name(&self) -> &str {
        "diagnostics.stack_trace"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Capture stacks of all threads in a running process via cdb (~*k). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("pid", "Process id to attach to.", true)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let pid = a
            .opt_u64("pid")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'pid'".into()))?;
        run_cmd(
            ctx,
            "cdb",
            vec![
                "-pv".into(),
                "-p".into(),
                pid.to_string(),
                "-c".into(),
                "~*k; q".into(),
            ],
            "cdb stacks",
            a.opt_u64("timeoutSecs")?,
        )
        .await
    }
}

/// ETW session control via logman.
struct EtwTrace;

#[async_trait]
impl Tool for EtwTrace {
    fn name(&self) -> &str {
        "diagnostics.etw_trace"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Control an ETW trace session with logman: create/start/stop/delete/query. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated(
                "action",
                "Action.",
                &["create", "start", "stop", "delete", "query"],
                true,
            )
            .string("session", "Trace session name.", true)
            .string("provider", "Provider GUID/name for 'create'.", false)
            .string(
                "outputEtl",
                "Output ETL path for 'create' (within an allowed root).",
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let session = a.str("session")?.to_string();
        match a.str("action")? {
            "create" => {
                let provider = a.str("provider")?;
                let out = ctx.resolve_path(a.str("outputEtl")?)?;
                run_cmd(
                    ctx,
                    "logman",
                    vec![
                        "create".into(),
                        "trace".into(),
                        session,
                        "-p".into(),
                        provider.into(),
                        "-o".into(),
                        out.display().to_string(),
                        "-ets".into(),
                    ],
                    "logman create trace",
                    None,
                )
                .await
            }
            "start" => {
                run_cmd(
                    ctx,
                    "logman",
                    vec!["start".into(), session, "-ets".into()],
                    "logman start",
                    None,
                )
                .await
            }
            "stop" => {
                run_cmd(
                    ctx,
                    "logman",
                    vec!["stop".into(), session, "-ets".into()],
                    "logman stop",
                    None,
                )
                .await
            }
            "delete" => {
                run_cmd(
                    ctx,
                    "logman",
                    vec!["delete".into(), session, "-ets".into()],
                    "logman delete",
                    None,
                )
                .await
            }
            "query" => {
                run_cmd(
                    ctx,
                    "logman",
                    vec!["query".into(), session, "-ets".into()],
                    "logman query",
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

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::Metrics;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec!["/**".into()],
            allowed_commands: vec!["cdb".into(), "procdump".into(), "logman".into()],
            ..Default::default()
        };
        let policy = EffectivePolicy::build("diagnostics.capabilities", &base, None).unwrap();
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
    async fn capabilities_reports_platform() {
        let res = Capabilities.call(&ctx(), json!({})).await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = match &res.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"os\""));
        assert!(text.contains("\"tools\""));
    }

    #[tokio::test]
    async fn analyze_dump_gated_off_windows() {
        if cfg!(windows) {
            return;
        }
        let err = AnalyzeDump
            .call(&ctx(), json!({"dump": "/tmp/x.dmp"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PlatformUnsupported(_)));
    }

    #[tokio::test]
    async fn metrics_and_config_are_readonly_cross_platform() {
        let m = ServerMetrics.call(&ctx(), json!({})).await.unwrap();
        assert_eq!(m.is_error, Some(false));
        let c = EffectiveConfig.call(&ctx(), json!({})).await.unwrap();
        assert_eq!(c.is_error, Some(false));
        let text = match &c.content[0] {
            nebula_mcp_protocol::Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("allowElevated"));
        assert!(text.contains("maxRuntimeSecs"));
    }
}
