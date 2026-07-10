//! Persistent interactive process sessions.
//!
//! Long-lived child processes (shells, REPLs, `powershell -NoExit`, ...) whose
//! stdin can be written to and whose combined output is buffered in a bounded
//! ring buffer for incremental reads. This lets an agent drive an interactive
//! program across multiple tool calls.
//!
//! A single [`SessionManager`] is shared by all session tools via
//! [`crate::ToolServices`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use dashmap::DashMap;
use nebula_mcp_core::ToolError;
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::Notify;

/// A bounded output buffer shared between reader tasks and readers.
struct OutputBuffer {
    data: Mutex<VecDeque<u8>>,
    limit: usize,
    notify: Notify,
    dropped: Mutex<u64>,
}

impl OutputBuffer {
    fn new(limit: usize) -> Self {
        Self {
            data: Mutex::new(VecDeque::new()),
            limit,
            notify: Notify::new(),
            dropped: Mutex::new(0),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut buf = self.data.lock();
        buf.extend(bytes.iter().copied());
        if buf.len() > self.limit {
            let overflow = buf.len() - self.limit;
            for _ in 0..overflow {
                buf.pop_front();
            }
            *self.dropped.lock() += overflow as u64;
        }
        drop(buf);
        self.notify.notify_waiters();
    }

    fn drain(&self) -> (String, u64) {
        let mut buf = self.data.lock();
        let bytes: Vec<u8> = buf.drain(..).collect();
        let dropped = std::mem::take(&mut *self.dropped.lock());
        (String::from_utf8_lossy(&bytes).into_owned(), dropped)
    }

    fn peek(&self) -> String {
        let buf = self.data.lock();
        String::from_utf8_lossy(&buf.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

/// A single live session.
struct Session {
    id: String,
    program: String,
    child: Mutex<Child>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    output: Arc<OutputBuffer>,
    created: std::time::Instant,
}

/// Public, serialisable summary of a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    /// Session identifier.
    pub id: String,
    /// Program that was launched.
    pub program: String,
    /// Seconds since the session was opened.
    pub age_secs: u64,
    /// Whether the process is still running.
    pub running: bool,
}

/// Manages the lifetime of interactive sessions.
#[derive(Default)]
pub struct SessionManager {
    sessions: DashMap<String, Arc<Session>>,
}

impl SessionManager {
    /// Create an empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new session, returning its id.
    pub fn open(
        &self,
        program: &str,
        args: &[String],
        cwd: PathBuf,
        env: &[(String, String)],
        output_limit: usize,
    ) -> Result<String, ToolError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Io(format!("spawning session '{program}': {e}")))?;

        let output = Arc::new(OutputBuffer::new(output_limit.max(4096)));
        let stdin = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            spawn_pump(stdout, output.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_pump(stderr, output.clone());
        }

        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(Session {
            id: id.clone(),
            program: program.to_string(),
            child: Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            output,
            created: std::time::Instant::now(),
        });
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// Write bytes to a session's stdin.
    pub async fn write(&self, id: &str, data: &[u8]) -> Result<(), ToolError> {
        let session = self.get(id)?;
        let mut guard = session.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(ToolError::Execution("session stdin is closed".into()));
        };
        stdin
            .write_all(data)
            .await
            .map_err(|e| ToolError::Io(format!("writing to session stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| ToolError::Io(format!("flushing session stdin: {e}")))?;
        Ok(())
    }

    /// Read (and clear) buffered output, optionally waiting up to `wait_ms` for
    /// new output to arrive.
    pub async fn read(&self, id: &str, wait_ms: u64) -> Result<(String, u64), ToolError> {
        let session = self.get(id)?;
        if wait_ms > 0 {
            // Wait for a notification or the timeout, whichever comes first.
            let notified = session.output.notify.notified();
            tokio::pin!(notified);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(wait_ms), notified).await;
        }
        Ok(session.output.drain())
    }

    /// Peek at buffered output without clearing it.
    pub fn peek(&self, id: &str) -> Result<String, ToolError> {
        Ok(self.get(id)?.output.peek())
    }

    /// Close a session, killing the process.
    pub async fn close(&self, id: &str) -> Result<(), ToolError> {
        let Some((_, session)) = self.sessions.remove(id) else {
            return Err(ToolError::InvalidArguments(format!(
                "no such session '{id}'"
            )));
        };
        // Drop stdin to signal EOF, then kill.
        session.stdin.lock().await.take();
        let mut child = session.child.lock();
        let _ = child.start_kill();
        Ok(())
    }

    /// List all sessions.
    #[must_use]
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|kv| {
                let s = kv.value();
                let running = s.child.lock().try_wait().ok().flatten().is_none();
                SessionInfo {
                    id: s.id.clone(),
                    program: s.program.clone(),
                    age_secs: s.created.elapsed().as_secs(),
                    running,
                }
            })
            .collect()
    }

    fn get(&self, id: &str) -> Result<Arc<Session>, ToolError> {
        self.sessions
            .get(id)
            .map(|s| s.clone())
            .ok_or_else(|| ToolError::InvalidArguments(format!("no such session '{id}'")))
    }
}

/// Spawn a task that pumps a child stream into the output buffer.
fn spawn_pump<R>(mut reader: R, output: Arc<OutputBuffer>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => output.push(&buf[..n]),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_write_read_close_shell() {
        if which::which("sh").is_err() {
            return;
        }
        let mgr = SessionManager::new();
        let id = mgr
            .open("sh", &[], std::env::temp_dir(), &[], 65536)
            .unwrap();
        mgr.write(&id, b"echo session-hello\n").await.unwrap();
        // Give the shell a moment and read.
        let (out, _dropped) = mgr.read(&id, 500).await.unwrap();
        // Read again in case output arrived after the first drain.
        let (out2, _) = mgr.read(&id, 500).await.unwrap();
        assert!(
            out.contains("session-hello") || out2.contains("session-hello"),
            "got: {out}{out2}"
        );
        assert_eq!(mgr.list().len(), 1);
        mgr.close(&id).await.unwrap();
        assert!(mgr.write(&id, b"x").await.is_err());
    }

    #[tokio::test]
    async fn unknown_session_errors() {
        let mgr = SessionManager::new();
        assert!(mgr.read("nope", 0).await.is_err());
        assert!(mgr.close("nope").await.is_err());
    }
}
