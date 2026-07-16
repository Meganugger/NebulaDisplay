//! Handshake and session crypto for NDSP v1.
//!
//! ## Pairing (first contact)
//!
//! ```text
//! client                                server (displays PIN)
//!   | PairStart(eph pubkey C)             |
//!   |------------------------------------>|
//!   |            PairChallenge(eph S, salt)|
//!   |<------------------------------------|
//!   shared = ECDH(c, S) = ECDH(s, C)
//!   pair_key = HKDF-SHA256(ikm=shared, salt, info="ndsp-pair-v1"||PIN||nonce)
//!   | PairConfirm(GCM_seal(pair_key, "ndsp-confirm-v1"||nonce))
//!   |------------------------------------>|  server verifies → client knew PIN
//!   |     PairResult(GCM_seal(pair_key, trust_token))
//!   |<------------------------------------|
//! session_key = HKDF-SHA256(ikm=shared, salt, info="ndsp-session-v1"||nonce)
//! ```
//!
//! The PIN never crosses the wire. A passive attacker recording the exchange
//! can offline-brute-force the (short) PIN, which is why PINs are single-use,
//! short-lived, and rate-limited; a PAKE (SPAKE2/OPAQUE) upgrade is planned —
//! see `docs/SECURITY.md`. An *active* MITM without the PIN cannot complete
//! `PairConfirm` and is rejected before any screen data flows.
//!
//! ## Returning devices
//!
//! Token auth proves possession of the 256-bit trust token by hashing it with
//! the per-connection nonce; the session key is still fresh ECDH-derived, so
//! stolen transcripts don't decrypt other sessions.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{ecdh::EphemeralSecret, PublicKey};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::{ProtocolError, Result};

pub const CONFIRM_CONTEXT: &[u8] = b"ndsp-confirm-v1";
pub const PAIR_INFO: &[u8] = b"ndsp-pair-v1";
pub const SESSION_INFO: &[u8] = b"ndsp-session-v1";
pub const TOKEN_LEN: usize = 32;

/// One side's ephemeral ECDH keypair for a handshake.
pub struct HandshakeKeys {
    secret: EphemeralSecret,
    public_sec1: Vec<u8>,
}

impl HandshakeKeys {
    pub fn generate() -> Self {
        let secret = EphemeralSecret::random(&mut OsRng);
        // Explicitly uncompressed SEC1 (65 bytes, 0x04-prefixed): the only raw
        // EC point encoding every browser's WebCrypto importKey("raw") accepts.
        let public_sec1 = secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        Self {
            secret,
            public_sec1,
        }
    }

    /// Uncompressed SEC1 encoding of our ephemeral public key.
    pub fn public_bytes(&self) -> &[u8] {
        &self.public_sec1
    }

    /// Complete ECDH with the peer's public key, producing the raw shared
    /// secret used as HKDF input keying material.
    pub fn agree(self, peer_sec1: &[u8]) -> Result<SharedSecret> {
        let peer = PublicKey::from_sec1_bytes(peer_sec1)
            .map_err(|_| ProtocolError::Crypto("invalid peer public key"))?;
        let shared = self.secret.diffie_hellman(&peer);
        Ok(SharedSecret(shared.raw_secret_bytes().to_vec()))
    }
}

pub struct SharedSecret(Vec<u8>);

impl SharedSecret {
    /// Key that gates pairing on knowledge of the PIN (legacy path; see
    /// [`crate::pake`] for the SPAKE2 replacement that removes the
    /// offline-grinding caveat).
    pub fn pairing_key(&self, salt: &[u8], pin: &str, connection_nonce: &[u8]) -> [u8; 32] {
        derive_key(
            &self.0,
            salt,
            &[PAIR_INFO, pin.as_bytes(), connection_nonce],
        )
    }

    /// Post-handshake transport key (independent of the PIN so a later PIN
    /// leak can't decrypt recorded sessions beyond the pairing exchange).
    pub fn session_key(&self, salt: &[u8], connection_nonce: &[u8]) -> [u8; 32] {
        derive_key(&self.0, salt, &[SESSION_INFO, connection_nonce])
    }
}

