//! Shared server state and host-level diagnostics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use serde::Serialize;

use crate::config::Config;
use crate::pairing::{PairingManager, TrustStore};

/// Live per-client diagnostics, surfaced on the control panel.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ClientDiag {
    pub device_id: String,
    pub name: String,
    pub remote_addr: String,
    pub authorized: bool,
    pub input_allowed: bool,
    pub streaming: bool,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub rtt_ms: f32,
    pub quality: u8,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub connected_secs: u64,
}

pub struct AppState {
    config: RwLock<Config>,
    pub pairing: Mutex<PairingManager>,
    pub trust: Mutex<TrustStore>,
    /// connection id → live diagnostics.
    pub clients: Mutex<HashMap<u64, ClientDiag>>,
    pub tls_fingerprint: Option<String>,
    pub started_at: Instant,
    next_conn_id: AtomicU64,
}

impl AppState {
    pub fn new(config: Config, tls_fingerprint: Option<String>) -> Self {
        let config_path = Config::default_path();
        let trust = TrustStore::load(TrustStore::default_path(&config_path));
        Self {
            config: RwLock::new(config),
            pairing: Mutex::new(PairingManager::new()),
            trust: Mutex::new(trust),
            clients: Mutex::new(HashMap::new()),
            tls_fingerprint,
            started_at: Instant::now(),
            next_conn_id: AtomicU64::new(1),
        }
    }

    /// Test constructor with an in-memory trust store.
    pub fn for_tests(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            pairing: Mutex::new(PairingManager::new()),
            trust: Mutex::new(TrustStore::in_memory()),
            clients: Mutex::new(HashMap::new()),
            tls_fingerprint: None,
            started_at: Instant::now(),
            next_conn_id: AtomicU64::new(1),
        }
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.config.write().unwrap();
        f(&mut c);
    }

    pub fn new_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn upsert_client(&self, id: u64, f: impl FnOnce(&mut ClientDiag)) {
        let mut clients = self.clients.lock().unwrap();
        let entry = clients.entry(id).or_default();
        f(entry);
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }
}
