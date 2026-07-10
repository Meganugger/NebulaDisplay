//! Async process execution engine.
//!
//! A single, hardened implementation used by every tool that shells out
//! (terminal, git, github, powershell, driver, benchmark, network, ...). It
//! provides:
//!
//! * bounded output capture (never OOM on a chatty process; overflow is counted
//!   and the child is still drained so it does not deadlock on a full pipe),
//! * hard timeout and cooperative cancellation, both of which kill the child
//!   (its whole process group on Unix),
//! * optional stdin, environment control and working directory,
//! * resolution errors that name the missing executable.
//!
//! Policy (command allowlist, cwd containment) is enforced by callers via
//! [`run_checked`], which is the entry point tools should prefer.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use nebula_mcp_core::{ToolContext, ToolError};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Description of a process to run.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Arguments (not shell-parsed; passed verbatim).
    pub args: Vec<String>,
    /// Working directory. Must already be validated by policy.
    pub cwd: PathBuf,
    /// Additional/overriding environment variables.
    pub env: Vec<(String, String)>,
    /// When `true`, start from an empty environment (only `env` is set).
    pub clear_env: bool,
    /// Optional stdin payload.
    pub stdin: Option<Vec<u8>>,
    /// Per-stream capture limit in bytes.
    pub max_output_bytes: usize,
}

impl CommandSpec {
    /// Start a spec for `program` in `cwd` with the context's output limit.
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>, ctx: &ToolContext) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            clear_env: false,
            stdin: None,
            max_output_bytes: ctx.policy.max_output_bytes(),
        }
    }

    /// Append a single argument.
    #[must_use]
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    /// Append multiple arguments.
    #[must_use]
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    /// Set environment variables.
    #[must_use]
    pub fn envs(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// Provide stdin bytes.
    #[must_use]
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }
}

/// Result of a completed process.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Exit code, or `None` if terminated by signal.
    pub code: Option<i32>,
    /// Captured stdout (UTF-8 lossy, possibly truncated).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy, possibly truncated).
    pub stderr: String,
    /// Whether stdout was truncated at the limit.
    pub stdout_truncated: bool,
    /// Whether stderr was truncated at the limit.
    pub stderr_truncated: bool,
    /// Wall-clock duration.
    pub duration: Duration,
}

impl ExecResult {
    /// Whether the process exited successfully (code 0).
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Total captured output bytes (for metrics).
    #[must_use]
    pub fn output_bytes(&self) -> u64 {
        (self.stdout.len() + self.stderr.len()) as u64
    }

    /// Turn a non-zero exit into a [`ToolError::Execution`], leaving success
    /// untouched. The error text includes trimmed stderr (or stdout).
    pub fn error_on_failure(self, what: &str) -> Result<ExecResult, ToolError> {
        if self.success() {
            Ok(self)
        } else {
            let detail = if !self.stderr.trim().is_empty() {
                self.stderr.trim()
            } else {
                self.stdout.trim()
            };
            Err(ToolError::Execution(format!(
                "{what} exited with {}: {}",
                self.code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                truncate_for_msg(detail, 2000)
            )))
        }
    }
}

fn truncate_for_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}… [{} more bytes]", &s[..max], s.len() - max)
    }
}

/// Resolve, policy-check and run a command spec under the context's timeout and
/// cancellation.
///
/// This is the entry point tools should use: it verifies the program is on the
/// command allowlist before doing anything else.
pub async fn run_checked(
    ctx: &ToolContext,
    spec: CommandSpec,
    requested_timeout_secs: Option<u64>,
) -> Result<ExecResult, ToolError> {
    ctx.policy.check_command(&spec.program)?;
    let timeout = ctx.timeout(requested_timeout_secs);
    run_raw(ctx, spec, timeout).await
}

/// Run without the command allowlist check. Only for internal callers that have
/// already validated the program by other means (never exposed to the model).
pub async fn run_raw(
    ctx: &ToolContext,
    spec: CommandSpec,
    timeout: Duration,
) -> Result<ExecResult, ToolError> {
    // Resolve the executable for a precise "not found" error.
    resolve_program(&spec.program)?;

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if spec.clear_env {
        cmd.env_clear();
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        // New process group so we can kill the whole tree on cancel/timeout.
        cmd.process_group(0);
    }

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Io(format!("spawning '{}': {e}", spec.program)))?;

    // Feed stdin if provided.
    if let Some(data) = spec.stdin.clone() {
        if let Some(mut sink) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            // Ignore broken-pipe if the child exits early.
            let _ = sink.write_all(&data).await;
            let _ = sink.shutdown().await;
        }
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let limit = spec.max_output_bytes;
    let out_task = tokio::spawn(async move { read_capped(stdout, limit).await });
    let err_task = tokio::spawn(async move { read_capped(stderr, limit).await });

    let status = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => {
            terminate(&mut child).await;
            let _ = out_task.await;
            let _ = err_task.await;
            return Err(ToolError::Cancelled);
        }
        r = tokio::time::timeout(timeout, child.wait()) => match r {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(ToolError::Io(format!("waiting for '{}': {e}", spec.program)));
            }
            Err(_) => {
                terminate(&mut child).await;
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(ToolError::Timeout(timeout));
            }
        }
    };

    let (stdout_bytes, stdout_truncated) = out_task
        .await
        .map_err(|e| ToolError::Internal(format!("stdout reader panicked: {e}")))??;
    let (stderr_bytes, stderr_truncated) = err_task
        .await
        .map_err(|e| ToolError::Internal(format!("stderr reader panicked: {e}")))??;

    Ok(ExecResult {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        stdout_truncated,
        stderr_truncated,
        duration: start.elapsed(),
    })
}

