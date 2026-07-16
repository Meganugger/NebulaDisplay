//! Persistent trusted-device store.
//!
//! Stored as JSON in the data dir (`devices.json`, mode 0600 on unix). Raw
//! trust tokens are stored because reconnect proofs are keyed hashes over the
//! token + handshake transcript (see `docs/SECURITY.md` §Trust store). The
//! host machine is inside the trust boundary — it renders the screen content
//! in the first place.

use ndsp_protocol::crypto::{random_bytes, reauth_transcript, token_proof, TOKEN_LEN};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    /// hex-encoded 32-byte token.
    pub token_hex: String,
    pub created_unix: u64,
    pub last_seen_unix: u64,
    /// Whether this device may inject input. **Deny by default.**
    pub input_allowed: bool,
    /// Whether this device may read/write the host clipboard. **Deny by
    /// default** (added in v0.5; absent in older stores = denied).
    #[serde(default)]
    pub clipboard_allowed: bool,
    /// Whether this device may receive host audio. Allowed by default —
    /// audio is the same sensitivity class as the screen the device already
    /// sees; the panel can mute a device live. (Absent in older stores =
    /// allowed.)
    #[serde(default = "default_true")]
    pub audio_allowed: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    devices: Vec<TrustedDevice>,
}

pub struct TrustStore {
    path: PathBuf,
    devices: Vec<TrustedDevice>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl TrustStore {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let devices = if path.exists() {
            let raw = std::fs::read(&path)?;
            // v0.5+: DPAPI-wrapped on Windows; legacy plaintext still reads
            // (and is upgraded to the wrapped format on the next save).
            crate::keystore::unprotect(&raw)
                .and_then(|plain| serde_json::from_slice::<StoreFile>(&plain).map_err(Into::into))
                .map(|f| f.devices)
                .unwrap_or_else(|e| {
                    warn!("trust store unreadable ({e}); starting empty (old file kept as .bak)");
                    let _ = std::fs::copy(&path, path.with_extension("json.bak"));
                    Vec::new()
                })
        } else {
            Vec::new()
        };
        Ok(Self { path, devices })
    }

