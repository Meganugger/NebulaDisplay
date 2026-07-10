//! Browser automation via Playwright.
//!
//! Rather than binding a browser protocol directly, these tools generate a
//! self-contained Playwright (Node.js) script and execute it with `node` (which
//! must be on the command allowlist). This supports Chromium, Firefox and
//! WebKit and yields screenshots, PDFs, console logs, network activity and
//! Playwright traces. Configuration is passed to the script via a temp JSON
//! file (never string-interpolated) to avoid script injection.
//!
//! Node and Playwright must be installed on the host; when they are not, the
//! tools return a clear `tool not found` / execution error.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::{CallToolResult, Content};
use serde_json::{json, Value};

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "browser";

/// The Playwright driver script (reads a JSON config path from argv[2]).
const DRIVER_SCRIPT: &str = r#"
const fs = require('fs');
const cfg = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
(async () => {
  let pw;
  try { pw = require('playwright'); }
  catch (e) { process.stderr.write('playwright is not installed: ' + e.message); process.exit(3); }
  const btype = pw[cfg.browser] || pw.chromium;
  const browser = await btype.launch({ headless: true });
  const context = await browser.newContext();
  if (cfg.tracePath) await context.tracing.start({ screenshots: true, snapshots: true });
  const page = await context.newPage();
  const consoleMsgs = [];
  const requests = [];
  if (cfg.collectConsole) page.on('console', m => consoleMsgs.push({ type: m.type(), text: m.text() }));
  if (cfg.collectNetwork) page.on('requestfinished', async r => {
    let status = null;
    try { const resp = await r.response(); status = resp ? resp.status() : null; } catch (_) {}
    requests.push({ url: r.url(), method: r.method(), status });
  });
  const result = {};
  try {
    const resp = await page.goto(cfg.url, { waitUntil: cfg.waitUntil || 'load', timeout: cfg.timeoutMs || 30000 });
    if (cfg.waitMs) await page.waitForTimeout(cfg.waitMs);
    result.status = resp ? resp.status() : null;
    result.title = await page.title();
    result.finalUrl = page.url();
    if (cfg.screenshotPath) { await page.screenshot({ path: cfg.screenshotPath, fullPage: !!cfg.fullPage }); result.screenshot = cfg.screenshotPath; }
    if (cfg.pdfPath && cfg.browser === 'chromium') { await page.pdf({ path: cfg.pdfPath }); result.pdf = cfg.pdfPath; }
    if (cfg.tracePath) { await context.tracing.stop({ path: cfg.tracePath }); result.trace = cfg.tracePath; }
    result.console = consoleMsgs;
    result.network = requests;
  } finally {
    await browser.close();
  }
  process.stdout.write(JSON.stringify(result));
})().catch(e => { process.stderr.write(String((e && e.stack) || e)); process.exit(1); });
"#;

/// Build browser tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(BrowserCapture), Arc::new(BrowserScreenshot)]
}

async fn run_driver(ctx: &ToolContext, cfg: Value, timeout: Option<u64>) -> Result<CallToolResult> {
    ctx.policy.ensure_network_allowed()?;

    // Write the driver + config to temp files.
    let dir = std::env::temp_dir();
    let script_path = dir.join(format!("nebula-pw-{}.cjs", uuid::Uuid::new_v4()));
    let config_path = dir.join(format!("nebula-pw-{}.json", uuid::Uuid::new_v4()));
    tokio::fs::write(&script_path, DRIVER_SCRIPT)
        .await
        .map_err(|e| ToolError::Io(format!("writing driver script: {e}")))?;
    tokio::fs::write(&config_path, serde_json::to_vec(&cfg)?)
        .await
        .map_err(|e| ToolError::Io(format!("writing driver config: {e}")))?;

    let spec = CommandSpec::new("node", ctx.working_dir.clone(), ctx).args(vec![
        script_path.display().to_string(),
        config_path.display().to_string(),
    ]);
    let result = run_checked(ctx, spec, timeout).await;

    // Best-effort cleanup of temp files.
    let _ = tokio::fs::remove_file(&script_path).await;
    let _ = tokio::fs::remove_file(&config_path).await;

    let result = result?;
    if result.success() {
        // The script prints JSON to stdout.
        let parsed: Value = serde_json::from_str(result.stdout.trim())
            .unwrap_or_else(|_| json!({ "raw": result.stdout }));
        let text = serde_json::to_string_pretty(&parsed).unwrap_or(result.stdout.clone());
        Ok(CallToolResult {
            content: vec![Content::text(text)],
            is_error: Some(false),
        })
    } else {
        Ok(CallToolResult::error_text(format!(
            "browser automation failed (exit {}): {}",
            result
                .code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            result.stderr.trim()
        )))
    }
}

/// Full-capability capture: navigate + optional screenshot/pdf/trace + logs.
struct BrowserCapture;

