//! At-rest protection for secret material (the trust store).
//!
//! * **Windows**: DPAPI (`CryptProtectData`, current-user scope) — the blob
//!   can only be decrypted by the same Windows user on the same machine, so
//!   copying `devices.json` off the box (or reading it as another local user)
//!   yields nothing. No extra key management, which is exactly the point.
//! * **Other hosts**: no OS-wide equivalent is guaranteed to exist headless;
//!   the store stays plaintext-with-0600 as before (documented in
//!   `docs/SECURITY.md`).
//!
//! The on-disk container is self-describing (`{"dpapi": "<base64>"}`), so a
//! store written by an older plaintext build is read transparently and
//! upgraded on the next write.

/// Encrypt `plain` for the current OS user. Returns `None` when the platform
/// has no keystore backend (caller persists plaintext as before).
#[cfg(windows)]
pub fn protect(plain: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    dpapi::protect(plain).map(Some)
}

/// Decrypt a blob produced by [`protect`] on this machine/user.
#[cfg(windows)]
pub fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    dpapi::unprotect(blob)
}

#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn unprotect(_blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("OS keystore not available on this platform")
}

#[cfg(windows)]
mod dpapi {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    /// Copy the DPAPI output buffer and release the LocalAlloc'd original.
    ///
    /// # Safety
    /// `out` must be a blob filled in by a successful DPAPI call.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        v
    }

    pub fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(plain);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input points at live memory for the duration of the call;
        // out is written by the API and released in `take`.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| anyhow::anyhow!("CryptProtectData: {e}"))?;
            Ok(take(out))
        }
    }

    pub fn unprotect(sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(sealed);
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
            .map_err(|e| anyhow::anyhow!("CryptUnprotectData: {e}"))?;
            Ok(take(out))
        }
    }
}
