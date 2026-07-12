//! Optional HTTPS for the viewer endpoint (roadmap P1.7).
//!
//! `https = true` in `config.toml` serves the web viewer + NDSP WebSocket
//! over TLS with a **persisted self-signed certificate**. What this buys on
//! a hostile LAN:
//!
//! * **Code integrity for the web viewer**: NDSP itself is end-to-end
//!   encrypted even over plain HTTP, but the *JavaScript* that implements
//!   the viewer is fetched from the host — on plain HTTP an active attacker
//!   could tamper with it in transit. TLS + the browser remembering the
//!   accepted certificate closes that hole.
//! * **Secure context**: browsers unlock native WebCrypto + WebCodecs, so
//!   the viewer runs its fastest decode path instead of the MSE fallback.
//!
//! The certificate is generated once (rcgen, P-256, 10-year validity, SANs
//! for the current LAN IPs + localhost) and stored in `<data_dir>/tls/`.
//! Its SHA-256 fingerprint is printed at startup and shown in the panel so
//! users can compare what the browser reports before clicking through the
//! one-time warning.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::info;

pub struct TlsMaterial {
    pub cert_pem_path: PathBuf,
    pub key_pem_path: PathBuf,
    /// SHA-256 of the certificate DER, hex.
    pub fingerprint: String,
}

/// Load the persisted cert/key, generating them on first use.
pub fn load_or_create(data_dir: &Path) -> anyhow::Result<TlsMaterial> {
    let dir = data_dir.join("tls");
    std::fs::create_dir_all(&dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if !(cert_path.exists() && key_path.exists()) {
        let mut names: Vec<String> = vec!["localhost".into(), "nebuladisplay.local".into()];
        for ip in crate::util::local_ips() {
            names.push(ip.to_string());
        }
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(names)?;
        std::fs::write(&cert_path, cert.pem())?;
        std::fs::write(&key_path, key_pair.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
        info!("generated self-signed TLS certificate in {}", dir.display());
    }

    let fingerprint = fingerprint_of(&cert_path)?;
    Ok(TlsMaterial {
        cert_pem_path: cert_path,
        key_pem_path: key_path,
        fingerprint,
    })
}

/// SHA-256 fingerprint (hex) of the first certificate in a PEM file.
pub fn fingerprint_of(cert_pem_path: &Path) -> anyhow::Result<String> {
    let pem = std::fs::read_to_string(cert_pem_path)?;
    let der = pem_to_der(&pem)?;
    Ok(hex::encode(Sha256::digest(&der)))
}

fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let b64: String = pem
        .lines()
        .skip_while(|l| !l.contains("BEGIN CERTIFICATE"))
        .skip(1)
        .take_while(|l| !l.contains("END CERTIFICATE"))
        .collect();
    anyhow::ensure!(!b64.is_empty(), "no certificate in PEM");
    Ok(B64.decode(b64.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_once_and_fingerprint_is_stable() {
        let dir = std::env::temp_dir().join(format!("ndsp-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create(&dir).unwrap();
        let b = load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "cert must persist");
        assert_eq!(a.fingerprint.len(), 64);
    }
}
