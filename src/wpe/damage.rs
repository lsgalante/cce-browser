//! What changed between the frame a tab's image holds and the one about to be
//! read, so that only that much is copied out of the engine's buffer and
//! uploaded.
//!
//! WebKit reports each frame's damage — what it repainted since the frame
//! before — once `PropagateDamagingInformation` is on (`host.rs` turns it on
//! for every webview). A frame that is handed back unread still changed the
//! picture, so its damage is kept and folded into the next frame of the same
//! view; that is what lets the readback skip frames *and* copy only regions.
//!
//! The whole point is the idle cost. A page that can scroll has WebKit
//! repainting its overlay scrollbar about sixty times a second, for good
//! (WebKit 2.52; measured in a shadow, and MiniBrowser does it too), and a
//! page with a sliding banner repaints that band. Reading the whole window
//! for each — 35 MB a frame at 3840x2400 — was most of the browser's own CPU
//! on such pages.

/// A rectangle of the buffer in pixels: `(x, y, width, height)`.
pub type Rect = (u32, u32, u32, u32);

/// More rectangles than this and they are merged into their bounding box:
/// each one is its own row loop and its own copy region, and a frame that
/// changed in that many places is better read as one.
const MAX_RECTS: usize = 16;

/// Damage that covers this share of the frame or more is read whole: one
/// contiguous copy beats many row copies adding up to nearly the same bytes.
const FULL_SHARE: f64 = 0.5;

/// What changed in one view since its frame was last read.
#[derive(Debug, Clone, PartialEq)]
pub enum Damage {
    /// Unknown or everything: read the whole frame.
    Full,
    /// Only these, in buffer pixels, not yet clamped to the buffer.
    Rects(Vec<(i32, i32, i32, i32)>),
}

impl Damage {
    /// Fold one frame's report in. A frame that reports no rectangles did
    /// not say what it changed — that is what an engine without damage
    /// propagation sends — so it counts as everything.
    pub fn add(&mut self, frame: &[(i32, i32, i32, i32)]) {
        if frame.is_empty() {
            *self = Damage::Full;
            return;
        }
        if let Damage::Rects(rects) = self {
            for r in frame {
                if !rects.contains(r) {
                    rects.push(*r);
                }
            }
        }
    }

    /// The regions to copy from a `width` x `height` buffer, or `None` when
    /// the whole frame should be read instead.
    pub fn regions(&self, width: u32, height: u32) -> Option<Vec<Rect>> {
        let Damage::Rects(raw) = self else { return None };
        let mut rects: Vec<Rect> = Vec::new();
        for r in raw.iter().filter_map(|&r| clamp(r, width, height)) {
            // Two reports can clamp to the same rectangle.
            if !rects.contains(&r) {
                rects.push(r);
            }
        }
        // A rectangle inside another adds nothing but a second copy of it.
        // (No two are equal by now, so containment is strict.)
        let mut i = 0;
        while i < rects.len() {
            let inner = rects[i];
            if rects.iter().enumerate().any(|(j, &outer)| j != i && contains(outer, inner)) {
                rects.remove(i);
            } else {
                i += 1;
            }
        }
        if rects.len() > MAX_RECTS {
            rects = vec![bounding(&rects)];
        }
        let area: u64 = rects.iter().map(|r| r.2 as u64 * r.3 as u64).sum();
        if area as f64 >= FULL_SHARE * width as f64 * height as f64 {
            return None;
        }
        Some(rects)
    }
}

fn clamp((x, y, w, h): (i32, i32, i32, i32), width: u32, height: u32) -> Option<Rect> {
    let x0 = x.max(0) as i64;
    let y0 = y.max(0) as i64;
    let x1 = (x as i64 + w as i64).min(width as i64);
    let y1 = (y as i64 + h as i64).min(height as i64);
    (x1 > x0 && y1 > y0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 + inner.2 <= outer.0 + outer.2
        && inner.1 + inner.3 <= outer.1 + outer.3
}

fn bounding(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.0).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.1).min().unwrap_or(0);
    let x1 = rects.iter().map(|r| r.0 + r.2).max().unwrap_or(0);
    let y1 = rects.iter().map(|r| r.1 + r.3).max().unwrap_or(0);
    (x0, y0, x1 - x0, y1 - y0)
}

