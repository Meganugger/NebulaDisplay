//! Secure pairing and the device trust store.
//!
//! Flow:
//! 1. The host user clicks "Pair a device" on the control panel, which calls
//!    [`PairingManager::issue_pin`]. A 6-digit PIN (and a QR code embedding
//!    the viewer URL) is displayed *on the host only*.
//! 2. A viewer sends `PairRequest { pin, device_name }` within the PIN
//!    lifetime. Wrong attempts are rate-limited; the PIN is single-use.
//! 3. On success the host generates a 256-bit random token, stores only its
//!    SHA-256 hash in the trust store, and returns the token to the viewer.
//! 4. Future connections authenticate with `Auth { token }`.
//!
//! Input injection is a *separate* per-device grant, off by default, toggled
//! by the host user on the control panel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// PIN lifetime. Short by design: pairing is an interactive act.
pub const PIN_TTL: Duration = Duration::from_secs(120);
/// Max wrong PIN attempts before the PIN is invalidated.
pub const MAX_PIN_ATTEMPTS: u32 = 5;

pub struct PairingManager {
    active: Option<ActivePin>,
}

struct ActivePin {
    pin: String,
    issued: Instant,
    attempts_left: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PinCheck {
    Ok,
    Wrong,
    Expired,
    NoneActive,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingManager {
    pub fn new() -> Self {
        Self { active: None }
    }

    /// Issue a new single-use PIN, replacing any previous one.
    pub fn issue_pin(&mut self) -> String {
        let n = OsRng.next_u32() % 1_000_000;
        let pin = format!("{n:06}");
        self.active = Some(ActivePin {
            pin: pin.clone(),
            issued: Instant::now(),
            attempts_left: MAX_PIN_ATTEMPTS,
        });
        pin
    }

    /// For displaying on the control panel.
    pub fn current_pin(&self) -> Option<(String, Duration)> {
        self.active.as_ref().and_then(|a| {
            let age = a.issued.elapsed();
            (age < PIN_TTL).then(|| (a.pin.clone(), PIN_TTL - age))
        })
    }

    /// Verify a pairing attempt. Constant-time comparison; consumes the PIN
    /// on success; counts down attempts on failure.
    pub fn check_pin(&mut self, attempt: &str) -> PinCheck {
        let Some(active) = &mut self.active else {
            return PinCheck::NoneActive;
        };
        if active.issued.elapsed() >= PIN_TTL {
            self.active = None;
            return PinCheck::Expired;
        }
        let ok = constant_time_eq(active.pin.as_bytes(), attempt.as_bytes());
        if ok {
            self.active = None; // single use
            PinCheck::Ok
        } else {
            active.attempts_left = active.attempts_left.saturating_sub(1);
            if active.attempts_left == 0 {
                self.active = None;
            }
            PinCheck::Wrong
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Trust store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: String,
    /// SHA-256 hex of the device token. The token itself is never stored.
    pub token_hash: String,
    /// Whether the host user allows this device to inject input.
    pub input_allowed: bool,
    pub paired_at_unix: u64,
    pub last_seen_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    devices: HashMap<String, DeviceEntry>,
}

/// Persistent trust store, saved as JSON next to the config file.
pub struct TrustStore {
    path: PathBuf,
    data: TrustFile,
}

impl TrustStore {
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, data }
    }

    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            data: TrustFile::default(),
        }
    }

