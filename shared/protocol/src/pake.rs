//! PAKE pairing for NDSP: SPAKE2 over NIST P-256.
//!
//! ## Why
//!
//! The PIN-bound HKDF pairing in [`crate::crypto`] has one honest weakness
//! (documented in `docs/SECURITY.md`): a *passive* attacker who records a
//! pairing exchange can brute-force the short PIN **offline** against
//! `pair_confirm`, because the confirmation is a deterministic function of
//! `HKDF(ECDH, …, PIN)` and the recorded transcript contains everything else.
//!
//! SPAKE2 removes that. The password (PIN) is folded into elliptic-curve
//! points; the resulting shared secret `K` can only be computed with knowledge
//! of one side's *ephemeral secret scalar*, which never appears on the wire.
//! An observer sees `pA = x·G + w·M` and `pB = y·G + w·N` but cannot verify a
//! password guess without solving a discrete log — so the only attack left is
//! *online* guessing, which the per-IP lockout already rate-limits.
//!
//! ## Flow (roles are fixed: client = A, server = B)
//!
//! ```text
//! w  = SPAKE2 password scalar = OS2IP(SHA-256("ndsp-pake-w-v1"‖nonce‖PIN)) mod n
//! A: x ←$ [1,n)   pA = x·G + w·M          --pA-->
//! B: y ←$ [1,n)   pB = y·G + w·N          <--pB--
//!    K = y·(pA − w·M) = x·y·G = A's K
//!    TT = transcript(idA,idB,M,N,pA,pB,K,w)
//!    Ka = SHA-256(TT);  KcA‖KcB = HKDF(Ka);  cX = HMAC(KcX, TT)
//! A: sends cA                      --cA-->
//! B: verifies cA (wrong PIN → count failure, rotate PIN, reject)
//!                                  <--cB--
//! A: verifies cB before trusting the session key
//! session_key = HKDF(ikm = Ka, salt = nonce, info = "ndsp-session-v1")
//! ```
//!
//! **Confirmation order matters for rate limiting.** The client confirms
//! *first*: a malicious client that wants to test a PIN guess must send `cA`,
//! which the server verifies and counts as a failed attempt (rotating the
//! PIN). A client that abandons the handshake after seeing `pB` learns
//! nothing — `pB` is indistinguishable from random without solving a discrete
//! log. Conversely a fake host gets exactly one PIN guess per run (committed
//! in `pB` before it sees `cA`) — the strongest guarantee any PAKE can give.
//!
//! The transcript binds both public shares, the group, the derived point `K`,
//! and the password scalar, so any tampering (including key substitution by an
//! active MITM) breaks confirmation before a session key is trusted.
//!
//! ## Group elements M and N
//!
//! SPAKE2 needs two public points whose discrete logs relative to the
//! generator are unknown to everyone ("nothing up my sleeve"). Rather than
//! ship hand-copied hex constants, both peers derive them deterministically by
//! hashing a fixed public seed string onto the curve (try-and-increment). The
//! points are therefore verifiable from source and impossible to mistype, and
//! the byte-identical construction in `viewer/web/src/pake.ts` guarantees the
//! Rust host and the browser agree. See `docs/PROTOCOL.md`.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::{
    ops::Reduce,
    sec1::{FromEncodedPoint, ToEncodedPoint},
    Field, Group,
};
use p256::{EncodedPoint, ProjectivePoint, Scalar, U256};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::{ProtocolError, Result};

/// Public seed strings hashed onto the curve to obtain the SPAKE2 elements.
const M_SEED: &[u8] = b"NebulaDisplay SPAKE2 point M v1";
const N_SEED: &[u8] = b"NebulaDisplay SPAKE2 point N v1";
/// Domain-separation tag for the hash-to-curve construction.
const H2C_TAG: &[u8] = b"ndsp-h2c-p256-v1";

const ID_CLIENT: &[u8] = b"ndsp-pake-client";
const ID_SERVER: &[u8] = b"ndsp-pake-server";
const W_INFO: &[u8] = b"ndsp-pake-w-v1";
const CONFIRM_A_INFO: &[u8] = b"ndsp-pake-confirm-a";
const CONFIRM_B_INFO: &[u8] = b"ndsp-pake-confirm-b";
/// Reused so the transport layer (envelopes) is identical to every other
/// session — the only thing that changed is how the key was agreed.
const SESSION_INFO: &[u8] = crate::crypto::SESSION_INFO;

type HmacSha256 = Hmac<Sha256>;

