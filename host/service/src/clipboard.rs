//! Host clipboard bridge (text only).
//!
//! Grant model mirrors input injection: clipboard traffic is only accepted
//! from / pushed to a device while its **clipboard grant** is on (deny by
//! default, toggled live in the panel). Payloads are capped at
//! [`ndsp_protocol::MAX_CLIPBOARD_BYTES`] in both directions.
//!
//! * Windows: real clipboard via `CF_UNICODETEXT`, change detection via
//!   `GetClipboardSequenceNumber` (cheap, no polling of contents).
//! * Non-Windows hosts: an in-memory backend — it makes the whole pipeline
//!   (grants, caps, echo suppression, wire format) testable in CI and on
//!   dev machines, exactly like the test-pattern capture source.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::debug;

use crate::state::AppState;

pub trait ClipboardBackend: Send + Sync {
    /// Current clipboard text, if the clipboard holds text.
    fn get_text(&self) -> Option<String>;
    /// Replace the clipboard contents with `text`.
    fn set_text(&self, text: &str);
    /// Monotonic-ish change counter; any change to the clipboard (by anyone)
    /// changes the value. Used for cheap change detection.
    fn sequence(&self) -> u64;
}

pub fn create_backend() -> Arc<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        Arc::new(windows_clipboard::WindowsClipboard)
    }
    #[cfg(not(windows))]
    {
        Arc::new(MemoryClipboard::default())
    }
}

/// In-memory clipboard used on non-Windows hosts and by the integration
/// tests (which drive it through [`AppState::clipboard`]).
#[derive(Default)]
pub struct MemoryClipboard {
    text: Mutex<Option<String>>,
    seq: AtomicU64,
}

impl ClipboardBackend for MemoryClipboard {
    fn get_text(&self) -> Option<String> {
        self.text.lock().unwrap().clone()
    }
    fn set_text(&self, text: &str) {
        *self.text.lock().unwrap() = Some(text.to_string());
        self.seq.fetch_add(1, Ordering::Relaxed);
    }
    fn sequence(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

/// Latest host-clipboard text published to sessions. `seq` deduplicates —
/// sessions remember the last seq they forwarded.
#[derive(Debug, Clone)]
pub struct ClipboardUpdate {
    pub seq: u64,
    pub text: Arc<String>,
}

/// Watch the host clipboard and publish text changes to sessions. Change
/// detection is sequence-number based, so the loop reads clipboard *contents*
/// only when something actually changed.
pub async fn run_clipboard_watch(state: Arc<AppState>) {
    let mut last_seq = state.clipboard.sequence();
    loop {
        if state.is_shutdown() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let seq = state.clipboard.sequence();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        let Some(text) = state.clipboard.get_text() else {
            continue; // non-text contents (image, files, …) are never synced
        };
        if text.len() > ndsp_protocol::MAX_CLIPBOARD_BYTES {
            debug!(
                len = text.len(),
                "host clipboard too large to sync; skipping"
            );
            continue;
        }
        let _ = state.clipboard_tx.send(Some(Arc::new(ClipboardUpdate {
            seq,
            text: Arc::new(text),
        })));
    }
}

#[cfg(windows)]
mod windows_clipboard {
    use super::ClipboardBackend;
    use std::time::Duration;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    pub struct WindowsClipboard;

    /// The clipboard is a globally contended resource: another process may
    /// hold it for a few ms. Retry briefly instead of failing.
    fn open_clipboard_retry() -> bool {
        for _ in 0..5 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    impl ClipboardBackend for WindowsClipboard {
        fn get_text(&self) -> Option<String> {
            unsafe {
                if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
                    return None;
                }
                if !open_clipboard_retry() {
                    return None;
                }
                let result = (|| {
                    let handle: HANDLE = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                    let hglobal = HGLOBAL(handle.0);
                    let ptr = GlobalLock(hglobal) as *const u16;
                    if ptr.is_null() {
                        return None;
                    }
                    // CF_UNICODETEXT is NUL-terminated UTF-16.
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    let _ = GlobalUnlock(hglobal);
                    Some(text)
                })();
                let _ = CloseClipboard();
                result
            }
        }

        fn set_text(&self, text: &str) {
            let mut utf16: Vec<u16> = text.encode_utf16().collect();
            utf16.push(0);
            unsafe {
                if !open_clipboard_retry() {
                    return;
                }
                let ok = (|| {
                    EmptyClipboard().ok()?;
                    let bytes = std::mem::size_of_val(utf16.as_slice());
                    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
                    let ptr = GlobalLock(hglobal) as *mut u16;
                    if ptr.is_null() {
                        return None;
                    }
                    std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
                    let _ = GlobalUnlock(hglobal);
                    // On success the system owns the HGLOBAL.
                    SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0))).ok()
                })();
                if ok.is_none() {
                    tracing::debug!("SetClipboardData failed");
                }
                let _ = CloseClipboard();
            }
        }

        fn sequence(&self) -> u64 {
            unsafe { GetClipboardSequenceNumber() as u64 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_tracks_sequence() {
        let c = MemoryClipboard::default();
        assert_eq!(c.get_text(), None);
        let s0 = c.sequence();
        c.set_text("hello");
        assert_eq!(c.get_text().as_deref(), Some("hello"));
        assert_ne!(c.sequence(), s0);
    }
}
