//! OpenH264 software encoder, tuned for screen content + low latency.
//!
//! Output is Annex-B (start-code delimited) H.264 baseline, decodable by
//! WebCodecs (no `description`), Android MediaCodec, and VideoToolbox.
//!
//! Performance/latency notes:
//! * BGRA capture is converted to I420 in a **single integer pass** (same
//!   BT.601 coefficients as openh264's own converter, so colors are
//!   identical) instead of the previous BGRA→RGB→YUV double pass.
//! * Bitrate and frame-rate changes are applied **at runtime** through the
//!   encoder's `SetOption` API — the encoder is never torn down for rate
//!   adaptation, so reference frames survive and no IDR storm happens.
//!   Re-creation only occurs on resolution change (or if `SetOption` fails).

use ndsp_protocol::messages::Codec;
use openh264::encoder::{
    BitRate, Complexity, Encoder as OEncoder, EncoderConfig, FrameRate, FrameType,
    IntraFramePeriod, Profile, RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

use super::{Encoded, Encoder};
use crate::state::CapturedFrame;

pub struct H264Encoder {
    inner: OEncoder,
    yuv: I420Buffer,
    current_bitrate_kbps: u32,
    current_fps: u32,
    size: (usize, usize),
    force_key_next: bool,
    last_rebuild: std::time::Instant,
}

/// Plain I420 planes implementing openh264's `YUVSource` so we can feed the
/// encoder without intermediate conversions.
#[derive(Default)]
struct I420Buffer {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    w: usize,
    h: usize,
}

impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.w, self.w / 2, self.w / 2)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

impl I420Buffer {
    fn ensure(&mut self, w: usize, h: usize) {
        if self.w != w || self.h != h {
            self.w = w;
            self.h = h;
            self.y.resize(w * h, 0);
            self.u.resize(w * h / 4, 0);
            self.v.resize(w * h / 4, 0);
        }
    }

    /// Single-pass BGRA → I420 (BT.601 limited range, integer math — the
    /// exact coefficients openh264's own RGB converter uses, so switching to
    /// this direct path changes no colors). Processes 2×2 blocks through
    /// slice iterators so each source pixel is read exactly once and the
    /// inner loop stays bounds-check free.
    fn fill_from_bgra(&mut self, bgra: &[u8], w: usize, h: usize) {
        self.ensure(w, h);
        debug_assert_eq!(bgra.len(), w * h * 4);
        let half_w = w / 2;
        let row = w * 4;
        #[inline(always)]
        fn luma(p: &[u8]) -> u8 {
            (((66 * p[2] as u32 + 129 * p[1] as u32 + 25 * p[0] as u32) >> 8) + 16) as u8
        }
        let src_pairs = bgra.chunks_exact(row * 2);
        let y_pairs = self.y.chunks_exact_mut(w * 2);
        let u_rows = self.u.chunks_exact_mut(half_w);
        let v_rows = self.v.chunks_exact_mut(half_w);
        for (((src2, y2), u_row), v_row) in src_pairs.zip(y_pairs).zip(u_rows).zip(v_rows) {
            let (src0, src1) = src2.split_at(row);
            let (y0, y1) = y2.split_at_mut(w);
            let it = src0
                .chunks_exact(8)
                .zip(src1.chunks_exact(8))
                .zip(y0.chunks_exact_mut(2))
                .zip(y1.chunks_exact_mut(2))
                .zip(u_row.iter_mut())
                .zip(v_row.iter_mut());
            for (((((s0, s1), yo0), yo1), u), v) in it {
                let (p00, p01) = (&s0[0..4], &s0[4..8]);
                let (p10, p11) = (&s1[0..4], &s1[4..8]);
                yo0[0] = luma(p00);
                yo0[1] = luma(p01);
                yo1[0] = luma(p10);
                yo1[1] = luma(p11);
                let r = (p00[2] as i32 + p01[2] as i32 + p10[2] as i32 + p11[2] as i32 + 2) / 4;
                let g = (p00[1] as i32 + p01[1] as i32 + p10[1] as i32 + p11[1] as i32 + 2) / 4;
                let b = (p00[0] as i32 + p01[0] as i32 + p10[0] as i32 + p11[0] as i32 + 2) / 4;
                *u = (((-38 * r - 74 * g + 112 * b) >> 8) + 128) as u8;
                *v = (((112 * r - 94 * g - 18 * b) >> 8) + 128) as u8;
            }
        }
    }
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
        .skip_frames(true)
        // Speed over marginal quality: this is a real-time screen encoder;
        // the bitrate controller owns quality. Measured at 1080p on 4 vCPUs:
        // 36.4 → 31.5 ms/frame vs the default (medium) complexity.
        .complexity(Complexity::Low);
    OEncoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| anyhow::anyhow!("openh264 init: {e}"))
}

impl H264Encoder {
    pub fn new() -> anyhow::Result<Self> {
        let bitrate = 6000;
        let fps = 60;
        Ok(Self {
            inner: build(bitrate, fps)?,
            yuv: I420Buffer::default(),
            current_bitrate_kbps: bitrate,
            current_fps: fps,
            size: (0, 0),
            force_key_next: false,
            last_rebuild: std::time::Instant::now(),
        })
    }

