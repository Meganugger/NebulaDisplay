//! A tiny, dependency-free Prometheus metrics endpoint.
//!
//! When `--metrics-addr` is configured, the server binds a minimal HTTP/1.1
//! listener that serves the current per-tool metrics at `GET /metrics` in
//! Prometheus text exposition format. It is intentionally trivial (no routing
//! framework): it reads the request line, checks the path, and writes a
//! response. The accept loop stops when the root cancellation token fires.

use std::net::SocketAddr;

use nebula_mcp_core::Metrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Bind and serve the metrics endpoint until `cancel` fires.
///
/// Returns the bound address (useful when binding to port 0 in tests).
pub async fn serve(
    addr: SocketAddr,
    metrics: Metrics,
    cancel: CancellationToken,
) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "metrics endpoint listening on http://{local}/metrics");
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        let metrics = metrics.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(stream, metrics).await {
                                tracing::debug!(error = %e, "metrics connection error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "metrics accept failed");
                        break;
                    }
                }
            }
        }
    });
    Ok(local)
}

async fn handle_conn(mut stream: TcpStream, metrics: Metrics) -> std::io::Result<()> {
    // Read headers (up to the blank line), bounded to avoid abuse.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .unwrap_or(Ok(0))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }

    let request = String::from_utf8_lossy(&buf);
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let response = if method == "GET" && (path == "/metrics" || path == "/") {
        let body = metrics.to_prometheus();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else if method.is_empty() {
        String::from("HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    } else {
        let body = "not found: try GET /metrics\n";
        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::Outcome;
    use std::time::Duration;

    #[tokio::test]
    async fn serves_metrics_over_http() {
        let metrics = Metrics::new();
        metrics.record("fs.read", Outcome::Success, Duration::from_micros(10), 3);
        let cancel = CancellationToken::new();
        let addr = serve("127.0.0.1:0".parse().unwrap(), metrics, cancel.clone())
            .await
            .unwrap();

        // Raw HTTP GET.
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("nebula_mcp_tool_calls_total{tool=\"fs.read\"} 1"));

        cancel.cancel();
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let cancel = CancellationToken::new();
        let addr = serve(
            "127.0.0.1:0".parse().unwrap(),
            Metrics::new(),
            cancel.clone(),
        )
        .await
        .unwrap();
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        assert!(String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 404"));
        cancel.cancel();
    }
}
