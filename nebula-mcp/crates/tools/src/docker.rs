//! Docker tools: typed wrappers over the `docker` CLI (and `docker compose`).
//!
//! Cross-platform. The `docker` executable must be on the command allowlist.
//! Build/compose file paths and bind sources are policy-checked.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::exec_result;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "docker";
const DOCKER: &str = "docker";

/// Build docker tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DockerPs),
        Arc::new(DockerImages),
        Arc::new(DockerBuild),
        Arc::new(DockerRun),
        Arc::new(DockerStop),
        Arc::new(DockerRm),
        Arc::new(DockerLogs),
        Arc::new(DockerExec),
        Arc::new(DockerCompose),
    ]
}

async fn docker(
    ctx: &ToolContext,
    args: Vec<String>,
    label: &str,
    timeout: Option<u64>,
) -> Result<CallToolResult> {
    let spec = CommandSpec::new(DOCKER, ctx.working_dir.clone(), ctx).args(args);
    let result = run_checked(ctx, spec, timeout).await?;
    Ok(exec_result(label, &result))
}

fn ro() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    })
}

/// List containers.
struct DockerPs;

#[async_trait]
impl Tool for DockerPs {
    fn name(&self) -> &str {
        "docker.ps"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List Docker containers (JSON lines). Set all=true to include stopped containers."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .boolean("all", "Include stopped containers.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let mut v = vec!["ps".to_string(), "--format".into(), "{{json .}}".into()];
        if a.bool_or("all", false)? {
            v.push("--all".into());
        }
        docker(ctx, v, "docker ps", None).await
    }
}

/// List images.
struct DockerImages;

