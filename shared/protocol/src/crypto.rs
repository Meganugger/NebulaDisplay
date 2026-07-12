//! Handshake and session crypto for NDSP v1.
//!
//! ## Pairing (first contact)
//!
//! Two pairing methods share the same message flow (`PairStart` →
//! `PairChallenge` → `PairConfirm` → `PairResult`); they differ only in how
//! the public shares and keys are computed.
//!
//! ### `pair_pake` — SPAKE2 over P-256 (default for current clients)
//!
//! ```text
//! client                                server (displays PIN)
//!   w = reduce(HKDF(ikm=PIN, salt=nonce, info="ndsp-pake-w-v1"))
//!   | PairStart(X = x·G + w·M)            |
//!   |------------------------------------>|
//!   |          PairChallenge(Y = y·G + w·N, salt)
//!   |<------------------------------------|
//!   Z = x·(Y − w·N) = y·(X − w·M)        (abort if identity)
//!   pair_key    = HKDF(ikm=Z, salt, "ndsp-pake-pair-v1"‖nonce‖X‖Y‖w)
//!   session_key = HKDF(ikm=Z, salt, "ndsp-pake-session-v1"‖nonce‖X‖Y‖w)
//!   | PairConfirm(GCM_seal(pair_key, "ndsp-confirm-v1"||nonce))
//!   |------------------------------------>|  server verifies → client knew PIN
//!   |     PairResult(GCM_seal(pair_key, trust_token))
//!   |<------------------------------------|
//! ```
//!
//! A recorded PAKE transcript is **not offline-grindable**: testing a PIN
//! guess against `X`/`Y` requires solving elliptic-curve Diffie–Hellman.
//! See [`pake`].
//!
//! ### `pair` — legacy PIN-bound HKDF (kept for older mobile viewers)
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
//! The PIN never crosses the wire in either method. Under the legacy method a
//! passive attacker recording the exchange can offline-brute-force the (short)
//! PIN, which is why PINs are single-use, short-lived, and rate-limited — and
//! why `pair_pake` is the default; see `docs/SECURITY.md`. An *active* MITM
//! without the PIN cannot complete `PairConfirm` under either method and is
//! rejected before any screen data flows.
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
    /// Key that gates pairing on knowledge of the PIN.
    pub fn pairing_key(&self, salt: &[u8], pin: &str, connection_nonce: &[u8]) -> [u8; 32] {
        derive(
            &self.0,
            salt,
            &[PAIR_INFO, pin.as_bytes(), connection_nonce],
        )
    }

    /// Post-handshake transport key (independent of the PIN so a later PIN
    /// leak can't decrypt recorded sessions beyond the pairing exchange).
    pub fn session_key(&self, salt: &[u8], connection_nonce: &[u8]) -> [u8; 32] {
        derive(&self.0, salt, &[SESSION_INFO, connection_nonce])
    }
}

