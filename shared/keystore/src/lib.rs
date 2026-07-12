//! At-rest protection for NebulaDisplay secrets (trust tokens, identity).
//!
//! * **Windows**: DPAPI (`CryptProtectData`, per-user scope) — the OS ties
//!   the key to the user's logon credentials, so `devices.json` copied off
//!   the machine (or read by another local account) is useless.
//! * **Unix**: no ubiquitous keystore exists at this layer; files stay
//!   plaintext at mode 0600 (documented in docs/SECURITY.md). macOS
//!   Keychain / Android Keystore integration lives in the platform viewers.
//!
//! File format: protected payloads are wrapped as `NDSPDPAPI1 || blob` so
//! [`open_file`] can transparently read both protected and legacy plaintext
//! files (migration happens on the next save).

/// Magic prefix identifying a DPAPI-protected NebulaDisplay file.
pub const MAGIC: &[u8] = b"NDSPDPAPI1";

/// Extra entropy mixed into DPAPI so other apps in the same user session
/// can't trivially call `CryptUnprotectData` on our blobs without also
/// knowing this constant (defense in depth, not a boundary).
#[cfg(windows)]
const ENTROPY: &[u8] = b"ndsp-keystore-v1";

/// True when payloads will actually be protected on this platform.
pub fn is_protected() -> bool {
    cfg!(windows)
}

/// Protect `plaintext` for storage. On non-Windows this is the identity
/// function (callers must still write files with restrictive permissions).
pub fn seal(plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    #[cfg(windows)]
    {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&dpapi::protect(plaintext)?);
        Ok(out)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext.to_vec())
    }
}

/// Inverse of [`seal`]; transparently accepts legacy plaintext files that
/// predate protection (no `MAGIC` prefix).
pub fn open_file(content: &[u8]) -> anyhow::Result<Vec<u8>> {
    let Some(blob) = content.strip_prefix(MAGIC) else {
        return Ok(content.to_vec());
    };
    #[cfg(windows)]
    {
        dpapi::unprotect(blob)
    }
    #[cfg(not(windows))]
    {
        let _ = blob;
        anyhow::bail!(
            "file is DPAPI-protected and can only be read on the Windows machine that wrote it"
        )
    }
}

#[cfg(windows)]
mod dpapi {
    use super::ENTROPY;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copy a DPAPI output blob into a Vec and free the LocalAlloc buffer.
    ///
    /// # Safety
    /// `out` must be a blob returned by a successful DPAPI call.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        v
    }

    pub fn protect(plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(plaintext);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input/entropy blobs point at live slices for the duration
        // of the call; `out` is freed by `take`.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                Some(&entropy),
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
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as above.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sealed = seal(b"secret token material").unwrap();
        let opened = open_file(&sealed).unwrap();
        assert_eq!(opened, b"secret token material");
        if is_protected() {
            assert!(sealed.starts_with(MAGIC));
            assert_ne!(&sealed[MAGIC.len()..], b"secret token material".as_slice());
        }
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        assert_eq!(open_file(b"{\"devices\":[]}").unwrap(), b"{\"devices\":[]}");
    }
}
