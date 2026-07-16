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

// ---------------------------------------------------------------------------
// Audio framing (channel 3)
// ---------------------------------------------------------------------------

/// Audio codecs carried on the audio channel.
///
/// * `Opus` — the default: 48 kHz, low-delay, ~10 ms frames.
/// * `PcmS16le` — uncompressed interleaved s16le. Used by web viewers on
///   insecure origins where WebCodecs (and thus an Opus decoder) does not
///   exist; ~1.5 Mbit/s at 48 kHz stereo, trivial on a LAN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
    PcmS16le,
}

fn audio_codec_to_u8(c: AudioCodec) -> u8 {
    match c {
        AudioCodec::Opus => 0,
        AudioCodec::PcmS16le => 1,
    }
}

fn audio_codec_from_u8(v: u8) -> Result<AudioCodec> {
    match v {
        0 => Ok(AudioCodec::Opus),
        1 => Ok(AudioCodec::PcmS16le),
        _ => Err(ProtocolError::Malformed("unknown audio codec id")),
    }
}

/// One audio packet (plaintext *inside* the encrypted audio channel).
///
/// ```text
/// [codec u8][channels u8][seq u32 BE][ts_us u64 BE][sample_rate u32 BE][payload…]
/// ```
///
/// * `ts_us` — capture timestamp (host clock, unix µs), same clock as video
///   frames so viewers can lip-sync and measure audio latency.
/// * `seq` — strictly increasing; a gap means packets were dropped and the
///   decoder should conceal rather than stall.
pub const AUDIO_HEADER_LEN: usize = 1 + 1 + 4 + 8 + 4;

#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub codec: AudioCodec,
    pub channels: u8,
    pub seq: u32,
    pub timestamp_us: u64,
    pub sample_rate: u32,
    pub payload: Vec<u8>,
}

impl AudioFrame {
    /// Just the fixed header (hot path seals `header ‖ payload` in place).
    pub fn header(&self) -> [u8; AUDIO_HEADER_LEN] {
        let mut h = [0u8; AUDIO_HEADER_LEN];
        h[0] = audio_codec_to_u8(self.codec);
        h[1] = self.channels;
        h[2..6].copy_from_slice(&self.seq.to_be_bytes());
        h[6..14].copy_from_slice(&self.timestamp_us.to_be_bytes());
        h[14..18].copy_from_slice(&self.sample_rate.to_be_bytes());
        h
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(ProtocolError::Malformed("audio frame header truncated"));
        }
        Ok(Self {
            codec: audio_codec_from_u8(buf[0])?,
            channels: buf[1],
            seq: u32::from_be_bytes(buf[2..6].try_into().unwrap()),
            timestamp_us: u64::from_be_bytes(buf[6..14].try_into().unwrap()),
            sample_rate: u32::from_be_bytes(buf[14..18].try_into().unwrap()),
            payload: buf[AUDIO_HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_roundtrip() {
        let f = AudioFrame {
            codec: AudioCodec::Opus,
            channels: 2,
            seq: 77,
            timestamp_us: 123456789,
            sample_rate: 48_000,
            payload: vec![0x5A; 120],
        };
        assert_eq!(AudioFrame::decode(&f.encode()).unwrap(), f);
        let p = AudioFrame {
            codec: AudioCodec::PcmS16le,
            ..f
        };
        assert_eq!(AudioFrame::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn audio_truncated_rejected() {
        assert!(AudioFrame::decode(&[0u8; 10]).is_err());
    }

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
