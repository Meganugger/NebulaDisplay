//! Windows driver toolchain tools: build, catalog signing, driver package
//! install/uninstall, Driver Verifier, IddCx diagnostics and display-driver
//! restart. Typed wrappers over `msbuild`, `inf2cat`, `signtool`, `pnputil`,
//! `devcon`, `verifier` and PowerShell PnP cmdlets. Windows-only.
//!
//! Install/uninstall, Verifier changes and display-driver restart are gated by
//! both the elevation and destructive policy switches because they mutate
//! kernel-mode driver state.

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

const CATEGORY: &str = "driver";

/// Build driver toolchain tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DriverBuild),
        Arc::new(Inf2Cat),
        Arc::new(SignTool),
        Arc::new(PnpUtil),
        Arc::new(DevCon),
        Arc::new(DriverVerifier),
        Arc::new(IddcxDiagnostics),
        Arc::new(DisplayRestart),
        Arc::new(DriverInstall),
        Arc::new(DriverUninstall),
        Arc::new(DriverLogs),
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

fn destructive() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        destructive_hint: Some(true),
        open_world_hint: Some(true),
        ..Default::default()
    })
}

/// Build a driver project with MSBuild.
struct DriverBuild;

#[async_trait]
impl Tool for DriverBuild {
    fn name(&self) -> &str {
        "driver.build"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Build a driver solution/project with MSBuild. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("project", "Path to .sln/.vcxproj.", true)
            .string(
                "configuration",
                "Build configuration (default Release).",
                false,
            )
            .string("platform", "Target platform (default x64).", false)
            .string_array("extraArgs", "Additional MSBuild arguments.", false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let project = ctx.resolve_path(a.str("project")?)?;
        let config = a.str_or("configuration", "Release")?;
        let platform = a.str_or("platform", "x64")?;
        let mut msbuild_args = vec![
            project.display().to_string(),
            format!("/p:Configuration={config}"),
            format!("/p:Platform={platform}"),
            "/nologo".into(),
            "/verbosity:minimal".into(),
        ];
        msbuild_args.extend(a.opt_str_array("extraArgs")?);
        run_cmd(
            ctx,
            "msbuild",
            msbuild_args,
            "msbuild",
            a.opt_u64("timeoutSecs")?,
        )
        .await
    }
}

/// Generate a catalog with inf2cat.
struct Inf2Cat;

#[async_trait]
impl Tool for Inf2Cat {
    fn name(&self) -> &str {
        "driver.inf2cat"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Create a driver catalog (.cat) from an INF directory with inf2cat. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("driverDir", "Directory containing the INF.", true)
            .string("os", "Target OS list (default 10_X64,Server10_X64).", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let dir = ctx.resolve_path(a.str("driverDir")?)?;
        let os = a.str_or("os", "10_X64,Server10_X64")?;
        run_cmd(
            ctx,
            "inf2cat",
            vec![
                format!("/driver:{}", dir.display()),
                format!("/os:{os}"),
                "/verbose".into(),
            ],
            "inf2cat",
            None,
        )
        .await
    }
}

/// Sign a file with signtool.
struct SignTool;

#[async_trait]
impl Tool for SignTool {
    fn name(&self) -> &str {
        "driver.signtool"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Sign a driver/catalog/binary with signtool using a certificate store thumbprint. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("file", "File to sign (.sys/.cat/.dll/.exe).", true)
            .string(
                "thumbprint",
                "Certificate SHA-1 thumbprint in the store.",
                true,
            )
            .string("timestampUrl", "RFC3161 timestamp URL.", false)
            .string("digest", "File digest algorithm (default sha256).", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let file = ctx.resolve_path(a.str("file")?)?;
        let thumb = a.str("thumbprint")?;
        let digest = a.str_or("digest", "sha256")?;
        let mut st_args = vec![
            "sign".to_string(),
            "/sha1".into(),
            thumb.into(),
            "/fd".into(),
            digest.into(),
        ];
        if let Some(ts) = a.opt_str("timestampUrl")? {
            st_args.push("/tr".into());
            st_args.push(ts.into());
            st_args.push("/td".into());
            st_args.push(digest.into());
        }
        st_args.push(file.display().to_string());
        run_cmd(ctx, "signtool", st_args, "signtool sign", None).await
    }
}

/// Generic pnputil pass-through with an action allowlist.
struct PnpUtil;

#[async_trait]
impl Tool for PnpUtil {
    fn name(&self) -> &str {
        "driver.pnputil"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run pnputil with a bounded action set: enum_drivers, add_driver, delete_driver. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated(
                "action",
                "Action.",
                &["enum_drivers", "add_driver", "delete_driver"],
                true,
            )
            .string(
                "inf",
                "INF path (add) or published oemNN.inf name (delete).",
                false,
            )
            .boolean(
                "install",
                "Also install/uninstall the device (implies elevation+destructive).",
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        match a.str("action")? {
            "enum_drivers" => {
                run_cmd(
                    ctx,
                    "pnputil",
                    vec!["/enum-drivers".into()],
                    "pnputil /enum-drivers",
                    None,
                )
                .await
            }
            "add_driver" => {
                let inf = a.str("inf")?;
                let install = a.bool_or("install", false)?;
                let mut v = vec!["/add-driver".to_string(), inf.into()];
                if install {
                    ctx.policy.ensure_elevation_allowed()?;
                    ctx.policy
                        .ensure_destructive_allowed("pnputil add-driver /install")?;
                    v.push("/install".into());
                }
                run_cmd(ctx, "pnputil", v, "pnputil /add-driver", None).await
            }
            "delete_driver" => {
                ctx.policy.ensure_elevation_allowed()?;
                ctx.policy
                    .ensure_destructive_allowed("pnputil delete-driver")?;
                let inf = a.str("inf")?;
                let mut v = vec!["/delete-driver".to_string(), inf.into()];
                if a.bool_or("install", false)? {
                    v.push("/uninstall".into());
                }
                v.push("/force".into());
                run_cmd(ctx, "pnputil", v, "pnputil /delete-driver", None).await
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// devcon device management.
struct DevCon;

#[async_trait]
impl Tool for DevCon {
    fn name(&self) -> &str {
        "driver.devcon"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run devcon for device management: status, restart, enable, disable. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated(
                "action",
                "Action.",
                &["status", "restart", "enable", "disable"],
                true,
            )
            .string("hardwareId", "Hardware/instance ID pattern.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        destructive()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let action = a.str("action")?;
        let hwid = a.str("hardwareId")?.to_string();
        if matches!(action, "restart" | "enable" | "disable") {
            ctx.policy.ensure_elevation_allowed()?;
            ctx.policy
                .ensure_destructive_allowed(&format!("devcon {action}"))?;
        }
        run_cmd(
            ctx,
            "devcon",
            vec![action.into(), hwid],
            &format!("devcon {action}"),
            None,
        )
        .await
    }
}

/// Driver Verifier control.
struct DriverVerifier;

#[async_trait]
impl Tool for DriverVerifier {
    fn name(&self) -> &str {
        "driver.verifier"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Control Driver Verifier: query, standard (enable standard flags for a driver), or reset. \
         Enabling/resetting requires elevation and destructive policy (takes effect after reboot and can cause bugchecks). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated("action", "Action.", &["query", "standard", "reset"], true)
            .string(
                "driver",
                "Driver file name (e.g. mydriver.sys) for 'standard'.",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        destructive()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        match a.str("action")? {
            "query" => {
                run_cmd(
                    ctx,
                    "verifier",
                    vec!["/query".into()],
                    "verifier /query",
                    None,
                )
                .await
            }
            "standard" => {
                ctx.policy.ensure_elevation_allowed()?;
                ctx.policy
                    .ensure_destructive_allowed("verifier /standard")?;
                let driver = a.str("driver")?;
                run_cmd(
                    ctx,
                    "verifier",
                    vec!["/standard".into(), "/driver".into(), driver.into()],
                    "verifier /standard",
                    None,
                )
                .await
            }
            "reset" => {
                ctx.policy.ensure_elevation_allowed()?;
                ctx.policy.ensure_destructive_allowed("verifier /reset")?;
                run_cmd(
                    ctx,
                    "verifier",
                    vec!["/reset".into()],
                    "verifier /reset",
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

/// IddCx / indirect-display diagnostics via PnP cmdlets.
struct IddcxDiagnostics;

#[async_trait]
impl Tool for IddcxDiagnostics {
    fn name(&self) -> &str {
        "driver.iddcx_diagnostics"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Report indirect display (IddCx) driver state: display-class PnP devices and their status/problem codes as JSON. Windows only."
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
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let _ = Args::new(&args)?;
        let script = "Get-PnpDevice -Class Display | \
             Select-Object FriendlyName,InstanceId,Status,Problem,ProblemDescription | \
             ConvertTo-Json -Depth 4";
        let r = run_powershell(ctx, POWERSHELL, script, None).await?;
        Ok(exec_result("Get-PnpDevice -Class Display", &r))
    }
}

/// Restart the display adapter driver.
struct DisplayRestart;

#[async_trait]
impl Tool for DisplayRestart {
    fn name(&self) -> &str {
        "driver.display_restart"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Restart display-class PnP devices (Disable/Enable) to reload the display driver. \
         Requires elevation and destructive policy; screens may flicker. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "instanceId",
                "Optional specific device instance id; otherwise all display devices.",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        destructive()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        ctx.policy.ensure_elevation_allowed()?;
        ctx.policy
            .ensure_destructive_allowed("driver.display_restart")?;
        let selector = match a.opt_str("instanceId")? {
            Some(id) => format!("-InstanceId '{}'", id.replace('\'', "''")),
            None => "-Class Display".to_string(),
        };
        let script = format!(
            "Get-PnpDevice {selector} | Where-Object {{ $_.Status -eq 'OK' }} | \
             ForEach-Object {{ Disable-PnpDevice -InstanceId $_.InstanceId -Confirm:$false; \
             Start-Sleep -Milliseconds 500; \
             Enable-PnpDevice -InstanceId $_.InstanceId -Confirm:$false; $_.InstanceId }}"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, Some(60)).await?;
        Ok(exec_result("display restart", &r))
    }
}

/// Install a driver package (pnputil /add-driver /install).
struct DriverInstall;

#[async_trait]
impl Tool for DriverInstall {
    fn name(&self) -> &str {
        "driver.install"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Install a driver package from an INF (pnputil /add-driver /install). Requires elevation and destructive policy. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("inf", "Path to the driver INF.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        destructive()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        ctx.policy.ensure_elevation_allowed()?;
        ctx.policy.ensure_destructive_allowed("driver.install")?;
        let inf = ctx.resolve_path(a.str("inf")?)?;
        run_cmd(
            ctx,
            "pnputil",
            vec![
                "/add-driver".into(),
                inf.display().to_string(),
                "/install".into(),
            ],
            "pnputil /add-driver /install",
            None,
        )
        .await
    }
}

/// Uninstall a driver package (pnputil /delete-driver /uninstall).
struct DriverUninstall;

#[async_trait]
impl Tool for DriverUninstall {
    fn name(&self) -> &str {
        "driver.uninstall"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Uninstall a published driver package by oemNN.inf name (pnputil /delete-driver /uninstall /force). \
         Requires elevation and destructive policy. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "publishedName",
                "Published driver name, e.g. oem12.inf.",
                true,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        destructive()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        ctx.policy.ensure_elevation_allowed()?;
        ctx.policy.ensure_destructive_allowed("driver.uninstall")?;
        let name = a.str("publishedName")?;
        run_cmd(
            ctx,
            "pnputil",
            vec![
                "/delete-driver".into(),
                name.into(),
                "/uninstall".into(),
                "/force".into(),
            ],
            "pnputil /delete-driver /uninstall",
            None,
        )
        .await
    }
}

/// Collect PnP/driver-related event log entries.
struct DriverLogs;

#[async_trait]
impl Tool for DriverLogs {
    fn name(&self) -> &str {
        "driver.logs"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Collect recent PnP/driver-related events (Kernel-PnP configuration log) as JSON. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("maxEvents", "Maximum events (default 50).", false)
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
        let max = a.u64_or("maxEvents", 50)?;
        let script = format!(
            "Get-WinEvent -LogName 'Microsoft-Windows-Kernel-PnP/Configuration' -MaxEvents {max} \
             | Select-Object TimeCreated,Id,LevelDisplayName,Message | ConvertTo-Json -Depth 4"
        );
        let r = run_powershell(ctx, POWERSHELL, &script, None).await?;
        Ok(exec_result("driver logs", &r))
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
            allowed_paths: vec!["/**".into()],
            allowed_commands: vec!["pnputil".into(), "msbuild".into(), "signtool".into()],
            allow_elevated: true,
            allow_destructive: true,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("driver.install", &base, None).unwrap();
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
    async fn driver_tools_gate_off_windows() {
        if cfg!(windows) {
            return;
        }
        let err = DriverInstall
            .call(&ctx(), json!({"inf": "/tmp/x.inf"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PlatformUnsupported(_)));
    }
}