    fn save(&self) -> anyhow::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let raw = serde_json::to_string_pretty(&StoreFile {
            devices: self.devices.clone(),
        })?;
        std::fs::write(&tmp, crate::keystore::protect(raw.as_bytes()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Register a freshly paired device and return its raw trust token.
    pub fn enroll(
        &mut self,
        device_id: &str,
        name: &str,
        platform: &str,
    ) -> anyhow::Result<[u8; TOKEN_LEN]> {
        let token: [u8; TOKEN_LEN] = random_bytes();
        // Re-pairing an existing id replaces its token (e.g. app reinstall).
        self.devices.retain(|d| d.device_id != device_id);
        self.devices.push(TrustedDevice {
            device_id: device_id.to_string(),
            name: name.to_string(),
            platform: platform.to_string(),
            token_hex: hex::encode(token),
            created_unix: now_unix(),
            last_seen_unix: now_unix(),
            input_allowed: false, // never grant input implicitly
            clipboard_allowed: false,
            audio_allowed: true,
        });
        self.save()?;
        info!(device_id, name, "device enrolled (input DENIED by default)");
        Ok(token)
    }

    /// Verify a reconnect proof. On success updates `last_seen` and returns
    /// the device record.
    pub fn verify(
        &mut self,
        device_id: &str,
        connection_nonce: &[u8],
        client_pub: &[u8],
        server_pub: &[u8],
        proof: &[u8],
    ) -> Option<TrustedDevice> {
        let dev = self.devices.iter_mut().find(|d| d.device_id == device_id)?;
        let token = hex::decode(&dev.token_hex).ok()?;
        let expected = token_proof(
            &token,
            &reauth_transcript(connection_nonce, client_pub, server_pub),
        );
        // Constant-time compare.
        if proof.len() != expected.len() {
            return None;
        }
        let mut diff = 0u8;
        for (a, b) in proof.iter().zip(expected.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return None;
        }
        dev.last_seen_unix = now_unix();
        let out = dev.clone();
        let _ = self.save();
        Some(out)
    }

    pub fn list(&self) -> &[TrustedDevice] {
        &self.devices
    }

    pub fn set_input_allowed(&mut self, device_id: &str, allowed: bool) -> anyhow::Result<bool> {
        let Some(dev) = self.devices.iter_mut().find(|d| d.device_id == device_id) else {
            return Ok(false);
        };
        dev.input_allowed = allowed;
        self.save()?;
        info!(device_id, allowed, "input grant updated");
        Ok(true)
    }

    pub fn set_clipboard_allowed(
        &mut self,
        device_id: &str,
        allowed: bool,
    ) -> anyhow::Result<bool> {
        let Some(dev) = self.devices.iter_mut().find(|d| d.device_id == device_id) else {
            return Ok(false);
        };
        dev.clipboard_allowed = allowed;
        self.save()?;
        info!(device_id, allowed, "clipboard grant updated");
        Ok(true)
    }

    pub fn set_audio_allowed(&mut self, device_id: &str, allowed: bool) -> anyhow::Result<bool> {
        let Some(dev) = self.devices.iter_mut().find(|d| d.device_id == device_id) else {
            return Ok(false);
        };
        dev.audio_allowed = allowed;
        self.save()?;
        info!(device_id, allowed, "audio grant updated");
        Ok(true)
    }

    pub fn revoke(&mut self, device_id: &str) -> anyhow::Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let removed = self.devices.len() != before;
        if removed {
            self.save()?;
            info!(device_id, "device revoked");
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub fn get(&self, device_id: &str) -> Option<&TrustedDevice> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stores written before v0.5 lack the clipboard/audio fields and are
    /// plaintext JSON — they must load with safe defaults and be upgraded
    /// (keystore-wrapped on Windows) on the next save.
    #[test]
    fn pre_v05_store_loads_with_safe_defaults() {
        let dir = std::env::temp_dir().join(format!("ndsp-test-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        std::fs::write(
            &path,
            r#"{"devices":[{"device_id":"old-1","name":"Old Tablet","platform":"android",
                "token_hex":"aa","created_unix":1,"last_seen_unix":2,"input_allowed":true}]}"#,
        )
        .unwrap();
        let store = TrustStore::load(path.clone()).unwrap();
        let d = store.get("old-1").expect("legacy device loads");
        assert!(d.input_allowed, "existing grants preserved");
        assert!(!d.clipboard_allowed, "clipboard must default to DENIED");
        assert!(d.audio_allowed, "audio defaults to allowed (panel-mutable)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enroll_verify_revoke() {
        let dir = std::env::temp_dir().join(format!("ndsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        let _ = std::fs::remove_file(&path);

        let mut store = TrustStore::load(path.clone()).unwrap();
        let token = store.enroll("dev-1", "Test Tablet", "android").unwrap();
        assert!(
            !store.get("dev-1").unwrap().input_allowed,
            "input must be denied by default"
        );

        let nonce = [5u8; 16];
        let cpub = [1u8; 33];
        let spub = [2u8; 33];
        let proof = token_proof(&token, &reauth_transcript(&nonce, &cpub, &spub));
        assert!(store
            .verify("dev-1", &nonce, &cpub, &spub, &proof)
            .is_some());
        // Wrong transcript (MITM key substitution) fails.
        let bad = token_proof(&token, &reauth_transcript(&nonce, &[9u8; 33], &spub));
        assert!(store.verify("dev-1", &nonce, &cpub, &spub, &bad).is_none());

        // Reload from disk — persistence works.
        let mut store2 = TrustStore::load(path.clone()).unwrap();
        assert!(store2
            .verify("dev-1", &nonce, &cpub, &spub, &proof)
            .is_some());
        assert!(store2.revoke("dev-1").unwrap());
        assert!(store2
            .verify("dev-1", &nonce, &cpub, &spub, &proof)
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
