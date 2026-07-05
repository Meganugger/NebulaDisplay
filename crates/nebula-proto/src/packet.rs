//! Binary media packets.
//!
//! Binary WebSocket messages start with a one-byte channel id followed by a
//! fixed little-endian header, then the payload. Layouts are frozen; new
//! layouts require a new `packet_version` byte value.
//!
//! ```text
//! VIDEO (channel 0x01), header 28 bytes:
//!   [0]      channel        = 0x01
//!   [1]      packet_version = 1
//!   [2]      codec          (VideoCodec)
//!   [3]      flags          bit0: full frame (else dirty rect)
//!                           bit1: keyframe (codec-level IDR)
//!   [4..8]   frame_id       u32
//!   [8..16]  capture_ts     u64 microseconds (host monotonic clock)
//!   [16..18] x              u16   dirty-rect origin in stream pixels
//!   [18..20] y              u16
//!   [20..22] w              u16   dirty-rect size
//!   [22..24] h              u16
//!   [24..26] stream_w       u16   full stream size (canvas size)
//!   [26..28] stream_h       u16
//!   [28..]   payload        encoded image/bitstream for the rect
//!
//! AUDIO (channel 0x02), header 20 bytes:
//!   [0]      channel        = 0x02
//!   [1]      packet_version = 1
//!   [2]      codec          (AudioCodec)
//!   [3]      channels
//!   [4..8]   seq            u32
//!   [8..16]  capture_ts     u64 microseconds
//!   [16..20] sample_rate    u32
//!   [20..]   payload
//! ```

use thiserror::Error;

pub const CHANNEL_VIDEO: u8 = 0x01;
pub const CHANNEL_AUDIO: u8 = 0x02;

pub const VIDEO_HEADER_LEN: usize = 28;
pub const AUDIO_HEADER_LEN: usize = 20;

pub const VIDEO_FLAG_FULL_FRAME: u8 = 0b0000_0001;
pub const VIDEO_FLAG_KEYFRAME: u8 = 0b0000_0010;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    Jpeg = 1,
    H264 = 2,
    Hevc = 3,
    Av1 = 4,
}

impl VideoCodec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Jpeg),
            2 => Some(Self::H264),
            3 => Some(Self::Hevc),
            4 => Some(Self::Av1),
            _ => None,
        }
    }

    /// Capability string used in `Hello`/`SessionStart` for this codec.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Jpeg => crate::caps::VIDEO_MJPEG,
            Self::H264 => crate::caps::VIDEO_H264,
            Self::Hevc => crate::caps::VIDEO_HEVC,
            Self::Av1 => crate::caps::VIDEO_AV1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    PcmS16le = 1,
    Opus = 2,
}

impl AudioCodec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::PcmS16le),
            2 => Some(Self::Opus),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PacketError {
    #[error("packet too short: {0} bytes")]
    TooShort(usize),
    #[error("unknown channel {0:#04x}")]
    UnknownChannel(u8),
    #[error("unsupported packet version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown codec id {0}")]
    UnknownCodec(u8),
}

/// A parsed video packet borrowing its payload from the input buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket<'a> {
    pub codec: VideoCodec,
    pub full_frame: bool,
    pub keyframe: bool,
    pub frame_id: u32,
    pub capture_ts_micros: u64,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub stream_w: u16,
    pub stream_h: u16,
    pub payload: &'a [u8],
}

