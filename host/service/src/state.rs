//! Shared application state wired between capture, sessions, discovery and
//! the control panel.

use ndsp_protocol::messages::{DisplayMode, HostStats, ViewerStats};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

use crate::clipboard::{ClipboardBackend, ClipboardUpdate};
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
    /// Panel-side per-client audio mute (viewer opt-in state is separate).
    SetAudioMute(bool),
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
    /// Client advertised the "cursor" feature (renders host cursor itself).
    pub supports_cursor: bool,
    /// Viewer opted into audio this session (panel indicator).
    pub audio_active: Arc<AtomicBool>,
    /// Panel-side mute for this client (independent of the viewer's opt-in).
    pub audio_muted: Arc<AtomicBool>,
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
    /// Host clipboard backend (real on Windows, in-memory elsewhere/tests).
    pub clipboard: Arc<dyn ClipboardBackend>,
    /// Latest host clipboard text; sessions `watch` this and forward it to
    /// clipboard-granted, clipboard-capable viewers.
    pub clipboard_tx: watch::Sender<Option<Arc<ClipboardUpdate>>>,
    /// Encoded Opus packets fan-out (None until the audio pipeline runs).
    pub audio_tx: tokio::sync::broadcast::Sender<Arc<ndsp_protocol::media::AudioFrame>>,
    /// The audio pipeline is up (capture + encoder created successfully).
    audio_ready: AtomicBool,
    /// Host-side global audio switch (config default; panel can flip live).
    audio_enabled: AtomicBool,
    pub host_stats: Mutex<HostStats>,
    /// Mode currently produced by the capture source.
    pub mode: Mutex<DisplayMode>,
    /// Desktop-space rect (left, top, right, bottom) of the captured surface,
    /// when the platform exposes one — used for multi-monitor input mapping.
    pub capture_rect: Mutex<Option<(i32, i32, i32, i32)>>,
    pub clients: Mutex<HashMap<u64, Arc<ClientHandle>>>,
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
        // 32 × 20 ms ≈ 640 ms of audio buffering per slow client before the
        // broadcast channel starts dropping (a drop = short glitch, counted
        // client-side via seq gaps — always better than growing latency).
        let (audio_tx, _) = tokio::sync::broadcast::channel(32);
        let audio_enabled = cfg.file.audio;
        Ok(Self {
            cfg,
            fingerprint,
            pins,
            trust: Mutex::new(trust),
            frame_tx,
            cursor_tx,
            clipboard: crate::clipboard::create_backend(),
            clipboard_tx,
            audio_tx,
            audio_ready: AtomicBool::new(false),
            audio_enabled: AtomicBool::new(audio_enabled),
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

    /// Same as [`Self::set_input_grant`] for the clipboard capability.
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

    /// Panel-side per-client audio mute (not persisted — a live control).
    pub fn set_audio_mute(&self, device_id: &str, muted: bool) -> bool {
        let mut found = false;
        for client in self.clients.lock().unwrap().values() {
            if client.device_id == device_id {
                found = true;
                client.audio_muted.store(muted, Ordering::Relaxed);
                let _ = client
                    .commands
                    .try_send(SessionCommand::SetAudioMute(muted));
            }
        }
        found
    }

    /// The audio pipeline exists (capture source + encoder came up).
    pub fn set_audio_ready(&self, ready: bool) {
        self.audio_ready.store(ready, Ordering::Relaxed);
    }

    /// Host-side global audio switch (panel toggle; starts at the config value).
    pub fn set_audio_enabled(&self, enabled: bool) {
        self.audio_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled.load(Ordering::Relaxed)
    }

    /// Audio can actually be delivered right now (pipeline up **and** the
    /// host switch is on). Advertised to viewers in `auth_ok`.
    pub fn audio_available(&self) -> bool {
        self.audio_ready.load(Ordering::Relaxed) && self.audio_enabled()
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
