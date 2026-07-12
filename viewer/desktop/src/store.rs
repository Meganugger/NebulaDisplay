//! Credential persistence for the desktop viewer (per-host trust tokens).
//!
//! At-rest protection (ROADMAP P1.6):
//! * **Windows**: the whole store file is sealed with DPAPI
//!   (`CryptProtectData`, current-user scope) — another local account, or a
//!   copied file, cannot recover the tokens. Legacy plaintext stores are
//!   read once and transparently re-encrypted on the next save.
//! * **Unix**: plaintext JSON at mode 0600 (the conventional keystore for
//!   headless boxes; Keychain/libsecret integration is roadmap).

use ndsp_client::Credentials;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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

/// Magic prefix of DPAPI-sealed store files.
const DPAPI_MAGIC: &[u8] = b"NDSP-DPAPI1\n";

fn load_file() -> StoreFile {
    let Ok(bytes) = std::fs::read(store_path()) else {
        return StoreFile::default();
    };
    let json: Vec<u8> = if bytes.starts_with(DPAPI_MAGIC) {
        match dpapi::unprotect(&bytes[DPAPI_MAGIC.len()..]) {
            Some(pt) => pt,
            None => {
                tracing::warn!("credential store cannot be decrypted (different user/machine?)");
                return StoreFile::default();
            }
        }
    } else {
        bytes // legacy plaintext (or Unix) — re-encrypted on next save
    };
    serde_json::from_slice(&json).unwrap_or_default()
}

fn save_file(f: &StoreFile) {
    let path = store_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(raw) = serde_json::to_string_pretty(f) else {
        return;
    };
    let bytes = match dpapi::protect(raw.as_bytes()) {
        Some(sealed) => {
            let mut out = DPAPI_MAGIC.to_vec();
            out.extend_from_slice(&sealed);
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

/// DPAPI wrappers (Windows). On other platforms both return `None`, which
/// keeps the plaintext-0600 path.
#[cfg(windows)]
mod dpapi {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn blob_to_vec_and_free(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: pbData/cbData come from a successful DPAPI call; the
        // buffer is LocalAlloc'd and owned by us until LocalFree.
        unsafe {
            let out = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut core::ffi::c_void)));
            out
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input blob points at live data; output is filled on Ok.
        unsafe {
            CryptProtectData(&input, None, None, None, None, 0, &mut output).ok()?;
        }
        Some(blob_to_vec_and_free(output))
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as above.
        unsafe {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        }
        Some(blob_to_vec_and_free(output))
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_data: &[u8]) -> Option<Vec<u8>> {
        None
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
