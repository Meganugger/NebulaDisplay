//! OpenH264 software encoder, tuned for screen content + low latency.
//!
//! Output is Annex-B (start-code delimited) H.264 baseline, decodable by
//! WebCodecs (no `description`), Android MediaCodec, and VideoToolbox.
//!
//! OpenH264 doesn't expose a clean runtime bitrate setter through the safe
//! wrapper, so large bitrate changes re-create the encoder (cheap: a few ms)
//! and force an IDR so decoders resynchronize immediately.

use ndsp_protocol::messages::Codec;
use openh264::encoder::{
    BitRate, Encoder as OEncoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;

use super::{Encoded, Encoder};
use crate::state::CapturedFrame;

pub struct H264Encoder {
    inner: OEncoder,
    yuv: Option<YUVBuffer>,
    yuv_dims: (usize, usize),
    rgb_scratch: Vec<u8>,
    current_bitrate_kbps: u32,
    current_fps: u32,
    force_key_next: bool,
}

fn build(bitrate_kbps: u32, fps: u32) -> anyhow::Result<OEncoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_kbps.max(100) * 1000))
        .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .profile(Profile::Baseline)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        // Periodic IDR as a safety net; explicit RequestKeyframe covers loss.
        .intra_frame_period(IntraFramePeriod::from_num_frames(300))
        .scene_change_detect(true)
        // Not supported for screen content (openh264 warns + auto-disables).
        .adaptive_quantization(false)
        .background_detection(false)
        // Required for bitrate mode to actually control the rate; skipped
        // frames surface as empty bitstreams and are simply not sent.
        .skip_frames(true);
    OEncoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| anyhow::anyhow!("openh264 init: {e}"))
}

impl H264Encoder {
    pub fn new() -> anyhow::Result<Self> {
        let bitrate = 6000;
        let fps = 60;
        Ok(Self {
            inner: build(bitrate, fps)?,
            yuv: None,
            yuv_dims: (0, 0),
            rgb_scratch: Vec::new(),
            current_bitrate_kbps: bitrate,
            current_fps: fps,
            force_key_next: false,
        })
    }

    /// Re-init on significant (>25%) bitrate moves or fps changes.
    fn maybe_reconfigure(&mut self, bitrate_kbps: u32, fps: u32) -> anyhow::Result<()> {
        let cur = self.current_bitrate_kbps as f64;
        let delta = (bitrate_kbps as f64 - cur).abs() / cur.max(1.0);
        if delta > 0.25 || fps != self.current_fps {
            self.inner = build(bitrate_kbps, fps)?;
            self.current_bitrate_kbps = bitrate_kbps;
            self.current_fps = fps;
            self.force_key_next = true;
            tracing::debug!(bitrate_kbps, fps, "h264 encoder reconfigured");
        }
        Ok(())
    }
}

impl Encoder for H264Encoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_keyframe: bool,
        target_bitrate_kbps: u32,
        fps_hint: u32,
    ) -> anyhow::Result<Encoded> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        anyhow::ensure!(frame.bgra.len() == w * h * 4, "frame buffer size mismatch");
        self.maybe_reconfigure(target_bitrate_kbps, fps_hint)?;

        if force_keyframe || self.force_key_next {
            self.inner.force_intra_frame();
            self.force_key_next = false;
        }

        // Reuse the YUV buffer across frames of the same size.
        if self.yuv.is_none() || self.yuv_dims != (w, h) {
            self.yuv = Some(YUVBuffer::new(w, h));
            self.yuv_dims = (w, h);
        }
        // BGRA → RGB swizzle (integer fast path into openh264's RGB8 reader).
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
        let src = RgbSliceU8::new(&self.rgb_scratch, (w, h));
        let yuv = self.yuv.as_mut().expect("yuv buffer initialized above");
        yuv.read_rgb8(src);

        let bitstream = self
            .inner
            .encode(yuv)
            .map_err(|e| anyhow::anyhow!("openh264 encode: {e}"))?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let skipped = matches!(bitstream.frame_type(), FrameType::Skip);
        let payload = bitstream.to_vec();
        if skipped || payload.is_empty() {
            // Rate controller elected to skip this frame — nothing to send.
            return Ok(Encoded {
                payload: Vec::new(),
                keyframe: false,
                codec: Codec::H264,
            });
        }
        Ok(Encoded {
            payload,
            keyframe,
            codec: Codec::H264,
        })
    }
}
