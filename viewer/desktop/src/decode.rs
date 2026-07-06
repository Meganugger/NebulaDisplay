//! Frame decoding for the desktop viewer: JPEG (always) + H.264 (feature).

use anyhow::Context;
use ndsp_protocol::media::VideoFrame;
use ndsp_protocol::messages::Codec;

use crate::RgbaFrame;

pub struct Decoder {
    #[cfg(feature = "h264")]
    h264: Option<openh264::decoder::Decoder>,
    /// Wait for a keyframe after start/decode errors.
    need_keyframe: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "h264")]
            h264: None,
            need_keyframe: true,
        }
    }

    pub fn decode(&mut self, frame: &VideoFrame) -> anyhow::Result<Option<RgbaFrame>> {
        match frame.codec {
            Codec::Jpeg => self.decode_jpeg(frame).map(Some),
            Codec::H264 => self.decode_h264(frame),
            other => anyhow::bail!("codec {other:?} not supported by this viewer"),
        }
    }

    fn decode_jpeg(&mut self, frame: &VideoFrame) -> anyhow::Result<RgbaFrame> {
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut dec = zune_jpeg::JpegDecoder::new_with_options(&frame.payload, opts);
        let rgba = dec.decode().context("jpeg decode")?;
        let (w, h) = dec.dimensions().context("jpeg dimensions")?;
        Ok(RgbaFrame {
            width: w as u32,
            height: h as u32,
            rgba,
            timestamp_us: frame.timestamp_us,
        })
    }

    #[cfg(feature = "h264")]
    fn decode_h264(&mut self, frame: &VideoFrame) -> anyhow::Result<Option<RgbaFrame>> {
        use openh264::decoder::Decoder as ODecoder;
        use openh264::formats::YUVSource;

        if self.need_keyframe && !frame.keyframe {
            return Ok(None); // wait for a resync point
        }
        self.need_keyframe = false;

        if self.h264.is_none() {
            self.h264 = Some(ODecoder::new().map_err(|e| anyhow::anyhow!("h264 init: {e}"))?);
        }
        let dec = self.h264.as_mut().expect("initialized above");
        match dec.decode(&frame.payload) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some(RgbaFrame {
                    width: w as u32,
                    height: h as u32,
                    rgba,
                    timestamp_us: frame.timestamp_us,
                }))
            }
            // Parameter sets / not-yet-emitting: nothing to show, not an error.
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!("h264 decode error ({e}); waiting for keyframe");
                self.need_keyframe = true;
                self.h264 = None;
                Ok(None)
            }
        }
    }

    #[cfg(not(feature = "h264"))]
    fn decode_h264(&mut self, _frame: &VideoFrame) -> anyhow::Result<Option<RgbaFrame>> {
        anyhow::bail!("this build has no H.264 support (rebuild with --features h264)")
    }
}