#[async_trait]
impl Tool for BrowserCapture {
    fn name(&self) -> &str {
        "browser.capture"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Drive Playwright to navigate a URL and optionally capture a screenshot, PDF (Chromium), \
         Playwright trace, console logs and network activity. Requires Node.js + Playwright."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "URL to open.", true)
            .enumerated(
                "browser",
                "Browser engine.",
                &["chromium", "firefox", "webkit"],
                false,
            )
            .string(
                "screenshotPath",
                "Screenshot output path (within an allowed root).",
                false,
            )
            .string(
                "pdfPath",
                "PDF output path (Chromium only, within an allowed root).",
                false,
            )
            .string(
                "tracePath",
                "Playwright trace zip output path (within an allowed root).",
                false,
            )
            .boolean("collectConsole", "Collect console messages.", true)
            .boolean("collectNetwork", "Collect finished network requests.", true)
            .boolean(
                "fullPage",
                "Capture the full scrollable page for screenshots.",
                false,
            )
            .enumerated(
                "waitUntil",
                "Navigation wait condition.",
                &["load", "domcontentloaded", "networkidle", "commit"],
                false,
            )
            .integer("waitMs", "Extra wait after navigation in ms.", false)
            .integer(
                "timeoutMs",
                "Navigation timeout in ms (default 30000).",
                false,
            )
            .integer(
                "timeoutSecs",
                "Overall process timeout override in seconds.",
                false,
            )
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
        let url = a.str("url")?.to_string();
        let browser = a.str_or("browser", "chromium")?.to_string();
        let mut cfg = json!({
            "url": url,
            "browser": browser,
            "collectConsole": a.bool_or("collectConsole", true)?,
            "collectNetwork": a.bool_or("collectNetwork", true)?,
            "fullPage": a.bool_or("fullPage", false)?,
            "waitUntil": a.str_or("waitUntil", "load")?,
            "waitMs": a.u64_or("waitMs", 0)?,
            "timeoutMs": a.u64_or("timeoutMs", 30000)?,
        });
        // Resolve output paths against policy before handing them to the script.
        if let Some(p) = a.opt_str("screenshotPath")? {
            cfg["screenshotPath"] = json!(ctx.resolve_path(p)?.display().to_string());
        }
        if let Some(p) = a.opt_str("pdfPath")? {
            cfg["pdfPath"] = json!(ctx.resolve_path(p)?.display().to_string());
        }
        if let Some(p) = a.opt_str("tracePath")? {
            cfg["tracePath"] = json!(ctx.resolve_path(p)?.display().to_string());
        }
        run_driver(ctx, cfg, a.opt_u64("timeoutSecs")?).await
    }
}

/// Convenience screenshot tool.
struct BrowserScreenshot;

#[async_trait]
impl Tool for BrowserScreenshot {
    fn name(&self) -> &str {
        "browser.screenshot"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Capture a screenshot of a URL to a file with Playwright. Requires Node.js + Playwright."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "URL to open.", true)
            .string(
                "outputPath",
                "Screenshot output path (within an allowed root).",
                true,
            )
            .enumerated(
                "browser",
                "Browser engine.",
                &["chromium", "firefox", "webkit"],
                false,
            )
            .boolean("fullPage", "Capture the full scrollable page.", false)
            .integer("timeoutSecs", "Process timeout override.", false)
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
        let url = a.str("url")?.to_string();
        let out = ctx.resolve_path(a.str("outputPath")?)?;
        let cfg = json!({
            "url": url,
            "browser": a.str_or("browser", "chromium")?,
            "screenshotPath": out.display().to_string(),
            "fullPage": a.bool_or("fullPage", false)?,
            "collectConsole": false,
            "collectNetwork": false,
            "waitUntil": "load",
            "timeoutMs": 30000,
        });
        run_driver(ctx, cfg, a.opt_u64("timeoutSecs")?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::Metrics;
    use tokio_util::sync::CancellationToken;

    fn ctx(network: bool) -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec!["/**".into()],
            allowed_commands: vec!["node".into()],
            allow_network: network,
            default_timeout_secs: 10,
            max_runtime_secs: 60,
            max_output_bytes: 1 << 20,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("browser.capture", &base, None).unwrap();
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
    async fn network_gate_blocks_capture() {
        let err = BrowserCapture
            .call(&ctx(false), json!({"url": "https://example.com"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn missing_node_is_reported() {
        // With node allowlisted but (likely) absent, we get a ToolNotFound.
        let c = ctx(true);
        if crate::common::exec::program_available("node") {
            return; // node present; skip negative assertion
        }
        let err = BrowserScreenshot
            .call(
                &c,
                json!({"url": "https://example.com", "outputPath": "/tmp/s.png"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ToolNotFound(_)));
    }
}
