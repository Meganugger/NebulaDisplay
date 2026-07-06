//! Synthetic animated frame source.
//!
//! Deliberately encoder-hostile *and* human-verifiable: a slowly hue-shifting
//! gradient (tests rate control), a bouncing high-contrast block (tests
//! motion + latency perception), scrolling diagonal bars (tests temporal
//! detail), and a binary sequence strip along the top edge (lets automated
//! tests confirm pixels actually change frame-to-frame).

use ndsp_protocol::messages::DisplayMode;
use std::sync::Arc;

use super::{CursorUpdate, FrameSource};
use crate::state::CursorShapeData;

pub struct TestPatternSource {
    width: u32,
    height: u32,
    tick: u64,
    /// Synthetic cursor (circular path) so the cursor channel is exercisable
    /// everywhere — CI, dev machines, browser E2E.
    cursor_sent_shape: bool,
    last_cursor: (f32, f32),
    composite_cursor: bool,
}

impl TestPatternSource {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            tick: 0,
            cursor_sent_shape: false,
            last_cursor: (-1.0, -1.0),
            composite_cursor: false,
        }
    }

    fn cursor_pos(&self) -> (f32, f32) {
        // One revolution every 240 ticks around the center third.
        let a = (self.tick as f32) * (std::f32::consts::TAU / 240.0);
        (0.5 + 0.25 * a.cos(), 0.5 + 0.25 * a.sin())
    }

    /// 12×19 white-outlined black arrow, procedurally drawn.
    fn cursor_shape() -> CursorShapeData {
        let (w, h) = (12u16, 19u16);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        for y in 0..h as usize {
            // Classic pointer silhouette: widening triangle with a tail.
            let row_w = if y < 12 { y + 1 } else { 12 - (y - 12).min(7) };
            for x in 0..row_w.min(w as usize) {
                let i = (y * w as usize + x) * 4;
                let edge = x == 0 || x == row_w - 1 || y == 0 || y == h as usize - 1;
                let c = if edge { 255 } else { 0 };
                rgba[i] = c;
                rgba[i + 1] = c;
                rgba[i + 2] = c;
                rgba[i + 3] = 255;
            }
        }
        CursorShapeData {
            width: w,
            height: h,
            hot_x: 0,
            hot_y: 0,
            rgba,
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

        // Composite the synthetic cursor when a legacy client needs it in
        // the frame (mirrors the DXGI behavior).
        if self.composite_cursor {
            let (cx, cy) = self.cursor_pos();
            let shape = Self::cursor_shape();
            let px = (cx * w as f32) as usize;
            let py = (cy * h as f32) as usize;
            for sy in 0..shape.height as usize {
                for sx in 0..shape.width as usize {
                    let (dx, dy) = (px + sx, py + sy);
                    if dx >= w || dy >= h {
                        continue;
                    }
                    let si = (sy * shape.width as usize + sx) * 4;
                    if shape.rgba[si + 3] == 0 {
                        continue;
                    }
                    let di = (dy * w + dx) * 4;
                    out[di] = shape.rgba[si + 2];
                    out[di + 1] = shape.rgba[si + 1];
                    out[di + 2] = shape.rgba[si];
                }
            }
        }

        self.tick += 1;
        Ok(true)
    }

    fn cursor(&mut self) -> Option<CursorUpdate> {
        let (x, y) = self.cursor_pos();
        let shape = if self.cursor_sent_shape {
            None
        } else {
            self.cursor_sent_shape = true;
            Some(Arc::new(Self::cursor_shape()))
        };
        if (x, y) == self.last_cursor && shape.is_none() {
            return None;
        }
        self.last_cursor = (x, y);
        Some(CursorUpdate {
            x,
            y,
            visible: true,
            shape,
        })
    }

    fn set_composite_cursor(&mut self, on: bool) {
        self.composite_cursor = on;
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
