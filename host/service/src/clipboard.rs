//! Host clipboard bridge (text), permission-gated per device.
//!
//! Grant model mirrors input: **deny by default**, toggled per device in the
//! panel, live-revocable. Data flow:
//!
//! ```text
//! viewer paste ─► session pump ─► apply_remote() ─► OS clipboard
//!                                        │
//!                                        └► publish ─► every *other* granted
//!                                                      session (watch channel)
//! host user copies ─► watcher (Windows poll) ─► publish ─► granted sessions
//! ```
//!
//! * Size cap: [`ndsp_protocol::MAX_CLIPBOARD_TEXT_BYTES`] enforced before
//!   anything touches the OS clipboard or the wire.
//! * Echo suppression: updates carry the originating session id, so the
//!   pushing viewer never receives its own text back; every *other* granted
//!   viewer does (that is the sync working as intended).
//! * Windows reads/writes `CF_UNICODETEXT`; change detection polls
//!   `GetClipboardSequenceNumber` (cheap — no window/message loop needed).
//! * Non-Windows hosts keep an in-process clipboard so the full protocol
//!   path is exercised by tests and non-Windows deployments degrade
//!   gracefully instead of failing.
//!
//! Clipboard *contents* are never logged — only sizes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::debug;
#[cfg(windows)]
use tracing::warn;

/// One clipboard change, shared zero-copy between sessions.
#[derive(Debug, Clone)]
pub struct ClipboardUpdate {
    /// Monotonic change counter (0 = never).
    pub seq: u64,
    /// Session client-id that pushed this text; `None` = host-local copy.
    pub origin: Option<u64>,
    pub text: Arc<str>,
}

pub struct ClipboardService {
    tx: watch::Sender<Option<Arc<ClipboardUpdate>>>,
    seq: AtomicU64,
    /// Last text applied/observed — dedupes watcher noise and remote echos.
    last: Mutex<Option<Arc<str>>>,
    /// Non-Windows in-process clipboard store.
    #[cfg(not(windows))]
    store: Mutex<Option<Arc<str>>>,
    /// OS clipboard sequence number of our own writes (echo suppression for
    /// the poll watcher).
    #[cfg(windows)]
    own_os_seq: std::sync::atomic::AtomicU32,
}

impl Default for ClipboardService {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardService {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(None);
        Self {
            tx,
            seq: AtomicU64::new(0),
            last: Mutex::new(None),
            #[cfg(not(windows))]
            store: Mutex::new(None),
            #[cfg(windows)]
            own_os_seq: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Sessions watch this for host-side (or other-viewer) clipboard changes.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<ClipboardUpdate>>> {
        self.tx.subscribe()
    }

    /// A granted viewer pushed clipboard text: apply it to the host clipboard
    /// and fan it out to every other granted session. Size is checked by the
    /// caller (session pump) against the protocol cap.
    pub fn apply_remote(&self, origin: u64, text: String) {
        let text: Arc<str> = text.into();
        {
            let mut last = self.last.lock().unwrap();
            if last.as_deref() == Some(&*text) {
                return; // no-op change; avoid publish loops
            }
            *last = Some(text.clone());
        }
        debug!(bytes = text.len(), origin, "clipboard from viewer");
        self.set_os_clipboard(&text);
        self.publish(Some(origin), text);
    }

    /// The host user copied something locally (watcher / tests).
    pub fn publish_local(&self, text: String) {
        let text: Arc<str> = text.into();
        {
            let mut last = self.last.lock().unwrap();
            if last.as_deref() == Some(&*text) {
                return;
            }
            *last = Some(text.clone());
        }
        // On Windows the OS clipboard already holds this text (that's how
        // the watcher noticed); keep the in-process store consistent too.
        #[cfg(not(windows))]
        {
            *self.store.lock().unwrap() = Some(text.clone());
        }
        self.publish(None, text);
    }

    fn publish(&self, origin: Option<u64>, text: Arc<str>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self
            .tx
            .send(Some(Arc::new(ClipboardUpdate { seq, origin, text })));
    }

    /// Current host clipboard text (None when empty/non-text).
    pub fn host_text(&self) -> Option<String> {
        #[cfg(windows)]
        {
            windows_clip::get_text()
        }
        #[cfg(not(windows))]
        {
            self.store.lock().unwrap().as_deref().map(str::to_owned)
        }
    }

    fn set_os_clipboard(&self, text: &str) {
        #[cfg(windows)]
        {
            match windows_clip::set_text(text) {
                Ok(()) => {
                    // Remember the OS sequence number of our own write so the
                    // watcher doesn't re-publish it as a host-local change.
                    self.own_os_seq
                        .store(windows_clip::sequence_number(), Ordering::Relaxed);
                }
                Err(e) => warn!("clipboard write failed: {e:#}"),
            }
        }
        #[cfg(not(windows))]
        {
            *self.store.lock().unwrap() = Some(Arc::from(text));
        }
    }

    /// Watch the OS clipboard for host-local changes and publish them.
    /// Windows-only (poll on `GetClipboardSequenceNumber`); a no-op elsewhere
    /// (the in-process store only changes through this service anyway).
    pub fn spawn_watcher(self: &Arc<Self>) {
        #[cfg(windows)]
        {
            let this = Arc::downgrade(self);
            std::thread::Builder::new()
                .name("clipboard-watch".into())
                .spawn(move || {
                    let mut last_seq = windows_clip::sequence_number();
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        let Some(this) = this.upgrade() else { return };
                        let seq = windows_clip::sequence_number();
                        if seq == last_seq {
                            continue;
                        }
                        last_seq = seq;
                        if seq == this.own_os_seq.load(Ordering::Relaxed) {
                            continue; // our own write echoing back
                        }
                        let Some(text) = windows_clip::get_text() else {
                            continue; // empty or non-text clipboard
                        };
                        if text.len() > ndsp_protocol::MAX_CLIPBOARD_TEXT_BYTES {
                            debug!(bytes = text.len(), "host clipboard too large to sync");
                            continue;
                        }
                        this.publish_local(text);
                    }
                })
                .expect("spawn clipboard watcher");
        }
    }
}

#[cfg(windows)]
mod windows_clip {
    //! `CF_UNICODETEXT` read/write via the raw Win32 clipboard API.

    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    pub fn sequence_number() -> u32 {
        // SAFETY: no preconditions.
        unsafe { GetClipboardSequenceNumber() }
    }

