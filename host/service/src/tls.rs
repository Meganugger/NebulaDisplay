//! Optional HTTPS/WSS for the LAN-facing viewer endpoint (`tls = true` in
//! `config.toml`).
//!
//! A per-install self-signed certificate is generated once and persisted in
//! the data dir; its SHA-256 fingerprint is logged at startup and exposed in
//! the panel status so native clients can **pin** it (browsers must accept
//! the self-signed warning once — the point of this mode is protecting the
//! *web viewer code* against on-path tampering on hostile networks; NDSP
//! itself is already end-to-end encrypted above the transport).

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;

const CERT_FILE: &str = "tls-cert.der";
const KEY_FILE: &str = "tls-key.der";

pub struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the DER certificate, lowercase hex — the pinnable value.
    pub fingerprint_hex: String,
}

/// SHA-256 fingerprint (lowercase hex) of a DER certificate.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Load the persisted TLS identity or create a fresh self-signed one.
pub fn load_or_create(data_dir: &Path) -> anyhow::Result<TlsIdentity> {
    let cert_path = data_dir.join(CERT_FILE);
    let key_path = data_dir.join(KEY_FILE);

    let (cert_der, key_der): (Vec<u8>, Vec<u8>) = if cert_path.exists() && key_path.exists() {
        (
            std::fs::read(&cert_path).context("reading tls-cert.der")?,
            std::fs::read(&key_path).context("reading tls-key.der")?,
        )
    } else {
        // SAN contents are irrelevant for fingerprint pinning; include the
        // common names so tooling that insists on SAN matching can be pointed
        // at "nebuladisplay.local".
        let names = vec!["nebuladisplay.local".to_string(), "localhost".to_string()];
        let certified = rcgen::generate_simple_self_signed(names)
            .context("generating self-signed certificate")?;
        let cert = certified.cert.der().to_vec();
        let key = certified.key_pair.serialize_der();
        std::fs::write(&cert_path, &cert).context("persisting tls-cert.der")?;
        std::fs::write(&key_path, &key).context("persisting tls-key.der")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
        (cert, key)
    };

    let fingerprint_hex = cert_fingerprint(&cert_der);
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_der)),
    )
    .context("building TLS server config")?;

    Ok(TlsIdentity {
        config: Arc::new(config),
        fingerprint_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_created_once_and_stable() {
        let dir = std::env::temp_dir().join(format!("ndsp-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create(&dir).unwrap();
        let b = load_or_create(&dir).unwrap();
        assert_eq!(
            a.fingerprint_hex, b.fingerprint_hex,
            "identity must persist"
        );
        assert_eq!(a.fingerprint_hex.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
