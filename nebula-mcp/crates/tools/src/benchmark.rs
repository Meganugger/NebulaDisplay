//! Benchmarking / profiling tools.
//!
//! Windows performance tooling is exposed through typed wrappers over the
//! standard utilities (`PresentMon`, WPR, `wpaexporter`, GPUView's logger,
//! LatencyMon CLI). `ffmpeg` and the cross-platform system sampler work on any
//! host. Frame-latency, encode/decode-latency and frame-pacing metrics are
//! derived from `PresentMon` CSV output.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde::Serialize;
use serde_json::{json, Value};
use sysinfo::System;

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::{exec_result, json_value_result};
use crate::common::platform::ensure_windows;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "benchmark";

/// Build benchmark tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SystemSample),
        Arc::new(Ffmpeg),
        Arc::new(PresentMon),
        Arc::new(Wpr),
        Arc::new(WpaExport),
        Arc::new(GpuView),
        Arc::new(LatencyMon),
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

/// Cross-platform CPU/memory sampling (GPU where the OS exposes it cheaply).
struct SystemSample;

#[async_trait]
impl Tool for SystemSample {
    fn name(&self) -> &str {
        "benchmark.system"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Sample CPU utilisation, per-core load and memory usage over a short interval. Cross-platform."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer(
                "intervalMs",
                "Sampling interval in ms (default 500).",
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
        let a = Args::new(&args)?;
        let interval = a.u64_or("intervalMs", 500)?.clamp(100, 5000);
        ctx.ensure_active()?;

        #[derive(Serialize)]
        struct Sample {
            cpu_global: f32,
            per_core: Vec<f32>,
            memory_used: u64,
            memory_total: u64,
            swap_used: u64,
            swap_total: u64,
        }

        let sample = tokio::task::spawn_blocking(move || {
            let mut sys = System::new_all();
            sys.refresh_cpu_all();
            std::thread::sleep(std::time::Duration::from_millis(interval));
            sys.refresh_cpu_all();
            sys.refresh_memory();
            Sample {
                cpu_global: sys.global_cpu_usage(),
                per_core: sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
                memory_used: sys.used_memory(),
                memory_total: sys.total_memory(),
                swap_used: sys.used_swap(),
                swap_total: sys.total_swap(),
            }
        })
        .await
        .map_err(|e| ToolError::Internal(format!("sample join: {e}")))?;

        Ok(json_value_result(json!({
            "intervalMs": interval,
            "cpuGlobalPercent": sample.cpu_global,
            "perCorePercent": sample.per_core,
            "memoryUsedBytes": sample.memory_used,
            "memoryTotalBytes": sample.memory_total,
            "swapUsedBytes": sample.swap_used,
            "swapTotalBytes": sample.swap_total,
        })))
    }
}

/// ffmpeg pass-through for encode/decode latency and media benchmarking.
struct Ffmpeg;

#[async_trait]
impl Tool for Ffmpeg {
    fn name(&self) -> &str {
        "benchmark.ffmpeg"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run ffmpeg with the given arguments (encode/decode benchmarking, transcoding). ffmpeg must be allowlisted. Cross-platform."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string_array(
                "args",
                "ffmpeg arguments (e.g. ['-benchmark','-i','in.mp4', ...]).",
                true,
            )
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let ff_args = a.str_array("args")?;
        run_cmd(ctx, "ffmpeg", ff_args, "ffmpeg", a.opt_u64("timeoutSecs")?).await
    }
}

/// PresentMon frame-timing capture.
struct PresentMon;

#[async_trait]
impl Tool for PresentMon {
    fn name(&self) -> &str {
        "benchmark.presentmon"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Capture frame timing (frame latency, frame pacing, present mode) with PresentMon to a CSV file. \
         Provide processName or captureAll; the CSV path must be within an allowed root. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "processName",
                "Target process image name (e.g. game.exe).",
                false,
            )
            .boolean("captureAll", "Capture all processes.", false)
            .integer(
                "timedSecs",
                "Capture duration in seconds (default 10).",
                false,
            )
            .string(
                "outputCsv",
                "CSV output path (within an allowed root).",
                true,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let out = ctx.resolve_path(a.str("outputCsv")?)?;
        let timed = a.u64_or("timedSecs", 10)?;
        let mut pm_args = vec![
            "-output_file".to_string(),
            out.display().to_string(),
            "-timed".into(),
            timed.to_string(),
            "-stop_existing_session".into(),
            "-no_top".into(),
        ];
        if let Some(name) = a.opt_str("processName")? {
            pm_args.push("-process_name".into());
            pm_args.push(name.into());
        } else if !a.bool_or("captureAll", false)? {
            return Err(ToolError::InvalidArguments(
                "provide processName or set captureAll=true".into(),
            ));
        }
        run_cmd(ctx, "PresentMon", pm_args, "PresentMon", Some(timed + 30)).await
    }
}

/// Windows Performance Recorder control.
struct Wpr;

