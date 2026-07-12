//! Shared application state wired between capture, sessions, discovery and
//! the control panel.

use ndsp_protocol::messages::{DisplayMode, HostStats, ViewerStats};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::pin::PinManager;
use crate::trust::TrustStore;

/// Host cursor image (RGBA8, tightly packed) + hotspot.
#[derive(Debug, PartialEq, Eq)]
pub struct CursorShapeData {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// Latest host cursor state, published by the capture loop and watched by
/// every session (forwarded over the control channel, never behind video).
#[derive(Debug, Clone, Default)]
pub struct CursorState {
    /// Monotonic update counter (0 = never updated).
    pub seq: u64,
    /// Normalized (0..1) position against the captured surface.
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    /// Bumped when `shape` changes so sessions know to (re)send the image.
    pub shape_seq: u64,
    pub shape: Option<Arc<CursorShapeData>>,
    /// True → the cursor is baked into the video frames (legacy client
    /// connected); cursor-capable viewers must hide their overlay.
    pub composited: bool,
}

/// One captured frame, shared zero-copy between all client sessions.
pub struct CapturedFrame {
    pub seq: u64,
    pub timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed BGRA8 (`width * height * 4`).
    pub bgra: Vec<u8>,
}

/// Commands the panel (or server) can push into a live session task.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    SetInputGrant(bool),
    SetClipboardGrant(bool),
    /// Panel decision on a pending file-drop offer from this session.
    FileDecision {
        transfer_id: u32,
        accept: bool,
    },
    Kick {
        reason: String,
    },
}

/// Panel-visible live client entry.
pub struct ClientHandle {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub addr: SocketAddr,
    pub connected_unix: u64,
    pub input_allowed: Arc<AtomicBool>,
    pub clipboard_allowed: Arc<AtomicBool>,
    /// Session currently receives host audio (panel indicator).
    pub audio_on: Arc<AtomicBool>,
    /// Client advertised the "cursor" feature (renders host cursor itself).
    pub supports_cursor: bool,
    pub stats: Mutex<ViewerStats>,
    pub commands: mpsc::Sender<SessionCommand>,
}

/// A file-drop offer waiting for the host user's decision in the panel.
#[derive(Debug, Clone)]
pub struct PendingFileOffer {
    /// Session-local transfer id (scoped by `client_id`).
    pub transfer_id: u32,
    pub client_id: u64,
    pub device_id: String,
    pub device_name: String,
    pub file_name: String,
    pub size: u64,
    pub offered_unix: u64,
}

pub struct AppState {
    pub cfg: Config,
    /// SHA-256 hex fingerprint of this host's persistent identity key.
    pub fingerprint: String,
    pub pins: PinManager,
    pub trust: Mutex<TrustStore>,
    /// Latest captured frame; sessions `watch` this.
    pub frame_tx: watch::Sender<Option<Arc<CapturedFrame>>>,
    /// Latest host cursor state; sessions `watch` this.
    pub cursor_tx: watch::Sender<CursorState>,
    pub host_stats: Mutex<HostStats>,
    /// Mode currently produced by the capture source.
    pub mode: Mutex<DisplayMode>,
    /// Desktop-space rect (left, top, right, bottom) of the captured surface,
    /// when the platform exposes one — used for multi-monitor input mapping.
    pub capture_rect: Mutex<Option<(i32, i32, i32, i32)>>,
    pub clients: Mutex<HashMap<u64, Arc<ClientHandle>>>,
    /// Host clipboard backend (real on Windows, in-memory elsewhere).
    pub clipboard: Arc<dyn crate::clipboard::ClipboardBackend>,
    /// Latest clipboard item to sync; sessions `watch` this.
    pub clipboard_tx: watch::Sender<Option<Arc<crate::clipboard::ClipboardItem>>>,
    /// Backend seq of the last clipboard write *we* performed (remote →
    /// host), so the watcher doesn't re-broadcast it as a host change.
    pub clipboard_own_seq: AtomicU64,
    /// File-drop offers awaiting the host user's panel decision.
    pub pending_files: Mutex<Vec<PendingFileOffer>>,
    /// Encoded host-audio packets (Opus); sessions with audio on `watch`
    /// this. `None` until the audio pipeline publishes its first packet.
    pub audio_tx: watch::Sender<Option<Arc<crate::audio::AudioPacket>>>,
    /// Number of sessions currently requesting audio (drives the pipeline).
    pub audio_listeners: AtomicU64,
    next_client_id: AtomicU64,
    serving_port: AtomicU64,
    shutdown: AtomicBool,
}

