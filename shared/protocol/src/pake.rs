//! NDSP-PAKE v1 — balanced PAKE pairing (CPace-style construction on P-256).
//!
//! Replaces the PIN-bound-HKDF pairing key derivation with a construction in
//! which **a passive recording of the handshake cannot be ground offline
//! against the (short) PIN**: recovering the shared secret from the exchanged
//! group elements requires solving a Diffie-Hellman problem *per PIN guess*.
//! An active attacker still gets exactly one online guess per connection,
//! which the host rate-limits and answers by rotating the PIN.
//!
//! ## Construction
//!
//! Following the CPace design (hash the low-entropy secret into a fresh
//! group generator, then run Diffie-Hellman on it):
//!
//! ```text
//! g  = hash_to_curve(P256_XMD:SHA-256_SSWU_RO_,
//!                    DST = "NDSP-PAKE-V1-P256_XMD:SHA-256_SSWU_RO_",
//!                    msg = lp(PIN) ‖ lp(nonce) ‖ lp(device_id) ‖ lp(fingerprint))
//! client: a ←$ [1, n-1],  Ya = a·g       (pake_start)
//! server: b ←$ [1, n-1],  Yb = b·g       (pake_challenge, + HKDF salt)
//! K  = a·Yb = b·Ya
//! ISK = SHA-256("ndsp-pake-isk-v1" ‖ lp(nonce) ‖ lp(Ya) ‖ lp(Yb) ‖ lp(x(K)))
//! pair_key    = HKDF-SHA256(ikm=ISK, salt, info="ndsp-pair-v1" ‖ nonce)
//! session_key = HKDF-SHA256(ikm=ISK, salt, info="ndsp-session-v1" ‖ nonce)
//! ```
//!
//! * `lp(x)` is a 2-byte big-endian length prefix followed by the bytes —
//!   every variable-length field is framed, so the transcript is
//!   injective.
//! * `nonce` is the server's per-connection 16-byte nonce (session id /
//!   replay binding), `device_id` the client's claimed id from `Hello`,
//!   `fingerprint` the server identity fingerprint from `HelloAck` —
//!   binding the run to this exact channel and peer pair.
//! * `Ya`/`Yb` are exchanged as uncompressed SEC1 points (65 bytes, the
//!   NDSP wire convention) and hashed into ISK exactly as transmitted.
//! * hash-to-curve is RFC 9380 `P256_XMD:SHA-256_SSWU_RO_` (test vector
//!   pinned below), interoperable across the Rust host (`p256` crate) and
//!   the web viewer (`@noble/curves`).
//! * The explicit key confirmation is the existing `pair_confirm` message:
//!   AES-GCM(pair_key, "ndsp-confirm-v1" ‖ nonce). A wrong PIN yields a
//!   different generator, hence a different K on each side, and the AEAD
//!   open fails — indistinguishable from the legacy failure mode.
//!
//! Wire negotiation: the server advertises `pake: "p256-v1"` in `hello_ack`;
//! clients that understand it send `pake_start` instead of `pair_start`.
//! Legacy clients ignore the unknown field and keep working (the host can
//! refuse legacy pairing via config).

use p256::elliptic_curve::hash2curve::{ExpandMsgXmd, GroupDigest};
use p256::elliptic_curve::point::AffineCoordinates;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::Group;
use p256::{AffinePoint, EncodedPoint, NistP256, NonZeroScalar, ProjectivePoint};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::crypto::{PAIR_INFO, SESSION_INFO};
use crate::{ProtocolError, Result};

/// Suite identifier advertised in `hello_ack.pake`.
pub const PAKE_SUITE: &str = "p256-v1";
/// RFC 9380 domain-separation tag for the generator derivation.
pub const PAKE_DST: &[u8] = b"NDSP-PAKE-V1-P256_XMD:SHA-256_SSWU_RO_";
const ISK_CONTEXT: &[u8] = b"ndsp-pake-isk-v1";

/// Append a 2-byte big-endian length prefix + the bytes (injective framing).
fn lp(out: &mut Vec<u8>, part: &[u8]) {
    debug_assert!(part.len() <= u16::MAX as usize);
    out.extend_from_slice(&(part.len() as u16).to_be_bytes());
    out.extend_from_slice(part);
}

