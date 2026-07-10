//! Scheduler tools: run allowlisted commands after a delay or on an interval,
//! and poll their captured results. Cross-platform. Scheduled commands run
//! under the same permission policy as the scheduling call.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::output::json_value_result;
use crate::common::scheduler::{JobEnv, SchedulerManager};
use crate::common::{Args, ObjectSchema};
use crate::ToolServices;

const CATEGORY: &str = "scheduler";

/// Build scheduler tools.
pub fn tools(services: &ToolServices) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ScheduleAfter {
            mgr: services.scheduler.clone(),
        }),
        Arc::new(ScheduleEvery {
            mgr: services.scheduler.clone(),
        }),
        Arc::new(ScheduleList {
            mgr: services.scheduler.clone(),
        }),
        Arc::new(ScheduleResults {
            mgr: services.scheduler.clone(),
        }),
        Arc::new(ScheduleCancel {
            mgr: services.scheduler.clone(),
        }),
    ]
}

fn schedule_schema(interval: bool) -> Value {
    let s = ObjectSchema::new()
        .string("program", "Executable to run (must be allowlisted).", true)
        .string_array("args", "Arguments passed verbatim.", false);
    if interval {
        s.integer("intervalSecs", "Seconds between runs.", true)
            .integer(
                "delaySecs",
                "Initial delay before the first run (default 0).",
                false,
            )
            .build()
    } else {
        s.integer("delaySecs", "Delay before running, in seconds.", true)
            .build()
    }
}

/// Run a command once after a delay.
struct ScheduleAfter {
    mgr: Arc<SchedulerManager>,
}

#[async_trait]
impl Tool for ScheduleAfter {
    fn name(&self) -> &str {
        "scheduler.after"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Schedule an allowlisted command to run once after a delay. Returns a job id; poll with scheduler.results."
    }
    fn input_schema(&self) -> Value {
        schedule_schema(false)
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let program = a.str("program")?.to_string();
        ctx.policy.check_command(&program)?;
        let cmd_args = a.opt_str_array("args")?;
        let delay = a
            .opt_u64("delaySecs")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'delaySecs'".into()))?;
        let id = self
            .mgr
            .schedule(JobEnv::from_context(ctx), program, cmd_args, delay, None);
        Ok(json_value_result(json!({ "jobId": id })))
    }
}

/// Run a command repeatedly on an interval.
struct ScheduleEvery {
    mgr: Arc<SchedulerManager>,
}

#[async_trait]
impl Tool for ScheduleEvery {
    fn name(&self) -> &str {
        "scheduler.every"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Schedule an allowlisted command to run repeatedly on an interval. Returns a job id; cancel with scheduler.cancel."
    }
    fn input_schema(&self) -> Value {
        schedule_schema(true)
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let program = a.str("program")?.to_string();
        ctx.policy.check_command(&program)?;
        let cmd_args = a.opt_str_array("args")?;
        let interval = a
            .opt_u64("intervalSecs")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'intervalSecs'".into()))?;
        let delay = a.u64_or("delaySecs", 0)?;
        let id = self.mgr.schedule(
            JobEnv::from_context(ctx),
            program,
            cmd_args,
            delay,
            Some(interval.max(1)),
        );
        Ok(json_value_result(json!({ "jobId": id })))
    }
}

/// List scheduled jobs.
struct ScheduleList {
    mgr: Arc<SchedulerManager>,
}

#[async_trait]
impl Tool for ScheduleList {
    fn name(&self) -> &str {
        "scheduler.list"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List scheduled jobs and their run counts."
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
        Ok(json_value_result(json!({ "jobs": self.mgr.list() })))
    }
}

/// Fetch a job's captured results.
struct ScheduleResults {
    mgr: Arc<SchedulerManager>,
}

#[async_trait]
impl Tool for ScheduleResults {
    fn name(&self) -> &str {
        "scheduler.results"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Fetch the captured results (stdout/stderr/exit) of a scheduled job's recent runs."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("jobId", "Job id from scheduler.after/every.", true)
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
        let id = a.str("jobId")?;
        match self.mgr.results(id) {
            Some((info, results)) => Ok(json_value_result(json!({
                "job": info,
                "results": results,
            }))),
            None => Err(ToolError::InvalidArguments(format!("no such job '{id}'"))),
        }
    }
}

/// Cancel a scheduled job.
struct ScheduleCancel {
    mgr: Arc<SchedulerManager>,
}

#[async_trait]
impl Tool for ScheduleCancel {
    fn name(&self) -> &str {
        "scheduler.cancel"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Cancel a scheduled job (aborts any in-progress run)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("jobId", "Job id to cancel.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            destructive_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let id = a.str("jobId")?;
        let cancelled = self.mgr.cancel(id);
        Ok(json_value_result(
            json!({ "jobId": id, "cancelled": cancelled }),
        ))
    }
}
