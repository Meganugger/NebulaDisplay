//! Host→viewer file receive for the desktop viewer (ROADMAP P2.15).
//!
//! Opt-in: files are only accepted when the viewer was started with
//! `--receive-dir <dir>`; every offer outside that (or beyond the size cap)
//! is declined. Files are written as `.part` and only renamed into place
//! after the size and sha256 both verify.

use base64::Engine as _;
use ndsp_protocol::files::{sanitize_filename, unique_destination};
use ndsp_protocol::messages::{ControlMsg, FILE_CHUNK_BYTES};
use sha2::Digest as _;
use std::io::Write as _;
use std::path::PathBuf;

/// Hard cap on a single accepted file (matches the host's default).
const MAX_RECEIVE_BYTES: u64 = 2048 * 1024 * 1024;

struct Active {
    id: String,
    file: std::fs::File,
    part_path: PathBuf,
    final_name: String,
    expected_size: u64,
    expected_sha256: String,
    hasher: sha2::Sha256,
    received: u64,
    next_seq: u32,
}

impl Active {
    fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.part_path);
    }
}

/// Drives at most one inbound transfer. Feed every control message through
/// [`FileReceiver::handle`]; it returns messages to send back to the host.
pub struct FileReceiver {
    dir: Option<PathBuf>,
    active: Option<Active>,
}

/// Outcome of handling one control message.
pub enum Handled {
    /// Not a transfer message — caller processes it.
    NotMine(ControlMsg),
    /// Consumed; optionally reply, optionally surface a status line.
    Consumed {
        reply: Option<ControlMsg>,
        status: Option<String>,
    },
}

impl FileReceiver {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, active: None }
    }

    pub fn handle(&mut self, msg: ControlMsg) -> Handled {
        match msg {
            ControlMsg::FileOffer {
                id,
                name,
                size_bytes,
                sha256,
            } => self.on_offer(id, name, size_bytes, sha256),
            ControlMsg::FileChunk { id, seq, data }
                if self.active.as_ref().is_some_and(|a| a.id == id) =>
            {
                self.on_chunk(id, seq, &data)
            }
            ControlMsg::FileEnd { id } if self.active.as_ref().is_some_and(|a| a.id == id) => {
                self.on_end(id)
            }
            ControlMsg::FileAbort { id, reason }
                if self.active.as_ref().is_some_and(|a| a.id == id) =>
            {
                if let Some(a) = self.active.take() {
                    a.discard();
                }
                Handled::Consumed {
                    reply: None,
                    status: Some(format!("file receive aborted by host: {reason}")),
                }
            }
            other => Handled::NotMine(other),
        }
    }

    /// Discard any in-flight transfer (session teardown).
    pub fn reset(&mut self) {
        if let Some(a) = self.active.take() {
            a.discard();
        }
    }

    fn decline(id: String, reason: &str) -> Handled {
        Handled::Consumed {
            reply: Some(ControlMsg::FileAnswer {
                id,
                accept: false,
                reason: Some(reason.into()),
            }),
            status: None,
        }
    }

    fn on_offer(&mut self, id: String, name: String, size_bytes: u64, sha256: String) -> Handled {
        let Some(dir) = self.dir.clone() else {
            return Self::decline(id, "file receive not enabled (run with --receive-dir)");
        };
        if self.active.is_some() {
            return Self::decline(id, "another transfer is in progress");
        }
        if size_bytes == 0 || size_bytes > MAX_RECEIVE_BYTES {
            return Self::decline(id, "size out of bounds for this viewer");
        }
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Self::decline(id, "malformed offer");
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("cannot create receive dir: {e:#}");
            return Self::decline(id, "viewer storage error");
        }
        let final_name = sanitize_filename(&name);
        let part_path = dir.join(format!(
            ".{}.part",
            id.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(64)
                .collect::<String>()
        ));
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("cannot open {}: {e:#}", part_path.display());
                return Self::decline(id, "viewer storage error");
            }
        };
        tracing::info!(%id, name = %final_name, size_bytes, "accepting file from host");
        let status = format!("receiving {final_name}…");
        self.active = Some(Active {
            id: id.clone(),
            file,
            part_path,
            final_name,
            expected_size: size_bytes,
            expected_sha256: sha256.to_ascii_lowercase(),
            hasher: sha2::Sha256::new(),
            received: 0,
            next_seq: 0,
        });
        Handled::Consumed {
            reply: Some(ControlMsg::FileAnswer {
                id,
                accept: true,
                reason: None,
            }),
            status: Some(status),
        }
    }

    fn on_chunk(&mut self, id: String, seq: u32, data_b64: &str) -> Handled {
        let a = self.active.as_mut().expect("checked by caller");
        let err: Option<String> = (|| {
            if seq != a.next_seq {
                return Some(format!(
                    "out-of-order chunk (expected {}, got {seq})",
                    a.next_seq
                ));
            }
            let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(d) => d,
                Err(_) => return Some("bad chunk encoding".into()),
            };
            if data.is_empty() || data.len() > FILE_CHUNK_BYTES {
                return Some("chunk size out of bounds".into());
            }
            if a.received + data.len() as u64 > a.expected_size {
                return Some("more data than offered".into());
            }
            if let Err(e) = a.file.write_all(&data) {
                return Some(format!("write failed: {e}"));
            }
            a.hasher.update(&data);
            a.received += data.len() as u64;
            a.next_seq += 1;
            None
        })();
        match err {
            None => Handled::Consumed {
                reply: None,
                status: None,
            },
            Some(reason) => {
                if let Some(a) = self.active.take() {
                    a.discard();
                }
                Handled::Consumed {
                    reply: Some(ControlMsg::FileAbort {
                        id,
                        reason: reason.clone(),
                    }),
                    status: Some(format!("file receive failed: {reason}")),
                }
            }
        }
    }

    fn on_end(&mut self, id: String) -> Handled {
        let mut a = self.active.take().expect("checked by caller");
        let fail = |a: Active, id: String, reason: String| {
            a.discard();
            Handled::Consumed {
                reply: Some(ControlMsg::FileAbort {
                    id,
                    reason: reason.clone(),
                }),
                status: Some(format!("file receive failed: {reason}")),
            }
        };
        if a.received != a.expected_size {
            let r = format!(
                "size mismatch ({} of {} bytes)",
                a.received, a.expected_size
            );
            return fail(a, id, r);
        }
        let digest = hex::encode(std::mem::take(&mut a.hasher).finalize());
        if digest != a.expected_sha256 {
            return fail(a, id, "sha256 mismatch — file corrupted in transit".into());
        }
        if let Err(e) = a.file.flush().and_then(|_| a.file.sync_all()) {
            let r = format!("flush failed: {e}");
            return fail(a, id, r);
        }
        let dir = a.part_path.parent().map(PathBuf::from).unwrap_or_default();
        let dest = unique_destination(&dir, &a.final_name);
        drop(a.file);
        match std::fs::rename(&a.part_path, &dest) {
            Ok(()) => {
                tracing::info!(path = %dest.display(), "file received and verified");
                Handled::Consumed {
                    reply: Some(ControlMsg::FileDone { id }),
                    status: Some(format!("received {} ✓", dest.display())),
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&a.part_path);
                Handled::Consumed {
                    reply: Some(ControlMsg::FileAbort {
                        id,
                        reason: format!("rename failed: {e}"),
                    }),
                    status: Some("file receive failed: rename".into()),
                }
            }
        }
    }
}

