//! Newline-delimited JSON transport over any async byte streams.
//!
//! MCP's stdio transport frames one JSON object per line. This module provides
//! a reader that yields parsed [`Request`](crate::jsonrpc::Request) values and a
//! writer that serialises [`Response`](crate::jsonrpc::Response) values, each
//! terminated by `\n`. The two halves are split so a server can read on one
//! task and write on another without contention.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::jsonrpc::{Request, Response};

/// Errors that can occur while reading or writing frames.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Underlying I/O failure.
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A line could not be parsed as a JSON-RPC request.
    #[error("failed to decode JSON-RPC frame: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Reads newline-delimited JSON-RPC requests from an async source.
pub struct FrameReader<R> {
    inner: BufReader<R>,
    line: String,
}

impl<R> FrameReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    /// Wrap an async reader.
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
            line: String::new(),
        }
    }

    /// Read the next frame.
    ///
    /// Returns `Ok(None)` on clean end-of-stream. Blank lines are skipped so
    /// that pretty-printers and keep-alive newlines do not surface as parse
    /// errors.
    pub async fn next_frame(&mut self) -> Result<Option<Request>, TransportError> {
        loop {
            self.line.clear();
            let n = self.inner.read_line(&mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let req: Request = serde_json::from_str(trimmed)?;
            return Ok(Some(req));
        }
    }
}

/// Writes newline-delimited JSON-RPC responses to an async sink.
///
/// Cloneable and `Send + Sync`: the inner writer is guarded by a mutex so that
/// concurrent tool tasks can emit responses safely and each frame is written
/// atomically (payload + newline under a single lock hold).
pub struct FrameWriter<W> {
    inner: Arc<Mutex<W>>,
}

// Manual `Clone` so that cloning does not require `W: Clone` (the writer lives
// behind an `Arc`).
impl<W> Clone for FrameWriter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin + Send,
{
    /// Wrap an async writer.
    pub fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    /// Serialise and write a full response frame, flushing immediately.
    pub async fn write_response(&self, resp: &Response) -> Result<(), TransportError> {
        let mut buf = serde_json::to_vec(resp)?;
        buf.push(b'\n');
        let mut guard = self.inner.lock().await;
        guard.write_all(&buf).await?;
        guard.flush().await?;
        Ok(())
    }

    /// Write an arbitrary pre-serialised JSON value as a frame (used for
    /// server-initiated notifications).
    pub async fn write_value(&self, value: &serde_json::Value) -> Result<(), TransportError> {
        let mut buf = serde_json::to_vec(value)?;
        buf.push(b'\n');
        let mut guard = self.inner.lock().await;
        guard.write_all(&buf).await?;
        guard.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{RequestId, Response};

    #[tokio::test]
    async fn reads_frames_and_skips_blank_lines() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n"
        );
        let mut reader = FrameReader::new(input.as_bytes());
        let f1 = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(f1.method, "a");
        let f2 = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(f2.method, "b");
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn writes_response_with_trailing_newline() {
        let buf: Vec<u8> = Vec::new();
        let writer = FrameWriter::new(buf);
        let resp = Response::success(Some(RequestId::Number(1)), serde_json::json!({"ok": true}));
        writer.write_response(&resp).await.unwrap();
        let guard = writer.inner.lock().await;
        let text = String::from_utf8(guard.clone()).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains("\"ok\":true"));
    }
}
