//! JPEG fallback encoder — universal decode support (every browser/OS),
//! zero codec negotiation risk, at the cost of bandwidth. The adaptive
//! controller's bitrate budget is mapped to JPEG quality.

use jpeg_encoder::{ColorType, Encoder as JEncoder};
use ndsp_protocol::messages::Codec;

use super::{Encoded, Encoder};
use crate::state::CapturedFrame;

pub struct JpegEncoder {
    rgb_scratch: Vec<u8>,
}

impl JpegEncoder {
    pub fn new() -> Self {
        Self {
            rgb_scratch: Vec::new(),
        }
    }

    /// Map a bitrate budget to JPEG quality for a given frame rate/size.
    /// Rough model: bytes/frame = bitrate / 8 / fps; quality scales with
    /// bytes-per-pixel budget.
    fn quality_for(&self, frame: &CapturedFrame, bitrate_kbps: u32, fps: u32) -> u8 {
        let bytes_per_frame = (bitrate_kbps as f64 * 1000.0 / 8.0) / fps.max(1) as f64;
        let pixels = (frame.width * frame.height) as f64;
        let bpp = bytes_per_frame / pixels.max(1.0);
        // Empirical mapping: 0.05 bpp ≈ q40, 0.2 bpp ≈ q75, 0.5+ bpp ≈ q90.
        let q = 30.0 + (bpp * 180.0);
        q.clamp(25.0, 92.0) as u8
    }
}

impl Encoder for JpegEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        _force_keyframe: bool,
        target_bitrate_kbps: u32,
        fps_hint: u32,
    ) -> anyhow::Result<Encoded> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        anyhow::ensure!(frame.bgra.len() == w * h * 4, "frame buffer size mismatch");

        // BGRA → RGB swizzle into a reused scratch buffer.
        self.rgb_scratch.resize(w * h * 3, 0);
        for (src, dst) in frame
            .bgra
            .chunks_exact(4)
            .zip(self.rgb_scratch.chunks_exact_mut(3))
        {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
        }

        let quality = self.quality_for(frame, target_bitrate_kbps, fps_hint);
        let mut out = Vec::with_capacity(w * h / 4);
        let enc = JEncoder::new(&mut out, quality);
        enc.encode(
            &self.rgb_scratch,
            frame.width as u16,
            frame.height as u16,
            ColorType::Rgb,
        )?;
        Ok(Encoded {
            payload: out,
            keyframe: true,
            codec: Codec::Jpeg,
        })
    }
}
