//! Balanced PAKE for PIN pairing (CPace-style, ristretto255).
//!
//! Replaces the v1 "PIN mixed into HKDF info" construction, whose recorded
//! transcript allowed *offline* brute force of the short PIN. With the PAKE,
//! knowledge of the PIN is required to compute the shared group element at
//! all, so:
//!
//! * a **passive** attacker recording the exchange learns nothing it can
//!   grind offline — testing a candidate PIN requires solving CDH in the
//!   ristretto255 group;
//! * an **active** attacker gets exactly one online PIN guess per
//!   connection, and the host's rate limiter locks it out.
//!
//! Construction (CPace pattern, per-connection session id):
//!
//! ```text
//! G   = map_to_group(SHA-512("ndsp-pake-v1" ‖ len(pin) u8 ‖ pin ‖ nonce))
//! a, b ← random nonzero scalars
//! A = a·G     (client share)         B = b·G     (server share)
//! K = a·B = b·A = ab·G               (32-byte canonical encoding)
//! ```
//!
//! `map_to_group` is the ristretto255 element-derivation map of RFC 9496
//! §4.3.4 ([`RistrettoPoint::from_uniform_bytes`]); the web viewer uses the
//! byte-identical `ristretto255_hasher.deriveToCurve` from @noble/curves.
//! `K` is then mixed into the HKDF along with the ephemeral P-256 ECDH
//! secret (see [`crate::crypto::SharedSecret::pairing_key_pake`]), so the
//! session is only as weak as the *stronger* of the two exchanges.
//!
//! Shares are validated on receipt: non-canonical encodings and the identity
//! element are rejected (a zero share would make `K` independent of the PIN).

use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::{Identity, IsIdentity};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha512};

use crate::{ProtocolError, Result};

/// Domain-separation label for the generator derivation.
pub const PAKE_DSI: &[u8] = b"ndsp-pake-v1";
/// Wire size of a PAKE share (canonical ristretto255 encoding).
pub const PAKE_SHARE_LEN: usize = 32;

/// Derive the PIN-bound generator. Both sides must feed the identical PIN
/// string and connection nonce.
fn generator(pin: &str, connection_nonce: &[u8]) -> RistrettoPoint {
    let mut h = Sha512::new();
    h.update(PAKE_DSI);
    // Unambiguous framing: pins are numeric and short, but length-prefix
    // anyway so ("1", nonce="23…") can never collide with ("12", nonce="3…").
    h.update([pin.len() as u8]);
    h.update(pin.as_bytes());
    h.update(connection_nonce);
    let uniform: [u8; 64] = h.finalize().into();
    RistrettoPoint::from_uniform_bytes(&uniform)
}

/// One side's in-progress PAKE. Create with [`PakeState::start`], send
/// [`PakeState::share_bytes`] to the peer, complete with [`PakeState::finish`].
pub struct PakeState {
    scalar: Scalar,
    share: [u8; PAKE_SHARE_LEN],
}

impl PakeState {
    /// Begin an exchange bound to `pin` and this connection's nonce.
    pub fn start(pin: &str, connection_nonce: &[u8]) -> Self {
        let g = generator(pin, connection_nonce);
        let scalar = loop {
            let mut wide = [0u8; 64];
            OsRng.fill_bytes(&mut wide);
            let s = Scalar::from_bytes_mod_order_wide(&wide);
            if s != Scalar::ZERO {
                break s;
            }
        };
        let share = (g * scalar).compress().to_bytes();
        Self { scalar, share }
    }

    /// Our public share (canonical ristretto255 encoding, 32 bytes).
    pub fn share_bytes(&self) -> &[u8; PAKE_SHARE_LEN] {
        &self.share
    }

