//! Video frame framing (plaintext *inside* the encrypted video channel).
//!
//! ```text
//! [codec u8][flags u8][seq u32 BE][ts_us u64 BE][w u16 BE][h u16 BE][payload…]
//! ```
//!
//! * `ts_us` — capture timestamp in host clock microseconds (unix epoch).
//!   Combined with Ping/Pong clock sync this yields *measured* end-to-end
//!   latency on the viewer, not a guess.
//! * `flags` bit 0 — keyframe / independently decodable.

use crate::{messages::Codec, ProtocolError, Result};

pub const HEADER_LEN: usize = 1 + 1 + 4 + 8 + 2 + 2;

pub const FLAG_KEYFRAME: u8 = 0b0000_0001;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub codec: Codec,
    pub keyframe: bool,
    pub seq: u32,
    pub timestamp_us: u64,
    pub width: u16,
    pub height: u16,
    pub payload: Vec<u8>,
}

fn codec_to_u8(c: Codec) -> u8 {
    match c {
        Codec::Jpeg => 0,
        Codec::H264 => 1,
        Codec::Hevc => 2,
        Codec::Av1 => 3,
    }
}

fn codec_from_u8(v: u8) -> Result<Codec> {
    match v {
        0 => Ok(Codec::Jpeg),
        1 => Ok(Codec::H264),
        2 => Ok(Codec::Hevc),
        3 => Ok(Codec::Av1),
        _ => Err(ProtocolError::Malformed("unknown codec id")),
    }
}

impl VideoFrame {
    /// Just the fixed header — lets the hot path seal `header || payload`
    /// without materializing the concatenation (see `Sealer::seal_parts`).
    pub fn header(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = codec_to_u8(self.codec);
        h[1] = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        h[2..6].copy_from_slice(&self.seq.to_be_bytes());
        h[6..14].copy_from_slice(&self.timestamp_us.to_be_bytes());
        h[14..16].copy_from_slice(&self.width.to_be_bytes());
        h[16..18].copy_from_slice(&self.height.to_be_bytes());
        h
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::Malformed("video frame header truncated"));
        }
        Ok(Self {
            codec: codec_from_u8(buf[0])?,
            keyframe: buf[1] & FLAG_KEYFRAME != 0,
            seq: u32::from_be_bytes(buf[2..6].try_into().unwrap()),
            timestamp_us: u64::from_be_bytes(buf[6..14].try_into().unwrap()),
            width: u16::from_be_bytes(buf[14..16].try_into().unwrap()),
            height: u16::from_be_bytes(buf[16..18].try_into().unwrap()),
            payload: buf[HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = VideoFrame {
            codec: Codec::H264,
            keyframe: true,
            seq: 1234,
            timestamp_us: 987654321,
            width: 1920,
            height: 1080,
            payload: vec![0xAB; 64],
        };
        assert_eq!(VideoFrame::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn truncated_rejected() {
        assert!(VideoFrame::decode(&[0u8; 5]).is_err());
    }
}