impl Drop for FileReceiver {
    /// Never leave `.part` litter behind, however the session ends.
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ndsp-recv-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn offer(id: &str, name: &str, data: &[u8]) -> ControlMsg {
        ControlMsg::FileOffer {
            id: id.into(),
            name: name.into(),
            size_bytes: data.len() as u64,
            sha256: hex::encode(sha2::Sha256::digest(data)),
        }
    }

    fn chunk(id: &str, seq: u32, data: &[u8]) -> ControlMsg {
        ControlMsg::FileChunk {
            id: id.into(),
            seq,
            data: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    #[test]
    fn declines_when_not_enabled() {
        let mut r = FileReceiver::new(None);
        match r.handle(offer("t1", "a.txt", b"hello")) {
            Handled::Consumed {
                reply: Some(ControlMsg::FileAnswer { accept, .. }),
                ..
            } => assert!(!accept),
            _ => panic!("expected a decline"),
        }
    }

    #[test]
    fn receives_and_verifies_a_file() {
        let dir = tmp();
        let mut r = FileReceiver::new(Some(dir.clone()));
        let data = b"the quick brown fox".to_vec();
        match r.handle(offer("t2", "../evil/../fox.txt", &data)) {
            Handled::Consumed {
                reply: Some(ControlMsg::FileAnswer { accept, .. }),
                ..
            } => assert!(accept),
            _ => panic!("expected accept"),
        }
        for (i, part) in data.chunks(7).enumerate() {
            match r.handle(chunk("t2", i as u32, part)) {
                Handled::Consumed { reply: None, .. } => {}
                _ => panic!("chunk {i} should be silently consumed"),
            }
        }
        match r.handle(ControlMsg::FileEnd { id: "t2".into() }) {
            Handled::Consumed {
                reply: Some(ControlMsg::FileDone { id }),
                ..
            } => assert_eq!(id, "t2"),
            _ => panic!("expected FileDone"),
        }
        // Path traversal in the offered name was neutralized.
        assert_eq!(std::fs::read(dir.join("fox.txt")).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupted_transfer_is_rejected() {
        let dir = tmp();
        let mut r = FileReceiver::new(Some(dir.clone()));
        let data = b"payload".to_vec();
        r.handle(offer("t3", "x.bin", &data));
        r.handle(chunk("t3", 0, b"paylons")); // wrong bytes, right size
        match r.handle(ControlMsg::FileEnd { id: "t3".into() }) {
            Handled::Consumed {
                reply: Some(ControlMsg::FileAbort { reason, .. }),
                ..
            } => assert!(reason.contains("sha256"), "{reason}"),
            _ => panic!("expected FileAbort"),
        }
        assert!(!dir.join("x.bin").exists());
        assert!(
            std::fs::read_dir(&dir).unwrap().next().is_none(),
            "no partials left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_chunk_aborts() {
        let dir = tmp();
        let mut r = FileReceiver::new(Some(dir.clone()));
        r.handle(offer("t4", "y.bin", b"abcdef"));
        match r.handle(chunk("t4", 1, b"abc")) {
            Handled::Consumed {
                reply: Some(ControlMsg::FileAbort { .. }),
                ..
            } => {}
            _ => panic!("expected FileAbort"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_transfer_messages_pass_through() {
        let mut r = FileReceiver::new(None);
        match r.handle(ControlMsg::Ping { t0_us: 1 }) {
            Handled::NotMine(ControlMsg::Ping { t0_us: 1 }) => {}
            _ => panic!("expected pass-through"),
        }
    }
}