impl AppState {
    pub async fn new(cfg: Config) -> anyhow::Result<Self> {
        let fingerprint = load_or_create_identity(&cfg)?;
        let pins = PinManager::new(
            cfg.file.pin_digits,
            cfg.file.pin_ttl_secs,
            cfg.file.max_pin_attempts,
            cfg.file.lockout_secs,
        );
        let trust = TrustStore::load(cfg.data_dir.join("devices.json"))?;
        let (frame_tx, _) = watch::channel(None);
        let (cursor_tx, _) = watch::channel(CursorState::default());
        let (clipboard_tx, _) = watch::channel(None);
        let (audio_tx, _) = watch::channel(None);
        Ok(Self {
            cfg,
            fingerprint,
            pins,
            trust: Mutex::new(trust),
            frame_tx,
            cursor_tx,
            host_stats: Mutex::new(HostStats::default()),
            mode: Mutex::new(DisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            }),
            capture_rect: Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
            clipboard: crate::clipboard::create_backend(),
            clipboard_tx,
            clipboard_own_seq: AtomicU64::new(0),
            pending_files: Mutex::new(Vec::new()),
            audio_tx,
            audio_listeners: AtomicU64::new(0),
            next_client_id: AtomicU64::new(1),
            serving_port: AtomicU64::new(ndsp_protocol::DEFAULT_PORT as u64),
            shutdown: AtomicBool::new(false),
        })
    }

    /// Ask long-running loops (capture thread) to exit.
    pub fn trigger_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn set_serving_port(&self, port: u16) {
        self.serving_port.store(port as u64, Ordering::Relaxed);
    }

    pub fn serving_port(&self) -> u16 {
        self.serving_port.load(Ordering::Relaxed) as u16
    }

    pub fn register_client(&self, handle: Arc<ClientHandle>) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(id, handle);
        let mut hs = self.host_stats.lock().unwrap();
        hs.clients += 1;
        drop(hs);
        self.refresh_cursor_policy();
        id
    }

    pub fn unregister_client(&self, id: u64) {
        if self.clients.lock().unwrap().remove(&id).is_some() {
            let mut hs = self.host_stats.lock().unwrap();
            hs.clients = hs.clients.saturating_sub(1);
            drop(hs);
            self.refresh_cursor_policy();
        }
    }

    /// The cursor rides its own channel only while *every* connected client
    /// can render it; one legacy client flips the capture path back to
    /// compositing the cursor into video frames.
    pub fn cursor_channel_active(&self) -> bool {
        self.clients
            .lock()
            .unwrap()
            .values()
            .all(|c| c.supports_cursor)
    }

    /// Re-publish cursor state with the current composited policy so capable
    /// viewers hide/show their overlay when the client mix changes.
    fn refresh_cursor_policy(&self) {
        let composited = !self.cursor_channel_active();
        self.cursor_tx.send_if_modified(|cs| {
            if cs.composited != composited {
                cs.composited = composited;
                cs.seq += 1;
                true
            } else {
                false
            }
        });
    }

    /// Push an input-grant change both to the persistent store and any live
    /// session for that device.
    pub fn set_input_grant(&self, device_id: &str, allowed: bool) -> anyhow::Result<bool> {
        let found = self
            .trust
            .lock()
            .unwrap()
            .set_input_allowed(device_id, allowed)?;
        for client in self.clients.lock().unwrap().values() {
            if client.device_id == device_id {
                client.input_allowed.store(allowed, Ordering::Relaxed);
                let _ = client
                    .commands
                    .try_send(SessionCommand::SetInputGrant(allowed));
            }
        }
        Ok(found)
    }

    /// Same as [`Self::set_input_grant`] for the clipboard grant.
    pub fn set_clipboard_grant(&self, device_id: &str, allowed: bool) -> anyhow::Result<bool> {
        let found = self
            .trust
            .lock()
            .unwrap()
            .set_clipboard_allowed(device_id, allowed)?;
        for client in self.clients.lock().unwrap().values() {
            if client.device_id == device_id {
                client.clipboard_allowed.store(allowed, Ordering::Relaxed);
                let _ = client
                    .commands
                    .try_send(SessionCommand::SetClipboardGrant(allowed));
            }
        }
        Ok(found)
    }

    /// Publish a clipboard item to all sessions (grant checked per session).
    pub fn publish_clipboard(&self, item: crate::clipboard::ClipboardItem) {
        let _ = self.clipboard_tx.send(Some(Arc::new(item)));
    }

    /// Record a pending file-drop offer for the panel.
    pub fn add_pending_file(&self, offer: PendingFileOffer) {
        let mut pending = self.pending_files.lock().unwrap();
        // A session re-offering the same id replaces the stale entry.
        pending.retain(|p| !(p.client_id == offer.client_id && p.transfer_id == offer.transfer_id));
        pending.push(offer);
    }

    pub fn remove_pending_file(&self, client_id: u64, transfer_id: u32) {
        self.pending_files
            .lock()
            .unwrap()
            .retain(|p| !(p.client_id == client_id && p.transfer_id == transfer_id));
    }

    /// Panel decision on a pending file offer → forward to the session.
    pub fn decide_file(&self, client_id: u64, transfer_id: u32, accept: bool) -> bool {
        let exists = self
            .pending_files
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.client_id == client_id && p.transfer_id == transfer_id);
        if !exists {
            return false;
        }
        self.remove_pending_file(client_id, transfer_id);
        if let Some(client) = self.clients.lock().unwrap().get(&client_id) {
            let _ = client.commands.try_send(SessionCommand::FileDecision {
                transfer_id,
                accept,
            });
            true
        } else {
            false
        }
    }

    /// Revoke trust and kick any live session for that device.
    pub fn revoke_device(&self, device_id: &str) -> anyhow::Result<bool> {
        let removed = self.trust.lock().unwrap().revoke(device_id)?;
        for client in self.clients.lock().unwrap().values() {
            if client.device_id == device_id {
                let _ = client.commands.try_send(SessionCommand::Kick {
                    reason: "device revoked by host".into(),
                });
            }
        }
        Ok(removed)
    }
}

/// The identity key is 32 random bytes persisted once per install; its hash
/// is the host fingerprint shown in beacons and QR codes so returning viewers
/// can detect impostors on the same address.
fn load_or_create_identity(cfg: &Config) -> anyhow::Result<String> {
    let path = cfg.data_dir.join("identity.key");
    let key: Vec<u8> = if path.exists() {
        let raw = std::fs::read(&path)?;
        anyhow::ensure!(raw.len() == 32, "identity.key corrupt (expected 32 bytes)");
        raw
    } else {
        let key: [u8; 32] = ndsp_protocol::crypto::random_bytes();
        std::fs::write(&path, key)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        key.to_vec()
    };
    Ok(hex::encode(Sha256::digest(&key)))
}
