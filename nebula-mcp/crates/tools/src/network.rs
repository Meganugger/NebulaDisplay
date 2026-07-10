//! Network tools.
//!
//! Pure-Rust implementations where practical (HTTP(S), DNS, TCP connect,
//! latency sampling, TLS certificate inspection, WebSocket), plus typed
//! wrappers over standard networking utilities for capabilities without a
//! lightweight native path (`ping`, `iperf3`, packet capture, QUIC/HTTP3 via
//! `curl`). All network tools honour the `allow_network` policy gate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::{exec_result, json_value_result};
use crate::common::{Args, ObjectSchema};
use crate::ToolServices;

const CATEGORY: &str = "network";

/// Build network tools.
pub fn tools(services: &ToolServices) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(HttpRequest {
            http: services.http.clone(),
        }),
        Arc::new(Download {
            http: services.http.clone(),
        }),
        Arc::new(DnsLookup),
        Arc::new(TcpConnect),
        Arc::new(LatencySample),
        Arc::new(TlsInfo),
        Arc::new(WebSocketProbe),
        Arc::new(Ping),
        Arc::new(Iperf),
        Arc::new(PacketCapture),
        Arc::new(QuicProbe),
    ]
}

fn ro() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        open_world_hint: Some(true),
        ..Default::default()
    })
}

/// HTTP/HTTPS request.
struct HttpRequest {
    http: reqwest::Client,
}

#[async_trait]
impl Tool for HttpRequest {
    fn name(&self) -> &str {
        "net.http_request"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Perform an HTTP/HTTPS request and return status, headers and (capped) body."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "Absolute URL.", true)
            .enumerated(
                "method",
                "HTTP method.",
                &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"],
                false,
            )
            .prop(
                "headers",
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                false,
            )
            .string("body", "Optional request body.", false)
            .integer("timeoutSecs", "Timeout override.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let url = a.str("url")?;
        let method = a.str_or("method", "GET")?.to_ascii_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| ToolError::InvalidArguments(format!("invalid method '{method}'")))?;
        let mut req = self.http.request(method, url);
        for (k, v) in a.opt_string_map("headers")? {
            req = req.header(k, v);
        }
        if let Some(b) = a.opt_str("body")? {
            req = req.body(b.to_string());
        }
        let limit = ctx.policy.max_output_bytes();
        let timeout = ctx.timeout(a.opt_u64("timeoutSecs")?);
        let fut = async move {
            let start = Instant::now();
            let resp = req
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("request failed: {e}")))?;
            let status = resp.status().as_u16();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string()))
                .collect();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Execution(format!("reading body: {e}")))?;
            let truncated = bytes.len() > limit;
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
            Ok::<_, ToolError>(json!({
                "url": url,
                "status": status,
                "headers": headers,
                "body": body,
                "bodyTruncated": truncated,
                "elapsedMs": start.elapsed().as_millis(),
            }))
        };
        let v = ctx.guarded(timeout, fut).await?;
        Ok(json_value_result(v))
    }
}

/// Stream a URL to a file on disk with a size cap.
struct Download {
    http: reqwest::Client,
}

