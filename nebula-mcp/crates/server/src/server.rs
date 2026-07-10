//! MCP server: request dispatch, concurrency control, cancellation and metrics.
//!
//! The server reads JSON-RPC frames from an async reader and writes responses
//! to an async writer. Requests are dispatched concurrently (bounded by a
//! semaphore); `tools/call` runs under a per-request cancellation token so that
//! a `notifications/cancelled` message or shutdown aborts the in-flight tool.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use nebula_mcp_core::config::ConfigStore;
use nebula_mcp_core::security::EffectivePolicy;
use nebula_mcp_core::{Metrics, Outcome, ToolContext, ToolError, ToolRegistry};
use nebula_mcp_protocol::{
    error_codes, CallToolParams, CallToolResult, Content, ErrorObject, FrameReader, FrameWriter,
    Implementation, InitializeResult, ListToolsResult, Request, Response, ServerCapabilities,
    ToolsCapability, PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// The MCP server runtime.
pub struct Server {
    registry: Arc<ToolRegistry>,
    config: ConfigStore,
    metrics: Metrics,
    working_dir: PathBuf,
    semaphore: Arc<Semaphore>,
    inflight: Arc<DashMap<String, CancellationToken>>,
    root_cancel: CancellationToken,
    initialized: Arc<AtomicBool>,
}

impl Server {
    /// Construct a server.
    pub fn new(
        registry: ToolRegistry,
        config: ConfigStore,
        working_dir: PathBuf,
        root_cancel: CancellationToken,
    ) -> Self {
        let max_concurrent = config.snapshot().server.max_concurrent_calls.max(1);
        Self {
            registry: Arc::new(registry),
            config,
            metrics: Metrics::new(),
            working_dir,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            inflight: Arc::new(DashMap::new()),
            root_cancel,
            initialized: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Shared metrics registry (for diagnostics/telemetry).
    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    /// Serve requests until end-of-input or shutdown.
    pub async fn serve<R, W>(self: Arc<Self>, reader: R, writer: W) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut frames = FrameReader::new(reader);
        let writer = FrameWriter::new(writer);
        let mut tasks = tokio::task::JoinSet::new();

        loop {
            let next = tokio::select! {
                biased;
                () = self.root_cancel.cancelled() => {
                    tracing::info!("shutdown signal received; stopping read loop");
                    break;
                }
                frame = frames.next_frame() => frame,
            };

            let request = match next {
                Ok(Some(req)) => req,
                Ok(None) => {
                    tracing::info!("client closed the stream");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to decode a frame; sending parse error");
                    let resp = Response::error(
                        None,
                        ErrorObject::new(error_codes::PARSE_ERROR, e.to_string(), None),
                    );
                    let _ = writer.write_response(&resp).await;
                    continue;
                }
            };

            if request.is_notification() {
                self.handle_notification(&request);
                continue;
            }

            // Register a per-request cancellation token *synchronously* here,
            // before reading the next frame, so a `notifications/cancelled` that
            // immediately follows the request always finds the token (no race
            // with the spawned handler task).
            let request_key = request
                .id
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let cancel = self.root_cancel.child_token();
            self.inflight.insert(request_key.clone(), cancel.clone());

            // Dispatch requests concurrently so long tool calls don't block the
            // read loop (and can be cancelled mid-flight).
            let server = self.clone();
            let writer = writer.clone();
            tasks.spawn(async move {
                let id = request.id.clone();
                let response = server
                    .clone()
                    .handle_request(request, request_key.clone(), cancel)
                    .await;
                server.inflight.remove(&request_key);
                if let Some(resp) = response {
                    if let Err(e) = writer.write_response(&resp).await {
                        tracing::warn!(error = %e, ?id, "failed to write response");
                    }
                }
            });

            // Reap finished tasks without blocking.
            while let Some(res) = tasks.try_join_next() {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "request task panicked");
                }
            }
        }

        // On shutdown, cancel any in-flight tool calls. On a normal EOF we let
        // outstanding requests finish so their responses are still written.
        if self.root_cancel.is_cancelled() {
            for entry in self.inflight.iter() {
                entry.value().cancel();
            }
        }
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                tracing::warn!(error = %e, "request task panicked during drain");
            }
        }
        Ok(())
    }

    /// Handle a notification (no response).
    fn handle_notification(&self, req: &Request) {
        match req.method.as_str() {
            "notifications/initialized" => {
                tracing::debug!("client initialized");
            }
            "notifications/cancelled" => {
                if let Some(id) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("requestId"))
                    .map(cancel_key)
                {
                    if let Some(token) = self.inflight.get(&id) {
                        tracing::info!(request_id = %id, "cancelling in-flight request");
                        token.cancel();
                    }
                }
            }
            other => tracing::debug!(method = other, "ignoring unknown notification"),
        }
    }

    /// Handle a request and produce a response.
    async fn handle_request(
        self: Arc<Self>,
        req: Request,
        request_key: String,
        cancel: CancellationToken,
    ) -> Option<Response> {
        let id = req.id.clone();
        match req.method.as_str() {
            "initialize" => {
                self.initialized.store(true, Ordering::SeqCst);
                Some(Response::success(
                    id,
                    serde_json::to_value(self.initialize_result()).unwrap_or_default(),
                ))
            }
            "ping" => Some(Response::success(id, serde_json::json!({}))),
            "tools/list" => {
                let config = self.config.snapshot();
                let result = ListToolsResult {
                    tools: self.registry.definitions(&config),
                    next_cursor: None,
                };
                Some(Response::success(
                    id,
                    serde_json::to_value(result).unwrap_or_default(),
                ))
            }
            "tools/call" => Some(self.handle_tool_call(req, request_key, cancel).await),
            other => Some(Response::error(
                id,
                ErrorObject::new(
                    error_codes::METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                    None,
                ),
            )),
        }
    }

    fn initialize_result(&self) -> InitializeResult {
        let config = self.config.snapshot();
        InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                logging: Some(serde_json::json!({})),
            },
            server_info: Implementation {
                name: config.server.name.clone(),
                version: config.server.version.clone(),
            },
            instructions: config.server.instructions.clone(),
        }
    }

    /// Execute a `tools/call`.
    async fn handle_tool_call(
        self: Arc<Self>,
        req: Request,
        request_id: String,
        cancel: CancellationToken,
    ) -> Response {
        let id = req.id.clone();
        let params: CallToolParams = match req
            .params
            .ok_or_else(|| "missing params".to_string())
            .and_then(|p| serde_json::from_value(p).map_err(|e| e.to_string()))
        {
            Ok(p) => p,
            Err(e) => {
                return Response::error(
                    id,
                    ErrorObject::new(
                        error_codes::INVALID_PARAMS,
                        format!("invalid tools/call params: {e}"),
                        None,
                    ),
                )
            }
        };

        let config = self.config.snapshot();
        let tool = match self.registry.get_enabled(&params.name, &config) {
            Some(t) => t,
            None => {
                let msg = if self.registry.is_known_but_disabled(&params.name, &config) {
                    format!("tool '{}' is disabled by configuration", params.name)
                } else {
                    format!("unknown tool '{}'", params.name)
                };
                return Response::error(
                    id,
                    ErrorObject::new(error_codes::METHOD_NOT_FOUND, msg, None),
                );
            }
        };

        // Resolve policy for this tool.
        let policy = match EffectivePolicy::resolve(&config, &params.name) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                return Response::error(
                    id,
                    ErrorObject::new(e.json_rpc_code(), e.to_string(), None),
                )
            }
        };

        // Per-request cancellation token was registered by the read loop.
        let ctx = ToolContext {
            policy: policy.clone(),
            working_dir: self.working_dir.clone(),
            cancel: cancel.clone(),
            metrics: self.metrics.clone(),
            config: config.clone(),
            request_id: request_id.clone(),
        };

        // Bounded concurrency.
        let permit = self.semaphore.clone().acquire_owned().await;
        let _permit = match permit {
            Ok(p) => p,
            Err(_) => {
                return Response::error(
                    id,
                    ErrorObject::new(error_codes::INTERNAL_ERROR, "server is shutting down", None),
                );
            }
        };

        let hard_ceiling = policy.max_runtime();
        let started = std::time::Instant::now();
        let span = tracing::info_span!("tool_call", tool = %params.name, request_id = %request_id);
        let _enter = span.enter();

        // Enforce the hard runtime ceiling and cancellation uniformly, on top of
        // any finer-grained timeout the tool applies internally.
        let outcome = ctx
            .guarded(hard_ceiling, tool.call(&ctx, params.arguments))
            .await;
        let elapsed = started.elapsed();

        let (result, metric) = match outcome {
            Ok(r) => {
                let m = if r.is_error == Some(true) {
                    Outcome::Failure
                } else {
                    Outcome::Success
                };
                (r, m)
            }
            Err(e @ (ToolError::Cancelled | ToolError::Timeout(_))) => {
                (error_result(&e), Outcome::Cancelled)
            }
            Err(e) => (error_result(&e), Outcome::Failure),
        };

        let output_bytes: u64 = result
            .content
            .iter()
            .map(|c| match c {
                Content::Text { text } => text.len() as u64,
                _ => 0,
            })
            .sum();
        self.metrics
            .record(&params.name, metric, elapsed, output_bytes);
        tracing::info!(
            tool = %params.name,
            outcome = ?metric,
            elapsed_ms = elapsed.as_millis() as u64,
            "tool call complete"
        );

        Response::success(id, serde_json::to_value(result).unwrap_or_default())
    }
}