impl<'a> VideoPacket<'a> {
    /// Serialize into a fresh buffer (header + payload copy).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + self.payload.len());
        out.push(CHANNEL_VIDEO);
        out.push(1); // packet_version
        out.push(self.codec as u8);
        let mut flags = 0u8;
        if self.full_frame {
            flags |= VIDEO_FLAG_FULL_FRAME;
        }
        if self.keyframe {
            flags |= VIDEO_FLAG_KEYFRAME;
        }
        out.push(flags);
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.capture_ts_micros.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        out.extend_from_slice(&self.stream_w.to_le_bytes());
        out.extend_from_slice(&self.stream_h.to_le_bytes());
        out.extend_from_slice(self.payload);
        out
    }

    /// Parse from a raw binary message (must start with `CHANNEL_VIDEO`).
    pub fn decode(buf: &'a [u8]) -> Result<Self, PacketError> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err(PacketError::TooShort(buf.len()));
        }
        if buf[0] != CHANNEL_VIDEO {
            return Err(PacketError::UnknownChannel(buf[0]));
        }
        if buf[1] != 1 {
            return Err(PacketError::UnsupportedVersion(buf[1]));
        }
        let codec = VideoCodec::from_u8(buf[2]).ok_or(PacketError::UnknownCodec(buf[2]))?;
        let flags = buf[3];
        let u32le = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        let u16le = |i: usize| u16::from_le_bytes(buf[i..i + 2].try_into().unwrap());
        let u64le = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        Ok(Self {
            codec,
            full_frame: flags & VIDEO_FLAG_FULL_FRAME != 0,
            keyframe: flags & VIDEO_FLAG_KEYFRAME != 0,
            frame_id: u32le(4),
            capture_ts_micros: u64le(8),
            x: u16le(16),
            y: u16le(18),
            w: u16le(20),
            h: u16le(22),
            stream_w: u16le(24),
            stream_h: u16le(26),
            payload: &buf[VIDEO_HEADER_LEN..],
        })
    }
}

/// A parsed audio packet borrowing its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub codec: AudioCodec,
    pub channels: u8,
    pub seq: u32,
    pub capture_ts_micros: u64,
    pub sample_rate: u32,
    pub payload: &'a [u8],
}

impl<'a> AudioPacket<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        out.push(CHANNEL_AUDIO);
        out.push(1);
        out.push(self.codec as u8);
        out.push(self.channels);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.capture_ts_micros.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(self.payload);
        out
    }

    pub fn decode(buf: &'a [u8]) -> Result<Self, PacketError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(PacketError::TooShort(buf.len()));
        }
        if buf[0] != CHANNEL_AUDIO {
            return Err(PacketError::UnknownChannel(buf[0]));
        }
        if buf[1] != 1 {
            return Err(PacketError::UnsupportedVersion(buf[1]));
        }
        let codec = AudioCodec::from_u8(buf[2]).ok_or(PacketError::UnknownCodec(buf[2]))?;
        Ok(Self {
            codec,
            channels: buf[3],
            seq: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            capture_ts_micros: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            sample_rate: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            payload: &buf[AUDIO_HEADER_LEN..],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_round_trip() {
        let p = VideoPacket {
            codec: VideoCodec::Jpeg,
            full_frame: true,
            keyframe: true,
            frame_id: 42,
            capture_ts_micros: 123_456_789,
            x: 10,
            y: 20,
            w: 640,
            h: 360,
            stream_w: 1920,
            stream_h: 1080,
            payload: b"jpegdata",
        };
        let bytes = p.encode();
        assert_eq!(bytes[0], CHANNEL_VIDEO);
        let q = VideoPacket::decode(&bytes).unwrap();
        assert_eq!(p, q);
    }

    #[test]
    fn audio_round_trip() {
        let p = AudioPacket {
            codec: AudioCodec::Opus,
            channels: 2,
            seq: 7,
            capture_ts_micros: 999,
            sample_rate: 48_000,
            payload: &[1, 2, 3],
        };
        let bytes = p.encode();
        let q = AudioPacket::decode(&bytes).unwrap();
        assert_eq!(p, q);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(VideoPacket::decode(&[]), Err(PacketError::TooShort(0)));
        assert_eq!(
            VideoPacket::decode(&[0x09; 40]),
            Err(PacketError::UnknownChannel(0x09))
        );
        let mut bad_version = VideoPacket {
            codec: VideoCodec::Jpeg,
            full_frame: false,
            keyframe: false,
            frame_id: 0,
            capture_ts_micros: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            stream_w: 0,
            stream_h: 0,
            payload: &[],
        }
        .encode();
        bad_version[1] = 99;
        assert_eq!(
            VideoPacket::decode(&bad_version),
            Err(PacketError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn header_length_is_frozen() {
        // The wire layout is a compatibility contract; this test freezes it.
        let p = VideoPacket {
            codec: VideoCodec::H264,
            full_frame: false,
            keyframe: false,
            frame_id: 0,
            capture_ts_micros: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            stream_w: 0,
            stream_h: 0,
            payload: &[],
        };
        assert_eq!(p.encode().len(), VIDEO_HEADER_LEN);
    }
}