/// Deterministic try-and-increment hash-to-curve: `x = SHA-256(tag‖seed‖ctr)`,
/// take the even-`y` point when `x` is a valid compressed coordinate, else
/// increment `ctr`. Byte-identical to the TypeScript viewer's construction.
fn hash_to_curve(seed: &[u8]) -> ProjectivePoint {
    for ctr in 0u32..=u32::MAX {
        let mut h = Sha256::new();
        h.update(H2C_TAG);
        h.update((seed.len() as u64).to_le_bytes());
        h.update(seed);
        h.update(ctr.to_be_bytes());
        let x = h.finalize();
        let mut enc = [0u8; 33];
        enc[0] = 0x02; // request the even-y root
        enc[1..].copy_from_slice(&x);
        if let Ok(ep) = EncodedPoint::from_bytes(enc) {
            if let Some(p) =
                Option::<ProjectivePoint>::from(ProjectivePoint::from_encoded_point(&ep))
            {
                return p;
            }
        }
    }
    unreachable!("a valid x-coordinate exists within 2^32 counter values")
}

fn m_point() -> ProjectivePoint {
    static M: OnceLock<ProjectivePoint> = OnceLock::new();
    *M.get_or_init(|| hash_to_curve(M_SEED))
}
fn n_point() -> ProjectivePoint {
    static N: OnceLock<ProjectivePoint> = OnceLock::new();
    *N.get_or_init(|| hash_to_curve(N_SEED))
}

/// Uncompressed SEC1 (65 bytes, 0x04-prefixed) — the only raw point encoding
/// every browser WebCrypto/`@noble` backend agrees on, matching the ECDH path.
fn point_bytes(p: &ProjectivePoint) -> Vec<u8> {
    p.to_affine().to_encoded_point(false).as_bytes().to_vec()
}

fn parse_point(bytes: &[u8]) -> Result<ProjectivePoint> {
    let ep = EncodedPoint::from_bytes(bytes)
        .map_err(|_| ProtocolError::Crypto("invalid PAKE share encoding"))?;
    let pt = Option::<ProjectivePoint>::from(ProjectivePoint::from_encoded_point(&ep))
        .ok_or(ProtocolError::Crypto("PAKE share not on curve"))?;
    if bool::from(pt.is_identity()) {
        return Err(ProtocolError::Crypto("PAKE share is the identity point"));
    }
    Ok(pt)
}

/// Map the PIN to the SPAKE2 password scalar `w`, bound to the per-connection
/// nonce so recorded shares are useless against a different handshake.
fn password_scalar(pin: &str, nonce: &[u8]) -> Scalar {
    let mut h = Sha256::new();
    h.update(W_INFO);
    h.update((nonce.len() as u64).to_le_bytes());
    h.update(nonce);
    h.update(pin.as_bytes());
    let digest = h.finalize();
    // Reduce the 256-bit hash modulo the curve order. The bias for a 256-bit
    // value mod n (n ≈ 2^256 − 2^128) is ~2^-128 — cryptographically irrelevant.
    <Scalar as Reduce<U256>>::reduce_bytes(&digest)
}

/// 8-byte little-endian length prefix, matching RFC 9382's transcript encoding.
fn push_lv(buf: &mut Vec<u8>, part: &[u8]) {
    buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
    buf.extend_from_slice(part);
}

/// The confirmation/derivation transcript. Identical on both peers regardless
/// of role: `pA` is always the client's share, `pB` always the server's.
fn transcript(
    pa: &ProjectivePoint,
    pb: &ProjectivePoint,
    k: &ProjectivePoint,
    w: &Scalar,
) -> Vec<u8> {
    let mut tt = Vec::with_capacity(512);
    push_lv(&mut tt, ID_CLIENT);
    push_lv(&mut tt, ID_SERVER);
    push_lv(&mut tt, &point_bytes(&m_point()));
    push_lv(&mut tt, &point_bytes(&n_point()));
    push_lv(&mut tt, &point_bytes(pa));
    push_lv(&mut tt, &point_bytes(pb));
    push_lv(&mut tt, &point_bytes(k));
    let w_bytes = w.to_bytes();
    push_lv(&mut tt, w_bytes.as_ref());
    tt
}

fn hkdf32(ka: &[u8], nonce: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(nonce), ka);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm).expect("32 bytes valid for HKDF");
    okm
}

