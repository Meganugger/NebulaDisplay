//! Windows clipboard integration: CF_UNICODETEXT read/write + a sequence-
//! number poll watcher (no hidden window / message pump required).

use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use crate::state::AppState;

use super::ClipboardSetter;

/// RAII open/close so early returns can't leak the clipboard lock.
struct OpenGuard;

impl OpenGuard {
    fn open() -> anyhow::Result<Self> {
        // The clipboard is a contended global resource; retry briefly.
        for attempt in 0..5 {
            // SAFETY: plain Win32 call; None = no owner window.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(10 << attempt));
        }
        anyhow::bail!("OpenClipboard failed (clipboard busy)");
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: balanced with the successful OpenClipboard above.
        let _ = unsafe { CloseClipboard() };
    }
}

pub(super) struct WindowsSetter;

impl ClipboardSetter for WindowsSetter {
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * 2;
        // SAFETY: standard CF_UNICODETEXT publish sequence. The HGLOBAL is
        // allocated movable, filled while locked, then ownership transfers
        // to the system on successful SetClipboardData.
        unsafe {
            let _guard = OpenGuard::open()?;
            EmptyClipboard()?;
            let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let ptr = GlobalLock(hglobal) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hglobal));
                anyhow::bail!("GlobalLock failed");
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hglobal);
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0))).is_err() {
                let _ = GlobalFree(Some(hglobal));
                anyhow::bail!("SetClipboardData failed");
            }
        }
        Ok(())
    }
}

fn read_text() -> Option<String> {
    // SAFETY: standard CF_UNICODETEXT read sequence; the returned handle is
    // owned by the clipboard and only borrowed while locked.
    unsafe {
        let _guard = OpenGuard::open().ok()?;
        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

/// Poll the clipboard sequence number and publish text changes.
pub(super) fn spawn_watcher(state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("ndsp-clipboard".into())
        .spawn(move || {
            // SAFETY: GetClipboardSequenceNumber is always safe to call.
            let mut last_seq = unsafe { GetClipboardSequenceNumber() };
            loop {
                if state.is_shutdown() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(400));
                // SAFETY: as above.
                let seq = unsafe { GetClipboardSequenceNumber() };
                if seq == last_seq {
                    continue;
                }
                last_seq = seq;
                if let Some(text) = read_text() {
                    if text.len() <= ndsp_protocol::messages::MAX_CLIPBOARD_BYTES {
                        state.clipboard.publish_from_host(text);
                    } else {
                        tracing::debug!(
                            len = text.len(),
                            "host clipboard change exceeds size cap; not broadcast"
                        );
                    }
                }
            }
        })
        .expect("spawn clipboard watcher");
}