/// The PIN-derived generator: hash_to_curve over the framed transcript.
fn generator(
    pin: &str,
    connection_nonce: &[u8],
    device_id: &str,
    server_fingerprint: &str,
) -> Result<ProjectivePoint> {
    let mut msg = Vec::with_capacity(
        8 + pin.len() + connection_nonce.len() + device_id.len() + server_fingerprint.len(),
    );
    lp(&mut msg, pin.as_bytes());
    lp(&mut msg, connection_nonce);
    lp(&mut msg, device_id.as_bytes());
    lp(&mut msg, server_fingerprint.as_bytes());
    NistP256::hash_from_bytes::<ExpandMsgXmd<Sha256>>(&[&msg], &[PAKE_DST])
        .map_err(|_| ProtocolError::Crypto("hash-to-curve failed"))
}

/// One side's ephemeral PAKE share for a single handshake.
pub struct PakeShare {
    scalar: NonZeroScalar,
    public_sec1: Vec<u8>,
}

impl PakeShare {
    /// Derive the generator from (PIN, channel binding) and produce this
    /// side's share `Y = scalar · g`.
    pub fn generate(
        pin: &str,
        connection_nonce: &[u8],
        device_id: &str,
        server_fingerprint: &str,
    ) -> Result<Self> {
        let g = generator(pin, connection_nonce, device_id, server_fingerprint)?;
        let scalar = NonZeroScalar::random(&mut OsRng);
        let public = g * scalar.as_ref();
        if bool::from(public.is_identity()) {
            // Unreachable for a valid hash-to-curve output and nonzero scalar
            // (prime-order group), kept as defense in depth.
            return Err(ProtocolError::Crypto("PAKE share is identity"));
        }
        let public_sec1 = public
            .to_affine()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        Ok(Self {
            scalar,
            public_sec1,
        })
    }

    /// Uncompressed SEC1 encoding of this side's share (the wire format).
    pub fn public_bytes(&self) -> &[u8] {
        &self.public_sec1
    }

    /// Complete the exchange with the peer's share.
    ///
    /// `client_share`/`server_share` are the exact wire bytes of both shares
    /// (one of which is `self.public_bytes()`), ordered by role so both
    /// sides hash an identical transcript.
    pub fn agree(
        self,
        peer_sec1: &[u8],
        connection_nonce: &[u8],
        client_share: &[u8],
        server_share: &[u8],
    ) -> Result<PakeSecret> {
        // Full SEC1 validation: on-curve, canonical field elements. The
        // uncompressed encoding cannot express the identity.
        let encoded = EncodedPoint::from_bytes(peer_sec1)
            .map_err(|_| ProtocolError::Crypto("invalid PAKE share encoding"))?;
        if encoded.is_identity() {
            return Err(ProtocolError::Crypto("PAKE share is identity"));
        }
        let peer_opt = AffinePoint::from_encoded_point(&encoded);
        let peer = if bool::from(peer_opt.is_some()) {
            peer_opt.unwrap()
        } else {
            return Err(ProtocolError::Crypto("PAKE share not on curve"));
        };
        let k = ProjectivePoint::from(peer) * self.scalar.as_ref();
        if bool::from(k.is_identity()) {
            return Err(ProtocolError::Crypto("PAKE agreement is identity"));
        }
        let kx = k.to_affine().x(); // 32-byte x-coordinate

        let mut transcript = Vec::with_capacity(
            ISK_CONTEXT.len()
                + 8
                + connection_nonce.len()
                + client_share.len()
                + server_share.len()
                + 32,
        );
        transcript.extend_from_slice(ISK_CONTEXT);
        lp(&mut transcript, connection_nonce);
        lp(&mut transcript, client_share);
        lp(&mut transcript, server_share);
        lp(&mut transcript, &kx);
        Ok(PakeSecret(Sha256::digest(&transcript).into()))
    }
}

/// Intermediate session key (ISK) from a completed PAKE run.
pub struct PakeSecret([u8; 32]);

impl PakeSecret {
    /// Key gating pairing (used to seal/verify `pair_confirm` and the trust
    /// token). Unlike the legacy path, the PIN is *not* in the info string —
    /// it is already bound into the generator.
    pub fn pairing_key(&self, salt: &[u8], connection_nonce: &[u8]) -> [u8; 32] {
        crate::crypto::derive_key(&self.0, salt, &[PAIR_INFO, connection_nonce])
    }