    /// RAII open/close so early returns can't leak the clipboard lock.
    struct OpenGuard;
    impl OpenGuard {
        fn acquire() -> Option<Self> {
            // SAFETY: passing no owner window is valid for read/write.
            unsafe { OpenClipboard(None).ok().map(|_| OpenGuard) }
        }
    }
    impl Drop for OpenGuard {
        fn drop(&mut self) {
            // SAFETY: guard exists only after a successful OpenClipboard.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    pub fn get_text() -> Option<String> {
        // SAFETY: standard clipboard read sequence; the guard keeps the
        // clipboard open (and thus the handle valid) while we copy.
        unsafe {
            IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
            let _guard = OpenGuard::acquire()?;
            let handle: HANDLE = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
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

    pub fn set_text(text: &str) -> anyhow::Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: standard clipboard write sequence. The HGLOBAL is handed
        // to the system on success (SetClipboardData takes ownership).
        unsafe {
            let _guard =
                OpenGuard::acquire().ok_or_else(|| anyhow::anyhow!("OpenClipboard failed"))?;
            EmptyClipboard().map_err(|e| anyhow::anyhow!("EmptyClipboard: {e}"))?;
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)
                .map_err(|e| anyhow::anyhow!("GlobalAlloc: {e}"))?;
            let dst = GlobalLock(hglobal) as *mut u16;
            if dst.is_null() {
                anyhow::bail!("GlobalLock failed");
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(hglobal);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0)))
                .map_err(|e| anyhow::anyhow!("SetClipboardData: {e}"))?;
            Ok(())
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_apply_publishes_to_others_not_origin_logic() {
        let svc = Arc::new(ClipboardService::new());
        let mut rx = svc.subscribe();
        svc.apply_remote(7, "hello".into());
        rx.changed().await.unwrap();
        let up = rx.borrow_and_update().clone().unwrap();
        assert_eq!(up.origin, Some(7));
        assert_eq!(&*up.text, "hello");
        assert_eq!(svc.host_text().as_deref(), Some("hello"));

        // Duplicate content is not re-published.
        svc.apply_remote(9, "hello".into());
        assert!(!rx.has_changed().unwrap());

        // Host-local copy publishes with no origin.
        svc.publish_local("from host".into());
        rx.changed().await.unwrap();
        let up = rx.borrow_and_update().clone().unwrap();
        assert_eq!(up.origin, None);
        assert_eq!(&*up.text, "from host");
    }
}