#[async_trait]
impl Tool for DockerImages {
    fn name(&self) -> &str {
        "docker.images"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List Docker images (JSON lines)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<CallToolResult> {
        docker(
            ctx,
            vec!["images".into(), "--format".into(), "{{json .}}".into()],
            "docker images",
            None,
        )
        .await
    }
}

/// Build an image.
struct DockerBuild;

#[async_trait]
impl Tool for DockerBuild {
    fn name(&self) -> &str {
        "docker.build"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Build a Docker image from a build context directory (within an allowed root)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("context", "Build context directory.", true)
            .string("tag", "Image tag (name:version).", true)
            .string(
                "dockerfile",
                "Dockerfile path relative to context (optional).",
                false,
            )
            .string_array("buildArgs", "Extra --build-arg values (KEY=VALUE).", false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let context = ctx.resolve_path(a.str("context")?)?;
        let tag = a.str("tag")?;
        let mut v = vec!["build".to_string(), "-t".into(), tag.into()];
        if let Some(df) = a.opt_str("dockerfile")? {
            v.push("-f".into());
            v.push(df.into());
        }
        for ba in a.opt_str_array("buildArgs")? {
            v.push("--build-arg".into());
            v.push(ba);
        }
        v.push(context.display().to_string());
        docker(ctx, v, "docker build", a.opt_u64("timeoutSecs")?).await
    }
}

/// Run a container.
struct DockerRun;

#[async_trait]
impl Tool for DockerRun {
    fn name(&self) -> &str {
        "docker.run"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run a container from an image. Supports detach, name, env, published ports and a command."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("image", "Image to run.", true)
            .boolean("detach", "Run detached (-d).", false)
            .boolean("rm", "Remove the container on exit (--rm).", false)
            .string("name", "Container name.", false)
            .string_array("env", "Environment variables (KEY=VALUE).", false)
            .string_array("publish", "Port mappings (host:container).", false)
            .string_array("command", "Command + args to run in the container.", false)
            .integer("timeoutSecs", "Timeout override.", false)
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
        let mut v = vec!["run".to_string()];
        if a.bool_or("detach", false)? {
            v.push("-d".into());
        }
        if a.bool_or("rm", false)? {
            v.push("--rm".into());
        }
        if let Some(name) = a.opt_str("name")? {
            v.push("--name".into());
            v.push(name.into());
        }
        for e in a.opt_str_array("env")? {
            v.push("-e".into());
            v.push(e);
        }
        for p in a.opt_str_array("publish")? {
            v.push("-p".into());
            v.push(p);
        }
        v.push(a.str("image")?.into());
        v.extend(a.opt_str_array("command")?);
        docker(ctx, v, "docker run", a.opt_u64("timeoutSecs")?).await
    }
}

/// Stop a container.
struct DockerStop;

#[async_trait]
impl Tool for DockerStop {
    fn name(&self) -> &str {
        "docker.stop"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Stop a running container by name or id."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("container", "Container name or id.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        docker(
            ctx,
            vec!["stop".into(), a.str("container")?.into()],
            "docker stop",
            None,
        )
        .await
    }
}

/// Remove a container.
struct DockerRm;

#[async_trait]
impl Tool for DockerRm {
    fn name(&self) -> &str {
        "docker.rm"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Remove a container (optionally forcing). Destructive."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("container", "Container name or id.", true)
            .boolean("force", "Force removal (-f).", false)
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
        ctx.policy.ensure_destructive_allowed("docker.rm")?;
        let mut v = vec!["rm".to_string()];
        if a.bool_or("force", false)? {
            v.push("-f".into());
        }
        v.push(a.str("container")?.into());
        docker(ctx, v, "docker rm", None).await
    }
}

/// Fetch container logs.
struct DockerLogs;

#[async_trait]
impl Tool for DockerLogs {
    fn name(&self) -> &str {
        "docker.logs"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Fetch a container's logs (last N lines)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("container", "Container name or id.", true)
            .integer("tail", "Number of trailing lines (default 200).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let tail = a.u64_or("tail", 200)?;
        docker(
            ctx,
            vec![
                "logs".into(),
                "--tail".into(),
                tail.to_string(),
                a.str("container")?.into(),
            ],
            "docker logs",
            None,
        )
        .await
    }
}

/// Execute a command in a running container.
struct DockerExec;

#[async_trait]
impl Tool for DockerExec {
    fn name(&self) -> &str {
        "docker.exec"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Execute a command inside a running container and capture its output."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("container", "Container name or id.", true)
            .string_array("command", "Command + args to execute.", true)
            .integer("timeoutSecs", "Timeout override.", false)
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
        let command = a.str_array("command")?;
        if command.is_empty() {
            return Err(ToolError::InvalidArguments(
                "command must not be empty".into(),
            ));
        }
        let mut v = vec!["exec".to_string(), a.str("container")?.into()];
        v.extend(command);
        docker(ctx, v, "docker exec", a.opt_u64("timeoutSecs")?).await
    }
}

/// Docker Compose control.
struct DockerCompose;

#[async_trait]
impl Tool for DockerCompose {
    fn name(&self) -> &str {
        "docker.compose"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run docker compose actions (up/down/ps/logs/build) against a compose file (within an allowed root)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("file", "Path to the compose file.", true)
            .enumerated(
                "action",
                "Compose action.",
                &["up", "down", "ps", "logs", "build"],
                true,
            )
            .boolean("detach", "For 'up', run detached (-d).", false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let file = ctx.resolve_path(a.str("file")?)?;
        let action = a.str("action")?;
        let mut v = vec![
            "compose".to_string(),
            "-f".into(),
            file.display().to_string(),
        ];
        match action {
            "up" => {
                v.push("up".into());
                if a.bool_or("detach", true)? {
                    v.push("-d".into());
                }
            }
            "down" => {
                ctx.policy
                    .ensure_destructive_allowed("docker.compose down")?;
                v.push("down".into());
            }
            "ps" => v.push("ps".into()),
            "logs" => {
                v.push("logs".into());
                v.push("--tail".into());
                v.push("200".into());
            }
            "build" => v.push("build".into()),
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown compose action '{other}'"
                )))
            }
        }
        docker(ctx, v, "docker compose", a.opt_u64("timeoutSecs")?).await
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

    fn ctx(destructive: bool) -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec!["/**".into()],
            allowed_commands: vec!["docker".into()],
            allow_destructive: destructive,
            default_timeout_secs: 10,
            max_runtime_secs: 30,
            max_output_bytes: 1 << 20,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("docker.ps", &base, None).unwrap();
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
    async fn rm_requires_destructive_policy() {
        let err = DockerRm
            .call(&ctx(false), json!({"container": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn exec_rejects_empty_command() {
        let err = DockerExec
            .call(&ctx(true), json!({"container": "x", "command": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
