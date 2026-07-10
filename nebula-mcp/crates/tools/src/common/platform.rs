//! Platform gating and a shared PowerShell/command helper for Windows tools.

use nebula_mcp_core::{ToolContext, ToolError};

use crate::common::exec::{run_checked, CommandSpec, ExecResult};

/// Return [`ToolError::PlatformUnsupported`] unless running on Windows.
///
/// Windows-only tools call this first so that on non-Windows hosts they fail
/// fast with a clear, structured error instead of an opaque "binary not found".
pub fn ensure_windows(tool: &str) -> Result<(), ToolError> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(ToolError::PlatformUnsupported(format!(
            "{tool} requires a Windows host; this server is running on {}",
            std::env::consts::OS
        )))
    }
}

/// The PowerShell executable to invoke. `powershell.exe` (Windows PowerShell)
/// is the broadest default; policy decides whether it is permitted, and callers
/// may allowlist `pwsh` instead.
pub const POWERSHELL: &str = "powershell";

/// Run a PowerShell command non-interactively and return the raw result.
///
/// The command allowlist must permit the chosen shell. `script` is passed via
/// `-Command`; callers that need structured output should have the script emit
/// JSON (e.g. `... | ConvertTo-Json`).
pub async fn run_powershell(
    ctx: &ToolContext,
    shell: &str,
    script: &str,
    requested_timeout_secs: Option<u64>,
) -> Result<ExecResult, ToolError> {
    let spec = CommandSpec::new(shell, ctx.working_dir.clone(), ctx).args(vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ]);
    run_checked(ctx, spec, requested_timeout_secs).await
}
