//! Host clipboard bridge (ROADMAP P2.9).
//!
//! Text-only, permission-gated, size-capped:
//! * A device may sync clipboards only after the host explicitly enables it
//!   in the panel (**deny by default** — the clipboard routinely contains
//!   passwords).
//! * Payloads over [`CLIPBOARD_MAX_BYTES`](ndsp_protocol::messages::CLIPBOARD_MAX_BYTES)
//!   are refused in both directions.
//! * Host→viewer flow only *polls* the OS clipboard while at least one
//!   granted device is connected — zero clipboard reads otherwise.
//!
//! Backends: `arboard` (Windows/macOS/X11/Wayland); an in-memory fallback
//! keeps headless machines and CI deterministic (and keeps the e2e suite off
//! the developer's real clipboard).

use ndsp_protocol::messages::CLIPBOARD_MAX_BYTES;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Poll cadence for host-side clipboard changes (no OS event API is
/// portable; 400 ms is imperceptible for a copy/paste flow).
const POLL_INTERVAL: Duration = Duration::from_millis(400);

pub trait HostClipboard: Send + Sync {
    fn name(&self) -> &'static str;
    /// Current clipboard text, if the clipboard holds text.
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}

/// Real OS clipboard via `arboard`. The context is recreated per call —
/// arboard contexts are cheap and holding one hostage across threads causes
/// ownership headaches on X11.
pub struct SystemClipboard;

impl HostClipboard for SystemClipboard {
    fn name(&self) -> &'static str {
        "system"
    }
    fn get_text(&self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        arboard::Clipboard::new()?.set_text(text.to_string())?;
        Ok(())
    }
}

/// Deterministic in-memory clipboard for tests / headless hosts.
#[derive(Default)]
pub struct InMemoryClipboard(Mutex<Option<String>>);

impl InMemoryClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl HostClipboard for InMemoryClipboard {
    fn name(&self) -> &'static str {
        "in-memory"
    }
    fn get_text(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        *self.0.lock().unwrap() = Some(text.to_string());
        Ok(())
    }
}

/// Pick the best backend: the system clipboard when one exists, otherwise
/// in-memory (headless Linux CI has no display server).
pub fn create_clipboard() -> Arc<dyn HostClipboard> {
    match arboard::Clipboard::new() {
        Ok(_) => Arc::new(SystemClipboard),
        Err(e) => {
            info!("system clipboard unavailable ({e}); using in-memory backend");
            Arc::new(InMemoryClipboard::new())
        }
    }
}

fn text_hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Shared echo-suppression state between the watcher (host→viewer) and the
/// apply path (viewer→host): text we just *applied* from a viewer must not
/// bounce straight back out as a "host clipboard change".
#[derive(Default)]
pub struct ClipboardSync {
    last_seen_hash: Mutex<Option<u64>>,
}

impl ClipboardSync {
    /// Apply viewer-provided text to the host clipboard (grant already
    /// checked by the caller). Returns `false` when refused by policy.
    pub fn apply_from_viewer(
        &self,
        clipboard: &dyn HostClipboard,
        text: &str,
    ) -> anyhow::Result<bool> {
        if text.len() > CLIPBOARD_MAX_BYTES {
            warn!(
                len = text.len(),
                "refusing oversized viewer clipboard payload"
            );
            return Ok(false);
        }
        *self.last_seen_hash.lock().unwrap() = Some(text_hash(text));
        clipboard.set_text(text)?;
        Ok(true)
    }

    /// Poll step: returns new host clipboard text exactly once per change.
    pub fn poll_host_change(&self, clipboard: &dyn HostClipboard) -> Option<String> {
        let text = clipboard.get_text()?;
        if text.is_empty() || text.len() > CLIPBOARD_MAX_BYTES {
            return None;
        }
        let hash = text_hash(&text);
        let mut last = self.last_seen_hash.lock().unwrap();
        if *last == Some(hash) {
            return None;
        }
        let first_observation = last.is_none();
        *last = Some(hash);
        // The very first poll just baselines whatever was already in the
        // clipboard — shipping pre-session clipboard content to a device
        // that merely connected would be a surprise leak.
        if first_observation {
            return None;
        }
        Some(text)
    }
}

/// Host→viewer watcher: publishes clipboard changes on
/// `state.clipboard_tx`; sessions forward them to granted clients.
pub async fn run_clipboard_watcher(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if state.is_shutdown() {
            return;
        }
        // Privacy + cycles: poll only while a granted device is connected.
        let any_granted = state.clients.lock().unwrap().values().any(|c| {
            c.clipboard_allowed
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        if !any_granted {
            continue;
        }
        let clipboard = state.clipboard.clone();
        let sync = state.clipboard_sync.clone();
        // arboard can block briefly on X11 — keep it off the async threads.
        let changed =
            tokio::task::spawn_blocking(move || sync.poll_host_change(clipboard.as_ref()))
                .await
                .unwrap_or(None);
        if let Some(text) = changed {
            debug!(len = text.len(), "host clipboard changed; broadcasting");
            state.clipboard_tx.send_modify(|slot| {
                slot.0 += 1;
                slot.1 = Arc::new(text);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_poll_baselines_without_publishing() {
        let cb = InMemoryClipboard::new();
        cb.set_text("pre-existing secret").unwrap();
        let sync = ClipboardSync::default();
        assert_eq!(
            sync.poll_host_change(&cb),
            None,
            "must not leak pre-session content"
        );
        cb.set_text("new copy").unwrap();
        assert_eq!(sync.poll_host_change(&cb).as_deref(), Some("new copy"));
        // Unchanged → silent.
        assert_eq!(sync.poll_host_change(&cb), None);
    }

    #[test]
    fn viewer_apply_suppresses_echo() {
        let cb = InMemoryClipboard::new();
        let sync = ClipboardSync::default();
        assert!(sync.apply_from_viewer(&cb, "from tablet").unwrap());
        assert_eq!(cb.get_text().as_deref(), Some("from tablet"));
        // The applied text must not bounce back out.
        assert_eq!(sync.poll_host_change(&cb), None);
        // ...but a genuine host-side change after it still flows.
        cb.set_text("host copy").unwrap();
        assert_eq!(sync.poll_host_change(&cb).as_deref(), Some("host copy"));
    }

    #[test]
    fn oversized_payload_refused() {
        let cb = InMemoryClipboard::new();
        let sync = ClipboardSync::default();
        let big = "x".repeat(CLIPBOARD_MAX_BYTES + 1);
        assert!(!sync.apply_from_viewer(&cb, &big).unwrap());
        assert_eq!(cb.get_text(), None, "oversized text must not be applied");
    }
}
