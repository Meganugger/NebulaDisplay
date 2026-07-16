//! SPAKE2 password-authenticated key exchange for pairing (NDSP v1.1).
//!
//! Replaces the PIN-bound-HKDF pairing key with a real PAKE so a **passive
//! attacker recording a pairing exchange can no longer offline-brute-force
//! the PIN** (the old scheme's one documented cryptographic caveat). An
//! active attacker still gets exactly one online PIN guess per protocol run,
//! which the host already rate-limits and answers with a PIN rotation.
//!
//! Construction: SPAKE2 over P-256 with the RFC 9382 `M`/`N` points.
//!
//! ```text
//! w  = OS2IP(SHA-256("ndsp-pake-w-v1" ‖ lp(PIN) ‖ lp(salt) ‖ lp(nonce))) mod n   (0 → 1)
//! client:  pA = x·G + w·M          server:  pB = y·G + w·N
//! client:  Z  = x·(pB − w·N)       server:  Z  = y·(pA − w·M)        ( = x·y·G )
//! TT = SHA-256("ndsp-pake-v1" ‖ lp(nonce) ‖ lp(salt) ‖ lp(client_ecdh_pub)
//!              ‖ lp(server_ecdh_pub) ‖ lp(pA) ‖ lp(pB) ‖ lp(Z) ‖ lp(w))
//! pair_key = HKDF-SHA256(ikm = TT, salt, info = "ndsp-pair-pake-v1" ‖ nonce)
//! ```
//!
//! `lp(x)` is a u16-BE length prefix. Points travel as uncompressed SEC1
//! (65 bytes) — the same encoding as the ECDH handshake keys. Including both
//! ephemeral ECDH public keys in `TT` binds the PAKE to this exact handshake,
//! so a MITM substituting ECDH keys cannot splice a PAKE transcript across
//! connections.
//!
//! Wire integration (backwards compatible, fields are optional JSON):
//!
//! ```text
//! C→S  pair_start     {client_pubkey, pake: true}
//! S→C  pair_challenge {server_pubkey, salt, pake_share: pB}
//! C→S  pair_confirm   {sealed, pake_share: pA}     # sealed under pair_key
//! S→C  pair_result    {ok, sealed_token}           # token sealed under pair_key
//! ```
//!
//! The client contributes its share *with* the confirmation (it needs the
//! server-chosen salt from `pair_challenge` to derive `w` first). Verifying
//! the sealed confirmation proves the client knew the PIN; opening the sealed
//! token proves the server did — mutual authentication, nothing grindable on
//! the wire.
//!
//! Byte-compatibility with the TypeScript implementation
//! (`viewer/web/src/pake.ts`) is locked by the shared test vector in
//! [`tests::fixed_vector_matches_reference`] / `viewer/web/tests/pake-vectors.mjs`.

use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::Field;
use p256::{AffinePoint, EncodedPoint, NonZeroScalar, ProjectivePoint, Scalar, U256};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::{ProtocolError, Result};

/// Domain-separation label for the password scalar derivation.
pub const PAKE_W_INFO: &[u8] = b"ndsp-pake-w-v1";
/// Domain-separation label for the transcript hash.
pub const PAKE_TT_CONTEXT: &[u8] = b"ndsp-pake-v1";
/// HKDF info label for the final pairing key.
pub const PAKE_KEY_INFO: &[u8] = b"ndsp-pair-pake-v1";

/// RFC 9382 §6 fixed point `M` for P-256 (compressed SEC1). Nothing-up-my-
/// sleeve construction (hash-to-curve of "M SPAKE2 seed OID 1.3.132.0.35...").
const M_COMPRESSED: [u8; 33] = [
    0x02, 0x88, 0x6e, 0x2f, 0x97, 0xac, 0xe4, 0x6e, 0x55, 0xba, 0x9d, 0xd7, 0x24, 0x25, 0x79, 0xf2,
    0x99, 0x3b, 0x64, 0xe1, 0x6e, 0xf3, 0xdc, 0xab, 0x95, 0xaf, 0xd4, 0x97, 0x33, 0x3d, 0x8f, 0xa1,
    0x2f,
];
/// RFC 9382 §6 fixed point `N` for P-256 (compressed SEC1).
const N_COMPRESSED: [u8; 33] = [
    0x03, 0xd8, 0xbb, 0xd6, 0xc6, 0x39, 0xc6, 0x29, 0x37, 0xb0, 0x4d, 0x99, 0x7f, 0x38, 0xc3, 0x77,
    0x07, 0x19, 0xc6, 0x29, 0xd7, 0x01, 0x4d, 0x49, 0xa2, 0x4b, 0x4f, 0x98, 0xba, 0xa1, 0x29, 0x2b,
    0x49,
];

