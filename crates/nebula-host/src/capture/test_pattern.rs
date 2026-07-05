//! Synthetic animated frame source.
//!
//! Draws a moving gradient, a bouncing square, and a seconds progress bar so
//! that dirty-region detection, adaptive quality, and viewer rendering can be
//! exercised end-to-end without any capture hardware. Deliberately cheap to
//! generate (a few ms at 1080p) and produces localized change regions.

use std::time::Instant;

use super::{Frame, FrameSource};

pub struct TestPatternSource {
    width: u32,
    height: u32,
    start: Instant,
    frame_no: u64,
    buf: Vec<u8>,
}

impl TestPatternSource {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(64);
        let height = height.max(64);
        Self {
            width,
            height,
            start: Instant::now(),
            frame_no: 0,
            buf: vec![0u8; (width * height * 4) as usize],
        }
    }

    fn draw(&mut self) {
        let (w, h) = (self.width as usize, self.height as usize);
        let t = self.start.elapsed().as_secs_f32();

        // Static-ish background gradient (changes very slowly => small dirty
        // regions most frames, which is representative of desktop content).
        if self.frame_no == 0 {
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    self.buf[i] = (x * 255 / w) as u8; // B
                    self.buf[i + 1] = 40; // G
                    self.buf[i + 2] = (y * 255 / h) as u8; // R
                    self.buf[i + 3] = 255;
                }
            }
        }

        // Bouncing square.
        let sq = (w.min(h) / 6).max(16);
        let px = ((t * 0.35).sin() * 0.5 + 0.5) * (w - sq) as f32;
        let py = ((t * 0.27).cos() * 0.5 + 0.5) * (h - sq) as f32;
        let (px, py) = (px as usize, py as usize);
        // Erase the previous square position by redrawing a generous band of
        // background around the whole travel area cheaply: instead, redraw
        // background under a bounding box around old+new positions.
        let margin = sq * 2;
        let x0 = px.saturating_sub(margin);
        let y0 = py.saturating_sub(margin);
        let x1 = (px + sq + margin).min(w);
        let y1 = (py + sq + margin).min(h);
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * w + x) * 4;
                self.buf[i] = (x * 255 / w) as u8;
                self.buf[i + 1] = 40;
                self.buf[i + 2] = (y * 255 / h) as u8;
                self.buf[i + 3] = 255;
            }
        }
        let hue = ((t * 40.0) as u32 % 255) as u8;
        for y in py..(py + sq).min(h) {
            for x in px..(px + sq).min(w) {
                let i = (y * w + x) * 4;
                self.buf[i] = 255 - hue;
                self.buf[i + 1] = hue;
                self.buf[i + 2] = 200;
                self.buf[i + 3] = 255;
            }
        }

        // Seconds progress bar along the bottom (thin, changes every frame).
        let bar_h = (h / 40).max(4);
        let filled = ((t % 1.0) * w as f32) as usize;
        for y in (h - bar_h)..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let on = x < filled;
                self.buf[i] = if on { 0 } else { 30 };
                self.buf[i + 1] = if on { 220 } else { 30 };
                self.buf[i + 2] = if on { 120 } else { 30 };
                self.buf[i + 3] = 255;
            }
        }

        self.frame_no += 1;
    }
}

impl FrameSource for TestPatternSource {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, _timeout_ms: u32) -> anyhow::Result<Option<Frame>> {
        self.draw();
        Ok(Some(Frame {
            bgra: self.buf.clone(),
            width: self.width,
            height: self.height,
            captured_at: Instant::now(),
        }))
    }

    fn describe(&self) -> String {
        format!("Test pattern {}x{}", self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_frames_with_changes() {
        let mut s = TestPatternSource::new(320, 200);
        let f1 = s.next_frame(0).unwrap().unwrap();
        let f2 = s.next_frame(0).unwrap().unwrap();
        assert_eq!(f1.bgra.len(), 320 * 200 * 4);
        assert_ne!(
            f1.bgra, f2.bgra,
            "consecutive frames must differ (animation)"
        );
    }
}
