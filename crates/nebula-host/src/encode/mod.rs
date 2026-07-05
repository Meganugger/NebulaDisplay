//! Video encoding: dirty-region detection + per-rect JPEG encoding.
//!
//! v1 ships a Motion-JPEG pipeline because it is dependency-light, works in
//! every browser via `createImageBitmap`, has no inter-frame state (instant
//! recovery from packet loss / reconnects), and combined with dirty-rect
//! updates is remarkably efficient for desktop content. The packet format
//! already carries codec ids for H.264/HEVC/AV1; the Windows Media
//! Foundation hardware path plugs in behind the same [`RegionEncoder`]
//! interface (see `docs/ARCHITECTURE.md`, "Encoder roadmap").

pub mod dirty;

use anyhow::Context;

/// Encodes a sub-rectangle of a BGRA frame into a compressed payload.
pub trait RegionEncoder: Send {
    /// Encode `rect` of the given frame. `quality` is 1..=100.
    fn encode_region(
        &mut self,
        bgra: &[u8],
        frame_w: u32,
        frame_h: u32,
        rect: dirty::Rect,
        quality: u8,
    ) -> anyhow::Result<Vec<u8>>;

    fn codec(&self) -> nebula_proto::VideoCodec;
}

/// JPEG region encoder built on the pure-Rust `jpeg-encoder` crate (SIMD).
pub struct JpegRegionEncoder {
    /// Scratch RGB buffer reused across frames to avoid re-allocation.
    rgb: Vec<u8>,
}

impl Default for JpegRegionEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegRegionEncoder {
    pub fn new() -> Self {
        Self { rgb: Vec::new() }
    }
}

impl RegionEncoder for JpegRegionEncoder {
    fn encode_region(
        &mut self,
        bgra: &[u8],
        frame_w: u32,
        _frame_h: u32,
        rect: dirty::Rect,
        quality: u8,
    ) -> anyhow::Result<Vec<u8>> {
        let (rw, rh) = (rect.w as usize, rect.h as usize);
        let fw = frame_w as usize;
        self.rgb.resize(rw * rh * 3, 0);

        // BGRA (frame) → RGB (rect) repack.
        for row in 0..rh {
            let src_off = ((rect.y as usize + row) * fw + rect.x as usize) * 4;
            let src = &bgra[src_off..src_off + rw * 4];
            let dst = &mut self.rgb[row * rw * 3..(row + 1) * rw * 3];
            for i in 0..rw {
                dst[i * 3] = src[i * 4 + 2]; // R
                dst[i * 3 + 1] = src[i * 4 + 1]; // G
                dst[i * 3 + 2] = src[i * 4]; // B
            }
        }

        let mut out = Vec::with_capacity(rw * rh / 4);
        let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
        encoder
            .encode(
                &self.rgb,
                rect.w as u16,
                rect.h as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .context("jpeg encode")?;
        Ok(out)
    }

    fn codec(&self) -> nebula_proto::VideoCodec {
        nebula_proto::VideoCodec::Jpeg
    }
}

#[cfg(test)]
mod tests {
    use super::dirty::Rect;
    use super::*;

    #[test]
    fn encodes_valid_jpeg() {
        let (w, h) = (64u32, 48u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for px in bgra.chunks_exact_mut(4) {
            px[0] = 200; // B
            px[1] = 100; // G
            px[2] = 50; // R
            px[3] = 255;
        }
        let mut enc = JpegRegionEncoder::new();
        let jpeg = enc
            .encode_region(
                &bgra,
                w,
                h,
                Rect {
                    x: 8,
                    y: 8,
                    w: 32,
                    h: 16,
                },
                80,
            )
            .unwrap();
        // JPEG magic (SOI marker) and EOI trailer.
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
    }

    #[test]
    fn quality_changes_size() {
        let (w, h) = (128u32, 128u32);
        // Noise-ish content so quality matters.
        let bgra: Vec<u8> = (0..(w * h * 4) as usize)
            .map(|i| ((i * 2654435761usize) >> 13) as u8)
            .collect();
        let mut enc = JpegRegionEncoder::new();
        let full = Rect { x: 0, y: 0, w, h };
        let hi = enc.encode_region(&bgra, w, h, full, 90).unwrap();
        let lo = enc.encode_region(&bgra, w, h, full, 20).unwrap();
        assert!(lo.len() < hi.len(), "lower quality must shrink output");
    }
}
