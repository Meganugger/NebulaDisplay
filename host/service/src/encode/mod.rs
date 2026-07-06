//! Video encoders. Each client session owns its own encoder instance so
//! quality adapts per client.
//!
//! * JPEG — always available, pure Rust; every frame is a "keyframe".
//! * H.264 — OpenH264 (feature `h264`, on by default), screen-content tuned,
//!   Annex-B output that WebCodecs/MediaCodec/VideoToolbox all accept.
//!
//! Hardware encoders (Media Foundation → NVENC/QuickSync/AMF) plug in behind
//! the same trait; see `docs/ROADMAP.md`.

#[cfg(feature = "h264")]
mod h264;
mod jpeg;

use ndsp_protocol::messages::Codec;

use crate::state::CapturedFrame;

pub struct Encoded {
    pub payload: Vec<u8>,
    pub keyframe: bool,
    pub codec: Codec,
}

pub trait Encoder: Send {
    /// Encode one frame. `force_keyframe` requests an IDR/self-contained
    /// frame. `target_bitrate_kbps` is the adaptive controller's current
    /// budget; encoders map it to their own quality knobs.
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_keyframe: bool,
        target_bitrate_kbps: u32,
        fps_hint: u32,
    ) -> anyhow::Result<Encoded>;
}

/// Instantiate the encoder for a negotiated codec.
pub fn create(codec: Codec) -> anyhow::Result<Box<dyn Encoder>> {
    match codec {
        Codec::Jpeg => Ok(Box::new(jpeg::JpegEncoder::new())),
        #[cfg(feature = "h264")]
        Codec::H264 => Ok(Box::new(h264::H264Encoder::new()?)),
        #[cfg(not(feature = "h264"))]
        Codec::H264 => anyhow::bail!("built without the h264 feature"),
        other => anyhow::bail!("codec {other:?} not implemented yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> CapturedFrame {
        let mut src = crate::capture::test_pattern_for_tests(w, h);
        let mut buf = Vec::new();
        use crate::capture::FrameSource;
        src.next_frame(&mut buf).unwrap();
        CapturedFrame {
            seq: 1,
            timestamp_us: 1,
            width: w,
            height: h,
            bgra: buf,
        }
    }

    #[test]
    fn jpeg_encodes_valid_frames() {
        let mut enc = create(Codec::Jpeg).unwrap();
        let out = enc.encode(&frame(320, 240), true, 4000, 30).unwrap();
        assert!(out.keyframe);
        assert!(out.payload.len() > 500, "suspiciously small JPEG");
        assert_eq!(&out.payload[..2], &[0xFF, 0xD8], "JPEG SOI marker");
    }

    #[cfg(feature = "h264")]
    #[test]
    fn h264_first_frame_is_keyframe_annexb() {
        let mut enc = create(Codec::H264).unwrap();
        let out = enc.encode(&frame(320, 240), false, 2000, 30).unwrap();
        assert!(out.keyframe, "first frame must be IDR");
        assert!(
            out.payload.windows(4).any(|w| w == [0, 0, 0, 1]),
            "Annex-B start code expected"
        );
        // Second frame should be a (smaller) delta frame.
        let out2 = enc.encode(&frame(320, 240), false, 2000, 30).unwrap();
        assert!(!out2.keyframe);
        // Forced keyframe honored.
        let out3 = enc.encode(&frame(320, 240), true, 2000, 30).unwrap();
        assert!(out3.keyframe);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn h264_bitrate_reconfigure_survives() {
        let mut enc = create(Codec::H264).unwrap();
        enc.encode(&frame(320, 240), false, 8000, 60).unwrap();
        // Big bitrate drop triggers encoder re-init; must keep working and
        // emit a keyframe so decoders can resync.
        let out = enc.encode(&frame(320, 240), false, 500, 30).unwrap();
        assert!(out.keyframe, "re-init must produce a keyframe");
    }
}
