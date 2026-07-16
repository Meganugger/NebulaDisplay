//! At-rest protection for host secrets (ROADMAP P1.6).
//!
//! * **Windows**: secrets (trust store, identity key, TLS private key) are
//!   wrapped with **DPAPI** (`CryptProtectData`, user scope) — another local
//!   account, or the same files exfiltrated to another machine, cannot read
//!   them. Files carry the `NDPP1\n` magic prefix.
//! * **Linux / macOS**: secrets are sealed with AES-256-GCM under a random
//!   wrapping key held in the **OS keychain** (Secret Service / macOS
//!   Keychain via the `keyring` crate) — the same "exfiltrated files are
//!   useless off this account" property DPAPI gives Windows. Files carry
//!   the `NDPK1\n` prefix. Headless systems without a keychain daemon keep
//!   the historical plaintext-with-`0600` behavior (logged loudly).
//! * **Other Unix**: plaintext with `0600` permissions.
//!
//! Reads are transparently backward compatible: a legacy plaintext file is
//! returned as-is and upgraded to the wrapped format on the next write.

/// Magic prefix marking a DPAPI-wrapped payload.
pub const MAGIC: &[u8] = b"NDPP1\n";

/// Magic prefix marking a keychain-key-wrapped payload (Linux/macOS).
#[cfg(not(windows))]
pub const MAGIC_KEYCHAIN: &[u8] = b"NDPK1\n";

/// Wrap plaintext for at-rest storage.
#[cfg(not(windows))]
pub fn protect(plain: &[u8]) -> Vec<u8> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(key) = keychain::wrapping_key() {
        let mut out = MAGIC_KEYCHAIN.to_vec();
        out.extend_from_slice(&keychain::seal_with(&key, plain));
        return out;
    }
    plain.to_vec()
}

/// Unwrap data read from disk. Accepts wrapped and legacy plaintext forms.
#[cfg(not(windows))]
pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    // A Windows-DPAPI-protected file copied to a Unix host cannot be
    // unwrapped here — which is exactly the property DPAPI provides.
    anyhow::ensure!(
        !data.starts_with(MAGIC),
        "this file was protected with Windows DPAPI on another machine"
    );
    if let Some(sealed) = data.strip_prefix(MAGIC_KEYCHAIN) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let key = keychain::wrapping_key().ok_or_else(|| {
                anyhow::anyhow!(
                    "this file is sealed under a key in the OS keychain, \
                     which is not reachable (different account/machine, or \
                     no keychain daemon)"
                )
            })?;
            return keychain::open_with(&key, sealed);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("this file was sealed with an OS keychain on another machine");
    }
    Ok(data.to_vec())
}