    /// Apply a new target bitrate without re-creating the encoder.
    fn set_bitrate_runtime(&mut self, bitrate_kbps: u32) -> bool {
        use openh264_sys2::{SBitrateInfo, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL};
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: (bitrate_kbps.max(100) * 1000) as std::os::raw::c_int,
        };
        // SAFETY: SBitrateInfo is the documented payload for
        // ENCODER_OPTION_BITRATE and outlives the call.
        let rc = unsafe {
            self.inner
                .raw_api()
                .set_option(ENCODER_OPTION_BITRATE, (&mut info as *mut _) as *mut _)
        };
        rc == 0
    }

    /// Apply a new max frame rate (rate-control input only) at runtime.
    fn set_framerate_runtime(&mut self, fps: u32) -> bool {
        use openh264_sys2::ENCODER_OPTION_FRAME_RATE;
        let mut rate: f32 = fps.max(1) as f32;
        // SAFETY: a float is the documented payload for
        // ENCODER_OPTION_FRAME_RATE and outlives the call.
        let rc = unsafe {
            self.inner
                .raw_api()
                .set_option(ENCODER_OPTION_FRAME_RATE, (&mut rate as *mut _) as *mut _)
        };
        rc == 0
    }

    /// Track rate-control targets. Runtime `SetOption` first; a full rebuild
    /// only as a rate-limited fallback (and always on resolution change).
    fn apply_targets(&mut self, bitrate_kbps: u32, fps: u32) -> anyhow::Result<()> {
        if bitrate_kbps != self.current_bitrate_kbps {
            if self.set_bitrate_runtime(bitrate_kbps) {
                self.current_bitrate_kbps = bitrate_kbps;
            } else if self.last_rebuild.elapsed() > std::time::Duration::from_secs(3) {
                tracing::warn!("runtime bitrate update failed; rebuilding encoder");
                self.rebuild(bitrate_kbps, fps)?;
                return Ok(());
            }
        }
        // Never rebuild for an fps change — it's only a rate-control hint.
        if fps != self.current_fps && self.set_framerate_runtime(fps) {
            self.current_fps = fps;
        }
        Ok(())
    }

    fn rebuild(&mut self, bitrate_kbps: u32, fps: u32) -> anyhow::Result<()> {
        self.inner = build(bitrate_kbps, fps)?;
        self.current_bitrate_kbps = bitrate_kbps;
        self.current_fps = fps;
        self.force_key_next = true;
        self.last_rebuild = std::time::Instant::now();
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
        if self.size != (w, h) {
            // Resolution changed (mode switch / rotation): a rebuild is the
            // only correct option here.
            if self.size != (0, 0) {
                tracing::info!(w, h, "capture resolution changed; rebuilding encoder");
                self.rebuild(target_bitrate_kbps, fps_hint)?;
            }
            self.size = (w, h);
        }
        self.apply_targets(target_bitrate_kbps, fps_hint)?;

        if force_keyframe || self.force_key_next {
            self.inner.force_intra_frame();
            self.force_key_next = false;
        }

        self.yuv.fill_from_bgra(&frame.bgra, w, h);

        let bitstream = self
            .inner
            .encode(&self.yuv)
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

#[cfg(test)]
mod conv_tests {
    use super::*;

    #[test]
    fn bgra_to_i420_matches_reference() {
        // Compare against openh264's own converter on random-ish data.
        let (w, h) = (64, 32);
        let mut bgra = vec![0u8; w * h * 4];
        for (i, b) in bgra.iter_mut().enumerate() {
            *b = ((i * 2654435761usize) >> 7) as u8;
        }
        let mut ours = I420Buffer::default();
        ours.fill_from_bgra(&bgra, w, h);

        let mut rgb = vec![0u8; w * h * 3];
        for (src, dst) in bgra.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
        }
        let slice = openh264::formats::RgbSliceU8::new(&rgb, (w, h));
        let mut reference = openh264::formats::YUVBuffer::new(w, h);
        reference.read_rgb8(slice);
        assert_eq!(ours.y(), reference.y(), "Y plane must match reference");
        assert_eq!(ours.u(), reference.u(), "U plane must match reference");
        assert_eq!(ours.v(), reference.v(), "V plane must match reference");
    }

    /// Ignored timing probe (run with --ignored --nocapture --release).
    #[test]
    #[ignore]
    fn print_conversion_timing() {
        let (w, h) = (1920usize, 1080usize);
        let bgra = vec![128u8; w * h * 4];
        let mut buf = I420Buffer::default();
        buf.fill_from_bgra(&bgra, w, h); // warm
        let t = std::time::Instant::now();
        const N: u32 = 100;
        for _ in 0..N {
            buf.fill_from_bgra(std::hint::black_box(&bgra), w, h);
        }
        println!(
            "1080p BGRA→I420: {:.2} ms/frame",
            t.elapsed().as_secs_f64() * 1000.0 / N as f64
        );
    }
}