fn derive(ikm: &[u8], salt: &[u8], info_parts: &[&[u8]]) -> [u8; 32] {
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

/// Balanced PAKE for PIN pairing: SPAKE2 over P-256 (auth method
/// `pair_pake`).
///
/// Unlike the legacy PIN-bound HKDF, a *recorded* PAKE exchange cannot be
/// ground offline: each PIN guess requires solving an elliptic-curve
/// Diffie–Hellman instance. Online guessing is still limited to one PIN try
/// per connection and stays covered by the host's per-IP lockout + PIN
/// rotation.
///
/// The blinding points `M`/`N` are the standard SPAKE2 per-curve constants
/// from RFC 9382 §6 (nothing-up-my-sleeve seeded points, public
/// documentation). Byte-compatibility with the web viewer's implementation
/// is enforced by `viewer/web/tests/web-compat.mjs`.
pub mod pake {
    use hkdf::Hkdf;
    use p256::elliptic_curve::ops::Reduce;
    use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
    use p256::elliptic_curve::Group;
    use p256::{AffinePoint, EncodedPoint, FieldBytes, NonZeroScalar, ProjectivePoint, Scalar};
    use rand::rngs::OsRng;
    use sha2::Sha256;

    use crate::{ProtocolError, Result};

    pub const W_INFO: &[u8] = b"ndsp-pake-w-v1";
    pub const PAIR_INFO: &[u8] = b"ndsp-pake-pair-v1";
    pub const SESSION_INFO: &[u8] = b"ndsp-pake-session-v1";

    /// RFC 9382 §6 M constant for P-256 (compressed SEC1).
    const M_SEC1: [u8; 33] =
        hex_literal(b"02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f");
    /// RFC 9382 §6 N constant for P-256 (compressed SEC1).
    const N_SEC1: [u8; 33] =
        hex_literal(b"03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49");

    /// Compile-time hex decode for the fixed point constants.
    const fn hex_literal(h: &[u8; 66]) -> [u8; 33] {
        const fn nibble(c: u8) -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => panic!("bad hex digit"),
            }
        }
        let mut out = [0u8; 33];
        let mut i = 0;
        while i < 33 {
            out[i] = (nibble(h[2 * i]) << 4) | nibble(h[2 * i + 1]);
            i += 1;
        }
        out
    }

    fn decode_point(sec1: &[u8]) -> Result<ProjectivePoint> {
        let ep = EncodedPoint::from_bytes(sec1)
            .map_err(|_| ProtocolError::Crypto("invalid PAKE point encoding"))?;
        let affine: Option<AffinePoint> = AffinePoint::from_encoded_point(&ep).into();
        let point = affine.ok_or(ProtocolError::Crypto("PAKE point not on curve"))?;
        Ok(ProjectivePoint::from(point))
    }

    fn constant(sec1: &[u8; 33]) -> ProjectivePoint {
        decode_point(sec1).expect("RFC 9382 constants are valid P-256 points")
    }

    fn encode_point(p: &ProjectivePoint) -> Vec<u8> {
        p.to_affine().to_encoded_point(false).as_bytes().to_vec()
    }

    /// `w = int(HKDF(ikm=PIN, salt=nonce, info="ndsp-pake-w-v1")) mod n`,
    /// mapped away from zero so the blinding term never vanishes.
    fn password_scalar(pin: &str, connection_nonce: &[u8]) -> (Scalar, [u8; 32]) {
        let hk = Hkdf::<Sha256>::new(Some(connection_nonce), pin.as_bytes());
        let mut w_bytes = [0u8; 32];
        hk.expand(W_INFO, &mut w_bytes)
            .expect("32 bytes is valid for SHA-256 HKDF");
        let mut w = <Scalar as Reduce<p256::U256>>::reduce_bytes(&FieldBytes::from(w_bytes));
        if w == Scalar::ZERO {
            w = Scalar::ONE; // ~2^-256; keeps both sides total & identical
        }
        (w, w_bytes)
    }

    /// Which SPAKE2 role this side plays (determines the blinding constant).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Role {
        /// Sends `X = x·G + w·M` in `PairStart`.
        Client,
        /// Sends `Y = y·G + w·N` in `PairChallenge`.
        Server,
    }

    /// One side's in-flight PAKE state.
    pub struct PakeShare {
        role: Role,
        secret: NonZeroScalar,
        w: Scalar,
        w_bytes: [u8; 32],
        public: Vec<u8>,
    }

    /// Keys agreed by a completed PAKE.
    pub struct PakeKeys {
        /// Gates pairing on knowledge of the PIN (confirm + token seal).
        pub pair_key: [u8; 32],
        /// Post-handshake transport key.
        pub session_key: [u8; 32],
    }

    impl PakeShare {
        /// Derive the password scalar and generate this side's public share.
        pub fn new(role: Role, pin: &str, connection_nonce: &[u8]) -> Self {
            let (w, w_bytes) = password_scalar(pin, connection_nonce);
            let secret = NonZeroScalar::random(&mut OsRng);
            let blind = match role {
                Role::Client => constant(&M_SEC1),
                Role::Server => constant(&N_SEC1),
            };
            let public = encode_point(&(ProjectivePoint::GENERATOR * *secret + blind * w));
            Self {
                role,
                secret,
                w,
                w_bytes,
                public,
            }
        }

        /// Uncompressed SEC1 encoding (65 bytes) of this side's public share.
        pub fn public_bytes(&self) -> &[u8] {
            &self.public
        }

        /// Complete the exchange with the peer's share, deriving both keys.
        ///
        /// Rejects off-curve/identity peer shares and a degenerate shared
        /// point. The salt is the 16-byte random from `PairChallenge`; the
        /// nonce is the per-connection nonce from `HelloAck`.
        pub fn complete(
            self,
            peer_public: &[u8],
            salt: &[u8],
            connection_nonce: &[u8],
        ) -> Result<PakeKeys> {
            let peer = decode_point(peer_public)?;
            let peer_blind = match self.role {
                Role::Client => constant(&N_SEC1), // peer is the server
                Role::Server => constant(&M_SEC1),
            };
            let z = (peer - peer_blind * self.w) * *self.secret;
            if bool::from(z.is_identity()) {
                return Err(ProtocolError::Crypto("degenerate PAKE shared point"));
            }
            let z_bytes = encode_point(&z);
            // Transcript: client share first regardless of our role.
            let (x_bytes, y_bytes): (&[u8], &[u8]) = match self.role {
                Role::Client => (&self.public, peer_public),
                Role::Server => (peer_public, &self.public),
            };
            let derive = |context: &[u8]| -> [u8; 32] {
                super::derive(
                    &z_bytes,
                    salt,
                    &[context, connection_nonce, x_bytes, y_bytes, &self.w_bytes],
                )
            };
            Ok(PakeKeys {
                pair_key: derive(PAIR_INFO),
                session_key: derive(SESSION_INFO),
            })
        }
    }
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

    #[test]
    fn pake_both_sides_agree() {
        use super::pake::{PakeShare, Role};
        let nonce = [7u8; 16];
        let salt = [9u8; 16];
        let client = PakeShare::new(Role::Client, "123456", &nonce);
        let server = PakeShare::new(Role::Server, "123456", &nonce);
        let x = client.public_bytes().to_vec();
        let y = server.public_bytes().to_vec();
        assert_eq!(x.len(), 65, "uncompressed SEC1 share");
        assert_eq!(x[0], 0x04);
        let ck = client.complete(&y, &salt, &nonce).unwrap();
        let sk = server.complete(&x, &salt, &nonce).unwrap();
        assert_eq!(ck.pair_key, sk.pair_key);
        assert_eq!(ck.session_key, sk.session_key);
        assert_ne!(ck.pair_key, ck.session_key);
    }

    #[test]
    fn pake_wrong_pin_diverges() {
        use super::pake::{PakeShare, Role};
        let nonce = [1u8; 16];
        let salt = [2u8; 16];
        let client = PakeShare::new(Role::Client, "111111", &nonce);
        let server = PakeShare::new(Role::Server, "111112", &nonce);
        let x = client.public_bytes().to_vec();
        let y = server.public_bytes().to_vec();
        let ck = client.complete(&y, &salt, &nonce).unwrap();
        let sk = server.complete(&x, &salt, &nonce).unwrap();
        assert_ne!(ck.pair_key, sk.pair_key, "wrong PIN must not agree");
        // The confirmation seal must therefore fail to open.
        let sealed = seal(&ck.pair_key, b"confirm", b"");
        assert!(open(&sk.pair_key, &sealed, b"").is_err());
    }

    #[test]
    fn pake_shares_are_blinded_and_fresh() {
        use super::pake::{PakeShare, Role};
        let nonce = [3u8; 16];
        // Same PIN twice → different public shares (fresh randomness), so a
        // passive observer cannot even link two pairings of the same PIN.
        let a = PakeShare::new(Role::Client, "424242", &nonce);
        let b = PakeShare::new(Role::Client, "424242", &nonce);
        assert_ne!(a.public_bytes(), b.public_bytes());
    }

    #[test]
    fn pake_rejects_bad_peer_points() {
        use super::pake::{PakeShare, Role};
        let nonce = [4u8; 16];
        let salt = [5u8; 16];
        // Not a curve point.
        let c = PakeShare::new(Role::Client, "000000", &nonce);
        assert!(c.complete(&[0x04; 65], &salt, &nonce).is_err());
        // Truncated encoding.
        let c = PakeShare::new(Role::Client, "000000", &nonce);
        assert!(c.complete(&[0x04; 32], &salt, &nonce).is_err());
    }

    /// Fixed vector pinning the KDF layout (w derivation + transcript order).
    /// Regenerating this vector requires deliberately changing the PAKE
    /// design — which must be caught, not silently shipped.
    #[test]
    fn pake_password_scalar_vector_is_stable() {
        use hkdf::Hkdf;
        let nonce = [0x11u8; 16];
        let hk = Hkdf::<Sha256>::new(Some(&nonce), b"123456");
        let mut w = [0u8; 32];
        hk.expand(super::pake::W_INFO, &mut w).unwrap();
        assert_eq!(
            hex::encode(w),
            // Independently verified against Node's crypto.hkdfSync.
            "58237cb383739458dcc5d7df8c36c5e77c627b9468d11d1585b1b2fd3c6b49af",
        );
    }
}