/// AES-256-GCM sealing under a wrapping key held in the OS keychain.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod keychain {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine as _;
    use rand::RngCore as _;

    const SERVICE: &str = "NebulaDisplay";
    const ENTRY: &str = "keystore-wrapping-key-v1";
    const NONCE_LEN: usize = 12;

    /// Get (or create on first use) the per-account wrapping key. `None`
    /// when no keychain is reachable — callers fall back to plaintext.
    /// Success is memoized; failures are retried on the next call (the
    /// daemon may simply not be up yet during login).
    ///
    /// `NDSP_NO_KEYCHAIN=1` skips the keychain entirely (plaintext-0600
    /// keystore). For headless boxes and CI: a *locked* macOS keychain
    /// makes `SecItem*` calls wait indefinitely for an interactive unlock
    /// prompt that will never be answered.
    pub fn wrapping_key() -> Option<[u8; 32]> {
        use std::sync::OnceLock;
        static KEY: OnceLock<[u8; 32]> = OnceLock::new();
        if let Some(k) = KEY.get() {
            return Some(*k);
        }
        if std::env::var_os("NDSP_NO_KEYCHAIN").is_some_and(|v| v != "0" && !v.is_empty()) {
            return None;
        }
        let fetched = fetch_or_create()?;
        Some(*KEY.get_or_init(|| fetched))
    }

    fn fetch_or_create() -> Option<[u8; 32]> {
        let entry = match keyring::Entry::new(SERVICE, ENTRY) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("OS keychain unavailable ({e}); keystore stays plaintext");
                return None;
            }
        };
        match entry.get_password() {
            Ok(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .ok()?;
                let key: [u8; 32] = bytes.try_into().ok()?;
                Some(key)
            }
            Err(keyring::Error::NoEntry) => {
                let mut key = [0u8; 32];
                rand::thread_rng().fill_bytes(&mut key);
                let b64 = base64::engine::general_purpose::STANDARD.encode(key);
                match entry.set_password(&b64) {
                    Ok(()) => {
                        tracing::info!("created keystore wrapping key in the OS keychain");
                        Some(key)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "cannot store wrapping key in the OS keychain ({e}); \
                             keystore stays plaintext"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("OS keychain unavailable ({e}); keystore stays plaintext");
                None
            }
        }
    }

    /// nonce(12) ‖ AES-256-GCM ciphertext+tag.
    pub fn seal_with(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new(key.into());
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut out = nonce.to_vec();
        // Encryption with a fresh random nonce cannot fail.
        let ct = cipher
            .encrypt(&Nonce::from(nonce), plain)
            .expect("AES-GCM seal");
        out.extend_from_slice(&ct);
        out
    }

    pub fn open_with(key: &[u8; 32], sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(sealed.len() > NONCE_LEN, "sealed keystore file truncated");
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split length");
        let cipher = Aes256Gcm::new(key.into());
        cipher
            .decrypt(&Nonce::from(nonce), ct)
            .map_err(|_| anyhow::anyhow!("keystore file failed authentication (wrong key?)"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seal_open_roundtrip_and_tamper_detection() {
            let key = [7u8; 32];
            let sealed = seal_with(&key, b"trust store json");
            assert_eq!(open_with(&key, &sealed).unwrap(), b"trust store json");
            let mut bad = sealed.clone();
            *bad.last_mut().unwrap() ^= 1;
            assert!(open_with(&key, &bad).is_err(), "tamper must fail");
            assert!(
                open_with(&[8u8; 32], &sealed).is_err(),
                "wrong key must fail"
            );
            // And the plaintext never appears in the sealed form.
            assert!(!sealed
                .windows(b"trust store json".len())
                .any(|w| w == b"trust store json"));
        }
    }
}

#[cfg(windows)]
pub fn protect(plain: &[u8]) -> Vec<u8> {
    match dpapi::protect(plain) {
        Ok(blob) => {
            let mut out = MAGIC.to_vec();
            out.extend_from_slice(&blob);
            out
        }
        Err(e) => {
            // Failing open (plaintext + NTFS ACLs) beats losing the trust
            // store; this is the pre-v0.5 status quo and is logged loudly.
            tracing::error!("DPAPI protect failed ({e:#}); storing plaintext");
            plain.to_vec()
        }
    }
}

#[cfg(windows)]
pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    match data.strip_prefix(MAGIC) {
        Some(blob) => dpapi::unprotect(blob),
        None => Ok(data.to_vec()), // legacy plaintext — upgraded on next save
    }
}

#[cfg(windows)]
mod dpapi {
    use anyhow::Context;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    const ENTROPY: &[u8] = b"ndsp-keystore-v1";

    fn blob_of(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copy a DPAPI output blob into a Vec and free the LocalAlloc buffer.
    unsafe fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        v
    }

    pub fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
        unsafe {
            let input = blob_of(plain);
            let entropy = blob_of(ENTROPY);
            let mut output = CRYPT_INTEGER_BLOB::default();
            CryptProtectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                0, // user scope, no UI
                &mut output,
            )
            .context("CryptProtectData")?;
            Ok(take_output(output))
        }
    }

    pub fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
        unsafe {
            let input = blob_of(blob);
            let entropy = blob_of(ENTROPY);
            let mut output = CRYPT_INTEGER_BLOB::default();
            CryptUnprotectData(&input, None, Some(&entropy), None, None, 0, &mut output)
                .context("CryptUnprotectData")?;
            Ok(take_output(output))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Windows CI this exercises real DPAPI; on Unix the pass-through.
    #[test]
    fn protect_unprotect_roundtrip() {
        let secret = b"token material \x00\xff\x10";
        let wrapped = protect(secret);
        assert_eq!(unprotect(&wrapped).unwrap(), secret);
    }

    #[cfg(windows)]
    #[test]
    fn windows_wrapped_form_is_not_plaintext() {
        let secret = b"super secret trust token";
        let wrapped = protect(secret);
        assert!(wrapped.starts_with(MAGIC));
        assert!(
            !wrapped
                .windows(secret.len())
                .any(|w| w == secret.as_slice()),
            "DPAPI output must not contain the plaintext"
        );
    }

    #[test]
    fn legacy_plaintext_reads_transparently() {
        #[cfg(windows)]
        assert_eq!(unprotect(b"legacy json").unwrap(), b"legacy json");
        #[cfg(not(windows))]
        assert_eq!(unprotect(b"legacy json").unwrap(), b"legacy json");
    }
}