    fn persist(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.data) {
            if let Err(e) = std::fs::write(&self.path, s) {
                tracing::warn!("failed to persist trust store: {e}");
            }
        }
    }

    /// Register a device after successful pairing. Returns the token that
    /// must be sent (once) to the client.
    pub fn register(&mut self, device_id: &str, name: &str) -> String {
        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);
        let now = unix_now();
        self.data.devices.insert(
            device_id.to_string(),
            DeviceEntry {
                name: name.to_string(),
                token_hash: sha256_hex(&token),
                input_allowed: false, // input is opt-in per device
                paired_at_unix: now,
                last_seen_unix: now,
            },
        );
        self.persist();
        token
    }

    /// Verify a token for a device. Updates `last_seen` on success.
    pub fn verify(&mut self, device_id: &str, token: &str) -> bool {
        let hash = sha256_hex(token);
        match self.data.devices.get_mut(device_id) {
            Some(e) if constant_time_eq(e.token_hash.as_bytes(), hash.as_bytes()) => {
                e.last_seen_unix = unix_now();
                self.persist();
                true
            }
            _ => false,
        }
    }

    pub fn contains(&self, device_id: &str) -> bool {
        self.data.devices.contains_key(device_id)
    }

    pub fn input_allowed(&self, device_id: &str) -> bool {
        self.data
            .devices
            .get(device_id)
            .map(|e| e.input_allowed)
            .unwrap_or(false)
    }

    pub fn set_input_allowed(&mut self, device_id: &str, allowed: bool) -> bool {
        if let Some(e) = self.data.devices.get_mut(device_id) {
            e.input_allowed = allowed;
            self.persist();
            true
        } else {
            false
        }
    }

    pub fn revoke(&mut self, device_id: &str) -> bool {
        let removed = self.data.devices.remove(device_id).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    pub fn list(&self) -> Vec<(String, DeviceEntry)> {
        let mut v: Vec<_> = self
            .data
            .devices
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        v.sort_by_key(|(_, e)| std::cmp::Reverse(e.last_seen_unix));
        v
    }

    pub fn default_path(config_path: &Path) -> PathBuf {
        crate::config::Config::data_dir(config_path).join("trusted_devices.json")
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_lifecycle() {
        let mut pm = PairingManager::new();
        assert_eq!(pm.check_pin("123456"), PinCheck::NoneActive);
        let pin = pm.issue_pin();
        assert_eq!(pin.len(), 6);
        assert_eq!(pm.check_pin("000000").eq(&PinCheck::Ok), pin == "000000");
        // Re-issue and use correctly.
        let pin = pm.issue_pin();
        assert_eq!(pm.check_pin(&pin), PinCheck::Ok);
        // Single use.
        assert_eq!(pm.check_pin(&pin), PinCheck::NoneActive);
    }

    #[test]
    fn pin_attempt_limit() {
        let mut pm = PairingManager::new();
        let pin = pm.issue_pin();
        let wrong = if pin == "999999" { "000000" } else { "999999" };
        for _ in 0..MAX_PIN_ATTEMPTS {
            assert_eq!(pm.check_pin(wrong), PinCheck::Wrong);
        }
        // PIN burned after too many attempts — even the right one fails now.
        assert_eq!(pm.check_pin(&pin), PinCheck::NoneActive);
    }

    #[test]
    fn trust_store_register_verify_revoke() {
        let mut ts = TrustStore::in_memory();
        let token = ts.register("dev-1", "Pixel 9");
        assert!(ts.contains("dev-1"));
        assert!(!ts.input_allowed("dev-1"), "input must be off by default");
        assert!(ts.verify("dev-1", &token));
        assert!(!ts.verify("dev-1", "wrong-token"));
        assert!(!ts.verify("dev-2", &token));
        assert!(ts.set_input_allowed("dev-1", true));
        assert!(ts.input_allowed("dev-1"));
        assert!(ts.revoke("dev-1"));
        assert!(!ts.verify("dev-1", &token));
    }

    #[test]
    fn tokens_are_unique_and_hashed() {
        let mut ts = TrustStore::in_memory();
        let t1 = ts.register("a", "A");
        let t2 = ts.register("b", "B");
        assert_ne!(t1, t2);
        // Stored value is a hash, not the token.
        for (_, e) in ts.list() {
            assert_ne!(e.token_hash, t1);
            assert_ne!(e.token_hash, t2);
        }
    }
}
