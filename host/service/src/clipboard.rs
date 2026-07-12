//! Host clipboard bridge for clipboard sync (text only in v1).
//!
//! Permission model mirrors input: **deny by default**, per-device grant
//! toggled in the control panel, revocable live. Payloads are capped at
//! [`ndsp_protocol::MAX_CLIPBOARD_BYTES`] in both directions (oversized
//! events are dropped, never truncated).
//!
//! * Windows: the Win32 clipboard (`CF_UNICODETEXT`), change detection via
//!   the cheap `GetClipboardSequenceNumber` poll (no hidden window needed).
//! * Non-Windows hosts: an in-memory clipboard, which doubles as the test
//!   backend the Linux CI e2e suite drives end-to-end.

use std::sync::Arc;
use tokio::time::Duration;

use crate::state::AppState;

pub trait ClipboardBackend: Send + Sync {
    /// Monotonic-ish value that changes whenever clipboard content changes
    /// (cheap to poll — no clipboard open, no allocation).
    fn sequence(&self) -> u64;
    /// Current clipboard text; `None` when empty / not text.
    fn get_text(&self) -> Option<String>;
    /// Replace the clipboard content with `text`.
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}

pub fn create_backend() -> Arc<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        Arc::new(windows_impl::WindowsClipboard)
    }
    #[cfg(not(windows))]
    {
        Arc::new(InMemoryClipboard::default())
    }
}

/// In-memory clipboard: non-Windows hosts + tests.
#[derive(Default)]
pub struct InMemoryClipboard {
    inner: std::sync::Mutex<(u64, String)>,
}

impl ClipboardBackend for InMemoryClipboard {
    fn sequence(&self) -> u64 {
        self.inner.lock().unwrap().0
    }
    fn get_text(&self) -> Option<String> {
        let g = self.inner.lock().unwrap();
        if g.0 == 0 {
            None
        } else {
            Some(g.1.clone())
        }
    }
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = text.to_string();
        Ok(())
    }
}

/// Poll the host clipboard for changes and publish new text into
/// `state.clipboard_tx` (a latest-only watch each session forwards from).
/// The sequence poll is ~free; text is only read when the sequence moved.
pub async fn run_clipboard_loop(state: Arc<AppState>) {
    let mut last_seq = state.clipboard.sequence();
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if state.is_shutdown() {
            return;
        }
        let seq = state.clipboard.sequence();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        let Some(text) = state.clipboard.get_text() else {
            continue;
        };
        if text.len() > ndsp_protocol::MAX_CLIPBOARD_BYTES {
            tracing::debug!(
                len = text.len(),
                "host clipboard exceeds sync cap; not forwarding"
            );
            continue;
        }
        state.publish_clipboard(text);
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
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    pub struct WindowsClipboard;

    /// The clipboard is a shared, briefly-lockable resource: retry opening a
    /// few times before giving up (another app may hold it for a moment).
    fn with_open_clipboard<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut opened = false;
        for _ in 0..5 {
            // SAFETY: plain Win32 call; None = current thread as owner.
            if unsafe { OpenClipboard(None) }.is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        anyhow::ensure!(opened, "clipboard busy (OpenClipboard failed)");
        let result = f();
        // SAFETY: balanced with the successful OpenClipboard above.
        unsafe {
            let _ = CloseClipboard();
        }
        result
    }

    impl super::ClipboardBackend for WindowsClipboard {
        fn sequence(&self) -> u64 {
            // SAFETY: no preconditions; documented as callable anytime.
            (unsafe { GetClipboardSequenceNumber() }) as u64
        }

        fn get_text(&self) -> Option<String> {
            with_open_clipboard(|| {
                // SAFETY: clipboard is open (guaranteed by the wrapper);
                // handle/lock lifetimes are contained to this closure.
                unsafe {
                    let handle: HANDLE = GetClipboardData(CF_UNICODETEXT.0 as u32)?;
                    let hglobal = HGLOBAL(handle.0);
                    let ptr = GlobalLock(hglobal) as *const u16;
                    anyhow::ensure!(!ptr.is_null(), "GlobalLock failed");
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    let _ = GlobalUnlock(hglobal);
                    Ok(text)
                }
            })
            .ok()
        }

        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            with_open_clipboard(|| {
                // SAFETY: clipboard open; on SetClipboardData success the
                // system owns the HGLOBAL (we must NOT free it), on failure
                // it stays ours but leaking a rare failure allocation is
                // preferable to a double-free.
                unsafe {
                    EmptyClipboard()?;
                    let hglobal = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
                    let ptr = GlobalLock(hglobal) as *mut u16;
                    anyhow::ensure!(!ptr.is_null(), "GlobalLock failed");
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                    let _ = GlobalUnlock(hglobal);
                    SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0)))?;
                    Ok(())
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_backend_tracks_sequence() {
        let c = InMemoryClipboard::default();
        assert_eq!(c.sequence(), 0);
        assert_eq!(c.get_text(), None);
        c.set_text("hello").unwrap();
        assert_eq!(c.sequence(), 1);
        assert_eq!(c.get_text().as_deref(), Some("hello"));
        c.set_text("world").unwrap();
        assert_eq!(c.sequence(), 2);
        assert_eq!(c.get_text().as_deref(), Some("world"));
    }
}