/// Build a `CallToolResult` describing a `ToolError`, tagged with its category.
fn error_result(e: &ToolError) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(format!("[{}] {}", e.category(), e))],
        is_error: Some(true),
    }
}

/// Normalise a `requestId` JSON value into the string key used by `inflight`.
fn cancel_key(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::Config;
    use nebula_mcp_protocol::RequestId;
    use nebula_mcp_tools::ToolServices;

    fn test_server() -> Arc<Server> {
        let mut config = Config::default();
        config.security.allowed_paths = vec!["/**".into()];
        config.security.allowed_commands = vec!["echo".into()];
        let store = ConfigStore::new(config);
        let registry = nebula_mcp_tools::build_registry(&ToolServices::new());
        Arc::new(Server::new(
            registry,
            store,
            std::env::temp_dir(),
            CancellationToken::new(),
        ))
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let server = test_server();
        let init = Request {
            jsonrpc: "2.0".into(),
            id: Some(RequestId::Number(1)),
            method: "initialize".into(),
            params: Some(serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            })),
        };
        let resp = server
            .clone()
            .handle_request(init, "1".into(), CancellationToken::new())
            .await
            .unwrap();
        assert!(resp.result.is_some());

        let list = Request {
            jsonrpc: "2.0".into(),
            id: Some(RequestId::Number(2)),
            method: "tools/list".into(),
            params: None,
        };
        let resp = server
            .clone()
            .handle_request(list, "2".into(), CancellationToken::new())
            .await
            .unwrap();
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().len();
        assert!(tools > 30, "expected a rich tool set, got {tools}");
    }

    #[tokio::test]
    async fn tool_call_executes() {
        let server = test_server();
        let call = Request {
            jsonrpc: "2.0".into(),
            id: Some(RequestId::Number(3)),
            method: "tools/call".into(),
            params: Some(serde_json::json!({
                "name": "terminal.run",
                "arguments": {"program": "echo", "args": ["hi"]}
            })),
        };
        let resp = server
            .handle_request(call, "3".into(), CancellationToken::new())
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hi"));
    }

    #[tokio::test]
    async fn unknown_tool_is_method_error() {
        let server = test_server();
        let call = Request {
            jsonrpc: "2.0".into(),
            id: Some(RequestId::Number(4)),
            method: "tools/call".into(),
            params: Some(serde_json::json!({"name": "does.not.exist", "arguments": {}})),
        };
        let resp = server
            .handle_request(call, "3".into(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn permission_error_is_surfaced_as_tool_result() {
        let server = test_server();
        // fs.read on a disallowed path -> tool result with isError, not a protocol error.
        let call = Request {
            jsonrpc: "2.0".into(),
            id: Some(RequestId::Number(5)),
            method: "tools/call".into(),
            params: Some(serde_json::json!({
                "name": "fs.read",
                "arguments": {"path": "/secrets/.ssh/id_rsa"}
            })),
        };
        let resp = server
            .handle_request(call, "3".into(), CancellationToken::new())
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }
}
