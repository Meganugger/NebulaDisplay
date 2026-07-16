//! Optional HTTPS for the viewer endpoint (ROADMAP P1.7).
//!
//! `--https` serves the viewer page + WebSocket over TLS with a
//! **self-signed certificate generated once and persisted** in the data
//! directory. The point is not CA trust (there is none on a LAN) — it is:
//!
//! 1. *Code integrity with pinning*: the certificate fingerprint is shown in
//!    the panel/banner; a user who checks it once gets tamper-proof viewer
//!    JS from then on (browsers pin the accepted cert per origin for the
//!    session, and the fingerprint never changes across restarts).
//! 2. *Secure-context features*: WebCodecs (H.264/Opus decode), WebCrypto
//!    and `navigator.clipboard` only exist on secure origins — HTTPS turns
//!    them all on for LAN addresses.
//!
//! The private key is stored next to the trust store with the same
//! protections (0600 / DPAPI-wrapped on Windows).

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct TlsMaterial {
    /// PEM certificate chain (single self-signed cert).
    pub cert_pem: String,
    /// PEM private key.
    pub key_pem: String,
    /// SHA-256 of the DER certificate, colon-separated hex (the string
    /// browsers show, and what users compare against the panel).
    pub fingerprint: String,
}

/// Load the persisted certificate or create one. The cert is deliberately
/// long-lived and reused forever — fingerprint stability *is* the trust
/// model here.
pub fn load_or_create(data_dir: &Path) -> anyhow::Result<TlsMaterial> {
    let cert_path = data_dir.join("tls-cert.pem");
    let key_path = data_dir.join("tls-key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path).context("reading tls-cert.pem")?;
        let key_raw = std::fs::read(&key_path).context("reading tls-key.pem")?;
        let key_pem = String::from_utf8(crate::keystore::unprotect(&key_raw)?)
            .context("tls-key.pem is not valid UTF-8")?;
        let fingerprint = fingerprint_of_pem(&cert_pem)?;
        return Ok(TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint,
        });
    }

    // Subject alt names: everything a viewer might type. Self-signed certs
    // warn regardless; SANs just keep the warning generic.
    let mut sans: Vec<String> = vec!["localhost".into(), "nebuladisplay.local".into()];
    for ip in crate::util::local_ips() {
        sans.push(ip.to_string());
    }
    let mut params = rcgen::CertificateParams::new(sans).context("certificate params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "NebulaDisplay Host");
    let key = rcgen::KeyPair::generate().context("generating TLS key")?;
    let cert = params.self_signed(&key).context("self-signing cert")?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();

    std::fs::write(&cert_path, &cert_pem).context("writing tls-cert.pem")?;
    std::fs::write(&key_path, crate::keystore::protect(key_pem.as_bytes()))
        .context("writing tls-key.pem")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    let fingerprint = fingerprint_of_pem(&cert_pem)?;
    tracing::info!(%fingerprint, "generated self-signed TLS certificate");
    Ok(TlsMaterial {
        cert_pem,
        key_pem,
        fingerprint,
    })
}

/// SHA-256 over the DER cert, `AA:BB:…` formatted.
fn fingerprint_of_pem(cert_pem: &str) -> anyhow::Result<String> {
    let der = pem_to_der(cert_pem).context("parsing certificate PEM")?;
    let digest = Sha256::digest(&der);
    Ok(digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// Minimal PEM → DER (first CERTIFICATE block).
fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    let begin = "-----BEGIN CERTIFICATE-----";
    let end = "-----END CERTIFICATE-----";
    let start = pem.find(begin).context("no BEGIN CERTIFICATE")? + begin.len();
    let stop = pem[start..]
        .find(end)
        .map(|i| start + i)
        .context("no END CERTIFICATE")?;
    let b64: String = pem[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("certificate base64")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_persists_with_stable_fingerprint() {
        let dir = std::env::temp_dir().join(format!("ndsp-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint.len(), 32 * 3 - 1, "AA:BB… format");
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(a.key_pem.contains("PRIVATE KEY"));
        // Reload must return the identical certificate — fingerprint
        // stability across restarts is the pinning contract.
        let b = load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.cert_pem, b.cert_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
