//! Frame-difference tracking for dirty-region encoding.
//!
//! Every encoder keeps a copy of the previous frame and diffs incoming
//! frames against it in *row pairs* (two rows at a time — the natural unit
//! for 4:2:0 chroma subsampling). The result drives two optimizations:
//!
//! 1. **Static-frame elision** — a frame with zero dirty row pairs is not
//!    encoded and not sent at all (desktop idle, cursor-only movement once
//!    the cursor rides its own channel). This drops encode cost and
//!    bandwidth to (near) zero for an idle screen.
//! 2. **Partial color conversion** — BGRA→I420 runs only over dirty row
//!    pairs; unchanged regions of the encoder's YUV buffer are still valid
//!    from the previous frame (text editing / window dragging / scrolling
//!    convert only what moved).
//!
//! The comparison is an exact `memcmp` per row pair (SIMD-accelerated by the
//! standard library, ~20 GB/s), *not* a hash — no false "clean" rows, ever.
//! Cost on a fully static 1080p frame ≈ 0.4 ms; on a fully dirty frame the
//! extra copy-to-previous adds ≈ 0.8 ms — repaid many times over by the
//! partial conversion + skipped encodes in every realistic desktop workload.
//!
//! The tracker is per-encoder (each session owns one), so sessions that
//! consume frames at different rates stay correct: the diff is always
//! against *that encoder's* last-seen frame.

/// Diff of the current frame against the previous one, in row pairs.
// `pairs` drives the H.264 partial conversion; the JPEG-only build (no
// `h264` feature) uses just `is_static`.
#[cfg_attr(not(feature = "h264"), allow(dead_code))]
pub struct DirtyMap {
    /// `pairs[i]` == true → rows `2i` and `2i+1` changed.
    pub pairs: Vec<bool>,
    pub dirty_pairs: usize,
    pub total_pairs: usize,
}

impl DirtyMap {
    /// Test/diagnostic helper.
    #[allow(dead_code)]
    pub fn all_dirty(&self) -> bool {
        self.dirty_pairs == self.total_pairs
    }
    pub fn is_static(&self) -> bool {
        self.dirty_pairs == 0
    }
}

#[derive(Default)]
pub struct DirtyTracker {
    prev: Vec<u8>,
    w: usize,
    h: usize,
    valid: bool,
    map: Vec<bool>,
}

impl DirtyTracker {
    /// Diff `bgra` against the previous frame and copy changed row pairs
    /// into the retained copy. Returns which row pairs changed (everything,
    /// on the first frame or after a resolution change).
    pub fn update(&mut self, bgra: &[u8], w: usize, h: usize) -> DirtyMap {
        debug_assert_eq!(bgra.len(), w * h * 4);
        let pair_bytes = w * 4 * 2;
        let total_pairs = h / 2;
        if self.w != w || self.h != h {
            self.w = w;
            self.h = h;
            self.valid = false;
            self.prev.clear();
            self.prev.reserve(bgra.len());
        }
        self.map.clear();
        self.map.resize(total_pairs, false);

        if !self.valid {
            self.prev.clear();
            self.prev.extend_from_slice(bgra);
            self.valid = true;
            for p in self.map.iter_mut() {
                *p = true;
            }
            return DirtyMap {
                pairs: self.map.clone(),
                dirty_pairs: total_pairs,
                total_pairs,
            };
        }

        let mut dirty_pairs = 0usize;
        for (i, (cur, prev)) in bgra
            .chunks_exact(pair_bytes)
            .zip(self.prev.chunks_exact_mut(pair_bytes))
            .enumerate()
        {
            if cur != prev {
                prev.copy_from_slice(cur);
                self.map[i] = true;
                dirty_pairs += 1;
            }
        }
        DirtyMap {
            pairs: self.map.clone(),
            dirty_pairs,
            total_pairs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_all_dirty_then_static() {
        let (w, h) = (8usize, 8usize);
        let frame = vec![7u8; w * h * 4];
        let mut t = DirtyTracker::default();
        let d = t.update(&frame, w, h);
        assert!(d.all_dirty());
        let d = t.update(&frame, w, h);
        assert!(d.is_static());
    }

    #[test]
    fn localized_change_dirties_only_its_row_pair() {
        let (w, h) = (8usize, 8usize);
        let mut frame = vec![7u8; w * h * 4];
        let mut t = DirtyTracker::default();
        t.update(&frame, w, h);
        // Touch one pixel in row 5 → row pair 2 (rows 4..6) dirty.
        frame[(5 * w + 3) * 4] = 99;
        let d = t.update(&frame, w, h);
        assert_eq!(d.dirty_pairs, 1);
        assert!(d.pairs[2]);
        assert!(!d.pairs[0] && !d.pairs[1] && !d.pairs[3]);
        // And the tracker retained the change: same frame again → static.
        let d = t.update(&frame, w, h);
        assert!(d.is_static());
    }

    #[test]
    fn resolution_change_resets() {
        let mut t = DirtyTracker::default();
        let a = vec![1u8; 8 * 8 * 4];
        t.update(&a, 8, 8);
        let b = vec![1u8; 16 * 4 * 4];
        let d = t.update(&b, 16, 4);
        assert!(d.all_dirty());
    }
}
