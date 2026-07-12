//! Host clipboard bridge (text only).
//!
//! Sync is **permission-gated per device** exactly like input: deny by
//! default, toggled in the panel, revocable live. Both directions enforce
//! [`ndsp_protocol::messages::MAX_CLIPBOARD_BYTES`].
//!
//! * host → viewers: a platform watcher publishes host clipboard changes
//!   into a `watch` channel; each session forwards them to its client while
//!   the device's clipboard grant is on.
//! * viewer → host: inbound `Clipboard` messages are applied through the
//!   platform setter (same grant + size checks first).
//!
//! Echo suppression: text applied *from a client* is remembered so the
//! watcher doesn't loop it straight back to every viewer.
//!
//! Windows uses a `GetClipboardSequenceNumber` poll (400 ms) instead of a
//! clipboard-listener window: no hidden HWND + message pump to own, and the
//! latency is fine for a human copy/paste action. Non-Windows hosts get an
//! in-memory setter (useful for tests/CI; Linux/macOS hosts are roadmap).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[cfg(windows)]
mod windows_clipboard;

/// Latest host clipboard text (seq 0 = nothing published yet).
#[derive(Debug, Clone, Default)]
pub struct ClipboardEvent {
    pub seq: u64,
    pub text: Arc<String>,
}

/// Applies text to the OS clipboard.
pub trait ClipboardSetter: Send + Sync {
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}

pub struct Clipboard {
    tx: watch::Sender<ClipboardEvent>,
    setter: Box<dyn ClipboardSetter>,
    /// Last text applied from a client — suppressed once when the platform
    /// watcher reports it back (echo prevention).
    suppress: Mutex<Option<String>>,
    seq: AtomicU64,
    /// Last text applied from any client (observable for tests/panel).
    last_applied: Mutex<Option<String>>,
}

impl Clipboard {
    pub fn new() -> Self {
        #[cfg(windows)]
        let setter: Box<dyn ClipboardSetter> = Box::new(windows_clipboard::WindowsSetter);
        #[cfg(not(windows))]
        let setter: Box<dyn ClipboardSetter> = Box::new(MemorySetter);
        let (tx, _) = watch::channel(ClipboardEvent::default());
        Self {
            tx,
            setter,
            suppress: Mutex::new(None),
            seq: AtomicU64::new(0),
            last_applied: Mutex::new(None),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<ClipboardEvent> {
        self.tx.subscribe()
    }

    /// Called by the platform watcher (or tests) when the *host* clipboard
    /// changed. Publishes to all sessions unless it's the echo of a text a
    /// client just sent us.
    pub fn publish_from_host(&self, text: String) {
        {
            let mut sup = self.suppress.lock().unwrap();
            if sup.as_deref() == Some(text.as_str()) {
                *sup = None;
                return;
            }
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(ClipboardEvent {
            seq,
            text: Arc::new(text),
        });
    }

    /// Apply text received from a granted client to the host clipboard.
    pub fn apply_from_client(&self, text: &str) -> anyhow::Result<()> {
        *self.suppress.lock().unwrap() = Some(text.to_string());
        self.setter.set_text(text)?;
        *self.last_applied.lock().unwrap() = Some(text.to_string());
        Ok(())
    }

    /// Last text a client pushed to the host (None if never). Test/telemetry
    /// hook — the authoritative copy lives in the OS clipboard.
    pub fn last_applied(&self) -> Option<String> {
        self.last_applied.lock().unwrap().clone()
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the platform clipboard watcher (Windows only; no-op elsewhere).
pub fn spawn_watcher(state: Arc<crate::state::AppState>) {
    #[cfg(windows)]
    windows_clipboard::spawn_watcher(state);
    #[cfg(not(windows))]
    let _ = state;
}

#[cfg(not(windows))]
struct MemorySetter;

#[cfg(not(windows))]
impl ClipboardSetter for MemorySetter {
    fn set_text(&self, _text: &str) -> anyhow::Result<()> {
        // No OS clipboard integration on non-Windows hosts yet; the text is
        // still recorded in `last_applied` by the caller.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_subscribe() {
        let cb = Clipboard::new();
        let mut rx = cb.subscribe();
        cb.publish_from_host("hello".into());
        rx.changed().await.unwrap();
        let ev = rx.borrow().clone();
        assert_eq!(ev.text.as_str(), "hello");
        assert_eq!(ev.seq, 1);
    }

    #[tokio::test]
    async fn client_apply_suppresses_echo_once() {
        let cb = Clipboard::new();
        let rx = cb.subscribe();
        cb.apply_from_client("from-client").unwrap();
        assert_eq!(cb.last_applied().as_deref(), Some("from-client"));
        // Watcher notices the change and reports it — must not be republished.
        cb.publish_from_host("from-client".into());
        assert!(!rx.has_changed().unwrap());
        // A genuine later copy of the same text does flow.
        cb.publish_from_host("from-client".into());
        assert!(rx.has_changed().unwrap());
    }
}
