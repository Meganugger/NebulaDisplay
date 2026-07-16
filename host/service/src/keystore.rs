//! At-rest protection for host secrets (ROADMAP P1.6).
//!
//! * **Windows**: secrets (trust store, identity key, TLS private key) are
//!   wrapped with **DPAPI** (`CryptProtectData`, user scope) — another local
//!   account, or the same files exfiltrated to another machine, cannot read
//!   them. Files carry the `NDPP1\n` magic prefix.
//! * **Unix**: files stay plaintext with `0600` permissions (the historical
//!   behavior; there is no universally-present keystore daemon to target).
//!
//! Reads are transparently backward compatible: a legacy plaintext file is
//! returned as-is and upgraded to the wrapped format on the next write.

/// Magic prefix marking a DPAPI-wrapped payload.
pub const MAGIC: &[u8] = b"NDPP1\n";

/// Wrap plaintext for at-rest storage. Pass-through on non-Windows.
#[cfg(not(windows))]
pub fn protect(plain: &[u8]) -> Vec<u8> {
    plain.to_vec()
}

/// Unwrap data read from disk. Accepts both wrapped and legacy plaintext.
#[cfg(not(windows))]
pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    // A Windows-DPAPI-protected file copied to a Unix host cannot be
    // unwrapped here — which is exactly the property DPAPI provides.
    anyhow::ensure!(
        !data.starts_with(MAGIC),
        "this file was protected with Windows DPAPI on another machine"
    );
    Ok(data.to_vec())
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