#[async_trait]
impl Tool for Download {
    fn name(&self) -> &str {
        "net.download"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Download a URL to a file (within an allowed root), streaming with a maximum size cap."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "URL to download.", true)
            .string(
                "outputPath",
                "Destination file path (within an allowed root).",
                true,
            )
            .integer(
                "maxBytes",
                "Maximum bytes to write (default 104857600 = 100 MiB).",
                false,
            )
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
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;

        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let url = a.str("url")?.to_string();
        let out = ctx.resolve_path(a.str("outputPath")?)?;
        let max_bytes = a.u64_or("maxBytes", 100 * 1024 * 1024)?;
        let timeout = ctx.timeout(a.opt_u64("timeoutSecs")?);
        let http = self.http.clone();

        let fut = async move {
            if let Some(parent) = out.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::Io(format!("creating parent dir: {e}")))?;
            }
            let resp = http
                .get(&url)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("request failed: {e}")))?;
            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                return Err(ToolError::Execution(format!(
                    "download failed with HTTP {status}"
                )));
            }
            let mut file = tokio::fs::File::create(&out)
                .await
                .map_err(|e| ToolError::Io(format!("creating {}: {e}", out.display())))?;
            let mut written: u64 = 0;
            let mut truncated = false;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| ToolError::Execution(format!("stream error: {e}")))?;
                let remaining = max_bytes.saturating_sub(written);
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let take = (chunk.len() as u64).min(remaining) as usize;
                file.write_all(&chunk[..take])
                    .await
                    .map_err(|e| ToolError::Io(format!("writing output: {e}")))?;
                written += take as u64;
                if take < chunk.len() {
                    truncated = true;
                    break;
                }
            }
            file.flush()
                .await
                .map_err(|e| ToolError::Io(format!("flushing output: {e}")))?;
            Ok::<_, ToolError>(json!({
                "url": url,
                "outputPath": out.display().to_string(),
                "status": status,
                "bytesWritten": written,
                "truncated": truncated,
            }))
        };
        Ok(json_value_result(ctx.guarded(timeout, fut).await?))
    }
}

/// DNS resolution.
struct DnsLookup;

#[async_trait]
impl Tool for DnsLookup {
    fn name(&self) -> &str {
        "net.dns_lookup"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Resolve a hostname to IP addresses."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("host", "Hostname to resolve.", true)
            .integer("port", "Port for resolution (default 0).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let host = a.str("host")?.to_string();
        let port = a.u64_or("port", 0)? as u16;
        let timeout = ctx.timeout(None);
        let fut = async move {
            let addrs: Vec<String> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| ToolError::Execution(format!("resolution failed: {e}")))?
                .map(|s| s.ip().to_string())
                .collect();
            Ok::<_, ToolError>(json!({ "host": host, "addresses": addrs }))
        };
        Ok(json_value_result(ctx.guarded(timeout, fut).await?))
    }
}

/// TCP connect probe.
struct TcpConnect;