/// HKDF-SHA256 → 32-byte key with a concatenated multi-part info string.
pub(crate) fn derive_key(ikm: &[u8], salt: &[u8], info_parts: &[&[u8]]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let info: Vec<u8> = info_parts.concat();
    let mut okm = [0u8; 32];
    hk.expand(&info, &mut okm)
        .expect("32 bytes is valid for SHA-256 HKDF");
    okm
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// Generate a human-friendly numeric PIN of `digits` length (leading zeros ok).
pub fn generate_pin(digits: u32) -> String {
    let max = 10u64.pow(digits);
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    let n = u64::from_le_bytes(buf) % max;
    format!("{n:0width$}", width = digits as usize)
}

/// Seal `plaintext` with AES-256-GCM under `key` using a random 12-byte nonce.
/// Output layout: `nonce(12) || ciphertext || tag(16)`.
pub fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(key.into());
    let nonce_bytes: [u8; 12] = random_bytes();
    let mut out = nonce_bytes.to_vec();
    let ct = cipher
        .encrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("AES-GCM encryption is infallible for valid inputs");
    out.extend_from_slice(&ct);
    out
}

/// Inverse of [`seal`].
pub fn open(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 12 + 16 {
        return Err(ProtocolError::Crypto("sealed blob too short"));
    }
    let (nonce, ct) = sealed.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().expect("split_at(12) guarantees length");
    let cipher = Aes256Gcm::new(key.into());
    cipher
        .decrypt(&Nonce::from(nonce), Payload { msg: ct, aad })
        .map_err(|_| ProtocolError::Crypto("AEAD open failed (wrong key/PIN or tampering)"))
}

/// Proof of trust-token possession: SHA-256(token || connection_nonce).
pub fn token_proof(token: &[u8], connection_nonce: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(token);
    h.update(connection_nonce);
    h.finalize().into()
}

/// Transcript covered by a reconnect token proof: binds the proof to this
/// exact handshake (nonce + both ephemeral public keys) so an active MITM
/// substituting keys invalidates it.
pub fn reauth_transcript(connection_nonce: &[u8], client_pub: &[u8], server_pub: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(connection_nonce.len() + client_pub.len() + server_pub.len());
    t.extend_from_slice(connection_nonce);
    t.extend_from_slice(client_pub);
    t.extend_from_slice(server_pub);
    t
}

/// What the server stores at rest: SHA-256(token). The raw token lives only
/// on the client device.
pub fn token_hash(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecdh_both_sides_agree() {
        let a = HandshakeKeys::generate();
        let b = HandshakeKeys::generate();
        let a_pub = a.public_bytes().to_vec();
        let b_pub = b.public_bytes().to_vec();
        let sa = a.agree(&b_pub).unwrap();
        let sb = b.agree(&a_pub).unwrap();
        let salt = [7u8; 16];
        let nonce = [9u8; 16];
        assert_eq!(
            sa.pairing_key(&salt, "123456", &nonce),
            sb.pairing_key(&salt, "123456", &nonce)
        );
        assert_eq!(sa.session_key(&salt, &nonce), sb.session_key(&salt, &nonce));
    }

    #[test]
    fn wrong_pin_derives_different_key_and_fails_open() {
        let a = HandshakeKeys::generate();
        let b = HandshakeKeys::generate();
        let b_pub = b.public_bytes().to_vec();
        let a_pub = a.public_bytes().to_vec();
        let sa = a.agree(&b_pub).unwrap();
        let sb = b.agree(&a_pub).unwrap();
        let salt = [1u8; 16];
        let nonce = [2u8; 16];
        let k_good = sa.pairing_key(&salt, "111111", &nonce);
        let k_bad = sb.pairing_key(&salt, "111112", &nonce);
        let sealed = seal(&k_good, b"payload", b"");
        assert!(open(&k_bad, &sealed, b"").is_err());
        assert_eq!(open(&k_good, &sealed, b"").unwrap(), b"payload");
    }

    #[test]
    fn pin_has_requested_length() {
        for _ in 0..50 {
            assert_eq!(generate_pin(6).len(), 6);
        }
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let key = [3u8; 32];
        let mut sealed = seal(&key, b"secret", b"aad");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(open(&key, &sealed, b"aad").is_err());
    }
}
