//! Synthetic animated frame source.
//!
//! Deliberately encoder-hostile *and* human-verifiable: a slowly hue-shifting
//! gradient (tests rate control), a bouncing high-contrast block (tests
//! motion + latency perception), scrolling diagonal bars (tests temporal
//! detail), and a binary sequence strip along the top edge (lets automated
//! tests confirm pixels actually change frame-to-frame).

use ndsp_protocol::messages::DisplayMode;

use super::FrameSource;

pub struct TestPatternSource {
    width: u32,
    height: u32,
    tick: u64,
}

impl TestPatternSource {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            tick: 0,
        }
    }
}

/// Cheap HSV→RGB for the animated background (h in 0..360).
fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h as u32 / 60) % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

impl FrameSource for TestPatternSource {
    fn name(&self) -> &'static str {
        "test-pattern"
    }

    fn mode(&self) -> DisplayMode {
        DisplayMode {
            width: self.width,
            height: self.height,
            refresh_hz: 60,
        }
    }

    fn next_frame(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        let (w, h) = (self.width as usize, self.height as usize);
        out.resize(w * h * 4, 0);
        let t = self.tick;

        // Background: horizontal hue gradient drifting over time.
        let base_hue = (t % 3600) as f32 / 10.0;
        for y in 0..h {
            let row = &mut out[y * w * 4..(y + 1) * w * 4];
            let vy = 0.35 + 0.3 * (y as f32 / h as f32);
            for x in 0..w {
                let hue = (base_hue + x as f32 * 360.0 / w as f32) % 360.0;
                let (r, g, b) = hsv(hue, 0.55, vy);
                let px = &mut row[x * 4..x * 4 + 4];
                px[0] = b;
                px[1] = g;
                px[2] = r;
                px[3] = 255;
            }
        }

        // Scrolling diagonal bars (bottom third).
        let bar_top = h * 2 / 3;
        for y in bar_top..h {
            let row = &mut out[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                if ((x + y + t as usize * 3) / 24).is_multiple_of(2) {
                    let px = &mut row[x * 4..x * 4 + 4];
                    px[0] /= 3;
                    px[1] /= 3;
                    px[2] /= 3;
                }
            }
        }

        // Bouncing block.
        let bw = (w / 8).max(8);
        let bh = (h / 8).max(8);
        let px_range = w - bw;
        let py_range = h - bh;
        let bounce = |range: usize, speed: usize| -> usize {
            let p = (t as usize * speed) % (range * 2);
            if p < range {
                p
            } else {
                range * 2 - p
            }
        };
        let bx = bounce(px_range, 7);
        let by = bounce(py_range, 5);
        for y in by..by + bh {
            let row = &mut out[y * w * 4..(y + 1) * w * 4];
            for x in bx..bx + bw {
                let px = &mut row[x * 4..x * 4 + 4];
                // High-contrast inverse block with white border.
                let edge = y < by + 3 || y >= by + bh - 3 || x < bx + 3 || x >= bx + bw - 3;
                if edge {
                    px[0] = 255;
                    px[1] = 255;
                    px[2] = 255;
                } else {
                    px[0] = 255 - px[0];
                    px[1] = 255 - px[1];
                    px[2] = 255 - px[2];
                }
            }
        }

        // Binary sequence strip: 32 blocks along the top edge encode the tick
        // so tests can assert frames differ.
        let block_w = (w / 32).max(1);
        for bit in 0..32usize {
            let on = (t >> bit) & 1 == 1;
            let color = if on { 255u8 } else { 0u8 };
            for y in 0..8.min(h) {
                let row = &mut out[y * w * 4..(y + 1) * w * 4];
                for x in bit * block_w..((bit + 1) * block_w).min(w) {
                    let px = &mut row[x * 4..x * 4 + 4];
                    px[0] = color;
                    px[1] = color;
                    px[2] = color;
                    px[3] = 255;
                }
            }
        }

        self.tick += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_change_over_time() {
        let mut src = TestPatternSource::new(64, 64);
        let mut a = Vec::new();
        let mut b = Vec::new();
        assert!(src.next_frame(&mut a).unwrap());
        assert!(src.next_frame(&mut b).unwrap());
        assert_eq!(a.len(), 64 * 64 * 4);
        assert_ne!(a, b, "consecutive frames must differ");
    }
}
