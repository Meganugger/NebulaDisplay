//! File-drop receive path (viewer → host).
//!
//! Safety model:
//! * Nothing is written to disk until the host user **explicitly accepts**
//!   the offer in the control panel (per transfer, with name + size shown).
//! * File names are sanitized to a single path component; collisions get a
//!   ` (n)` suffix instead of overwriting.
//! * A per-file size cap (`file_max_bytes`) is enforced at offer time *and*
//!   while streaming (a lying sender is cut off at the cap).
//! * The whole file is SHA-256-verified against the offer before the final
//!   rename — a partial or corrupted stream never lands under the real name.
//!
//! Chunks arrive on encrypted channel 4 (see `media::FileChunk`), strictly
//! sequential per transfer.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use ndsp_protocol::media::FileChunk;

/// Outcome of feeding one chunk.
#[derive(Debug, PartialEq)]
pub enum Progress {
    /// More chunks expected.
    Receiving { received: u64 },
    /// Final chunk received and hash verified; file is at `path`.
    Done { path: PathBuf },
}

struct ActiveTransfer {
    name: String,
    size: u64,
    sha256: [u8; 32],
    received: u64,
    hasher: Sha256,
    tmp_path: PathBuf,
    file: std::fs::File,
}

/// Per-session receiver: tracks offers the host user accepted.
pub struct FileReceiver {
    dir: PathBuf,
    max_bytes: u64,
    active: HashMap<u32, ActiveTransfer>,
    /// Offers seen (id → (name, size, sha256)) awaiting a panel decision.
    offered: HashMap<u32, (String, u64, [u8; 32])>,
}

impl FileReceiver {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            active: HashMap::new(),
            offered: HashMap::new(),
        }
    }

    /// Validate and register an offer. Returns the sanitized display name,
    /// or an error string suitable for `FileReject.reason`.
    pub fn offer(
        &mut self,
        transfer_id: u32,
        name: &str,
        size: u64,
        sha256_hex: &str,
    ) -> Result<String, String> {
        if self.active.contains_key(&transfer_id) {
            return Err("transfer id already active".into());
        }
        if size == 0 {
            return Err("empty file".into());
        }
        if size > self.max_bytes {
            return Err(format!(
                "file exceeds host cap ({} > {} bytes)",
                size, self.max_bytes
            ));
        }
        let clean = sanitize_name(name);
        if clean.is_empty() {
            return Err("unusable file name".into());
        }
        let mut hash = [0u8; 32];
        hex::decode_to_slice(sha256_hex, &mut hash).map_err(|_| "bad sha256".to_string())?;
        self.offered
            .insert(transfer_id, (clean.clone(), size, hash));
        Ok(clean)
    }

    /// Host user accepted → open the temp file. Errors are host-side (disk).
    pub fn accept(&mut self, transfer_id: u32) -> anyhow::Result<()> {
        let (name, size, sha256) = self
            .offered
            .remove(&transfer_id)
            .ok_or_else(|| anyhow::anyhow!("no such offer"))?;
        std::fs::create_dir_all(&self.dir)?;
        let tmp_path = self.dir.join(format!(
            ".ndsp-partial-{transfer_id}-{}",
            std::process::id()
        ));
        let file = std::fs::File::create(&tmp_path)?;
        self.active.insert(
            transfer_id,
            ActiveTransfer {
                name,
                size,
                sha256,
                received: 0,
                hasher: Sha256::new(),
                tmp_path,
                file,
            },
        );
        Ok(())
    }

    /// Host user rejected (or the offer went stale).
    pub fn reject(&mut self, transfer_id: u32) {
        self.offered.remove(&transfer_id);
        self.cancel(transfer_id);
    }

    /// Drop an in-flight transfer and its partial file.
    pub fn cancel(&mut self, transfer_id: u32) {
        if let Some(t) = self.active.remove(&transfer_id) {
            drop(t.file);
            let _ = std::fs::remove_file(&t.tmp_path);
        }
    }

    /// True if this id was accepted and is currently streaming.
    pub fn is_active(&self, transfer_id: u32) -> bool {
        self.active.contains_key(&transfer_id)
    }

    /// Feed one chunk. On any error the transfer is cancelled and the
    /// partial file removed.
    pub fn chunk(&mut self, c: &FileChunk) -> Result<Progress, String> {
        let Some(t) = self.active.get_mut(&c.transfer_id) else {
            return Err("chunk for inactive transfer".into());
        };
        if c.offset != t.received {
            let id = c.transfer_id;
            self.cancel(id);
            return Err("out-of-order chunk".into());
        }
        if t.received + c.data.len() as u64 > t.size {
            let id = c.transfer_id;
            self.cancel(id);
            return Err("more data than offered".into());
        }
        if let Err(e) = t.file.write_all(&c.data) {
            let id = c.transfer_id;
            self.cancel(id);
            return Err(format!("write failed: {e}"));
        }
        t.hasher.update(&c.data);
        t.received += c.data.len() as u64;
        if t.received < t.size {
            return Ok(Progress::Receiving {
                received: t.received,
            });
        }
        // Complete: verify hash, then move into place.
        let t = self.active.remove(&c.transfer_id).expect("checked above");
        let digest: [u8; 32] = t.hasher.finalize().into();
        if digest != t.sha256 {
            let _ = std::fs::remove_file(&t.tmp_path);
            return Err("sha256 mismatch — file discarded".into());
        }
        if let Err(e) = t.file.sync_all() {
            let _ = std::fs::remove_file(&t.tmp_path);
            return Err(format!("sync failed: {e}"));
        }
        drop(t.file);
        let final_path = unique_path(&self.dir, &t.name);
        match std::fs::rename(&t.tmp_path, &final_path) {
            Ok(()) => Ok(Progress::Done { path: final_path }),
            Err(e) => {
                let _ = std::fs::remove_file(&t.tmp_path);
                Err(format!("finalize failed: {e}"))
            }
        }
    }
}

