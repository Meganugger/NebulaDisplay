//! Host clipboard bridge.
//!
//! * Windows: real clipboard via `CF_UNICODETEXT`, change detection via
//!   `GetClipboardSequenceNumber` (no polling of content, only the cheap
//!   sequence counter).
//! * Everywhere else: an in-memory backend so the whole sync path is
//!   exercisable in tests/CI (`set_external` simulates a local copy).
//!
//! Grant model mirrors input: a device's clipboard access is **denied until
//! the host user allows it** in the panel, and every payload is capped by
//! `clipboard_max_bytes` in both directions.

use std::sync::Arc;

/// One clipboard state, published on the state watch channel.
#[derive(Debug, Clone)]
pub struct ClipboardItem {
    /// Backend change counter (monotonic).
    pub seq: u64,
    pub text: String,
    /// `Some(device_id)` when a remote viewer set it — used to suppress
    /// echoing the item back to its origin.
    pub origin: Option<String>,
}

pub trait ClipboardBackend: Send + Sync {
    /// Current change counter (cheap; called by the watcher poll).
    fn change_seq(&self) -> u64;
    /// Read the clipboard text, if any.
    fn get_text(&self) -> Option<String>;
    /// Write the clipboard.
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}

pub fn create_backend() -> Arc<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        Arc::new(windows_impl::WindowsClipboard)
    }
    #[cfg(not(windows))]
    {
        Arc::new(MemoryClipboard::default())
    }
}

/// In-memory backend (non-Windows hosts + tests).
#[derive(Default)]
pub struct MemoryClipboard {
    inner: std::sync::Mutex<(u64, Option<String>)>,
}

impl MemoryClipboard {
    /// Test/simulation hook: behave as if the host user copied `text` locally.
    pub fn set_external(&self, text: &str) {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(text.to_string());
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn change_seq(&self) -> u64 {
        self.inner.lock().unwrap().0
    }
    fn get_text(&self) -> Option<String> {
        self.inner.lock().unwrap().1.clone()
    }
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(text.to_string());
        Ok(())
    }
}

#[cfg(windows)]
mod windows_impl {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    /// `CF_UNICODETEXT`
    const CF_UNICODETEXT: u32 = 13;

    pub struct WindowsClipboard;

    /// RAII open/close (the clipboard is a global lock — hold it briefly).
    struct Open;
    impl Open {
        fn new() -> Option<Self> {
            // SAFETY: plain API call; None = no owner window (we only do
            // immediate get/set operations).
            unsafe { OpenClipboard(None).ok().map(|_| Open) }
        }
    }
    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: paired with a successful OpenClipboard.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    impl super::ClipboardBackend for WindowsClipboard {
        fn change_seq(&self) -> u64 {
            // SAFETY: always safe; returns 0 when unavailable.
            unsafe { GetClipboardSequenceNumber() as u64 }
        }

        fn get_text(&self) -> Option<String> {
            let _open = Open::new()?;
            // SAFETY: clipboard is open; handle validity checked below.
            unsafe {
                let handle: HANDLE = GetClipboardData(CF_UNICODETEXT).ok()?;
                if handle.is_invalid() {
                    return None;
                }
                let hglobal = HGLOBAL(handle.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let text = String::from_utf16_lossy(slice);
                let _ = GlobalUnlock(hglobal);
                Some(text)
            }
        }

        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            let _open =
                Open::new().ok_or_else(|| anyhow::anyhow!("clipboard busy (OpenClipboard)"))?;
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: allocation sized to the wide string; ownership of the
            // HGLOBAL transfers to the system on SetClipboardData success.
            unsafe {
                EmptyClipboard()?;
                let hglobal = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
                let ptr = GlobalLock(hglobal) as *mut u16;
                anyhow::ensure!(!ptr.is_null(), "GlobalLock failed");
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(hglobal);
                SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0)))?;
            }
            Ok(())
        }
    }
}

/// Poll the backend's change counter and publish new clipboard text to the
/// state watch channel. ~2.5 Hz keeps sync latency invisible while the idle
/// cost stays one integer read per tick.
pub async fn watch_loop(state: Arc<crate::state::AppState>) {
    let mut last_seq = state.clipboard.change_seq();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(400));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if state.is_shutdown() {
            return;
        }
        let seq = state.clipboard.change_seq();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        // A change we caused ourselves (remote → host set_text) was already
        // published with its origin by the session; skip re-publishing.
        if state
            .clipboard_own_seq
            .swap(0, std::sync::atomic::Ordering::AcqRel)
            == seq
        {
            continue;
        }
        let Some(text) = state.clipboard.get_text() else {
            continue;
        };
        if text.len() > state.cfg.file.clipboard_max_bytes {
            tracing::debug!(
                len = text.len(),
                cap = state.cfg.file.clipboard_max_bytes,
                "host clipboard exceeds sync cap; not broadcast"
            );
            continue;
        }
        state.publish_clipboard(ClipboardItem {
            seq,
            text,
            origin: None,
        });
    }
}
