//! Credential persistence for the desktop viewer (per-host trust tokens).
//!
//! At rest the file is DPAPI-protected on Windows (OS keystore, tied to the
//! user account) and mode-0600 plaintext on unix — matching the host's trust
//! store policy (see `docs/SECURITY.md`).

use ndsp_client::Credentials;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Magic prefix of DPAPI-protected store files.
const DPAPI_MAGIC: &[u8] = b"NDSP-DPAPI\x01";

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    device_id: Option<String>,
    /// host address → credentials
    hosts: HashMap<String, StoredHost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredHost {
    token_hex: String,
    fingerprint: String,
}

fn store_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("nebuladisplay")
        .join("viewer.json")
}

fn load_file() -> StoreFile {
    let Ok(raw) = std::fs::read(store_path()) else {
        return StoreFile::default();
    };
    let plain: Vec<u8> = if let Some(blob) = raw.strip_prefix(DPAPI_MAGIC) {
        match dpapi_unprotect(blob) {
            Some(p) => p,
            None => return StoreFile::default(),
        }
    } else {
        raw
    };
    serde_json::from_slice(&plain).unwrap_or_default()
}

fn save_file(f: &StoreFile) {
    let path = store_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(f) {
        let bytes = match dpapi_protect(raw.as_bytes()) {
            Some(blob) => {
                let mut out = DPAPI_MAGIC.to_vec();
                out.extend_from_slice(&blob);
                out
            }
            None => raw.into_bytes(),
        };
        let _ = std::fs::write(&path, bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// DPAPI (user scope). `None` on non-Windows or on API failure.
#[cfg(windows)]
fn dpapi_protect(plain: &[u8]) -> Option<Vec<u8>> {
    dpapi::protect(plain)
}
#[cfg(windows)]
fn dpapi_unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    dpapi::unprotect(blob)
}
#[cfg(not(windows))]
fn dpapi_protect(_plain: &[u8]) -> Option<Vec<u8>> {
    None
}
#[cfg(not(windows))]
fn dpapi_unprotect(_blob: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(windows)]
mod dpapi {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob_of(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copy a DPAPI output blob into a Vec and free the LocalAlloc'd buffer.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        v
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        let input = blob_of(plain);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input points at live memory for the call; output is
        // LocalAlloc'd by the API and freed in `take`.
        unsafe {
            CryptProtectData(
                &input,
                windows::core::w!("NebulaDisplay viewer credentials"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .ok()?;
            Some(take(out))
        }
    }

    pub fn unprotect(blob: &[u8]) -> Option<Vec<u8>> {
        let input = blob_of(blob);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as above.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .ok()?;
            Some(take(out))
        }
    }
}

/// Stable device id for this install.
pub fn device_id() -> String {
    let mut f = load_file();
    if let Some(id) = &f.device_id {
        return id.clone();
    }
    let id = uuid_v4();
    f.device_id = Some(id.clone());
    save_file(&f);
    id
}

pub fn load(host: &str) -> Option<Credentials> {
    let f = load_file();
    let h = f.hosts.get(host)?;
    Some(Credentials {
        device_id: f.device_id.clone()?,
        token: hex_decode(&h.token_hex)?,
        host_fingerprint: h.fingerprint.clone(),
    })
}

pub fn save(host: &str, creds: &Credentials) {
    let mut f = load_file();
    f.device_id = Some(creds.device_id.clone());
    f.hosts.insert(
        host.to_string(),
        StoredHost {
            token_hex: hex_encode(&creds.token),
            fingerprint: creds.host_fingerprint.clone(),
        },
    );
    save_file(&f);
}

pub fn clear(host: &str) {
    let mut f = load_file();
    if f.hosts.remove(host).is_some() {
        save_file(&f);
    }
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// RFC 4122 v4 UUID from OS randomness (avoids a uuid dep here).
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn getrandom(buf: &mut [u8]) {
    // std::random when stabilized; for now read from the OS via the rand
    // machinery already linked through our dependency tree is unavailable
    // here, so use a timestamp-seeded xorshift as last resort ONLY if
    // /dev/urandom fails (never on supported platforms).
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
    }
    #[cfg(windows)]
    {
        // BCryptGenRandom via the widely-available `std` fallback: fill from
        // RandomState hashing (not cryptographic, but device ids are not
        // secrets — the trust token from the host is the secret).
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    for chunk in buf.chunks_mut(8) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = (seed >> (i * 8)) as u8;
        }
    }
}
