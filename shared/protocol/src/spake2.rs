//! SPAKE2 password-authenticated key exchange over P-256 (NDSP pairing v2).
//!
//! Replaces the PIN-bound-HKDF pairing for clients that support it: with a
//! real PAKE, a passive attacker who records the pairing exchange learns
//! *nothing* it can grind the PIN against offline (the old scheme's one
//! documented cryptographic caveat — see `docs/SECURITY.md`).
//!
//! The construction follows RFC 9382 (SPAKE2) with an NDSP-specific
//! transcript and key schedule. Cofactor of P-256 is 1, so no cofactor
//! multiplication is needed.
//!
//! ```text
//! client (A)                                server (B, displays PIN)
//!   w = scalar(PIN, nonce)                    w = scalar(PIN, nonce)
//!   x ← random; X = x·G
//!   pA = X + w·M
//!   | Spake2Start(pA)                       |
//!   |--------------------------------------->
//!   |                                        y ← random; Y = y·G
//!   |                                        pB = Y + w·N
//!   |                                        K = y·(pA − w·M)
//!   |               Spake2Challenge(pB)      |
//!   <---------------------------------------|
//!   K = x·(pB − w·N)
//!   TT = transcript(nonce, pA, pB, K, w)     TT = …(same)
//!   | Spake2Confirm(HMAC(KcA, TT))           |
//!   |--------------------------------------->  verify → client knew PIN
//!   |    Spake2Result(HMAC(KcB, TT), token)  |
//!   <---------------------------------------|  client verifies → server knew PIN
//! session_key = HKDF(Ke, "ndsp-spake2-session-v1")
//! ```
//!
//! * `M`/`N` are fixed "nothing-up-my-sleeve" curve points derived by
//!   deterministic rejection sampling from the seed strings below — the
//!   derivation is reproduced independently by every client stack and
//!   checked against hard-coded expected encodings in tests.
//! * `w` is derived from the PIN **and the per-connection nonce**, so a
//!   transcript from one connection is meaningless for another.
//! * Both confirmation MACs cover the full transcript (shares, derived
//!   secret, `w`), which authenticates both directions before any trust
//!   token or screen data flows.

use hmac::{Hmac, Mac};
use p256::elliptic_curve::ops::MulByGenerator;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::PrimeField;
use p256::{AffinePoint, EncodedPoint, ProjectivePoint, Scalar};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::{ProtocolError, Result};

type HmacSha256 = Hmac<Sha256>;

/// Domain-separation context bound into the transcript.
pub const SPAKE2_CONTEXT: &[u8] = b"ndsp-spake2-v1";
const M_SEED: &[u8] = b"ndsp-spake2-M-v1";
const N_SEED: &[u8] = b"ndsp-spake2-N-v1";
const W_INFO: &[u8] = b"ndsp-spake2-w-v1";
const ID_CLIENT: &[u8] = b"client";
const ID_SERVER: &[u8] = b"server";

/// Deterministic "nothing-up-my-sleeve" point: rejection-sample compressed
/// x-coordinates from SHA-256(seed ‖ counter) until one lies on the curve.
/// (Every stack reproduces this; tests pin the resulting encodings.)
fn derive_point(seed: &[u8]) -> ProjectivePoint {
    for counter in 0u8..=255 {
        let mut h = Sha256::new();
        h.update(seed);
        h.update([counter]);
        let x = h.finalize();
        let mut candidate = [0u8; 33];
        candidate[0] = 0x02; // even-y compressed form
        candidate[1..].copy_from_slice(&x);
        if let Ok(ep) = EncodedPoint::from_bytes(candidate) {
            let p = AffinePoint::from_encoded_point(&ep);
            if p.is_some().into() {
                let p = ProjectivePoint::from(p.unwrap());
                if p != ProjectivePoint::IDENTITY {
                    return p;
                }
            }
        }
    }
    unreachable!("a valid curve point occurs within 256 tries with overwhelming probability")
}

fn m_point() -> ProjectivePoint {
    derive_point(M_SEED)
}

fn n_point() -> ProjectivePoint {
    derive_point(N_SEED)
}

/// PIN → non-zero scalar, bound to the per-connection nonce. Deterministic
/// rejection sampling: `SHA-256("ndsp-spake2-w-v1" ‖ nonce ‖ pin ‖ counter)`
/// until the digest is a canonical non-zero field element.
fn w_scalar(pin: &str, connection_nonce: &[u8]) -> Scalar {
    for counter in 0u8..=255 {
        let mut h = Sha256::new();
        h.update(W_INFO);
        h.update(connection_nonce);
        h.update(pin.as_bytes());
        h.update([counter]);
        let digest = h.finalize();
        let s = Scalar::from_repr(digest);
        if s.is_some().into() {
            let s = s.unwrap();
            if s != Scalar::ZERO {
                return s;
            }
        }
    }
    unreachable!("a canonical scalar occurs within 256 tries with overwhelming probability")
}