/// Read a child stream, keeping at most `limit` bytes but fully draining the
/// pipe so the child never blocks on a full buffer.
async fn read_capped<R>(reader: Option<R>, limit: usize) -> Result<(Vec<u8>, bool), ToolError>
where
    R: AsyncReadExt + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut kept = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut scratch = [0u8; 16 * 1024];
    loop {
        let n = reader
            .read(&mut scratch)
            .await
            .map_err(|e| ToolError::Io(format!("reading process output: {e}")))?;
        if n == 0 {
            break;
        }
        if kept.len() < limit {
            let take = (limit - kept.len()).min(n);
            kept.extend_from_slice(&scratch[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((kept, truncated))
}

/// Terminate a child and its group best-effort.
async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Negative pid targets the whole process group created above.
            // SIGTERM first, then rely on kill_on_drop / start_kill for SIGKILL.
            unsafe {
                libc_kill(-(pid as i32), 15);
            }
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let _ = kill(pid, sig);
}

/// Resolve a program on PATH (or as an absolute/relative path), returning a
/// [`ToolError::ToolNotFound`] with the name when it cannot be located.
pub fn resolve_program(program: &str) -> Result<PathBuf, ToolError> {
    let p = Path::new(program);
    if p.is_absolute() || program.contains('/') || program.contains('\\') {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(ToolError::ToolNotFound(program.to_string()));
    }
    which::which(program).map_err(|_| ToolError::ToolNotFound(program.to_string()))
}

/// Whether a program is available (for capability probing / diagnostics).
#[must_use]
pub fn program_available(program: &str) -> bool {
    resolve_program(program).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::{Metrics, ToolContext};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx_with(cmds: &[&str], out_limit: usize, timeout: u64) -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec!["/**".into()],
            allowed_commands: cmds.iter().map(|s| s.to_string()).collect(),
            default_timeout_secs: timeout,
            max_runtime_secs: 60,
            max_output_bytes: out_limit,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("t", &base, None).unwrap();
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
    async fn runs_and_captures_stdout() {
        let ctx = ctx_with(&["echo"], 4096, 10);
        let spec = CommandSpec::new("echo", std::env::temp_dir(), &ctx).arg("hello world");
        let r = run_checked(&ctx, spec, None).await.unwrap();
        assert!(r.success());
        assert!(r.stdout.contains("hello world"));
    }

    #[tokio::test]
    async fn denies_unlisted_command() {
        let ctx = ctx_with(&["echo"], 4096, 10);
        let spec = CommandSpec::new("ls", std::env::temp_dir(), &ctx);
        let err = run_checked(&ctx, spec, None).await.unwrap_err();
        assert!(matches!(err, ToolError::CommandNotAllowed(_)));
    }

    #[tokio::test]
    async fn reports_missing_binary() {
        let ctx = ctx_with(&["definitely-not-a-real-binary-xyz"], 4096, 10);
        let spec = CommandSpec::new(
            "definitely-not-a-real-binary-xyz",
            std::env::temp_dir(),
            &ctx,
        );
        let err = run_checked(&ctx, spec, None).await.unwrap_err();
        assert!(matches!(err, ToolError::ToolNotFound(_)));
    }

    #[tokio::test]
    async fn enforces_timeout() {
        let ctx = ctx_with(&["sleep"], 4096, 1);
        let spec = CommandSpec::new("sleep", std::env::temp_dir(), &ctx).arg("30");
        let err = run_checked(&ctx, spec, Some(1)).await.unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn truncates_large_output() {
        let ctx = ctx_with(&["sh"], 16, 10);
        let spec = CommandSpec::new("sh", std::env::temp_dir(), &ctx)
            .arg("-c")
            .arg("printf 'x%.0s' $(seq 1 1000)");
        let r = run_checked(&ctx, spec, None).await.unwrap();
        assert!(r.stdout_truncated);
        assert_eq!(r.stdout.len(), 16);
    }

    #[tokio::test]
    async fn feeds_stdin() {
        let ctx = ctx_with(&["cat"], 4096, 10);
        let spec = CommandSpec::new("cat", std::env::temp_dir(), &ctx)
            .stdin_bytes(b"piped-input".to_vec());
        let r = run_checked(&ctx, spec, None).await.unwrap();
        assert!(r.stdout.contains("piped-input"));
    }

    #[tokio::test]
    async fn cancellation_stops_process() {
        let ctx = ctx_with(&["sleep"], 4096, 30);
        let spec = CommandSpec::new("sleep", std::env::temp_dir(), &ctx).arg("30");
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let err = run_checked(&ctx, spec, Some(30)).await.unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }
}
