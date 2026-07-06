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
    Kick { reason: String },
}

/// Panel-visible live client entry.
pub struct ClientHandle {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub addr: SocketAddr,
    pub connected_unix: u64,
    pub input_allowed: Arc<AtomicBool>,
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
        Ok(Self {
            cfg,
            fingerprint,
            pins,
            trust: Mutex::new(trust),
            frame_tx,
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
        id
    }

    pub fn unregister_client(&self, id: u64) {
        if self.clients.lock().unwrap().remove(&id).is_some() {
            let mut hs = self.host_stats.lock().unwrap();
            hs.clients = hs.clients.saturating_sub(1);
        }
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