fn mac(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Keys + confirmation tags derived once both shares are known.
struct Derived {
    session_key: [u8; 32],
    /// HMAC the client sends to the server.
    confirm_a: [u8; 32],
    /// HMAC the server sends to the client.
    confirm_b: [u8; 32],
}

fn derive(
    pa: &ProjectivePoint,
    pb: &ProjectivePoint,
    k: &ProjectivePoint,
    w: &Scalar,
    nonce: &[u8],
) -> Derived {
    let tt = transcript(pa, pb, k, w);
    let ka: [u8; 32] = Sha256::digest(&tt).into();
    let kc_a = hkdf32(&ka, nonce, CONFIRM_A_INFO);
    let kc_b = hkdf32(&ka, nonce, CONFIRM_B_INFO);
    Derived {
        session_key: hkdf32(&ka, nonce, SESSION_INFO),
        confirm_a: mac(&kc_a, &tt),
        confirm_b: mac(&kc_b, &tt),
    }
}

/// Client (role A) state between sending `pA` and receiving `pB`.
pub struct PakeClient {
    x: Scalar,
    w: Scalar,
    pa: ProjectivePoint,
    nonce: Vec<u8>,
}

/// Client state after processing the server share: holds the confirmation to
/// send and what to expect back. The session key is only released once the
/// server's confirmation verifies (see [`PakeClientPending::verify_server`]).
pub struct PakeClientPending {
    session_key: [u8; 32],
    /// HMAC the client sends to the server (send this immediately).
    pub confirm_a: Vec<u8>,
    confirm_b_expected: [u8; 32],
}

impl PakeClient {
    /// Begin pairing: derive `w` from the PIN and produce the public share
    /// `pA = x·G + w·M`.
    pub fn start(pin: &str, nonce: &[u8]) -> Self {
        let w = password_scalar(pin, nonce);
        let x = Scalar::random(&mut OsRng);
        let pa = ProjectivePoint::GENERATOR * x + m_point() * w;
        Self {
            x,
            w,
            pa,
            nonce: nonce.to_vec(),
        }
    }

    /// The public share to send to the server (uncompressed SEC1, 65 bytes).
    pub fn share_bytes(&self) -> Vec<u8> {
        point_bytes(&self.pa)
    }

    /// Consume the server's share, deriving keys and both confirmation tags.
    pub fn finish(self, server_share: &[u8]) -> Result<PakeClientPending> {
        let pb = parse_point(server_share)?;
        // K = x·(pB − w·N)
        let k = (pb - n_point() * self.w) * self.x;
        let d = derive(&self.pa, &pb, &k, &self.w, &self.nonce);
        Ok(PakeClientPending {
            session_key: d.session_key,
            confirm_a: d.confirm_a.to_vec(),
            confirm_b_expected: d.confirm_b,
        })
    }
}

impl PakeClientPending {
    /// Verify the server's confirmation tag; only then is the session key
    /// released. A mismatch means the host did not know the PIN (or the
    /// exchange was tampered with).
    pub fn verify_server(self, server_confirm: &[u8]) -> Result<[u8; 32]> {
        if !ct_eq(&self.confirm_b_expected, server_confirm) {
            return Err(ProtocolError::Crypto(
                "PAKE server confirmation failed (wrong PIN or tampering)",
            ));
        }
        Ok(self.session_key)
    }
}

/// Server (role B) state between receiving `pA` and receiving `cA`.
pub struct PakeServer {
    session_key: [u8; 32],
    confirm_a_expected: [u8; 32],
    confirm_b: [u8; 32],
}

/// Keys released to the server once the client's confirmation verified.
pub struct PakeServerKeys {
    pub session_key: [u8; 32],
    /// HMAC the server sends back to the client (in the success result).
    pub confirm_b: Vec<u8>,
}

impl PakeServer {
    /// Respond to the client's share: compute `pB` and derive the keys.
    /// Returns the state plus the raw `pB` bytes to send. Fails only if the
    /// client share is malformed / off-curve.
    pub fn respond(pin: &str, nonce: &[u8], client_share: &[u8]) -> Result<(Self, Vec<u8>)> {
        let pa = parse_point(client_share)?;
        let w = password_scalar(pin, nonce);
        let y = Scalar::random(&mut OsRng);
        let pb = ProjectivePoint::GENERATOR * y + n_point() * w;
        // K = y·(pA − w·M)
        let k = (pa - m_point() * w) * y;
        let d = derive(&pa, &pb, &k, &w, nonce);
        Ok((
            Self {
                session_key: d.session_key,
                confirm_a_expected: d.confirm_a,
                confirm_b: d.confirm_b,
            },
            point_bytes(&pb),
        ))
    }

    /// Verify the client's confirmation tag and, on success, yield the agreed
    /// session key + the server confirmation to send back. A mismatch means
    /// the client did not know the PIN — callers must count it as a failed
    /// pairing attempt (rate limiting / PIN rotation).
    pub fn verify(self, client_confirm: &[u8]) -> Result<PakeServerKeys> {
        if !ct_eq(&self.confirm_a_expected, client_confirm) {
            return Err(ProtocolError::Crypto(
                "PAKE client confirmation failed (wrong PIN)",
            ));
        }
        Ok(PakeServerKeys {
            session_key: self.session_key,
            confirm_b: self.confirm_b.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pin_client: &str, pin_server: &str, nonce: &[u8]) -> Result<([u8; 32], [u8; 32])> {
        let client = PakeClient::start(pin_client, nonce);
        let (server, pb) = PakeServer::respond(pin_server, nonce, &client.share_bytes())?;
        let pending = client.finish(&pb)?;
        let confirm_a = pending.confirm_a.clone();
        let server_keys = server.verify(&confirm_a)?;
        let client_key = pending.verify_server(&server_keys.confirm_b)?;
        Ok((client_key, server_keys.session_key))
    }

    #[test]
    fn matching_pin_agrees_on_session_key() {
        let nonce = [7u8; 16];
        let (ck, sk) = run("123456", "123456", &nonce).unwrap();
        assert_eq!(ck, sk, "both sides must derive the same session key");
    }

    #[test]
    fn wrong_pin_fails_server_side_confirmation() {
        let nonce = [3u8; 16];
        // The server (which verifies first) must detect the wrong PIN so it
        // can count the attempt and rotate the PIN.
        let client = PakeClient::start("111111", &nonce);
        let (server, pb) = PakeServer::respond("222222", &nonce, &client.share_bytes()).unwrap();
        let pending = client.finish(&pb).unwrap();
        assert!(
            server.verify(&pending.confirm_a).is_err(),
            "server must reject a wrong-PIN confirmation"
        );
    }

    #[test]
    fn client_rejects_fake_server_confirmation() {
        let nonce = [8u8; 16];
        let client = PakeClient::start("123456", &nonce);
        let (_server, pb) = PakeServer::respond("123456", &nonce, &client.share_bytes()).unwrap();
        let pending = client.finish(&pb).unwrap();
        // A host that cannot produce the right cB (didn't know the PIN /
        // MITM) must not be trusted with the session key.
        assert!(pending.verify_server(&[0u8; 32]).is_err());
    }

    #[test]
    fn different_nonce_yields_different_keys() {
        let (k1, _) = run("000000", "000000", &[1u8; 16]).unwrap();
        let (k2, _) = run("000000", "000000", &[2u8; 16]).unwrap();
        assert_ne!(k1, k2, "session key must be bound to the connection nonce");
    }

    #[test]
    fn off_curve_share_is_rejected() {
        let nonce = [4u8; 16];
        let bogus = [0x04u8; 65]; // 0x04 prefix but coordinates not on curve
        assert!(PakeServer::respond("123456", &nonce, &bogus).is_err());
        let client = PakeClient::start("123456", &nonce);
        assert!(client.finish(&bogus).is_err());
    }

    #[test]
    fn identity_share_is_rejected() {
        let nonce = [5u8; 16];
        let identity = [0u8; 1]; // SEC1 encoding of the point at infinity
        assert!(PakeServer::respond("123456", &nonce, &identity).is_err());
    }

    #[test]
    fn seed_points_decode() {
        // Guards the hash-to-curve construction (must match the TS viewer).
        assert!(!bool::from(m_point().is_identity()));
        assert!(!bool::from(n_point().is_identity()));
        assert_ne!(point_bytes(&m_point()), point_bytes(&n_point()));
        // Pin the derived constants: any change here is a breaking protocol
        // change and must be mirrored in viewer/web/src/pake.ts.
        assert_eq!(
            hex::encode(m_point().to_affine().to_encoded_point(true).as_bytes()),
            "02298b62b0cb297207b0cec3d3207dfd416f0b3cbf0df2e85febbb6946e54fd979"
        );
        assert_eq!(
            hex::encode(n_point().to_affine().to_encoded_point(true).as_bytes()),
            "026a3280e5e73b302922aa95e66a75f6dd5d0150dc0b16d2041cfe8688f33988e4"
        );
    }
}