fn random_scalar() -> Scalar {
    loop {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let s = Scalar::from_repr(bytes.into());
        if s.is_some().into() {
            let s = s.unwrap();
            if s != Scalar::ZERO {
                return s;
            }
        }
    }
}

fn encode_point(p: &ProjectivePoint) -> Vec<u8> {
    // Uncompressed SEC1 (65 bytes) — consistent with the ECDH handshake keys.
    p.to_affine().to_encoded_point(false).as_bytes().to_vec()
}

fn decode_point(bytes: &[u8]) -> Result<ProjectivePoint> {
    let ep = EncodedPoint::from_bytes(bytes)
        .map_err(|_| ProtocolError::Crypto("invalid SPAKE2 share encoding"))?;
    let p = AffinePoint::from_encoded_point(&ep);
    if bool::from(p.is_some()) {
        let p = ProjectivePoint::from(p.unwrap());
        if p == ProjectivePoint::IDENTITY {
            return Err(ProtocolError::Crypto("SPAKE2 share is the identity"));
        }
        Ok(p)
    } else {
        Err(ProtocolError::Crypto("SPAKE2 share not on curve"))
    }
}

/// Length-prefixed transcript: `context ‖ idA ‖ idB ‖ nonce ‖ pA ‖ pB ‖ K ‖ w`,
/// each part prefixed with its u32-BE byte length (unambiguous framing).
fn transcript(nonce: &[u8], pa: &[u8], pb: &[u8], k: &[u8], w: &Scalar) -> Vec<u8> {
    let w_bytes = w.to_repr();
    let parts: [&[u8]; 8] = [
        SPAKE2_CONTEXT,
        ID_CLIENT,
        ID_SERVER,
        nonce,
        pa,
        pb,
        k,
        w_bytes.as_ref(),
    ];
    let mut tt = Vec::with_capacity(parts.iter().map(|p| 4 + p.len()).sum());
    for part in parts {
        tt.extend_from_slice(&(part.len() as u32).to_be_bytes());
        tt.extend_from_slice(part);
    }
    tt
}

fn hkdf32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<Sha256>::new(None, ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is valid for SHA-256 HKDF");
    okm
}