fn fixed_point(compressed: &[u8; 33], cell: &'static OnceLock<ProjectivePoint>) -> ProjectivePoint {
    *cell.get_or_init(|| {
        let ep = EncodedPoint::from_bytes(compressed).expect("RFC 9382 constant is valid SEC1");
        let affine: Option<AffinePoint> = AffinePoint::from_encoded_point(&ep).into();
        ProjectivePoint::from(affine.expect("RFC 9382 constant is on-curve"))
    })
}

fn point_m() -> ProjectivePoint {
    static M: OnceLock<ProjectivePoint> = OnceLock::new();
    fixed_point(&M_COMPRESSED, &M)
}

fn point_n() -> ProjectivePoint {
    static N: OnceLock<ProjectivePoint> = OnceLock::new();
    fixed_point(&N_COMPRESSED, &N)
}

/// Which SPAKE2 role we play. The client blinds with `M`, the server with `N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakeRole {
    Client,
    Server,
}

/// u16-BE length-prefixed hash update. Every variable-length transcript item
/// goes through this so no two transcripts can collide by boundary shifting.
fn lp(h: &mut Sha256, data: &[u8]) {
    let len = u16::try_from(data.len()).expect("transcript item fits u16");
    h.update(len.to_be_bytes());
    h.update(data);
}

/// Password scalar `w`: OS2IP(SHA-256(label ‖ lp(pin) ‖ lp(salt) ‖ lp(nonce))) mod n,
/// mapped to 1 in the (2⁻²⁵⁶-probability) zero case so `w·M` is never the identity.
fn derive_w(pin: &str, salt: &[u8], nonce: &[u8]) -> Scalar {
    let mut h = Sha256::new();
    h.update(PAKE_W_INFO);
    lp(&mut h, pin.as_bytes());
    lp(&mut h, salt);
    lp(&mut h, nonce);
    let digest = h.finalize();
    let w = <Scalar as Reduce<U256>>::reduce_bytes(&digest);
    if bool::from(w.is_zero()) {
        Scalar::ONE
    } else {
        w
    }
}

fn encode_point(p: &ProjectivePoint) -> Vec<u8> {
    p.to_affine().to_encoded_point(false).as_bytes().to_vec()
}

fn decode_point(bytes: &[u8]) -> Result<ProjectivePoint> {
    let ep = EncodedPoint::from_bytes(bytes)
        .map_err(|_| ProtocolError::Crypto("invalid PAKE share encoding"))?;
    let affine: Option<AffinePoint> = AffinePoint::from_encoded_point(&ep).into();
    let affine = affine.ok_or(ProtocolError::Crypto("PAKE share not on curve"))?;
    let p = ProjectivePoint::from(affine);
    if p == ProjectivePoint::IDENTITY {
        return Err(ProtocolError::Crypto("PAKE share is the identity"));
    }
    Ok(p)
}

/// One side's in-flight SPAKE2 state.
pub struct Pake {
    role: PakeRole,
    w: Scalar,
    secret: Scalar,
    share: Vec<u8>,
    salt: Vec<u8>,
    nonce: Vec<u8>,
}

impl Pake {
    /// Begin an exchange: derives `w` from the PIN and produces our share.
    pub fn new(role: PakeRole, pin: &str, salt: &[u8], nonce: &[u8]) -> Self {
        let secret = *NonZeroScalar::random(&mut OsRng).as_ref();
        Self::with_secret(role, pin, salt, nonce, secret)
    }

