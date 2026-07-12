//! Clipboard sync: permission-gated, size-capped text clipboard bridging
//! between the host and granted viewers.
//!
//! Security model (see `docs/SECURITY.md`):
//! * **Deny by default** — like input, every device starts without the
//!   clipboard grant; the host user enables it per device from the panel.
//! * **Size-capped** — both directions enforce `clipboard_max_bytes`
//!   (config, default 256 KiB); oversized transfers are dropped, never
//!   truncated (a silently cut-off paste is worse than none).
//! * **Text only** — images/files are the separate file-drop feature.
//! * **No echo loops** — every publication carries its origin; the poller
//!   marks host-side writes as seen, and sessions skip their own events.
//!
//! Platform backends:
//! * Windows: Win32 clipboard (`CF_UNICODETEXT`), change detection via
//!   `GetClipboardSequenceNumber` so polling never opens the clipboard
//!   unless something actually changed.
//! * Everywhere else: an in-memory backend (the non-Windows host is a
//!   dev/test host; it has no desktop clipboard to bridge). Unit and E2E
//!   tests drive this backend.

use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use crate::state::AppState;

/// Host-side clipboard access. Implementations may block briefly.
pub trait ClipboardBackend: Send {
    /// Text if the clipboard changed since the last `poll_text` / `set_text`
    /// (None = unchanged or non-text content). First call returns the current
    /// contents, if any.
    fn poll_text(&mut self) -> anyhow::Result<Option<String>>;
    /// Current clipboard text without consuming the change flag.
    fn peek_text(&mut self) -> anyhow::Result<Option<String>>;
    /// Replace the clipboard contents. Marks the new value as seen so the
    /// next `poll_text` does not report our own write back to us.
    fn set_text(&mut self, text: &str) -> anyhow::Result<()>;
}

/// In-memory backend for non-Windows hosts and tests.
#[derive(Default)]
pub struct MemoryClipboard {
    text: Option<String>,
    seen: Option<String>,
}

impl ClipboardBackend for MemoryClipboard {
    fn poll_text(&mut self) -> anyhow::Result<Option<String>> {
        if self.text == self.seen {
            return Ok(None);
        }
        self.seen = self.text.clone();
        Ok(self.text.clone())
    }
    fn peek_text(&mut self) -> anyhow::Result<Option<String>> {
        Ok(self.text.clone())
    }
    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.text = Some(text.to_string());
        self.seen = self.text.clone();
        Ok(())
    }
}

impl MemoryClipboard {
    /// Test helper: simulate the *host user* copying something (leaves the
    /// change flag set so the poll loop picks it up and broadcasts).
    pub fn simulate_host_copy(&mut self, text: &str) {
        self.text = Some(text.to_string());
    }
}

/// Best backend for this platform.
pub fn create_backend() -> Box<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        return Box::new(windows_impl::WindowsClipboard::new());
    }
    #[cfg(not(windows))]
    Box::new(MemoryClipboard::default())
}