impl Drop for FileReceiver {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.active.keys().copied().collect();
        for id in ids {
            self.cancel(id);
        }
    }
}

/// Reduce an arbitrary client-supplied name to one safe path component.
fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('.');
    let cleaned: String = base
        .chars()
        .map(|c| {
            // Windows-reserved + control chars → underscore.
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(200)
        .collect();
    cleaned.trim().to_string()
}

/// `name`, `name (1)`, `name (2)`, … until unused.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

/// The directory accepted files land in.
pub fn download_dir(cfg: &crate::config::Config) -> PathBuf {
    cfg.file
        .file_dir
        .clone()
        .unwrap_or_else(|| cfg.data_dir.join("downloads"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ndsp-filedrop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha_hex(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    #[test]
    fn full_transfer_roundtrip() {
        let dir = tmpdir("ok");
        let mut rx = FileReceiver::new(dir.clone(), 1024 * 1024);
        let data = vec![7u8; 100_000];
        rx.offer(1, "report.pdf", data.len() as u64, &sha_hex(&data))
            .unwrap();
        rx.accept(1).unwrap();
        let mut off = 0u64;
        let mut last = None;
        for part in data.chunks(16_384) {
            last = Some(
                rx.chunk(&FileChunk {
                    transfer_id: 1,
                    offset: off,
                    data: part.to_vec(),
                })
                .unwrap(),
            );
            off += part.len() as u64;
        }
        let Some(Progress::Done { path }) = last else {
            panic!("expected Done, got {last:?}");
        };
        assert_eq!(path.file_name().unwrap(), "report.pdf");
        assert_eq!(std::fs::read(&path).unwrap(), data);
        // No stray partial files.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn hash_mismatch_discards_file() {
        let dir = tmpdir("badhash");
        let mut rx = FileReceiver::new(dir.clone(), 1024);
        let data = b"hello world".to_vec();
        rx.offer(1, "a.txt", data.len() as u64, &sha_hex(b"different"))
            .unwrap();
        rx.accept(1).unwrap();
        let err = rx
            .chunk(&FileChunk {
                transfer_id: 1,
                offset: 0,
                data,
            })
            .unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn oversize_and_overflow_rejected() {
        let dir = tmpdir("size");
        let mut rx = FileReceiver::new(dir.clone(), 10);
        assert!(rx.offer(1, "big.bin", 11, &sha_hex(b"x")).is_err());
        // Lying sender: offered 5 bytes, streams 6.
        let mut rx = FileReceiver::new(dir, 100);
        rx.offer(2, "b.bin", 5, &sha_hex(b"xxxxx")).unwrap();
        rx.accept(2).unwrap();
        let err = rx
            .chunk(&FileChunk {
                transfer_id: 2,
                offset: 0,
                data: vec![0u8; 6],
            })
            .unwrap_err();
        assert!(err.contains("more data"), "{err}");
        assert!(!rx.is_active(2));
    }

    #[test]
    fn name_sanitization_and_collisions() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("C:\\Users\\x\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_name("a<b>:c.txt"), "a_b__c.txt");
        assert_eq!(sanitize_name("..hidden"), "hidden");
        assert!(sanitize_name("...").is_empty());

        let dir = tmpdir("collide");
        std::fs::write(dir.join("f.txt"), b"1").unwrap();
        assert_eq!(unique_path(&dir, "f.txt").file_name().unwrap(), "f (1).txt");
    }

    #[test]
    fn out_of_order_chunk_cancels() {
        let dir = tmpdir("ooo");
        let mut rx = FileReceiver::new(dir, 100);
        rx.offer(1, "x.bin", 10, &sha_hex(&[0u8; 10])).unwrap();
        rx.accept(1).unwrap();
        assert!(rx
            .chunk(&FileChunk {
                transfer_id: 1,
                offset: 5,
                data: vec![0u8; 5],
            })
            .is_err());
        assert!(!rx.is_active(1));
    }

    #[test]
    fn chunks_require_acceptance() {
        let dir = tmpdir("noaccept");
        let mut rx = FileReceiver::new(dir, 100);
        rx.offer(1, "x.bin", 10, &sha_hex(&[0u8; 10])).unwrap();
        // No accept → chunks refused.
        assert!(rx
            .chunk(&FileChunk {
                transfer_id: 1,
                offset: 0,
                data: vec![0u8; 10],
            })
            .is_err());
    }
}
