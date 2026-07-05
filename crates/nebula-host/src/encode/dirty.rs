//! Dirty-region detection via per-tile hashing.
//!
//! The frame is divided into a grid of tiles (default 64×64). Each tile's
//! bytes are hashed with FNV-1a; comparing hashes against the previous frame
//! yields the set of changed tiles, from which we compute a bounding
//! rectangle. Encoding and sending only that rectangle typically cuts
//! bandwidth by 10–100× for office-style content where most of the screen is
//! static.
//!
//! A bounding rect (rather than per-tile packets) keeps the protocol and the
//! viewer trivially simple; per-tile update lists are a straightforward
//! future optimization the packet format already permits (multiple video
//! packets may share one `frame_id`).

pub const TILE: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn full(w: u32, h: u32) -> Self {
        Self { x: 0, y: 0, w, h }
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

pub struct DirtyDetector {
    tiles_x: u32,
    tiles_y: u32,
    width: u32,
    height: u32,
    hashes: Vec<u64>,
    /// True until the first frame is analyzed (forces a full-frame update).
    first: bool,
}

impl DirtyDetector {
    pub fn new(width: u32, height: u32) -> Self {
        let tiles_x = width.div_ceil(TILE);
        let tiles_y = height.div_ceil(TILE);
        Self {
            tiles_x,
            tiles_y,
            width,
            height,
            hashes: vec![0; (tiles_x * tiles_y) as usize],
            first: true,
        }
    }

    /// Analyze a BGRA frame. Returns:
    /// * `None` — nothing changed.
    /// * `Some(rect)` — bounding rect of all changed tiles (whole frame on
    ///   the first call or after a resolution change).
    pub fn detect(&mut self, bgra: &[u8], width: u32, height: u32) -> Option<Rect> {
        if width != self.width || height != self.height {
            *self = Self::new(width, height);
        }

        let mut min_tx = u32::MAX;
        let mut min_ty = u32::MAX;
        let mut max_tx = 0u32;
        let mut max_ty = 0u32;
        let mut any = false;

        for ty in 0..self.tiles_y {
            let y0 = ty * TILE;
            let y1 = (y0 + TILE).min(height);
            for tx in 0..self.tiles_x {
                let x0 = tx * TILE;
                let x1 = (x0 + TILE).min(width);
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
                for y in y0..y1 {
                    let row_start = ((y * width + x0) * 4) as usize;
                    let row_end = ((y * width + x1) * 4) as usize;
                    // FNV-1a over the tile row. Hashing every 4th u32 would
                    // be faster but risks missing single-pixel changes;
                    // correctness first, SIMD later.
                    for &b in &bgra[row_start..row_end] {
                        hash ^= b as u64;
                        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                    }
                }
                let idx = (ty * self.tiles_x + tx) as usize;
                if self.hashes[idx] != hash {
                    self.hashes[idx] = hash;
                    any = true;
                    min_tx = min_tx.min(tx);
                    min_ty = min_ty.min(ty);
                    max_tx = max_tx.max(tx);
                    max_ty = max_ty.max(ty);
                }
            }
        }

        if self.first {
            self.first = false;
            return Some(Rect::full(width, height));
        }
        if !any {
            return None;
        }
        let x = min_tx * TILE;
        let y = min_ty * TILE;
        let w = ((max_tx + 1) * TILE).min(width) - x;
        let h = ((max_ty + 1) * TILE).min(height) - y;
        Some(Rect { x, y, w, h })
    }

    /// Force the next frame to be treated as fully dirty (e.g. after a new
    /// client joins mid-stream and needs a full refresh).
    pub fn invalidate(&mut self) {
        self.first = true;
        self.hashes.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> Vec<u8> {
        vec![fill; (w * h * 4) as usize]
    }

    #[test]
    fn first_frame_is_full() {
        let mut d = DirtyDetector::new(256, 192);
        let f = frame(256, 192, 7);
        assert_eq!(d.detect(&f, 256, 192), Some(Rect::full(256, 192)));
    }

    #[test]
    fn unchanged_frame_is_none() {
        let mut d = DirtyDetector::new(256, 192);
        let f = frame(256, 192, 7);
        d.detect(&f, 256, 192);
        assert_eq!(d.detect(&f, 256, 192), None);
    }

    #[test]
    fn localized_change_gives_small_rect() {
        let mut d = DirtyDetector::new(256, 256);
        let mut f = frame(256, 256, 7);
        d.detect(&f, 256, 256);
        // Change one pixel at (130, 130) — inside tile (2, 2).
        let i = ((130 * 256 + 130) * 4) as usize;
        f[i] = 99;
        let r = d.detect(&f, 256, 256).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 128,
                y: 128,
                w: 64,
                h: 64
            }
        );
        assert!(r.area() < Rect::full(256, 256).area() / 10);
    }

    #[test]
    fn resolution_change_resets() {
        let mut d = DirtyDetector::new(128, 128);
        let f1 = frame(128, 128, 1);
        d.detect(&f1, 128, 128);
        let f2 = frame(256, 128, 1);
        assert_eq!(d.detect(&f2, 256, 128), Some(Rect::full(256, 128)));
    }

    #[test]
    fn invalidate_forces_full() {
        let mut d = DirtyDetector::new(128, 128);
        let f = frame(128, 128, 3);
        d.detect(&f, 128, 128);
        d.invalidate();
        assert_eq!(d.detect(&f, 128, 128), Some(Rect::full(128, 128)));
    }

    #[test]
    fn edge_tiles_clamp_to_frame() {
        // 100x100 is not a multiple of 64; the bottom-right tile is partial.
        let mut d = DirtyDetector::new(100, 100);
        let mut f = frame(100, 100, 0);
        d.detect(&f, 100, 100);
        let i = ((99 * 100 + 99) * 4) as usize;
        f[i] = 1;
        let r = d.detect(&f, 100, 100).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 64,
                y: 64,
                w: 36,
                h: 36
            }
        );
    }
}