#[async_trait]
impl Tool for Wpr {
    fn name(&self) -> &str {
        "benchmark.wpr"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Control Windows Performance Recorder: start a profile, or stop and write an ETL trace. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated(
                "action",
                "Action.",
                &["start", "stop", "cancel", "status"],
                true,
            )
            .string(
                "profile",
                "Profile for start (default GeneralProfile).",
                false,
            )
            .string(
                "outputEtl",
                "ETL output path for stop (within an allowed root).",
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        match a.str("action")? {
            "start" => {
                let profile = a.str_or("profile", "GeneralProfile")?;
                run_cmd(
                    ctx,
                    "wpr",
                    vec!["-start".into(), profile.into()],
                    "wpr -start",
                    None,
                )
                .await
            }
            "stop" => {
                let out = ctx.resolve_path(a.str("outputEtl")?)?;
                run_cmd(
                    ctx,
                    "wpr",
                    vec!["-stop".into(), out.display().to_string()],
                    "wpr -stop",
                    Some(300),
                )
                .await
            }
            "cancel" => run_cmd(ctx, "wpr", vec!["-cancel".into()], "wpr -cancel", None).await,
            "status" => run_cmd(ctx, "wpr", vec!["-status".into()], "wpr -status", None).await,
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}'"
            ))),
        }
    }
}

/// Export data from an ETL trace with wpaexporter (the WPA CLI companion).
struct WpaExport;

#[async_trait]
impl Tool for WpaExport {
    fn name(&self) -> &str {
        "benchmark.wpa_export"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Export tables from an ETL trace to CSV using wpaexporter (WPA CLI). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("etl", "ETL trace file to export from.", true)
            .string(
                "profile",
                "Optional WPA profile (.wpaProfile) selecting tables.",
                false,
            )
            .string(
                "outputDir",
                "Directory to write CSVs (within an allowed root).",
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let etl = ctx.resolve_path(a.str("etl")?)?;
        let mut wpa_args = vec!["-i".to_string(), etl.display().to_string()];
        if let Some(profile) = a.opt_str("profile")? {
            let p = ctx.resolve_path(profile)?;
            wpa_args.push("-profile".into());
            wpa_args.push(p.display().to_string());
        }
        if let Some(dir) = a.opt_str("outputDir")? {
            let d = ctx.resolve_path(dir)?;
            wpa_args.push("-outputfolder".into());
            wpa_args.push(d.display().to_string());
        }
        run_cmd(ctx, "wpaexporter", wpa_args, "wpaexporter", Some(300)).await
    }
}

/// GPUView ETW logging via its log.cmd helper.
struct GpuView;

#[async_trait]
impl Tool for GpuView {
    fn name(&self) -> &str {
        "benchmark.gpuview"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Drive GPUView's log helper (log.cmd) to start/stop a GPU ETW capture. \
         Provide the full path to log.cmd via 'logCmd' (must be allowlisted). Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("logCmd", "Path to GPUView's log.cmd.", true)
            .enumerated("action", "start or stop.", &["start", "stop"], true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let log_cmd = ctx.resolve_path(a.str("logCmd")?)?;
        let action = a.str("action")?;
        let arg = if action == "stop" { "stop" } else { "" };
        let mut cmd_args = Vec::new();
        if !arg.is_empty() {
            cmd_args.push(arg.to_string());
        }
        run_cmd(
            ctx,
            &log_cmd.display().to_string(),
            cmd_args,
            "gpuview log.cmd",
            Some(120),
        )
        .await
    }
}

/// LatencyMon CLI wrapper.
struct LatencyMon;

#[async_trait]
impl Tool for LatencyMon {
    fn name(&self) -> &str {
        "benchmark.latencymon"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run LatencyMon in CLI mode for DPC/ISR latency measurement. \
         Provide the LatencyMon.exe path via 'exe' (must be allowlisted) and CLI args. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("exe", "Path to LatencyMon.exe.", true)
            .string_array("args", "CLI arguments (e.g. ['/CLI','/RTIME:60']).", false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        ensure_windows(self.name())?;
        let a = Args::new(&args)?;
        let exe = ctx.resolve_path(a.str("exe")?)?;
        let lm_args = a.opt_str_array("args")?;
        run_cmd(
            ctx,
            &exe.display().to_string(),
            lm_args,
            "LatencyMon",
            a.opt_u64("timeoutSecs")?,
        )
        .await
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
            allowed_commands: vec!["ffmpeg".into(), "PresentMon".into()],
            max_output_bytes: 1 << 20,
            default_timeout_secs: 10,
            max_runtime_secs: 60,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("benchmark.system", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Default::default()),
            request_id: "r".into(),
        }
    }

    #[tokio::test]
    async fn system_sample_works_cross_platform() {
        let res = SystemSample
            .call(&ctx(), json!({"intervalMs": 150}))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn presentmon_gated_off_windows() {
        if cfg!(windows) {
            return;
        }
        let err = PresentMon
            .call(
                &ctx(),
                json!({"processName": "x.exe", "outputCsv": "/tmp/o.csv"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PlatformUnsupported(_)));
    }
}