    /// Post-handshake transport key.
    pub fn session_key(&self, salt: &[u8], connection_nonce: &[u8]) -> [u8; 32] {
        crate::crypto::derive_key(&self.0, salt, &[SESSION_INFO, connection_nonce])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "123456";
    const NONCE: [u8; 16] = [7u8; 16];
    const DEVICE: &str = "11111111-2222-3333-4444-555555555555";
    const FP: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn run(pin_a: &str, pin_b: &str) -> ([u8; 32], [u8; 32], [u8; 32], [u8; 32]) {
        let client = PakeShare::generate(pin_a, &NONCE, DEVICE, FP).unwrap();
        let server = PakeShare::generate(pin_b, &NONCE, DEVICE, FP).unwrap();
        let ya = client.public_bytes().to_vec();
        let yb = server.public_bytes().to_vec();
        let cs = client.agree(&yb, &NONCE, &ya, &yb).unwrap();
        let ss = server.agree(&ya, &NONCE, &ya, &yb).unwrap();
        let salt = [3u8; 16];
        (
            cs.pairing_key(&salt, &NONCE),
            ss.pairing_key(&salt, &NONCE),
            cs.session_key(&salt, &NONCE),
            ss.session_key(&salt, &NONCE),
        )
    }

    #[test]
    fn same_pin_agrees_on_both_keys() {
        let (pk_c, pk_s, sk_c, sk_s) = run(PIN, PIN);
        assert_eq!(pk_c, pk_s);
        assert_eq!(sk_c, sk_s);
        assert_ne!(pk_c, sk_c, "pairing and session keys must differ");
    }

    #[test]
    fn wrong_pin_disagrees() {
        let (pk_c, pk_s, sk_c, sk_s) = run("123456", "123457");
        assert_ne!(pk_c, pk_s);
        assert_ne!(sk_c, sk_s);
    }

    #[test]
    fn channel_binding_changes_keys() {
        let client = PakeShare::generate(PIN, &NONCE, DEVICE, FP).unwrap();
        // Server that saw a different device id (MITM splicing) disagrees.
        let server = PakeShare::generate(PIN, &NONCE, "other-device", FP).unwrap();
        let ya = client.public_bytes().to_vec();
        let yb = server.public_bytes().to_vec();
        let cs = client.agree(&yb, &NONCE, &ya, &yb).unwrap();
        let ss = server.agree(&ya, &NONCE, &ya, &yb).unwrap();
        let salt = [3u8; 16];
        assert_ne!(cs.pairing_key(&salt, &NONCE), ss.pairing_key(&salt, &NONCE));
    }

    #[test]
    fn shares_are_fresh_per_run() {
        let a = PakeShare::generate(PIN, &NONCE, DEVICE, FP).unwrap();
        let b = PakeShare::generate(PIN, &NONCE, DEVICE, FP).unwrap();
        assert_ne!(a.public_bytes(), b.public_bytes());
    }

    #[test]
    fn invalid_peer_share_rejected() {
        let a = PakeShare::generate(PIN, &NONCE, DEVICE, FP).unwrap();
        let ya = a.public_bytes().to_vec();
        // Not on the curve (valid length, corrupted y).
        let mut bad = ya.clone();
        bad[64] ^= 1;
        assert!(a.agree(&bad, &NONCE, &ya, &bad).is_err());
    }

    /// RFC 9380 test vector for P256_XMD:SHA-256_SSWU_RO_ (§J.1.1, msg="") —
    /// pins the hash-to-curve implementation both stacks must match.
    #[test]
    fn rfc9380_hash_to_curve_vector() {
        let dst: &[u8] = b"QUUX-V01-CS02-with-P256_XMD:SHA-256_SSWU_RO_";
        let p = NistP256::hash_from_bytes::<ExpandMsgXmd<Sha256>>(&[b""], &[dst]).unwrap();
        let enc = p.to_affine().to_encoded_point(false);
        assert_eq!(
            hex::encode(enc.x().unwrap()),
            "2c15230b26dbc6fc9a37051158c95b79656e17a1a920b11394ca91c44247d3e4"
        );
        assert_eq!(
            hex::encode(enc.y().unwrap()),
            "8a7a74985cc5c776cdfe4b1f19884970453912e9d31528c060be9ab5c43e8415"
        );
    }
}
