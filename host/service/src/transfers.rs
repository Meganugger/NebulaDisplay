//! Viewer → host file drop (ROADMAP P2.10).
//!
//! Nothing touches the disk without an **explicit per-transfer accept** in
//! the control panel:
//!
//! ```text
//! viewer                      host session                 panel (user)
//!   FileOffer ───────────────▶ validate, queue offer ────▶ shows accept/deny
//!                                                          user clicks accept
//!   FileAnswer(accept) ◀────── SessionCommand ◀──────────── POST /api/transfers/answer
//!   FileChunk × N ────────────▶ write .part file (size-capped, in-order)
//!   FileEnd ──────────────────▶ verify sha256 → rename → FileDone
//! ```
//!
//! Safety properties: filenames are sanitized to a single path component,
//! sizes are capped (`max_file_mb`), exactly one active transfer per
//! session, offers expire, and a failed hash check deletes the partial file.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::state::SessionCommand;

/// Offers not answered in the panel within this window are auto-denied.
pub const OFFER_TTL: Duration = Duration::from_secs(120);

/// A file offer waiting for a panel decision.
pub struct PendingOffer {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    /// Already-sanitized filename (single path component).
    pub name: String,
    pub size_bytes: u64,
    pub sha256_hex: String,
    pub offered_at: Instant,
    pub offered_unix: u64,
    /// Command channel of the owning session (routes the answer back).
    pub session: mpsc::Sender<SessionCommand>,
}

/// Panel-facing view of a pending offer.
#[derive(Serialize)]
pub struct PendingOfferView {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub name: String,
    pub size_bytes: u64,
    pub offered_unix: u64,
    pub expires_in_secs: u64,
}

#[derive(Default)]
pub struct TransferManager {
    pending: Mutex<HashMap<String, PendingOffer>>,
}

impl TransferManager {
    /// Register a validated offer. Returns `false` (and the session should
    /// deny) when an offer with this id is already pending.
    pub fn register(&self, offer: PendingOffer) -> bool {
        let mut map = self.pending.lock().unwrap();
        Self::prune(&mut map);
        if map.contains_key(&offer.id) {
            return false;
        }
        info!(id = %offer.id, name = %offer.name, size = offer.size_bytes, from = %offer.device_name,
              "file offer queued — waiting for panel accept");
        map.insert(offer.id.clone(), offer);
        true
    }

    /// Panel decision. Routes the answer to the owning session. Returns
    /// `false` for unknown/expired ids.
    pub fn answer(&self, id: &str, accept: bool) -> bool {
        let offer = {
            let mut map = self.pending.lock().unwrap();
            Self::prune(&mut map);
            map.remove(id)
        };
        let Some(offer) = offer else {
            return false;
        };
        info!(id = %offer.id, accept, "file offer answered from panel");
        offer
            .session
            .try_send(SessionCommand::AnswerFileOffer {
                id: offer.id.clone(),
                accept,
                name: offer.name,
                size_bytes: offer.size_bytes,
                sha256_hex: offer.sha256_hex,
            })
            .is_ok()
    }

    /// Drop all pending offers belonging to a disconnecting session.
    pub fn drop_for_device(&self, device_id: &str) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, o| o.device_id != device_id);
    }

    pub fn list(&self) -> Vec<PendingOfferView> {
        let mut map = self.pending.lock().unwrap();
        Self::prune(&mut map);
        let mut v: Vec<PendingOfferView> = map
            .values()
            .map(|o| PendingOfferView {
                id: o.id.clone(),
                device_id: o.device_id.clone(),
                device_name: o.device_name.clone(),
                name: o.name.clone(),
                size_bytes: o.size_bytes,
                offered_unix: o.offered_unix,
                expires_in_secs: OFFER_TTL.saturating_sub(o.offered_at.elapsed()).as_secs(),
            })
            .collect();
        v.sort_by_key(|o| o.offered_unix);
        v
    }

    fn prune(map: &mut HashMap<String, PendingOffer>) {
        map.retain(|id, o| {
            let live = o.offered_at.elapsed() < OFFER_TTL;
            if !live {
                warn!(%id, "file offer expired without a panel answer; auto-denying");
                let _ = o.session.try_send(SessionCommand::AnswerFileOffer {
                    id: id.clone(),
                    accept: false,
                    name: o.name.clone(),
                    size_bytes: o.size_bytes,
                    sha256_hex: o.sha256_hex.clone(),
                });
            }
            live
        });
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Reduce an untrusted filename to one safe path component. Path separators
/// and traversal are stripped; control characters and Windows-reserved
/// characters are replaced; empty results get a placeholder.
pub fn sanitize_filename(name: &str) -> String {
    // Take the final path component whichever separator the sender used.
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches([' ', '.']).to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "unnamed-file".to_string()
    } else {
        trimmed
    }
}

/// Pick a non-clobbering destination path: `name`, `name (1)`, `name (2)`, …
pub fn unique_destination(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1..10_000 {
        let c = dir.join(format!("{stem} ({i}){ext}"));
        if !c.exists() {
            return c;
        }
    }
    dir.join(format!("{stem} ({}){ext}", now_unix()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_reduced_to_safe_components() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Windows\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("a/b/../c.txt"), "c.txt");
        assert_eq!(sanitize_filename("con<>:\"|?*.txt"), "con_______.txt");
        assert_eq!(sanitize_filename("..."), "unnamed-file");
        assert_eq!(sanitize_filename(""), "unnamed-file");
        assert_eq!(sanitize_filename("\u{7}bell.txt"), "_bell.txt");
        assert_eq!(sanitize_filename("trailing. "), "trailing");
    }

    #[test]
    fn unique_destination_avoids_clobbering() {
        let dir = std::env::temp_dir().join(format!("ndsp-xfer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = unique_destination(&dir, "file.txt");
        assert_eq!(p1.file_name().unwrap(), "file.txt");
        std::fs::write(&p1, b"x").unwrap();
        let p2 = unique_destination(&dir, "file.txt");
        assert_eq!(p2.file_name().unwrap(), "file (1).txt");
        std::fs::write(&p2, b"x").unwrap();
        let p3 = unique_destination(&dir, "file.txt");
        assert_eq!(p3.file_name().unwrap(), "file (2).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offers_expire_and_route_answers() {
        let mgr = TransferManager::default();
        let (tx, mut rx) = mpsc::channel(4);
        assert!(mgr.register(PendingOffer {
            id: "t1".into(),
            device_id: "dev".into(),
            device_name: "Tablet".into(),
            name: "a.bin".into(),
            size_bytes: 10,
            sha256_hex: "00".repeat(32),
            offered_at: Instant::now(),
            offered_unix: now_unix(),
            session: tx.clone(),
        }));
        // Duplicate id refused.
        assert!(!mgr.register(PendingOffer {
            id: "t1".into(),
            device_id: "dev".into(),
            device_name: "Tablet".into(),
            name: "b.bin".into(),
            size_bytes: 10,
            sha256_hex: "00".repeat(32),
            offered_at: Instant::now(),
            offered_unix: now_unix(),
            session: tx.clone(),
        }));
        assert_eq!(mgr.list().len(), 1);
        assert!(mgr.answer("t1", true));
        match rx.try_recv().unwrap() {
            SessionCommand::AnswerFileOffer { id, accept, .. } => {
                assert_eq!(id, "t1");
                assert!(accept);
            }
            other => panic!("unexpected command {other:?}"),
        }
        assert!(!mgr.answer("t1", true), "answered offers are gone");
        assert!(!mgr.answer("nope", true));
    }
}