    /// Combine with the peer's share → 32-byte shared secret. Fails on
    /// malformed / non-canonical / identity peer shares.
    pub fn finish(self, peer_share: &[u8]) -> Result<[u8; 32]> {
        let bytes: [u8; PAKE_SHARE_LEN] = peer_share
            .try_into()
            .map_err(|_| ProtocolError::Crypto("PAKE share must be 32 bytes"))?;
        let point = CompressedRistretto(bytes)
            .decompress()
            .ok_or(ProtocolError::Crypto("invalid PAKE share encoding"))?;
        if point.is_identity() {
            return Err(ProtocolError::Crypto("identity PAKE share rejected"));
        }
        let k = point * self.scalar;
        if k == RistrettoPoint::identity() {
            return Err(ProtocolError::Crypto("degenerate PAKE result"));
        }
        Ok(k.compress().to_bytes())
    }
}

/// Deterministic variant used only by tests / cross-implementation vectors:
/// the scalar is derived from `seed` instead of the OS RNG.
pub fn deterministic_for_tests(pin: &str, connection_nonce: &[u8], seed: &[u8]) -> PakeState {
    let mut h = Sha512::new();
    h.update(b"ndsp-pake-test-scalar");
    h.update(seed);
    let wide: [u8; 64] = h.finalize().into();
    let scalar = Scalar::from_bytes_mod_order_wide(&wide);
    let g = generator(pin, connection_nonce);
    let share = (g * scalar).compress().to_bytes();
    PakeState { scalar, share }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_with_same_pin() {
        let nonce = [7u8; 16];
        let a = PakeState::start("123456", &nonce);
        let b = PakeState::start("123456", &nonce);
        let a_share = *a.share_bytes();
        let b_share = *b.share_bytes();
        let ka = a.finish(&b_share).unwrap();
        let kb = b.finish(&a_share).unwrap();
        assert_eq!(ka, kb);
    }

    #[test]
    fn different_pin_yields_different_secret() {
        let nonce = [9u8; 16];
        let a = PakeState::start("111111", &nonce);
        let b = PakeState::start("111112", &nonce);
        let a_share = *a.share_bytes();
        let b_share = *b.share_bytes();
        assert_ne!(a.finish(&b_share).unwrap(), b.finish(&a_share).unwrap());
    }

    #[test]
    fn different_nonce_yields_different_secret() {
        let a = PakeState::start("123456", &[1u8; 16]);
        let b = PakeState::start("123456", &[2u8; 16]);
        let a_share = *a.share_bytes();
        let b_share = *b.share_bytes();
        assert_ne!(a.finish(&b_share).unwrap(), b.finish(&a_share).unwrap());
    }

    #[test]
    fn identity_share_rejected() {
        let a = PakeState::start("123456", &[3u8; 16]);
        let identity = RistrettoPoint::identity().compress().to_bytes();
        assert!(a.finish(&identity).is_err());
    }

    #[test]
    fn malformed_share_rejected() {
        let a = PakeState::start("123456", &[3u8; 16]);
        assert!(a.finish(&[0xFFu8; 32]).is_err());
        let b = PakeState::start("123456", &[3u8; 16]);
        assert!(b.finish(&[1u8; 7]).is_err());
    }

    /// Cross-implementation vector — the same inputs are asserted against
    /// @noble/curves in `viewer/web/tests/pake-vector.mjs`. If this test's
    /// expected values change, regenerate that file's constants.
    #[test]
    fn cross_implementation_vector() {
        let nonce: [u8; 16] = (1..=16u8).collect::<Vec<_>>().try_into().unwrap();
        let a = deterministic_for_tests("483920", &nonce, b"client-seed");
        let b = deterministic_for_tests("483920", &nonce, b"server-seed");
        assert_eq!(
            hex::encode(a.share_bytes()),
            "22546d580d5a85e7d891e65afb83598c07a2e1648023af95c43391a60870ed12"
        );
        assert_eq!(
            hex::encode(b.share_bytes()),
            "c28a0a09bf1c7cccea8e484b890e511d4fb441e221dd603cb3f7da97d163ee59"
        );
        let b_share = *b.share_bytes();
        let k = a.finish(&b_share).unwrap();
        assert_eq!(
            hex::encode(k),
            "b21f1af9b3f99e94c72d4f70092420686f588a677fdbf675debf50900798fb15"
        );
    }
}
