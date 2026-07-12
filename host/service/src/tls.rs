//! Optional HTTPS for the viewer endpoint (ROADMAP P1.7).
//!
//! NDSP traffic is end-to-end encrypted regardless of transport; what plain
//! HTTP cannot protect is the *viewer page's code* — a hostile LAN box could
//! serve a tampered viewer. With `https = true` the host serves everything
//! over TLS using a **persistent self-signed certificate** whose SHA-256
//! fingerprint is printed in the banner and shown in the panel, so users
//! (and pinning clients like the desktop viewer's `--tls-fingerprint`) can
//! verify they reached the real host once and be safe forever after.
//!
//! No CA, no ACME, no cloud — the certificate is generated locally on first
//! use and reused so the fingerprint stays stable across restarts.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Once;

pub struct TlsIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// SHA-256 of the DER certificate, lowercase hex.
    pub fingerprint: String,
}

/// Install the process-wide rustls crypto provider exactly once.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Load the persistent TLS identity, generating it on first use.
pub fn load_or_create(data_dir: &Path, host_name: &str) -> anyhow::Result<TlsIdentity> {
    let cert_path = data_dir.join("https-cert.der");
    let key_path = data_dir.join("https-key.der");
    if cert_path.exists() && key_path.exists() {
        let cert_der = std::fs::read(&cert_path)?;
        let key_der = std::fs::read(&key_path)?;
        let fingerprint = hex::encode(Sha256::digest(&cert_der));
        return Ok(TlsIdentity {
            cert_der,
            key_der,
            fingerprint,
        });
    }

    // Subject alt names: hostname + every local IP we can see now. Browsers
    // will warn on a self-signed cert regardless; SANs just keep the warning
    // to "unknown issuer" instead of "wrong host".
    let mut sans: Vec<String> = vec![host_name.to_string(), "localhost".into()];
    for ip in crate::util::local_ips() {
        sans.push(ip.to_string());
    }
    sans.push("127.0.0.1".into());
    sans.sort();
    sans.dedup();

    let mut params = rcgen::CertificateParams::new(sans).context("certificate params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "NebulaDisplay self-signed");
    let key = rcgen::KeyPair::generate().context("generating TLS key")?;
    let cert = params.self_signed(&key).context("self-signing")?;
    let cert_der = cert.der().to_vec();
    let key_der = key.serialize_der();

    std::fs::write(&cert_path, &cert_der)?;
    std::fs::write(&key_path, &key_der)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }
    let fingerprint = hex::encode(Sha256::digest(&cert_der));
    tracing::info!(
        fingerprint,
        "generated persistent self-signed HTTPS certificate"
    );
    Ok(TlsIdentity {
        cert_der,
        key_der,
        fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_persistent_and_fingerprint_stable() {
        let dir = std::env::temp_dir().join(format!("ndsp-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create(&dir, "test-host").unwrap();
        assert_eq!(a.fingerprint.len(), 64);
        let b = load_or_create(&dir, "test-host").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "must reuse the stored cert");
        assert_eq!(a.cert_der, b.cert_der);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