/// Poll the host clipboard and broadcast changes to granted sessions.
/// Cheap when idle: change detection is a sequence-number read on Windows,
/// and nothing is polled while no client is connected.
pub async fn run_poll_loop(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if state.is_shutdown() {
            return;
        }
        if state.clients.lock().unwrap().is_empty() {
            continue;
        }
        let polled = state.clipboard.lock().unwrap().poll_text();
        match polled {
            Ok(Some(text)) => {
                let cap = state.cfg.file.clipboard_max_bytes;
                if text.len() > cap {
                    debug!(
                        len = text.len(),
                        cap, "host clipboard exceeds cap; not broadcasting"
                    );
                    continue;
                }
                state.publish_clipboard(None, text);
            }
            Ok(None) => {}
            Err(e) => warn!("clipboard poll failed: {e:#}"),
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    //! Win32 `CF_UNICODETEXT` clipboard backend.

    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    /// RAII open/close so early returns can't leak the clipboard lock.
    struct OpenGuard;
    impl OpenGuard {
        fn open() -> anyhow::Result<Self> {
            // The clipboard is a shared resource; another app may hold it
            // briefly. A couple of short retries smooth over that.
            for attempt in 0..5 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Ok(Self);
                }
                std::thread::sleep(std::time::Duration::from_millis(5 * (attempt + 1)));
            }
            anyhow::bail!("OpenClipboard failed (held by another application)")
        }
    }
    impl Drop for OpenGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    pub struct WindowsClipboard {
        /// Last `GetClipboardSequenceNumber` we consumed or produced.
        last_seq: u32,
    }

    impl WindowsClipboard {
        pub fn new() -> Self {
            // Start "seen": don't broadcast whatever was copied before the
            // service started (it predates any viewer's grant).
            Self {
                last_seq: unsafe { GetClipboardSequenceNumber() },
            }
        }

        fn read_text() -> anyhow::Result<Option<String>> {
            if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_err() {
                return Ok(None); // non-text contents
            }
            let _guard = OpenGuard::open()?;
            let handle: HANDLE = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }
                .map_err(|e| anyhow::anyhow!("GetClipboardData: {e}"))?;
            let hglobal = HGLOBAL(handle.0);
            let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
            if ptr.is_null() {
                anyhow::bail!("GlobalLock returned null");
            }
            // CF_UNICODETEXT is NUL-terminated UTF-16.
            let mut len = 0usize;
            unsafe {
                while *ptr.add(len) != 0 {
                    len += 1;
                }
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            let text = String::from_utf16_lossy(slice);
            unsafe {
                let _ = GlobalUnlock(hglobal);
            }
            Ok(Some(text))
        }

        fn write_text(text: &str) -> anyhow::Result<()> {
            let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = utf16.len() * 2;
            let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
                .map_err(|e| anyhow::anyhow!("GlobalAlloc: {e}"))?;
            unsafe {
                let ptr = GlobalLock(hglobal) as *mut u16;
                if ptr.is_null() {
                    let _ = GlobalFree(Some(hglobal));
                    anyhow::bail!("GlobalLock returned null");
                }
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
                let _ = GlobalUnlock(hglobal);
            }
            let guard = OpenGuard::open();
            if guard.is_err() {
                unsafe {
                    let _ = GlobalFree(Some(hglobal));
                }
            }
            let _guard = guard?;
            unsafe {
                EmptyClipboard().map_err(|e| anyhow::anyhow!("EmptyClipboard: {e}"))?;
                // On success the system owns the allocation; on failure we
                // must free it ourselves.
                if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0))) {
                    let _ = GlobalFree(Some(hglobal));
                    anyhow::bail!("SetClipboardData: {e}");
                }
            }
            Ok(())
        }
    }

    impl super::ClipboardBackend for WindowsClipboard {
        fn poll_text(&mut self) -> anyhow::Result<Option<String>> {
            let seq = unsafe { GetClipboardSequenceNumber() };
            if seq == self.last_seq {
                return Ok(None);
            }
            self.last_seq = seq;
            Self::read_text()
        }
        fn peek_text(&mut self) -> anyhow::Result<Option<String>> {
            Self::read_text()
        }
        fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
            Self::write_text(text)?;
            // Mark our own write as seen so poll_text doesn't echo it back.
            self.last_seq = unsafe { GetClipboardSequenceNumber() };
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_poll_semantics() {
        let mut c = MemoryClipboard::default();
        assert_eq!(c.poll_text().unwrap(), None, "empty clipboard: no change");

        c.simulate_host_copy("hello");
        assert_eq!(c.poll_text().unwrap().as_deref(), Some("hello"));
        assert_eq!(c.poll_text().unwrap(), None, "change consumed");
        assert_eq!(
            c.peek_text().unwrap().as_deref(),
            Some("hello"),
            "peek does not consume"
        );

        // A client-driven set must not be reported back by the poller.
        c.set_text("from client").unwrap();
        assert_eq!(c.poll_text().unwrap(), None);
        assert_eq!(c.peek_text().unwrap().as_deref(), Some("from client"));

        c.simulate_host_copy("again");
        assert_eq!(c.poll_text().unwrap().as_deref(), Some("again"));
    }
}
