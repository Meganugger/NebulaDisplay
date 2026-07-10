//! Process tools: enumerate, inspect and terminate OS processes.
//!
//! Backed by the cross-platform `sysinfo` crate. Enumeration and inspection are
//! read-only; termination is destructive and gated by policy.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde::Serialize;
use serde_json::{json, Value};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::common::output::json_value_result;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "process";

/// Build process tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ProcessList),
        Arc::new(ProcessInfo),
        Arc::new(ProcessKill),
    ]
}

#[derive(Serialize)]
struct ProcView {
    pid: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<u32>,
    cpu_usage: f32,
    memory_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exe: Option<String>,
    run_time_secs: u64,
}

fn view(proc: &sysinfo::Process) -> ProcView {
    ProcView {
        pid: proc.pid().as_u32(),
        name: proc.name().to_string_lossy().into_owned(),
        parent: proc.parent().map(|p| p.as_u32()),
        cpu_usage: proc.cpu_usage(),
        memory_bytes: proc.memory(),
        exe: proc.exe().map(|p| p.display().to_string()),
        run_time_secs: proc.run_time(),
    }
}

/// List processes, optionally filtered by a name substring.
struct ProcessList;

#[async_trait]
impl Tool for ProcessList {
    fn name(&self) -> &str {
        "process.list"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List running processes (pid, name, cpu, memory), optionally filtered by a name substring."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "nameContains",
                "Case-insensitive name substring filter.",
                false,
            )
            .integer("limit", "Maximum processes to return (default 200).", false)
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
        let filter = a.opt_str("nameContains")?.map(|s| s.to_lowercase());
        let limit = a.u64_or("limit", 200)? as usize;
        let cancel = ctx.cancel.clone();

        let value = tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::everything(),
            );
            let mut procs: Vec<ProcView> = sys
                .processes()
                .values()
                .filter(|p| match &filter {
                    Some(f) => p.name().to_string_lossy().to_lowercase().contains(f),
                    None => true,
                })
                .map(view)
                .collect();
            procs.sort_by_key(|p| std::cmp::Reverse(p.memory_bytes));
            let total = procs.len();
            procs.truncate(limit);
            Ok(json!({
                "count": procs.len(),
                "total": total,
                "processes": procs,
            }))
        })
        .await
        .map_err(|e| ToolError::Internal(format!("process list join: {e}")))??;
        Ok(json_value_result(value))
    }
}

/// Inspect a single process.
struct ProcessInfo;

#[async_trait]
impl Tool for ProcessInfo {
    fn name(&self) -> &str {
        "process.info"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Return detailed information about a process by pid."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("pid", "Process id.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let pid = a
            .opt_u64("pid")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'pid'".into()))?;
        let value = tokio::task::spawn_blocking(move || {
            let mut sys = System::new();
            let p = Pid::from_u32(pid as u32);
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[p]),
                true,
                ProcessRefreshKind::everything(),
            );
            match sys.process(p) {
                Some(proc) => {
                    let mut v = serde_json::to_value(view(proc)).unwrap_or_default();
                    if let Value::Object(map) = &mut v {
                        let cmd: Vec<String> = proc
                            .cmd()
                            .iter()
                            .map(|s| s.to_string_lossy().into_owned())
                            .collect();
                        map.insert("cmd".into(), json!(cmd));
                        map.insert("status".into(), json!(proc.status().to_string()));
                    }
                    Ok(v)
                }
                None => Err(ToolError::Execution(format!("no process with pid {pid}"))),
            }
        })
        .await
        .map_err(|e| ToolError::Internal(format!("process info join: {e}")))??;
        Ok(json_value_result(value))
    }
}

/// Terminate a process.
struct ProcessKill;

#[async_trait]
impl Tool for ProcessKill {
    fn name(&self) -> &str {
        "process.kill"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Terminate a process by pid. Destructive; requires allow_destructive."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("pid", "Process id to terminate.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            destructive_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let pid = a
            .opt_u64("pid")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'pid'".into()))?;
        ctx.policy.ensure_destructive_allowed("process.kill")?;
        let value = tokio::task::spawn_blocking(move || {
            let mut sys = System::new();
            let p = Pid::from_u32(pid as u32);
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[p]),
                true,
                ProcessRefreshKind::new(),
            );
            match sys.process(p) {
                Some(proc) => {
                    let killed = proc.kill();
                    Ok(json!({ "pid": pid, "killed": killed }))
                }
                None => Err(ToolError::Execution(format!("no process with pid {pid}"))),
            }
        })
        .await
        .map_err(|e| ToolError::Internal(format!("process kill join: {e}")))??;
        Ok(json_value_result(value))
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
        let policy = EffectivePolicy::build("p", &SecurityConfig::default(), None).unwrap();
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
    async fn lists_current_process() {
        let res = ProcessList
            .call(&ctx(), json!({"limit": 500}))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn info_for_self() {
        let pid = std::process::id();
        let res = ProcessInfo.call(&ctx(), json!({"pid": pid})).await.unwrap();
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn kill_denied_without_destructive() {
        let err = ProcessKill
            .call(&ctx(), json!({"pid": 999999}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }
}