#[async_trait]
impl Tool for TcpConnect {
    fn name(&self) -> &str {
        "net.tcp_connect"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Measure TCP connect latency to host:port."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("host", "Host.", true)
            .integer("port", "Port.", true)
            .integer("timeoutMs", "Connect timeout (default 5000).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let host = a.str("host")?.to_string();
        let port = a
            .opt_u64("port")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'port'".into()))?
            as u16;
        let to = Duration::from_millis(a.u64_or("timeoutMs", 5000)?);
        let (ok, elapsed, err) = connect_once(&host, port, to).await;
        Ok(json_value_result(json!({
            "host": host, "port": port, "connected": ok,
            "elapsedMs": elapsed.as_millis(),
            "error": err,
        })))
    }
}

async fn connect_once(host: &str, port: u16, to: Duration) -> (bool, Duration, Option<String>) {
    let start = Instant::now();
    match tokio::time::timeout(to, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => (true, start.elapsed(), None),
        Ok(Err(e)) => (false, start.elapsed(), Some(e.to_string())),
        Err(_) => (false, start.elapsed(), Some("connect timed out".into())),
    }
}

/// Repeated TCP connect latency sampling.
struct LatencySample;

#[async_trait]
impl Tool for LatencySample {
    fn name(&self) -> &str {
        "net.latency"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Sample TCP connect latency over multiple attempts and report min/avg/max/jitter."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("host", "Host.", true)
            .integer("port", "Port.", true)
            .integer("count", "Number of samples (default 5, max 50).", false)
            .integer("timeoutMs", "Per-attempt timeout (default 3000).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let host = a.str("host")?.to_string();
        let port = a
            .opt_u64("port")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'port'".into()))?
            as u16;
        let count = a.u64_or("count", 5)?.clamp(1, 50);
        let to = Duration::from_millis(a.u64_or("timeoutMs", 3000)?);

        let mut samples = Vec::new();
        let mut ok = 0u64;
        for _ in 0..count {
            ctx.ensure_active()?;
            let (success, elapsed, _) = connect_once(&host, port, to).await;
            if success {
                ok += 1;
                samples.push(elapsed.as_secs_f64() * 1000.0);
            }
        }
        let (min, avg, max, jitter) = stats(&samples);
        Ok(json_value_result(json!({
            "host": host, "port": port,
            "attempts": count, "successful": ok,
            "minMs": min, "avgMs": avg, "maxMs": max, "jitterMs": jitter,
            "samplesMs": samples,
        })))
    }
}

fn stats(samples: &[f64]) -> (f64, f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples.iter().map(|s| (s - avg).powi(2)).sum::<f64>() / samples.len() as f64;
    (min, avg, max, variance.sqrt())
}

/// TLS certificate inspection via a rustls handshake.
struct TlsInfo;

#[async_trait]
impl Tool for TlsInfo {
    fn name(&self) -> &str {
        "net.tls_info"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Perform a TLS handshake and report the negotiated protocol and peer certificate details."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("host", "Host (SNI).", true)
            .integer("port", "Port (default 443).", false)
            .integer("timeoutMs", "Handshake timeout (default 8000).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let host = a.str("host")?.to_string();
        let port = a.u64_or("port", 443)? as u16;
        let to = Duration::from_millis(a.u64_or("timeoutMs", 8000)?);
        let v = tokio::time::timeout(to, tls_handshake(host.clone(), port))
            .await
            .map_err(|_| ToolError::Timeout(to))??;
        Ok(json_value_result(v))
    }
}

async fn tls_handshake(host: String, port: u16) -> Result<Value> {
    use tokio_rustls::TlsConnector;

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = rustls_pki_types::ServerName::try_from(host.clone())
        .map_err(|_| ToolError::InvalidArguments(format!("invalid host '{host}'")))?;

    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| ToolError::Execution(format!("tcp connect: {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ToolError::Execution(format!("tls handshake: {e}")))?;

    let (_, conn) = tls.get_ref();
    let protocol = conn
        .protocol_version()
        .map(|v| format!("{v:?}"))
        .unwrap_or_else(|| "unknown".into());
    let cipher = conn
        .negotiated_cipher_suite()
        .map(|c| format!("{:?}", c.suite()))
        .unwrap_or_else(|| "unknown".into());

    let mut certs_info = Vec::new();
    if let Some(chain) = conn.peer_certificates() {
        for cert in chain {
            match x509_parser::parse_x509_certificate(cert.as_ref()) {
                Ok((_, parsed)) => {
                    certs_info.push(json!({
                        "subject": parsed.subject().to_string(),
                        "issuer": parsed.issuer().to_string(),
                        "notBefore": parsed.validity().not_before.to_string(),
                        "notAfter": parsed.validity().not_after.to_string(),
                        "serial": parsed.raw_serial_as_string(),
                    }));
                }
                Err(_) => certs_info.push(json!({"der_bytes": cert.as_ref().len()})),
            }
        }
    }

    Ok(json!({
        "host": host,
        "port": port,
        "protocol": protocol,
        "cipherSuite": cipher,
        "certificates": certs_info,
    }))
}

/// WebSocket probe: connect, optionally send a message and read replies.
struct WebSocketProbe;

#[async_trait]
impl Tool for WebSocketProbe {
    fn name(&self) -> &str {
        "net.websocket"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Connect to a WebSocket endpoint, optionally send a text message, and collect replies for a short window."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "ws:// or wss:// URL.", true)
            .string(
                "send",
                "Optional text message to send after connecting.",
                false,
            )
            .integer(
                "readMs",
                "Milliseconds to collect replies (default 1500).",
                false,
            )
            .integer(
                "maxMessages",
                "Maximum messages to collect (default 10).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let url = a.str("url")?.to_string();
        let send = a.opt_str("send")?.map(str::to_string);
        let read_ms = a.u64_or("readMs", 1500)?;
        let max_msgs = a.u64_or("maxMessages", 10)? as usize;
        let connect_to = ctx.timeout(None);

        let fut = async move {
            let (mut ws, response) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|e| ToolError::Execution(format!("websocket connect: {e}")))?;
            let status = response.status().as_u16();
            if let Some(msg) = &send {
                ws.send(Message::Text(msg.clone()))
                    .await
                    .map_err(|e| ToolError::Execution(format!("websocket send: {e}")))?;
            }
            let mut messages = Vec::new();
            let deadline = tokio::time::sleep(Duration::from_millis(read_ms));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    () = &mut deadline => break,
                    item = ws.next() => match item {
                        Some(Ok(Message::Text(t))) => {
                            messages.push(t);
                            if messages.len() >= max_msgs { break; }
                        }
                        Some(Ok(Message::Binary(b))) => {
                            messages.push(format!("<binary {} bytes>", b.len()));
                            if messages.len() >= max_msgs { break; }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(ToolError::Execution(format!("websocket error: {e}"))),
                        None => break,
                    }
                }
            }
            let _ = ws.close(None).await;
            Ok::<_, ToolError>(json!({
                "url": url,
                "handshakeStatus": status,
                "messages": messages,
            }))
        };
        Ok(json_value_result(ctx.guarded(connect_to, fut).await?))
    }
}

/// ICMP ping via the system `ping` utility.
struct Ping;

#[async_trait]
impl Tool for Ping {
    fn name(&self) -> &str {
        "net.ping"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Ping a host using the system 'ping' utility (must be allowlisted)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("host", "Host to ping.", true)
            .integer("count", "Echo request count (default 4).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let host = a.str("host")?.to_string();
        let count = a.u64_or("count", 4)?.to_string();
        // Windows uses -n, Unix uses -c for count.
        let count_flag = if cfg!(windows) { "-n" } else { "-c" };
        let spec = CommandSpec::new("ping", ctx.working_dir.clone(), ctx).args(vec![
            count_flag.to_string(),
            count,
            host.clone(),
        ]);
        let result = run_checked(ctx, spec, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result(&format!("ping {host}"), &result))
    }
}

/// Throughput measurement via iperf3.
struct Iperf;

#[async_trait]
impl Tool for Iperf {
    fn name(&self) -> &str {
        "net.iperf"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Run an iperf3 client against a server and return JSON throughput results (iperf3 must be allowlisted)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("server", "iperf3 server host.", true)
            .integer("port", "Server port (default 5201).", false)
            .integer("durationSecs", "Test duration (default 10).", false)
            .boolean("reverse", "Reverse direction (download).", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let server = a.str("server")?.to_string();
        let port = a.u64_or("port", 5201)?.to_string();
        let dur = a.u64_or("durationSecs", 10)?.to_string();
        let mut iperf_args = vec![
            "-c".to_string(),
            server.clone(),
            "-p".into(),
            port,
            "-t".into(),
            dur,
            "--json".into(),
        ];
        if a.bool_or("reverse", false)? {
            iperf_args.push("-R".into());
        }
        let spec = CommandSpec::new("iperf3", ctx.working_dir.clone(), ctx).args(iperf_args);
        let result = run_checked(ctx, spec, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result(&format!("iperf3 -c {server}"), &result))
    }
}

/// Packet capture via dumpcap/tshark/tcpdump.
struct PacketCapture;

#[async_trait]
impl Tool for PacketCapture {
    fn name(&self) -> &str {
        "net.packet_capture"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Capture packets to a pcap file using dumpcap/tcpdump. Requires elevation and network policy; the output path must be within an allowed root."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("interface", "Capture interface name/index.", true)
            .string(
                "outputPath",
                "Destination .pcap path (within an allowed root).",
                true,
            )
            .integer("durationSecs", "Capture duration (default 10).", false)
            .string("filter", "Optional BPF capture filter.", false)
            .string(
                "tool",
                "Capture tool to use: dumpcap or tcpdump (default dumpcap).",
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
        ctx.policy.ensure_network_allowed()?;
        ctx.policy.ensure_elevation_allowed()?;
        let iface = a.str("interface")?.to_string();
        let out = ctx.resolve_path(a.str("outputPath")?)?;
        let dur = a.u64_or("durationSecs", 10)?;
        let filter = a.opt_str("filter")?.map(str::to_string);
        let tool = a.str_or("tool", "dumpcap")?.to_string();

        let cmd_args = match tool.as_str() {
            "dumpcap" => {
                let mut v = vec![
                    "-i".to_string(),
                    iface,
                    "-a".into(),
                    format!("duration:{dur}"),
                    "-w".into(),
                    out.display().to_string(),
                ];
                if let Some(f) = filter {
                    v.push("-f".into());
                    v.push(f);
                }
                v
            }
            "tcpdump" => {
                let mut v = vec![
                    "-i".to_string(),
                    iface,
                    "-w".into(),
                    out.display().to_string(),
                    "-G".into(),
                    dur.to_string(),
                    "-W".into(),
                    "1".into(),
                ];
                if let Some(f) = filter {
                    v.push(f);
                }
                v
            }
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "unsupported capture tool '{other}'"
                )))
            }
        };
        let spec = CommandSpec::new(&tool, ctx.working_dir.clone(), ctx).args(cmd_args);
        let result = run_checked(ctx, spec, Some(dur + 10)).await?;
        Ok(exec_result(&format!("{tool} capture"), &result))
    }
}

/// QUIC / HTTP3 reachability probe via curl --http3.
struct QuicProbe;

#[async_trait]
impl Tool for QuicProbe {
    fn name(&self) -> &str {
        "net.quic_probe"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Probe an endpoint for HTTP/3 (QUIC) support using 'curl --http3' (curl must be allowlisted and built with HTTP/3)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "HTTPS URL to probe.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        ro()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let url = a.str("url")?.to_string();
        let spec = CommandSpec::new("curl", ctx.working_dir.clone(), ctx).args(vec![
            "--http3".to_string(),
            "-sS".into(),
            "-o".into(),
            "/dev/null".into(),
            "-w".into(),
            "%{http_version} %{http_code}".into(),
            url.clone(),
        ]);
        let result = run_checked(ctx, spec, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result(&format!("curl --http3 {url}"), &result))
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
            allowed_commands: vec!["ping".into(), "curl".into(), "iperf3".into()],
            allow_network: network,
            default_timeout_secs: 10,
            max_runtime_secs: 30,
            max_output_bytes: 1 << 20,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("net", &base, None).unwrap();
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

    #[test]
    fn stats_are_sane() {
        let (min, avg, max, jitter) = stats(&[10.0, 20.0, 30.0]);
        assert_eq!(min, 10.0);
        assert_eq!(max, 30.0);
        assert_eq!(avg, 20.0);
        assert!(jitter > 0.0);
    }

    #[tokio::test]
    async fn network_gate_blocks_when_disabled() {
        let c = ctx(false);
        let err = DnsLookup
            .call(&c, json!({"host": "localhost"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn dns_resolves_localhost() {
        let c = ctx(true);
        let res = DnsLookup
            .call(&c, json!({"host": "localhost"}))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn tcp_connect_to_closed_port_reports_failure() {
        let c = ctx(true);
        let res = TcpConnect
            .call(
                &c,
                json!({"host": "127.0.0.1", "port": 1, "timeoutMs": 300}),
            )
            .await
            .unwrap();
        // Connection should fail but the tool returns a structured success result.
        assert_eq!(res.is_error, Some(false));
    }
}
