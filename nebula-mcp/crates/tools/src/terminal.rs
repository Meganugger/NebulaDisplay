//! Terminal tools: one-shot command execution and persistent interactive
//! sessions. All executed programs must be on the command allowlist.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::{exec_result, json_value_result};
use crate::common::session::SessionManager;
use crate::common::{Args, ObjectSchema};
use crate::ToolServices;

const CATEGORY: &str = "terminal";

/// Build terminal tools, sharing the session manager.
pub fn tools(services: &ToolServices) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RunCommand),
        Arc::new(SessionOpen {
            sessions: services.sessions.clone(),
        }),
        Arc::new(SessionWrite {
            sessions: services.sessions.clone(),
        }),
        Arc::new(SessionRead {
            sessions: services.sessions.clone(),
        }),
        Arc::new(SessionList {
            sessions: services.sessions.clone(),
        }),
        Arc::new(SessionClose {
            sessions: services.sessions.clone(),
        }),
    ]
}

/// Run a single command to completion.
struct RunCommand;

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &str {
        "terminal.run"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run an allowlisted command to completion, capturing stdout/stderr with a timeout. \
         The program (args[0]) must be permitted by policy; arguments are passed verbatim (no shell)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("program", "Executable name or path (must be allowlisted).", true)
            .string_array("args", "Arguments passed verbatim (not shell-parsed).", false)
            .string("cwd", "Working directory (default: workspace root).", false)
            .prop(
                "env",
                json!({"type": "object", "description": "Environment variables to set.", "additionalProperties": {"type": "string"}}),
                false,
            )
            .boolean("clearEnv", "Start from an empty environment.", false)
            .string("stdin", "Optional stdin text.", false)
            .integer("timeoutSecs", "Timeout in seconds (clamped to policy max).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            open_world_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let program = a.str("program")?.to_string();
        let cmd_args = a.opt_str_array("args")?;
        let cwd = match a.opt_str("cwd")? {
            Some(c) => ctx.resolve_path(c)?,
            None => ctx.working_dir.clone(),
        };
        let env = a.opt_string_map("env")?;
        let clear_env = a.bool_or("clearEnv", false)?;
        let timeout = a.opt_u64("timeoutSecs")?;

        let mut spec = CommandSpec::new(&program, cwd, ctx)
            .args(cmd_args)
            .envs(env);
        spec.clear_env = clear_env;
        if let Some(s) = a.opt_str("stdin")? {
            spec = spec.stdin_bytes(s.as_bytes().to_vec());
        }
        let display = format!("{program} {}", spec.args.join(" "));
        let result = run_checked(ctx, spec, timeout).await?;
        Ok(exec_result(&display, &result))
    }
}

/// Open an interactive session.
struct SessionOpen {
    sessions: Arc<SessionManager>,
}

#[async_trait]
impl Tool for SessionOpen {
    fn name(&self) -> &str {
        "terminal.session_open"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Open a persistent interactive process (e.g. a shell). Returns a session id to write to and read from across calls."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "program",
                "Executable to launch (must be allowlisted).",
                true,
            )
            .string_array("args", "Arguments.", false)
            .string("cwd", "Working directory.", false)
            .prop(
                "env",
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let program = a.str("program")?;
        ctx.policy.check_command(program)?;
        let cmd_args = a.opt_str_array("args")?;
        let cwd = match a.opt_str("cwd")? {
            Some(c) => ctx.resolve_path(c)?,
            None => ctx.working_dir.clone(),
        };
        let env = a.opt_string_map("env")?;
        let id =
            self.sessions
                .open(program, &cmd_args, cwd, &env, ctx.policy.max_output_bytes())?;
        Ok(json_value_result(json!({ "sessionId": id })))
    }
}

/// Write to a session's stdin.
struct SessionWrite {
    sessions: Arc<SessionManager>,
}

#[async_trait]
impl Tool for SessionWrite {
    fn name(&self) -> &str {
        "terminal.session_write"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Write text to an interactive session's stdin. Include a trailing newline to submit a command."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("sessionId", "Session id from session_open.", true)
            .string("data", "Text to write to stdin.", true)
            .build()
    }
    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let id = a.str("sessionId")?;
        let data = a.str("data")?;
        self.sessions.write(id, data.as_bytes()).await?;
        Ok(json_value_result(json!({ "written": data.len() })))
    }
}

/// Read buffered output from a session.
struct SessionRead {
    sessions: Arc<SessionManager>,
}

#[async_trait]
impl Tool for SessionRead {
    fn name(&self) -> &str {
        "terminal.session_read"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Read and clear buffered output from an interactive session, optionally waiting up to waitMs for new output."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("sessionId", "Session id.", true)
            .integer(
                "waitMs",
                "Milliseconds to wait for new output (default 0).",
                false,
            )
            .build()
    }
    async fn call(&self, _ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let id = a.str("sessionId")?;
        let wait = a.u64_or("waitMs", 0)?;
        let (out, dropped) = self.sessions.read(id, wait).await?;
        Ok(json_value_result(json!({
            "sessionId": id,
            "output": out,
            "droppedBytes": dropped,
        })))
    }
}

/// List active sessions.
struct SessionList {
    sessions: Arc<SessionManager>,
}

#[async_trait]
impl Tool for SessionList {
    fn name(&self) -> &str {
        "terminal.session_list"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List active interactive sessions."
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
        Ok(json_value_result(
            json!({ "sessions": self.sessions.list() }),
        ))
    }
}

/// Close a session.
struct SessionClose {
    sessions: Arc<SessionManager>,
}

#[async_trait]
impl Tool for SessionClose {
    fn name(&self) -> &str {
        "terminal.session_close"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Terminate an interactive session and free its resources."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("sessionId", "Session id.", true)
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
        let id = a.str("sessionId")?;
        self.sessions.close(id).await?;
        Ok(json_value_result(json!({ "closed": id })))
    }
}