/// Copy `regions` of a 4-byte-per-pixel image whose rows are `stride` bytes
/// apart into `out`, each region tightly packed, one after another — the
/// layout `cce_ui::vk::update_pixel_regions` takes.
///
/// # Safety
/// `src` must be readable for every byte of every region at `stride`.
pub unsafe fn pack(src: *const u8, stride: usize, regions: &[Rect], out: &mut [u8]) {
    let mut at = 0usize;
    for &(x, y, w, h) in regions {
        let row = w as usize * 4;
        for r in 0..h as usize {
            let from = src.add((y as usize + r) * stride + x as usize * 4);
            std::ptr::copy_nonoverlapping(from, out.as_mut_ptr().add(at), row);
            at += row;
        }
    }
}

/// Bytes [`pack`] writes for `regions`.
pub fn packed_len(regions: &[Rect]) -> usize {
    regions.iter().map(|r| r.2 as usize * r.3 as usize * 4).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_without_rects_is_everything() {
        let mut d = Damage::Rects(vec![]);
        d.add(&[(0, 0, 10, 10)]);
        d.add(&[]);
        assert_eq!(d, Damage::Full);
        // And stays so: later reports cannot narrow an unknown change.
        d.add(&[(0, 0, 1, 1)]);
        assert_eq!(d.regions(100, 100), None);
    }

    #[test]
    fn skipped_frames_accumulate_without_repeats() {
        let mut d = Damage::Rects(vec![]);
        d.add(&[(1238, 0, 42, 720)]);
        d.add(&[(1238, 0, 42, 720)]);
        d.add(&[(0, 0, 10, 10)]);
        assert_eq!(d.regions(1280, 720), Some(vec![(1238, 0, 42, 720), (0, 0, 10, 10)]));
    }

    #[test]
    fn rects_are_clamped_to_the_buffer() {
        // The engine's first frame reports its pre-resize size.
        let mut d = Damage::Rects(vec![]);
        d.add(&[(-5, -5, 20, 20), (1270, 710, 50, 50)]);
        assert_eq!(d.regions(1280, 720), Some(vec![(0, 0, 15, 15), (1270, 710, 10, 10)]));
        let mut off = Damage::Rects(vec![]);
        off.add(&[(2000, 0, 10, 10)]);
        assert_eq!(off.regions(1280, 720), Some(vec![]));
    }

    #[test]
    fn contained_rects_are_dropped() {
        let mut d = Damage::Rects(vec![]);
        d.add(&[(0, 0, 100, 100), (10, 10, 5, 5)]);
        assert_eq!(d.regions(1000, 1000), Some(vec![(0, 0, 100, 100)]));
    }

    #[test]
    fn large_damage_reads_the_whole_frame() {
        let mut d = Damage::Rects(vec![]);
        d.add(&[(0, 0, 1280, 400)]);
        assert_eq!(d.regions(1280, 720), None);
    }

    #[test]
    fn many_rects_merge_into_their_bounds() {
        let mut d = Damage::Rects(vec![]);
        let many: Vec<_> = (0..20).map(|i| (i * 10, 0, 5, 5)).collect();
        d.add(&many);
        assert_eq!(d.regions(1280, 720), Some(vec![(0, 0, 195, 5)]));
    }

    #[test]
    fn pack_copies_each_region_tightly() {
        // A 4x3 image, stride padded to 5 pixels; each pixel's bytes are its
        // index, so what lands where is readable.
        let (w, h, stride) = (4usize, 3usize, 20usize);
        let mut src = vec![0xEEu8; stride * h];
        for y in 0..h {
            for x in 0..w {
                src[y * stride + x * 4..][..4].fill((y * w + x) as u8);
            }
        }
        let regions = [(1, 0, 2, 2), (3, 2, 1, 1)];
        let mut out = vec![0u8; packed_len(&regions)];
        unsafe { pack(src.as_ptr(), stride, &regions, &mut out) };
        let px: Vec<u8> = out.chunks(4).map(|p| p[0]).collect();
        assert_eq!(px, vec![1, 2, 5, 6, 11]);
    }
}