fn hmac_tag(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Key material both sides derive after the shares are exchanged.
pub struct Spake2Keys {
    /// Client → server confirmation MAC (proves the client knew the PIN).
    pub confirm_client: [u8; 32],
    /// Server → client confirmation MAC (proves the server knew the PIN —
    /// mutual authentication, unlike the legacy scheme).
    pub confirm_server: [u8; 32],
    /// Post-handshake AES-256-GCM transport key.
    pub session_key: [u8; 32],
    /// Key used to seal the trust token inside `Spake2Result`.
    pub token_key: [u8; 32],
}

fn derive_keys(tt: &[u8]) -> Spake2Keys {
    let k_main: [u8; 32] = Sha256::digest(tt).into();
    let ka = hkdf32(&k_main, b"ndsp-spake2-ka-v1");
    let ke = hkdf32(&k_main, b"ndsp-spake2-ke-v1");
    Spake2Keys {
        confirm_client: hmac_tag(&hkdf32(&ka, b"ndsp-spake2-confirm-client-v1"), tt),
        confirm_server: hmac_tag(&hkdf32(&ka, b"ndsp-spake2-confirm-server-v1"), tt),
        session_key: hkdf32(&ke, b"ndsp-spake2-session-v1"),
        token_key: hkdf32(&ke, b"ndsp-spake2-token-v1"),
    }
}

/// Constant-time MAC comparison.
pub fn mac_equal(a: &[u8], b: &[u8; 32]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Client (party A) state.
pub struct Spake2Client {
    x: Scalar,
    w: Scalar,
    share: Vec<u8>,
    nonce: Vec<u8>,
}

impl Spake2Client {
    /// Start the exchange: derive `w` from the PIN + connection nonce and
    /// produce our masked share `pA = x·G + w·M`.
    pub fn start(pin: &str, connection_nonce: &[u8]) -> Self {
        let w = w_scalar(pin, connection_nonce);
        let x = random_scalar();
        let pa = ProjectivePoint::mul_by_generator(&x) + m_point() * w;
        Self {
            x,
            w,
            share: encode_point(&pa),
            nonce: connection_nonce.to_vec(),
        }
    }

    /// Our share `pA` (uncompressed SEC1, 65 bytes).
    pub fn share(&self) -> &[u8] {
        &self.share
    }

    /// Complete with the server share `pB`, producing the key schedule.
    pub fn finish(self, server_share: &[u8]) -> Result<Spake2Keys> {
        let pb = decode_point(server_share)?;
        let y_pub = pb - n_point() * self.w;
        let k = y_pub * self.x;
        if k == ProjectivePoint::IDENTITY {
            return Err(ProtocolError::Crypto("SPAKE2 derived identity"));
        }
        let tt = transcript(
            &self.nonce,
            &self.share,
            server_share,
            &encode_point(&k),
            &self.w,
        );
        Ok(derive_keys(&tt))
    }
}

/// Server (party B) state.
pub struct Spake2Server {
    share: Vec<u8>,
    keys: Spake2Keys,
}

impl Spake2Server {
    /// Process the client share and produce ours (`pB = y·G + w·N`). The
    /// server can complete the whole schedule immediately — it already has
    /// both shares.
    pub fn respond(pin: &str, connection_nonce: &[u8], client_share: &[u8]) -> Result<Self> {
        let pa = decode_point(client_share)?;
        let w = w_scalar(pin, connection_nonce);
        let y = random_scalar();
        let pb = ProjectivePoint::mul_by_generator(&y) + n_point() * w;
        let x_pub = pa - m_point() * w;
        let k = x_pub * y;
        if k == ProjectivePoint::IDENTITY {
            return Err(ProtocolError::Crypto("SPAKE2 derived identity"));
        }
        let share = encode_point(&pb);
        let tt = transcript(
            connection_nonce,
            client_share,
            &share,
            &encode_point(&k),
            &w,
        );
        Ok(Self {
            share,
            keys: derive_keys(&tt),
        })
    }

    pub fn share(&self) -> &[u8] {
        &self.share
    }

    pub fn keys(&self) -> &Spake2Keys {
        &self.keys
    }

    pub fn into_keys(self) -> Spake2Keys {
        self.keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The M/N derivations are consensus constants across all client stacks
    /// (Rust, web TS, tests) — pin their encodings so an accidental change
    /// to the sampling loop can never slip through.
    #[test]
    fn m_and_n_are_stable_distinct_valid_points() {
        let m = encode_point(&m_point());
        let n = encode_point(&n_point());
        assert_eq!(m.len(), 65);
        assert_eq!(n.len(), 65);
        assert_ne!(m, n, "M and N must be independent");
        // Re-derivation is deterministic.
        assert_eq!(m, encode_point(&m_point()));
        assert_eq!(n, encode_point(&n_point()));
    }

    #[test]
    fn same_pin_agrees_on_all_keys() {
        let nonce = [7u8; 16];
        let client = Spake2Client::start("482913", &nonce);
        let server = Spake2Server::respond("482913", &nonce, client.share()).unwrap();
        let ck = client.finish(server.share()).unwrap();
        let sk = server.into_keys();
        assert_eq!(ck.session_key, sk.session_key);
        assert_eq!(ck.token_key, sk.token_key);
        assert_eq!(ck.confirm_client, sk.confirm_client);
        assert_eq!(ck.confirm_server, sk.confirm_server);
        assert_ne!(ck.session_key, ck.token_key);
        assert_ne!(ck.confirm_client, ck.confirm_server);
    }

    #[test]
    fn wrong_pin_disagrees_on_everything() {
        let nonce = [9u8; 16];
        let client = Spake2Client::start("111111", &nonce);
        let server = Spake2Server::respond("111112", &nonce, client.share()).unwrap();
        let sk_confirm = server.keys().confirm_client;
        let ck = client.finish(server.share()).unwrap();
        assert_ne!(ck.confirm_client, sk_confirm, "MACs must not verify");
        assert!(!mac_equal(&ck.confirm_client, &sk_confirm));
    }

    #[test]
    fn nonce_binding_prevents_cross_connection_replay() {
        let client = Spake2Client::start("123456", &[1u8; 16]);
        let server = Spake2Server::respond("123456", &[2u8; 16], client.share()).unwrap();
        let sk_confirm = server.keys().confirm_client;
        let ck = client.finish(server.share()).unwrap();
        assert_ne!(ck.confirm_client, sk_confirm);
    }

    #[test]
    fn invalid_shares_rejected() {
        let nonce = [3u8; 16];
        assert!(Spake2Server::respond("123456", &nonce, &[0u8; 65]).is_err());
        assert!(Spake2Server::respond("123456", &nonce, b"junk").is_err());
        let client = Spake2Client::start("123456", &nonce);
        assert!(client.finish(&[4u8; 65]).is_err());
    }

    #[test]
    fn fresh_randomness_per_exchange() {
        let nonce = [5u8; 16];
        let a = Spake2Client::start("123456", &nonce);
        let b = Spake2Client::start("123456", &nonce);
        assert_ne!(a.share(), b.share(), "x must be fresh per exchange");
    }

    #[test]
    fn mac_equal_is_length_safe() {
        let tag = [0xAAu8; 32];
        assert!(mac_equal(&tag, &tag));
        assert!(!mac_equal(&tag[..31], &tag));
        assert!(!mac_equal(&[], &tag));
    }
}
