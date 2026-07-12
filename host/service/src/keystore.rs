//! At-rest protection for secret files (trust store).
//!
//! * **Windows**: DPAPI (`CryptProtectData` / `CryptUnprotectData`,
//!   user scope) — the OS keystore ties the blob to this user account, so a
//!   copied `devices.json` is useless on another machine/account.
//! * **Unix**: plaintext with mode 0600 (the caller sets permissions). A
//!   passphrase-less local service has no OS-provided secret store that is
//!   stronger than file permissions on stock Linux; macOS Keychain support
//!   is tracked in `docs/ROADMAP.md`.
//!
//! Format: protected files start with the magic `NDSP-DPAPI\x01` followed by
//! the raw DPAPI blob; anything else is treated as plaintext (transparent
//! migration — old plaintext stores load fine and are protected on the next
//! save).

const MAGIC: &[u8] = b"NDSP-DPAPI\x01";

/// Protect `plain` for at-rest storage. Returns the bytes to write.
/// On non-Windows this is the identity function.
pub fn protect(plain: &[u8]) -> Vec<u8> {
    #[cfg(windows)]
    {
        match dpapi::protect(plain) {
            Ok(blob) => {
                let mut out = Vec::with_capacity(MAGIC.len() + blob.len());
                out.extend_from_slice(MAGIC);
                out.extend_from_slice(&blob);
                return out;
            }
            Err(e) => {
                tracing::warn!("DPAPI protect failed ({e}); storing plaintext (0600-equivalent)");
            }
        }
    }
    plain.to_vec()
}

/// Inverse of [`protect`]. Plaintext (unprefixed) input passes through.
pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if let Some(blob) = data.strip_prefix(MAGIC) {
        #[cfg(windows)]
        {
            return dpapi::unprotect(blob);
        }
        #[cfg(not(windows))]
        {
            let _ = blob;
            anyhow::bail!(
                "store is DPAPI-protected (written on Windows) and cannot be read on this OS"
            );
        }
    }
    Ok(data.to_vec())
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

    pub fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob_of(plain);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input blob points at live memory for the duration of the
        // call; output blob is LocalAlloc'd by the API and freed in `take`.
        unsafe {
            CryptProtectData(
                &input,
                windows::core::w!("NebulaDisplay trust store"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
            Ok(take(out))
        }
    }

    pub fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
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
            )?;
            Ok(take(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"{\"devices\":[]}";
        let stored = protect(data);
        assert_eq!(unprotect(&stored).unwrap(), data);
    }

    #[cfg(not(windows))]
    #[test]
    fn foreign_dpapi_blob_errors_cleanly() {
        let mut fake = MAGIC.to_vec();
        fake.extend_from_slice(b"opaque");
        assert!(unprotect(&fake).is_err());
    }
}
