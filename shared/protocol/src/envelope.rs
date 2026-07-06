//! Post-authentication encrypted envelopes.
//!
//! Every WebSocket **binary** message after `AuthOk` is:
//!
//! ```text
//! [chan u8][counter u64 BE][AES-256-GCM(nonce = dir||chan||0||counter) ciphertext+tag]
//! ```
//!
//! * `chan` — [`Channel`]; also authenticated as AAD.
//! * `counter` — strictly increasing per direction. Receivers reject
//!   non-monotonic counters (replay/reorder protection; WS is ordered, so a
//!   violation means an attack or a broken proxy).
//! * nonce layout (12 bytes): `[dir u8][chan u8][0u8;2][counter u64 BE]` —
//!   unique per (key, direction, channel, counter).

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};

use crate::{ProtocolError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    Control = 1,
    Video = 2,
    Audio = 3,
}

impl TryFrom<u8> for Channel {
    type Error = ProtocolError;
    fn try_from(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Channel::Control),
            2 => Ok(Channel::Video),
            3 => Ok(Channel::Audio),
            _ => Err(ProtocolError::Malformed("unknown channel")),
        }
    }
}

/// Direction of travel; part of the nonce so the same counter value on both
/// sides never reuses a nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    ServerToClient = 0,
    ClientToServer = 1,
}

fn nonce_for(dir: Direction, chan: Channel, counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = dir as u8;
    n[1] = chan as u8;
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// Stateful sealer for one direction of a session.
pub struct Sealer {
    cipher: Aes256Gcm,
    dir: Direction,
    counters: [u64; 4], // indexed by channel discriminant
}

impl Sealer {
    pub fn new(session_key: &[u8; 32], dir: Direction) -> Self {
        Self {
            cipher: Aes256Gcm::new(session_key.into()),
            dir,
            counters: [0; 4],
        }
    }

    pub fn seal(&mut self, chan: Channel, plaintext: &[u8]) -> Vec<u8> {
        self.seal_parts(chan, &[plaintext])
    }

    /// Seal a plaintext given as multiple concatenated parts (e.g. a frame
    /// header + payload) **in place**: one output allocation, one copy of
    /// each part, and in-buffer AES-GCM. Wire format is byte-identical to
    /// [`Sealer::seal`]. This is the video hot path — the naive compose →
    /// encrypt → frame pipeline cost two extra full-frame copies per frame.
    pub fn seal_parts(&mut self, chan: Channel, parts: &[&[u8]]) -> Vec<u8> {
        use aes_gcm::aead::AeadInPlace;
        let idx = chan as usize;
        let counter = self.counters[idx];
        self.counters[idx] = counter.checked_add(1).expect("envelope counter overflow");
        let nonce = nonce_for(self.dir, chan, counter);
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let mut out = Vec::with_capacity(9 + total + 16);
        out.push(chan as u8);
        out.extend_from_slice(&counter.to_be_bytes());
        for p in parts {
            out.extend_from_slice(p);
        }
        let tag = self
            .cipher
            .encrypt_in_place_detached(&Nonce::from(nonce), &[chan as u8], &mut out[9..])
            .expect("AES-GCM encryption is infallible for valid inputs");
        out.extend_from_slice(&tag);
        out
    }
}

/// Stateful opener for the opposite direction.
pub struct Opener {
    cipher: Aes256Gcm,
    dir: Direction,
    next_expected: [u64; 4],
}

impl Opener {
    /// `dir` is the direction the *peer* seals with.
    pub fn new(session_key: &[u8; 32], dir: Direction) -> Self {
        Self {
            cipher: Aes256Gcm::new(session_key.into()),
            dir,
            next_expected: [0; 4],
        }
    }

    pub fn open(&mut self, envelope: &[u8]) -> Result<(Channel, Vec<u8>)> {
        if envelope.len() < 1 + 8 + 16 {
            return Err(ProtocolError::Malformed("envelope too short"));
        }
        let chan = Channel::try_from(envelope[0])?;
        let counter = u64::from_be_bytes(envelope[1..9].try_into().unwrap());
        let idx = chan as usize;
        if counter < self.next_expected[idx] {
            return Err(ProtocolError::Replay);
        }
        let nonce = nonce_for(self.dir, chan, counter);
        let pt = self
            .cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &envelope[9..],
                    aad: &[chan as u8],
                },
            )
            .map_err(|_| ProtocolError::Crypto("envelope AEAD open failed"))?;
        self.next_expected[idx] = counter + 1;
        Ok((chan, pt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_multiple_channels() {
        let key = [42u8; 32];
        let mut s = Sealer::new(&key, Direction::ServerToClient);
        let mut o = Opener::new(&key, Direction::ServerToClient);
        for i in 0..10u8 {
            let env = s.seal(Channel::Video, &[i; 100]);
            let (chan, pt) = o.open(&env).unwrap();
            assert_eq!(chan, Channel::Video);
            assert_eq!(pt, vec![i; 100]);
        }
        let env = s.seal(Channel::Control, b"{}");
        assert_eq!(o.open(&env).unwrap().0, Channel::Control);
    }

    #[test]
    fn seal_parts_matches_seal_wire_format() {
        let key = [7u8; 32];
        let mut s1 = Sealer::new(&key, Direction::ServerToClient);
        let mut s2 = Sealer::new(&key, Direction::ServerToClient);
        let mut o = Opener::new(&key, Direction::ServerToClient);
        let header = [1u8, 2, 3];
        let payload = vec![9u8; 500];
        let joined: Vec<u8> = header.iter().chain(payload.iter()).copied().collect();
        let a = s1.seal(Channel::Video, &joined);
        let b = s2.seal_parts(Channel::Video, &[&header, &payload]);
        assert_eq!(a, b, "in-place seal must be byte-identical");
        assert_eq!(o.open(&b).unwrap().1, joined);
    }

    #[test]
    fn replayed_envelope_rejected() {
        let key = [1u8; 32];
        let mut s = Sealer::new(&key, Direction::ClientToServer);
        let mut o = Opener::new(&key, Direction::ClientToServer);
        let env = s.seal(Channel::Control, b"hello");
        o.open(&env).unwrap();
        assert!(matches!(o.open(&env), Err(ProtocolError::Replay)));
    }

    #[test]
    fn wrong_direction_fails() {
        let key = [1u8; 32];
        let mut s = Sealer::new(&key, Direction::ClientToServer);
        let mut o = Opener::new(&key, Direction::ServerToClient);
        let env = s.seal(Channel::Control, b"hello");
        assert!(o.open(&env).is_err());
    }

    #[test]
    fn channel_swap_detected_via_aad() {
        let key = [1u8; 32];
        let mut s = Sealer::new(&key, Direction::ClientToServer);
        let mut o = Opener::new(&key, Direction::ClientToServer);
        let mut env = s.seal(Channel::Control, b"hello");
        env[0] = Channel::Video as u8; // tamper with channel byte
        assert!(o.open(&env).is_err());
    }
}