    /// Deterministic constructor used by tests and the cross-stack vectors.
    pub fn with_secret(
        role: PakeRole,
        pin: &str,
        salt: &[u8],
        nonce: &[u8],
        secret: Scalar,
    ) -> Self {
        let w = derive_w(pin, salt, nonce);
        let blind = match role {
            PakeRole::Client => point_m(),
            PakeRole::Server => point_n(),
        };
        let share_point = ProjectivePoint::GENERATOR * secret + blind * w;
        Self {
            role,
            w,
            secret,
            share: encode_point(&share_point),
            salt: salt.to_vec(),
            nonce: nonce.to_vec(),
        }
    }

    /// Our share (`pA` for the client role, `pB` for the server role),
    /// uncompressed SEC1.
    pub fn share(&self) -> &[u8] {
        &self.share
    }

    /// Complete the exchange with the peer's share and the two ephemeral ECDH
    /// public keys of the surrounding handshake, yielding the pairing key.
    ///
    /// Fails on malformed / off-curve / identity peer shares and on a
    /// degenerate shared point (which only a hostile peer can produce).
    pub fn finish(
        self,
        peer_share: &[u8],
        client_ecdh_pub: &[u8],
        server_ecdh_pub: &[u8],
    ) -> Result<[u8; 32]> {
        let peer = decode_point(peer_share)?;
        // Unblind the peer share with the *peer's* fixed point.
        let peer_blind = match self.role {
            PakeRole::Client => point_n(),
            PakeRole::Server => point_m(),
        };
        let z = (peer - peer_blind * self.w) * self.secret;
        if z == ProjectivePoint::IDENTITY {
            return Err(ProtocolError::Crypto("degenerate PAKE shared point"));
        }
        let (p_a, p_b) = match self.role {
            PakeRole::Client => (self.share.as_slice(), peer_share),
            PakeRole::Server => (peer_share, self.share.as_slice()),
        };
        let mut h = Sha256::new();
        h.update(PAKE_TT_CONTEXT);
        lp(&mut h, &self.nonce);
        lp(&mut h, &self.salt);
        lp(&mut h, client_ecdh_pub);
        lp(&mut h, server_ecdh_pub);
        lp(&mut h, p_a);
        lp(&mut h, p_b);
        lp(&mut h, &encode_point(&z));
        lp(&mut h, &self.w.to_bytes());
        let tt = h.finalize();

        Ok(crate::crypto::derive_key(
            &tt,
            &self.salt,
            &[PAKE_KEY_INFO, &self.nonce],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::elliptic_curve::PrimeField;

    fn scalar(n: u64) -> Scalar {
        Scalar::from(n)
    }

    #[test]
    fn both_sides_agree_on_key() {
        let salt = [7u8; 16];
        let nonce = [9u8; 16];
        let cpub = [1u8; 65];
        let spub = [2u8; 65];
        let client = Pake::new(PakeRole::Client, "123456", &salt, &nonce);
        let server = Pake::new(PakeRole::Server, "123456", &salt, &nonce);
        let p_a = client.share().to_vec();
        let p_b = server.share().to_vec();
        let k_c = client.finish(&p_b, &cpub, &spub).unwrap();
        let k_s = server.finish(&p_a, &cpub, &spub).unwrap();
        assert_eq!(k_c, k_s);
    }

    #[test]
    fn wrong_pin_yields_different_keys() {
        let salt = [1u8; 16];
        let nonce = [2u8; 16];
        let cpub = [1u8; 65];
        let spub = [2u8; 65];
        let client = Pake::new(PakeRole::Client, "111111", &salt, &nonce);
        let server = Pake::new(PakeRole::Server, "111112", &salt, &nonce);
        let p_a = client.share().to_vec();
        let p_b = server.share().to_vec();
        let k_c = client.finish(&p_b, &cpub, &spub).unwrap();
        let k_s = server.finish(&p_a, &cpub, &spub).unwrap();
        assert_ne!(k_c, k_s);
    }

    #[test]
    fn transcript_binds_ecdh_keys() {
        let salt = [1u8; 16];
        let nonce = [2u8; 16];
        let client = Pake::new(PakeRole::Client, "111111", &salt, &nonce);
        let server = Pake::new(PakeRole::Server, "111111", &salt, &nonce);
        let p_a = client.share().to_vec();
        let p_b = server.share().to_vec();
        let k_c = client.finish(&p_b, &[1u8; 65], &[2u8; 65]).unwrap();
        // MITM substituted the server's ECDH key as seen by the server.
        let k_s = server.finish(&p_a, &[1u8; 65], &[3u8; 65]).unwrap();
        assert_ne!(k_c, k_s);
    }

    #[test]
    fn invalid_shares_rejected() {
        let salt = [1u8; 16];
        let nonce = [2u8; 16];
        let client = Pake::new(PakeRole::Client, "123456", &salt, &nonce);
        // Not a curve point.
        let mut bogus = vec![0x04u8; 65];
        bogus[1..].fill(0xFF);
        assert!(client.finish(&bogus, &[1u8; 65], &[2u8; 65]).is_err());

        let client = Pake::new(PakeRole::Client, "123456", &salt, &nonce);
        // Identity / malformed encodings.
        assert!(client.finish(&[0u8; 1], &[1u8; 65], &[2u8; 65]).is_err());
    }

    #[test]
    fn shares_are_not_grindable_offline() {
        // pA = x·G + w·M is uniformly distributed regardless of the PIN, so
        // two exchanges with the same PIN must not produce related shares.
        let salt = [3u8; 16];
        let nonce = [4u8; 16];
        let a = Pake::new(PakeRole::Client, "000000", &salt, &nonce);
        let b = Pake::new(PakeRole::Client, "000000", &salt, &nonce);
        assert_ne!(a.share(), b.share());
    }

    /// Fixed vector shared with `viewer/web/tests/pake-vectors.mjs` — locks
    /// byte-compatibility between the Rust and TypeScript implementations.
    /// Regenerate both sides together if the construction ever changes.
    #[test]
    fn fixed_vector_matches_reference() {
        let salt: Vec<u8> = (0u8..16).collect();
        let nonce: Vec<u8> = (16u8..32).collect();
        let x = scalar(0x1111_2222_3333_4444);
        let y = scalar(0x5555_6666_7777_8888);
        let client = Pake::with_secret(PakeRole::Client, "424242", &salt, &nonce, x);
        let server = Pake::with_secret(PakeRole::Server, "424242", &salt, &nonce, y);
        assert_eq!(
            hex::encode(client.share()),
            "046ced788260bc0c17179d3458786ae6470cff0f3306edb09889b95efc763dec92\
             4a7c73a4bc173da1a1bf7ebdfbdb860094070d32305ace2fc4b68bf613c17b29",
            "client share vector drifted",
        );
        assert_eq!(
            hex::encode(server.share()),
            "0437ea8ba904de147c0b3671d2d04abd97814a7926023bcaa0f1ea7228806f64be\
             4da435353f82f19b01f23767199ac68f482904001e38abb88443ef3fd4ad2800",
            "server share vector drifted",
        );
        let p_a = client.share().to_vec();
        let p_b = server.share().to_vec();
        let cpub = [0xAAu8; 65];
        let spub = [0xBBu8; 65];
        let k_c = client.finish(&p_b, &cpub, &spub).unwrap();
        let k_s = server.finish(&p_a, &cpub, &spub).unwrap();
        assert_eq!(k_c, k_s);
        assert_eq!(
            hex::encode(k_c),
            "4a0229ce2ed537da978bc78db844b445d0dc94262a01d7c8b404b74bb1a516c1",
            "pair-key vector drifted",
        );
    }

    #[test]
    fn w_derivation_is_reduced_and_nonzero() {
        let w = derive_w("123456", &[0u8; 16], &[0u8; 16]);
        assert!(!bool::from(w.is_zero()));
        // Round-trips through canonical bytes (i.e. already reduced mod n).
        let bytes = w.to_bytes();
        let back = Scalar::from_repr(bytes).unwrap();
        assert_eq!(w, back);
    }
}
