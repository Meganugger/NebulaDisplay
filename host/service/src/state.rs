//! Shared application state wired between capture, sessions, discovery and
//! the control panel.

use ndsp_protocol::messages::{DisplayMode, HostStats, ViewerStats};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, watch};

use crate::audio::AudioBlock;
use crate::clipboard::{ClipboardSync, HostClipboard};
use crate::config::Config;
use crate::pin::PinManager;
use crate::transfers::TransferManager;
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
    SetAudioGrant(bool),
    /// Panel decision for a pending file offer (routed by the transfer
    /// manager; carries the validated metadata so the session doesn't have
    /// to keep its own pending map).
    AnswerFileOffer {
        id: String,
        accept: bool,
        name: String,
        size_bytes: u64,
        sha256_hex: String,
    },
    /// Host→viewer file send (panel-initiated). `path` is a host-owned
    /// spool file (already size-capped and hashed by the initiator); the
    /// session offers it to the viewer and streams it only after the
    /// viewer explicitly accepts. The session owns `path` from here on
    /// (deletes it when the transfer ends, whatever the outcome).
    SendFile {
        id: String,
        name: String,
        size_bytes: u64,
        sha256_hex: String,
        path: std::path::PathBuf,
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
    /// Whether this device may sync clipboards (deny by default).
    pub clipboard_allowed: Arc<AtomicBool>,
    /// Whether the panel permits audio for this device.
    pub audio_allowed: Arc<AtomicBool>,
    /// The viewer currently has audio enabled *and* permitted — the panel's
    /// "this device is listening" indicator.
    pub audio_active: Arc<AtomicBool>,
    /// Client advertised the "cursor" feature (renders host cursor itself).
    pub supports_cursor: bool,
    pub stats: Mutex<ViewerStats>,
    pub commands: mpsc::Sender<SessionCommand>,
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
    /// Encoded audio blocks from the global audio loop; sessions subscribe.
    pub audio_tx: broadcast::Sender<Arc<AudioBlock>>,
    /// Sessions currently receiving audio (enabled + permitted). The audio
    /// loop releases the capture device at zero.
    audio_listeners: AtomicUsize,
    /// Host clipboard backend (system or in-memory).
    pub clipboard: Arc<dyn HostClipboard>,
    /// Echo suppression shared between watcher and apply path.
    pub clipboard_sync: Arc<ClipboardSync>,
    /// Latest host clipboard text (seq, text); sessions `watch` this and
    /// forward to granted clients.
    pub clipboard_tx: watch::Sender<(u64, Arc<String>)>,
    /// Pending viewer→host file offers awaiting a panel decision.
    pub transfers: TransferManager,
    pub host_stats: Mutex<HostStats>,
    /// Mode currently produced by the capture source.
    pub mode: Mutex<DisplayMode>,
    /// Desktop-space rect (left, top, right, bottom) of the captured surface,
    /// when the platform exposes one — used for multi-monitor input mapping.
    pub capture_rect: Mutex<Option<(i32, i32, i32, i32)>>,
    /// SHA-256 fingerprint of the TLS certificate when `--https` is active
    /// (panel display; users compare against the browser's cert warning).
    pub tls_fingerprint: Mutex<Option<String>>,
    pub clients: Mutex<HashMap<u64, Arc<ClientHandle>>>,
    next_client_id: AtomicU64,
    serving_port: AtomicU64,
    shutdown: AtomicBool,
}

impl AppState {
    pub async fn new(cfg: Config) -> anyhow::Result<Self> {
        let clipboard = crate::clipboard::create_clipboard();
        Self::new_with_clipboard(cfg, clipboard).await
    }

    /// Test/embedding constructor with an explicit clipboard backend (keeps
    /// the e2e suite off the developer's real clipboard).
    pub async fn new_with_clipboard(
        cfg: Config,
        clipboard: Arc<dyn HostClipboard>,
    ) -> anyhow::Result<Self> {
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
        let (audio_tx, _) = broadcast::channel(64);
        let (clipboard_tx, _) = watch::channel((0, Arc::new(String::new())));
        Ok(Self {
            cfg,
            fingerprint,
            pins,
            trust: Mutex::new(trust),
            frame_tx,
            cursor_tx,
            audio_tx,
            audio_listeners: AtomicUsize::new(0),
            clipboard,
            clipboard_sync: Arc::new(ClipboardSync::default()),
            clipboard_tx,
            transfers: TransferManager::default(),
            tls_fingerprint: Mutex::new(None),
            host_stats: Mutex::new(HostStats::default()),
            mode: Mutex::new(DisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            }),
            capture_rect: Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
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

    /// URL scheme of the viewer endpoint ("http" or "https").
    pub fn viewer_scheme(&self) -> &'static str {
        if self.tls_fingerprint.lock().unwrap().is_some() {
            "https"
        } else {
            "http"
        }
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

    /// Push a clipboard-grant change to the store and any live session.
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

    /// Push an audio-permission change to the store and any live session.
    pub fn set_audio_grant(&self, device_id: &str, allowed: bool) -> anyhow::Result<bool> {
        let found = self
            .trust
            .lock()
            .unwrap()
            .set_audio_allowed(device_id, allowed)?;
        for client in self.clients.lock().unwrap().values() {
            if client.device_id == device_id {
                client.audio_allowed.store(allowed, Ordering::Relaxed);
                let _ = client
                    .commands
                    .try_send(SessionCommand::SetAudioGrant(allowed));
            }
        }
        Ok(found)
    }

    /// Audio listener bookkeeping (sessions call these on state changes).
    pub fn audio_listener_add(&self) {
        self.audio_listeners.fetch_add(1, Ordering::Relaxed);
    }
    pub fn audio_listener_remove(&self) {
        let prev = self.audio_listeners.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "audio listener count underflow");
    }
    pub fn audio_listener_count(&self) -> usize {
        self.audio_listeners.load(Ordering::Relaxed)
    }

    /// Offer a spooled file to a connected client (host→viewer send,
    /// ROADMAP P2.15). The session takes ownership of `path`. Returns the
    /// transfer id, or an error when the client is unknown / its command
    /// queue is full (callers must then delete the spool file themselves).
    pub fn send_file_to_client(
        &self,
        client_id: u64,
        name: &str,
        size_bytes: u64,
        sha256_hex: &str,
        path: std::path::PathBuf,
    ) -> anyhow::Result<String> {
        let commands = self
            .clients
            .lock()
            .unwrap()
            .get(&client_id)
            .map(|c| c.commands.clone())
            .ok_or_else(|| anyhow::anyhow!("no connected client with id {client_id}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let cmd = SessionCommand::SendFile {
            id: id.clone(),
            name: crate::transfers::sanitize_filename(name),
            size_bytes,
            sha256_hex: sha256_hex.to_ascii_lowercase(),
            path,
        };
        commands
            .try_send(cmd)
            .map_err(|_| anyhow::anyhow!("client session is busy"))?;
        Ok(id)
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
        let raw = crate::keystore::unprotect(&raw)?;
        anyhow::ensure!(raw.len() == 32, "identity.key corrupt (expected 32 bytes)");
        raw
    } else {
        let key: [u8; 32] = ndsp_protocol::crypto::random_bytes();
        std::fs::write(&path, crate::keystore::protect(&key))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        key.to_vec()
    };
    Ok(hex::encode(Sha256::digest(&key)))
}
