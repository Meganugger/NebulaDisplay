//! Self-signed TLS material, generated once and persisted.
//!
//! Browsers will show a one-time warning for the self-signed certificate on
//! first visit; the certificate's SHA-256 fingerprint is displayed on the
//! host control panel and embedded in QR pairing payloads so users (and
//! non-browser viewers, which pin the fingerprint) can verify they are
//! talking to the right machine and not a spoofed host.

use std::path::Path;

use anyhow::Context;
use sha2::{Digest, Sha256};

pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    /// SHA-256 of the DER certificate, colon-separated hex (like browsers show).
    pub fingerprint: String,
}

/// Load persisted cert/key, or generate a fresh self-signed pair valid for
/// local hostnames and typical LAN usage.
pub fn load_or_generate(config_path: &Path) -> anyhow::Result<TlsMaterial> {
    let dir = crate::config::Config::data_dir(config_path);
    let cert_path = dir.join("tls_cert.pem");
    let key_path = dir.join("tls_key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        let fingerprint = fingerprint_from_pem(&cert_pem)?;
        return Ok(TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint,
        });
    }

    let mut params = rcgen::CertificateParams::new(vec![
        "localhost".to_string(),
        "nebuladisplay.local".to_string(),
    ])
    .context("certificate params")?;
    params.distinguished_name = {
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "NebulaDisplay Host");
        dn
    };
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&cert_path, &cert_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    restrict_permissions(&key_path);

    let fingerprint = fingerprint_from_pem(&cert_pem)?;
    Ok(TlsMaterial {
        cert_pem,
        key_pem,
        fingerprint,
    })
}

fn fingerprint_from_pem(pem: &str) -> anyhow::Result<String> {
    let der = pem_to_der(pem).context("invalid certificate PEM")?;
    let digest = Sha256::digest(&der);
    Ok(digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// Minimal PEM → DER decoding (base64 body between BEGIN/END markers).
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN CERTIFICATE-----")? + "-----BEGIN CERTIFICATE-----".len();
    let end = pem.find("-----END CERTIFICATE-----")?;
    let body: String = pem[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64_decode(&body)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        let mut bits = 0;
        for &b in chunk {
            let v = rev[b as usize];
            if v == 255 {
                return None;
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).ok();
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {
    // On Windows the key inherits the per-user %APPDATA% ACL, which is
    // already restricted to the current user.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_reloads() {
        let dir = std::env::temp_dir().join(format!("nebula-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("host.toml");
        let a = load_or_generate(&cfg).unwrap();
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(!a.fingerprint.is_empty());
        // Second call must load the same material.
        let b = load_or_generate(&cfg).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn base64_decoder_works() {
        assert_eq!(base64_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64_decode("!!!"), None);
    }
}
